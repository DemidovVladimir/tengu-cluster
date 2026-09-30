//! Paper exec + position tools (`risk-paper-tools`): orders through
//! `exec_common::run_exec` (the `[risk]` gate inside the tool, tracker
//! convention 9) and the account at fresh marks.
//!
//! | Tool | Args (* required) | Row |
//! |---|---|---|
//! | `paper_order` | `instrument`* (full id), `side`* buy / sell, `notional_usd`*, `kind`* market / limit, `limit_px` (limit only), `tif` ioc, `reduce_only`, `max_slippage_bps`*, `strategy` (§21 type), `hedge_instrument`, `opportunity` (row key), `client_order_id`, `exit_at_ms` | `paper_fill/1:<account>:<client_order_id>` |
//! | `paper_close` | `instrument` or `all = true`; `max_slippage_bps`*; `client_order_id` — reduce-only market IOC of the whole position | one: `paper_fill/1` · all: `paper_close/1:<account>:<client_order_id>`, legs `<client_order_id>:<instrument>` (each its own `paper_fill/1`, replayed by its id) |
//! | `paper_positions` | `account` (default `[risk] account`) | `paper_positions/1:<account>` (2 s): books the funding owed first; marks from fresh `mkt_ctx/1` rows (missing ⇒ partial, never 0); exit deadlines per position |
//!
//! Arguments parse strictly — an unknown key, a wrong type, a short id or
//! `null` for a required key is a tool error naming it; nothing is placed.
//! Exec orders run on the `[risk]` account with `[risk]` limits. Scopes:
//! `fs_roots` = the workspace (store); `paper_order` / `paper_close` also
//! `net_hosts = ["api.hyperliquid.xyz"]`, `env_reads = ["HL_API_URL"]`
//! (the live book after the latency).

use std::collections::BTreeSet;
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use serde_json::{Map, Value};

use super::exec_common::{
    accrue_due_funding, check_private_agent, client_order_id, finish, live_books, run_exec, ExecIo,
    ExecOrder, ExecSize, MarketRows,
};
use super::{defs, XmShared};
use crate::adapters::outbound::clock::SystemClock;
use crate::adapters::outbound::rate_limit::jitter01;
use crate::adapters::outbound::tools::hyperliquid::store_live;
use crate::config::risk::STRATEGIES;
use crate::domain::book::Side;
use crate::domain::market::InstrumentId;
use crate::domain::message::ToolDef;
use crate::domain::observation::{ObsSource, Observation};
use crate::domain::tools as names;
use crate::domain::xm::exec::{CloseLeg, PaperCloseAll, PaperFillRow};
use crate::domain::xm::paper::OrderKind;
use crate::domain::xm::risk::RiskLimits;
use crate::ports::clock::Clock;
use crate::ports::tool::{Tool, ToolCtx, ToolOutput};

/// How long a `paper_positions/1` row stays fresh in the store.
pub(crate) const PAPER_POSITIONS_TTL_MS: u64 = 2_000;

pub(crate) fn tools(shared: &XmShared) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(PaperOrderTool {
            def: defs::def(names::PAPER_ORDER),
            shared: shared.clone(),
        }),
        Arc::new(PaperCloseTool {
            def: defs::def(names::PAPER_CLOSE),
            shared: shared.clone(),
        }),
        Arc::new(PaperPositionsTool {
            def: defs::def(names::PAPER_POSITIONS),
            shared: shared.clone(),
        }),
    ]
}

// ── Argument parsing (strict) ─────────────────────────────────────

/// `args` as an object whose keys are all in `allowed`; `null` values
/// count as absent.
pub(super) fn object<'a>(
    tool: &str,
    args: &'a Value,
    allowed: &[&str],
) -> Result<&'a Map<String, Value>> {
    let o = args
        .as_object()
        .ok_or_else(|| anyhow!("{tool}: arguments must be a JSON object"))?;
    let unknown: Vec<&str> = o
        .keys()
        .map(String::as_str)
        .filter(|k| !allowed.contains(k))
        .collect();
    if !unknown.is_empty() {
        bail!(
            "{tool}: unknown argument(s) {unknown:?} (allowed: {})",
            allowed.join(", ")
        );
    }
    Ok(o)
}

fn field<'a>(o: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    o.get(key).filter(|v| !v.is_null())
}

fn opt_str<'a>(tool: &str, o: &'a Map<String, Value>, key: &str) -> Result<Option<&'a str>> {
    match field(o, key) {
        None => Ok(None),
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(Some(s.as_str())),
        Some(v) => bail!("{tool}: '{key}' must be a non-empty string, got {v}"),
    }
}

fn req_str<'a>(tool: &str, o: &'a Map<String, Value>, key: &str) -> Result<&'a str> {
    opt_str(tool, o, key)?.ok_or_else(|| anyhow!("{tool}: '{key}' is required (string)"))
}

/// A finite number inside `(lo, hi)`.
pub(super) fn opt_num(
    tool: &str,
    o: &Map<String, Value>,
    key: &str,
    lo: f64,
    hi: f64,
) -> Result<Option<f64>> {
    match field(o, key) {
        None => Ok(None),
        Some(v) => match v.as_f64().filter(|x| x.is_finite() && *x > lo && *x < hi) {
            Some(x) => Ok(Some(x)),
            None => bail!("{tool}: '{key}' must be a number > {lo} and < {hi}, got {v}"),
        },
    }
}

fn req_num(tool: &str, o: &Map<String, Value>, key: &str, lo: f64, hi: f64) -> Result<f64> {
    opt_num(tool, o, key, lo, hi)?.ok_or_else(|| anyhow!("{tool}: '{key}' is required (number)"))
}

fn opt_bool(tool: &str, o: &Map<String, Value>, key: &str) -> Result<Option<bool>> {
    match field(o, key) {
        None => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(v) => bail!("{tool}: '{key}' must be a boolean, got {v}"),
    }
}

/// A full `<venue>:<native id>`, verbatim (convention 1).
fn instrument(tool: &str, key: &str, s: &str) -> Result<InstrumentId> {
    InstrumentId::parse(s).map_err(|e| anyhow!("{tool}: '{key}': {e}"))
}

const ORDER_ARGS: &[&str] = &[
    "instrument",
    "side",
    "notional_usd",
    "kind",
    "limit_px",
    "tif",
    "reduce_only",
    "max_slippage_bps",
    "strategy",
    "hedge_instrument",
    "opportunity",
    "client_order_id",
    "exit_at_ms",
];

/// `paper_order` arguments (module table) → the order on `limits`.
pub(crate) fn parse_order(args: &Value, limits: RiskLimits) -> Result<ExecOrder> {
    let tool = names::PAPER_ORDER;
    let o = object(tool, args, ORDER_ARGS)?;
    let id = instrument(tool, "instrument", req_str(tool, o, "instrument")?)?;
    let side = Side::parse(req_str(tool, o, "side")?)
        .ok_or_else(|| anyhow!("{tool}: 'side' must be buy or sell"))?;
    let notional = req_num(tool, o, "notional_usd", 0.0, f64::INFINITY)?;
    let kind = match req_str(tool, o, "kind")? {
        "market" => OrderKind::Market,
        "limit" => OrderKind::Limit,
        other => bail!("{tool}: 'kind' must be market or limit, got {other}"),
    };
    let limit_px = opt_num(tool, o, "limit_px", 0.0, f64::INFINITY)?;
    match (kind, limit_px) {
        (OrderKind::Limit, None) => bail!("{tool}: kind = limit needs 'limit_px'"),
        (OrderKind::Market, Some(_)) => {
            bail!("{tool}: a market order takes no 'limit_px' (max_slippage_bps bounds it)")
        }
        _ => {}
    }
    if let Some(t) = opt_str(tool, o, "tif")? {
        if t != "ioc" {
            bail!("{tool}: 'tif' must be ioc (GTC / ALO are not supported), got {t}");
        }
    }
    let strategy = opt_str(tool, o, "strategy")?;
    if let Some(s) = strategy {
        if !STRATEGIES.contains(&s) {
            bail!(
                "{tool}: 'strategy' must be one of {}, got {s}",
                STRATEGIES.join(", ")
            );
        }
    }
    let hedge = opt_str(tool, o, "hedge_instrument")?
        .map(|h| instrument(tool, "hedge_instrument", h))
        .transpose()?;
    let exit_at_ms = match field(o, "exit_at_ms") {
        None => None,
        Some(v) => Some(
            v.as_i64()
                .filter(|t| *t > 0)
                .ok_or_else(|| anyhow!("{tool}: 'exit_at_ms' must be epoch ms > 0, got {v}"))?,
        ),
    };
    Ok(ExecOrder {
        tool,
        limits,
        instrument: id,
        side,
        size: ExecSize::NotionalUsd(notional),
        kind,
        limit_px,
        reduce_only: opt_bool(tool, o, "reduce_only")?.unwrap_or(false),
        max_slippage_bps: req_num(tool, o, "max_slippage_bps", 0.0, 10_000.0)?,
        strategy: strategy.map(str::to_string),
        hedge_instrument: hedge,
        opportunity_key: opt_str(tool, o, "opportunity")?.map(str::to_string),
        client_order_id: opt_str(tool, o, "client_order_id")?.map(str::to_string),
        exit_at_ms,
    })
}

/// What `paper_close` closes.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CloseTarget {
    One(InstrumentId),
    All,
}

/// `paper_close` arguments: the target, the slippage bound, the id arg.
pub(crate) fn parse_close(args: &Value) -> Result<(CloseTarget, f64, Option<String>)> {
    let tool = names::PAPER_CLOSE;
    let o = object(
        tool,
        args,
        &["instrument", "all", "max_slippage_bps", "client_order_id"],
    )?;
    let target = match (opt_str(tool, o, "instrument")?, opt_bool(tool, o, "all")?) {
        (Some(i), None | Some(false)) => CloseTarget::One(instrument(tool, "instrument", i)?),
        (None, Some(true)) => CloseTarget::All,
        (Some(_), Some(true)) => bail!("{tool}: give 'instrument' or all = true, not both"),
        (None, _) => bail!("{tool}: give 'instrument' (full id) or all = true"),
    };
    Ok((
        target,
        req_num(tool, o, "max_slippage_bps", 0.0, 10_000.0)?,
        opt_str(tool, o, "client_order_id")?.map(str::to_string),
    ))
}

// ── Tools ─────────────────────────────────────────────────────────

/// The live clock, book source and jitter of one tool call.
struct Live {
    books: crate::adapters::outbound::tools::hyperliquid::book::HlBookSource,
    rand01: f64,
}

impl Live {
    fn for_call(shared: &XmShared, ctx: &ToolCtx<'_>) -> Self {
        Self {
            books: live_books(ctx, shared.store.clone()),
            rand01: jitter01(),
        }
    }

    fn io(&self) -> ExecIo<'_> {
        ExecIo {
            clock: &SystemClock,
            books: &self.books,
            rand01: self.rand01,
        }
    }
}

pub(crate) struct PaperOrderTool {
    def: ToolDef,
    shared: XmShared,
}

impl PaperOrderTool {
    pub(crate) async fn place(
        &self,
        args: &Value,
        ctx: &ToolCtx<'_>,
        io: &ExecIo<'_>,
    ) -> Result<Observation> {
        let (risk, _, _) = self.shared.parts()?;
        let order = parse_order(args, risk.limits())?;
        run_exec(&self.shared, ctx, io, order).await
    }
}

#[async_trait]
impl Tool for PaperOrderTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        ctx.scope.check_fs_write(ctx.workspace)?;
        let live = Live::for_call(&self.shared, ctx);
        let obs = self.place(args, ctx, &live.io()).await?;
        Ok(ToolOutput::observed(obs, SystemClock.now_ms()))
    }
}

pub(crate) struct PaperCloseTool {
    def: ToolDef,
    shared: XmShared,
}

impl PaperCloseTool {
    pub(crate) async fn close(
        &self,
        args: &Value,
        ctx: &ToolCtx<'_>,
        io: &ExecIo<'_>,
    ) -> Result<Observation> {
        let (risk, paper, ledger) = self.shared.parts()?;
        let (target, max_slippage_bps, coid) = parse_close(args)?;
        let order = |id: InstrumentId, coid: Option<String>| ExecOrder {
            tool: names::PAPER_CLOSE,
            limits: risk.limits(),
            instrument: id,
            side: Side::Sell,
            size: ExecSize::Close,
            kind: OrderKind::Market,
            limit_px: None,
            reduce_only: true,
            max_slippage_bps,
            strategy: None,
            hedge_instrument: None,
            opportunity_key: None,
            client_order_id: coid,
            exit_at_ms: None,
        };
        if let CloseTarget::One(id) = target {
            return run_exec(&self.shared, ctx, io, order(id, coid)).await;
        }
        check_private_agent(ctx)?;
        let base = client_order_id(coid.as_deref(), ctx.call_id)?;
        let account = risk.account.clone();
        let now = io.clock.now_ms();
        ledger
            .open_account(&account, paper.initial_cash_usd, now)
            .await?;
        let snap = ledger.snapshot(&account, now).await?;
        let open: Vec<String> = snap
            .account
            .open_positions()
            .map(|p| p.instrument.clone())
            .collect();
        let mut legs = Vec::new();
        for instrument_id in open {
            let leg_id = format!("{base}:{instrument_id}");
            let result = match InstrumentId::parse(&instrument_id) {
                Ok(id) => run_exec(&self.shared, ctx, io, order(id, Some(leg_id.clone()))).await,
                Err(e) => Err(anyhow!("{e}")),
            };
            legs.push(match result {
                Ok(obs) => leg_of(&instrument_id, &leg_id, &obs),
                Err(e) => CloseLeg {
                    instrument: instrument_id,
                    client_order_id: leg_id,
                    status: "error".into(),
                    rule: None,
                    filled_qty: None,
                    avg_px: None,
                    error: Some(format!("{e:#}")),
                },
            });
        }
        let row = PaperCloseAll {
            account,
            client_order_id: base,
            legs,
            ts_ms: io.clock.now_ms(),
        };
        let store = self.shared.store.as_deref();
        Ok(finish(store, names::PAPER_CLOSE, &row, row.ts_ms).await)
    }
}

fn leg_of(instrument: &str, leg_id: &str, obs: &Observation) -> CloseLeg {
    let row: Option<PaperFillRow> = obs.typed().ok();
    let fill = row.as_ref().and_then(|r| r.fill.as_ref());
    CloseLeg {
        instrument: instrument.to_string(),
        client_order_id: leg_id.to_string(),
        status: row
            .as_ref()
            .map_or("error", |r| r.status_word())
            .to_string(),
        rule: row.as_ref().map(|r| r.gate.rule.clone()),
        filled_qty: fill.map(|f| f.filled_qty),
        avg_px: fill.and_then(|f| f.avg_px),
        error: None,
    }
}

#[async_trait]
impl Tool for PaperCloseTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        ctx.scope.check_fs_write(ctx.workspace)?;
        let live = Live::for_call(&self.shared, ctx);
        let obs = self.close(args, ctx, &live.io()).await?;
        Ok(ToolOutput::observed(obs, SystemClock.now_ms()))
    }
}

pub(crate) struct PaperPositionsTool {
    def: ToolDef,
    shared: XmShared,
}

impl PaperPositionsTool {
    pub(crate) async fn positions(&self, args: &Value, now_ms: i64) -> Result<Observation> {
        let tool = names::PAPER_POSITIONS;
        let (risk, paper, ledger) = self.shared.parts()?;
        let o = object(tool, args, &["account"])?;
        let account = opt_str(tool, o, "account")?.unwrap_or(&risk.account);
        let valid = account
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
        if !valid {
            bail!("{tool}: 'account' must be a [A-Za-z0-9._-] ledger account, got {account}");
        }
        if account == risk.account {
            ledger
                .open_account(account, paper.initial_cash_usd, now_ms)
                .await?;
        }
        let max_ctx_ms = risk.max_data_age_ms.ctx;
        let store = self.shared.store.as_deref();
        let before = ledger.snapshot(account, now_ms).await?;
        let ids: BTreeSet<String> = before
            .account
            .open_positions()
            .map(|p| p.instrument.clone())
            .collect();
        let rows = MarketRows::read(store, &ids, None, None).await;
        accrue_due_funding(ledger.as_ref(), &before.account, &rows, now_ms, max_ctx_ms).await?;
        let snap = ledger.snapshot(account, now_ms).await?;
        let (mut valued, _) = snap
            .risk
            .value(&snap.account, &rows.marks(&ids), now_ms, max_ctx_ms);
        for r in &mut valued.positions {
            r.exit_at_ms = snap.exit_at_ms.get(&r.instrument).copied();
        }
        let obs = Observation::of(
            tool,
            &valued,
            now_ms,
            PAPER_POSITIONS_TTL_MS,
            ObsSource::Live,
        );
        store_live(store, &obs).await;
        Ok(obs)
    }
}

#[async_trait]
impl Tool for PaperPositionsTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        ctx.scope.check_fs_write(ctx.workspace)?;
        let now = SystemClock.now_ms();
        let obs = self.positions(args, now).await?;
        Ok(ToolOutput::observed(obs, now))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::adapters::outbound::tools::xm::exec_common::tests::{
        market_rows, Rig, NOW, OPP, TSLA,
    };
    use crate::domain::observation::ObsStatus;
    use crate::domain::xm::ledger::PaperPositions;
    use crate::domain::xm::paper::FillStatus;
    use crate::ports::observation::ObservationStore;

    fn order_tool(rig: &Rig) -> PaperOrderTool {
        PaperOrderTool {
            def: defs::def(names::PAPER_ORDER),
            shared: rig.shared.clone(),
        }
    }

    fn close_tool(rig: &Rig) -> PaperCloseTool {
        PaperCloseTool {
            def: defs::def(names::PAPER_CLOSE),
            shared: rig.shared.clone(),
        }
    }

    fn positions_tool(rig: &Rig) -> PaperPositionsTool {
        PaperPositionsTool {
            def: defs::def(names::PAPER_POSITIONS),
            shared: rig.shared.clone(),
        }
    }

    fn io(rig: &Rig) -> ExecIo<'_> {
        ExecIo {
            clock: rig.clock.as_ref(),
            books: &rig.books,
            rand01: 0.5,
        }
    }

    fn buy_args(usd: f64) -> Value {
        json!({"instrument": TSLA, "side": "buy", "notional_usd": usd, "kind": "market",
               "max_slippage_bps": 30, "strategy": "overreaction", "opportunity": OPP})
    }

    #[test]
    fn order_args_parse_strictly() {
        let rig_limits = || {
            let dir = tempfile::tempdir().unwrap();
            crate::adapters::outbound::tools::xm::exec_common::tests::risk(
                &dir.path().join("KILL"),
                25,
            )
            .limits()
        };
        let o = parse_order(&buy_args(25.0), rig_limits()).unwrap();
        assert_eq!(o.instrument.to_string(), TSLA);
        assert_eq!(
            (o.size, o.kind, o.side, o.reduce_only),
            (
                ExecSize::NotionalUsd(25.0),
                OrderKind::Market,
                Side::Buy,
                false
            )
        );
        assert_eq!(o.opportunity_key.as_deref(), Some(OPP));
        let with = |k: &str, v: Value| {
            let mut a = buy_args(25.0);
            a[k] = v;
            parse_order(&a, rig_limits())
                .map(|_| ())
                .unwrap_err()
                .to_string()
        };
        assert!(with("instrument", json!("xyz:TSLA")).contains("'instrument'"));
        assert!(with("instrument", json!("TSLA")).contains("not `<venue>:<native id>`"));
        assert!(with("side", json!("long")).contains("buy or sell"));
        assert!(with("notional_usd", json!(0)).contains("'notional_usd' must be a number > 0"));
        assert!(with("notional_usd", json!("25")).contains("'notional_usd'"));
        assert!(with("kind", json!("limit")).contains("needs 'limit_px'"));
        assert!(with("limit_px", json!(347.3)).contains("takes no 'limit_px'"));
        assert!(with("tif", json!("gtc")).contains("'tif' must be ioc"));
        assert!(with("strategy", json!("momentum")).contains("'strategy' must be one of"));
        assert!(with("max_slippage_bps", json!(10_000)).contains("'max_slippage_bps'"));
        assert!(with("exit_at_ms", json!(-5)).contains("'exit_at_ms'"));
        assert!(with("reason", json!("x")).contains("unknown argument(s) [\"reason\"]"));
        let mut missing = buy_args(25.0);
        missing.as_object_mut().unwrap().remove("max_slippage_bps");
        let e = parse_order(&missing, rig_limits()).unwrap_err().to_string();
        assert!(e.contains("'max_slippage_bps' is required"), "{e}");
        // `null` for an optional key is absent.
        let mut nulls = buy_args(25.0);
        nulls["client_order_id"] = Value::Null;
        nulls["exit_at_ms"] = Value::Null;
        assert!(parse_order(&nulls, rig_limits()).is_ok());
    }

    #[test]
    fn close_args_name_one_position_or_all() {
        let (t, s, id) = parse_close(&json!({"instrument": TSLA, "max_slippage_bps": 50})).unwrap();
        assert_eq!(
            (t, s, id),
            (
                CloseTarget::One(InstrumentId::parse(TSLA).unwrap()),
                50.0,
                None
            )
        );
        let (t, _, id) =
            parse_close(&json!({"all": true, "max_slippage_bps": 50, "client_order_id": "x:1"}))
                .unwrap();
        assert_eq!((t, id.as_deref()), (CloseTarget::All, Some("x:1")));
        for bad in [
            json!({"max_slippage_bps": 50}),
            json!({"all": false, "max_slippage_bps": 50}),
            json!({"instrument": TSLA, "all": true, "max_slippage_bps": 50}),
            json!({"instrument": TSLA}),
        ] {
            assert!(parse_close(&bad).is_err(), "{bad}");
        }
    }

    /// `paper_order` → the gate → a fill; `paper_positions` shows it with its
    /// exit deadline; `paper_close` closes it through the same gate.
    #[tokio::test]
    async fn order_positions_close() {
        let rig = Rig::new(25).await;
        let mut args = buy_args(25.0);
        args["exit_at_ms"] = json!(NOW + 3_600_000);
        let o = order_tool(&rig)
            .place(&args, &rig.ctx(Some("mcp:n:2")), &io(&rig))
            .await
            .unwrap();
        assert_eq!(o.status, ObsStatus::Ok, "{}", o.render_text(NOW));
        assert_eq!(o.features["exit_at_ms"], NOW + 3_600_000);

        let p = positions_tool(&rig)
            .positions(&json!({}), rig.clock.now_ms())
            .await
            .unwrap();
        assert_eq!(p.key, "paper_positions/1:xmarket");
        assert_eq!(
            (p.status, p.ttl_ms),
            (ObsStatus::Ok, PAPER_POSITIONS_TTL_MS)
        );
        let pp: PaperPositions = p.typed().unwrap();
        assert_eq!(pp.positions[0].instrument, TSLA);
        assert_eq!(pp.positions[0].exit_at_ms, Some(NOW + 3_600_000));
        assert!(rig.store.get(&p.key).await.unwrap().is_some(), "stored 2 s");

        let c = close_tool(&rig)
            .close(
                &json!({"instrument": TSLA, "max_slippage_bps": 50}),
                &rig.ctx(Some("mcp:n:3")),
                &io(&rig),
            )
            .await
            .unwrap();
        let r: PaperFillRow = c.typed().unwrap();
        assert_eq!(c.key, "paper_fill/1:xmarket:mcp:n:3");
        assert!(r.reduce_only && r.side == Side::Sell && r.gate.allow);
        assert_eq!(r.fill.as_ref().unwrap().status, FillStatus::Filled);
        assert_eq!(r.position_qty_after, 0.0);
        let p = positions_tool(&rig)
            .positions(&json!({}), rig.clock.now_ms())
            .await
            .unwrap();
        assert_eq!(p.features["n_positions"], 0);
        // Unknown account: refused; the [risk] account is opened on first use.
        let e = positions_tool(&rig)
            .positions(&json!({"account": "shadow"}), NOW)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("shadow"), "{e}");
    }

    /// `all = true`: one leg per open position, each its own id; with none
    /// open the summary is empty and ok.
    #[tokio::test]
    async fn close_all_closes_every_position() {
        let rig = Rig::new(25).await;
        order_tool(&rig)
            .place(&buy_args(20.0), &rig.ctx(Some("c:1")), &io(&rig))
            .await
            .unwrap();
        let o = close_tool(&rig)
            .close(
                &json!({"all": true, "max_slippage_bps": 50}),
                &rig.ctx(Some("c:2")),
                &io(&rig),
            )
            .await
            .unwrap();
        assert_eq!(o.key, "paper_close/1:xmarket:c:2");
        assert_eq!(o.status, ObsStatus::Ok, "{}", o.render_text(NOW));
        let all: PaperCloseAll = o.typed().unwrap();
        assert_eq!(all.legs.len(), 1);
        assert_eq!(all.legs[0].client_order_id, format!("c:2:{TSLA}"));
        assert_eq!(all.legs[0].status, "filled");
        let again = close_tool(&rig)
            .close(
                &json!({"all": true, "max_slippage_bps": 50}),
                &rig.ctx(Some("c:3")),
                &io(&rig),
            )
            .await
            .unwrap();
        assert_eq!(
            (again.status, again.features["positions"].clone()),
            (ObsStatus::Ok, 0.into())
        );
        // A routable caller is refused even with nothing to close.
        let mut routable = Rig::new(25).await;
        routable.agent.description = Some("routable".into());
        let e = close_tool(&routable)
            .close(
                &json!({"all": true, "max_slippage_bps": 50}),
                &routable.ctx(Some("c:4")),
                &io(&routable),
            )
            .await
            .unwrap_err();
        assert!(e.to_string().starts_with("exec_agent_not_private"), "{e}");
    }

    /// Positions accrue their hourly funding when read; a stale mark leaves
    /// the numbers out (partial), never 0.
    #[tokio::test]
    async fn positions_book_funding_and_never_zero_a_stale_mark() {
        let rig = Rig::new(25).await;
        order_tool(&rig)
            .place(&buy_args(20.0), &rig.ctx(Some("f:1")), &io(&rig))
            .await
            .unwrap();
        let later = NOW + 2 * 3_600_000;
        let p = positions_tool(&rig)
            .positions(&json!({}), later)
            .await
            .unwrap();
        assert_eq!(p.status, ObsStatus::Partial, "the mark is 2 h old");
        assert!(!p.features.contains_key("equity_usd"), "never 0");
        assert_eq!(rig.rows()["funding"], 0, "a stale rate books nothing");
        for r in market_rows(347.2, later - 500) {
            rig.store.put(&r).await.unwrap();
        }
        let p = positions_tool(&rig)
            .positions(&json!({}), later)
            .await
            .unwrap();
        assert_eq!(p.status, ObsStatus::Ok, "{}", p.render_text(later));
        assert_eq!(rig.rows()["funding"], 2);
        assert!(p.features["funding_usd"].as_f64().unwrap() > 0.0);
    }

    #[tokio::test]
    async fn refused_without_risk() {
        let mut rig = Rig::new(25).await;
        rig.shared.risk = None;
        for e in [
            order_tool(&rig)
                .place(&buy_args(20.0), &rig.ctx(Some("r:1")), &io(&rig))
                .await
                .unwrap_err(),
            close_tool(&rig)
                .close(
                    &json!({"all": true, "max_slippage_bps": 50}),
                    &rig.ctx(Some("r:2")),
                    &io(&rig),
                )
                .await
                .unwrap_err(),
            positions_tool(&rig)
                .positions(&json!({}), NOW)
                .await
                .unwrap_err(),
        ] {
            assert!(e.to_string().starts_with("risk_config_missing"), "{e}");
        }
    }

    #[tokio::test]
    async fn execute_gates_the_scope_first() {
        let rig = Rig::new(25).await;
        let mut denied = rig.ctx(Some("s:1"));
        let no_fs = crate::domain::scope::ToolScope::default();
        denied.scope = &no_fs;
        for t in tools(&rig.shared) {
            let e = t.execute(&json!({}), &denied).await.unwrap_err();
            assert!(e.to_string().contains("fs"), "{}: {e}", t.definition().name);
        }
        let names: Vec<String> = tools(&rig.shared)
            .iter()
            .map(|t| t.definition().name.clone())
            .collect();
        assert_eq!(names, ["paper_order", "paper_close", "paper_positions"]);
    }
}
