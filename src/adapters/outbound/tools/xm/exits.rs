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
//! | Marks | the positions' `mkt_ctx/1` rows in the store, never fetched; missing, older than `[risk] max_data_age_ms.ctx` or stamped > 1 s ahead ⇒ no stop-loss / take-profit for that position (`n_stale_marks`); deadline and max hold still apply |
//! | Close | per due position, in id order: `run_exec` with `ExecSize::Close` (sized from the position inside the ledger transaction: a closed position is never closed again), IOC bound `max_slippage_bps` (the arg, ≤ 500 bps — `exec::MAX_EXIT_SLIPPAGE_BPS`; default `[risk] max_slippage_bps` cut to 500), id `exit:<account>:<instrument>:<reason>:<opened_ms>` — when that id's order is already stored and the position is still open (rejected, partial), the first unstored attempt `…:<n>`; a denied attempt stores nothing and is judged again next run; each close needs the instrument's `mkt_instrument/1` row (keep `hl_ctx` running, else `missing:mkt_instrument`); exits never count toward, nor are denied by, `[risk] max_orders_per_min` |
//! | Retry (review #3) | the latest stored attempt decides (`domain::xm::exits::exit_retry`, read from the ledger — a restart keeps it): rejected for a reason that may pass ⇒ the next attempt 15 s, 30 s, 1 min … 15 min after it (`backoff`, `next_attempt_ms`); rejected for a final one (`delisted`, `invalid_order`, a lot / tick rule) ⇒ never placed again (`stuck`): one WARN line when it happens, the stored rejection + its verdict stay the marker, the operator decides |
//! | Row | `xm_exits/1:<account>` (ttl 0, `domain/xm/exits.rs::XmExits`): `n_open`, `n_due`, `n_closed`, `n_failed` (`backoff` and `stuck` too), `n_stale_marks`, `n_stuck`; per position the full id, entry, deadline, mark, P&L, reason, status, id, gate rule, fill and the next attempt time |
//!
//! Each close is its own `paper_fill/1` row and verdict (`ledger.db` +
//! `logs/risk.jsonl`, tool `xm_exits`, the feed's call id).

use std::collections::BTreeSet;
use std::future::Future;
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use serde_json::Value;
use tracing::warn;

use super::exec_common::{
    check_private_agent, finish, live_books, run_exec, ExecGate, ExecIo, ExecOrder, ExecSize,
    MarketRows,
};
use super::paper::{exit_bound_arg, object};
use super::{defs, XmShared};
use crate::adapters::outbound::clock::SystemClock;
use crate::adapters::outbound::rate_limit::jitter01;
use crate::config::risk::RiskConfig;
use crate::domain::book::Side;
use crate::domain::market::InstrumentId;
use crate::domain::message::ToolDef;
use crate::domain::observation::{Field, Observation};
use crate::domain::tools as names;
use crate::domain::xm::exec::{rejected_message, PaperFillRow, MAX_EXIT_SLIPPAGE_BPS};
use crate::domain::xm::exits::{
    exit_client_order_id, exit_due, exit_retry, pnl_bps, ExitAttempt, ExitCheck, ExitReason,
    ExitRetry, ExitStatus, XmExits, EXIT_RETRY_STEPS,
};
use crate::domain::xm::ledger::{fresh_mark, Position};
use crate::domain::xm::paper::{FillStatus, OrderKind};
use crate::ports::clock::Clock;
use crate::ports::paper::PaperLedger;
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
        let max_slippage_bps = exit_bound_arg(tool, o)?
            .unwrap_or_else(|| risk.max_slippage_bps.min(MAX_EXIT_SLIPPAGE_BPS));
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
                next_attempt_ms: None,
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

    /// Close `p` for `reason` under its next exit id — unless the retry
    /// rule holds it back (`backoff`, `stuck`); the outcome lands in
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
            let plan = plan_exit(ledger.as_ref(), &risk.account, &id, io.clock.now_ms()).await?;
            if plan.retry != ExitRetry::Now {
                held_back(check, &plan);
                return Ok(None);
            }
            check.attempt = Some(plan.next);
            check.client_order_id = Some(id(plan.next));
            let order = exit_order(risk, instrument, id(plan.next), max_slippage_bps);
            let obs = run_exec(&self.shared, ctx, io, order).await?;
            warn_final_exit(&risk.account, &p.instrument, &obs);
            Ok::<_, anyhow::Error>(Some(obs))
        }
        .await;
        match placed {
            Ok(Some(obs)) => record(check, &obs),
            Ok(None) => {}
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

/// The next attempt of one exit — ids `id(1)`, `id(2)`, … of `account` —
/// and whether the retry rule lets it go now (module table: Retry).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ExitPlan {
    /// The first unstored attempt (1-based).
    pub next: u32,
    pub retry: ExitRetry,
    /// The latest stored attempt: number, id, and its outcome (a
    /// rejection's `rejected <reason>: <message>`); `None` before the first.
    pub latest: Option<(u32, String, String)>,
}

/// [`ExitPlan`] from the ledger: the first unstored attempt, then the
/// latest stored ones (≤ `EXIT_RETRY_STEPS`) into `exit_retry` — derived
/// from stored attempts only, so a restart keeps the backoff. Also the
/// weekend fade's shadow exits.
pub(crate) async fn plan_exit(
    ledger: &dyn PaperLedger,
    account: &str,
    id: &(dyn Fn(u32) -> String + Sync),
    now_ms: i64,
) -> Result<ExitPlan> {
    let next = first_unstored(|n| {
        let coid = id(n);
        async move { Ok(ledger.order(account, &coid).await?.is_some()) }
    })
    .await?;
    let mut attempts = Vec::new();
    let mut latest = None;
    let oldest = next.saturating_sub(EXIT_RETRY_STEPS).max(1);
    for n in (oldest..next).rev() {
        let coid = id(n);
        let Some(o) = ledger.order(account, &coid).await? else {
            break;
        };
        if latest.is_none() {
            let outcome = match o.result.status {
                FillStatus::Rejected => rejected_message(&o.result),
                s => s.as_str().to_string(),
            };
            latest = Some((n, coid, outcome));
        }
        attempts.push(ExitAttempt {
            status: o.result.status,
            reason: o.result.reason,
            ts_ms: o.ts_ms,
        });
    }
    Ok(ExitPlan {
        next,
        retry: exit_retry(&attempts, now_ms),
        latest,
    })
}

impl ExitPlan {
    /// An exit held back this run: `backoff` / `stuck` and why; `None` when
    /// the next attempt goes now.
    pub(crate) fn held_back(&self) -> Option<(ExitStatus, String)> {
        let outcome = self.latest.as_ref().map_or("", |(_, _, o)| o.as_str());
        match self.retry {
            ExitRetry::Now => None,
            ExitRetry::Wait { at_ms, rejections } => Some((
                ExitStatus::Backoff,
                format!(
                    "{outcome}; {rejections} rejected in a row — attempt {} at {at_ms} ms",
                    self.next
                ),
            )),
            ExitRetry::Final { reason } => Some((
                ExitStatus::Stuck,
                format!(
                    "{outcome} — `{}` is final: never retried, the operator decides",
                    reason.as_str()
                ),
            )),
        }
    }
}

/// `check` of an exit the retry rule held back this run.
fn held_back(check: &mut ExitCheck, plan: &ExitPlan) {
    let Some((status, why)) = plan.held_back() else {
        return;
    };
    if let Some((attempt, coid, _)) = &plan.latest {
        check.attempt = Some(*attempt);
        check.client_order_id = Some(coid.clone());
    }
    if let ExitRetry::Wait { at_ms, .. } = plan.retry {
        check.next_attempt_ms = Some(at_ms);
    }
    check.status = status;
    check.error = Some(why);
}

/// One WARN line when an exit attempt was just rejected for a final reason
/// (review #3): never retried — the stored rejection and its verdict are the
/// marker. A replay logs nothing (the first caller did).
pub(crate) fn warn_final_exit(account: &str, instrument: &str, obs: &Observation) {
    let Ok(row) = obs.typed::<PaperFillRow>() else {
        return;
    };
    let Some(f) = row.fill.as_ref().filter(|_| !row.replayed) else {
        return;
    };
    if let Some(reason) = f
        .reason
        .filter(|r| f.status == FillStatus::Rejected && !r.is_transient())
    {
        warn!(
            account,
            instrument,
            client_order_id = %row.client_order_id,
            reason = reason.as_str(),
            message = %rejected_message(f),
            "exit rejected for a final reason: never retried — the position stays open until \
             the operator acts"
        );
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
/// fade's shadow exits and entry attempts (`weekend_fade.rs`).
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
    use crate::domain::book::fixture::tsla_book;
    use crate::domain::market::{InstrumentId, MarketCtx};
    use crate::domain::observation::{ErrorClass, ObsSource, ObsStatus, ReadError};
    use crate::domain::xm::risk::CheckStatus;
    use crate::ports::book::ScriptedBooks;
    use crate::ports::observation::ObservationStore;

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

    /// The fixture book stamped at the rig clock's now (a fresh read).
    fn fresh_books(rig: &Rig) -> ScriptedBooks {
        let mut book = tsla_book();
        book.venue_ts_ms = rig.clock.now_ms();
        ScriptedBooks::new(
            rig.clock.clone(),
            InstrumentId::parse(TSLA).unwrap(),
            vec![(0, book)],
        )
    }

    /// A book read that times out after the latency.
    fn no_book(rig: &Rig) -> ScriptedBooks {
        ScriptedBooks::new(
            rig.clock.clone(),
            InstrumentId::parse(TSLA).unwrap(),
            vec![],
        )
        .failing(ReadError::new("book", ErrorClass::Timeout, "slow"))
    }

    /// A close rejected for lack of a book waits 15 s (`backoff`), then is
    /// retried under the next attempt id; the first id is never reused.
    #[tokio::test]
    async fn a_rejected_exit_is_retried_under_the_next_attempt() {
        let rig = Rig::new(25).await;
        let opened = open(&rig, Some(NOW + 500)).await;
        rig.clock.set(NOW + 1_000);
        let no_book = no_book(&rig);
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
        let base = format!("exit:xmarket:{TSLA}:deadline:{opened}");
        let rejected_at = rig
            .ledger
            .order("xmarket", &base)
            .await
            .unwrap()
            .unwrap()
            .ts_ms;
        // The next run, too soon: held back, nothing placed.
        let o = run(&rig, "f:2").await;
        let c = &exits(&o).positions[0];
        assert_eq!(
            (c.status, c.attempt, c.next_attempt_ms),
            (ExitStatus::Backoff, Some(1), Some(rejected_at + 15_000))
        );
        assert!(
            c.error.as_deref().unwrap().contains("1 rejected in a row"),
            "{c:?}"
        );
        assert_eq!(orders(&rig).len(), 2);
        // 15 s later, the book back: attempt 2 closes it.
        rig.clock.set(rejected_at + 15_000);
        let books = fresh_books(&rig);
        let o = tool(&rig.shared)
            .check(&json!({}), &rig.ctx(Some("f:3")), &io(&rig, &books))
            .await
            .unwrap();
        let c = &exits(&o).positions[0];
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
        // Review #9: the IOC bound of an exit is at most 500 bps.
        let e = tool(&rig.shared)
            .check(
                &json!({"max_slippage_bps": 501}),
                &rig.ctx(Some("r:4")),
                &io(&rig, &rig.books),
            )
            .await
            .unwrap_err();
        assert!(e.to_string().contains("≤ 500"), "{e}");
    }

    /// Review #3, the reviewer's probe asserting the fix: a close that keeps
    /// failing (no book after the latency), run every 5 s, is placed 15 s,
    /// then 30 s … apart — `backoff` in between; no run is denied
    /// `order_rate`, the exits never count toward it, and an unrelated entry
    /// afterwards passes the rate rule. Each run is a new tool: the backoff
    /// comes from the ledger (a restart keeps it).
    #[tokio::test]
    async fn a_failing_exit_backs_off_and_never_uses_the_order_budget() {
        let rig = Rig::new(25).await;
        open(&rig, Some(NOW + 500)).await;
        let no_book = no_book(&rig);
        let mut runs = Vec::new();
        for i in 0..7i64 {
            rig.clock.set(NOW + 1_000 + i * 5_000);
            let o = tool(&rig.shared)
                .check(
                    &json!({}),
                    &rig.ctx(Some(&format!("f:{i}"))),
                    &io(&rig, &no_book),
                )
                .await
                .unwrap();
            let c = &exits(&o).positions[0];
            runs.push((c.status, c.rule.clone(), c.attempt));
        }
        use ExitStatus::{Backoff, Rejected};
        let statuses: Vec<ExitStatus> = runs.iter().map(|r| r.0).collect();
        // Attempt 1 at +1.25 s; attempt 2 at +21.25 s (≥ 15 s later); the
        // third waits 30 s.
        assert_eq!(
            statuses,
            [Rejected, Backoff, Backoff, Backoff, Rejected, Backoff, Backoff],
            "{runs:?}"
        );
        assert_eq!(runs[4].2, Some(2));
        assert!(
            runs.iter()
                .all(|(_, rule, _)| rule.as_deref() != Some("order_rate")),
            "{runs:?}"
        );
        assert_eq!(orders(&rig).len(), 3, "the entry + two exit attempts");
        // An unrelated entry: fresh rows and book; the rate rule passes.
        let now = rig.clock.now_ms();
        for r in market_rows(347.2, now - 500) {
            rig.store.put(&r).await.unwrap();
        }
        let books = fresh_books(&rig);
        let o = run_exec(
            &rig.shared,
            &rig.ctx(Some("entry:s:2")),
            &io(&rig, &books),
            rig.buy(20.0),
        )
        .await
        .unwrap();
        let r: PaperFillRow = o.typed().unwrap();
        assert_eq!(r.gate.rule, "ok", "{:?}", r.gate);
        let d = rig.ledger.decisions("xmarket", 1).await.unwrap();
        let rate = d[0].verdict.check("order_rate").unwrap();
        assert_eq!(rate.status, CheckStatus::Pass, "{rate:?}");
        assert!(rate.detail.starts_with("1 entries"), "{rate:?}");
    }

    /// Review #3: an exit rejected for a final reason (`delisted`) is never
    /// placed again — `stuck` on every later run, also after a restart (a
    /// new tool on the same ledger); one stored attempt and one verdict line
    /// stay the operator's marker.
    #[tokio::test]
    async fn a_final_rejection_stops_the_retries() {
        let rig = Rig::new(25).await;
        let opened = open(&rig, Some(NOW + 500)).await;
        rig.clock.set(NOW + 1_000);
        let gone = MarketCtx::not_found(InstrumentId::parse(TSLA).unwrap(), NOW + 900);
        rig.store
            .put(&Observation::of(
                names::HL_CTX,
                &gone,
                NOW + 900,
                5_000,
                ObsSource::Live,
            ))
            .await
            .unwrap();
        let o = run(&rig, "d:1").await;
        let c = &exits(&o).positions[0];
        assert_eq!((c.status, c.attempt), (ExitStatus::Rejected, Some(1)));
        assert!(
            c.error.as_deref().unwrap().starts_with("rejected delisted"),
            "{c:?}"
        );
        let base = format!("exit:xmarket:{TSLA}:deadline:{opened}");
        rig.clock.set(NOW + 3_600_000);
        for call in ["d:2", "d:3"] {
            let o = run(&rig, call).await;
            let c = &exits(&o).positions[0];
            assert_eq!(
                (c.status, c.attempt, c.client_order_id.as_deref()),
                (ExitStatus::Stuck, Some(1), Some(base.as_str()))
            );
            assert!(
                c.error.as_deref().unwrap().contains("`delisted` is final"),
                "{c:?}"
            );
            assert_eq!(o.features["n_stuck"], 1);
            assert_eq!(o.status, ObsStatus::Error, "the position stays open");
        }
        assert_eq!(orders(&rig).len(), 2, "the entry + one exit attempt");
        assert_eq!(rig.risk_lines().len(), 2, "one verdict line for the exit");
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
