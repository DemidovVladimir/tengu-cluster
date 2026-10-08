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
//! | Entry liquidity (`min_entry_trades`) | a candidate whose entry bar (the bar ending at the decision) counts fewer trades (`n`) is skipped (`thin_entry`) before any ranking, cooldown or position; a bar without `n` passes — HL keeps a no-trade hour as a flat bar at the last close (n = 0): a stale price |
//! | Information labels (`weekend_window` `labels`, `labels.rs`) | each name past the price, cost and `thin_entry` checks is labelled NEWS / UNCERTAIN / NOISE from what was published in [anchor − `lookback_mins`, decision] ([`MarketData::info`]); a class in `skip` ⇒ `label_skipped` before `top_n` ranks the rest; candidates carry `info_label` (absent for an unlabelled spec — older run dirs parse); not in the Jev gate event |
//! | Fill | entry and exit at the instant's close; gross bps = side × (exit / entry − 1) × 10⁴ — a linear USD perp's simple return (signals stay log moves); net bps = gross − (entry + exit cost) + funding, all in bps of the entry notional; USD = net bps × notional / 10⁴; a pair: each leg `notional` / 2, the trade's bps the legs' mean |
//! | Per candidate (`simulate`) | 1. what the decision shows: read past it (`future_data`), no leg, a leg without a cost (`no_costs`) ⇒ dropped, never in a book; 2. the capped book as of the decision (exits ≤ it realized, then the caps); 3. the fill: the exit plan walked, prices at its exit — none ⇒ `missing_exit` |
//! | `research` arm | every candidate at the spec's notional, no caps; candidates of one instant by instrument key; a plan whose horizon (planned exit, or the max hold of a TP / SL, funding or z exit) passes the end of a leg's data is `missing_exit` even when its path exited earlier — keeping only the early exits there would pick trades by outcome; drawdown in USD and bps of one trade's notional (`max_drawdown_bps`) — no %: it keeps no cash book |
//! | `capped` arm ([`RiskCaps`]) | a ledger: in time order, the candidates of one instant by descending \|signal\| (ties: instrument key) — the caps keep the highest-conviction trades; positions open until their exit, exits at an instant before entries; an admitted candidate whose exit has no price (a gap, or still open where the data ends) holds its exposure until its exit instant, P&L unknown and never booked (`missing_exit`) — admissions read only the book as of the decision, never whether a later bar exists; notional clamped to `max_order_notional_usd`; refused, rule = the `[risk]` field: `total_loss_limit_usd` once realized equity ≤ initial − limit (for good; equity read once per exit instant, after every exit of it) · `daily_loss_limit_usd` while the UTC day's realized P&L ≤ −limit · `max_gross_exposure_usd` / `max_net_exposure_usd` when the open notional with this one would exceed; drawdown in USD and % of `initial_cash_usd` |
//! | Skips ([`SkipReason`]) | excluded · missing_anchor / missing_entry / missing_price · flat · below_min_signal · not_top_n · thin_entry · label_skipped (its class in `info_label`; counted as `label_skipped:<CLASS>`) · no_costs (no `costs`, no `[backtest.costs]` prefix) · missing_exit (no price at the exit, or the data ends before the plan's horizon: not a trade) · future_data; an arm's drop carries its candidate's `seq` |
//! | Share splits ([`MarketData::adjust_for_splits`]) | before any decision: each instrument's bars closed before each of its splits (and ctx rows before it) split-adjusted (`marketdata::StockSplit`); a bar straddling the split (a day bar around an intraday split) dropped — its prices mix both share counts; one data note per split that changed a row |
//! | Data through ([`MarketData::cut_after`], [`MarketData::newest_ms`]) | a run reads what the warehouse held: its report records `data_through_ms` = the newest bar close, funding or ctx row, or event loaded; given one (`tengu backtest --data-through`), rows after it are cut before any decision — a rerun over a grown `market.db` reads the same data (lineage D1) |
//! | `max_candidates` ([`RunParams`]) | [`candidates`] stops with an error once a run passes it — nothing simulated or written (`[backtest] max_candidates`) |

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::domain::backtest::costs::{cost_for, CostSpec};
use crate::domain::backtest::fills::{
    funding_over, gross_bps, side_cost, spread_points, walk_bars_exit, walk_funding_exit,
    walk_spread_exit, ExitReason,
};
use crate::domain::backtest::kinds;
use crate::domain::backtest::labels::{InfoData, InfoLabel};
use crate::domain::backtest::spec::{StrategyKind, StrategySpec};
use crate::domain::backtest::stats::{StatsParams, Summary};
use crate::domain::book::Side;
use crate::domain::calendar::Calendar;
use crate::domain::marketdata::{fmt_time, BarSeries, CtxSeries, FundingSeries, StockSplit};
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
    /// What `weekend_window` labels read (`labels.rs`); empty unless the
    /// spec has `labels`.
    pub info: InfoData,
}

impl MarketData {
    /// The newest observation loaded — the latest bar close, funding row,
    /// ctx row or event (`info`); `None` when nothing is (a run's
    /// `data_through_ms`).
    pub fn newest_ms(&self) -> Option<i64> {
        let bars = self
            .bars
            .values()
            .flat_map(|s| s.bars.iter().map(move |b| b.t_close_ms(s.interval)));
        let funding = self
            .funding
            .values()
            .flat_map(|s| s.points.iter().map(|p| p.t_ms));
        let ctx = self
            .ctx
            .values()
            .flat_map(|s| s.points.iter().map(|p| p.t_ms));
        bars.chain(funding)
            .chain(ctx)
            .chain(self.info.newest_ms())
            .max()
    }

    /// Keep what was known at `through_ms` (module table): bars closed by
    /// it, funding and ctx rows stamped by it, events published by it
    /// (`InfoData::cut_after`: coverage clipped too); a series left empty
    /// goes. Returns the rows removed.
    pub fn cut_after(&mut self, through_ms: i64) -> usize {
        let mut removed = self.info.cut_after(through_ms);
        for s in self.bars.values_mut() {
            let (iv, before) = (s.interval, s.bars.len());
            s.bars.retain(|b| b.t_close_ms(iv) <= through_ms);
            removed += before - s.bars.len();
        }
        for s in self.funding.values_mut() {
            let before = s.points.len();
            s.points.retain(|p| p.t_ms <= through_ms);
            removed += before - s.points.len();
        }
        for s in self.ctx.values_mut() {
            let before = s.points.len();
            s.points.retain(|p| p.t_ms <= through_ms);
            removed += before - s.points.len();
        }
        self.bars.retain(|_, s| !s.bars.is_empty());
        self.funding.retain(|_, s| !s.points.is_empty());
        self.ctx.retain(|_, s| !s.points.is_empty());
        removed
    }

    /// Split-adjust the series (module table): for each instrument's
    /// splits, its bars closed before the split and its ctx rows before it
    /// (`adjust_for_split`; funding is a rate, untouched); a bar straddling
    /// the split is dropped. Returns one data note per split that changed a
    /// row, ids in full.
    pub fn adjust_for_splits(&mut self, splits: &BTreeMap<String, Vec<StockSplit>>) -> Vec<String> {
        let mut notes = Vec::new();
        for (id, list) in splits {
            for split in list {
                let (bars, dropped, iv) = match self.bars.get_mut(id) {
                    Some(s) => {
                        let a = s.adjust_for_split(*split);
                        (a.adjusted, a.dropped_open_ms, s.interval.ms())
                    }
                    None => (0, Vec::new(), 0),
                };
                let ctx = self
                    .ctx
                    .get_mut(id)
                    .map_or(0, |s| s.adjust_for_split(*split));
                if bars + ctx == 0 && dropped.is_empty() {
                    continue;
                }
                let r = split.ratio;
                let ctx_text = if ctx > 0 {
                    format!(" and {ctx} ctx rows")
                } else {
                    String::new()
                };
                let dropped_text = if dropped.is_empty() {
                    String::new()
                } else {
                    let spans: Vec<String> = dropped
                        .iter()
                        .map(|t| format!("{} → {}", fmt_time(*t), fmt_time(t + iv)))
                        .collect();
                    format!(
                        "; dropped the bar {} — it opens before the split and closes after it \
                         (its open, high, low and volume mix both share counts): a decision \
                         priced there is skipped as missing",
                        spans.join(", ")
                    )
                };
                notes.push(format!(
                    "split-adjusted {id}: ratio {r} (new shares per old) at {} — {bars} bars{ctx_text} \
                     before it: prices ÷ {r}, volume × {r}{dropped_text}",
                    fmt_time(split.at_ms)
                ));
            }
        }
        notes
    }
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
    /// `[backtest] max_candidates`: [`candidates`] stops with an error once
    /// a run passes it — before any arm, file or byte of output.
    pub max_candidates: usize,
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

impl ExitPlan {
    /// The latest instant the plan can exit at: the planned instant, else
    /// the longest hold (a TP / SL, funding or z exit may come earlier).
    pub fn horizon_ms(&self) -> i64 {
        match self {
            ExitPlan::At { exit_ms } => *exit_ms,
            ExitPlan::Bars { max_exit_ms, .. }
            | ExitPlan::Funding { max_exit_ms, .. }
            | ExitPlan::Spread { max_exit_ms, .. } => *max_exit_ms,
        }
    }
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
    /// `event_window`: the event's operator text (may hold hindsight).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// A labelled `weekend_window`'s class at the decision (`labels.rs`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub info_label: Option<InfoLabel>,
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
    /// The entry bar counts fewer trades than `min_entry_trades`.
    ThinEntry,
    /// The name's information class is in the spec's `labels.skip`.
    LabelSkipped,
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
            SkipReason::ThinEntry => "thin_entry",
            SkipReason::LabelSkipped => "label_skipped",
            SkipReason::NoCosts => "no_costs",
            SkipReason::MissingExit => "missing_exit",
            SkipReason::FutureData => "future_data",
        }
    }

    /// A data gap (the report flags them), not a rule's choice.
    #[cfg_attr(not(test), allow(dead_code))] // the report reads `missing_*` keys; tools may ask
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
    /// The candidate's `seq` — an arm's drop (`simulate`); a candidate-level
    /// skip has none (no candidate was made).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<usize>,
    /// The name's class when it was labelled before the skip
    /// (`label_skipped`, `below_min_signal`, `not_top_n` of a labelled spec;
    /// an arm's drop of a labelled candidate).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub info_label: Option<InfoLabel>,
}

impl Skip {
    /// The key skip counts use: the reason, a label skip's class with it
    /// (`label_skipped:NEWS`).
    pub fn count_key(&self) -> String {
        match (self.reason, self.info_label) {
            (SkipReason::LabelSkipped, Some(l)) => {
                format!("{}:{}", self.reason.as_str(), l.as_str())
            }
            _ => self.reason.as_str().to_string(),
        }
    }
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
    /// Skips per reason ([`Skip::count_key`]).
    pub fn skip_counts(&self) -> BTreeMap<String, usize> {
        count_skips(&self.skipped)
    }
}

pub(crate) fn count_skips(skips: &[Skip]) -> BTreeMap<String, usize> {
    let mut m = BTreeMap::new();
    for s in skips {
        *m.entry(s.count_key()).or_insert(0) += 1;
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
    /// In the order the arm took them: by decision time, then (research) the
    /// instrument key or (capped) descending \|signal\| — module table.
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
    gross: f64,
    net: f64,
    /// `None`: admitted, but its exit has no price (`missing_exit`) — it
    /// holds its exposure until `exit_ms`; its P&L is unknown, never booked.
    net_usd: Option<f64>,
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

    /// Close every position with exit ≤ `t`. Equity moves once per exit
    /// instant — every exit of an instant fills at the same close — and the
    /// total-loss halt reads it then, never between two exits of one
    /// instant (the order inside an instant is only the seq).
    fn realize(&mut self, t: i64) {
        let mut by_instant: BTreeMap<i64, f64> = BTreeMap::new();
        self.open.retain(|p| {
            let closed = p.exit_ms <= t;
            if closed {
                *by_instant.entry(p.exit_ms).or_insert(0.0) += p.net_usd.unwrap_or(0.0);
            }
            !closed
        });
        for (exit_ms, usd) in by_instant {
            self.equity += usd;
            *self
                .day_pnl
                .entry(exit_ms.div_euclid(DAY_MS))
                .or_insert(0.0) += usd;
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

    /// Hold `exposure` (signed leg notionals) until `exit_ms`; `net_usd`
    /// is booked then — `None` when the exit has no price: the P&L is
    /// unknown and never booked, the exposure still counts until `exit_ms`.
    fn open(&mut self, exposure: &[f64], exit_ms: i64, net_usd: Option<f64>) {
        self.open.push(Open {
            exit_ms,
            gross: exposure.iter().map(|x| x.abs()).sum(),
            net: exposure.iter().sum(),
            net_usd,
        });
    }
}

/// An admitted candidate without a trade: why, and until when the capped
/// book holds its exposure (its exit instant, priced or not).
struct Unfilled {
    reason: SkipReason,
    hold_until: i64,
}

/// What the decision itself shows (module table): a candidate that read
/// past its decision, has no leg or a leg without a cost is dropped before
/// any cap — it never enters a book. `Ok` = each leg's cost.
fn admissible<'a>(
    spec: &'a StrategySpec,
    params: &'a RunParams,
    c: &Candidate,
) -> Result<Vec<&'a CostSpec>, SkipReason> {
    if c.data_asof_ms > c.decided_at_ms {
        return Err(SkipReason::FutureData);
    }
    if c.legs.is_empty() {
        return Err(SkipReason::MissingPrice);
    }
    c.legs
        .iter()
        .map(|leg| {
            spec.costs
                .as_ref()
                .or_else(|| cost_for(&params.costs, &leg.instrument))
                .ok_or(SkipReason::NoCosts)
        })
        .collect()
}

/// The close of the last bar of `id` — where its data ends.
fn data_end(md: &MarketData, id: &str) -> Option<i64> {
    let s = md.bars.get(id)?;
    s.bars.last().map(|b| b.t_close_ms(s.interval))
}

/// Where `c`'s exit plan exits (module table): the planned instant, or the
/// walk of its TP / SL, funding or z exit over the data; `None` without
/// the series the walk reads.
fn exit_of(spec: &StrategySpec, md: &MarketData, c: &Candidate) -> Option<(i64, ExitReason)> {
    let first = c.legs.first()?;
    let entry = c.decided_at_ms;
    Some(match &c.exit {
        ExitPlan::At { exit_ms } => (*exit_ms, ExitReason::Window),
        ExitPlan::Bars {
            max_exit_ms,
            take_profit_bps,
            stop_loss_bps,
        } => walk_bars_exit(
            md.bars.get(&first.instrument)?,
            entry,
            first.entry_px,
            first.side,
            *max_exit_ms,
            *take_profit_bps,
            *stop_loss_bps,
        ),
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
                return None;
            };
            let (sa, sb) = (md.bars.get(&a.instrument)?, md.bars.get(&b.instrument)?);
            let pts = spread_points(sa, sb, p.lookback_bars as usize);
            walk_spread_exit(&pts, entry, *max_exit_ms, *exit_z)
        }
    })
}

/// Fill an admitted `c` at `notional` with its legs' `costs` (module
/// table). `censor` (research arms): a plan whose horizon passes the end
/// of a leg's data is unfilled even when its path exited earlier — keeping
/// only the early exits there would pick trades by their outcome.
fn fill(
    spec: &StrategySpec,
    md: &MarketData,
    c: &Candidate,
    costs: &[&CostSpec],
    notional: f64,
    censor: bool,
) -> Result<Trade, Unfilled> {
    let entry = c.decided_at_ms;
    let missing = |hold_until: i64| Unfilled {
        reason: SkipReason::MissingExit,
        hold_until,
    };
    let (exit_ms, exit_reason) = exit_of(spec, md, c).ok_or(missing(c.exit.horizon_ms()))?;
    if exit_ms <= entry {
        return Err(missing(exit_ms));
    }
    if censor {
        let end = c
            .legs
            .iter()
            .map(|l| data_end(md, &l.instrument))
            .min()
            .flatten();
        if end.is_none_or(|e| c.exit.horizon_ms() > e) {
            return Err(missing(exit_ms));
        }
    }
    let leg_notional = notional / c.legs.len() as f64;
    let mut legs = Vec::with_capacity(c.legs.len());
    for (leg, cost) in c.legs.iter().zip(costs) {
        let series = md.bars.get(&leg.instrument).ok_or(missing(exit_ms))?;
        let exit_px = series.close_at(exit_ms).ok_or(missing(exit_ms))?;
        let gross = gross_bps(leg.side, leg.entry_px, exit_px).ok_or(missing(exit_ms))?;
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

/// |signal| — how far a candidate's move went; not a number ⇒ 0 (taken last).
fn conviction(c: &Candidate) -> f64 {
    let s = c.signal_bps.abs();
    if s.is_nan() {
        0.0
    } else {
        s
    }
}

/// One arm over `candidates` (module table): trades in the order taken, the
/// capped arm's refusals, candidates dropped at the decision or unfilled,
/// the summary. Per candidate: what the decision shows (`admissible`), then
/// the capped book as of the decision (`realize` + `refusal`), then the
/// fill — an unfilled admitted candidate keeps its exposure in the book
/// until its exit instant, so no admission ever depends on a later bar.
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
        let at = a.decided_at_ms.cmp(&b.decided_at_ms);
        // The caps keep the highest-conviction trades of an instant.
        let ranked = match &arm {
            Arm::Capped(_) => at.then_with(|| conviction(b).total_cmp(&conviction(a))),
            Arm::Research => at,
        };
        ranked
            .then_with(|| a.instrument.cmp(&b.instrument))
            .then(a.seq.cmp(&b.seq))
    });
    let (mut trades, mut refusals, mut skipped) = (Vec::new(), Vec::new(), Vec::new());
    let mut book = match &arm {
        Arm::Capped(caps) => Some(Book::new(caps)),
        Arm::Research => None,
    };
    let censor = matches!(arm, Arm::Research);
    for c in order {
        let size = match &arm {
            Arm::Capped(caps) => notional.min(caps.max_order_notional_usd),
            Arm::Research => notional,
        };
        let drop = |reason: SkipReason| Skip {
            instrument: c.instrument.clone(),
            decided_at_ms: c.decided_at_ms,
            period: c.period.clone(),
            reason,
            seq: Some(c.seq),
            info_label: c.info_label,
        };
        let costs = match admissible(spec, params, c) {
            Ok(costs) => costs,
            Err(reason) => {
                skipped.push(drop(reason));
                continue;
            }
        };
        let exposure: Vec<f64> = c
            .legs
            .iter()
            .map(|l| l.side.sign() * size / c.legs.len() as f64)
            .collect();
        if let Some(book) = book.as_mut() {
            book.realize(c.decided_at_ms);
            if let Some((rule, detail)) = book.refusal(c.decided_at_ms, &exposure) {
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
        match fill(spec, md, c, &costs, size, censor) {
            Ok(t) => {
                if let Some(book) = book.as_mut() {
                    book.open(&exposure, t.exit_ms, Some(t.net_usd));
                }
                trades.push(t);
            }
            Err(Unfilled { reason, hold_until }) => {
                if let Some(book) = book.as_mut() {
                    book.open(&exposure, hold_until, None);
                }
                skipped.push(drop(reason));
            }
        }
    }
    // Drawdown: the capped arm's of its own cash book (%), the research
    // arm's of one trade (bps) — it trades every candidate, with no book.
    let (start_equity_usd, trade_notional_usd) = match &arm {
        Arm::Capped(caps) => (Some(caps.initial_cash_usd), None),
        Arm::Research => (None, Some(notional)),
    };
    let stats = StatsParams {
        bootstrap: params.bootstrap,
        seed: params.seed,
        periods_per_year: spec.period_kind().per_year(),
        decided_ms: Some((params.from_ms, params.to_ms)),
        start_equity_usd,
        trade_notional_usd,
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

    /// Lineage D1: `cut_after` keeps what was known at the bound — bars
    /// closed by it, funding stamped by it — and drops a series left empty;
    /// `newest_ms` is the latest bar close or row.
    #[test]
    fn cut_after_keeps_what_was_known_then() {
        let t0 = utc("2026-09-01 00:00");
        let mut md = market(vec![
            series("a", Interval::H1, t0, &[1.0, 2.0, 3.0]),
            series("b", Interval::H1, t0 + 5 * H, &[1.0]),
        ]);
        md.funding.insert(
            "a".into(),
            funding("a", &[(t0 + H + 37, 1e-6), (t0 + 2 * H + 37, 1e-6)]),
        );
        assert_eq!(md.newest_ms(), Some(t0 + 6 * H));
        // At t0 + 2 h: bars closing at +1 h, +2 h; the +1 h funding row.
        assert_eq!(md.cut_after(t0 + 2 * H), 3);
        assert_eq!(md.bars["a"].bars.len(), 2);
        assert!(!md.bars.contains_key("b"));
        assert_eq!(md.funding["a"].points.len(), 1);
        assert_eq!(md.newest_ms(), Some(t0 + 2 * H));
        assert_eq!(MarketData::default().newest_ms(), None);
    }

    use super::*;
    use crate::domain::backtest::testkit::{
        funding, market, run_params, series, sparse, spec, utc, H,
    };
    use crate::domain::marketdata::{BarSeries, Interval};

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
            info_label: None,
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

    /// The capped arm takes an instant's candidates by descending |signal|
    /// (ties: instrument key; not a number: last), so a $100 gross cap keeps
    /// the two largest moves; the research arm keeps instrument order; seqs
    /// never change.
    #[test]
    fn the_capped_arm_takes_the_largest_moves_of_an_instant_first() {
        let t = utc("2026-09-28 10:00");
        let flat = |id| sparse(id, Interval::H1, &[(t - H, 100.0), (t + H, 100.0)]);
        let md = market(vec![flat(A), flat(B), flat(C), flat(D)]);
        let p = run_params(t - 10 * H, t + 10 * H);
        let mut c = caps();
        c.max_net_exposure_usd = 100.0; // gross binds: two $50 positions
                                        // signals of A, B, C, D (seq 0–3) → capped trades in the order taken.
        let cases: [([f64; 4], [&str; 2]); 5] = [
            ([10.0, -300.0, 200.0, 50.0], [B, C]),
            ([-5.0, 5.0, 4.0, -4.0], [A, B]),
            ([100.0, 100.0, -100.0, 5.0], [A, B]),
            ([f64::NAN, 1.0, 2.0, 3.0], [D, C]),
            ([0.0, 0.0, 0.0, 0.0], [A, B]),
        ];
        for (signals, want) in cases {
            let cs: Vec<Candidate> = [A, B, C, D]
                .iter()
                .zip(signals)
                .enumerate()
                .map(|(seq, (id, s))| {
                    let mut d = decision(seq, id, Side::Buy, t, 100.0, t + 2 * H);
                    d.signal_bps = s;
                    d
                })
                .collect();
            let r = simulate(&flat_spec(), &md, &p, &cs, Arm::Capped(c.clone()));
            let taken: Vec<&str> = r.trades.iter().map(|t| t.instrument.as_str()).collect();
            assert_eq!(taken, want, "{signals:?}");
            assert_eq!(r.refusals.len(), 2, "{signals:?}");
            assert!(r
                .refusals
                .iter()
                .all(|x| x.rule == "max_gross_exposure_usd"));
            for tr in &r.trades {
                assert_eq!(cs[tr.seq].instrument, tr.instrument, "seq unchanged");
            }
            let research = simulate(&flat_spec(), &md, &p, &cs, Arm::Research);
            let order: Vec<usize> = research.trades.iter().map(|t| t.seq).collect();
            assert_eq!(order, vec![0, 1, 2, 3], "research: instrument order");
        }
        // Instants stay in time order: a later instant's big move never
        // jumps an earlier one.
        let mut early = decision(0, A, Side::Buy, t, 100.0, t + 2 * H);
        early.signal_bps = 1.0;
        let mut late = decision(1, B, Side::Buy, t + H, 100.0, t + 2 * H);
        late.signal_bps = 900.0;
        let md2 = market(vec![
            sparse(A, Interval::H1, &[(t - H, 100.0), (t + H, 100.0)]),
            sparse(B, Interval::H1, &[(t, 100.0), (t + H, 100.0)]),
        ]);
        let mut one = caps();
        one.max_gross_exposure_usd = 50.0;
        let r = simulate(&flat_spec(), &md2, &p, &[late, early], Arm::Capped(one));
        assert_eq!(r.trades.iter().map(|t| t.seq).collect::<Vec<_>>(), vec![0]);
        assert_eq!(r.refusals[0].seq, 1);
    }

    /// A 3-for-1 split between entry and exit: the raw series books the
    /// split as a fall to a third; adjusted, the candidate, its features and the
    /// trade equal the unsplit series' (KIOXIA, weekend of 2026-09-25).
    #[test]
    fn a_split_mid_hold_gives_the_unsplit_trade() {
        let t0 = utc("2026-09-17 00:00");
        let at = utc("2026-09-28 08:00");
        // A wave, then a Sunday jump the rule fades.
        let closes: Vec<f64> = (0..(12 * 24))
            .map(|h| {
                let t = t0 + h as i64 * H;
                let jump = if (utc("2026-09-27 12:00")..at).contains(&t) {
                    1.05
                } else {
                    1.0
                };
                (300.0 + 10.0 * ((h as f64) / 7.0).sin()) * jump
            })
            .collect();
        let unsplit = market(vec![series(A, Interval::H1, t0, &closes)]);
        // What the venue stored: pre-split prices 3× higher, a third of the volume.
        let mut raw = unsplit.clone();
        for b in raw.bars.get_mut(A).unwrap().bars.iter_mut() {
            if b.t_open_ms < at {
                (b.o, b.h, b.l, b.c, b.v) = (b.o * 3.0, b.h * 3.0, b.l * 3.0, b.c * 3.0, b.v / 3.0);
            }
        }
        let s = spec(
            json!({"kind": "weekend_window", "universe": [A], "interval": "1h",
            "calendar": "us_equity", "direction": "fade"}),
        );
        let p = run_params(utc("2026-09-21 00:00"), utc("2026-09-29 00:00"));
        let trade = |md: &MarketData| {
            let set = candidates(&s, md, &p).unwrap();
            assert_eq!(set.candidates.len(), 1, "{:?}", set.skipped);
            let r = simulate(&s, md, &p, &set.candidates, Arm::Research);
            (set.candidates[0].clone(), r.trades[0].clone())
        };
        let (c0, t_unsplit) = trade(&unsplit);
        assert!(
            t_unsplit.entry_ms < at && at < t_unsplit.exit_ms,
            "mid-hold"
        );
        // Raw: the split reads as a fall to a third the short books as
        // profit — entry 3×, exit as is.
        let (_, t_raw) = trade(&raw);
        let (e, x) = (t_unsplit.legs[0].entry_px, t_unsplit.legs[0].exit_px);
        assert_eq!(
            (t_raw.legs[0].entry_px, t_raw.legs[0].exit_px),
            (e * 3.0, x)
        );
        assert!((t_raw.gross_bps - (1.0 - x / (3.0 * e)) * 1e4).abs() < 1e-6);
        assert!(t_raw.gross_bps > 6_000.0, "{}", t_raw.gross_bps);

        let mut adjusted = raw.clone();
        let splits = BTreeMap::from([(
            A.to_string(),
            vec![StockSplit {
                at_ms: at,
                ratio: 3.0,
            }],
        )]);
        let notes = adjusted.adjust_for_splits(&splits);
        assert_eq!(notes.len(), 1);
        assert!(
            notes[0].starts_with(&format!(
                "split-adjusted {A}: ratio 3 (new shares per old) at 2026-09-28T08:00:00Z — 272 bars"
            )),
            "{}",
            notes[0]
        );
        let (c1, t_adjusted) = trade(&adjusted);
        let close = |a: f64, b: f64| (a - b).abs() < 1e-9 * a.abs().max(1.0);
        assert_eq!(
            (c1.side, c1.decided_at_ms, c1.seq),
            (c0.side, c0.decided_at_ms, c0.seq)
        );
        assert!(close(c1.signal_bps, c0.signal_bps));
        assert_eq!(
            c1.features.keys().collect::<Vec<_>>(),
            c0.features.keys().collect::<Vec<_>>()
        );
        for (k, v) in &c0.features {
            assert!(close(c1.features[k], *v), "{k}: {} vs {v}", c1.features[k]);
        }
        for (a, b) in [
            (t_adjusted.gross_bps, t_unsplit.gross_bps),
            (t_adjusted.net_bps, t_unsplit.net_bps),
            (t_adjusted.legs[0].entry_px, t_unsplit.legs[0].entry_px),
            (t_adjusted.legs[0].exit_px, t_unsplit.legs[0].exit_px),
        ] {
            assert!(close(a, b), "{a} vs {b}");
        }
        // An id without splits, or a split before its data, changes nothing.
        let mut none = raw.clone();
        let later = BTreeMap::from([
            (
                B.to_string(),
                vec![StockSplit {
                    at_ms: at,
                    ratio: 3.0,
                }],
            ),
            (
                A.to_string(),
                vec![StockSplit {
                    at_ms: t0,
                    ratio: 3.0,
                }],
            ),
        ]);
        assert!(none.adjust_for_splits(&later).is_empty());
        assert_eq!(none, raw);
    }

    #[test]
    fn a_daily_loss_halts_until_the_next_utc_day_and_a_total_loss_for_good() {
        let d0 = utc("2026-09-28 00:00");
        let day = 24 * H;
        // −$15 a trade on $100: exit = entry × 0.85 (simple return).
        let lose = 85.0;
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

    /// Regression (review: split straddling a bar): KIOXIA's 3-for-1 split
    /// at 2026-09-28 08:00 UTC on 1d bars — the day bar opening 00:00 opens
    /// pre-split (356.5) and closes post-split (114.12). Dividing its close
    /// by 3 (38.04) booked a fake −68 % / +190 % pair of moves; the bar is
    /// dropped instead (noted), so no decision prices on a mixed bar.
    #[test]
    fn a_split_inside_a_bar_never_fabricates_a_move() {
        let d0 = utc("2026-09-20 00:00");
        let day = 24 * H;
        let at = utc("2026-09-28 08:00");
        // Raw venue bars: pre-split ~357–362 until the 09-28 bar, which
        // opens before the split and closes after it; post-split ~111–113.
        let closes = [
            360.0, 362.0, 358.0, 361.0, 359.0, 357.0, 356.0, 356.5, 114.12, 110.63, 112.0, 111.0,
            113.0,
        ];
        let bars: Vec<crate::domain::marketdata::Bar> = closes
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let t = d0 + i as i64 * day;
                if t == utc("2026-09-28 00:00") {
                    crate::domain::backtest::testkit::ohlc(t, 356.5, 360.0, 110.0, *c)
                } else {
                    crate::domain::backtest::testkit::ohlc(t, *c, *c, *c, *c)
                }
            })
            .collect();
        let mut md = market(vec![BarSeries::new(A, Interval::D1, bars)]);
        let splits = BTreeMap::from([(
            A.to_string(),
            vec![StockSplit {
                at_ms: at,
                ratio: 3.0,
            }],
        )]);
        let notes = md.adjust_for_splits(&splits);
        let s = spec(
            json!({"kind": "move_trigger", "universe": [A], "interval": "1d", "lookback_bars": 1,
            "threshold_bps": 300, "direction": "fade", "hold_bars": 1}),
        );
        let p = run_params(d0 + 2 * day, d0 + 12 * day);
        let set = candidates(&s, &md, &p).unwrap();
        // Every adjusted close is on the post-split scale; nothing moved 3 %.
        assert!(
            set.candidates.is_empty(),
            "a fabricated move: {:?}",
            set.candidates
                .iter()
                .map(|c| (fmt_time(c.decided_at_ms), c.signal_bps))
                .collect::<Vec<_>>()
        );
        let r = simulate(&s, &md, &p, &set.candidates, Arm::Research);
        assert!(r.trades.is_empty());
        // The straddling bar is gone, said in the note; the rest adjusted.
        let series = &md.bars[A];
        assert!(series.bar_ending_at(utc("2026-09-29 00:00")).is_none());
        let pre = series.close_at(utc("2026-09-28 00:00")).unwrap();
        assert!((pre - 356.5 / 3.0).abs() < 1e-9, "{pre}");
        assert_eq!(series.close_at(utc("2026-09-30 00:00")), Some(110.63));
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(
            notes[0].contains("8 bars") && notes[0].contains("2026-09-28T00:00:00Z"),
            "{}",
            notes[0]
        );
    }

    /// Regression (review: capped admission read a future exit bar): A
    /// (|s| 300) and B (|s| 200) at one instant, room for one $50
    /// position. A's exit bar is missing; the book cannot know that at the
    /// decision, so A keeps its slot until its planned exit (reported
    /// `missing_exit`, P&L unknown) and B is refused — not traded in A's
    /// place. A later candidate after A's planned exit gets the room.
    #[test]
    fn a_fill_drop_never_frees_capacity() {
        let t = utc("2026-09-28 10:00");
        let md = market(vec![
            // A: entry close, no bar at t + 2h (its exit), bars after.
            sparse(
                A,
                Interval::H1,
                &[(t - H, 100.0), (t + 3 * H, 101.0), (t + 4 * H, 101.0)],
            ),
            sparse(B, Interval::H1, &[(t - H, 100.0), (t + H, 99.0)]),
            sparse(C, Interval::H1, &[(t + 2 * H, 100.0), (t + 4 * H, 102.0)]),
        ]);
        let p = run_params(t - 10 * H, t + 10 * H);
        let mut a = decision(0, A, Side::Buy, t, 100.0, t + 2 * H);
        a.signal_bps = 300.0;
        let mut b = decision(1, B, Side::Buy, t, 100.0, t + 2 * H);
        b.signal_bps = 200.0;
        let mut c = decision(2, C, Side::Buy, t + 3 * H, 100.0, t + 5 * H);
        c.signal_bps = 10.0;
        let mut one = caps();
        one.max_gross_exposure_usd = 50.0;
        let r = simulate(&flat_spec(), &md, &p, &[a, b, c], Arm::Capped(one));
        let refused: Vec<(&str, &str)> = r
            .refusals
            .iter()
            .map(|x| (x.instrument.as_str(), x.rule.as_str()))
            .collect();
        assert_eq!(refused, vec![(B, "max_gross_exposure_usd")]);
        assert_eq!(
            r.skipped
                .iter()
                .map(|k| (k.instrument.as_str(), k.reason))
                .collect::<Vec<_>>(),
            vec![(A, SkipReason::MissingExit)]
        );
        assert_eq!(
            r.trades
                .iter()
                .map(|x| x.instrument.as_str())
                .collect::<Vec<_>>(),
            vec![C],
            "after A's planned exit the room is back"
        );
        // The research arm has no book: B trades, A is dropped.
        let research = simulate(
            &flat_spec(),
            &md,
            &p,
            &[
                decision(0, A, Side::Buy, t, 100.0, t + 2 * H),
                decision(1, B, Side::Buy, t, 100.0, t + 2 * H),
            ],
            Arm::Research,
        );
        assert_eq!(research.trades.len(), 1);
        assert_eq!(research.skipped[0].reason, SkipReason::MissingExit);
    }

    /// Regression (review: transient equity between same-instant exits):
    /// four −5 % days leave $100 at ~$80; then a −6 % and a +6 % position
    /// exit at the same instant (the loser first by seq). Equity never sits
    /// at ~$74 — the instant nets ~0 — so the next day's entry is taken,
    /// not refused for a total-loss halt that never happened.
    #[test]
    fn a_total_loss_halt_reads_equity_after_all_exits_of_an_instant() {
        let d0 = utc("2026-09-21 00:00");
        let day = 24 * H;
        let mut a_bars = Vec::new();
        let mut b_bars = Vec::new();
        for k in 0..6 {
            let exit_px = match k {
                0..=3 => 95.0,
                4 => 94.0,
                _ => 100.0,
            };
            a_bars.push((d0 + k * day, 100.0)); // closes 01:00 = entry
            a_bars.push((d0 + k * day + 2 * H, exit_px)); // closes 03:00 = exit
            b_bars.push((d0 + k * day, 100.0));
            b_bars.push((d0 + k * day + 2 * H, if k == 4 { 106.0 } else { 100.0 }));
        }
        let md = market(vec![
            sparse(A, Interval::H1, &a_bars),
            sparse(B, Interval::H1, &b_bars),
        ]);
        let p = run_params(d0, d0 + 7 * day);
        let c = RiskCaps {
            initial_cash_usd: 100.0,
            max_order_notional_usd: 100.0,
            max_gross_exposure_usd: 300.0,
            max_net_exposure_usd: 300.0,
            daily_loss_limit_usd: 1_000.0,
            total_loss_limit_usd: 25.0,
        };
        let mut cs: Vec<Candidate> = (0..5)
            .map(|k| {
                decision(
                    k as usize,
                    A,
                    Side::Buy,
                    d0 + k * day + H,
                    100.0,
                    d0 + k * day + 3 * H,
                )
            })
            .collect();
        // Day 4: B (+6 %) exits with A (−6 %); A's seq is lower: realized first.
        cs.push(decision(
            5,
            B,
            Side::Buy,
            d0 + 4 * day + H,
            100.0,
            d0 + 4 * day + 3 * H,
        ));
        cs.push(decision(
            6,
            A,
            Side::Buy,
            d0 + 5 * day + H,
            100.0,
            d0 + 5 * day + 3 * H,
        ));
        let r = simulate(&flat_spec(), &md, &p, &cs, Arm::Capped(c));
        assert!(r.refusals.is_empty(), "{:?}", r.refusals);
        assert_eq!(r.trades.len(), 7);
        let total: f64 = r.trades.iter().map(|t| t.net_usd).sum();
        assert!(total > -25.0 && total < -15.0, "{total}");
    }

    /// Regression (review: Sharpe annualised by √365 over active days only):
    /// a year-long decision range with trades on 10 days. The Sharpe of the
    /// per-period USD is annualised by the observed rate of periods with
    /// trades (10 a year), not by 365 calendar days a year.
    #[test]
    fn sharpe_is_annualised_by_the_rate_of_periods_with_trades() {
        let t0 = utc("2026-01-05 00:00");
        let year_ms: i64 = 31_557_600_000; // 365.25 days
        let rets = [1.0, -0.5, 2.0, 0.5, -1.0, 1.5, 0.25, 1.0, -0.25, 0.75];
        let mut bars = Vec::new();
        let mut cs = Vec::new();
        for (k, r) in rets.iter().enumerate() {
            let entry = t0 + k as i64 * 30 * 24 * H + H;
            bars.push((entry - H, 100.0));
            bars.push((entry + H, 100.0 * (1.0 + r / 100.0)));
            cs.push(decision(k, A, Side::Buy, entry, 100.0, entry + 2 * H));
        }
        let md = market(vec![sparse(A, Interval::H1, &bars)]);
        let p = run_params(t0, t0 + year_ms);
        let r = simulate(&flat_spec(), &md, &p, &cs, Arm::Research);
        assert_eq!((r.summary.n, r.summary.n_periods), (10, 10));
        let usd: Vec<f64> = r.trades.iter().map(|t| t.net_usd).collect();
        let m = usd.iter().sum::<f64>() / 10.0;
        let sd = (usd.iter().map(|x| (x - m).powi(2)).sum::<f64>() / 9.0).sqrt();
        let want = m / sd * 10f64.sqrt();
        let got = r.summary.sharpe.unwrap();
        assert!((got - want).abs() < 1e-9, "sharpe {got} vs {want}");
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
