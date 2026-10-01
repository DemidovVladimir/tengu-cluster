//! Backtest engine (`docs/xlab-2026-10-01.md` § 5–6), pure, in two steps so
//! the application can put Jev between them: [`candidates`] — every decision
//! a spec takes in `[from, to)`, before caps (per-kind logic: `kinds.rs`) —
//! then [`simulate`] — one arm's trades, refusals and `Summary`. Fills,
//! costs and exits: `fills.rs`.
//!
//! | Kind | Decision instants (in `[from, to)`) | Signal → side | Exit plan | Period |
//! |---|---|---|---|---|
//! | `weekend_window` | each `weekend_fade::fade_window` of the spec's calendar, entry + `entry_offset_mins` | rule W: `signal_of` (excluded · no anchor · no entry · flat), s = ln(entry / anchor) bps, `fade` = −sign(s), `follow` = +sign(s); `top_n` set ⇒ `select_capped(top_n, min_abs_signal_bps)`, else every \|s\| ≥ min | the window's exit + offset | anchor date |
//! | `daily_window` | each local day of `days` at `entry` (`tz`) | as above, `anchor` → `entry` (an anchor time ≥ the entry's = the previous (trading) day) | `exit`; ≤ the entry time = the next (trading) day | entry date (local) |
//! | `move_trigger` | every bar close per instrument, not within `cooldown_bars` of its last trigger | \|ln(c_t / c_{t−lookback})\| ≥ threshold (+ the lookback's volume ≥ ratio × the baseline's mean per bar × lookback; the series must reach the baseline) | hold / TP / SL | UTC day |
//! | `funding_carry` | every funding row with \|rate\| APR ≥ min, at the next bar close; one position per instrument | short when longs pay, long when shorts pay; signal = rate × 8760 × 10⁴ | APR < exit / hold | UTC day |
//! | `pair_spread` | every close both legs have; one position at a time | \|z\| ≥ entry_z: short the rich leg, long the cheap one (`sell` = short a / long b), `notional_usd` / 2 each; signal = (s − mean) bps | \|z\| ≤ exit_z / max hold | UTC day |
//! | `event_window` | each event's `t` + `entry_delay_mins`, at the next bar close | the move from the close at or before `t` (\|move\| ≥ `min_abs_move_bps`) | `exit_after_mins`, or the next `exit_at` (`tz`), at the next bar close | UTC day |
//!
//! | Rule | Value |
//! |---|---|
//! | Prices | `BarSeries::close_at` of the instant only (the bar ending there); missing ⇒ a skip, never interpolated |
//! | As-of | `data_asof_ms` = the latest observation a candidate read (price bars, funding row, features) ≤ `decided_at_ms`; `simulate` drops one that is not (`future_data`) |
//! | Order | candidates by (decided_at, instrument key), `seq` = the index; a pair's key = `<a>/<b>` |
//! | Re-entry | `funding_carry` / `pair_spread` re-enter only after the rule's own exit — known by the next decision, so still time-honest |
//! | Fill | entry and exit at the instant's close; gross bps = side × ln(exit / entry) × 10⁴; net bps = gross − (entry + exit cost) + funding; USD = net bps × notional / 10⁴; a pair: each leg `notional` / 2, the trade's bps the legs' mean |
//! | `research` arm | every candidate at the spec's notional, no caps |
//! | `capped` arm ([`RiskCaps`]) | in time order; positions open until their exit, exits at an instant before entries; notional clamped to `max_order_notional_usd`; refused, rule = the `[risk]` field: `total_loss_limit_usd` once realized equity ≤ initial − limit (for good) · `daily_loss_limit_usd` while the UTC day's realized P&L ≤ −limit · `max_gross_exposure_usd` / `max_net_exposure_usd` when the open notional with this one would exceed |
//! | Skips ([`SkipReason`]) | excluded · missing_anchor / missing_entry / missing_price · flat · below_min_signal · not_top_n · no_costs (no `costs`, no `[backtest.costs]` prefix) · missing_exit (no exit bar: the trade is dropped) · future_data |

// Consumers land with the xlab application wave (docs/xlab-2026-10-01.md); drop this then.
#![cfg_attr(not(test), allow(dead_code))]

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::domain::backtest::costs::{cost_for, CostSpec};
use crate::domain::backtest::fills::{
    funding_over, gross_bps, side_cost, spread_points, walk_bars_exit, walk_funding_exit,
    walk_spread_exit, ExitReason,
};
use crate::domain::backtest::kinds;
use crate::domain::backtest::spec::{StrategyKind, StrategySpec};
use crate::domain::backtest::stats::{StatsParams, Summary};
use crate::domain::book::Side;
use crate::domain::calendar::Calendar;
use crate::domain::marketdata::{fmt_time, BarSeries, CtxSeries, FundingSeries};
use crate::domain::xm::weekend_fade::Skip as FadeSkip;

const DAY_MS: i64 = 86_400_000;
/// Exposure comparisons tolerate float residue.
const EPS_USD: f64 = 1e-9;

/// The series a run reads, by full instrument id; bars at the spec's interval.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MarketData {
    pub bars: BTreeMap<String, BarSeries>,
    pub funding: BTreeMap<String, FundingSeries>,
    pub ctx: BTreeMap<String, CtxSeries>,
}

/// What a run needs besides the spec and the series (the application fills
/// it from the sandbox).
#[derive(Debug, Clone, PartialEq)]
pub struct RunParams {
    /// Decisions in `[from_ms, to_ms)`.
    pub from_ms: i64,
    pub to_ms: i64,
    /// The spec's universe as full ids (`@<name>` resolved); empty ⇒ the
    /// spec's own ids. `pair_spread` / `event_window` ignore it.
    pub universe: Vec<String>,
    /// `[backtest] notional_usd` — when the spec sets none.
    pub notional_usd: f64,
    /// `[backtest.costs]` — prefix → cost; the spec's `costs` overrides.
    pub costs: BTreeMap<String, CostSpec>,
    /// `[xmarket.calendars]`, built.
    pub calendars: BTreeMap<String, Calendar>,
    /// `[backtest] bootstrap` / `seed`.
    pub bootstrap: u32,
    pub seed: u64,
    /// The research arm's start equity for drawdown % (`[paper]
    /// initial_cash_usd`); the capped arm uses its caps'.
    pub start_equity_usd: Option<f64>,
}

/// One leg of a decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Leg {
    pub instrument: String,
    pub side: Side,
    /// The close at the decision instant — the entry fill.
    pub entry_px: f64,
}

/// When a decision's position closes (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "rule", rename_all = "snake_case")]
pub enum ExitPlan {
    /// A clock instant fixed at the decision (window and event kinds).
    At { exit_ms: i64 },
    /// `move_trigger`: TP / SL at a bar close, else `max_exit_ms`.
    Bars {
        max_exit_ms: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        take_profit_bps: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop_loss_bps: Option<f64>,
    },
    /// `funding_carry`: a settlement under `exit_apr_pct`, else `max_exit_ms`.
    Funding {
        max_exit_ms: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_apr_pct: Option<f64>,
    },
    /// `pair_spread`: \|z\| ≤ `exit_z`, else `max_exit_ms`.
    Spread { max_exit_ms: i64, exit_z: f64 },
}

/// One decision of the rule, before caps (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    /// Index in [`CandidateSet::candidates`].
    pub seq: usize,
    /// The id, or `<a>/<b>` for a pair.
    pub instrument: String,
    /// The leg's side; a pair's: the spread's (`sell` = short a / long b).
    pub side: Side,
    pub legs: Vec<Leg>,
    pub signal_bps: f64,
    pub decided_at_ms: i64,
    /// The latest observation read; ≤ `decided_at_ms`.
    pub data_asof_ms: i64,
    pub period: String,
    /// The reference price of the move (window / event anchor, the
    /// lookback close of a trigger).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor_px: Option<f64>,
    pub exit: ExitPlan,
    /// `features.rs` of the (first) leg at the decision.
    pub features: BTreeMap<String, f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Why a name or a candidate gets no trade (module table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    Excluded,
    MissingAnchor,
    MissingEntry,
    MissingPrice,
    Flat,
    BelowMinSignal,
    NotTopN,
    NoCosts,
    MissingExit,
    FutureData,
}

impl SkipReason {
    pub fn as_str(self) -> &'static str {
        match self {
            SkipReason::Excluded => "excluded",
            SkipReason::MissingAnchor => "missing_anchor",
            SkipReason::MissingEntry => "missing_entry",
            SkipReason::MissingPrice => "missing_price",
            SkipReason::Flat => "flat",
            SkipReason::BelowMinSignal => "below_min_signal",
            SkipReason::NotTopN => "not_top_n",
            SkipReason::NoCosts => "no_costs",
            SkipReason::MissingExit => "missing_exit",
            SkipReason::FutureData => "future_data",
        }
    }

    /// A data gap (the report flags them), not a rule's choice.
    pub fn is_missing_data(self) -> bool {
        matches!(
            self,
            SkipReason::MissingAnchor
                | SkipReason::MissingEntry
                | SkipReason::MissingPrice
                | SkipReason::MissingExit
        )
    }

    /// Rule W's skip as a backtest skip.
    pub(crate) fn of_fade(s: FadeSkip) -> SkipReason {
        match s {
            FadeSkip::Excluded => SkipReason::Excluded,
            FadeSkip::MissingAnchor => SkipReason::MissingAnchor,
            FadeSkip::MissingEntry | FadeSkip::Stale => SkipReason::MissingEntry,
            FadeSkip::MissingExit => SkipReason::MissingExit,
            FadeSkip::Flat => SkipReason::Flat,
        }
    }
}

/// One skip; per-instrument skips (excluded, no costs) of the bar kinds
/// carry `from_ms` and an empty period.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Skip {
    pub instrument: String,
    pub decided_at_ms: i64,
    pub period: String,
    pub reason: SkipReason,
}

/// [`candidates`]' result.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CandidateSet {
    pub candidates: Vec<Candidate>,
    pub skipped: Vec<Skip>,
    /// Data notes for the report (an instrument without bars, …).
    pub notes: Vec<String>,
}

impl CandidateSet {
    /// Skips per reason (`as_str`).
    pub fn skip_counts(&self) -> BTreeMap<String, usize> {
        count_skips(&self.skipped)
    }
}

pub(crate) fn count_skips(skips: &[Skip]) -> BTreeMap<String, usize> {
    let mut m = BTreeMap::new();
    for s in skips {
        *m.entry(s.reason.as_str().to_string()).or_insert(0) += 1;
    }
    m
}

/// The `[risk]` caps the capped arm applies, on `[paper] initial_cash_usd`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RiskCaps {
    pub initial_cash_usd: f64,
    pub max_order_notional_usd: f64,
    pub max_gross_exposure_usd: f64,
    pub max_net_exposure_usd: f64,
    pub daily_loss_limit_usd: f64,
    pub total_loss_limit_usd: f64,
}

/// Which ledger a simulation keeps (module table).
#[derive(Debug, Clone, PartialEq)]
pub enum Arm {
    Research,
    Capped(RiskCaps),
}

impl Arm {
    pub fn name(&self) -> &'static str {
        match self {
            Arm::Research => "research",
            Arm::Capped(_) => "capped",
        }
    }
}

/// One leg of a trade; bps of the leg's notional.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TradeLeg {
    pub instrument: String,
    pub side: Side,
    pub notional_usd: f64,
    pub entry_px: f64,
    pub exit_px: f64,
    pub gross_bps: f64,
    /// Entry + exit.
    pub fee_bps: f64,
    pub spread_bps: f64,
    pub slippage_bps: f64,
    pub funding_bps: f64,
    pub funding_complete: bool,
    pub net_bps: f64,
}

/// One simulated trade — the audit row (PRD § 33, § 39); bps of the
/// trade's notional.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Trade {
    /// The candidate's `seq`.
    pub seq: usize,
    pub instrument: String,
    pub side: Side,
    pub legs: Vec<TradeLeg>,
    pub signal_bps: f64,
    pub decided_at_ms: i64,
    pub data_asof_ms: i64,
    pub entry_ms: i64,
    pub exit_ms: i64,
    pub notional_usd: f64,
    pub gross_bps: f64,
    pub fee_bps: f64,
    pub spread_bps: f64,
    pub slippage_bps: f64,
    pub funding_bps: f64,
    pub funding_complete: bool,
    pub net_bps: f64,
    pub net_usd: f64,
    pub period: String,
    pub exit_reason: ExitReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// A candidate the capped arm refused.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Refusal {
    pub seq: usize,
    pub instrument: String,
    pub side: Side,
    pub decided_at_ms: i64,
    pub period: String,
    /// The `[risk]` field.
    pub rule: String,
    pub notional_usd: f64,
    pub detail: String,
}

/// One arm's simulation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArmResult {
    pub arm: String,
    /// In decision order.
    pub trades: Vec<Trade>,
    pub refusals: Vec<Refusal>,
    /// Candidates dropped while filling (`missing_exit`, `no_costs`,
    /// `future_data`).
    pub skipped: Vec<Skip>,
    pub summary: Summary,
    /// What `summary` was computed with (the report's split halves reuse it).
    pub stats: StatsParams,
}

/// UTC date of `t_ms`, `YYYY-MM-DD`.
pub(crate) fn utc_day(t_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(t_ms)
        .map_or_else(|| t_ms.to_string(), |d| d.format("%Y-%m-%d").to_string())
}

/// Every decision `spec` takes over `[params.from_ms, params.to_ms)`, in
/// time order, with the skips and data notes (module tables; the per-kind
/// logic is `kinds.rs`). `Err` for a run that cannot start: an empty range,
/// an unresolved universe, a calendar that is not an exchange row, bars at
/// another interval.
pub fn candidates(
    spec: &StrategySpec,
    md: &MarketData,
    params: &RunParams,
) -> Result<CandidateSet, String> {
    if params.from_ms >= params.to_ms {
        return Err(format!(
            "from {} is not before to {}",
            fmt_time(params.from_ms),
            fmt_time(params.to_ms)
        ));
    }
    if let Some((id, s)) = md.bars.iter().find(|(_, s)| s.interval != spec.interval) {
        return Err(format!(
            "bars of {id} are {}; the spec reads {}",
            s.interval, spec.interval
        ));
    }
    kinds::decide(spec, md, params)
}

/// An open position of the capped arm.
#[derive(Debug, Clone, Copy)]
struct Open {
    exit_ms: i64,
    seq: usize,
    gross: f64,
    net: f64,
    net_usd: f64,
}

/// The capped arm's ledger (module table).
struct Book<'a> {
    caps: &'a RiskCaps,
    open: Vec<Open>,
    equity: f64,
    day_pnl: BTreeMap<i64, f64>,
    total_halt: bool,
}

impl<'a> Book<'a> {
    fn new(caps: &'a RiskCaps) -> Self {
        Self {
            caps,
            open: Vec::new(),
            equity: caps.initial_cash_usd,
            day_pnl: BTreeMap::new(),
            total_halt: false,
        }
    }

    /// Close every position with exit ≤ `t`, in exit order.
    fn realize(&mut self, t: i64) {
        let mut done: Vec<Open> = Vec::new();
        self.open.retain(|p| {
            let closed = p.exit_ms <= t;
            if closed {
                done.push(*p);
            }
            !closed
        });
        done.sort_by_key(|p| (p.exit_ms, p.seq));
        for p in done {
            self.equity += p.net_usd;
            *self
                .day_pnl
                .entry(p.exit_ms.div_euclid(DAY_MS))
                .or_insert(0.0) += p.net_usd;
            if self.equity <= self.caps.initial_cash_usd - self.caps.total_loss_limit_usd {
                self.total_halt = true;
            }
        }
    }

    /// The first rule refusing an entry at `t` with these signed leg
    /// notionals.
    fn refusal(&self, t: i64, legs: &[f64]) -> Option<(&'static str, String)> {
        let c = self.caps;
        if self.total_halt {
            return Some((
                "total_loss_limit_usd",
                format!(
                    "realized equity {:.2} ≤ {:.2} − {:.2}: no entries after a total-loss halt",
                    self.equity, c.initial_cash_usd, c.total_loss_limit_usd
                ),
            ));
        }
        let day = self
            .day_pnl
            .get(&t.div_euclid(DAY_MS))
            .copied()
            .unwrap_or(0.0);
        if day <= -c.daily_loss_limit_usd {
            return Some((
                "daily_loss_limit_usd",
                format!(
                    "realized {day:.2} today ≤ −{:.2}: no entries until 00:00 UTC",
                    c.daily_loss_limit_usd
                ),
            ));
        }
        let gross = self.open.iter().map(|p| p.gross).sum::<f64>()
            + legs.iter().map(|x| x.abs()).sum::<f64>();
        if gross > c.max_gross_exposure_usd + EPS_USD {
            return Some((
                "max_gross_exposure_usd",
                format!("gross {gross:.2} > {:.2}", c.max_gross_exposure_usd),
            ));
        }
        let net = self.open.iter().map(|p| p.net).sum::<f64>() + legs.iter().sum::<f64>();
        if net.abs() > c.max_net_exposure_usd + EPS_USD {
            return Some((
                "max_net_exposure_usd",
                format!("net {net:.2} beyond ±{:.2}", c.max_net_exposure_usd),
            ));
        }
        None
    }

    fn open(&mut self, t: &Trade) {
        self.open.push(Open {
            exit_ms: t.exit_ms,
            seq: t.seq,
            gross: t.legs.iter().map(|l| l.notional_usd).sum(),
            net: t.legs.iter().map(|l| l.side.sign() * l.notional_usd).sum(),
            net_usd: t.net_usd,
        });
    }
}

/// Fill `c` at `notional` (module table); `Err` = why it is dropped.
fn build_trade(
    spec: &StrategySpec,
    md: &MarketData,
    params: &RunParams,
    c: &Candidate,
    notional: f64,
) -> Result<Trade, SkipReason> {
    if c.data_asof_ms > c.decided_at_ms {
        return Err(SkipReason::FutureData);
    }
    let first = c.legs.first().ok_or(SkipReason::MissingPrice)?;
    let entry = c.decided_at_ms;
    let (exit_ms, exit_reason) = match &c.exit {
        ExitPlan::At { exit_ms } => (*exit_ms, ExitReason::Window),
        ExitPlan::Bars {
            max_exit_ms,
            take_profit_bps,
            stop_loss_bps,
        } => {
            let s = md
                .bars
                .get(&first.instrument)
                .ok_or(SkipReason::MissingExit)?;
            walk_bars_exit(
                s,
                entry,
                first.entry_px,
                first.side,
                *max_exit_ms,
                *take_profit_bps,
                *stop_loss_bps,
            )
        }
        ExitPlan::Funding {
            max_exit_ms,
            exit_apr_pct,
        } => match md.funding.get(&first.instrument) {
            Some(f) => walk_funding_exit(f, entry, *max_exit_ms, *exit_apr_pct, spec.interval.ms()),
            None => (*max_exit_ms, ExitReason::Hold),
        },
        ExitPlan::Spread {
            max_exit_ms,
            exit_z,
        } => {
            let (StrategyKind::PairSpread(p), [a, b]) = (&spec.kind, c.legs.as_slice()) else {
                return Err(SkipReason::MissingExit);
            };
            let (Some(sa), Some(sb)) = (md.bars.get(&a.instrument), md.bars.get(&b.instrument))
            else {
                return Err(SkipReason::MissingExit);
            };
            let pts = spread_points(sa, sb, p.lookback_bars as usize);
            walk_spread_exit(&pts, entry, *max_exit_ms, *exit_z)
        }
    };
    if exit_ms <= entry {
        return Err(SkipReason::MissingExit);
    }
    let leg_notional = notional / c.legs.len() as f64;
    let mut legs = Vec::with_capacity(c.legs.len());
    for leg in &c.legs {
        let cost = spec
            .costs
            .as_ref()
            .or_else(|| cost_for(&params.costs, &leg.instrument))
            .ok_or(SkipReason::NoCosts)?;
        let series = md
            .bars
            .get(&leg.instrument)
            .ok_or(SkipReason::MissingExit)?;
        let exit_px = series.close_at(exit_ms).ok_or(SkipReason::MissingExit)?;
        let gross = gross_bps(leg.side, leg.entry_px, exit_px).ok_or(SkipReason::MissingExit)?;
        let ctx = md.ctx.get(&leg.instrument);
        let (on_entry, on_exit) = (
            side_cost(cost, series, ctx, entry),
            side_cost(cost, series, ctx, exit_ms),
        );
        let (funding_bps, funding_complete) = funding_over(
            md.funding.get(&leg.instrument),
            leg.side,
            entry,
            exit_ms,
            cost.funding,
        );
        legs.push(TradeLeg {
            instrument: leg.instrument.clone(),
            side: leg.side,
            notional_usd: leg_notional,
            entry_px: leg.entry_px,
            exit_px,
            gross_bps: gross,
            fee_bps: on_entry.fee_bps + on_exit.fee_bps,
            spread_bps: on_entry.spread_bps + on_exit.spread_bps,
            slippage_bps: on_entry.slippage_bps + on_exit.slippage_bps,
            funding_bps,
            funding_complete,
            net_bps: gross - (on_entry.total_bps() + on_exit.total_bps()) + funding_bps,
        });
    }
    // Legs share the notional equally: the trade's bps are their mean.
    let mean = |f: fn(&TradeLeg) -> f64| -> f64 {
        if legs.len() == 1 {
            f(&legs[0])
        } else {
            legs.iter().map(f).sum::<f64>() / legs.len() as f64
        }
    };
    let net_bps = mean(|l| l.net_bps);
    Ok(Trade {
        seq: c.seq,
        instrument: c.instrument.clone(),
        side: c.side,
        signal_bps: c.signal_bps,
        decided_at_ms: c.decided_at_ms,
        data_asof_ms: c.data_asof_ms,
        entry_ms: entry,
        exit_ms,
        notional_usd: notional,
        gross_bps: mean(|l| l.gross_bps),
        fee_bps: mean(|l| l.fee_bps),
        spread_bps: mean(|l| l.spread_bps),
        slippage_bps: mean(|l| l.slippage_bps),
        funding_bps: mean(|l| l.funding_bps),
        funding_complete: legs.iter().all(|l| l.funding_complete),
        net_bps,
        net_usd: net_bps * notional / 10_000.0,
        period: c.period.clone(),
        exit_reason,
        label: c.label.clone(),
        legs,
    })
}

/// One arm over `candidates` (module table): trades in decision order, the
/// capped arm's refusals, candidates dropped while filling, the summary.
pub fn simulate(
    spec: &StrategySpec,
    md: &MarketData,
    params: &RunParams,
    candidates: &[Candidate],
    arm: Arm,
) -> ArmResult {
    let notional = spec.notional(params.notional_usd);
    let mut order: Vec<&Candidate> = candidates.iter().collect();
    order.sort_by(|a, b| {
        a.decided_at_ms
            .cmp(&b.decided_at_ms)
            .then_with(|| a.instrument.cmp(&b.instrument))
            .then(a.seq.cmp(&b.seq))
    });
    let (mut trades, mut refusals, mut skipped) = (Vec::new(), Vec::new(), Vec::new());
    let mut book = match &arm {
        Arm::Capped(caps) => Some(Book::new(caps)),
        Arm::Research => None,
    };
    for c in order {
        let size = match &arm {
            Arm::Capped(caps) => notional.min(caps.max_order_notional_usd),
            Arm::Research => notional,
        };
        if let Some(book) = book.as_mut() {
            book.realize(c.decided_at_ms);
            let legs: Vec<f64> = c
                .legs
                .iter()
                .map(|l| l.side.sign() * size / c.legs.len() as f64)
                .collect();
            if let Some((rule, detail)) = book.refusal(c.decided_at_ms, &legs) {
                refusals.push(Refusal {
                    seq: c.seq,
                    instrument: c.instrument.clone(),
                    side: c.side,
                    decided_at_ms: c.decided_at_ms,
                    period: c.period.clone(),
                    rule: rule.to_string(),
                    notional_usd: size,
                    detail,
                });
                continue;
            }
        }
        match build_trade(spec, md, params, c, size) {
            Ok(t) => {
                if let Some(book) = book.as_mut() {
                    book.open(&t);
                }
                trades.push(t);
            }
            Err(reason) => skipped.push(Skip {
                instrument: c.instrument.clone(),
                decided_at_ms: c.decided_at_ms,
                period: c.period.clone(),
                reason,
            }),
        }
    }
    let stats = StatsParams {
        bootstrap: params.bootstrap,
        seed: params.seed,
        periods_per_year: spec.period_kind().per_year(),
        start_equity_usd: match &arm {
            Arm::Capped(caps) => Some(caps.initial_cash_usd),
            Arm::Research => params.start_equity_usd,
        },
    };
    let summary = Summary::compute(&trades, &stats);
    ArmResult {
        arm: arm.name().to_string(),
        trades,
        refusals,
        skipped,
        summary,
        stats,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::domain::backtest::testkit::{market, run_params, sparse, spec, utc, H};
    use crate::domain::marketdata::Interval;

    const A: &str = "hyperliquid:xyz:AAA";
    const B: &str = "hyperliquid:xyz:BBB";
    const C: &str = "hyperliquid:xyz:CCC";
    const D: &str = "hyperliquid:xyz:DDD";

    fn caps() -> RiskCaps {
        RiskCaps {
            initial_cash_usd: 100.0,
            max_order_notional_usd: 50.0,
            max_gross_exposure_usd: 100.0,
            max_net_exposure_usd: 60.0,
            daily_loss_limit_usd: 10.0,
            total_loss_limit_usd: 25.0,
        }
    }

    /// A hand-made decision on `id` at `t` exiting at `exit`.
    fn decision(seq: usize, id: &str, side: Side, t: i64, px: f64, exit: i64) -> Candidate {
        Candidate {
            seq,
            instrument: id.into(),
            side,
            legs: vec![Leg {
                instrument: id.into(),
                side,
                entry_px: px,
            }],
            signal_bps: 0.0,
            decided_at_ms: t,
            data_asof_ms: t,
            period: utc_day(t),
            anchor_px: None,
            exit: ExitPlan::At { exit_ms: exit },
            features: BTreeMap::new(),
            label: None,
        }
    }

    fn flat_spec() -> StrategySpec {
        spec(
            json!({"kind": "move_trigger", "universe": [A], "interval": "1h", "lookback_bars": 1,
            "threshold_bps": 100, "direction": "fade", "hold_bars": 2}),
        )
    }

    #[test]
    fn capped_arm_clamps_and_refuses_on_exposure() {
        let t = utc("2026-09-28 10:00");
        let flat = |id| {
            sparse(
                id,
                Interval::H1,
                &[
                    (t - H, 100.0),
                    (t + H, 100.0),
                    (t + 3 * H, 100.0),
                    (t + 5 * H, 100.0),
                ],
            )
        };
        let md = market(vec![flat(A), flat(B), flat(C), flat(D)]);
        let p = run_params(t - 10 * H, t + 10 * H);
        let cs = vec![
            decision(0, A, Side::Buy, t, 100.0, t + 2 * H),
            decision(1, B, Side::Buy, t, 100.0, t + 2 * H),
            decision(2, C, Side::Sell, t, 100.0, t + 2 * H),
            decision(3, D, Side::Buy, t, 100.0, t + 2 * H),
            decision(4, D, Side::Buy, t + 4 * H, 100.0, t + 6 * H),
        ];
        let r = simulate(&flat_spec(), &md, &p, &cs, Arm::Capped(caps()));
        let rules: Vec<(&str, &str)> = r
            .refusals
            .iter()
            .map(|x| (x.instrument.as_str(), x.rule.as_str()))
            .collect();
        // A long 50; B long would make net 100 > 60; C short nets to 0 (gross
        // 100); D long would make gross 150 > 100; after the exits D trades.
        assert_eq!(
            rules,
            vec![(B, "max_net_exposure_usd"), (D, "max_gross_exposure_usd")]
        );
        assert_eq!(
            r.trades.iter().map(|t| t.seq).collect::<Vec<_>>(),
            vec![0, 2, 4]
        );
        assert!(
            r.trades.iter().all(|t| t.notional_usd == 50.0),
            "clamped to max_order"
        );
        assert_eq!(r.refusals[0].notional_usd, 50.0);
        assert!(
            r.refusals[1].detail.contains("gross 150.00 > 100.00"),
            "{}",
            r.refusals[1].detail
        );
        assert_eq!(r.stats.start_equity_usd, Some(100.0));
        // The research arm takes everything at the full notional.
        let all = simulate(&flat_spec(), &md, &p, &cs, Arm::Research);
        assert_eq!(all.trades.len(), 5);
        assert!(all.trades.iter().all(|t| t.notional_usd == 100.0));
        assert!(all.refusals.is_empty());
        assert_eq!(
            (Arm::Research.name(), Arm::Capped(caps()).name()),
            ("research", "capped")
        );
    }

    #[test]
    fn a_daily_loss_halts_until_the_next_utc_day_and_a_total_loss_for_good() {
        let d0 = utc("2026-09-28 00:00");
        let day = 24 * H;
        // −$15 a trade on $100: exit = entry × e^−0.15.
        let lose = 100.0 * (-0.15f64).exp();
        let mut bars = Vec::new();
        for k in 0..6 {
            bars.push((d0 + k * day, 100.0)); // close at 01:00 = entry
            bars.push((d0 + k * day + 2 * H, lose)); // close at 03:00 = exit
            bars.push((d0 + k * day + 4 * H, 100.0)); // close at 05:00
            bars.push((d0 + k * day + 6 * H, 100.0)); // close at 07:00
        }
        let md = market(vec![sparse(A, Interval::H1, &bars)]);
        let p = run_params(d0, d0 + 7 * day);
        let mut c = caps();
        c.max_order_notional_usd = 100.0;
        c.max_net_exposure_usd = 100.0;
        // Daily: day 0 loses 15 ≥ 10 ⇒ the 05:00 entry is refused; day 1 trades.
        c.total_loss_limit_usd = 50.0;
        let cs = vec![
            decision(0, A, Side::Buy, d0 + H, 100.0, d0 + 3 * H),
            decision(1, A, Side::Buy, d0 + 5 * H, 100.0, d0 + 7 * H),
            decision(2, A, Side::Buy, d0 + day + H, 100.0, d0 + day + 5 * H),
        ];
        let r = simulate(&flat_spec(), &md, &p, &cs, Arm::Capped(c.clone()));
        assert_eq!(r.refusals.len(), 1);
        assert_eq!(
            (r.refusals[0].seq, r.refusals[0].rule.as_str()),
            (1, "daily_loss_limit_usd")
        );
        assert_eq!(
            r.trades.iter().map(|t| t.seq).collect::<Vec<_>>(),
            vec![0, 2]
        );
        assert!((r.trades[0].net_usd + 15.0).abs() < 1e-9);
        // Total: −15 on day 0 and −15 on day 1 ⇒ equity 70 ≤ 100 − 25: no
        // entry on day 2 or day 5.
        c.total_loss_limit_usd = 25.0;
        c.daily_loss_limit_usd = 1_000.0;
        let cs = vec![
            decision(0, A, Side::Buy, d0 + H, 100.0, d0 + 3 * H),
            decision(1, A, Side::Buy, d0 + day + H, 100.0, d0 + day + 3 * H),
            decision(
                2,
                A,
                Side::Buy,
                d0 + 2 * day + H,
                100.0,
                d0 + 2 * day + 5 * H,
            ),
            decision(
                3,
                A,
                Side::Buy,
                d0 + 5 * day + H,
                100.0,
                d0 + 5 * day + 5 * H,
            ),
        ];
        let r = simulate(&flat_spec(), &md, &p, &cs, Arm::Capped(c));
        let rules: Vec<(usize, &str)> = r
            .refusals
            .iter()
            .map(|x| (x.seq, x.rule.as_str()))
            .collect();
        assert_eq!(
            rules,
            vec![(2, "total_loss_limit_usd"), (3, "total_loss_limit_usd")]
        );
        assert!((r.summary.max_drawdown_usd - 30.0).abs() < 1e-9);
        assert!((r.summary.max_drawdown_pct.unwrap() - 30.0).abs() < 1e-9);
    }

    #[test]
    fn drops_are_counted_and_runs_that_cannot_start_say_why() {
        let t = utc("2026-09-28 10:00");
        let md = market(vec![sparse(A, Interval::H1, &[(t - H, 100.0)])]);
        let p = run_params(t - H, t + 5 * H);
        let mut future = decision(1, A, Side::Buy, t, 100.0, t + 2 * H);
        future.data_asof_ms = t + 1;
        let mut nocost = decision(
            2,
            "solana:So11111111111111111111111111111111111111112",
            Side::Buy,
            t,
            1.0,
            t + H,
        );
        nocost.legs[0].entry_px = 1.0;
        let cs = vec![
            decision(0, A, Side::Buy, t, 100.0, t + 2 * H),
            future,
            nocost,
        ];
        let mut md2 = md.clone();
        md2.bars.insert(
            "solana:So11111111111111111111111111111111111111112".into(),
            sparse(
                "solana:So11111111111111111111111111111111111111112",
                Interval::H1,
                &[(t, 1.0)],
            ),
        );
        let r = simulate(&flat_spec(), &md2, &p, &cs, Arm::Research);
        let why: Vec<SkipReason> = r.skipped.iter().map(|k| k.reason).collect();
        assert_eq!(
            why,
            vec![
                SkipReason::MissingExit,
                SkipReason::FutureData,
                SkipReason::NoCosts
            ]
        );
        assert!(r.trades.is_empty());
        assert!(
            SkipReason::MissingExit.is_missing_data() && !SkipReason::NoCosts.is_missing_data()
        );
        assert_eq!(count_skips(&r.skipped)["missing_exit"], 1);

        // Runs that cannot start.
        let w = spec(
            json!({"kind": "weekend_window", "universe": "@xyz", "interval": "1h",
            "calendar": "us_equity", "direction": "fade"}),
        );
        assert!(candidates(&w, &md, &p)
            .unwrap_err()
            .contains("universe @xyz is not resolved"));
        let mut p2 = p.clone();
        p2.universe = vec![A.into()];
        p2.calendars.clear();
        assert!(candidates(&w, &md, &p2)
            .unwrap_err()
            .contains("calendar `us_equity`"));
        let mut p3 = p.clone();
        p3.to_ms = p3.from_ms;
        assert!(candidates(&flat_spec(), &md, &p3)
            .unwrap_err()
            .contains("is not before"));
        let five = market(vec![sparse(A, Interval::M5, &[(t, 1.0)])]);
        assert!(candidates(&flat_spec(), &five, &p)
            .unwrap_err()
            .contains("are 5m; the spec reads 1h"));
        // Bar kinds: an instrument without bars is a note.
        let mut p4 = p.clone();
        p4.universe = vec![A.into(), B.into()];
        let set = candidates(&flat_spec(), &md, &p4).unwrap();
        assert_eq!(set.notes, vec![format!("no 1h bars for {B}")]);
    }
}
