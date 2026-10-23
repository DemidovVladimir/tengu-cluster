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
#[path = "exits_tests.rs"]
mod tests;
