//! `xm_exits` — the exit rules (`x-exit-rules`): every open position of the
//! `[risk]` account that `domain::xm::exits::exit_due` names is closed
//! through `run_exec` (a reduce-only market IOC of the whole position, the
//! `[risk]` gate inside the tool). An exec tool: private agents only. Run by
//! a `kind = "tool"` feed (`[feeds.xm_exits]`, every 15 s — no LLM, no Jev);
//! any engine may call it too.
//!
//! | Step | Rule |
//! |---|---|
//! | Refuse | no `[risk]` / ledger (`risk_config_missing` · `state_dir_missing` · `ledger_unavailable`); a caller that is not a private agent (`exec_agent_not_private`); an unknown argument |
//! | Positions | the `[risk]` account's ledger snapshot (opened on first use) and each position's exit deadline |
//! | Marks | the positions' `mkt_ctx/1` rows in the store, never fetched; missing or older than `[risk] max_data_age_ms.ctx` ⇒ no stop-loss / take-profit for that position (`n_stale_marks`); deadline and max hold still apply |
//! | Close | per due position, in id order: `run_exec` with `ExecSize::Close` (sized from the position inside the ledger transaction: a closed position is never closed again), IOC bound `max_slippage_bps` (the arg, default `[risk] max_slippage_bps`), id `exit:<account>:<instrument>:<reason>:<opened_ms>` — when that id's order is already stored and the position is still open (rejected, partial), the first unstored attempt `…:<n>`; a denied attempt stores nothing and is judged again next run; each close needs the instrument's `mkt_instrument/1` row (keep `hl_ctx` running, else `missing:mkt_instrument`) and counts toward `[risk] max_orders_per_min` |
//! | Row | `xm_exits/1:<account>` (ttl 0, `domain/xm/exits.rs::XmExits`): `n_open`, `n_due`, `n_closed`, `n_failed`, `n_stale_marks`; per position the full id, entry, deadline, mark, P&L, reason, status, id, gate rule and fill |
//!
//! Each close is its own `paper_fill/1` row and verdict (`ledger.db` +
//! `logs/risk.jsonl`, tool `xm_exits`, the feed's call id).

use std::collections::BTreeSet;
use std::future::Future;
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use serde_json::Value;

use super::exec_common::{
    check_private_agent, finish, live_books, run_exec, ExecGate, ExecIo, ExecOrder, ExecSize,
    MarketRows,
};
use super::paper::{object, opt_num};
use super::{defs, XmShared};
use crate::adapters::outbound::clock::SystemClock;
use crate::adapters::outbound::rate_limit::jitter01;
use crate::config::risk::RiskConfig;
use crate::domain::book::Side;
use crate::domain::market::InstrumentId;
use crate::domain::message::ToolDef;
use crate::domain::observation::{Field, Observation};
use crate::domain::tools as names;
use crate::domain::xm::exec::PaperFillRow;
use crate::domain::xm::exits::{
    exit_client_order_id, exit_due, pnl_bps, ExitCheck, ExitReason, ExitStatus, XmExits,
};
use crate::domain::xm::ledger::{fresh_mark, Position};
use crate::domain::xm::paper::{FillStatus, OrderKind};
use crate::ports::clock::Clock;
use crate::ports::tool::{Tool, ToolCtx, ToolOutput};

/// Exit attempts searched per position opening + reason — a bound on the
/// search, not a policy (≈ 6 months of 15 s runs).
const MAX_EXIT_ATTEMPTS: u32 = 1 << 20;

pub(crate) fn tools(shared: &XmShared) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(XmExitsTool {
        def: defs::def(names::XM_EXITS),
        shared: shared.clone(),
    })]
}

pub(crate) struct XmExitsTool {
    def: ToolDef,
    shared: XmShared,
}

impl XmExitsTool {
    /// The module table on `io`'s clock and books.
    pub(crate) async fn check(
        &self,
        args: &Value,
        ctx: &ToolCtx<'_>,
        io: &ExecIo<'_>,
    ) -> Result<Observation> {
        let tool = names::XM_EXITS;
        let (risk, paper, ledger) = self.shared.parts()?;
        check_private_agent(ctx)?;
        let o = object(tool, args, &["max_slippage_bps"])?;
        let max_slippage_bps =
            opt_num(tool, o, "max_slippage_bps", 0.0, 10_000.0)?.unwrap_or(risk.max_slippage_bps);
        let account = risk.account.clone();
        let now = io.clock.now_ms();
        ledger
            .open_account(&account, paper.initial_cash_usd, now)
            .await?;
        let snap = ledger.snapshot(&account, now).await?;
        let ids: BTreeSet<String> = snap
            .account
            .open_positions()
            .map(|p| p.instrument.clone())
            .collect();
        let store = self.shared.store.as_deref();
        let marks = MarketRows::read(store, &ids, None, None).await.marks(&ids);
        let rules = risk.exits.rules();
        let mut positions = Vec::new();
        for p in snap.account.open_positions() {
            let raw = marks.get(&p.instrument).cloned().unwrap_or(Field::Absent);
            let mark = fresh_mark(&p.instrument, &raw, now, risk.max_data_age_ms.ctx);
            let exit_at_ms = snap.exit_at_ms.get(&p.instrument).copied();
            let reason = exit_due(p, &mark, exit_at_ms, now, &rules);
            let mut check = ExitCheck {
                instrument: p.instrument.clone(),
                qty: p.qty,
                avg_px: p.avg_px,
                opened_ms: p.opened_ms,
                exit_at_ms,
                pnl_bps: mark.value().and_then(|px| pnl_bps(p, *px)),
                mark_px: mark,
                reason,
                status: ExitStatus::Held,
                client_order_id: None,
                attempt: None,
                rule: None,
                filled_qty: None,
                fill_px: None,
                error: None,
            };
            if let Some(reason) = reason {
                self.close(ctx, io, p, reason, max_slippage_bps, &mut check)
                    .await;
            }
            positions.push(check);
        }
        let row = XmExits {
            account,
            positions,
            ts_ms: io.clock.now_ms(),
        };
        Ok(finish(store, tool, &row, row.ts_ms).await)
    }

    /// Close `p` for `reason` under its next exit id; the outcome lands in
    /// `check`.
    async fn close(
        &self,
        ctx: &ToolCtx<'_>,
        io: &ExecIo<'_>,
        p: &Position,
        reason: ExitReason,
        max_slippage_bps: f64,
        check: &mut ExitCheck,
    ) {
        let placed = async {
            let (risk, _, ledger) = self.shared.parts()?;
            let opened_ms = p
                .opened_ms
                .ok_or_else(|| anyhow!("open position {} has no opened_ms", p.instrument))?;
            let instrument = InstrumentId::parse(&p.instrument).map_err(|e| anyhow!("{e}"))?;
            let id = |n| exit_client_order_id(&risk.account, &p.instrument, reason, opened_ms, n);
            let attempt = first_unstored(|n| {
                let coid = id(n);
                async move {
                    let stored = ledger.order(&risk.account, &coid).await?;
                    Ok(stored.is_some())
                }
            })
            .await?;
            check.attempt = Some(attempt);
            check.client_order_id = Some(id(attempt));
            let order = exit_order(risk, instrument, id(attempt), max_slippage_bps);
            run_exec(&self.shared, ctx, io, order).await
        }
        .await;
        match placed {
            Ok(obs) => record(check, &obs),
            // Closed by another caller since the snapshot.
            Err(e) if format!("{e:#}").starts_with("no_position") => {
                check.status = ExitStatus::Flat;
            }
            Err(e) => {
                check.status = ExitStatus::Error;
                check.error = Some(format!("{e:#}"));
            }
        }
    }
}

/// A reduce-only market IOC of the whole position (`paper_close`'s order).
fn exit_order(
    risk: &RiskConfig,
    instrument: InstrumentId,
    client_order_id: String,
    max_slippage_bps: f64,
) -> ExecOrder {
    ExecOrder {
        tool: names::XM_EXITS,
        gate: ExecGate::Risk,
        limits: risk.limits(),
        instrument,
        // Ignored for a close: the position decides.
        side: Side::Sell,
        size: ExecSize::Close,
        kind: OrderKind::Market,
        limit_px: None,
        reduce_only: true,
        max_slippage_bps,
        strategy: None,
        hedge_instrument: None,
        opportunity_key: None,
        client_order_id: Some(client_order_id),
        exit_at_ms: None,
    }
}

/// The close's `paper_fill/1` row into `check`.
fn record(check: &mut ExitCheck, obs: &Observation) {
    let Ok(row) = obs.typed::<PaperFillRow>() else {
        check.status = ExitStatus::Error;
        check.error = Some(format!("unexpected row {}", obs.key));
        return;
    };
    let fill = row.fill.as_ref();
    check.rule = Some(row.gate.rule.clone());
    check.filled_qty = fill.map(|f| f.filled_qty);
    check.fill_px = fill.and_then(|f| f.avg_px);
    check.status = match fill.map(|f| f.status) {
        None => ExitStatus::Denied,
        Some(FillStatus::Filled) => ExitStatus::Filled,
        Some(FillStatus::Partial) => ExitStatus::Partial,
        Some(FillStatus::Rejected) => ExitStatus::Rejected,
    };
    check.error = match check.status {
        ExitStatus::Partial => Some(format!(
            "partial fill, the rest canceled: {}",
            fill.and_then(|f| f.reason).map_or("-", |r| r.as_str())
        )),
        s if s.failed() => obs.errors.first().map(|e| e.message.clone()),
        _ => None,
    };
}

/// The first attempt `n` (1-based) that `stored(n)` says is unstored.
/// Attempts are placed in order and a denied one stores nothing, so the
/// stored ones are 1..k: a galloping then binary search reads O(log k) of
/// them, and the attempt returned was read unstored. Also the weekend
/// fade's shadow exits (`weekend_fade.rs`).
pub(crate) async fn first_unstored<F, Fut>(stored: F) -> Result<u32>
where
    F: Fn(u32) -> Fut,
    Fut: Future<Output = Result<bool>>,
{
    // Invariant: `lo` = 0 or stored; `hi` unstored once the gallop ends.
    let (mut lo, mut hi) = (0u32, 1u32);
    while stored(hi).await? {
        lo = hi;
        if hi >= MAX_EXIT_ATTEMPTS {
            bail!("exit_attempts_exhausted: {hi} exit orders are stored for this position");
        }
        hi = hi.saturating_mul(2).min(MAX_EXIT_ATTEMPTS);
    }
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if stored(mid).await? {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Ok(hi)
}

#[async_trait]
impl Tool for XmExitsTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        ctx.scope.check_fs_write(ctx.workspace)?;
        let books = live_books(ctx, self.shared.store.clone());
        let io = ExecIo {
            clock: &SystemClock,
            books: &books,
            rand01: jitter01(),
        };
        let obs = self.check(args, ctx, &io).await?;
        Ok(ToolOutput::observed(obs, SystemClock.now_ms()))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Mutex;

    use serde_json::json;

    use super::*;
    use crate::adapters::outbound::tools::xm::exec_common::tests::{market_rows, Rig, NOW, TSLA};
    use crate::domain::market::InstrumentId;
    use crate::domain::observation::{ErrorClass, ObsStatus, ReadError};
    use crate::ports::book::ScriptedBooks;
    use crate::ports::observation::ObservationStore;
    use crate::ports::paper::PaperLedger;

    fn tool(shared: &XmShared) -> XmExitsTool {
        XmExitsTool {
            def: defs::def(names::XM_EXITS),
            shared: shared.clone(),
        }
    }

    fn io<'a>(rig: &'a Rig, books: &'a ScriptedBooks) -> ExecIo<'a> {
        ExecIo {
            clock: rig.clock.as_ref(),
            books,
            rand01: 0.5,
        }
    }

    /// Run `xm_exits` on the rig's clock and book as call `call_id`.
    async fn run(rig: &Rig, call_id: &str) -> Observation {
        tool(&rig.shared)
            .check(&json!({}), &rig.ctx(Some(call_id)), &io(rig, &rig.books))
            .await
            .unwrap()
    }

    fn exits(o: &Observation) -> XmExits {
        o.typed().unwrap()
    }

    /// A $20 TSLA buy (0.057 at 347.23) whose position is due at `exit_at_ms`.
    async fn open(rig: &Rig, exit_at_ms: Option<i64>) -> i64 {
        let mut buy = rig.buy(20.0);
        buy.exit_at_ms = exit_at_ms;
        let o = rig.run(buy, "entry:s:1").await.unwrap();
        assert_eq!(o.status, ObsStatus::Ok, "{}", o.render_text(NOW));
        let snap = rig.ledger.snapshot("xmarket", NOW + 1_000).await.unwrap();
        snap.account.positions[TSLA].opened_ms.unwrap()
    }

    /// `(client_order_id, status, reduce_only)` of every stored order.
    fn orders(rig: &Rig) -> Vec<(String, String, bool)> {
        let c = rusqlite::Connection::open(rig.dir.path().join("state/ledger.db")).unwrap();
        let mut s = c
            .prepare("SELECT client_order_id, status, reduce_only FROM orders ORDER BY id")
            .unwrap();
        s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    /// Past its deadline (stale mark and all) the position is closed under
    /// `exit:<account>:<instrument>:deadline:<opened_ms>`, once: the next
    /// run finds nothing open.
    #[tokio::test]
    async fn a_position_past_its_deadline_is_closed_once() {
        let rig = Rig::new(25).await;
        let opened = open(&rig, Some(NOW + 1_000)).await;
        // Not due yet.
        let o = run(&rig, "feed:xm_exits:1:0").await;
        let e = exits(&o);
        assert_eq!((e.n_due(), e.positions[0].status), (0, ExitStatus::Held));
        assert_eq!(o.status, ObsStatus::Ok, "{}", o.render_text(NOW));
        rig.clock.set(NOW + 2_000);
        let o = run(&rig, "feed:xm_exits:2:0").await;
        let coid = format!("exit:xmarket:{TSLA}:deadline:{opened}");
        assert_eq!(o.key, "xm_exits/1:xmarket");
        assert_eq!(o.status, ObsStatus::Ok, "{}", o.render_text(NOW));
        assert_eq!(
            o.headline,
            format!(
                "xm_exits account=xmarket open=1 due=1 closed=1 failed=0 deadline {TSLA} filled"
            )
        );
        let c = &exits(&o).positions[0];
        assert_eq!(
            (c.reason, c.status, c.attempt, c.client_order_id.as_deref()),
            (
                Some(ExitReason::Deadline),
                ExitStatus::Filled,
                Some(1),
                Some(coid.as_str())
            )
        );
        assert_eq!(c.filled_qty, Some(0.057));
        for (k, v) in [
            ("n_open", 1),
            ("n_due", 1),
            ("n_closed", 1),
            ("n_failed", 0),
        ] {
            assert_eq!(o.features[k], v, "{k}");
        }
        assert_eq!(
            orders(&rig),
            [
                ("entry:s:1".to_string(), "filled".to_string(), false),
                (coid.clone(), "filled".to_string(), true)
            ]
        );
        // The verdict names the tool and the feed's call.
        let line = rig.risk_lines().pop().unwrap();
        assert_eq!(
            (&line["tool"], &line["call_id"], &line["client_order_id"]),
            (
                &json!("xm_exits"),
                &json!("feed:xm_exits:2:0"),
                &json!(coid)
            )
        );
        // A retry of the same feed call: nothing open, nothing placed.
        let again = run(&rig, "feed:xm_exits:2:0").await;
        assert_eq!(again.features["n_open"], 0);
        assert_eq!(orders(&rig).len(), 2);
        assert_eq!(rig.risk_lines().len(), 2);
    }

    /// Take-profit fires on a fresh mark; the same price on a stale mark
    /// closes nothing (partial row, the mark named in `errors`).
    #[tokio::test]
    async fn take_profit_needs_a_fresh_mark() {
        let rig = Rig::new(25).await;
        open(&rig, None).await;
        // +201 bps over the 347.23 entry, 30 s old: stale (ctx limit 20 s).
        for r in market_rows(354.2, NOW - 30_000) {
            rig.store.put(&r).await.unwrap();
        }
        let o = run(&rig, "x:1").await;
        let e = exits(&o);
        assert_eq!(o.status, ObsStatus::Partial);
        assert_eq!((e.n_due(), e.n_stale_marks()), (0, 1));
        assert!(e.positions[0].mark_px.is_error() && e.positions[0].pnl_bps.is_none());
        assert_eq!(o.errors[0].field, format!("mark:{TSLA}"));
        assert_eq!(orders(&rig).len(), 1, "nothing closed");
        // The same price, fresh: take-profit. The close fills at the book.
        for r in market_rows(354.2, rig.clock.now_ms()) {
            rig.store.put(&r).await.unwrap();
        }
        let o = run(&rig, "x:2").await;
        let c = &exits(&o).positions[0];
        assert_eq!(
            (c.reason, c.status),
            (Some(ExitReason::TakeProfit), ExitStatus::Filled)
        );
        assert!(c.pnl_bps.unwrap() > 200.0, "{:?}", c.pnl_bps);
        assert!(c
            .client_order_id
            .as_deref()
            .unwrap()
            .starts_with(&format!("exit:xmarket:{TSLA}:take_profit:")));
    }

    /// A close rejected for lack of a book is retried next run under the
    /// next attempt id; the first id is never reused.
    #[tokio::test]
    async fn a_rejected_exit_is_retried_under_the_next_attempt() {
        let rig = Rig::new(25).await;
        let opened = open(&rig, Some(NOW + 500)).await;
        rig.clock.set(NOW + 1_000);
        let no_book = ScriptedBooks::new(
            rig.clock.clone(),
            InstrumentId::parse(TSLA).unwrap(),
            vec![],
        )
        .failing(ReadError::new("book", ErrorClass::Timeout, "slow"));
        let o = tool(&rig.shared)
            .check(&json!({}), &rig.ctx(Some("f:1")), &io(&rig, &no_book))
            .await
            .unwrap();
        let c = &exits(&o).positions[0];
        assert_eq!((c.status, c.attempt), (ExitStatus::Rejected, Some(1)));
        assert!(
            c.error
                .as_deref()
                .unwrap()
                .starts_with("rejected stale_book"),
            "{c:?}"
        );
        assert_eq!(o.status, ObsStatus::Error, "every due close failed");
        assert_eq!(o.features["n_failed"], 1);
        // The book is back: attempt 2 closes it.
        let o = run(&rig, "f:2").await;
        let c = &exits(&o).positions[0];
        let base = format!("exit:xmarket:{TSLA}:deadline:{opened}");
        assert_eq!(
            (c.status, c.client_order_id.clone()),
            (ExitStatus::Filled, Some(format!("{base}:2")))
        );
        let ids: Vec<(String, String)> = orders(&rig)
            .into_iter()
            .skip(1)
            .map(|(id, status, _)| (id, status))
            .collect();
        assert_eq!(
            ids,
            [
                (base.clone(), "rejected".to_string()),
                (format!("{base}:2"), "filled".to_string())
            ]
        );
    }

    /// A gate denial stores no order, so the next run judges the same id
    /// again; a caller that is not private is refused before anything.
    #[tokio::test]
    async fn a_denied_exit_keeps_its_id_and_callers_are_checked() {
        let rig = Rig::new(25).await;
        open(&rig, Some(NOW + 500)).await;
        rig.clock.set(NOW + 1_000);
        std::fs::write(rig.dir.path().join("KILL"), "").unwrap();
        let mut strict = rig.shared.clone();
        strict.risk.as_mut().unwrap().allow_reduce_degraded = false;
        for call in ["k:1", "k:2"] {
            let o = tool(&strict)
                .check(&json!({}), &rig.ctx(Some(call)), &io(&rig, &rig.books))
                .await
                .unwrap();
            let c = &exits(&o).positions[0];
            assert_eq!(
                (c.status, c.attempt, c.rule.as_deref()),
                (ExitStatus::Denied, Some(1), Some("kill_switch"))
            );
        }
        assert_eq!(orders(&rig).len(), 1, "denials store no order");
        assert_eq!(rig.rows()["risk_decisions"], 3, "two denied verdicts");
        // Refusals: a routable caller, an unknown argument, no [risk].
        let mut routable = Rig::new(25).await;
        routable.agent.description = Some("routable".into());
        let e = tool(&routable.shared)
            .check(
                &json!({}),
                &routable.ctx(Some("r:1")),
                &io(&routable, &routable.books),
            )
            .await
            .unwrap_err();
        assert!(e.to_string().starts_with("exec_agent_not_private"), "{e}");
        let e = tool(&rig.shared)
            .check(
                &json!({"all": true}),
                &rig.ctx(Some("r:2")),
                &io(&rig, &rig.books),
            )
            .await
            .unwrap_err();
        assert!(
            e.to_string().contains("unknown argument(s) [\"all\"]"),
            "{e}"
        );
        let mut none = rig.shared.clone();
        none.risk = None;
        let e = tool(&none)
            .check(&json!({}), &rig.ctx(Some("r:3")), &io(&rig, &rig.books))
            .await
            .unwrap_err();
        assert!(e.to_string().starts_with("risk_config_missing"), "{e}");
    }

    /// The attempt search: first unstored attempt, O(log k) reads, never a
    /// stored one.
    #[tokio::test]
    async fn the_attempt_search_finds_the_first_unstored_id() {
        for (stored, want, max_reads) in [
            (BTreeSet::new(), 1, 1),
            (BTreeSet::from([1]), 2, 3),
            ((1..=5).collect::<BTreeSet<u32>>(), 6, 6),
            ((1..=1_000).collect(), 1_001, 21),
            // Not a prefix (an id placed by hand): still an unstored one.
            (BTreeSet::from([1, 2, 5]), 3, 5),
        ] {
            let reads = Mutex::new(0);
            let got = first_unstored(|n| {
                *reads.lock().unwrap() += 1;
                let hit = stored.contains(&n);
                async move { Ok(hit) }
            })
            .await
            .unwrap();
            assert_eq!(got, want, "{stored:?}");
            assert!(!stored.contains(&got));
            let n = *reads.lock().unwrap();
            assert!(n <= max_reads, "{n} reads for {stored:?}");
        }
    }

    #[tokio::test]
    async fn execute_gates_the_scope_first() {
        let rig = Rig::new(25).await;
        let mut denied = rig.ctx(Some("s:1"));
        let no_fs = crate::domain::scope::ToolScope::default();
        denied.scope = &no_fs;
        let t = tool(&rig.shared);
        let e = t.execute(&json!({}), &denied).await.unwrap_err();
        assert!(e.to_string().contains("fs"), "{e}");
        assert_eq!(t.definition().name, "xm_exits");
    }
}
