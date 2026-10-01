//! Weekend-fade rule W (`x-weekend-fade-strategy`; fixed before the data in
//! `docs/xmarket-feasibility-2026-09-30.md`): at the entry instant fade every
//! name's move since the anchor, exit at 09:00 on the next trading day. Pure —
//! prices, times, ledgers and the calendar are inputs; the exec tool
//! `xm_weekend_fade` (`adapters/outbound/tools/xm/weekend_fade.rs`) reads
//! them, places the orders and returns [`WeekendFade`]
//! (`xm_weekend/1:<anchor date>`). Knobs: `[xmarket.weekend_fade]`
//! (`config/xmarket.rs`). Ids in full everywhere.
//!
//! | Piece | Rule |
//! |---|---|
//! | Window ([`fade_window`]) | an `ExchangeCalendar::weekend_window` break that holds a Saturday and a Sunday (a single mid-week holiday is not one): anchor 20:00 on the last trading day before it (normally Fri), entry 18:00 on its last non-trading day (Sun; Mon for a Monday holiday), exit 09:00 on the next trading day — `America/New_York` wall clock, DST-safe |
//! | Price | live: a `mkt_ctx/1` row's `mid`, else `mark` ([`price_point`]) — the anchor from the history as of the anchor (≤ `anchor_max_age_secs` old), the entry from the store (≤ `entry_max_age_secs` old at the call); replay: the close of the 5 m candle ending at the instant ([`close_at`], `t` = instant − 300 000) |
//! | Signal ([`signal_of`]) | s = ln(P_entry / P_anchor), bps; fade = −sign(s): sell after a rise, buy after a fall |
//! | Eligible | not in `exclude`, both prices present, fresh and > 0, s ≠ 0 (s = 0 ⇒ `flat`, no order) |
//! | Complete snapshot ([`WeekendFade::complete`]) | kept only when every name not excluded and with an anchor has an entry row fresher than `entry_max_age_secs` (a missing or older one: `stale`); taken before [`snapshot_deadline_ms`] = entry + [`ENTRY_WAIT_MS`] (≤ half the lateness) with a `stale` name, the call keeps nothing (an `error` row) and the next one reads again; from the deadline on the `stale` names are left out for the window. A fresh row without a usable price is `missing_entry` and waits for nothing |
//! | Shadow ledger | every eligible name, `shadow_notional_usd` each, the shadow gate (`risk::evaluate_shadow`), no caps |
//! | Capped ledger ([`select_capped`]) | the `capped_top_n` largest \|s\| with \|s\| ≥ `min_abs_signal_bps`, ties by full id; `capped_notional_usd` each through the `[risk]` gate |
//! | Ids | anchor date = the last trading day (local `YYYY-MM-DD`); capped `fade:<account>:<full id>:<anchor date>`, shadow `fade-shadow:<shadow account>:<full id>:<anchor date>` ([`capped_order_id`], [`shadow_order_id`]); attempt n ≥ 2 of either `<id>:<n>` ([`fade_attempt_id`], the exits' numbering) |
//! | Attempts | the first keeps the id; a stored rejection the next attempt may not meet (`FillReason::is_transient`: missing data, book age, liquidity, a price bound, a venue state) is placed again as the next attempt within `entry_lateness_max_secs`; a fill or a partial fill is never placed again; a final rejection (size, lot and tick rules, delisting, a bad order) is the outcome; a gate denial stores nothing and is judged again under the same id |
//! | P&L ([`WeekendFade::refresh`]) | per name and ledger, once flat after the exit: (realized − fees − funding) now minus the same before the entry, USD, and bps of the filled entry notional |
//! | Replay ([`replay`]) | per eligible name with an exit price: gross = dir × ln(P_exit / P_entry) bps, net = gross − the round-trip cost; the mean over names in id order, positives, the capped set |
//!
//! | Row | Holds |
//! |---|---|
//! | `xm_weekend/1:<anchor date>` ([`WeekendFade`]) | the window's phase `waiting` → `entered` (or `missed_entry`) → `closing` → `closed`; from the entry on, the snapshot: per name both prices, s, the fade side, capped or not, ledger bases, the orders, open quantities, P&L — the tool writes it before any order and never recomputes it |
//! | `xm_weekend_signal/1:<anchor date>:<full id>` ([`FadeSignal`]) | one eligible name's signal — the opportunity row a capped fade names: `edge_after_costs_bps` = `expected_edge_bps` |

use std::collections::{BTreeMap, BTreeSet};

use chrono::{Datelike, Weekday};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::book::Side;
use crate::domain::calendar::{ExchangeCalendar, WeekendWindow};
use crate::domain::observation::{
    set_bool, set_int, set_num, set_str, ErrorClass, Features, Field, ObsStatus, Observed,
    ReadError, MAX_LINE1_CHARS,
};
use crate::domain::xm::ledger::{PaperAccount, Position};
use crate::domain::xm::risk::EDGE_FEATURE;

/// One replay candle: the price at an instant is the close of the 5 m
/// candle ending there.
pub const CANDLE_MS: i64 = 300_000;
/// The §21 opportunity type every fade order carries (`strategy`): a
/// weekend overshoot is an overreaction.
pub const FADE_STRATEGY: &str = "overreaction";
const DAY_MS: i64 = 86_400_000;
/// How long after the entry instant the snapshot waits for every name's
/// fresh entry row before it is kept without the stale ones (at most half
/// of `entry_lateness_max_secs`).
pub const ENTRY_WAIT_MS: i64 = 120_000;
/// Breaks [`fade_window`] skips (single mid-week holidays) before giving up.
const MAX_SKIPPED_BREAKS: usize = 8;
/// How far back [`previous_fade_window`] looks.
const LOOKBACK_MS: i64 = 31 * DAY_MS;

/// The rule's selection knobs (`[xmarket.weekend_fade]`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FadeRule {
    /// Capped ledger: this many names …
    pub capped_top_n: usize,
    /// … with at least this \|s\|, bps.
    pub min_abs_signal_bps: f64,
}

// ── Window ──────────────────────────────────────────────────────────

/// The break holds a Saturday and a Sunday (module table).
pub fn is_weekend_break(w: &WeekendWindow) -> bool {
    let (mut sat, mut sun) = (false, false);
    let mut d = w.last_trading_day;
    while let Some(next) = d.succ_opt().filter(|n| *n < w.next_trading_day) {
        match next.weekday() {
            Weekday::Sat => sat = true,
            Weekday::Sun => sun = true,
            _ => {}
        }
        d = next;
    }
    sat && sun
}

/// The fade window still running at `now_ms` (exit > now), else the next
/// one; `None` when the calendar reaches none.
pub fn fade_window(cal: &ExchangeCalendar, now_ms: i64) -> Option<WeekendWindow> {
    let mut t = now_ms;
    for _ in 0..=MAX_SKIPPED_BREAKS {
        let w = cal.weekend_window(t)?;
        if is_weekend_break(&w) {
            return Some(w);
        }
        t = w.exit_ms;
    }
    None
}

/// The latest fade window whose exit is at or before `now_ms` (the one a
/// call after its exit closes), within the last 31 days.
pub fn previous_fade_window(cal: &ExchangeCalendar, now_ms: i64) -> Option<WeekendWindow> {
    let mut t = now_ms.saturating_sub(LOOKBACK_MS);
    let mut last = None;
    while let Some(w) = fade_window(cal, t) {
        if w.exit_ms > now_ms {
            break;
        }
        t = w.exit_ms;
        last = Some(w);
    }
    last
}

/// The window's key date: its last trading day, local `YYYY-MM-DD`.
pub fn anchor_date(w: &WeekendWindow) -> String {
    w.last_trading_day.format("%Y-%m-%d").to_string()
}

/// `fade:<account>:<full id>:<anchor date>`.
pub fn capped_order_id(account: &str, instrument: &str, anchor_date: &str) -> String {
    format!("fade:{account}:{instrument}:{anchor_date}")
}

/// `fade-shadow:<shadow account>:<full id>:<anchor date>`.
pub fn shadow_order_id(shadow_account: &str, instrument: &str, anchor_date: &str) -> String {
    format!("fade-shadow:{shadow_account}:{instrument}:{anchor_date}")
}

/// Attempt `attempt` of fade `id`: the id itself for the first, else
/// `<id>:<attempt>` (2, 3, …) — `exits::exit_client_order_id`'s numbering.
/// An anchor date never ends in `:<n>`, so no attempt is another name's id.
pub fn fade_attempt_id(id: &str, attempt: u32) -> String {
    if attempt <= 1 {
        id.to_string()
    } else {
        format!("{id}:{attempt}")
    }
}

/// Until when the snapshot waits for every fresh entry row (module table):
/// entry + [`ENTRY_WAIT_MS`], at most half the lateness.
pub fn snapshot_deadline_ms(w: &WeekendWindow, lateness_ms: u64) -> i64 {
    let half = i64::try_from(lateness_ms / 2).unwrap_or(i64::MAX);
    w.entry_ms.saturating_add(ENTRY_WAIT_MS.min(half))
}

// ── Prices ──────────────────────────────────────────────────────────

/// Where a price came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PriceSource {
    /// `mkt_ctx/1` `mid`.
    Mid,
    /// `mkt_ctx/1` `mark` (no mid).
    Mark,
    /// A replay candle's close.
    Close,
}

/// A price and when it was observed.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PricePoint {
    pub px: f64,
    pub at_ms: i64,
    pub source: PriceSource,
}

/// A stored (`observations.db`) or recorded (history) `mkt_ctx/1` row as
/// the rule reads it.
#[derive(Debug, Clone, Copy)]
pub struct CtxRow<'a> {
    pub status: ObsStatus,
    pub observed_at_ms: i64,
    pub features: &'a Features,
    /// The row's first error, when it has one.
    pub error: Option<&'a ReadError>,
}

fn valid_px(px: f64) -> bool {
    px.is_finite() && px > 0.0
}

/// A `mkt_ctx/1` row's price: its `mid`, else its `mark` (finite, > 0).
pub fn ctx_price(features: &Features) -> Option<(f64, PriceSource)> {
    [("mid", PriceSource::Mid), ("mark", PriceSource::Mark)]
        .into_iter()
        .find_map(|(key, source)| {
            features
                .get(key)
                .and_then(Value::as_f64)
                .filter(|px| valid_px(*px))
                .map(|px| (px, source))
        })
}

/// The price `row` gives `instrument` at `t_ms`: `Error` on field
/// `<what>:<instrument>` — class `missing` — when there is no row, and when
/// the row failed, has no price or is older than `max_age_ms`. Never 0.
pub fn price_point(
    what: &str,
    instrument: &str,
    row: Option<CtxRow<'_>>,
    t_ms: i64,
    max_age_ms: u64,
    missing: ErrorClass,
) -> Field<PricePoint> {
    let field = format!("{what}:{instrument}");
    let Some(r) = row else {
        return Field::err(ReadError::new(
            field,
            missing,
            format!("no mkt_ctx/1 row within {max_age_ms} ms of the {what}"),
        ));
    };
    if r.status == ObsStatus::Error {
        let why = r.error.map_or("row status error".to_string(), |e| {
            format!("{} {}", e.class.as_str(), e.message)
        });
        return Field::err(ReadError::new(field, missing, why));
    }
    let age = t_ms.saturating_sub(r.observed_at_ms).max(0) as u64;
    if age > max_age_ms {
        return Field::err(ReadError::new(
            field,
            missing,
            format!("stale: mkt_ctx/1 row {age} ms old > {max_age_ms} ms"),
        ));
    }
    match ctx_price(r.features) {
        Some((px, source)) => Field::ok(PricePoint {
            px,
            at_ms: r.observed_at_ms,
            source,
        }),
        None => Field::err(ReadError::new(
            field,
            ErrorClass::Decode,
            "the mkt_ctx/1 row has no mid or mark > 0",
        )),
    }
}

// ── Signal ──────────────────────────────────────────────────────────

/// Why a name gets no fade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Skip {
    /// In `exclude` (a split, a halt).
    Excluded,
    /// No fresh anchor price.
    MissingAnchor,
    /// No fresh entry price.
    MissingEntry,
    /// No entry row fresher than `entry_max_age_secs` (none, or too old —
    /// a restart): the snapshot waits for it until [`snapshot_deadline_ms`]
    /// ([`WeekendFade::complete`]), then the name is left out of the window.
    Stale,
    /// Replay only: no exit price.
    MissingExit,
    /// s = 0: nothing to fade.
    Flat,
}

/// One eligible name at the entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Signal {
    pub instrument: String,
    pub anchor_px: f64,
    pub entry_px: f64,
    /// ln(entry / anchor) × 10 000.
    pub s_bps: f64,
    /// The fade: sell after a rise, buy after a fall.
    pub side: Side,
}

/// s = ln(`entry_px` / `anchor_px`) in bps; `None` unless both are finite
/// and > 0.
pub fn signal_bps(anchor_px: f64, entry_px: f64) -> Option<f64> {
    (valid_px(anchor_px) && valid_px(entry_px)).then(|| (entry_px / anchor_px).ln() * 10_000.0)
}

/// −sign(s) as an order side; `None` for s = 0 (or not a number).
pub fn fade_side(s_bps: f64) -> Option<Side> {
    if s_bps > 0.0 {
        Some(Side::Sell)
    } else if s_bps < 0.0 {
        Some(Side::Buy)
    } else {
        None
    }
}

/// The module table's eligibility, first reason wins: excluded, no anchor,
/// no entry, flat.
pub fn signal_of(
    instrument: &str,
    excluded: bool,
    anchor_px: Option<f64>,
    entry_px: Option<f64>,
) -> Result<Signal, Skip> {
    if excluded {
        return Err(Skip::Excluded);
    }
    let anchor_px = anchor_px
        .filter(|p| valid_px(*p))
        .ok_or(Skip::MissingAnchor)?;
    let entry_px = entry_px
        .filter(|p| valid_px(*p))
        .ok_or(Skip::MissingEntry)?;
    let s_bps = signal_bps(anchor_px, entry_px).ok_or(Skip::MissingEntry)?;
    let side = fade_side(s_bps).ok_or(Skip::Flat)?;
    Ok(Signal {
        instrument: instrument.to_string(),
        anchor_px,
        entry_px,
        s_bps,
        side,
    })
}

/// The capped ledger's names: the `capped_top_n` largest \|s\| with \|s\|
/// ≥ `min_abs_signal_bps`, largest first, ties by full id.
pub fn select_capped(signals: &[Signal], rule: &FadeRule) -> Vec<String> {
    let mut picks: Vec<&Signal> = signals
        .iter()
        .filter(|s| s.s_bps.abs() >= rule.min_abs_signal_bps)
        .collect();
    picks.sort_by(|a, b| {
        b.s_bps
            .abs()
            .total_cmp(&a.s_bps.abs())
            .then_with(|| a.instrument.cmp(&b.instrument))
    });
    picks
        .into_iter()
        .take(rule.capped_top_n)
        .map(|s| s.instrument.clone())
        .collect()
}

// ── Replay ──────────────────────────────────────────────────────────

/// One 5 m candle (HL `candleSnapshot`: `t` open time, `c` close, `n` trades).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Candle {
    pub t_ms: i64,
    pub close: f64,
    pub trades: u64,
}

/// The close of the candle ending at `instant_ms` (`t` = instant − 5 min);
/// `None` when that candle is missing or its close is not > 0.
pub fn close_at(candles: &[Candle], instant_ms: i64) -> Option<f64> {
    candles
        .iter()
        .find(|c| c.t_ms == instant_ms - CANDLE_MS)
        .map(|c| c.close)
        .filter(|px| valid_px(*px))
}

/// One traded name of a replay.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayRow {
    pub instrument: String,
    pub anchor_px: f64,
    pub entry_px: f64,
    pub exit_px: f64,
    pub s_bps: f64,
    /// −sign(s): −1 short, +1 long.
    pub dir: i8,
    /// dir × ln(exit / entry) × 10 000.
    pub gross_bps: f64,
    /// gross − the round-trip cost.
    pub net_bps: f64,
}

/// A replay of one window (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Replay {
    /// Traded names, by full id.
    pub rows: Vec<ReplayRow>,
    /// Names without a trade, and why.
    pub skipped: BTreeMap<String, Skip>,
    /// The capped ledger's names ([`select_capped`] over the entry's
    /// eligible names).
    pub capped: Vec<String>,
    /// Mean `net_bps` over `rows` (summed in id order); `None` without rows.
    pub mean_net_bps: Option<f64>,
    /// Rows with `net_bps` > 0.
    pub positive: usize,
}

/// Replay rule W over 5 m candles by full id (module table). The replay is
/// the offline check of `x-weekend-sandbox` (the golden in
/// `tests/fixtures/xmarket/`); `tengu xm replay` (M7) builds on it.
#[cfg_attr(not(test), allow(dead_code))]
pub fn replay(
    candles: &BTreeMap<String, Vec<Candle>>,
    window: &WeekendWindow,
    exclude: &BTreeSet<String>,
    cost_rt_bps: f64,
    rule: &FadeRule,
) -> Replay {
    let mut rows = Vec::new();
    let mut skipped = BTreeMap::new();
    let mut signals = Vec::new();
    for (id, cs) in candles {
        let signal = signal_of(
            id,
            exclude.contains(id),
            close_at(cs, window.anchor_ms),
            close_at(cs, window.entry_ms),
        );
        let s = match signal {
            Ok(s) => s,
            Err(skip) => {
                skipped.insert(id.clone(), skip);
                continue;
            }
        };
        signals.push(s.clone());
        let Some(exit_px) = close_at(cs, window.exit_ms) else {
            skipped.insert(id.clone(), Skip::MissingExit);
            continue;
        };
        let dir: f64 = -s.s_bps.signum();
        let gross_bps = dir * (exit_px / s.entry_px).ln() * 10_000.0;
        rows.push(ReplayRow {
            instrument: s.instrument,
            anchor_px: s.anchor_px,
            entry_px: s.entry_px,
            exit_px,
            s_bps: s.s_bps,
            dir: dir as i8,
            gross_bps,
            net_bps: gross_bps - cost_rt_bps,
        });
    }
    let mean_net_bps =
        (!rows.is_empty()).then(|| rows.iter().map(|r| r.net_bps).sum::<f64>() / rows.len() as f64);
    let positive = rows.iter().filter(|r| r.net_bps > 0.0).count();
    Replay {
        capped: select_capped(&signals, rule),
        rows,
        skipped,
        mean_net_bps,
        positive,
    }
}

// ── Rows ────────────────────────────────────────────────────────────

/// `xm_weekend_signal/1:<anchor date>:<full id>` — one eligible name's
/// signal, written at the entry before any order: the opportunity row its
/// fades name (`edge_after_costs_bps` = `[xmarket.weekend_fade]
/// expected_edge_bps`, for the `[risk]` gate's `min_edge`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FadeSignal {
    pub anchor_date: String,
    pub instrument: String,
    pub anchor_px: f64,
    pub entry_px: f64,
    pub s_bps: f64,
    pub side: Side,
    /// In the capped ledger.
    pub capped: bool,
    pub edge_after_costs_bps: f64,
}

impl Observed for FadeSignal {
    const SCHEMA: &'static str = "xm_weekend_signal/1";

    fn subject(&self) -> String {
        format!("{}:{}", self.anchor_date, self.instrument)
    }

    fn headline(&self) -> String {
        format!(
            "xm_weekend_signal {} {} s_bps={:+.2} fade={}{} edge_after_costs_bps={}",
            self.anchor_date,
            self.instrument,
            self.s_bps,
            self.side.as_str(),
            if self.capped { " capped" } else { "" },
            self.edge_after_costs_bps
        )
    }

    fn features(&self) -> Features {
        let mut f = Features::new();
        set_num(&mut f, "s_bps", Some(self.s_bps));
        set_num(&mut f, "abs_s_bps", Some(self.s_bps.abs()));
        set_str(&mut f, "side", Some(self.side.as_str()));
        set_num(&mut f, "anchor_px", Some(self.anchor_px));
        set_num(&mut f, "entry_px", Some(self.entry_px));
        set_bool(&mut f, "capped", Some(self.capped));
        set_num(&mut f, EDGE_FEATURE, Some(self.edge_after_costs_bps));
        f
    }
}

/// A window's phase (module table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FadePhase {
    /// Before the entry.
    Waiting,
    /// The snapshot is taken; orders placed (re-placed within the lateness).
    Entered,
    /// No call reached the entry within `entry_lateness_max_secs`: no late
    /// entry.
    MissedEntry,
    /// After the exit, some filled name still open in a ledger.
    Closing,
    /// After the exit, every filled name flat: the P&L is final.
    Closed,
}

impl FadePhase {
    pub fn as_str(self) -> &'static str {
        match self {
            FadePhase::Waiting => "waiting",
            FadePhase::Entered => "entered",
            FadePhase::MissedEntry => "missed_entry",
            FadePhase::Closing => "closing",
            FadePhase::Closed => "closed",
        }
    }

    /// The row holds the entry snapshot.
    pub fn has_snapshot(self) -> bool {
        matches!(
            self,
            FadePhase::Entered | FadePhase::Closing | FadePhase::Closed
        )
    }
}

/// One order a fade placed — its latest attempt, from its `paper_fill/1`
/// row or the ledger — or why it was not placed; ids in full.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FadeOrder {
    pub instrument: String,
    /// The attempt's id ([`fade_attempt_id`]).
    pub client_order_id: String,
    /// `filled` · `partial` · `rejected` (venue) · `denied` (gate) · `error`
    /// (not placed: `position_open`, a refusal) · `flat` (a close found
    /// nothing open).
    pub status: String,
    /// The gate's rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filled_qty: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avg_px: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notional_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fee_usd: Option<f64>,
    /// Why it failed (the venue's reason, the refusal).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl FadeOrder {
    /// Something filled (`filled`, `partial`): the fade is never placed
    /// again.
    pub fn filled(&self) -> bool {
        self.filled_qty.is_some_and(|q| q > 0.0)
    }

    /// Placed but not (fully) filled, or not placed (a close that found
    /// the position flat did its job).
    pub fn failed(&self) -> bool {
        !matches!(self.status.as_str(), "filled" | "flat")
    }

    fn why(&self) -> String {
        let why = self
            .error
            .clone()
            .or_else(|| self.rule.clone())
            .unwrap_or_default();
        format!("{} {} {why}", self.instrument, self.status)
    }
}

/// One universe name in a window (a `data` row of [`WeekendFade`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FadeName {
    pub instrument: String,
    pub anchor: Field<PricePoint>,
    pub entry: Field<PricePoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub s_bps: Option<f64>,
    /// The fade's side; `None` without a fade.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub side: Option<Side>,
    /// Why no fade; `None` = eligible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip: Option<Skip>,
    /// In the capped ledger.
    #[serde(default)]
    pub capped: bool,
    /// (realized − fees − funding) of the name's position in each ledger
    /// before the entry, USD (0 = never traded).
    #[serde(default)]
    pub shadow_base_usd: f64,
    #[serde(default)]
    pub capped_base_usd: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shadow: Option<FadeOrder>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capped_order: Option<FadeOrder>,
    /// Signed open quantity at the last refresh (filled names only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shadow_qty: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capped_qty: Option<f64>,
    /// Once flat after the exit: net P&L, USD and bps of the entry notional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shadow_pnl_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shadow_pnl_bps: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capped_pnl_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capped_pnl_bps: Option<f64>,
}

impl FadeName {
    /// The name as the entry sees it: its prices, then [`signal_of`].
    pub fn at_entry(
        instrument: &str,
        excluded: bool,
        anchor: Field<PricePoint>,
        entry: Field<PricePoint>,
    ) -> Self {
        let signal = signal_of(
            instrument,
            excluded,
            anchor.value().map(|p| p.px),
            entry.value().map(|p| p.px),
        );
        let (s_bps, side, skip) = match &signal {
            Ok(s) => (Some(s.s_bps), Some(s.side), None),
            Err(Skip::Flat) => (Some(0.0), None, Some(Skip::Flat)),
            Err(skip) => (None, None, Some(*skip)),
        };
        Self {
            instrument: instrument.to_string(),
            anchor,
            entry,
            s_bps,
            side,
            skip,
            capped: false,
            shadow_base_usd: 0.0,
            capped_base_usd: 0.0,
            shadow: None,
            capped_order: None,
            shadow_qty: None,
            capped_qty: None,
            shadow_pnl_usd: None,
            shadow_pnl_bps: None,
            capped_pnl_usd: None,
            capped_pnl_bps: None,
        }
    }

    /// The eligible name's signal.
    pub fn signal(&self) -> Option<Signal> {
        Some(Signal {
            instrument: self.instrument.clone(),
            anchor_px: self.anchor.value()?.px,
            entry_px: self.entry.value()?.px,
            s_bps: self.s_bps?,
            side: self.side?,
        })
    }
}

/// (realized − fees − funding) of a position so far, USD; 0 for a name
/// never traded.
pub fn position_net_usd(position: Option<&Position>) -> f64 {
    position.map_or(0.0, |p| p.realized_pnl - p.fees_paid - p.funding_paid)
}

/// `xm_weekend/1:<anchor date>` — one window of the weekend fade (module
/// tables). TTL > 0: the tool keeps its snapshot here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WeekendFade {
    pub anchor_date: String,
    pub phase: FadePhase,
    pub window: WeekendWindow,
    /// The capped ledger (`[risk] account`).
    pub account: String,
    pub shadow_account: String,
    pub n_universe: usize,
    pub n_excluded: usize,
    /// When the snapshot was taken.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entered_at_ms: Option<i64>,
    /// The snapshot, one per universe name, in universe order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub names: Vec<FadeName>,
    /// Shadow closes this call placed (positions past their deadline).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shadow_exits: Vec<FadeOrder>,
    /// `missed_entry`: seconds after the entry instant of the first call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub late_s: Option<i64>,
    pub lateness_max_s: u64,
    pub ts_ms: i64,
}

impl WeekendFade {
    /// A row of `window` in `phase` without a snapshot.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        phase: FadePhase,
        window: WeekendWindow,
        account: &str,
        shadow_account: &str,
        n_universe: usize,
        n_excluded: usize,
        lateness_max_s: u64,
        now_ms: i64,
    ) -> Self {
        Self {
            anchor_date: anchor_date(&window),
            phase,
            window,
            account: account.to_string(),
            shadow_account: shadow_account.to_string(),
            n_universe,
            n_excluded,
            entered_at_ms: None,
            names: Vec::new(),
            shadow_exits: Vec::new(),
            late_s: None,
            lateness_max_s,
            ts_ms: now_ms,
        }
    }

    fn count(&self, f: impl Fn(&FadeName) -> bool) -> usize {
        self.names.iter().filter(|n| f(n)).count()
    }

    fn skipped(&self, skip: Skip) -> usize {
        self.count(|n| n.skip == Some(skip))
    }

    pub fn n_eligible(&self) -> usize {
        self.count(|n| n.skip.is_none())
    }

    /// The snapshot may be kept: no name is `stale`, or it was taken at or
    /// after [`snapshot_deadline_ms`] (the stale names are then left out).
    pub fn complete(&self) -> bool {
        let deadline = snapshot_deadline_ms(&self.window, self.lateness_max_s.saturating_mul(1000));
        self.skipped(Skip::Stale) == 0 || self.entered_at_ms.is_some_and(|t| t >= deadline)
    }

    pub fn n_capped(&self) -> usize {
        self.count(|n| n.capped)
    }

    fn orders(&self, capped: bool) -> impl Iterator<Item = &FadeOrder> {
        self.names.iter().filter_map(move |n| {
            if capped {
                n.capped_order.as_ref()
            } else {
                n.shadow.as_ref()
            }
        })
    }

    fn n_filled(&self, capped: bool) -> usize {
        self.orders(capped).filter(|o| o.filled()).count()
    }

    fn n_failed(&self, capped: bool) -> usize {
        self.orders(capped).filter(|o| o.failed()).count()
    }

    fn n_open(&self, capped: bool) -> usize {
        self.count(|n| {
            let qty = if capped { n.capped_qty } else { n.shadow_qty };
            qty.is_some_and(|q| q != 0.0)
        })
    }

    /// Σ P&L (USD) and the mean bps over the names with one; `None` when no
    /// name has one.
    fn pnl(&self, capped: bool) -> Option<(f64, f64, usize)> {
        let rows: Vec<(f64, f64)> = self
            .names
            .iter()
            .filter_map(|n| {
                if capped {
                    n.capped_pnl_usd.zip(n.capped_pnl_bps)
                } else {
                    n.shadow_pnl_usd.zip(n.shadow_pnl_bps)
                }
            })
            .collect();
        (!rows.is_empty()).then(|| {
            let usd = rows.iter().map(|r| r.0).sum::<f64>();
            let bps = rows.iter().map(|r| r.1).sum::<f64>() / rows.len() as f64;
            (usd, bps, rows.len())
        })
    }

    fn next_entry_s(&self) -> i64 {
        (self.window.entry_ms - self.ts_ms).max(0) / 1000
    }

    /// Re-read both ledgers at `now_ms`: open quantities of the filled
    /// names; after the exit, each flat filled name's P&L, and the phase
    /// `closing` / `closed`.
    pub fn refresh(&mut self, shadow: &PaperAccount, capped: &PaperAccount, now_ms: i64) {
        self.ts_ms = now_ms;
        if !self.phase.has_snapshot() {
            return;
        }
        let after_exit = now_ms >= self.window.exit_ms;
        let mut open = false;
        for n in &mut self.names {
            for (ledger, is_capped) in [(shadow, false), (capped, true)] {
                let (order, base) = if is_capped {
                    (n.capped_order.as_ref(), n.capped_base_usd)
                } else {
                    (n.shadow.as_ref(), n.shadow_base_usd)
                };
                let Some(order) = order.filter(|o| o.filled()) else {
                    continue;
                };
                let position = ledger.positions.get(&n.instrument);
                let qty = position.map_or(0.0, |p| p.qty);
                let flat = position.is_none_or(Position::is_flat);
                let pnl = (after_exit && flat).then(|| {
                    let usd = position_net_usd(position) - base;
                    let bps = order
                        .notional_usd
                        .filter(|x| *x > 0.0)
                        .map(|notional| usd / notional * 10_000.0);
                    (usd, bps)
                });
                open |= !flat;
                let (q, usd, bps) = if is_capped {
                    (
                        &mut n.capped_qty,
                        &mut n.capped_pnl_usd,
                        &mut n.capped_pnl_bps,
                    )
                } else {
                    (
                        &mut n.shadow_qty,
                        &mut n.shadow_pnl_usd,
                        &mut n.shadow_pnl_bps,
                    )
                };
                *q = Some(if flat { 0.0 } else { qty });
                *usd = pnl.map(|p| p.0);
                *bps = pnl.and_then(|p| p.1);
            }
        }
        if after_exit {
            self.phase = if open {
                FadePhase::Closing
            } else {
                FadePhase::Closed
            };
        }
    }

    fn ids_of(&self, f: impl Fn(&FadeName) -> bool) -> Vec<&str> {
        self.names
            .iter()
            .filter(|n| f(n))
            .map(|n| n.instrument.as_str())
            .collect()
    }
}

impl Observed for WeekendFade {
    const SCHEMA: &'static str = "xm_weekend/1";

    fn subject(&self) -> String {
        self.anchor_date.clone()
    }

    /// Counts per phase; the capped names when they fit in line 1 (ids
    /// whole, else left to `data`).
    fn headline(&self) -> String {
        let base = format!("xm_weekend {} {}", self.anchor_date, self.phase.as_str());
        match self.phase {
            FadePhase::Waiting => format!(
                "{base} next_entry_s={} universe={} excluded={}",
                self.next_entry_s(),
                self.n_universe,
                self.n_excluded
            ),
            FadePhase::MissedEntry => format!(
                "{base} late_s={} lateness_max_s={} universe={}",
                self.late_s.unwrap_or_default(),
                self.lateness_max_s,
                self.n_universe
            ),
            FadePhase::Entered => {
                let mut h = format!(
                    "{base} eligible={}/{} shadow={} capped={}/{} missing_anchor={} missing_entry={}",
                    self.n_eligible(),
                    self.names.len(),
                    self.n_filled(false),
                    self.n_filled(true),
                    self.n_capped(),
                    self.skipped(Skip::MissingAnchor),
                    self.skipped(Skip::MissingEntry)
                );
                let stale = self.skipped(Skip::Stale);
                if stale > 0 {
                    h.push_str(&format!(" stale={stale}"));
                }
                let capped: String = self
                    .ids_of(|n| n.capped)
                    .iter()
                    .map(|id| format!(" {id}"))
                    .collect();
                let with = format!("{h} capped:{capped}");
                if !capped.is_empty() && with.chars().count() <= MAX_LINE1_CHARS {
                    with
                } else {
                    h
                }
            }
            FadePhase::Closing => format!(
                "{base} shadow_open={} capped_open={} shadow_exits={}",
                self.n_open(false),
                self.n_open(true),
                self.shadow_exits.len()
            ),
            FadePhase::Closed => {
                let part = |label: &str, pnl: Option<(f64, f64, usize)>| match pnl {
                    Some((usd, bps, n)) => {
                        format!(" {label}_pnl_usd={usd:+.2} {label}_mean_net_bps={bps:+.1} {label}_names={n}")
                    }
                    None => format!(" {label}=none_filled"),
                };
                format!(
                    "{base}{}{}",
                    part("shadow", self.pnl(false)),
                    part("capped", self.pnl(true))
                )
            }
        }
    }

    fn features(&self) -> Features {
        let mut f = Features::new();
        let w = &self.window;
        set_str(&mut f, "phase", Some(self.phase.as_str()));
        set_int(&mut f, "closed_days", Some(i64::from(w.closed_days)));
        set_int(&mut f, "anchor_ms", Some(w.anchor_ms));
        set_int(&mut f, "entry_ms", Some(w.entry_ms));
        set_int(&mut f, "exit_ms", Some(w.exit_ms));
        if self.phase == FadePhase::Waiting {
            set_int(&mut f, "next_entry_s", Some(self.next_entry_s()));
        }
        set_int(&mut f, "n_universe", Some(self.n_universe as i64));
        set_int(&mut f, "n_excluded", Some(self.n_excluded as i64));
        set_int(&mut f, "late_s", self.late_s);
        if self.phase.has_snapshot() {
            for (key, n) in [
                ("n_eligible", self.n_eligible()),
                ("n_missing_anchor", self.skipped(Skip::MissingAnchor)),
                ("n_missing_entry", self.skipped(Skip::MissingEntry)),
                ("n_stale", self.skipped(Skip::Stale)),
                ("n_flat", self.skipped(Skip::Flat)),
                ("n_capped", self.n_capped()),
                ("n_capped_filled", self.n_filled(true)),
                ("n_capped_failed", self.n_failed(true)),
                ("n_shadow_filled", self.n_filled(false)),
                ("n_shadow_failed", self.n_failed(false)),
                ("n_capped_open", self.n_open(true)),
                ("n_shadow_open", self.n_open(false)),
            ] {
                set_int(&mut f, key, Some(n as i64));
            }
        }
        if !self.shadow_exits.is_empty() {
            set_int(
                &mut f,
                "n_shadow_exits",
                Some(self.shadow_exits.len() as i64),
            );
            let failed = self.shadow_exits.iter().filter(|o| o.failed()).count();
            set_int(&mut f, "n_shadow_exits_failed", Some(failed as i64));
        }
        if self.phase == FadePhase::Closed {
            for (label, pnl) in [("shadow", self.pnl(false)), ("capped", self.pnl(true))] {
                if let Some((usd, bps, _)) = pnl {
                    set_num(&mut f, &format!("{label}_pnl_usd"), Some(usd));
                    set_num(&mut f, &format!("{label}_mean_net_bps"), Some(bps));
                }
            }
        }
        f
    }

    /// `error`: an entry with no eligible name, or not [`complete`] (no
    /// snapshot is kept: the store keeps no `error` row); `partial`: any
    /// error below; else `ok`.
    ///
    /// [`complete`]: WeekendFade::complete
    fn status(&self) -> ObsStatus {
        if self.phase == FadePhase::Entered && (self.n_eligible() == 0 || !self.complete()) {
            ObsStatus::Error
        } else if self.errors().is_empty() {
            ObsStatus::Ok
        } else {
            ObsStatus::Partial
        }
    }

    /// One error per kind, naming every id in full: a missed entry, names
    /// without an anchor / entry price, stale names, fades and shadow exits
    /// not filled.
    fn errors(&self) -> Vec<ReadError> {
        let mut out = Vec::new();
        if self.phase == FadePhase::MissedEntry {
            out.push(ReadError::new(
                "entry",
                ErrorClass::NotApplicable,
                format!(
                    "missed_entry: no entry snapshot within entry_lateness_max_secs {} of the \
                     entry instant (now {} s after it) — no call came in time, or no name had \
                     both prices; no late entry",
                    self.lateness_max_s,
                    self.late_s.unwrap_or_default()
                ),
            ));
        }
        let stale = if self.complete() {
            (
                ErrorClass::NotApplicable,
                "left out stale (no fresh entry row by the snapshot's deadline)",
            )
        } else {
            (
                ErrorClass::Transient,
                "without a fresh entry row yet (no snapshot kept until they have one, or the \
                 deadline)",
            )
        };
        for (skip, field, class, what) in [
            (
                Skip::MissingAnchor,
                "anchor",
                ErrorClass::NotApplicable,
                "without a recorded anchor price",
            ),
            (
                Skip::MissingEntry,
                "entry",
                ErrorClass::Transient,
                "without a fresh entry price",
            ),
            (Skip::Stale, "entry", stale.0, stale.1),
        ] {
            let ids = self.ids_of(|n| n.skip == Some(skip));
            if !ids.is_empty() {
                out.push(ReadError::new(
                    field,
                    class,
                    format!("{} names {what}: {}", ids.len(), ids.join(", ")),
                ));
            }
        }
        for (field, label, orders) in [
            (
                "capped",
                "capped fades",
                self.orders(true).collect::<Vec<_>>(),
            ),
            (
                "shadow",
                "shadow fades",
                self.orders(false).collect::<Vec<_>>(),
            ),
            (
                "shadow_exit",
                "shadow exits",
                self.shadow_exits.iter().collect::<Vec<_>>(),
            ),
        ] {
            let failed: Vec<String> = orders
                .iter()
                .filter(|o| o.failed())
                .map(|o| o.why())
                .collect();
            if !failed.is_empty() {
                out.push(ReadError::new(
                    field,
                    ErrorClass::NotApplicable,
                    format!(
                        "{} {label} not filled in full: {}",
                        failed.len(),
                        failed.join("; ")
                    ),
                ));
            }
        }
        out
    }
}

/// The replay golden of the 2026-09-26 → 09-28 weekend
/// (`tests/fixtures/xmarket/weekend_2026-09-26_*.json`, produced outside the
/// repo; provenance in `meta.json`): its candles and a bit-for-bit check of
/// a [`Replay`]. Shared by the rule's test below and the weekend sandbox's
/// (`config/xmarket.rs`).
#[cfg(test)]
pub(crate) mod golden {
    use std::collections::{BTreeMap, BTreeSet};

    use serde_json::Value;

    use super::{Candle, Replay, Skip};

    const CANDLES: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/xmarket/weekend_2026-09-26_candles.json"
    ));
    const GOLDEN: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/xmarket/weekend_2026-09-26_golden.json"
    ));

    fn golden() -> Value {
        serde_json::from_str(GOLDEN).unwrap()
    }

    /// The candles of every universe name, by full id (`hyperliquid:` +
    /// the fixture's coin).
    pub(crate) fn candles() -> BTreeMap<String, Vec<Candle>> {
        let raw: BTreeMap<String, Vec<Value>> = serde_json::from_str(CANDLES).unwrap();
        raw.into_iter()
            .map(|(coin, rows)| {
                let cs = rows
                    .iter()
                    .map(|c| {
                        assert_eq!(c["s"], coin.as_str());
                        Candle {
                            t_ms: c["t"].as_i64().unwrap(),
                            close: c["c"].as_str().unwrap().parse().unwrap(),
                            trades: c["n"].as_u64().unwrap(),
                        }
                    })
                    .collect();
                (format!("hyperliquid:{coin}"), cs)
            })
            .collect()
    }

    /// The golden's `excluded` names, full ids.
    pub(crate) fn excluded() -> BTreeSet<String> {
        golden()["excluded"]
            .as_object()
            .unwrap()
            .keys()
            .map(|coin| format!("hyperliquid:{coin}"))
            .collect()
    }

    /// The golden's round-trip cost, bps.
    pub(crate) fn cost_rt_bps() -> f64 {
        golden()["cost_rt_bps"].as_f64().unwrap()
    }

    /// The golden's `rows` and `summary` as written, `key → value text`
    /// (jq prints one key per line): `str::parse` rounds the text correctly,
    /// serde_json's default float parser can miss by an ulp.
    fn golden_text() -> (Vec<BTreeMap<String, String>>, BTreeMap<String, String>) {
        let (mut rows, mut summary) = (Vec::<BTreeMap<String, String>>::new(), BTreeMap::new());
        let mut section = "";
        for line in GOLDEN.lines() {
            let t = line.trim().trim_end_matches(',');
            if t.starts_with("\"rows\"") || t.starts_with("\"summary\"") {
                section = if t.starts_with("\"rows\"") {
                    "rows"
                } else {
                    "summary"
                };
                continue;
            }
            let Some((k, v)) = t.split_once(": ") else {
                continue;
            };
            let (k, v) = (
                k.trim_matches('"').to_string(),
                v.trim_matches('"').to_string(),
            );
            match section {
                "rows" if k == "coin" => rows.push(BTreeMap::from([(k, v)])),
                "rows" => {
                    rows.last_mut().unwrap().insert(k, v);
                }
                "summary" => {
                    summary.insert(k, v);
                }
                _ => {}
            }
        }
        (rows, summary)
    }

    /// `r` is the golden bit for bit: 74 rows (id, prices, s, dir, gross,
    /// net — the shortest text of each net is the golden's), mean net
    /// +95.4585 bps, 53 positive, the capped four, only the excluded names
    /// skipped.
    pub(crate) fn assert_replay(r: &Replay) {
        let golden = golden();
        let (want, summary) = golden_text();
        assert_eq!(want.len(), golden["rows"].as_array().unwrap().len());
        assert_eq!(r.rows.len(), want.len());
        assert_eq!(r.rows.len(), 74);
        for (got, want) in r.rows.iter().zip(&want) {
            let id = format!("hyperliquid:{}", want["coin"]);
            assert_eq!(got.instrument, id);
            let f = |k: &str| want[k].parse::<f64>().unwrap();
            assert_eq!(
                (got.anchor_px, got.entry_px, got.exit_px),
                (f("anchor"), f("entry"), f("exit")),
                "{id} prices"
            );
            assert_eq!(got.s_bps, f("s_bps"), "{id} s_bps");
            assert_eq!(f64::from(got.dir), f("dir"), "{id}");
            assert_eq!(got.gross_bps, f("gross_bps"), "{id} gross_bps");
            assert_eq!(got.net_bps, f("net_bps"), "{id} net_bps");
            // Bit for bit: the shortest text of each number is the golden's.
            assert_eq!(got.net_bps.to_string(), want["net_bps"], "{id}");
        }
        let s = &golden["summary"];
        assert_eq!(s["n"], 74);
        let mean: f64 = summary["mean_net_bps"].parse().unwrap();
        assert_eq!(r.mean_net_bps, Some(mean));
        assert_eq!(r.mean_net_bps, Some(95.45850028087516));
        assert_eq!(r.positive as u64, s["positive"].as_u64().unwrap());
        assert_eq!(r.positive, 53);
        let capped: Vec<String> = s["capped"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| format!("hyperliquid:{}", c.as_str().unwrap()))
            .collect();
        assert_eq!(r.capped, capped);
        assert_eq!(
            r.capped,
            [
                "hyperliquid:xyz:CRCL",
                "hyperliquid:xyz:SMSN",
                "hyperliquid:xyz:MINIMAX",
                "hyperliquid:xyz:MSTR"
            ]
        );
        let skipped: BTreeMap<String, Skip> = excluded()
            .into_iter()
            .map(|id| (id, Skip::Excluded))
            .collect();
        assert_eq!(r.skipped, skipped);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use chrono::{NaiveDate, NaiveDateTime};
    use serde_json::json;

    use super::*;
    use crate::domain::calendar::parse_date;
    use crate::domain::observation::{assert_features_ok, ObsSource, Observation};
    use crate::domain::tz::Zone;
    use crate::domain::xm::ledger::Fill;
    use crate::domain::xm::risk::key_names;

    const TSLA: &str = "hyperliquid:xyz:TSLA";
    const NVDA: &str = "hyperliquid:xyz:NVDA";

    fn utc(s: &str) -> i64 {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M")
            .unwrap()
            .and_utc()
            .timestamp_millis()
    }

    fn et(s: &str) -> i64 {
        Zone::NewYork.to_utc_ms(NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap())
    }

    fn date(s: &str) -> NaiveDate {
        parse_date(s).unwrap()
    }

    /// NYSE with the holidays these tests touch (the full list: the
    /// `tests/fixtures/xmarket/calendars.toml` row).
    fn nyse() -> ExchangeCalendar {
        let hm = |h: u32, m: u32| h * 60 + m;
        ExchangeCalendar {
            zone: Zone::NewYork,
            open: hm(9, 30),
            close: hm(16, 0),
            pre: Some(hm(4, 0)),
            post: Some(hm(20, 0)),
            overnight: true,
            early_close: Some(hm(13, 0)),
            early_post: Some(hm(17, 0)),
            holidays: ["2026-11-26", "2026-12-25", "2027-01-01", "2027-01-18"]
                .into_iter()
                .map(date)
                .collect(),
            early_closes: ["2026-11-27", "2026-12-24"].into_iter().map(date).collect(),
        }
    }

    const RULE: FadeRule = FadeRule {
        capped_top_n: 4,
        min_abs_signal_bps: 50.0,
    };

    /// The replay of the 2026-09-26 → 09-28 weekend reproduces the jq golden
    /// bit for bit: the window from the calendar, KIOXIA excluded (split
    /// halt), 74 rows, mean net +95.4585 bps, 53 positive, the capped four.
    #[test]
    fn golden_replay_2026_09_26() {
        let w = fade_window(&nyse(), et("2026-09-25 12:00")).unwrap();
        assert_eq!(
            (w.anchor_ms, w.entry_ms, w.exit_ms),
            (
                utc("2026-09-26 00:00"),
                utc("2026-09-27 22:00"),
                utc("2026-09-28 13:00")
            )
        );
        assert_eq!(anchor_date(&w), "2026-09-25");
        let exclude = golden::excluded();
        assert_eq!(
            exclude,
            BTreeSet::from(["hyperliquid:xyz:KIOXIA".to_string()])
        );
        assert_eq!(golden::cost_rt_bps(), 3.8);
        let candles = golden::candles();
        assert_eq!(candles.len(), 75);
        let r = replay(&candles, &w, &exclude, golden::cost_rt_bps(), &RULE);
        golden::assert_replay(&r);
        assert_eq!(
            r.skipped,
            BTreeMap::from([("hyperliquid:xyz:KIOXIA".to_string(), Skip::Excluded)])
        );
    }

    /// The window: the 2026-11-01 DST switch (anchor in EDT, entry and exit
    /// in EST), MLK 2027 (entry Mon 18:00, exit Tue 09:00), Thanksgiving (a
    /// mid-week holiday is skipped), and the previous window.
    #[test]
    fn windows_follow_dst_three_day_weekends_and_skip_mid_week_holidays() {
        let cal = nyse();
        let dst = fade_window(&cal, et("2026-10-30 12:00")).unwrap();
        assert_eq!(dst.last_trading_day, date("2026-10-30"));
        assert_eq!(dst.anchor_ms, utc("2026-10-31 00:00")); // Fri 20:00 EDT
        assert_eq!(dst.entry_ms, utc("2026-11-01 23:00")); // Sun 18:00 EST
        assert_eq!(dst.exit_ms, utc("2026-11-02 14:00")); // Mon 09:00 EST
        assert_eq!(anchor_date(&dst), "2026-10-30");

        let mlk = fade_window(&cal, et("2027-01-17 12:00")).unwrap();
        assert_eq!(mlk.closed_days, 3);
        assert_eq!(mlk.anchor_ms, utc("2027-01-16 01:00")); // Fri 20:00 EST
        assert_eq!(mlk.entry_ms, utc("2027-01-18 23:00")); // Mon 18:00 EST
        assert_eq!(mlk.exit_ms, utc("2027-01-19 14:00")); // Tue 09:00 EST
        assert!(is_weekend_break(&mlk));

        // Thanksgiving Thu 2026-11-26: a one-day break, not a fade window.
        let thu = cal.weekend_window(et("2026-11-25 10:00")).unwrap();
        assert_eq!(thu.closed_days, 1);
        assert!(!is_weekend_break(&thu));
        let w = fade_window(&cal, et("2026-11-25 10:00")).unwrap();
        assert_eq!(w.last_trading_day, date("2026-11-27"));
        assert_eq!(w.entry_ms, utc("2026-11-29 23:00")); // Sun 18:00 EST
        assert_eq!(w.exit_ms, utc("2026-11-30 14:00"));
        // Christmas Fri 2026-12-25: anchor Thu 20:00, entry Sun 18:00.
        let xmas = fade_window(&cal, et("2026-12-24 10:00")).unwrap();
        assert_eq!(
            (xmas.closed_days, xmas.anchor_ms, xmas.entry_ms),
            (3, utc("2026-12-25 01:00"), utc("2026-12-27 23:00"))
        );

        // The window runs until its exit; then the next one.
        let sep = fade_window(&cal, et("2026-09-28 08:59")).unwrap();
        assert_eq!(sep.exit_ms, utc("2026-09-28 13:00"));
        let next = fade_window(&cal, et("2026-09-28 09:00")).unwrap();
        assert_eq!(next.anchor_ms, utc("2026-10-03 00:00"));
        // The previous window: the latest exit at or before now.
        assert_eq!(
            previous_fade_window(&cal, et("2026-09-28 09:00")),
            Some(sep)
        );
        assert_eq!(
            previous_fade_window(&cal, et("2026-09-28 08:59")).map(|w| w.exit_ms),
            Some(utc("2026-09-21 13:00"))
        );
        // Across Thanksgiving: the Sat–Sun window, not the Thursday.
        let prev = previous_fade_window(&cal, et("2026-11-27 10:00")).unwrap();
        assert_eq!(prev.exit_ms, utc("2026-11-23 14:00"));
    }

    fn point(px: f64) -> Field<PricePoint> {
        Field::ok(PricePoint {
            px,
            at_ms: 0,
            source: PriceSource::Mid,
        })
    }

    fn missing(what: &str) -> Field<PricePoint> {
        Field::err(ReadError::new(what, ErrorClass::Transient, "absent"))
    }

    #[test]
    fn signals_eligibility_and_the_capped_selection() {
        let s = signal_of(TSLA, false, Some(100.0), Some(101.0)).unwrap();
        assert_eq!(s.side, Side::Sell, "a rise is faded short");
        assert_eq!(s.s_bps, (101.0f64 / 100.0).ln() * 10_000.0);
        let fall = signal_of(TSLA, false, Some(100.0), Some(99.0)).unwrap();
        assert_eq!(fall.side, Side::Buy);
        for (excluded, a, e, want) in [
            (true, Some(100.0), Some(101.0), Skip::Excluded),
            (false, None, Some(101.0), Skip::MissingAnchor),
            (false, Some(0.0), Some(101.0), Skip::MissingAnchor),
            (false, Some(100.0), None, Skip::MissingEntry),
            (false, Some(100.0), Some(f64::NAN), Skip::MissingEntry),
            (false, Some(100.0), Some(100.0), Skip::Flat),
        ] {
            assert_eq!(signal_of(TSLA, excluded, a, e), Err(want), "{want:?}");
        }
        assert_eq!(fade_side(0.0), None);
        assert_eq!(signal_bps(100.0, -1.0), None);

        let sig = |id: &str, s_bps: f64| Signal {
            instrument: id.into(),
            anchor_px: 1.0,
            entry_px: 1.0,
            s_bps,
            side: fade_side(s_bps).unwrap(),
        };
        let signals = [
            sig("hyperliquid:xyz:A", 50.0), // exactly the minimum: in
            sig("hyperliquid:xyz:B", -49.999),
            sig("hyperliquid:xyz:C", -120.0),
            sig("hyperliquid:xyz:E", 80.0),
            sig("hyperliquid:xyz:D", -80.0), // tie with E: by id
            sig("hyperliquid:xyz:F", 60.0),
        ];
        assert_eq!(
            select_capped(&signals, &RULE),
            [
                "hyperliquid:xyz:C",
                "hyperliquid:xyz:D",
                "hyperliquid:xyz:E",
                "hyperliquid:xyz:F"
            ]
        );
        let all = FadeRule {
            capped_top_n: 10,
            ..RULE
        };
        assert_eq!(select_capped(&signals, &all).len(), 5, "B is under 50 bps");
        let none = FadeRule {
            capped_top_n: 0,
            ..RULE
        };
        assert!(select_capped(&signals, &none).is_empty());

        // A name from its prices; the flat one keeps s = 0 but no side.
        let n = FadeName::at_entry(TSLA, false, point(100.0), point(101.0));
        assert_eq!((n.skip, n.side), (None, Some(Side::Sell)));
        assert_eq!(n.signal().unwrap().s_bps, s.s_bps);
        let flat = FadeName::at_entry(TSLA, false, point(100.0), point(100.0));
        assert_eq!(
            (flat.skip, flat.s_bps, flat.side),
            (Some(Skip::Flat), Some(0.0), None)
        );
        assert!(flat.signal().is_none());
        let gone = FadeName::at_entry(TSLA, false, missing("anchor"), point(100.0));
        assert_eq!((gone.skip, gone.s_bps), (Some(Skip::MissingAnchor), None));
    }

    #[test]
    fn prices_come_from_mid_else_mark_fresh_or_never() {
        let feats = |v: Value| -> Features { serde_json::from_value(v).unwrap() };
        let with_mid = feats(json!({"mid": 347.2, "mark": 347.3}));
        let mark_only = feats(json!({"mark": 347.3, "mid": 0.0}));
        let none = feats(json!({"oracle": 347.0}));
        fn row(features: &Features, observed_at_ms: i64) -> CtxRow<'_> {
            CtxRow {
                status: ObsStatus::Ok,
                observed_at_ms,
                features,
                error: None,
            }
        }
        let p = price_point(
            "anchor",
            TSLA,
            Some(row(&with_mid, 900)),
            1_000,
            500,
            ErrorClass::NotApplicable,
        );
        assert_eq!(
            p,
            Field::ok(PricePoint {
                px: 347.2,
                at_ms: 900,
                source: PriceSource::Mid
            })
        );
        let p = price_point(
            "entry",
            TSLA,
            Some(row(&mark_only, 900)),
            1_000,
            500,
            ErrorClass::Transient,
        );
        assert_eq!(p.value().unwrap().source, PriceSource::Mark);
        let stale = price_point(
            "entry",
            TSLA,
            Some(row(&with_mid, 400)),
            1_000,
            500,
            ErrorClass::Transient,
        );
        let e = stale.error().unwrap();
        assert_eq!(e.field, format!("entry:{TSLA}"));
        assert!(e.message.starts_with("stale"), "{e:?}");
        assert_eq!(e.class, ErrorClass::Transient);
        let nothing = price_point("anchor", TSLA, None, 1_000, 500, ErrorClass::NotApplicable);
        assert_eq!(nothing.error().unwrap().class, ErrorClass::NotApplicable);
        let no_px = price_point(
            "anchor",
            TSLA,
            Some(row(&none, 900)),
            1_000,
            500,
            ErrorClass::NotApplicable,
        );
        assert_eq!(no_px.error().unwrap().class, ErrorClass::Decode);
        let failed = ReadError::new("ctx", ErrorClass::Timeout, "no answer");
        let err_row = CtxRow {
            status: ObsStatus::Error,
            error: Some(&failed),
            ..row(&with_mid, 900)
        };
        let e = price_point(
            "entry",
            TSLA,
            Some(err_row),
            1_000,
            500,
            ErrorClass::Transient,
        );
        assert!(e.error().unwrap().message.contains("timeout no answer"));
        // Replay closes: the candle ending at the instant only.
        let cs = [
            Candle {
                t_ms: 700,
                close: 1.5,
                trades: 3,
            },
            Candle {
                t_ms: 1_000 - CANDLE_MS,
                close: 2.5,
                trades: 0,
            },
        ];
        assert_eq!(close_at(&cs, 1_000), Some(2.5));
        assert_eq!(close_at(&cs, 1_300), None);
    }

    #[test]
    fn ids_and_rows_keep_full_ids() {
        assert_eq!(
            capped_order_id("xmarket-weekend", TSLA, "2026-10-02"),
            "fade:xmarket-weekend:hyperliquid:xyz:TSLA:2026-10-02"
        );
        assert_eq!(
            shadow_order_id("xmarket-weekend-shadow", TSLA, "2026-10-02"),
            "fade-shadow:xmarket-weekend-shadow:hyperliquid:xyz:TSLA:2026-10-02"
        );
        // Attempts: the first keeps the id, a retry counts from 2.
        let id = capped_order_id("xmarket-weekend", TSLA, "2026-10-02");
        assert_eq!(fade_attempt_id(&id, 1), id);
        assert_eq!(
            fade_attempt_id(&id, 2),
            "fade:xmarket-weekend:hyperliquid:xyz:TSLA:2026-10-02:2"
        );
        let sig = FadeSignal {
            anchor_date: "2026-10-02".into(),
            instrument: TSLA.into(),
            anchor_px: 400.0,
            entry_px: 404.0,
            s_bps: 99.5,
            side: Side::Sell,
            capped: true,
            edge_after_costs_bps: 23.0,
        };
        let o = Observation::of("xm_weekend_fade", &sig, 1, 600_000, ObsSource::Live);
        assert_eq!(o.key, "xm_weekend_signal/1:2026-10-02:hyperliquid:xyz:TSLA");
        assert!(key_names(&o.key, TSLA), "the gate's min_edge finds it");
        assert!(!key_names(&o.key, "hyperliquid:xyz:TSL"));
        assert_eq!(o.features[EDGE_FEATURE], 23.0);
        assert_features_ok(&o.features);
        assert_eq!(
            o.headline,
            "xm_weekend_signal 2026-10-02 hyperliquid:xyz:TSLA s_bps=+99.50 fade=sell capped \
             edge_after_costs_bps=23"
        );
    }

    fn window() -> WeekendWindow {
        fade_window(&nyse(), et("2026-10-01 12:00")).unwrap()
    }

    fn order(id: &str, status: &str, qty: f64, notional: f64) -> FadeOrder {
        FadeOrder {
            instrument: id.into(),
            client_order_id: format!("fade:x:{id}:2026-10-02"),
            status: status.into(),
            rule: Some("ok".into()),
            filled_qty: Some(qty),
            avg_px: (qty > 0.0).then_some(100.0),
            notional_usd: Some(notional),
            fee_usd: Some(0.01),
            error: None,
        }
    }

    fn fill(account: &mut PaperAccount, id: &str, side: Side, px: f64, fee: f64) {
        account
            .apply_fill(&Fill {
                instrument: id.into(),
                underlying: id.into(),
                venue: "hyperliquid".into(),
                side,
                qty: 1.0,
                px,
                fee_usd: fee,
                ts_ms: 0,
            })
            .unwrap();
    }

    /// Waiting → entered → closing → closed: counts, features, the P&L
    /// against the bases, statuses and line 1.
    #[test]
    fn the_window_row_counts_and_closes() {
        let w = window();
        assert_eq!(anchor_date(&w), "2026-10-02");
        let mut row = WeekendFade::new(
            FadePhase::Waiting,
            w,
            "xmarket-weekend",
            "xmarket-weekend-shadow",
            3,
            1,
            600,
            w.entry_ms - 3_600_000,
        );
        let o = Observation::of("xm_weekend_fade", &row, row.ts_ms, 120_000, ObsSource::Live);
        assert_eq!(o.key, "xm_weekend/1:2026-10-02");
        assert_eq!(o.status, ObsStatus::Ok);
        assert_eq!(
            o.headline,
            "xm_weekend 2026-10-02 waiting next_entry_s=3600 universe=3 excluded=1"
        );
        assert_eq!(o.features["next_entry_s"], 3600);
        assert!(!o.features.contains_key("n_eligible"), "no snapshot yet");

        // The entry: TSLA faded short (both ledgers), NVDA long (shadow
        // only), the third name without an entry price.
        row.phase = FadePhase::Entered;
        row.entered_at_ms = Some(w.entry_ms);
        let mut tsla = FadeName::at_entry(TSLA, false, point(100.0), point(101.0));
        tsla.capped = true;
        tsla.shadow_base_usd = 1.0;
        tsla.shadow = Some(order(TSLA, "filled", 1.0, 101.0));
        tsla.capped_order = Some(order(TSLA, "filled", 1.0, 101.0));
        let mut nvda = FadeName::at_entry(NVDA, false, point(100.0), point(99.0));
        nvda.shadow = Some(order(NVDA, "filled", 1.0, 99.0));
        let amd = FadeName::at_entry("hyperliquid:xyz:AMD", false, point(10.0), missing("entry"));
        row.names = vec![tsla, nvda, amd];
        row.ts_ms = w.entry_ms + 30_000;
        let o = Observation::of("xm_weekend_fade", &row, row.ts_ms, 120_000, ObsSource::Live);
        assert_eq!(o.status, ObsStatus::Partial, "{:?}", o.errors);
        assert_eq!(
            o.headline,
            "xm_weekend 2026-10-02 entered eligible=2/3 shadow=2 capped=1/1 missing_anchor=0 \
             missing_entry=1 capped: hyperliquid:xyz:TSLA"
        );
        assert_eq!(o.errors[0].field, "entry");
        assert_eq!(
            o.errors[0].message,
            "1 names without a fresh entry price: hyperliquid:xyz:AMD"
        );
        assert_features_ok(&o.features);
        for (k, v) in [
            ("n_eligible", 2),
            ("n_capped", 1),
            ("n_shadow_filled", 2),
            ("n_capped_filled", 1),
            ("n_missing_entry", 1),
            ("n_stale", 0),
        ] {
            assert_eq!(o.features[k], v, "{k}");
        }

        // AMD without a fresh entry row (`stale`): taken before the deadline
        // the snapshot is incomplete — an `error` row, never kept; from the
        // deadline on AMD is left out.
        let deadline = snapshot_deadline_ms(&w, 600_000);
        assert_eq!(deadline, w.entry_ms + ENTRY_WAIT_MS);
        assert_eq!(snapshot_deadline_ms(&w, 60_000), w.entry_ms + 30_000);
        let mut early = row.clone();
        early.names[2].skip = Some(Skip::Stale);
        early.entered_at_ms = Some(deadline - 1);
        assert!(!early.complete());
        let o = Observation::of(
            "xm_weekend_fade",
            &early,
            early.ts_ms,
            120_000,
            ObsSource::Live,
        );
        assert_eq!(o.status, ObsStatus::Error, "{:?}", o.errors);
        assert_eq!(o.errors[0].class, ErrorClass::Transient);
        assert!(
            o.errors[0]
                .message
                .starts_with("1 names without a fresh entry row yet"),
            "{:?}",
            o.errors
        );
        early.entered_at_ms = Some(deadline);
        assert!(early.complete());
        let o = Observation::of(
            "xm_weekend_fade",
            &early,
            early.ts_ms,
            120_000,
            ObsSource::Live,
        );
        assert_eq!(o.status, ObsStatus::Partial, "{:?}", o.errors);
        assert_eq!(
            o.headline,
            "xm_weekend 2026-10-02 entered eligible=2/3 shadow=2 capped=1/1 missing_anchor=0 \
             missing_entry=0 stale=1 capped: hyperliquid:xyz:TSLA"
        );
        assert_eq!(
            o.errors[0].message,
            "1 names left out stale (no fresh entry row by the snapshot's deadline): \
             hyperliquid:xyz:AMD"
        );
        assert_eq!(o.features["n_stale"], 1);

        // After the exit: the shadow ledger is flat, the capped TSLA short
        // still open ⇒ closing, no P&L yet.
        let mut shadow = PaperAccount::new("xmarket-weekend-shadow", 10_000.0).unwrap();
        let mut capped = PaperAccount::new("xmarket-weekend", 100.0).unwrap();
        fill(&mut shadow, TSLA, Side::Sell, 101.0, 0.01);
        fill(&mut shadow, TSLA, Side::Buy, 100.0, 0.01);
        fill(&mut shadow, NVDA, Side::Buy, 99.0, 0.01);
        fill(&mut shadow, NVDA, Side::Buy, 99.0, 0.0);
        fill(&mut capped, TSLA, Side::Sell, 101.0, 0.01);
        row.refresh(&shadow, &capped, w.entry_ms + 60_000);
        assert_eq!(row.phase, FadePhase::Entered, "before the exit");
        assert_eq!(row.names[1].shadow_qty, Some(2.0));
        row.refresh(&shadow, &capped, w.exit_ms);
        assert_eq!(row.phase, FadePhase::Closing);
        assert_eq!(row.names[0].shadow_pnl_usd, Some(1.0 - 0.02 - 1.0));
        assert_eq!(row.names[0].capped_pnl_usd, None, "still open");
        // NVDA still open in the shadow ledger (2 long).
        assert_eq!(row.names[1].shadow_pnl_usd, None);
        let o = Observation::of("xm_weekend_fade", &row, row.ts_ms, 120_000, ObsSource::Live);
        assert!(o
            .headline
            .ends_with("closing shadow_open=1 capped_open=1 shadow_exits=0"));
        assert!(
            !o.features.contains_key("shadow_pnl_usd"),
            "never a partial sum"
        );

        // Everything closed: P&L = net now − base; bps of the entry notional.
        fill(&mut shadow, NVDA, Side::Sell, 98.0, 0.0);
        fill(&mut shadow, NVDA, Side::Sell, 98.0, 0.0);
        fill(&mut capped, TSLA, Side::Buy, 100.5, 0.01);
        row.refresh(&shadow, &capped, w.exit_ms + 60_000);
        assert_eq!(row.phase, FadePhase::Closed);
        let tsla = &row.names[0];
        let t_usd = 1.0 - 0.02 - 1.0;
        assert_eq!(tsla.shadow_pnl_usd, Some(t_usd));
        assert_eq!(tsla.shadow_pnl_bps, Some(t_usd / 101.0 * 10_000.0));
        let c_usd = 0.5 - 0.02;
        assert!((tsla.capped_pnl_usd.unwrap() - c_usd).abs() < 1e-12);
        let n_usd = -2.0 - 0.01;
        assert!((row.names[1].shadow_pnl_usd.unwrap() - n_usd).abs() < 1e-12);
        let o = Observation::of("xm_weekend_fade", &row, row.ts_ms, 120_000, ObsSource::Live);
        assert_features_ok(&o.features);
        let shadow_usd = o.features["shadow_pnl_usd"].as_f64().unwrap();
        assert!((shadow_usd - (t_usd + n_usd)).abs() < 1e-12);
        assert!(o.features.contains_key("capped_mean_net_bps"));
        assert!(
            o.headline.starts_with(
                "xm_weekend 2026-10-02 closed shadow_pnl_usd=-2.03 shadow_mean_net_bps="
            ),
            "{}",
            o.headline
        );
        assert!(o.headline.chars().count() <= MAX_LINE1_CHARS);

        // An entry with no eligible name is an error row (not kept).
        let mut none = WeekendFade::new(FadePhase::Entered, w, "a", "b", 1, 0, 600, w.entry_ms);
        none.names = vec![FadeName::at_entry(
            TSLA,
            false,
            missing("anchor"),
            point(1.0),
        )];
        assert_eq!(none.status(), ObsStatus::Error);
        assert_eq!(none.errors()[0].class, ErrorClass::NotApplicable);
        // A missed entry: partial, says why.
        let mut missed = WeekendFade::new(FadePhase::MissedEntry, w, "a", "b", 1, 0, 600, 0);
        missed.late_s = Some(900);
        assert_eq!(missed.status(), ObsStatus::Partial);
        assert_eq!(
            missed.headline(),
            "xm_weekend 2026-10-02 missed_entry late_s=900 lateness_max_s=600 universe=1"
        );
        // The row round-trips (the tool reloads its snapshot).
        let back: WeekendFade = o.typed().unwrap();
        assert_eq!(back, row);
    }

    /// Line 1 never cuts an id: with long ids the capped names move to `data`.
    #[test]
    fn a_long_capped_list_leaves_line_one_whole() {
        let w = window();
        let mut row = WeekendFade::new(FadePhase::Entered, w, "a", "b", 6, 0, 600, w.entry_ms);
        row.names = (0..6)
            .map(|i| {
                let id = format!("hyperliquid:xyz:{}", "LONGNAME".repeat(3) + &i.to_string());
                let mut n = FadeName::at_entry(&id, false, point(100.0), point(102.0));
                n.capped = true;
                n
            })
            .collect();
        let h = row.headline();
        assert!(h.chars().count() <= MAX_LINE1_CHARS, "{h}");
        assert!(!h.contains("capped:"), "{h}");
    }
}
