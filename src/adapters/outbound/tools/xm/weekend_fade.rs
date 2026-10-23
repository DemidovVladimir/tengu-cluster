//! `xm_weekend_fade` — the weekend-fade rule W (`x-weekend-fade-strategy`;
//! pure rule `domain/xm/weekend_fade.rs`, knobs `[xmarket.weekend_fade]`).
//! One call = one step of the window's state machine, idempotent; run by a
//! `kind = "tool"` feed every 60 s (no LLM, no Jev), any engine may call it
//! too. An exec tool: private agents only. No arguments — time comes from
//! the `Clock` port (`SystemClock` live), so no caller can move it.
//!
//! | Step (every call) | Rule |
//! |---|---|
//! | Refuse | no `[risk]` / `[paper]` / ledger (`risk_config_missing` · `state_dir_missing` · `ledger_unavailable`); no `[xmarket.weekend_fade]` or its calendar not an `exchange` row (`weekend_fade_config_missing`); a caller that is not a private agent (`exec_agent_not_private`); any argument |
//! | Funding | both accounts (`[risk] account`, `shadow_account`) book the hourly funding they owe — open positions, and closed ones with hours owed (settled at a stale rate, review #10) — at a fresh `mkt_ctx/1` rate + oracle |
//! | Shadow exits | every open shadow position whose `exit_at_ms` passed, 4 at a time: a reduce-only market IOC of the whole position through the shadow gate (`risk::evaluate_shadow`; paper only — `shadow_not_paper` otherwise), IOC bound `max_slippage_bps` (≤ 500 bps, `exec::MAX_EXIT_SLIPPAGE_BPS`), id `exit:<shadow account>:<full id>:deadline:<opened_ms>` (`…:<n>` once a rejected / partial attempt is stored), the exits' retry rule (`exits::plan_exit`: `backoff` 15 s … 15 min after a rejection, `stuck` after a final one). The capped positions close through `xm_exits` (reason `deadline`) — never here |
//! | Previous window | its `xm_weekend/1` row, still `entered` / `closing`: each fade not filled in the row read from the ledger (below); open quantities from both ledgers; once every filled name is flat, `closed` with the P&L |
//! | Current window, before the entry | `waiting` (`next_entry_s`) |
//! | … entry ≤ now < entry + `entry_lateness_max_secs`, no snapshot | the snapshot: anchor prices from the history as of the anchor (`mkt_ctx/1` mid, else mark, ≤ `anchor_max_age_secs` old), entry prices from the store (≤ `entry_max_age_secs` old), signals, the capped set, each name's ledger bases — kept in the store first (compare-and-swap: one snapshot per window, across processes). Kept only complete (`WeekendFade::complete`): while a name (not excluded, with an anchor) has no entry row fresher than `entry_max_age_secs` (`stale`) and now < entry + 120 s (≤ half the lateness, `weekend_fade::snapshot_deadline_ms`), or no name is eligible, nothing is kept (an `error` row) and the next call reads again; from that deadline the `stale` names are left out. A fresh row without a usable price (`missing_entry`) holds nothing back |
//! | … then, and on later calls within the lateness | per fade not filled in the row, its attempts in the ledger (`fade_attempt_id`: the id, then `<id>:2`, `<id>:3` …): the latest stored one is its outcome — a fill or a partial fill is never placed again, a final rejection stays; with none, or a transient rejection (`FillReason::is_transient`), the next attempt is placed — unless the account holds the name with no fill under those ids (`position_open`, not faded). Placing writes the name's `xm_weekend_signal/1` row (the opportunity row, stamped at the snapshot, TTL = the lateness; it backs the fade only — its side, `overreaction`, ≤ `capped_notional_usd`, review #8), then the capped fades, largest \|s\| first (`run_exec`, the `[risk]` gate, `capped_notional_usd`, id `fade:<account>:<full id>:<anchor date>`, `exit_at_ms` = the exit, strategy `overreaction`), then the shadow fades of every eligible name, 4 at a time (the shadow gate, `shadow_notional_usd`, id `fade-shadow:<shadow account>:<full id>:<anchor date>`). A denial or a refusal stores nothing: the same attempt is judged again next call. Two callers place the same attempt id: the ledger keeps one (`UNIQUE (account, client_order_id)` in one `BEGIN IMMEDIATE`), the other replays it |
//! | … after the lateness, with a snapshot | no placement; each fade not filled in the row is read from the ledger (a row save lost to a crash or a concurrent call still counts its fills, P&L and closes) |
//! | … entry + lateness ≤ now, no snapshot | `missed_entry`: no late entry |
//! | Row | `xm_weekend/1:<anchor date>` (TTL 120 s): the previous window's while it closes and the current one waits, else the current one; both are stored |
//!
//! Every order is its own `paper_fill/1` row and verdict (`ledger.db` +
//! `logs/risk.jsonl`, tool `xm_weekend_fade`, the feed's call id).

use std::collections::BTreeSet;
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::Value;

use super::exec_common::{
    accrue_due_funding, check_private_agent, live_books, run_exec, ExecGate, ExecIo, ExecOrder,
    ExecSize, MarketRows, PaperFills, SHADOW_NOT_PAPER,
};
use super::exits::{first_unstored, plan_exit, warn_final_exit};
use super::paper::object;
use super::{defs, XmShared};
use crate::adapters::outbound::clock::SystemClock;
use crate::adapters::outbound::rate_limit::jitter01;
use crate::adapters::outbound::tools::hyperliquid::store_live;
use crate::config::risk::RiskConfig;
use crate::config::xmarket::WeekendFadeConfig;
use crate::domain::book::Side;
use crate::domain::calendar::{ExchangeCalendar, WeekendWindow};
use crate::domain::market::{InstrumentId, MarketCtx};
use crate::domain::message::ToolDef;
use crate::domain::observation::{ErrorClass, Field, ObsSource, Observation, Observed, ReadError};
use crate::domain::tools as names;
use crate::domain::xm::exec::{rejected_message, PaperFillRow};
use crate::domain::xm::exits::{exit_client_order_id, ExitReason};
use crate::domain::xm::ledger::{stamp_age_ms, PaperAccount, Position};
use crate::domain::xm::paper::{FillReason, FillResult, FillStatus, OrderKind};
use crate::domain::xm::risk::RiskLimits;
use crate::domain::xm::weekend_fade::{
    anchor_date, capped_order_id, fade_attempt_id, fade_window, position_net_usd,
    previous_fade_window, price_point, select_capped, shadow_order_id, CtxRow, FadeName, FadeOrder,
    FadePhase, FadeSignal, PricePoint, Signal, Skip, WeekendFade, FADE_STRATEGY,
};
use crate::ports::clock::Clock;
use crate::ports::history::HistoryStore;
use crate::ports::observation::ObservationStore;
use crate::ports::paper::{PaperLedger, Placement};
use crate::ports::tool::{Tool, ToolCtx, ToolOutput};

/// Refusal: no `[xmarket.weekend_fade]`, or its calendar is unusable.
pub(crate) const WEEKEND_FADE_CONFIG_MISSING: &str = "weekend_fade_config_missing";
/// How long a window row stays fresh in the store (the feed runs every 60 s).
pub(crate) const WINDOW_TTL_MS: u64 = 120_000;
/// Shadow fades (and shadow exits) placed at once: their latency and book
/// reads overlap.
const SHADOW_CONCURRENCY: usize = 4;
/// Compare-and-swap attempts for the entry snapshot.
const CLAIM_ATTEMPTS: usize = 3;

/// `[xmarket.weekend_fade]` with its calendar built.
pub(crate) struct FadeSetup {
    pub config: WeekendFadeConfig,
    /// The `exchange` calendar `config.calendar` names, or why there is none.
    pub calendar: std::result::Result<ExchangeCalendar, String>,
}

pub(crate) fn tools(shared: &XmShared) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(XmWeekendFadeTool {
        def: defs::def(names::XM_WEEKEND_FADE),
        shared: shared.clone(),
    })]
}

pub(crate) struct XmWeekendFadeTool {
    def: ToolDef,
    shared: XmShared,
}

/// One call's sections and stores.
struct Env<'a> {
    risk: &'a RiskConfig,
    ledger: &'a dyn PaperLedger,
    store: Option<&'a dyn ObservationStore>,
    history: Option<&'a dyn HistoryStore>,
    cfg: &'a WeekendFadeConfig,
    cal: &'a ExchangeCalendar,
}

impl Env<'_> {
    fn shadow_limits(&self) -> RiskLimits {
        RiskLimits {
            account: self.cfg.shadow_account.clone(),
            ..self.risk.limits()
        }
    }

    /// The shadow gate — paper only (review #5): no `PaperFills` proof
    /// (`[risk] mode` other than `paper`) ⇒ `shadow_not_paper`.
    fn shadow_gate(&self) -> Result<ExecGate> {
        let paper = PaperFills::of(self.risk).ok_or_else(|| {
            anyhow!(
                "{SHADOW_NOT_PAPER}: the shadow ledger fills on the paper engine only ([risk] \
                 mode = \"paper\")"
            )
        })?;
        Ok(ExecGate::Shadow {
            initial_cash_usd: self.cfg.shadow_initial_cash_usd,
            paper,
        })
    }

    fn row_key(&self, w: &WeekendWindow) -> String {
        Observation::key_for(WeekendFade::SCHEMA, &anchor_date(w))
    }
}

fn secs_ms(secs: u64) -> u64 {
    secs.saturating_mul(1000)
}

fn ms_i64(ms: u64) -> i64 {
    i64::try_from(ms).unwrap_or(i64::MAX)
}

impl XmWeekendFadeTool {
    /// The module table on `io`'s clock and books.
    pub(crate) async fn step(
        &self,
        args: &Value,
        ctx: &ToolCtx<'_>,
        io: &ExecIo<'_>,
    ) -> Result<Observation> {
        let tool = names::XM_WEEKEND_FADE;
        let (risk, paper, ledger) = self.shared.parts()?;
        check_private_agent(ctx)?;
        object(tool, args, &[])?;
        let setup = self.shared.fade.as_deref().ok_or_else(|| {
            anyhow!(
                "{WEEKEND_FADE_CONFIG_MISSING}: this sandbox has no [xmarket.weekend_fade] section"
            )
        })?;
        let cal = setup
            .calendar
            .as_ref()
            .map_err(|why| anyhow!("{WEEKEND_FADE_CONFIG_MISSING}: {why}"))?;
        let env = Env {
            risk,
            ledger: ledger.as_ref(),
            store: self.shared.store.as_deref(),
            history: self.shared.history.as_deref(),
            cfg: &setup.config,
            cal,
        };
        let now = io.clock.now_ms();
        ledger
            .open_account(&risk.account, paper.initial_cash_usd, now)
            .await?;
        ledger
            .open_account(
                &env.cfg.shadow_account,
                env.cfg.shadow_initial_cash_usd,
                now,
            )
            .await?;
        for account in [risk.account.as_str(), env.cfg.shadow_account.as_str()] {
            book_funding(&env, account, now).await?;
        }
        let exits = self.close_due_shadow(&env, ctx, io).await?;
        let mut prev = self.previous(&env, io).await?;
        let mut cur = self.current(&env, ctx, io).await?;
        let shown = match prev.as_mut() {
            Some(p) if cur.phase == FadePhase::Waiting => p,
            _ => &mut cur,
        };
        shown.shadow_exits = exits;
        let shown = shown.clone();
        if let Some(p) = &prev {
            save(env.store, p).await;
        }
        save(env.store, &cur).await;
        Ok(Observation::of(
            tool,
            &shown,
            shown.ts_ms,
            WINDOW_TTL_MS,
            ObsSource::Live,
        ))
    }

    /// Close every open shadow position past its deadline (module table).
    async fn close_due_shadow(
        &self,
        env: &Env<'_>,
        ctx: &ToolCtx<'_>,
        io: &ExecIo<'_>,
    ) -> Result<Vec<FadeOrder>> {
        let now = io.clock.now_ms();
        let snap = env.ledger.snapshot(&env.cfg.shadow_account, now).await?;
        let due: Vec<Position> = snap
            .account
            .open_positions()
            .filter(|p| {
                snap.exit_at_ms
                    .get(&p.instrument)
                    .is_some_and(|t| *t <= now)
            })
            .cloned()
            .collect();
        Ok(futures::stream::iter(due)
            .map(|p| async move { self.close_shadow(env, ctx, io, &p).await })
            .buffered(SHADOW_CONCURRENCY)
            .collect()
            .await)
    }

    async fn close_shadow(
        &self,
        env: &Env<'_>,
        ctx: &ToolCtx<'_>,
        io: &ExecIo<'_>,
        p: &Position,
    ) -> FadeOrder {
        let shadow = env.cfg.shadow_account.as_str();
        let base = p.opened_ms.map(|opened| {
            exit_client_order_id(shadow, &p.instrument, ExitReason::Deadline, opened, 1)
        });
        let placed = async {
            let opened_ms = p
                .opened_ms
                .ok_or_else(|| anyhow!("open position {} has no opened_ms", p.instrument))?;
            let instrument = InstrumentId::parse(&p.instrument).map_err(|e| anyhow!("{e}"))?;
            let id =
                |n| exit_client_order_id(shadow, &p.instrument, ExitReason::Deadline, opened_ms, n);
            // The exits' retry rule (review #3): backoff after a rejection,
            // never again after a final one.
            let plan = plan_exit(env.ledger, shadow, &id, io.clock.now_ms()).await?;
            if let Some((status, why)) = plan.held_back() {
                let coid = plan.latest.as_ref().map_or("-", |(_, c, _)| c.as_str());
                let mut held = not_placed(&p.instrument, coid, anyhow!("{why}"));
                held.status = status.as_str().to_string();
                return Ok(held);
            }
            let coid = id(plan.next);
            let order = ExecOrder {
                tool: names::XM_WEEKEND_FADE,
                gate: env.shadow_gate()?,
                limits: env.shadow_limits(),
                instrument,
                // Ignored for a close: the position decides.
                side: Side::Sell,
                size: ExecSize::Close,
                kind: OrderKind::Market,
                limit_px: None,
                reduce_only: true,
                max_slippage_bps: env.cfg.max_slippage_bps,
                strategy: None,
                hedge_instrument: None,
                opportunity_key: None,
                client_order_id: Some(coid.clone()),
                exit_at_ms: None,
                tp_sl_on_book: None,
            };
            let result = run_exec(&self.shared, ctx, io, order).await;
            if let Ok(obs) = &result {
                warn_final_exit(shadow, &p.instrument, obs);
            }
            Ok::<_, anyhow::Error>(fade_order(&p.instrument, &coid, result))
        }
        .await;
        placed.unwrap_or_else(|e| not_placed(&p.instrument, base.as_deref().unwrap_or("-"), e))
    }

    /// The current window's step (module table).
    async fn current(
        &self,
        env: &Env<'_>,
        ctx: &ToolCtx<'_>,
        io: &ExecIo<'_>,
    ) -> Result<WeekendFade> {
        let now = io.clock.now_ms();
        let cfg = env.cfg;
        let w = fade_window(env.cal, now).ok_or_else(|| {
            anyhow!(
                "no_window: calendar `{}` reaches no Saturday + Sunday break",
                cfg.calendar
            )
        })?;
        let new_row = |phase| {
            WeekendFade::new(
                phase,
                w,
                &env.risk.account,
                &cfg.shadow_account,
                cfg.universe.len(),
                cfg.exclude.len(),
                cfg.entry_lateness_max_secs,
                now,
            )
        };
        if now < w.entry_ms {
            return Ok(new_row(FadePhase::Waiting));
        }
        let lateness_ms = secs_ms(cfg.entry_lateness_max_secs);
        let entry_open = now < w.entry_ms.saturating_add(ms_i64(lateness_ms));
        match read_row(env.store, &env.row_key(&w)).await? {
            Some((mut row, _)) if row.phase.has_snapshot() => {
                let place = entry_open.then_some((ctx, io));
                self.settle(env, &mut row, place).await?;
                refresh(env, &mut row, io.clock.now_ms()).await?;
                Ok(row)
            }
            Some((mut row, _)) if row.phase == FadePhase::MissedEntry => {
                row.late_s = Some((now - w.entry_ms) / 1000);
                row.ts_ms = now;
                Ok(row)
            }
            _ if !entry_open => {
                let mut row = new_row(FadePhase::MissedEntry);
                row.late_s = Some((now - w.entry_ms) / 1000);
                Ok(row)
            }
            stored => {
                let mut row = snapshot(env, &w, now).await?;
                if row.n_eligible() == 0 || !row.complete() {
                    // An `error` row: nothing kept, the next call reads again.
                    return Ok(row);
                }
                claim(env, &mut row, stored.map(|(_, at)| at)).await?;
                self.settle(env, &mut row, Some((ctx, io))).await?;
                refresh(env, &mut row, io.clock.now_ms()).await?;
                Ok(row)
            }
        }
    }

    /// The previous window's row while it still closes: its fades read from
    /// the ledger, refreshed.
    async fn previous(&self, env: &Env<'_>, io: &ExecIo<'_>) -> Result<Option<WeekendFade>> {
        let now = io.clock.now_ms();
        let Some(w) = previous_fade_window(env.cal, now) else {
            return Ok(None);
        };
        let Some((mut row, _)) = read_row(env.store, &env.row_key(&w)).await? else {
            return Ok(None);
        };
        if !matches!(row.phase, FadePhase::Entered | FadePhase::Closing) {
            return Ok(None);
        }
        self.settle(env, &mut row, None).await?;
        refresh(env, &mut row, now).await?;
        Ok(Some(row))
    }

    /// Settle every fade of `row` not filled in it with the ledgers (module
    /// table): its latest stored attempt is its outcome; with `place`
    /// (inside the lateness) the next attempt is placed when none is stored
    /// or the latest is a transient rejection — unless the account holds
    /// the name with no fill under the fade's ids (`position_open`).
    async fn settle(
        &self,
        env: &Env<'_>,
        row: &mut WeekendFade,
        place: Option<(&ToolCtx<'_>, &ExecIo<'_>)>,
    ) -> Result<()> {
        let cfg = env.cfg;
        let exit_at_ms = row.window.exit_ms;
        // Signal rows are stamped at the snapshot.
        let stamp = row.entered_at_ms.unwrap_or(row.ts_ms);
        // Both accounts (shadow, capped) read before any attempt: a fill
        // landing in between is then a stored attempt, never a position
        // someone else opened.
        let held = match place {
            Some((_, io)) => {
                let now = io.clock.now_ms();
                let shadow = env.ledger.snapshot(&cfg.shadow_account, now).await?;
                let capped = env.ledger.snapshot(&env.risk.account, now).await?;
                Some((shadow.account, capped.account))
            }
            None => None,
        };

        // Capped: the snapshot's set, largest |s| first, ties by id.
        let mut picks: Vec<(usize, Signal)> = row
            .names
            .iter()
            .enumerate()
            .filter(|(_, n)| n.capped)
            .filter_map(|(i, n)| Some((i, n.signal()?)))
            .collect();
        picks.sort_by(|(_, a), (_, b)| {
            b.s_bps
                .abs()
                .total_cmp(&a.s_bps.abs())
                .then_with(|| a.instrument.cmp(&b.instrument))
        });
        let account = env.risk.account.as_str();
        for (i, signal) in picks {
            if row.names[i]
                .capped_order
                .as_ref()
                .is_some_and(FadeOrder::filled)
            {
                continue;
            }
            let id = signal.instrument.clone();
            let base = capped_order_id(account, &id, &row.anchor_date);
            let a = attempts(env, account, &base, &id).await?;
            let (Some(coid), Some((ctx, io)), Some((_, capped))) = (a.next, place, &held) else {
                if let Some(o) = a.latest {
                    row.names[i].capped_order = Some(o);
                }
                continue;
            };
            let outcome = if holds(capped, &id) {
                position_open(&id, &coid, account)
            } else {
                let key = write_signal(env, &row.anchor_date, &signal, true, stamp).await;
                let order = FadeEntry {
                    gate: ExecGate::Risk,
                    limits: env.risk.limits(),
                    notional_usd: cfg.capped_notional_usd,
                    opportunity_key: key,
                    exit_at_ms,
                };
                match order.build(env, &signal, &coid) {
                    Ok(order) => {
                        fade_order(&id, &coid, run_exec(&self.shared, ctx, io, order).await)
                    }
                    Err(e) => not_placed(&id, &coid, e),
                }
            };
            row.names[i].capped_order = Some(outcome);
        }

        // Shadow: every eligible name, a few at a time.
        let account = cfg.shadow_account.as_str();
        let mut todo: Vec<(usize, String, ExecOrder)> = Vec::new();
        for i in 0..row.names.len() {
            let n = &row.names[i];
            if n.shadow.as_ref().is_some_and(FadeOrder::filled) {
                continue;
            }
            let Some(signal) = n.signal() else {
                continue;
            };
            let capped = n.capped;
            let id = signal.instrument.clone();
            let base = shadow_order_id(account, &id, &row.anchor_date);
            let a = attempts(env, account, &base, &id).await?;
            let (Some(coid), Some((shadow, _))) = (a.next, &held) else {
                if let Some(o) = a.latest {
                    row.names[i].shadow = Some(o);
                }
                continue;
            };
            if holds(shadow, &id) {
                row.names[i].shadow = Some(position_open(&id, &coid, account));
                continue;
            }
            let key = write_signal(env, &row.anchor_date, &signal, capped, stamp).await;
            let order = env.shadow_gate().and_then(|gate| {
                FadeEntry {
                    gate,
                    limits: env.shadow_limits(),
                    notional_usd: cfg.shadow_notional_usd,
                    opportunity_key: key,
                    exit_at_ms,
                }
                .build(env, &signal, &coid)
            });
            match order {
                Ok(order) => todo.push((i, coid, order)),
                Err(e) => row.names[i].shadow = Some(not_placed(&id, &coid, e)),
            }
        }
        let Some((ctx, io)) = place else {
            return Ok(());
        };
        let placed: Vec<(usize, String, Result<Observation>)> = futures::stream::iter(todo)
            .map(|(i, coid, order)| async move {
                (i, coid, run_exec(&self.shared, ctx, io, order).await)
            })
            .buffered(SHADOW_CONCURRENCY)
            .collect()
            .await;
        for (i, coid, result) in placed {
            let id = row.names[i].instrument.clone();
            row.names[i].shadow = Some(fade_order(&id, &coid, result));
        }
        Ok(())
    }
}

/// One fade's attempts in its ledger (one name, one account).
struct Attempts {
    /// The latest stored attempt, as its outcome.
    latest: Option<FadeOrder>,
    /// The attempt to place next: the id when none is stored, the next
    /// `<id>:<n>` after a transient rejection; `None` once the latest is a
    /// fill, a partial fill or a final rejection.
    next: Option<String>,
}

/// The attempts of fade `id` (of `instrument`) stored in `account`'s ledger.
/// Attempts are placed in order and a denied one stores nothing, so the
/// stored ones are 1..k (`first_unstored`).
async fn attempts(env: &Env<'_>, account: &str, id: &str, instrument: &str) -> Result<Attempts> {
    let n = first_unstored(|n| {
        let coid = fade_attempt_id(id, n);
        async move { Ok(env.ledger.order(account, &coid).await?.is_some()) }
    })
    .await?;
    if n <= 1 {
        return Ok(Attempts {
            latest: None,
            next: Some(id.to_string()),
        });
    }
    let coid = fade_attempt_id(id, n - 1);
    let Some(p) = env.ledger.stored(account, &coid).await? else {
        bail!("order {coid} of account {account} was stored, then not found in the ledger");
    };
    let retry = p.order.as_ref().is_some_and(|o| {
        o.result.status == FillStatus::Rejected
            && o.result.reason.is_some_and(FillReason::is_transient)
    });
    Ok(Attempts {
        latest: Some(stored_order(instrument, &coid, &p)),
        next: retry.then(|| fade_attempt_id(id, n)),
    })
}

/// One ledger's fade entry: a market IOC of `notional_usd` on the fade's
/// side, closed at `exit_at_ms`.
struct FadeEntry {
    gate: ExecGate,
    limits: RiskLimits,
    notional_usd: f64,
    /// The name's `xm_weekend_signal/1` row.
    opportunity_key: String,
    exit_at_ms: i64,
}

impl FadeEntry {
    fn build(self, env: &Env<'_>, signal: &Signal, coid: &str) -> Result<ExecOrder> {
        Ok(ExecOrder {
            tool: names::XM_WEEKEND_FADE,
            gate: self.gate,
            limits: self.limits,
            instrument: InstrumentId::parse(&signal.instrument).map_err(|e| anyhow!("{e}"))?,
            side: signal.side,
            size: ExecSize::NotionalUsd(self.notional_usd),
            kind: OrderKind::Market,
            limit_px: None,
            reduce_only: false,
            max_slippage_bps: env.cfg.max_slippage_bps,
            strategy: Some(FADE_STRATEGY.to_string()),
            hedge_instrument: None,
            opportunity_key: Some(self.opportunity_key),
            client_order_id: Some(coid.to_string()),
            exit_at_ms: Some(self.exit_at_ms),
            tp_sl_on_book: None,
        })
    }
}

/// Write the `xm_weekend_signal/1` row of `signal`, stamped at the snapshot
/// (valid for the whole lateness window); its key. The row backs the
/// capped fade only (review #8): its side, `overreaction`, at most
/// `capped_notional_usd`.
async fn write_signal(
    env: &Env<'_>,
    anchor_date: &str,
    signal: &Signal,
    capped: bool,
    stamp_ms: i64,
) -> String {
    let sig = FadeSignal {
        anchor_date: anchor_date.to_string(),
        instrument: signal.instrument.clone(),
        anchor_px: signal.anchor_px,
        entry_px: signal.entry_px,
        s_bps: signal.s_bps,
        side: signal.side,
        capped,
        edge_after_costs_bps: env.cfg.expected_edge_bps,
        max_notional_usd: Some(env.cfg.capped_notional_usd),
    };
    let obs = Observation::of(
        names::XM_WEEKEND_FADE,
        &sig,
        stamp_ms,
        secs_ms(env.cfg.entry_lateness_max_secs),
        ObsSource::Live,
    );
    store_live(env.store, &obs).await;
    obs.key
}

/// The account holds an open position in `instrument`.
fn holds(account: &PaperAccount, instrument: &str) -> bool {
    account
        .positions
        .get(instrument)
        .is_some_and(|p| !p.is_flat())
}

fn position_open(instrument: &str, coid: &str, account: &str) -> FadeOrder {
    not_placed(
        instrument,
        coid,
        anyhow!("position_open: account {account} already holds {instrument}; not faded"),
    )
}

/// An order that was not placed, and why.
fn not_placed(instrument: &str, coid: &str, e: anyhow::Error) -> FadeOrder {
    let why = format!("{e:#}");
    FadeOrder {
        instrument: instrument.to_string(),
        client_order_id: coid.to_string(),
        status: if why.starts_with("no_position") {
            "flat".into()
        } else {
            "error".into()
        },
        rule: None,
        filled_qty: None,
        avg_px: None,
        notional_usd: None,
        fee_usd: None,
        error: Some(why),
    }
}

/// An order's outcome from its `paper_fill/1` row (or its refusal).
fn fade_order(instrument: &str, coid: &str, placed: Result<Observation>) -> FadeOrder {
    let obs = match placed {
        Ok(obs) => obs,
        Err(e) => return not_placed(instrument, coid, e),
    };
    let Ok(row) = obs.typed::<PaperFillRow>() else {
        return not_placed(instrument, coid, anyhow!("unexpected row {}", obs.key));
    };
    let error = obs.errors.first().map(|e| e.message.clone());
    outcome(instrument, coid, &row.gate.rule, row.fill.as_ref(), error)
}

/// A stored attempt's outcome, read from the ledger: what a replay's
/// `paper_fill/1` row would say.
fn stored_order(instrument: &str, coid: &str, p: &Placement) -> FadeOrder {
    let fill = p.order.as_ref().map(|o| &o.result);
    let error = fill
        .filter(|f| f.status == FillStatus::Rejected)
        .map(rejected_message);
    outcome(instrument, coid, &p.decision.verdict.rule, fill, error)
}

/// An order's outcome: the gate's rule, the fill (`None` = denied) and,
/// when it did not fill in full, why (`error`: the row's first error).
fn outcome(
    instrument: &str,
    coid: &str,
    rule: &str,
    fill: Option<&FillResult>,
    error: Option<String>,
) -> FadeOrder {
    let status = fill.map_or("denied", |f| f.status.as_str());
    let error = match fill {
        Some(f) if f.status == FillStatus::Partial => Some(format!(
            "partial fill, the rest canceled: {}",
            f.reason.map_or("-", |r| r.as_str())
        )),
        _ if status != "filled" => error,
        _ => None,
    };
    FadeOrder {
        instrument: instrument.to_string(),
        client_order_id: coid.to_string(),
        status: status.to_string(),
        rule: Some(rule.to_string()),
        filled_qty: fill.map(|f| f.filled_qty),
        avg_px: fill.and_then(|f| f.avg_px),
        notional_usd: fill.map(|f| f.filled_notional_usd),
        fee_usd: fill.map(|f| f.fee_usd),
        error,
    }
}

/// Book the funding `account` owes — its open positions, and closed ones
/// with hours owed — at fresh `mkt_ctx/1` rates.
async fn book_funding(env: &Env<'_>, account: &str, now: i64) -> Result<()> {
    let snap = env.ledger.snapshot(account, now).await?;
    let ids: BTreeSet<String> = snap.account.funding_ids();
    if ids.is_empty() {
        return Ok(());
    }
    let rows = MarketRows::read(env.store, &ids, None, None).await;
    accrue_due_funding(
        env.ledger,
        &snap.account,
        &rows,
        now,
        env.risk.max_data_age_ms.ctx,
    )
    .await?;
    Ok(())
}

/// A window row and the stored row's `observed_at_ms`; a row that does not
/// decode fails the call (the snapshot is never recomputed over it).
async fn read_row(
    store: Option<&dyn ObservationStore>,
    key: &str,
) -> Result<Option<(WeekendFade, i64)>> {
    let Some(store) = store else {
        return Ok(None);
    };
    let Some(obs) = store.get(key).await? else {
        return Ok(None);
    };
    let row: WeekendFade = obs.typed().map_err(|e| {
        anyhow!("store_unreadable: {e:#} — delete the row to start the window over")
    })?;
    Ok(Some((row, obs.observed_at_ms)))
}

async fn refresh(env: &Env<'_>, row: &mut WeekendFade, now: i64) -> Result<()> {
    let shadow = env.ledger.snapshot(&env.cfg.shadow_account, now).await?;
    let capped = env.ledger.snapshot(&env.risk.account, now).await?;
    row.refresh(&shadow.account, &capped.account, now);
    Ok(())
}

async fn save(store: Option<&dyn ObservationStore>, row: &WeekendFade) {
    let obs = Observation::of(
        names::XM_WEEKEND_FADE,
        row,
        row.ts_ms,
        WINDOW_TTL_MS,
        ObsSource::Live,
    );
    store_live(store, &obs).await;
}

/// The entry snapshot of window `w` at `now` (module table), not yet kept.
async fn snapshot(env: &Env<'_>, w: &WeekendWindow, now: i64) -> Result<WeekendFade> {
    let cfg = env.cfg;
    let keys: Vec<String> = cfg
        .universe
        .iter()
        .map(|id| Observation::key_for(MarketCtx::SCHEMA, id))
        .collect();
    let anchor_age = secs_ms(cfg.anchor_max_age_secs);
    let entry_age = secs_ms(cfg.entry_max_age_secs);
    let every = |what: &str, class: ErrorClass, why: String| -> Vec<Field<PricePoint>> {
        cfg.universe
            .iter()
            .map(|id| Field::err(ReadError::new(format!("{what}:{id}"), class, why.clone())))
            .collect()
    };
    let anchors = match env.history {
        None => every(
            "anchor",
            ErrorClass::NotApplicable,
            "no history store: [recorder] records the anchor price".into(),
        ),
        Some(h) => match h.asof(&keys, w.anchor_ms, anchor_age).await {
            Ok(rows) => rows
                .iter()
                .zip(&cfg.universe)
                .map(|(r, id)| {
                    let row = r.as_ref().map(|r| CtxRow {
                        status: r.status,
                        observed_at_ms: r.observed_at_ms,
                        features: &r.features,
                        error: r.errors.first(),
                    });
                    price_point(
                        "anchor",
                        id,
                        row,
                        w.anchor_ms,
                        anchor_age,
                        ErrorClass::NotApplicable,
                    )
                })
                .collect(),
            Err(e) => every(
                "anchor",
                ErrorClass::Transient,
                format!("history read failed: {e:#}"),
            ),
        },
    };
    // Per name, whether the store holds no entry row or only one older than
    // `entry_max_age_secs` (a restart): `stale`, waited for. A failed read
    // is no answer either way.
    let no_answer = vec![false; cfg.universe.len()];
    let (entries, stale): (Vec<Field<PricePoint>>, Vec<bool>) = match env.store {
        None => (
            every(
                "entry",
                ErrorClass::NotApplicable,
                "no observation store".into(),
            ),
            no_answer,
        ),
        Some(s) => match s.get_many(&keys).await {
            Ok(rows) => rows
                .iter()
                .zip(&cfg.universe)
                .map(|(r, id)| {
                    let row = r.as_ref().map(|o| CtxRow {
                        status: o.status,
                        observed_at_ms: o.observed_at_ms,
                        features: &o.features,
                        error: o.errors.first(),
                    });
                    // A row stamped in the future (a clock step) is stale too.
                    let stale = r.as_ref().is_none_or(|o| {
                        stamp_age_ms(now, o.observed_at_ms).is_none_or(|age| age > entry_age)
                    });
                    (
                        price_point("entry", id, row, now, entry_age, ErrorClass::Transient),
                        stale,
                    )
                })
                .unzip(),
            Err(e) => (
                every(
                    "entry",
                    ErrorClass::Transient,
                    format!("store read failed: {e:#}"),
                ),
                no_answer,
            ),
        },
    };
    let excluded: BTreeSet<&str> = cfg.exclude.iter().map(String::as_str).collect();
    let mut names: Vec<FadeName> = cfg
        .universe
        .iter()
        .zip(anchors)
        .zip(entries)
        .zip(stale)
        .map(|(((id, a), e), stale)| {
            let mut n = FadeName::at_entry(id, excluded.contains(id.as_str()), a, e);
            if stale && n.skip == Some(Skip::MissingEntry) {
                n.skip = Some(Skip::Stale);
            }
            n
        })
        .collect();
    let signals: Vec<Signal> = names.iter().filter_map(FadeName::signal).collect();
    let capped: BTreeSet<String> = select_capped(&signals, &cfg.rule()).into_iter().collect();
    let shadow = env.ledger.snapshot(&cfg.shadow_account, now).await?;
    let held = env.ledger.snapshot(&env.risk.account, now).await?;
    for n in &mut names {
        n.capped = capped.contains(&n.instrument);
        n.shadow_base_usd = position_net_usd(shadow.account.positions.get(&n.instrument));
        n.capped_base_usd = position_net_usd(held.account.positions.get(&n.instrument));
    }
    let mut row = WeekendFade::new(
        FadePhase::Entered,
        *w,
        &env.risk.account,
        &cfg.shadow_account,
        cfg.universe.len(),
        cfg.exclude.len(),
        cfg.entry_lateness_max_secs,
        now,
    );
    row.entered_at_ms = Some(now);
    row.names = names;
    Ok(row)
}

/// Keep `row` as the window's snapshot unless another call kept one first
/// (compare-and-swap on the stored row, `expected` = its `observed_at_ms`
/// as read); then `row` becomes theirs.
async fn claim(env: &Env<'_>, row: &mut WeekendFade, mut expected: Option<i64>) -> Result<()> {
    let Some(store) = env.store else {
        bail!(
            "store_unavailable: the weekend fade keeps its entry snapshot in the workspace \
             observation store, which did not open"
        );
    };
    let key = Observation::key_for(WeekendFade::SCHEMA, &row.anchor_date);
    for _ in 0..CLAIM_ATTEMPTS {
        let obs = Observation::of(
            names::XM_WEEKEND_FADE,
            &*row,
            row.ts_ms,
            WINDOW_TTL_MS,
            ObsSource::Live,
        );
        if store.put_if_unchanged(&obs, expected).await? {
            return Ok(());
        }
        match read_row(Some(store), &key).await? {
            Some((theirs, _)) if theirs.phase.has_snapshot() => {
                *row = theirs;
                return Ok(());
            }
            Some((_, at)) => expected = Some(at),
            None => expected = None,
        }
    }
    bail!("snapshot_conflict: the window row {key} kept changing; the next call tries again")
}

#[async_trait]
impl Tool for XmWeekendFadeTool {
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
        let obs = self.step(args, ctx, &io).await?;
        Ok(ToolOutput::observed(obs, SystemClock.now_ms()))
    }
}

#[cfg(test)]
#[path = "weekend_fade_tests.rs"]
mod tests;
