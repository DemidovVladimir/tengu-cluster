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
//! | Funding | both accounts (`[risk] account`, `shadow_account`) book the hourly funding their open positions owe, at a fresh `mkt_ctx/1` rate + oracle |
//! | Shadow exits | every open shadow position whose `exit_at_ms` passed, 4 at a time: a reduce-only market IOC of the whole position through the shadow gate (`risk::evaluate_shadow`), IOC bound `max_slippage_bps`, id `exit:<shadow account>:<full id>:deadline:<opened_ms>` (`…:<n>` once a rejected / partial attempt is stored). The capped positions close through `xm_exits` (reason `deadline`) — never here |
//! | Previous window | its `xm_weekend/1` row, still `entered` / `closing`: each fade not filled in the row read from the ledger (below); open quantities from both ledgers; once every filled name is flat, `closed` with the P&L |
//! | Current window, before the entry | `waiting` (`next_entry_s`) |
//! | … entry ≤ now < entry + `entry_lateness_max_secs`, no snapshot | the snapshot: anchor prices from the history as of the anchor (`mkt_ctx/1` mid, else mark, ≤ `anchor_max_age_secs` old), entry prices from the store (≤ `entry_max_age_secs` old), signals, the capped set, each name's ledger bases — kept in the store first (compare-and-swap: one snapshot per window, across processes). Kept only complete (`WeekendFade::complete`): while a name (not excluded, with an anchor) has no entry row fresher than `entry_max_age_secs` (`stale`) and now < entry + 120 s (≤ half the lateness, `weekend_fade::snapshot_deadline_ms`), or no name is eligible, nothing is kept (an `error` row) and the next call reads again; from that deadline the `stale` names are left out. A fresh row without a usable price (`missing_entry`) holds nothing back |
//! | … then, and on later calls within the lateness | per fade not filled in the row, its attempts in the ledger (`fade_attempt_id`: the id, then `<id>:2`, `<id>:3` …): the latest stored one is its outcome — a fill or a partial fill is never placed again, a final rejection stays; with none, or a transient rejection (`FillReason::is_transient`), the next attempt is placed — unless the account holds the name with no fill under those ids (`position_open`, not faded). Placing writes the name's `xm_weekend_signal/1` row (the opportunity row, stamped at the snapshot, TTL = the lateness), then the capped fades, largest \|s\| first (`run_exec`, the `[risk]` gate, `capped_notional_usd`, id `fade:<account>:<full id>:<anchor date>`, `exit_at_ms` = the exit, strategy `overreaction`), then the shadow fades of every eligible name, 4 at a time (the shadow gate, `shadow_notional_usd`, id `fade-shadow:<shadow account>:<full id>:<anchor date>`). A denial or a refusal stores nothing: the same attempt is judged again next call. Two callers place the same attempt id: the ledger keeps one (`UNIQUE (account, client_order_id)` in one `BEGIN IMMEDIATE`), the other replays it |
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
    ExecSize, MarketRows,
};
use super::exits::first_unstored;
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
use crate::domain::xm::ledger::{PaperAccount, Position};
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

    fn shadow_gate(&self) -> ExecGate {
        ExecGate::Shadow {
            initial_cash_usd: self.cfg.shadow_initial_cash_usd,
        }
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
            let attempt = first_unstored(|n| {
                let coid = id(n);
                async move { Ok(env.ledger.order(shadow, &coid).await?.is_some()) }
            })
            .await?;
            let coid = id(attempt);
            let order = ExecOrder {
                tool: names::XM_WEEKEND_FADE,
                gate: env.shadow_gate(),
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
            };
            Ok::<_, anyhow::Error>((coid, run_exec(&self.shared, ctx, io, order).await))
        }
        .await;
        match placed {
            Ok((coid, result)) => fade_order(&p.instrument, &coid, result),
            Err(e) => not_placed(&p.instrument, base.as_deref().unwrap_or("-"), e),
        }
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
            let order = FadeEntry {
                gate: env.shadow_gate(),
                limits: env.shadow_limits(),
                notional_usd: cfg.shadow_notional_usd,
                opportunity_key: key,
                exit_at_ms,
            };
            match order.build(env, &signal, &coid) {
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
        })
    }
}

/// Write the `xm_weekend_signal/1` row of `signal`, stamped at the snapshot
/// (valid for the whole lateness window); its key.
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

/// Book the funding `account`'s open positions owe (fresh `mkt_ctx/1` rows).
async fn book_funding(env: &Env<'_>, account: &str, now: i64) -> Result<()> {
    let snap = env.ledger.snapshot(account, now).await?;
    let ids: BTreeSet<String> = snap
        .account
        .open_positions()
        .map(|p| p.instrument.clone())
        .collect();
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
                    let stale = r.as_ref().is_none_or(|o| {
                        now.saturating_sub(o.observed_at_ms).max(0) as u64 > entry_age
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
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::Mutex;

    use serde_json::json;

    use super::*;
    use crate::adapters::outbound::tools::xm::exec_common::tests::Rig;
    use crate::config::xmarket::XmarketConfig;
    use crate::domain::book::{L2Book, L2Level};
    use crate::domain::market::{InstrumentKind, Listing, MarketInstrument, QuoteCcy};
    use crate::domain::observation::{ObsStatus, ReadError};
    use crate::domain::xm::weekend_fade::{PriceSource, ENTRY_WAIT_MS};
    use crate::ports::book::{BookRead, BookSource};
    use crate::ports::history::HistoryRow;

    const TSLA: &str = "hyperliquid:xyz:TSLA";
    const NVDA: &str = "hyperliquid:xyz:NVDA";
    const AAPL: &str = "hyperliquid:xyz:AAPL";
    const KIOXIA: &str = "hyperliquid:xyz:KIOXIA";
    const AMD: &str = "hyperliquid:xyz:AMD";
    const ACCOUNT: &str = "fade";
    const SHADOW: &str = "fade-shadow";
    /// Fri 2026-10-02 20:00 EDT / Sun 10-04 18:00 EDT / Mon 10-05 09:00 EDT.
    const ANCHOR: i64 = 1_790_985_600_000;
    const ENTRY: i64 = 1_791_151_200_000;
    const EXIT: i64 = 1_791_205_200_000;
    const DATE: &str = "2026-10-02";

    const CALENDARS: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/xmarket/calendars.toml"
    ));

    /// NYSE from the calendar fixture (= `config.example.toml`).
    fn nyse() -> ExchangeCalendar {
        #[derive(serde::Deserialize)]
        struct Fixture {
            xmarket: XmarketConfig,
        }
        let x = toml::from_str::<Fixture>(CALENDARS).unwrap().xmarket;
        x.calendars()["us_equity"].exchange().unwrap().clone()
    }

    /// `[risk]` with room for the capped fades: every universe name
    /// allowed, ctx rows up to 2 min old, leverage 2.
    fn risk(kill: &std::path::Path) -> RiskConfig {
        let toml = format!(
            r#"
account = "{ACCOUNT}"
mode = "paper"
venues = ["hyperliquid"]
min_lifecycle = "paper_tradable"
instruments_allow = ["{TSLA}", "{NVDA}", "{AAPL}", "{KIOXIA}", "{AMD}"]
instruments_deny = []
max_order_notional_usd = 25
max_position_notional_usd = 50
max_asset_exposure_usd = 50
max_venue_exposure_usd = 100
max_gross_exposure_usd = 100
max_net_exposure_usd = 100
max_leverage = 2
daily_loss_limit_usd = 10
total_loss_limit_usd = 25
min_edge_bps = 10
max_slippage_bps = 30
min_depth_usd = 250
require_hedge_for = []
max_skew_ms = 5000
max_orders_per_min = 8
max_open_orders = 4
kill_switch_file = "{}"
allow_reduce_degraded = true
exits = {{ take_profit_bps = 2000, stop_loss_bps = 1000, max_hold_secs = 172800 }}
max_data_age_ms = {{ book = 5000, ctx = 120000, reference = 60000, quote = 20000 }}
"#,
            kill.display()
        );
        toml::from_str::<RiskConfig>(&toml).unwrap().resolved()
    }

    fn fade_config() -> WeekendFadeConfig {
        toml::from_str(&format!(
            r#"
calendar = "us_equity"
universe = ["{TSLA}", "{NVDA}", "{AAPL}", "{KIOXIA}", "{AMD}"]
exclude = ["{KIOXIA}"]
capped_top_n = 2
min_abs_signal_bps = 50
capped_notional_usd = 20
shadow_account = "{SHADOW}"
shadow_initial_cash_usd = 10000
shadow_notional_usd = 100
expected_edge_bps = 23
anchor_max_age_secs = 600
entry_max_age_secs = 120
entry_lateness_max_secs = 600
max_slippage_bps = 50
"#
        ))
        .unwrap()
    }

    /// A book 0.05 either side of the mid, 1 000 deep, stamped now; a
    /// failing id answers a timeout, a one-sided id has bids only.
    struct FadeBooks {
        clock: Arc<crate::ports::clock::ManualClock>,
        mids: Mutex<BTreeMap<String, f64>>,
        failing: Mutex<BTreeSet<String>>,
        one_sided: Mutex<BTreeSet<String>>,
        reads: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl BookSource for FadeBooks {
        async fn fresh_book(&self, id: &InstrumentId) -> std::result::Result<BookRead, ReadError> {
            let key = id.to_string();
            self.reads.lock().unwrap().push(key.clone());
            if self.failing.lock().unwrap().contains(&key) {
                return Err(ReadError::new("hl_book", ErrorClass::Timeout, "slow"));
            }
            let mid = *self.mids.lock().unwrap().get(&key).ok_or_else(|| {
                ReadError::new(
                    "hl_book",
                    ErrorClass::NotApplicable,
                    format!("no book {key}"),
                )
            })?;
            let now = self.clock.now_ms();
            let level = |px: f64| L2Level {
                px,
                sz: 1_000.0,
                n: 1,
            };
            let asks = if self.one_sided.lock().unwrap().contains(&key) {
                vec![]
            } else {
                vec![level(mid + 0.05)]
            };
            Ok(BookRead {
                book: L2Book::new(vec![level(mid - 0.05)], asks, now).unwrap(),
                observed_at_ms: now,
            })
        }
    }

    /// `mkt_ctx/1` (mid = mark = oracle = `px`) + `mkt_instrument/1` of `id`.
    fn ctx_rows(id: &str, px: f64, at_ms: i64) -> Vec<Observation> {
        rows_with(id, px, at_ms, Some(false), 3)
    }

    /// [`ctx_rows`] with the OI-cap state as `hl_ctx` wrote it (`None`: the
    /// `perpsAtOpenInterestCap` read failed) and the lot size.
    fn rows_with(
        id: &str,
        px: f64,
        at_ms: i64,
        at_oi_cap: Option<bool>,
        sz_decimals: u32,
    ) -> Vec<Observation> {
        let iid = InstrumentId::parse(id).unwrap();
        let mut c = MarketCtx::new(iid.clone(), at_ms);
        c.mark = Field::ok(px);
        c.oracle = Field::ok(px);
        c.mid = Field::ok(px);
        c.funding_1h = Field::ok(0.0001);
        c.at_oi_cap = at_oi_cap;
        let mut i = MarketInstrument::new(iid, InstrumentKind::Perp, Listing::Listed);
        i.sz_decimals = Some(sz_decimals);
        i.quote_ccy = Some(QuoteCcy::Usdc);
        i.deployer_fee_scale = Some(1.0);
        i.growth_mode = Some(true);
        i.at_oi_cap = at_oi_cap;
        vec![
            Observation::of(names::HL_CTX, &c, at_ms, 5_000, ObsSource::Live),
            Observation::of(names::HL_CTX, &i, at_ms, 60_000, ObsSource::Live),
        ]
    }

    struct Fade {
        rig: Rig,
        books: FadeBooks,
        history: Arc<crate::adapters::outbound::history_sqlite::SqliteHistoryStore>,
        tool: XmWeekendFadeTool,
    }

    impl Fade {
        /// The paper rig with `[risk]` + `[xmarket.weekend_fade]` above, a
        /// recorder history, books for every name; clock at `now`.
        async fn new(now: i64) -> Fade {
            let mut rig = Rig::new(25).await;
            let history = Arc::new(
                crate::adapters::outbound::history_sqlite::SqliteHistoryStore::open(
                    &rig.dir.path().join("history"),
                    0,
                )
                .unwrap(),
            );
            rig.shared.risk = Some(risk(&rig.dir.path().join("KILL")));
            rig.shared.history = Some(history.clone() as Arc<dyn HistoryStore>);
            rig.shared.fade = Some(Arc::new(FadeSetup {
                config: fade_config(),
                calendar: Ok(nyse()),
            }));
            rig.clock.set(now);
            let books = FadeBooks {
                clock: rig.clock.clone(),
                mids: Mutex::new(BTreeMap::new()),
                failing: Mutex::new(BTreeSet::new()),
                one_sided: Mutex::new(BTreeSet::new()),
                reads: Mutex::new(Vec::new()),
            };
            let tool = XmWeekendFadeTool {
                def: defs::def(names::XM_WEEKEND_FADE),
                shared: rig.shared.clone(),
            };
            Fade {
                rig,
                books,
                history,
                tool,
            }
        }

        /// Anchor rows (60 s before the anchor) in the history, fresh entry
        /// rows in the store at `at_ms`, books at the entry prices.
        async fn market(&self, prices: &[(&str, Option<f64>, f64)], at_ms: i64) {
            for (id, anchor, entry) in prices {
                if let Some(a) = anchor {
                    let rows: Vec<HistoryRow> = ctx_rows(id, *a, ANCHOR - 60_000)
                        .iter()
                        .map(|o| HistoryRow::of(o, false))
                        .collect();
                    self.history.append(&rows).await.unwrap();
                }
                self.prices(id, *entry, at_ms).await;
            }
        }

        /// Fresh rows and the book of `id` at `px`.
        async fn prices(&self, id: &str, px: f64, at_ms: i64) {
            self.put(ctx_rows(id, px, at_ms)).await;
            self.books.mids.lock().unwrap().insert(id.to_string(), px);
        }

        async fn put(&self, rows: Vec<Observation>) {
            for row in rows {
                self.rig.store.put(&row).await.unwrap();
            }
        }

        /// The stored window row becomes `row` without its order outcomes:
        /// the snapshot as claimed — a crash after the fills but before the
        /// final save, or a concurrent call that saved last.
        async fn lose_outcomes(&self, row: &WeekendFade) {
            let mut claimed = row.clone();
            for n in &mut claimed.names {
                n.capped_order = None;
                n.shadow = None;
                n.capped_qty = None;
                n.shadow_qty = None;
            }
            let obs = Observation::of(
                names::XM_WEEKEND_FADE,
                &claimed,
                claimed.ts_ms,
                WINDOW_TTL_MS,
                ObsSource::Live,
            );
            self.rig.store.put(&obs).await.unwrap();
        }

        /// `xm_exits`' deadline close of every open capped position.
        async fn close_capped(&self, call: &str) {
            let now = self.rig.clock.now_ms();
            let snap = self.rig.ledger.snapshot(ACCOUNT, now).await.unwrap();
            for p in snap.account.open_positions() {
                let opened = p.opened_ms.unwrap();
                let order = ExecOrder {
                    tool: names::XM_EXITS,
                    gate: ExecGate::Risk,
                    limits: self.rig.shared.risk.as_ref().unwrap().limits(),
                    instrument: InstrumentId::parse(&p.instrument).unwrap(),
                    side: Side::Sell,
                    size: ExecSize::Close,
                    kind: OrderKind::Market,
                    limit_px: None,
                    reduce_only: true,
                    max_slippage_bps: 50.0,
                    strategy: None,
                    hedge_instrument: None,
                    opportunity_key: None,
                    client_order_id: Some(exit_client_order_id(
                        ACCOUNT,
                        &p.instrument,
                        ExitReason::Deadline,
                        opened,
                        1,
                    )),
                    exit_at_ms: None,
                };
                let io = ExecIo {
                    clock: self.rig.clock.as_ref(),
                    books: &self.books,
                    rand01: 0.5,
                };
                run_exec(&self.rig.shared, &self.rig.ctx(Some(call)), &io, order)
                    .await
                    .unwrap();
            }
        }

        async fn step(&self, call: &str) -> Result<Observation> {
            let io = ExecIo {
                clock: self.rig.clock.as_ref(),
                books: &self.books,
                rand01: 0.5,
            };
            self.tool
                .step(&json!({}), &self.rig.ctx(Some(call)), &io)
                .await
        }

        /// `(account, client_order_id, side, status, exit_at_ms)` of every
        /// stored order, in order.
        fn orders(&self) -> Vec<(String, String, String, String, Option<i64>)> {
            let c =
                rusqlite::Connection::open(self.rig.dir.path().join("state/ledger.db")).unwrap();
            let mut s = c
                .prepare(
                    "SELECT account, client_order_id, side, status, exit_at_ms FROM orders \
                     ORDER BY id",
                )
                .unwrap();
            s.query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect()
        }
    }

    fn fade_row(o: &Observation) -> WeekendFade {
        o.typed().unwrap()
    }

    fn name<'a>(row: &'a WeekendFade, id: &str) -> &'a FadeName {
        row.names.iter().find(|n| n.instrument == id).unwrap()
    }

    /// TSLA +198 bps and NVDA −100.5 bps (the capped two), AAPL +20 bps
    /// (shadow only), KIOXIA excluded, AMD without an anchor.
    const PRICES: [(&str, Option<f64>, f64); 5] = [
        (TSLA, Some(100.0), 102.0),
        (NVDA, Some(200.0), 198.0),
        (AAPL, Some(50.0), 50.1),
        (KIOXIA, Some(300.0), 100.0),
        (AMD, None, 150.0),
    ];

    /// The window's whole life: waiting → the entry (capped + shadow, one
    /// snapshot, a failed book placed again next call, never twice) → the
    /// exit (shadow closed here, capped left to `xm_exits`) → closed with
    /// the P&L → the next window waits.
    #[tokio::test]
    async fn a_window_from_waiting_to_closed() {
        let f = Fade::new(ENTRY - 3 * 86_400_000).await;
        let o = f.step("feed:xm_weekend_fade:1:0").await.unwrap();
        assert_eq!(o.key, format!("xm_weekend/1:{DATE}"));
        assert_eq!(o.status, ObsStatus::Ok, "{}", o.render_text(0));
        assert_eq!(
            o.headline,
            "xm_weekend 2026-10-02 waiting next_entry_s=259200 universe=5 excluded=1"
        );
        assert_eq!(o.ttl_ms, WINDOW_TTL_MS);
        assert!(f.orders().is_empty());
        assert!(f.rig.store.get(&o.key).await.unwrap().is_some(), "stored");

        // The entry, 30 s late; AAPL's book read times out once.
        f.rig.clock.set(ENTRY + 30_000);
        f.market(&PRICES, ENTRY + 20_000).await;
        f.books.failing.lock().unwrap().insert(AAPL.into());
        let o = f.step("feed:xm_weekend_fade:2:0").await.unwrap();
        let row = fade_row(&o);
        assert_eq!(row.phase, FadePhase::Entered);
        assert_eq!(o.status, ObsStatus::Partial, "AMD, AAPL: {:?}", o.errors);
        assert_eq!(
            o.headline,
            format!(
                "xm_weekend 2026-10-02 entered eligible=3/5 shadow=2 capped=2/2 \
                 missing_anchor=1 missing_entry=0 capped: {TSLA} {NVDA}"
            )
        );
        for (k, v) in [
            ("n_eligible", 3),
            ("n_capped", 2),
            ("n_capped_filled", 2),
            ("n_shadow_filled", 2),
            ("n_shadow_failed", 1),
            ("n_missing_anchor", 1),
            ("n_excluded", 1),
        ] {
            assert_eq!(o.features[k], v, "{k}");
        }
        let tsla = name(&row, TSLA);
        assert_eq!((tsla.side, tsla.capped), (Some(Side::Sell), true));
        assert_eq!(tsla.s_bps, Some((102.0f64 / 100.0).ln() * 10_000.0));
        assert_eq!(name(&row, NVDA).side, Some(Side::Buy));
        assert!(!name(&row, AAPL).capped);
        assert_eq!(name(&row, KIOXIA).skip, Some(Skip::Excluded));
        assert_eq!(name(&row, AMD).skip, Some(Skip::MissingAnchor));
        let aapl = name(&row, AAPL).shadow.clone().unwrap();
        assert_eq!(
            (aapl.status.as_str(), aapl.rule.as_deref()),
            ("denied", Some("missing:book"))
        );
        // The ledgers: capped (the [risk] gate) + shadow (its own gate),
        // deterministic ids, every position due at the exit.
        let capped_tsla = format!("fade:{ACCOUNT}:{TSLA}:{DATE}");
        let capped_nvda = format!("fade:{ACCOUNT}:{NVDA}:{DATE}");
        let shadow_tsla = format!("fade-shadow:{SHADOW}:{TSLA}:{DATE}");
        let shadow_nvda = format!("fade-shadow:{SHADOW}:{NVDA}:{DATE}");
        let shadow_aapl = format!("fade-shadow:{SHADOW}:{AAPL}:{DATE}");
        let s = |a: &str, id: &str, side: &str| {
            (
                a.to_string(),
                id.to_string(),
                side.to_string(),
                "filled".to_string(),
                Some(EXIT),
            )
        };
        // Capped first, in |s| order; the shadow fades run 4 at a time.
        let orders = f.orders();
        assert_eq!(
            orders[..2],
            [
                s(ACCOUNT, &capped_tsla, "sell"),
                s(ACCOUNT, &capped_nvda, "buy")
            ]
        );
        let mut shadow_orders = orders[2..].to_vec();
        shadow_orders.sort();
        assert_eq!(
            shadow_orders,
            [
                s(SHADOW, &shadow_nvda, "buy"),
                s(SHADOW, &shadow_tsla, "sell")
            ]
        );
        let lines = f.rig.risk_lines();
        assert_eq!(lines.len(), 5, "4 allowed + the AAPL denial");
        let shadow_line = lines
            .iter()
            .find(|l| l["client_order_id"] == shadow_tsla)
            .unwrap();
        assert_eq!(shadow_line["tool"], "xm_weekend_fade");
        assert_eq!(shadow_line["call_id"], "feed:xm_weekend_fade:2:0");
        let skipped = shadow_line["checks"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["detail"] == "shadow: measurement only")
            .count();
        assert!(skipped >= 15, "{shadow_line}");
        // The capped fade named its signal row (edge 23 ≥ min_edge 10).
        let capped_line = lines
            .iter()
            .find(|l| l["client_order_id"] == capped_tsla)
            .unwrap();
        assert_eq!(capped_line["intent"]["strategy"], "overreaction");
        let signal_key = format!("xm_weekend_signal/1:{DATE}:{TSLA}");
        assert_eq!(capped_line["intent"]["opportunity_key"], signal_key);
        let signal = f.rig.store.get(&signal_key).await.unwrap().unwrap();
        assert_eq!(signal.features["edge_after_costs_bps"], 23.0);
        assert_eq!(
            signal.observed_at_ms,
            ENTRY + 30_000,
            "stamped at the snapshot"
        );

        // Next call: the snapshot holds (a new TSLA price changes nothing),
        // AAPL is placed again (its book is back), nothing else twice.
        f.books.failing.lock().unwrap().clear();
        f.rig.clock.set(ENTRY + 90_000);
        f.prices(TSLA, 90.0, ENTRY + 85_000).await;
        let o = f.step("feed:xm_weekend_fade:3:0").await.unwrap();
        let again = fade_row(&o);
        assert_eq!(name(&again, TSLA).s_bps, tsla.s_bps, "decided once");
        assert_eq!(again.entered_at_ms, Some(ENTRY + 30_000));
        assert_eq!(name(&again, AAPL).shadow.as_ref().unwrap().status, "filled");
        assert_eq!(o.status, ObsStatus::Partial, "AMD still has no anchor");
        let orders = f.orders();
        assert_eq!(orders.len(), 5);
        assert_eq!(orders[4].1, shadow_aapl);
        assert_eq!(f.rig.risk_lines().len(), 6, "one new verdict: AAPL");
        // And again: nothing to place.
        f.step("feed:xm_weekend_fade:4:0").await.unwrap();
        assert_eq!((f.orders().len(), f.rig.risk_lines().len()), (5, 6));

        // The exit: the shadow positions close here; the capped ones are
        // left to xm_exits.
        f.rig.clock.set(EXIT + 10_000);
        for (id, px) in [(TSLA, 101.0), (NVDA, 199.0), (AAPL, 50.0)] {
            f.prices(id, px, EXIT + 5_000).await;
        }
        let o = f.step("feed:xm_weekend_fade:5:0").await.unwrap();
        let row = fade_row(&o);
        assert_eq!(o.key, format!("xm_weekend/1:{DATE}"), "the closing window");
        assert_eq!(row.phase, FadePhase::Closing);
        assert_eq!(row.shadow_exits.len(), 3);
        assert!(
            row.shadow_exits.iter().all(|e| e.status == "filled"),
            "{:?}",
            row.shadow_exits
        );
        assert!(row.shadow_exits[0]
            .client_order_id
            .starts_with(&format!("exit:{SHADOW}:")));
        assert_eq!(
            (
                o.features["n_shadow_open"].clone(),
                o.features["n_capped_open"].clone()
            ),
            (json!(0), json!(2))
        );
        assert!(name(&row, TSLA).shadow_pnl_usd.is_some());
        let capped_open = f
            .orders()
            .iter()
            .filter(|o| o.0 == ACCOUNT && o.1.starts_with("exit:"))
            .count();
        assert_eq!(capped_open, 0, "never closes the capped ledger");

        // xm_exits closes the capped two (deadline): closed, with the P&L.
        f.close_capped("feed:xm_exits:1:0").await;
        let o = f.step("feed:xm_weekend_fade:6:0").await.unwrap();
        let row = fade_row(&o);
        assert_eq!(row.phase, FadePhase::Closed, "{}", o.render_text(0));
        let shadow = f.rig.ledger.snapshot(SHADOW, EXIT + 20_000).await.unwrap();
        let mut want = 0.0;
        for id in [TSLA, NVDA, AAPL] {
            let n = name(&row, id);
            let net = position_net_usd(shadow.account.positions.get(id)) - n.shadow_base_usd;
            assert_eq!(n.shadow_pnl_usd, Some(net), "{id}");
            want += net;
        }
        let got = o.features["shadow_pnl_usd"].as_f64().unwrap();
        assert!((got - want).abs() < 1e-9, "{got} vs {want}");
        assert!(
            name(&row, TSLA).shadow_pnl_usd.unwrap() > 0.0,
            "a short from 102 to 101"
        );
        assert!(o.features.contains_key("capped_pnl_usd"));
        assert!(o
            .headline
            .starts_with("xm_weekend 2026-10-02 closed shadow_pnl_usd="));

        // Then the next window waits; the closed one stays closed.
        let o = f.step("feed:xm_weekend_fade:7:0").await.unwrap();
        assert_eq!(o.key, "xm_weekend/1:2026-10-09");
        assert_eq!(fade_row(&o).phase, FadePhase::Waiting);
        let closed = f
            .rig
            .store
            .get(&format!("xm_weekend/1:{DATE}"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fade_row(&closed).phase, FadePhase::Closed);
    }

    /// No call within the lateness: no late entry. A missing entry price on
    /// every name keeps no snapshot (an error row), so the next call tries
    /// again.
    #[tokio::test]
    async fn no_late_entry_and_no_snapshot_without_prices() {
        let f = Fade::new(ENTRY + 601_000).await;
        f.market(&PRICES, ENTRY + 600_000).await;
        let o = f.step("feed:xm_weekend_fade:1:0").await.unwrap();
        let row = fade_row(&o);
        assert_eq!((row.phase, row.late_s), (FadePhase::MissedEntry, Some(601)));
        assert_eq!(o.status, ObsStatus::Partial);
        assert!(
            o.errors[0].message.starts_with("missed_entry"),
            "{:?}",
            o.errors
        );
        assert!(f.orders().is_empty());

        // Inside the lateness, but every entry row is 10 min old.
        let f = Fade::new(ENTRY + 60_000).await;
        f.market(&PRICES, ENTRY - 600_000).await;
        let o = f.step("feed:xm_weekend_fade:1:0").await.unwrap();
        assert_eq!(o.status, ObsStatus::Error);
        assert_eq!(fade_row(&o).n_eligible(), 0);
        assert!(
            o.errors.iter().any(|e| e.class == ErrorClass::Transient),
            "a stale entry is retried soon: {:?}",
            o.errors
        );
        assert!(
            f.rig.store.get(&o.key).await.unwrap().is_none(),
            "no snapshot kept"
        );
        // Fresh rows: the next call enters.
        f.rig.clock.set(ENTRY + 120_000);
        f.market(&PRICES[..3], ENTRY + 110_000).await;
        let o = f.step("feed:xm_weekend_fade:2:0").await.unwrap();
        assert_eq!(fade_row(&o).phase, FadePhase::Entered);
        assert_eq!(f.orders().len(), 5, "2 capped + 3 shadow");
    }

    /// Two callers race for the entry: the snapshot kept first wins
    /// (compare-and-swap) and the loser places that one, so the capped set
    /// is never decided twice.
    #[tokio::test]
    async fn a_lost_snapshot_race_takes_the_winner() {
        let f = Fade::new(ENTRY + 30_000).await;
        f.market(&PRICES, ENTRY + 20_000).await;
        let w = fade_window(&nyse(), ENTRY).unwrap();
        let px = |px: f64, at_ms: i64| {
            Field::ok(PricePoint {
                px,
                at_ms,
                source: PriceSource::Mid,
            })
        };
        // Theirs: TSLA fell (a buy), the only name.
        let mut theirs = WeekendFade::new(
            FadePhase::Entered,
            w,
            ACCOUNT,
            SHADOW,
            5,
            1,
            600,
            ENTRY + 1_000,
        );
        let mut n = FadeName::at_entry(TSLA, false, px(100.0, ANCHOR), px(99.0, ENTRY));
        n.capped = true;
        theirs.names = vec![n];
        theirs.entered_at_ms = Some(ENTRY + 1_000);
        let obs = Observation::of(
            names::XM_WEEKEND_FADE,
            &theirs,
            ENTRY + 1_000,
            WINDOW_TTL_MS,
            ObsSource::Live,
        );
        // Ours read "no row", then theirs landed: the claim loses to it.
        f.rig.store.put(&obs).await.unwrap();
        let setup = f.rig.shared.fade.clone().unwrap();
        let env = Env {
            risk: f.rig.shared.risk.as_ref().unwrap(),
            ledger: f.rig.ledger.as_ref(),
            store: f.rig.shared.store.as_deref(),
            history: f.rig.shared.history.as_deref(),
            cfg: &setup.config,
            cal: setup.calendar.as_ref().unwrap(),
        };
        for (account, cash) in [(ACCOUNT, 100.0), (SHADOW, 10_000.0)] {
            f.rig
                .ledger
                .open_account(account, cash, ENTRY)
                .await
                .unwrap();
        }
        let mut ours = snapshot(&env, &w, ENTRY + 30_000).await.unwrap();
        assert_eq!(ours.n_eligible(), 3);
        claim(&env, &mut ours, None).await.unwrap();
        assert_eq!(ours, theirs, "the loser takes the winner's snapshot");
        // A whole step places theirs.
        let o = f.step("feed:xm_weekend_fade:1:0").await.unwrap();
        let row = fade_row(&o);
        assert_eq!(row.entered_at_ms, Some(ENTRY + 1_000), "theirs");
        assert_eq!(row.names.len(), 1);
        let orders = f.orders();
        assert_eq!(
            orders.iter().map(|o| o.1.clone()).collect::<Vec<_>>(),
            [
                format!("fade:{ACCOUNT}:{TSLA}:{DATE}"),
                format!("fade-shadow:{SHADOW}:{TSLA}:{DATE}")
            ]
        );
        assert_eq!(orders[0].2, "buy", "their signal: TSLA fell");
    }

    /// Review #1: the `perpsAtOpenInterestCap` read fails at the entry, so
    /// every row has `at_oi_cap` unknown. Both gates deny every fade
    /// `missing:at_oi_cap` (verdicts only, nothing stored); the next call,
    /// on known rows, places every eligible fade under its first id.
    #[tokio::test]
    async fn an_unknown_oi_cap_at_entry_is_denied_then_placed_next_call() {
        let f = Fade::new(ENTRY + 30_000).await;
        f.market(&PRICES, ENTRY + 20_000).await;
        for (id, _, px) in PRICES {
            f.put(rows_with(id, px, ENTRY + 25_000, None, 3)).await;
        }
        let o = f.step("feed:xm_weekend_fade:1:0").await.unwrap();
        let row = fade_row(&o);
        assert_eq!(row.phase, FadePhase::Entered, "the snapshot is kept");
        assert!(f.orders().is_empty(), "nothing stored: {:?}", f.orders());
        let denied = |o: &Option<FadeOrder>| {
            let o = o.clone().unwrap();
            (o.status, o.rule)
        };
        let want = ("denied".to_string(), Some("missing:at_oi_cap".to_string()));
        for id in [TSLA, NVDA] {
            assert_eq!(denied(&name(&row, id).capped_order), want, "{id}");
        }
        for id in [TSLA, NVDA, AAPL] {
            assert_eq!(denied(&name(&row, id).shadow), want, "{id}");
        }
        assert_eq!(f.rig.risk_lines().len(), 5, "2 capped + 3 shadow verdicts");

        // The next hl_ctx run reads the cap list again.
        f.rig.clock.set(ENTRY + 90_000);
        for (id, _, px) in PRICES {
            f.prices(id, px, ENTRY + 85_000).await;
        }
        let o = f.step("feed:xm_weekend_fade:2:0").await.unwrap();
        let orders = f.orders();
        assert_eq!(orders.len(), 5, "{orders:?}");
        assert!(orders.iter().all(|o| o.3 == "filled"), "{orders:?}");
        assert_eq!(
            (
                o.features["n_capped_filled"].clone(),
                o.features["n_shadow_filled"].clone()
            ),
            (json!(2), json!(3))
        );
        let ids: BTreeSet<&str> = orders.iter().map(|o| o.1.as_str()).collect();
        for id in [
            format!("fade:{ACCOUNT}:{TSLA}:{DATE}"),
            format!("fade:{ACCOUNT}:{NVDA}:{DATE}"),
            format!("fade-shadow:{SHADOW}:{AAPL}:{DATE}"),
        ] {
            assert!(ids.contains(id.as_str()), "{id} in {ids:?}");
        }
    }

    /// Review #5: a stored rejection the next attempt may not meet (AAPL's
    /// one-sided book: `missing:mid`) is placed again as `<id>:2` within the
    /// lateness; a final one (NVDA in whole lots: $20 and $100 at 198 round
    /// to 0, `MinTradeNtl`) and a fill never are; after the lateness nothing
    /// is.
    #[tokio::test]
    async fn transient_entry_rejections_are_retried_within_the_lateness() {
        let f = Fade::new(ENTRY + 30_000).await;
        f.market(&PRICES, ENTRY + 20_000).await;
        f.put(rows_with(NVDA, 198.0, ENTRY + 20_000, Some(false), 0))
            .await;
        f.books.one_sided.lock().unwrap().insert(AAPL.into());
        let o = f.step("feed:xm_weekend_fade:1:0").await.unwrap();
        let row = fade_row(&o);
        let aapl = name(&row, AAPL).shadow.clone().unwrap();
        assert_eq!(aapl.status, "rejected");
        assert!(
            aapl.error
                .as_deref()
                .unwrap()
                .starts_with("rejected missing:mid"),
            "{aapl:?}"
        );
        let nvda = name(&row, NVDA);
        for o in [&nvda.capped_order, &nvda.shadow] {
            let o = o.clone().unwrap();
            assert_eq!(o.status, "rejected");
            assert!(
                o.error
                    .as_deref()
                    .unwrap()
                    .starts_with("rejected MinTradeNtl"),
                "{o:?}"
            );
        }
        assert_eq!(f.orders().len(), 5, "every fade stored once");

        // Next call, AAPL's book whole again: its attempt 2 fills; NVDA's
        // final rejections and TSLA's fills stay.
        f.books.one_sided.lock().unwrap().clear();
        f.rig.clock.set(ENTRY + 90_000);
        let o = f.step("feed:xm_weekend_fade:2:0").await.unwrap();
        let row = fade_row(&o);
        let shadow_aapl = format!("fade-shadow:{SHADOW}:{AAPL}:{DATE}");
        let aapl = name(&row, AAPL).shadow.clone().unwrap();
        assert_eq!(
            (aapl.status.as_str(), aapl.client_order_id.as_str()),
            ("filled", format!("{shadow_aapl}:2").as_str())
        );
        let orders = f.orders();
        assert_eq!(orders.len(), 6, "{orders:?}");
        assert_eq!(
            (orders[5].1.as_str(), orders[5].3.as_str()),
            (format!("{shadow_aapl}:2").as_str(), "filled")
        );
        let nvda = name(&row, NVDA).capped_order.clone().unwrap();
        assert_eq!(
            (nvda.status.as_str(), nvda.client_order_id.as_str()),
            ("rejected", format!("fade:{ACCOUNT}:{NVDA}:{DATE}").as_str())
        );
        assert_eq!(o.features["n_shadow_filled"], 2);
        // And again: nothing to place.
        f.rig.clock.set(ENTRY + 150_000);
        f.step("feed:xm_weekend_fade:3:0").await.unwrap();
        assert_eq!(f.orders().len(), 6);

        // The same rejection with the lateness over: it stays the outcome.
        let g = Fade::new(ENTRY + 30_000).await;
        g.market(&PRICES, ENTRY + 20_000).await;
        g.books.one_sided.lock().unwrap().insert(AAPL.into());
        g.step("feed:xm_weekend_fade:1:0").await.unwrap();
        assert_eq!(g.orders().len(), 5);
        g.books.one_sided.lock().unwrap().clear();
        g.rig.clock.set(ENTRY + 600_000);
        for (id, _, px) in PRICES {
            g.prices(id, px, ENTRY + 595_000).await;
        }
        let o = g.step("feed:xm_weekend_fade:2:0").await.unwrap();
        assert_eq!(g.orders().len(), 5, "no attempt after the lateness");
        let aapl = name(&fade_row(&o), AAPL).shadow.clone().unwrap();
        assert_eq!(
            (aapl.status.as_str(), aapl.client_order_id.as_str()),
            ("rejected", shadow_aapl.as_str())
        );
    }

    /// Review #3: the window row is saved without the order outcomes (a
    /// crash after the fills, or a concurrent `tool call` that saved the
    /// claimed snapshot last). The next call reads each fade's outcome from
    /// the ledger — inside the lateness or after the exit — with no double
    /// entry, and the fills, closes and P&L of both ledgers count. A
    /// position the fade did not open stays `position_open`.
    #[tokio::test]
    async fn a_lost_row_save_recovers_the_outcomes_from_the_ledger() {
        let f = Fade::new(ENTRY + 30_000).await;
        f.market(&PRICES, ENTRY + 20_000).await;
        let o = f.step("feed:xm_weekend_fade:1:0").await.unwrap();
        assert_eq!(o.features["n_capped_filled"], 2);
        let placed = fade_row(&o);
        let n_orders = f.orders().len();
        f.lose_outcomes(&placed).await;
        f.rig.clock.set(ENTRY + 90_000);
        let o = f.step("feed:xm_weekend_fade:2:0").await.unwrap();
        let row = fade_row(&o);
        assert_eq!(f.orders().len(), n_orders, "no double entry");
        for n in &placed.names {
            let got = name(&row, &n.instrument);
            assert_eq!(
                (&got.capped_order, &got.shadow),
                (&n.capped_order, &n.shadow),
                "{} read back from the ledger",
                n.instrument
            );
        }
        assert_eq!(
            (
                o.features["n_capped_filled"].clone(),
                o.features["n_shadow_filled"].clone()
            ),
            (json!(2), json!(3))
        );

        // The exit: the shadow closes here, the capped two by xm_exits; the
        // row closes with both ledgers' P&L.
        f.rig.clock.set(EXIT + 10_000);
        for (id, px) in [(TSLA, 101.0), (NVDA, 199.0), (AAPL, 50.0)] {
            f.prices(id, px, EXIT + 5_000).await;
        }
        f.close_capped("feed:xm_exits:1:0").await;
        let o = f.step("feed:xm_weekend_fade:3:0").await.unwrap();
        let row = fade_row(&o);
        assert_eq!(row.phase, FadePhase::Closed, "{}", o.render_text(0));
        assert_eq!(row.shadow_exits.len(), 3, "the shadow closed here");
        let now = f.rig.clock.now_ms();
        let shadow = f.rig.ledger.snapshot(SHADOW, now).await.unwrap().account;
        let capped = f.rig.ledger.snapshot(ACCOUNT, now).await.unwrap().account;
        for id in [TSLA, NVDA, AAPL] {
            let n = name(&row, id);
            let net = position_net_usd(shadow.positions.get(id)) - n.shadow_base_usd;
            assert_eq!(n.shadow_pnl_usd, Some(net), "{id}");
        }
        for id in [TSLA, NVDA] {
            let n = name(&row, id);
            let net = position_net_usd(capped.positions.get(id)) - n.capped_base_usd;
            assert_eq!(n.capped_pnl_usd, Some(net), "{id}");
        }
        assert!(
            o.features.contains_key("capped_pnl_usd"),
            "{:?}",
            o.features
        );
        assert!(o.features.contains_key("shadow_pnl_usd"));

        // Lost again, found only after the exit (the previous window's row).
        let g = Fade::new(ENTRY + 30_000).await;
        g.market(&PRICES, ENTRY + 20_000).await;
        let o = g.step("feed:xm_weekend_fade:1:0").await.unwrap();
        g.lose_outcomes(&fade_row(&o)).await;
        g.rig.clock.set(EXIT + 10_000);
        for (id, px) in [(TSLA, 101.0), (NVDA, 199.0), (AAPL, 50.0)] {
            g.prices(id, px, EXIT + 5_000).await;
        }
        let o = g.step("feed:xm_weekend_fade:2:0").await.unwrap();
        let row = fade_row(&o);
        assert_eq!(row.phase, FadePhase::Closing, "the capped two still open");
        assert_eq!(o.features["n_capped_open"], 2);
        g.close_capped("feed:xm_exits:1:0").await;
        let o = g.step("feed:xm_weekend_fade:3:0").await.unwrap();
        assert_eq!(fade_row(&o).phase, FadePhase::Closed);
        assert!(
            o.features.contains_key("capped_pnl_usd"),
            "{:?}",
            o.features
        );
        assert_eq!(g.orders().len(), 5 + 5, "5 entries, 5 closes");

        // A shadow position opened by hand before the entry: not faded.
        let h = Fade::new(ENTRY - 60_000).await;
        h.market(&PRICES, ENTRY - 70_000).await;
        let risk = h.rig.shared.risk.clone().unwrap();
        let by_hand = ExecOrder {
            tool: names::PAPER_ORDER,
            gate: ExecGate::Shadow {
                initial_cash_usd: 10_000.0,
            },
            limits: RiskLimits {
                account: SHADOW.into(),
                ..risk.limits()
            },
            instrument: InstrumentId::parse(NVDA).unwrap(),
            side: Side::Buy,
            size: ExecSize::NotionalUsd(50.0),
            kind: OrderKind::Market,
            limit_px: None,
            reduce_only: false,
            max_slippage_bps: 50.0,
            strategy: None,
            hedge_instrument: None,
            opportunity_key: None,
            client_order_id: Some("manual:nvda:1".into()),
            exit_at_ms: None,
        };
        let io = ExecIo {
            clock: h.rig.clock.as_ref(),
            books: &h.books,
            rand01: 0.5,
        };
        let o = run_exec(&h.rig.shared, &h.rig.ctx(Some("manual:1")), &io, by_hand)
            .await
            .unwrap();
        assert_eq!(o.status, ObsStatus::Ok, "{}", o.render_text(0));
        h.rig.clock.set(ENTRY + 30_000);
        h.market(&PRICES, ENTRY + 20_000).await;
        let o = h.step("feed:xm_weekend_fade:1:0").await.unwrap();
        let row = fade_row(&o);
        let nvda = name(&row, NVDA).shadow.clone().unwrap();
        assert_eq!(nvda.status, "error");
        assert!(
            nvda.error.as_deref().unwrap().starts_with("position_open"),
            "{nvda:?}"
        );
        assert_eq!(
            name(&row, NVDA).capped_order.as_ref().unwrap().status,
            "filled",
            "the capped ledger holds no NVDA"
        );
    }

    /// Review #4: a restart near the entry finds some entry rows stale. The
    /// snapshot is kept only complete: until entry + 120 s an incomplete
    /// read keeps nothing (an `error` row); from then on the stale names are
    /// left out `stale` for the window. A fresh row without a usable price
    /// holds nothing back.
    #[tokio::test]
    async fn the_snapshot_waits_for_every_fresh_entry_row() {
        let f = Fade::new(ENTRY + 30_000).await;
        f.market(&PRICES, ENTRY - 300_000).await;
        f.market(&PRICES[..2], ENTRY + 20_000).await;
        let o = f.step("feed:xm_weekend_fade:1:0").await.unwrap();
        assert_eq!(o.status, ObsStatus::Error, "{}", o.render_text(0));
        let row = fade_row(&o);
        assert_eq!(name(&row, AAPL).skip, Some(Skip::Stale));
        assert_eq!(
            name(&row, AMD).skip,
            Some(Skip::MissingAnchor),
            "no anchor: never waited for"
        );
        assert!(
            f.rig.store.get(&o.key).await.unwrap().is_none(),
            "nothing kept"
        );
        assert!(f.orders().is_empty());
        // AAPL's row arrives: the whole snapshot.
        f.rig.clock.set(ENTRY + 60_000);
        f.prices(AAPL, 50.1, ENTRY + 55_000).await;
        let o = f.step("feed:xm_weekend_fade:2:0").await.unwrap();
        let row = fade_row(&o);
        assert_eq!((row.phase, row.n_eligible()), (FadePhase::Entered, 3));
        assert_eq!(f.orders().len(), 5);

        // AAPL never refreshes: kept without it from entry + 120 s.
        let g = Fade::new(ENTRY + 30_000).await;
        g.market(&PRICES, ENTRY - 300_000).await;
        g.market(&PRICES[..2], ENTRY + 20_000).await;
        g.rig.clock.set(ENTRY + ENTRY_WAIT_MS - 1);
        g.market(&PRICES[..2], ENTRY + 110_000).await;
        let o = g.step("feed:xm_weekend_fade:1:0").await.unwrap();
        assert_eq!(o.status, ObsStatus::Error, "still waiting");
        g.rig.clock.set(ENTRY + ENTRY_WAIT_MS);
        let o = g.step("feed:xm_weekend_fade:2:0").await.unwrap();
        let row = fade_row(&o);
        assert_eq!(row.phase, FadePhase::Entered);
        assert_eq!(name(&row, AAPL).skip, Some(Skip::Stale));
        assert_eq!(o.features["n_stale"], 1);
        assert!(o.headline.contains(" stale=1 "), "{}", o.headline);
        assert!(g.rig.store.get(&o.key).await.unwrap().is_some(), "kept");
        assert_eq!(g.orders().len(), 4, "2 capped + 2 shadow");
        // A fresh AAPL row later changes nothing.
        g.rig.clock.set(ENTRY + 180_000);
        g.prices(AAPL, 50.1, ENTRY + 175_000).await;
        g.step("feed:xm_weekend_fade:3:0").await.unwrap();
        assert_eq!(g.orders().len(), 4);

        // AAPL's row is fresh but has no price: `missing_entry`, kept at once.
        let h = Fade::new(ENTRY + 30_000).await;
        h.market(&PRICES, ENTRY + 20_000).await;
        let mut no_px = MarketCtx::new(InstrumentId::parse(AAPL).unwrap(), ENTRY + 25_000);
        no_px.at_oi_cap = Some(false);
        let row = Observation::of(
            names::HL_CTX,
            &no_px,
            ENTRY + 25_000,
            5_000,
            ObsSource::Live,
        );
        h.put(vec![row]).await;
        let o = h.step("feed:xm_weekend_fade:1:0").await.unwrap();
        let row = fade_row(&o);
        assert_eq!(row.phase, FadePhase::Entered, "{}", o.render_text(0));
        assert_eq!(name(&row, AAPL).skip, Some(Skip::MissingEntry));
        assert!(h.rig.store.get(&o.key).await.unwrap().is_some(), "kept");
        assert_eq!(h.orders().len(), 4, "2 capped + 2 shadow");
    }

    #[tokio::test]
    async fn refusals_come_before_anything() {
        let f = Fade::new(ENTRY - 1_000).await;
        let io = ExecIo {
            clock: f.rig.clock.as_ref(),
            books: &f.books,
            rand01: 0.5,
        };
        let e = f
            .tool
            .step(&json!({"at": 1}), &f.rig.ctx(Some("x:1")), &io)
            .await
            .unwrap_err();
        assert!(
            e.to_string().contains("unknown argument(s) [\"at\"]"),
            "{e}"
        );
        let mut no_cfg = XmWeekendFadeTool {
            def: defs::def(names::XM_WEEKEND_FADE),
            shared: f.rig.shared.clone(),
        };
        no_cfg.shared.fade = None;
        let e = no_cfg
            .step(&json!({}), &f.rig.ctx(Some("x:2")), &io)
            .await
            .unwrap_err();
        assert!(
            e.to_string().starts_with(WEEKEND_FADE_CONFIG_MISSING),
            "{e}"
        );
        no_cfg.shared.fade = Some(Arc::new(FadeSetup {
            config: fade_config(),
            calendar: Err("calendar `us_equity` is not an exchange row".into()),
        }));
        let e = no_cfg
            .step(&json!({}), &f.rig.ctx(Some("x:3")), &io)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("not an exchange row"), "{e}");
        let mut routable = Fade::new(ENTRY - 1_000).await;
        routable.rig.agent.description = Some("routable".into());
        let e = routable.step("x:4").await.unwrap_err();
        assert!(e.to_string().starts_with("exec_agent_not_private"), "{e}");
        no_cfg.shared.risk = None;
        let e = no_cfg
            .step(&json!({}), &f.rig.ctx(Some("x:5")), &io)
            .await
            .unwrap_err();
        assert!(e.to_string().starts_with("risk_config_missing"), "{e}");
    }

    #[tokio::test]
    async fn execute_gates_the_scope_first() {
        let f = Fade::new(ENTRY - 1_000).await;
        let mut denied = f.rig.ctx(Some("s:1"));
        let no_fs = crate::domain::scope::ToolScope::default();
        denied.scope = &no_fs;
        let e = f.tool.execute(&json!({}), &denied).await.unwrap_err();
        assert!(e.to_string().contains("fs"), "{e}");
        assert_eq!(f.tool.definition().name, "xm_weekend_fade");
    }
}
