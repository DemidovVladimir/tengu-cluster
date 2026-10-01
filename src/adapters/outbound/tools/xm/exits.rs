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
//! | Positions | the `[risk]` account's ledger snapshot (opened on first use): each position's exit deadline and fired TP / SL (`exit_triggers`) |
//! | Funding | the account's owed and due funding booked first at fresh `mkt_ctx/1` rates (`exec_common::accrue_due_funding`; closed positions owing hours too — review #10) |
//! | Marks | the positions' `mkt_ctx/1` rows in the store, never fetched; missing, older than `[risk] max_data_age_ms.ctx` or stamped > 1 s ahead ⇒ stale (`n_stale_marks`) |
//! | Due | `domain::xm::exits::exit_due`: deadline, max hold, a fired TP / SL (kept in the ledger until the position closes — a later stale mark never cancels it), TP / SL at the fresh mark — a TP / SL found due is recorded first (`PaperLedger::trigger_exit`) |
//! | Stale mark (review #6) | not due by time or a fired trigger: a close judged on the live book (`ExecOrder::tp_sl_on_book`) — after the latency the post-latency book's mid decides TP / SL (`book_mid`, `n_book_marks`); nothing fires ⇒ nothing placed, `held` |
//! | Close | per due position, in id order: `run_exec` with `ExecSize::Close` (sized from the position inside the ledger transaction: a closed position is never closed again), IOC bound `max_slippage_bps` (the arg, ≤ 500 bps — `exec::MAX_EXIT_SLIPPAGE_BPS`; default `[risk] max_slippage_bps` cut to 500), id `exit:<account>:<instrument>:<reason>:<opened_ms>` — when that id's order is already stored and the position is still open (rejected, partial), the first unstored attempt `…:<n>`; a denied attempt stores nothing and is judged again next run; the venue facts from the instrument's `mkt_instrument/1` row, else the ones kept with the position at its entry (`exec::order_venue_facts`: a purged or unreadable store still closes — review #6); exits never count toward, nor are denied by, `[risk] max_orders_per_min` |
//! | Retry (review #3) | the latest stored attempt decides (`domain::xm::exits::exit_retry`, read from the ledger — a restart keeps it): rejected for a reason that may pass ⇒ the next attempt 15 s, 30 s, 1 min … 15 min after it (`backoff`, `next_attempt_ms`); rejected for a final one (`delisted`, `invalid_order`, a lot / tick rule) ⇒ never placed again (`stuck`): one WARN line when it happens, the stored rejection + its verdict stay the marker, the operator decides |
//! | Row | `xm_exits/1:<account>` (ttl 0, `domain/xm/exits.rs::XmExits`): `n_open`, `n_due`, `n_closed`, `n_failed` (`backoff` and `stuck` too), `n_stale_marks`, `n_book_marks`, `n_stuck`; per position the full id, entry, deadline, mark (or book mid), P&L, reason, when a TP / SL fired, status, id, gate rule, fill and the next attempt time |
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
    accrue_due_funding, check_private_agent, exec, finish, live_books, run_exec, ExecGate, ExecIo,
    ExecOrder, ExecSize, Executed, MarketRows,
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
    ExitRetry, ExitRules, ExitStatus, ExitTrigger, XmExits, EXIT_RETRY_STEPS,
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
        // The open positions' rows, and those of closed ones owing funding.
        let rows = MarketRows::read(store, &snap.account.funding_ids(), None, None).await;
        let max_ctx_ms = risk.max_data_age_ms.ctx;
        accrue_due_funding(ledger.as_ref(), &snap.account, &rows, now, max_ctx_ms).await?;
        let marks = rows.marks(&ids);
        let rules = risk.exits.rules();
        let mut positions = Vec::new();
        for p in snap.account.open_positions() {
            let raw = marks.get(&p.instrument).cloned().unwrap_or(Field::Absent);
            let mark = fresh_mark(&p.instrument, &raw, now, max_ctx_ms);
            let exit_at_ms = snap.exit_at_ms.get(&p.instrument).copied();
            let fired = snap.exit_triggers.get(&p.instrument).copied();
            let reason = exit_due(p, &mark, exit_at_ms, fired.map(|t| t.reason), now, &rules);
            let mut check = ExitCheck {
                instrument: p.instrument.clone(),
                qty: p.qty,
                avg_px: p.avg_px,
                opened_ms: p.opened_ms,
                exit_at_ms,
                pnl_bps: mark.value().and_then(|px| pnl_bps(p, *px)),
                book_mid: None,
                triggered_ms: None,
                reason,
                status: ExitStatus::Held,
                client_order_id: None,
                attempt: None,
                rule: None,
                filled_qty: None,
                fill_px: None,
                error: None,
                next_attempt_ms: None,
                mark_px: mark,
            };
            match reason {
                Some(reason) => {
                    self.close(ctx, io, p, reason, fired, max_slippage_bps, &mut check)
                        .await
                }
                // Review #6: no fresh mark — TP / SL on the live book.
                None if check.mark_px.value().is_none() => {
                    self.close_on_book(ctx, io, p, &rules, max_slippage_bps, &mut check)
                        .await
                }
                None => {}
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
    /// `check`. A TP / SL not yet `fired` is recorded first (it stays due).
    #[allow(clippy::too_many_arguments)]
    async fn close(
        &self,
        ctx: &ToolCtx<'_>,
        io: &ExecIo<'_>,
        p: &Position,
        reason: ExitReason,
        fired: Option<ExitTrigger>,
        max_slippage_bps: f64,
        check: &mut ExitCheck,
    ) {
        let placed = async {
            let (risk, _, ledger) = self.shared.parts()?;
            let opened_ms = p
                .opened_ms
                .ok_or_else(|| anyhow!("open position {} has no opened_ms", p.instrument))?;
            let instrument = InstrumentId::parse(&p.instrument).map_err(|e| anyhow!("{e}"))?;
            // Review #6: a TP / SL that fires is kept in the ledger — due
            // until the position closes, whatever later marks say.
            let reason = match fired {
                Some(t) if t.reason == reason => {
                    check.triggered_ms = Some(t.at_ms);
                    reason
                }
                _ if reason.on_price() => {
                    let now = io.clock.now_ms();
                    let t = ledger
                        .trigger_exit(&risk.account, &p.instrument, opened_ms, reason, now)
                        .await?
                        .ok_or_else(|| {
                            anyhow!("no_position: {} closed since the snapshot", p.instrument)
                        })?;
                    check.triggered_ms = Some(t.at_ms);
                    check.reason = Some(t.reason);
                    t.reason
                }
                _ => reason,
            };
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

    /// Review #6: `p` has no fresh mark and is not due by time — its
    /// take-profit / stop-loss are judged on the mid of the book the close
    /// reads after its latency (`ExecOrder::tp_sl_on_book`). Nothing fires
    /// ⇒ nothing placed, `held`; one fires ⇒ recorded and closed under
    /// `exit:<account>:<instrument>:<reason>:<opened_ms>`. A failure before
    /// the book (no venue facts, the ledger) leaves it `held` with the
    /// error: the position is not known to be due.
    async fn close_on_book(
        &self,
        ctx: &ToolCtx<'_>,
        io: &ExecIo<'_>,
        p: &Position,
        rules: &ExitRules,
        max_slippage_bps: f64,
        check: &mut ExitCheck,
    ) {
        let done = async {
            let (risk, _, _) = self.shared.parts()?;
            let instrument = InstrumentId::parse(&p.instrument).map_err(|e| anyhow!("{e}"))?;
            let mut order = exit_order(risk, instrument, String::new(), max_slippage_bps);
            order.client_order_id = None;
            order.tp_sl_on_book = Some(*rules);
            exec(&self.shared, ctx, io, order).await
        }
        .await;
        match done {
            Ok(Executed::NotCalled { mid }) => {
                check.book_mid = mid.value().copied();
                check.pnl_bps = check.book_mid.and_then(|px| pnl_bps(p, px));
                if let Some(e) = mid.error() {
                    check.error = Some(format!(
                        "take-profit / stop-loss not judged: no book mid ({} {})",
                        e.class.as_str(),
                        e.message
                    ));
                }
            }
            Ok(Executed::Called { trigger, mid, row }) => {
                check.book_mid = Some(mid);
                check.pnl_bps = pnl_bps(p, mid);
                check.reason = Some(trigger.reason);
                check.triggered_ms = Some(trigger.at_ms);
                check.attempt = Some(1);
                check.client_order_id = row.typed::<PaperFillRow>().ok().map(|r| r.client_order_id);
                if let Some(account) = self.shared.risk.as_ref().map(|r| r.account.as_str()) {
                    warn_final_exit(account, &p.instrument, &row);
                }
                record(check, &row);
            }
            // A judged order always has an id from the book.
            Ok(Executed::Row(row)) => record(check, &row),
            Err(e) if format!("{e:#}").starts_with("no_position") => {
                check.status = ExitStatus::Flat;
            }
            Err(e) => {
                check.error = Some(format!("take-profit / stop-loss not judged: {e:#}"));
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
        tp_sl_on_book: None,
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
    use crate::application::observe::tests::MemStore;
    use crate::domain::book::fixture::tsla_book;
    use crate::domain::book::{L2Book, L2Level};
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

    /// Take-profit fires on a fresh mark. The same price on a stale mark
    /// fires nothing by itself: TP / SL are judged on the live book instead
    /// (here at the entry: held, nothing placed, no verdict; the stale mark
    /// still named in `errors`).
    #[tokio::test]
    async fn take_profit_needs_a_fresh_mark_or_the_book() {
        let rig = Rig::new(25).await;
        open(&rig, None).await;
        // +201 bps over the 347.23 entry, 30 s old: stale (ctx limit 20 s).
        for r in market_rows(354.2, NOW - 30_000) {
            rig.store.put(&r).await.unwrap();
        }
        let o = run(&rig, "x:1").await;
        let e = exits(&o);
        assert_eq!(o.status, ObsStatus::Partial);
        assert_eq!((e.n_due(), e.n_stale_marks(), e.n_book_marks()), (0, 1, 1));
        let c = &e.positions[0];
        assert!(c.mark_px.is_error());
        assert_eq!(c.status, ExitStatus::Held);
        let mid = c.book_mid.unwrap();
        assert!(
            (mid - 347.195).abs() < 1e-9,
            "the fixture book's mid: {mid}"
        );
        assert!(c.pnl_bps.unwrap() < 0.0, "at the book mid: {c:?}");
        assert_eq!(o.errors[0].field, format!("mark:{TSLA}"));
        assert_eq!(orders(&rig).len(), 1, "nothing closed");
        assert_eq!(
            rig.rows()["risk_decisions"],
            1,
            "nothing judged by the gate"
        );
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
        assert!(c.triggered_ms.is_some(), "recorded before the close");
        assert!(c
            .client_order_id
            .as_deref()
            .unwrap()
            .starts_with(&format!("exit:xmarket:{TSLA}:take_profit:")));
    }

    /// A two-level TSLA book around `mid`, 10 deep, stamped at the rig
    /// clock's now.
    fn book_at(rig: &Rig, mid: f64) -> ScriptedBooks {
        let level = |px: f64| L2Level { px, sz: 10.0, n: 1 };
        let book = L2Book::new(
            vec![level(mid - 0.05)],
            vec![level(mid + 0.05)],
            rig.clock.now_ms(),
        )
        .unwrap();
        ScriptedBooks::new(
            rig.clock.clone(),
            InstrumentId::parse(TSLA).unwrap(),
            vec![(0, book)],
        )
    }

    /// Review #6: with a stale mark the stop-loss is judged on the mid of
    /// the live book the close reads after its latency — a failed read
    /// judges nothing (held, nothing placed); 122 bps under the entry it
    /// fires, is recorded, and closes under the stop-loss id.
    #[tokio::test]
    async fn a_stop_loss_on_a_stale_mark_fires_on_the_live_book() {
        let rig = Rig::new(25).await;
        let opened = open(&rig, None).await;
        // The rig's rows are from NOW − 1 s: 61 s old, stale (limit 20 s).
        rig.clock.set(NOW + 60_000);
        let o = tool(&rig.shared)
            .check(&json!({}), &rig.ctx(Some("b:1")), &io(&rig, &no_book(&rig)))
            .await
            .unwrap();
        let c = &exits(&o).positions[0];
        assert_eq!(
            (c.status, c.reason, c.book_mid),
            (ExitStatus::Held, None, None)
        );
        assert!(c.error.as_deref().unwrap().contains("not judged"), "{c:?}");
        assert_eq!(orders(&rig).len(), 1, "nothing placed");
        let books = book_at(&rig, 343.0);
        let o = tool(&rig.shared)
            .check(&json!({}), &rig.ctx(Some("b:2")), &io(&rig, &books))
            .await
            .unwrap();
        let c = &exits(&o).positions[0];
        assert_eq!(
            (c.reason, c.status, c.book_mid),
            (Some(ExitReason::StopLoss), ExitStatus::Filled, Some(343.0)),
            "{c:?}"
        );
        assert!(c.mark_px.is_error() && c.triggered_ms.is_some(), "{c:?}");
        assert!(c.pnl_bps.unwrap() < -100.0, "{c:?}");
        let id = format!("exit:xmarket:{TSLA}:stop_loss:{opened}");
        assert_eq!(c.client_order_id.as_deref(), Some(id.as_str()));
        for (k, v) in [("n_book_marks", 1), ("n_closed", 1), ("n_stale_marks", 1)] {
            assert_eq!(o.features[k], v, "{k}");
        }
        let line = rig.risk_lines().pop().unwrap();
        assert_eq!(
            (&line["client_order_id"], &line["tool"]),
            (&json!(id), &json!("xm_exits"))
        );
        let snap = rig
            .ledger
            .snapshot("xmarket", rig.clock.now_ms())
            .await
            .unwrap();
        assert!(snap.account.positions[TSLA].is_flat());
    }

    /// Review #6: a stop-loss that fired and was rejected stays due — on a
    /// stale mark (the backoff holds it, never `held`) and with the price
    /// back inside the band (attempt 2 closes it).
    #[tokio::test]
    async fn a_fired_stop_loss_stays_due_until_the_position_closes() {
        let rig = Rig::new(25).await;
        let opened = open(&rig, None).await;
        rig.clock.set(NOW + 1_000);
        for r in market_rows(343.0, NOW + 900) {
            rig.store.put(&r).await.unwrap();
        }
        let o = tool(&rig.shared)
            .check(&json!({}), &rig.ctx(Some("t:1")), &io(&rig, &no_book(&rig)))
            .await
            .unwrap();
        let c = &exits(&o).positions[0];
        assert_eq!(
            (c.reason, c.status, c.attempt),
            (Some(ExitReason::StopLoss), ExitStatus::Rejected, Some(1))
        );
        let fired = rig
            .ledger
            .snapshot("xmarket", NOW + 2_000)
            .await
            .unwrap()
            .exit_triggers[TSLA];
        assert_eq!(fired.reason, ExitReason::StopLoss);
        assert_eq!(c.triggered_ms, Some(fired.at_ms));
        // The mark goes stale: still due, held back by the backoff.
        rig.clock.set(NOW + 5_000);
        for r in market_rows(343.0, NOW - 60_000) {
            rig.store.put(&r).await.unwrap();
        }
        let o = run(&rig, "t:2").await;
        let c = &exits(&o).positions[0];
        assert_eq!(
            (c.reason, c.status, c.book_mid),
            (Some(ExitReason::StopLoss), ExitStatus::Backoff, None)
        );
        assert!(c.mark_px.is_error());
        // The price back inside the band, fresh: still due — attempt 2.
        rig.clock.set(NOW + 20_000);
        for r in market_rows(350.0, NOW + 19_900) {
            rig.store.put(&r).await.unwrap();
        }
        let books = fresh_books(&rig);
        let o = tool(&rig.shared)
            .check(&json!({}), &rig.ctx(Some("t:3")), &io(&rig, &books))
            .await
            .unwrap();
        let c = &exits(&o).positions[0];
        let base = format!("exit:xmarket:{TSLA}:stop_loss:{opened}");
        assert_eq!(
            (c.reason, c.status, c.client_order_id.clone()),
            (
                Some(ExitReason::StopLoss),
                ExitStatus::Filled,
                Some(format!("{base}:2"))
            )
        );
        assert!(c.pnl_bps.unwrap() > 0.0, "the mark says no stop: {c:?}");
        assert_eq!(c.triggered_ms, Some(fired.at_ms), "fired once");
    }

    /// Review #6: the instrument row purged (the store keeps 7 days) or no
    /// store at all, a close still fills on the venue facts kept with the
    /// position at its entry; an entry still needs the row, and so does a
    /// position from before the kept facts.
    #[tokio::test]
    async fn a_close_without_the_instrument_row_uses_the_kept_facts() {
        let rig = Rig::new(25).await;
        open(&rig, Some(NOW + 500)).await;
        let kept = rig
            .ledger
            .snapshot("xmarket", NOW + 600)
            .await
            .unwrap()
            .facts[TSLA];
        assert_eq!((kept.sz_decimals, kept.at_ms), (3, NOW - 1_000));
        assert!((kept.fees.taker_bps - 0.9).abs() < 1e-12, "{kept:?}");
        // Purged: the store holds the ctx row only.
        let purged = Arc::new(MemStore::default());
        purged.put(&market_rows(347.2, NOW + 500)[0]).await.unwrap();
        let mut gone = rig.shared.clone();
        gone.store = Some(purged as Arc<dyn ObservationStore>);
        rig.clock.set(NOW + 1_000);
        let books = fresh_books(&rig);
        let o = tool(&gone)
            .check(&json!({}), &rig.ctx(Some("p:1")), &io(&rig, &books))
            .await
            .unwrap();
        let c = &exits(&o).positions[0];
        assert_eq!(
            (c.reason, c.status),
            (Some(ExitReason::Deadline), ExitStatus::Filled),
            "{c:?}"
        );
        let id = c.client_order_id.clone().unwrap();
        let r = rig
            .ledger
            .order("xmarket", &id)
            .await
            .unwrap()
            .unwrap()
            .result;
        let fee = r.filled_notional_usd * 0.9e-4;
        assert!((r.fee_usd - fee).abs() < 1e-12, "the kept fee: {r:?}");
        // An entry on that store is refused: it needs the row.
        let e = run_exec(
            &gone,
            &rig.ctx(Some("p:2")),
            &io(&rig, &books),
            rig.buy(20.0),
        )
        .await
        .unwrap_err();
        assert!(e.to_string().starts_with("missing:mkt_instrument"), "{e}");
        // No store at all: a close still fills.
        rig.clock.set(NOW + 2_000);
        rig.run(rig.buy(20.0), "entry:s:2").await.unwrap();
        let mut blind = rig.shared.clone();
        blind.store = None;
        let books = fresh_books(&rig);
        let o = run_exec(
            &blind,
            &rig.ctx(Some("n:1")),
            &io(&rig, &books),
            rig.close(),
        )
        .await
        .unwrap();
        let r: PaperFillRow = o.typed().unwrap();
        assert_eq!(
            (r.fill.unwrap().status, r.position_qty_after),
            (FillStatus::Filled, 0.0)
        );
        // A position without kept facts (an older ledger) needs the row.
        rig.clock.set(NOW + 3_000);
        rig.run(rig.buy(20.0), "entry:s:3").await.unwrap();
        rusqlite::Connection::open(rig.dir.path().join("state/ledger.db"))
            .unwrap()
            .execute_batch("UPDATE positions SET sz_decimals = NULL")
            .unwrap();
        let e = run_exec(&gone, &rig.ctx(Some("p:3")), &io(&rig, &books), rig.close())
            .await
            .unwrap_err();
        assert!(e.to_string().starts_with("missing:mkt_instrument"), "{e}");
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
