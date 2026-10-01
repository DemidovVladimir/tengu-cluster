//! Strategy specs — the Architect's level-2 capabilities
//! (`docs/xlab-2026-10-01.md` § 5): a kind and its parameters, never code.
//! JSON (tool `spec`, `tengu backtest --spec`) or a
//! `[backtest.strategies.<name>]` table (TOML → `serde_json::Value`); unknown
//! fields are refused and [`StrategySpec::from_value`] validates before any
//! run. `@<name>` universes are resolved by the application.
//!
//! | Common field | Rule |
//! |---|---|
//! | `name` | `[a-z0-9_]{1,48}`: the strategies key, or the JSON's own (both given ⇒ equal) |
//! | `kind` | one of the six below |
//! | `universe` | `"@<name>"` or full ids (one string or a list, ≤ 1000, distinct); required by `weekend_window` `daily_window` `move_trigger` `funding_carry`, refused by `pair_spread` / `event_window` (they name their instruments) |
//! | `interval` | `1m 5m 15m 1h 4h 1d`; window kinds ≤ 1h (their instants are local wall-clock times) |
//! | `notional_usd` | per trade, 0 < x ≤ 1 000 000 (default `[backtest] notional_usd`) |
//! | `exclude` | full ids never traded (≤ 1000) |
//! | `costs` | `CostSpec` override (`costs.rs`); else `[backtest.costs."<prefix>"]` |
//!
//! | Kind | Params — default | Bounds |
//! |---|---|---|
//! | `weekend_window` | `calendar` (an exchange `[xmarket.calendars]` id), `direction`, `min_abs_signal_bps` — 0, `top_n` — all, `anchor_offset_mins` / `entry_offset_mins` / `exit_offset_mins` — 0 | offsets ±1440, whole bars |
//! | `daily_window` | `days` (`all` · `weekdays` · `trading` + `calendar`), `tz`, `anchor` / `entry` / `exit` (`HH:MM`), `direction`, `min_abs_signal_bps` — 0, `top_n` — all | `tz` a `domain::tz` zone; times whole bars |
//! | `move_trigger` | `lookback_bars`, `threshold_bps`, `min_volume_ratio` + `volume_baseline_bars` — off, `direction`, `hold_bars`, `cooldown_bars` — `hold_bars`, `take_profit_bps` / `stop_loss_bps` — off | bars 1..=10000 (cooldown 0..=10000), bps 0 < x ≤ 10000, ratio 0 < x ≤ 1000 |
//! | `funding_carry` | `min_apr_pct`, `exit_apr_pct` — off, `hold_hours` | 0 < min ≤ 10000, 0 ≤ exit < min, hours 1..=8760 |
//! | `pair_spread` | `legs` (2 distinct ids), `lookback_bars`, `entry_z`, `exit_z`, `max_hold_bars` | lookback 2..=10000, 0 ≤ exit_z < entry_z ≤ 100, hold 1..=10000 |
//! | `event_window` | `events` (`{instrument, t, label?}`, `t` = RFC 3339 publication time), `entry_delay_mins` — 0, `direction`, `min_abs_move_bps` — 0, `exit_after_mins` xor `exit_at` (`HH:MM`) + `tz` | 1..=10000 events, label ≤ 120 chars, delay ≤ 10080, exit_after 1..=43200 |
//!
//! | [`SplitSpec`] (`--split`, tool `split`) | Holdout |
//! |---|---|
//! | `time:<RFC 3339 \| date \| ms>` | decided at or after the instant (in-sample: before) |
//! | `instruments:<id,id,…>` | trades on a listed id (any leg) |

// Consumers land with the xlab application wave (docs/xlab-2026-10-01.md); drop this then.
#![cfg_attr(not(test), allow(dead_code))]

use std::collections::BTreeSet;
use std::fmt;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::domain::backtest::costs::{CostSpec, HalfSpread};
use crate::domain::backtest::stats::PeriodKind;
use crate::domain::book::Side;
use crate::domain::calendar::parse_hm;
use crate::domain::market::InstrumentId;
use crate::domain::marketdata::{parse_time, Interval};
use crate::domain::tz::Zone;

/// Ids in a universe, an `exclude` list or a split.
pub const MAX_IDS: usize = 1_000;
/// Events of one `event_window` spec.
pub const MAX_EVENTS: usize = 10_000;
/// Bars of any lookback, baseline, hold or cooldown.
pub const MAX_BARS: u32 = 10_000;
const MAX_NOTIONAL_USD: f64 = 1_000_000.0;
const MAX_BPS: f64 = 10_000.0;
const MAX_RATIO: f64 = 1_000.0;
const MAX_Z: f64 = 100.0;
const MAX_LABEL_CHARS: usize = 120;
const MAX_OFFSET_MINS: i64 = 1_440;
const MAX_DELAY_MINS: u32 = 10_080;
const MAX_EXIT_AFTER_MINS: u32 = 43_200;
const MAX_HOLD_HOURS: u32 = 8_760;
/// Event problems listed before "… and N more".
const MAX_EVENT_ERRORS: usize = 20;
const MIN_MS: i64 = 60_000;
const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 86_400_000;

/// The six kinds, wire names.
pub const KINDS: [&str; 6] = [
    "weekend_window",
    "daily_window",
    "move_trigger",
    "funding_carry",
    "pair_spread",
    "event_window",
];
const COMMON_FIELDS: [&str; 7] = [
    "name",
    "kind",
    "universe",
    "interval",
    "notional_usd",
    "exclude",
    "costs",
];

/// `fade` trades against the move, `follow` with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Fade,
    Follow,
}

impl Direction {
    /// The side for a move of `move_bps`: fade sells a rise and buys a fall,
    /// follow the reverse; `None` without a move (0 or not a number).
    pub fn side(self, move_bps: f64) -> Option<Side> {
        let fade = if move_bps > 0.0 {
            Side::Sell
        } else if move_bps < 0.0 {
            Side::Buy
        } else {
            return None;
        };
        Some(match self {
            Direction::Fade => fade,
            Direction::Follow => fade.opposite(),
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Direction::Fade => "fade",
            Direction::Follow => "follow",
        }
    }
}

/// `"@<name>"` (`[backtest.universes.<name>]`) or full instrument ids.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Value", into = "Value")]
pub enum Universe {
    /// The text as written, `@` included (`BacktestConfig::resolve_universe`
    /// takes it as is).
    Named(String),
    Ids(Vec<String>),
}

impl Universe {
    /// The `[backtest.universes]` key of a named universe (no `@`).
    pub fn name(&self) -> Option<&str> {
        match self {
            Universe::Named(s) => Some(s.strip_prefix('@').unwrap_or(s)),
            Universe::Ids(_) => None,
        }
    }
}

impl TryFrom<Value> for Universe {
    type Error = String;
    fn try_from(v: Value) -> Result<Self, String> {
        let bad = || {
            "universe is \"@<name>\" or full instrument ids (a string or a list of strings)"
                .to_string()
        };
        match v {
            Value::String(s) => Ok(if s.starts_with('@') {
                Universe::Named(s)
            } else {
                Universe::Ids(vec![s])
            }),
            Value::Array(items) => items
                .into_iter()
                .map(|i| match i {
                    Value::String(s) => Ok(s),
                    _ => Err(bad()),
                })
                .collect::<Result<Vec<_>, _>>()
                .map(Universe::Ids),
            _ => Err(bad()),
        }
    }
}

impl From<Universe> for Value {
    fn from(u: Universe) -> Value {
        match u {
            Universe::Named(name) => Value::String(name),
            Universe::Ids(ids) => json!(ids),
        }
    }
}

/// Which days a `daily_window` trades.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Days {
    All,
    Weekdays,
    /// The `calendar`'s trading days; "next day" = the next trading day.
    Trading,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WeekendWindowParams {
    pub calendar: String,
    pub direction: Direction,
    #[serde(default)]
    pub min_abs_signal_bps: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_n: Option<usize>,
    #[serde(default)]
    pub anchor_offset_mins: i64,
    #[serde(default)]
    pub entry_offset_mins: i64,
    #[serde(default)]
    pub exit_offset_mins: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DailyWindowParams {
    pub days: Days,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calendar: Option<String>,
    pub tz: String,
    pub anchor: String,
    pub entry: String,
    pub exit: String,
    pub direction: Direction,
    #[serde(default)]
    pub min_abs_signal_bps: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_n: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoveTriggerParams {
    pub lookback_bars: u32,
    pub threshold_bps: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_volume_ratio: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume_baseline_bars: Option<u32>,
    pub direction: Direction,
    pub hold_bars: u32,
    /// Bars after a trigger before the instrument may trigger again
    /// (default `hold_bars`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_bars: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub take_profit_bps: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_loss_bps: Option<f64>,
}

impl MoveTriggerParams {
    pub fn cooldown(&self) -> u32 {
        self.cooldown_bars.unwrap_or(self.hold_bars)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FundingCarryParams {
    pub min_apr_pct: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_apr_pct: Option<f64>,
    pub hold_hours: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairSpreadParams {
    pub legs: Vec<String>,
    pub lookback_bars: u32,
    pub entry_z: f64,
    pub exit_z: f64,
    pub max_hold_bars: u32,
}

/// One `event_window` event: an instrument and the publication time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventSpec {
    pub instrument: String,
    /// RFC 3339.
    pub t: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventWindowParams {
    pub events: Vec<EventSpec>,
    #[serde(default)]
    pub entry_delay_mins: u32,
    pub direction: Direction,
    #[serde(default)]
    pub min_abs_move_bps: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_after_mins: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tz: Option<String>,
}

/// A kind and its parameters (module table).
#[derive(Debug, Clone, PartialEq)]
pub enum StrategyKind {
    WeekendWindow(WeekendWindowParams),
    DailyWindow(DailyWindowParams),
    MoveTrigger(MoveTriggerParams),
    FundingCarry(FundingCarryParams),
    PairSpread(PairSpreadParams),
    EventWindow(EventWindowParams),
}

fn parse_params<T: DeserializeOwned>(kind: &str, v: Value) -> Result<T, String> {
    serde_json::from_value(v).map_err(|e| {
        let e = e.to_string();
        if e.starts_with("unknown field") {
            format!("{kind}: {e} (common fields: {})", COMMON_FIELDS.join(", "))
        } else {
            format!("{kind}: {e}")
        }
    })
}

impl StrategyKind {
    fn parse(kind: &str, params: Value) -> Result<Self, String> {
        Ok(match kind {
            "weekend_window" => StrategyKind::WeekendWindow(parse_params(kind, params)?),
            "daily_window" => StrategyKind::DailyWindow(parse_params(kind, params)?),
            "move_trigger" => StrategyKind::MoveTrigger(parse_params(kind, params)?),
            "funding_carry" => StrategyKind::FundingCarry(parse_params(kind, params)?),
            "pair_spread" => StrategyKind::PairSpread(parse_params(kind, params)?),
            "event_window" => StrategyKind::EventWindow(parse_params(kind, params)?),
            other => return Err(format!("kind `{other}` is not one of {}", KINDS.join(", "))),
        })
    }

    pub fn name(&self) -> &'static str {
        match self {
            StrategyKind::WeekendWindow(_) => "weekend_window",
            StrategyKind::DailyWindow(_) => "daily_window",
            StrategyKind::MoveTrigger(_) => "move_trigger",
            StrategyKind::FundingCarry(_) => "funding_carry",
            StrategyKind::PairSpread(_) => "pair_spread",
            StrategyKind::EventWindow(_) => "event_window",
        }
    }

    /// The kind names its own instruments (no `universe`).
    pub fn names_instruments(&self) -> bool {
        matches!(
            self,
            StrategyKind::PairSpread(_) | StrategyKind::EventWindow(_)
        )
    }

    fn params_value(&self) -> Value {
        let v = match self {
            StrategyKind::WeekendWindow(p) => serde_json::to_value(p),
            StrategyKind::DailyWindow(p) => serde_json::to_value(p),
            StrategyKind::MoveTrigger(p) => serde_json::to_value(p),
            StrategyKind::FundingCarry(p) => serde_json::to_value(p),
            StrategyKind::PairSpread(p) => serde_json::to_value(p),
            StrategyKind::EventWindow(p) => serde_json::to_value(p),
        };
        v.unwrap_or(Value::Null)
    }
}

/// The common part of a spec on the wire.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommonWire {
    #[serde(default)]
    name: Option<String>,
    kind: String,
    #[serde(default)]
    universe: Option<Universe>,
    interval: Interval,
    #[serde(default)]
    notional_usd: Option<f64>,
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default)]
    costs: Option<CostSpec>,
}

/// A validated strategy spec (module tables).
#[derive(Debug, Clone, PartialEq)]
pub struct StrategySpec {
    pub name: String,
    pub universe: Option<Universe>,
    pub interval: Interval,
    pub notional_usd: Option<f64>,
    pub exclude: Vec<String>,
    pub costs: Option<CostSpec>,
    pub kind: StrategyKind,
}

/// `[a-z0-9_]{1,48}` — strategy and universe names (`config::backtest::valid_name`).
pub fn valid_name(name: &str) -> bool {
    (1..=48).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

impl StrategySpec {
    /// Parse and validate `value`. `name` is the strategies key (`""` = take
    /// the value's own `name`); every error names the strategy and the field.
    pub fn from_value(name: &str, value: &Value) -> Result<StrategySpec, Vec<String>> {
        let label = if name.is_empty() {
            value.get("name").and_then(Value::as_str).unwrap_or("spec")
        } else {
            name
        };
        let fail = |m: String| vec![format!("strategy `{label}`: {m}")];
        let Some(obj) = value.as_object() else {
            return Err(fail(
                "a spec is an object: kind, universe, interval and the kind's parameters"
                    .to_string(),
            ));
        };
        let (mut common, mut params) = (Map::new(), Map::new());
        for (k, v) in obj {
            let side = if COMMON_FIELDS.contains(&k.as_str()) {
                &mut common
            } else {
                &mut params
            };
            side.insert(k.clone(), v.clone());
        }
        let c: CommonWire =
            serde_json::from_value(Value::Object(common)).map_err(|e| fail(e.to_string()))?;
        let kind = StrategyKind::parse(&c.kind, Value::Object(params)).map_err(fail)?;
        let spec_name = match c.name {
            Some(own) if !name.is_empty() && own != name => {
                return Err(fail(format!(
                    "name `{own}` differs from the strategy's key `{name}`"
                )))
            }
            Some(own) => own,
            None => name.to_string(),
        };
        let spec = StrategySpec {
            name: spec_name,
            universe: c.universe,
            interval: c.interval,
            notional_usd: c.notional_usd,
            exclude: c.exclude,
            costs: c.costs,
            kind,
        };
        let errors = spec.validation_errors();
        if errors.is_empty() {
            Ok(spec)
        } else {
            Err(errors
                .into_iter()
                .map(|e| format!("strategy `{label}`: {e}"))
                .collect())
        }
    }

    /// The spec as one JSON object (common fields + the kind's parameters);
    /// [`from_value`](Self::from_value) reads it back unchanged.
    pub fn to_value(&self) -> Value {
        let mut m = match self.kind.params_value() {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        m.insert("name".into(), json!(self.name));
        m.insert("kind".into(), json!(self.kind.name()));
        if let Some(u) = &self.universe {
            m.insert("universe".into(), Value::from(u.clone()));
        }
        m.insert("interval".into(), json!(self.interval.as_str()));
        if let Some(n) = self.notional_usd {
            m.insert("notional_usd".into(), json!(n));
        }
        if !self.exclude.is_empty() {
            m.insert("exclude".into(), json!(self.exclude));
        }
        if let Some(c) = &self.costs {
            m.insert(
                "costs".into(),
                serde_json::to_value(c).unwrap_or(Value::Null),
            );
        }
        Value::Object(m)
    }

    pub fn kind_name(&self) -> &'static str {
        self.kind.name()
    }

    /// Per-trade notional: the spec's, else `default_usd` (`[backtest]`).
    pub fn notional(&self, default_usd: f64) -> f64 {
        self.notional_usd.unwrap_or(default_usd)
    }

    /// The calendar the spec reads (`weekend_window`, `daily_window` with
    /// `days = "trading"`) — the application checks it names an exchange
    /// `[xmarket.calendars]` row.
    pub fn calendar(&self) -> Option<&str> {
        match &self.kind {
            StrategyKind::WeekendWindow(p) => Some(p.calendar.as_str()),
            StrategyKind::DailyWindow(p) => p.calendar.as_deref(),
            _ => None,
        }
    }

    /// The ids a `pair_spread` / `event_window` spec names (sorted, distinct);
    /// empty for the universe kinds.
    pub fn named_instruments(&self) -> Vec<String> {
        let ids: BTreeSet<String> = match &self.kind {
            StrategyKind::PairSpread(p) => p.legs.iter().cloned().collect(),
            StrategyKind::EventWindow(p) => p.events.iter().map(|e| e.instrument.clone()).collect(),
            _ => BTreeSet::new(),
        };
        ids.into_iter().collect()
    }

    /// The bucket statistics group trades by (`stats.rs`).
    pub fn period_kind(&self) -> PeriodKind {
        match &self.kind {
            StrategyKind::WeekendWindow(_) => PeriodKind::Weekend,
            StrategyKind::DailyWindow(p) => match p.days {
                Days::All => PeriodKind::Day,
                Days::Weekdays => PeriodKind::Weekday,
                Days::Trading => PeriodKind::TradingDay,
            },
            _ => PeriodKind::Day,
        }
    }

    /// The time range a run over `[from_ms, to_ms)` reads: back far enough
    /// for anchors, lookbacks, features (168 h returns, the 7-day volume
    /// baseline) and an `abdi_ranaldo` window of the spec's own costs;
    /// forward to the latest exit. `[backtest.costs]` windows are the
    /// caller's to add.
    pub fn data_range(&self, from_ms: i64, to_ms: i64) -> (i64, i64) {
        let iv = self.interval.ms();
        let bars = |n: u32| i64::from(n).saturating_mul(iv);
        let mins = |m: i64| m.saturating_abs().saturating_mul(MIN_MS);
        let features = 8 * DAY_MS + 2 * iv;
        let spread = match self.costs.as_ref().map(|c| &c.half_spread) {
            Some(HalfSpread::AbdiRanaldo { window_bars, .. }) => {
                bars(window_bars.saturating_add(1))
            }
            _ => 0,
        };
        let (back, forward) = match &self.kind {
            StrategyKind::WeekendWindow(p) => (
                8 * DAY_MS + mins(p.anchor_offset_mins),
                2 * DAY_MS + mins(p.exit_offset_mins).max(mins(p.entry_offset_mins)),
            ),
            StrategyKind::DailyWindow(_) => (6 * DAY_MS, 6 * DAY_MS),
            StrategyKind::MoveTrigger(p) => (
                bars(
                    p.lookback_bars
                        .saturating_add(p.volume_baseline_bars.unwrap_or(0))
                        .saturating_add(1),
                ),
                bars(p.hold_bars) + iv,
            ),
            StrategyKind::FundingCarry(p) => (iv, i64::from(p.hold_hours) * HOUR_MS + iv),
            // Aligned closes skip gaps: four times the lookback as margin.
            StrategyKind::PairSpread(p) => (
                bars(p.lookback_bars.saturating_add(1)).saturating_mul(4),
                bars(p.max_hold_bars) + iv,
            ),
            StrategyKind::EventWindow(p) => {
                let exit = p
                    .exit_after_mins
                    .map_or(2 * DAY_MS, |m| i64::from(m) * MIN_MS);
                (iv, i64::from(p.entry_delay_mins) * MIN_MS + exit + 2 * iv)
            }
        };
        (
            from_ms.saturating_sub(back.max(features).max(spread)),
            to_ms.saturating_add(forward),
        )
    }

    /// Every problem, each naming its field (module tables).
    pub fn validation_errors(&self) -> Vec<String> {
        let mut e = Vec::new();
        if !valid_name(&self.name) {
            e.push(format!("name `{}`: [a-z0-9_], 1-48 characters", self.name));
        }
        let kind = self.kind.name();
        match (&self.universe, self.kind.names_instruments()) {
            (Some(_), true) => e.push(format!(
                "universe: {kind} trades the instruments it names (`{}`); leave universe out",
                if matches!(self.kind, StrategyKind::PairSpread(_)) {
                    "legs"
                } else {
                    "events"
                }
            )),
            (None, false) => e.push(format!(
                "universe is required for {kind}: \"@<name>\" or full instrument ids"
            )),
            (Some(u @ Universe::Named(text)), false) => {
                if !u.name().is_some_and(valid_name) {
                    e.push(format!(
                        "universe `{text}`: universe names are [a-z0-9_], 1-48 characters"
                    ));
                }
            }
            (Some(Universe::Ids(ids)), false) => {
                if ids.is_empty() {
                    e.push("universe lists no instrument".to_string());
                }
                check_ids(&mut e, "universe", ids);
            }
            (None, true) => {}
        }
        if let Some(n) = self.notional_usd {
            if !(n.is_finite() && n > 0.0 && n <= MAX_NOTIONAL_USD) {
                e.push(format!(
                    "notional_usd must be finite, > 0 and ≤ {MAX_NOTIONAL_USD}"
                ));
            }
        }
        check_ids(&mut e, "exclude", &self.exclude);
        if let Some(c) = &self.costs {
            e.extend(c.validation_errors("costs"));
        }
        let iv = self.interval;
        match &self.kind {
            StrategyKind::WeekendWindow(p) => {
                if p.calendar.trim().is_empty() {
                    e.push(
                        "calendar: name an exchange [xmarket.calendars.<id>] (e.g. us_equity)"
                            .to_string(),
                    );
                }
                check_window_interval(&mut e, kind, iv);
                check_bps(&mut e, "min_abs_signal_bps", p.min_abs_signal_bps, true);
                check_top_n(&mut e, p.top_n);
                for (field, mins) in [
                    ("anchor_offset_mins", p.anchor_offset_mins),
                    ("entry_offset_mins", p.entry_offset_mins),
                    ("exit_offset_mins", p.exit_offset_mins),
                ] {
                    if mins.abs() > MAX_OFFSET_MINS {
                        e.push(format!(
                            "{field} must be within -{MAX_OFFSET_MINS}..={MAX_OFFSET_MINS}"
                        ));
                    } else if (mins * MIN_MS) % iv.ms() != 0 {
                        e.push(format!(
                            "{field} = {mins} is not a whole number of {iv} bars"
                        ));
                    }
                }
            }
            StrategyKind::DailyWindow(p) => {
                match (p.days, p.calendar.as_deref().map(str::trim)) {
                    (Days::Trading, None | Some("")) => e.push(
                        "calendar is required with days = \"trading\" (an exchange [xmarket.calendars.<id>])"
                            .to_string(),
                    ),
                    (Days::All | Days::Weekdays, Some(_)) => e.push(
                        "calendar is read only with days = \"trading\"".to_string(),
                    ),
                    _ => {}
                }
                check_zone(&mut e, "tz", &p.tz);
                check_window_interval(&mut e, kind, iv);
                for (field, hm) in [
                    ("anchor", &p.anchor),
                    ("entry", &p.entry),
                    ("exit", &p.exit),
                ] {
                    check_hm_on_grid(&mut e, field, hm, iv);
                }
                check_bps(&mut e, "min_abs_signal_bps", p.min_abs_signal_bps, true);
                check_top_n(&mut e, p.top_n);
            }
            StrategyKind::MoveTrigger(p) => {
                check_bars(&mut e, "lookback_bars", p.lookback_bars, 1);
                check_bps(&mut e, "threshold_bps", p.threshold_bps, false);
                match (p.min_volume_ratio, p.volume_baseline_bars) {
                    (Some(r), Some(b)) => {
                        if !(r.is_finite() && r > 0.0 && r <= MAX_RATIO) {
                            e.push(format!(
                                "min_volume_ratio must be finite, > 0 and ≤ {MAX_RATIO}"
                            ));
                        }
                        check_bars(&mut e, "volume_baseline_bars", b, 1);
                    }
                    (Some(_), None) => {
                        e.push("volume_baseline_bars is required with min_volume_ratio".to_string())
                    }
                    (None, Some(_)) => e.push(
                        "volume_baseline_bars is read only with min_volume_ratio".to_string(),
                    ),
                    (None, None) => {}
                }
                check_bars(&mut e, "hold_bars", p.hold_bars, 1);
                if let Some(c) = p.cooldown_bars {
                    check_bars(&mut e, "cooldown_bars", c, 0);
                }
                for (field, v) in [
                    ("take_profit_bps", p.take_profit_bps),
                    ("stop_loss_bps", p.stop_loss_bps),
                ] {
                    if let Some(v) = v {
                        check_bps(&mut e, field, v, false);
                    }
                }
            }
            StrategyKind::FundingCarry(p) => {
                if !(p.min_apr_pct.is_finite() && p.min_apr_pct > 0.0 && p.min_apr_pct <= MAX_BPS) {
                    e.push(format!("min_apr_pct must be finite, > 0 and ≤ {MAX_BPS}"));
                }
                if let Some(x) = p.exit_apr_pct {
                    if !(x.is_finite() && x >= 0.0 && x < p.min_apr_pct) {
                        e.push("exit_apr_pct must be finite, ≥ 0 and < min_apr_pct".to_string());
                    }
                }
                if !(1..=MAX_HOLD_HOURS).contains(&p.hold_hours) {
                    e.push(format!("hold_hours must be within 1..={MAX_HOLD_HOURS}"));
                }
            }
            StrategyKind::PairSpread(p) => {
                if p.legs.len() != 2 {
                    e.push(format!(
                        "legs lists {} ids; a pair has exactly 2",
                        p.legs.len()
                    ));
                }
                check_ids(&mut e, "legs", &p.legs);
                check_bars(&mut e, "lookback_bars", p.lookback_bars, 2);
                if !(p.entry_z.is_finite() && p.entry_z > 0.0 && p.entry_z <= MAX_Z) {
                    e.push(format!("entry_z must be finite, > 0 and ≤ {MAX_Z}"));
                }
                if !(p.exit_z.is_finite() && p.exit_z >= 0.0 && p.exit_z < p.entry_z) {
                    e.push("exit_z must be finite, ≥ 0 and < entry_z".to_string());
                }
                check_bars(&mut e, "max_hold_bars", p.max_hold_bars, 1);
            }
            StrategyKind::EventWindow(p) => {
                if p.events.is_empty() || p.events.len() > MAX_EVENTS {
                    e.push(format!(
                        "events lists {}; give 1..={MAX_EVENTS}",
                        p.events.len()
                    ));
                }
                let mut problems = Vec::new();
                for (i, ev) in p.events.iter().enumerate() {
                    if let Err(err) = InstrumentId::parse(&ev.instrument) {
                        problems.push(format!("events[{i}].instrument: {err}"));
                    }
                    if parse_rfc3339(&ev.t).is_none() {
                        problems.push(format!(
                            "events[{i}].t `{}` is not RFC 3339 (2026-10-02T13:30:00Z)",
                            ev.t
                        ));
                    }
                    if ev
                        .label
                        .as_ref()
                        .is_some_and(|l| l.chars().count() > MAX_LABEL_CHARS)
                    {
                        problems.push(format!(
                            "events[{i}].label is longer than {MAX_LABEL_CHARS} characters"
                        ));
                    }
                }
                let more = problems.len().saturating_sub(MAX_EVENT_ERRORS);
                e.extend(problems.into_iter().take(MAX_EVENT_ERRORS));
                if more > 0 {
                    e.push(format!("events: … and {more} more problems"));
                }
                if p.entry_delay_mins > MAX_DELAY_MINS {
                    e.push(format!(
                        "entry_delay_mins must be within 0..={MAX_DELAY_MINS}"
                    ));
                }
                check_bps(&mut e, "min_abs_move_bps", p.min_abs_move_bps, true);
                match (p.exit_after_mins, &p.exit_at) {
                    (Some(m), None) => {
                        if !(1..=MAX_EXIT_AFTER_MINS).contains(&m) {
                            e.push(format!(
                                "exit_after_mins must be within 1..={MAX_EXIT_AFTER_MINS}"
                            ));
                        }
                        if p.tz.is_some() {
                            e.push("tz is read only with exit_at".to_string());
                        }
                    }
                    (None, Some(hm)) => {
                        if parse_hm(hm).is_none() {
                            e.push(format!("exit_at `{hm}` is not HH:MM (00:00-23:59)"));
                        }
                        match &p.tz {
                            Some(tz) => check_zone(&mut e, "tz", tz),
                            None => e.push("tz is required with exit_at".to_string()),
                        }
                    }
                    (Some(_), Some(_)) => {
                        e.push("set exit_after_mins or exit_at, not both".to_string())
                    }
                    (None, None) => e.push("set exit_after_mins or exit_at".to_string()),
                }
            }
        }
        e
    }
}

/// Strict RFC 3339 → epoch ms.
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s.trim())
        .ok()
        .map(|t| t.timestamp_millis())
}

fn check_ids(e: &mut Vec<String>, field: &str, ids: &[String]) {
    if ids.len() > MAX_IDS {
        e.push(format!(
            "{field} lists {} ids; at most {MAX_IDS}",
            ids.len()
        ));
    }
    let mut seen = BTreeSet::new();
    for id in ids {
        if let Err(err) = InstrumentId::parse(id) {
            e.push(format!("{field}: {err}"));
        } else if !seen.insert(id.as_str()) {
            e.push(format!("{field}: `{id}` is listed twice"));
        }
    }
}

/// Finite, `0 ≤ x ≤ 10000` (`zero_ok`) or `0 < x ≤ 10000`.
fn check_bps(e: &mut Vec<String>, field: &str, x: f64, zero_ok: bool) {
    let ok = x.is_finite() && x <= MAX_BPS && if zero_ok { x >= 0.0 } else { x > 0.0 };
    if !ok {
        let lo = if zero_ok { "≥ 0" } else { "> 0" };
        e.push(format!("{field} must be finite, {lo} and ≤ {MAX_BPS}"));
    }
}

fn check_bars(e: &mut Vec<String>, field: &str, n: u32, min: u32) {
    if !(min..=MAX_BARS).contains(&n) {
        e.push(format!("{field} must be within {min}..={MAX_BARS}"));
    }
}

fn check_top_n(e: &mut Vec<String>, top_n: Option<usize>) {
    if let Some(n) = top_n {
        if !(1..=MAX_IDS).contains(&n) {
            e.push(format!("top_n must be within 1..={MAX_IDS}"));
        }
    }
}

fn check_zone(e: &mut Vec<String>, field: &str, tz: &str) {
    if Zone::parse(tz).is_none() {
        e.push(format!(
            "{field} `{tz}` is not one of America/New_York, Europe/Paris, UTC"
        ));
    }
}

/// Window kinds price local wall-clock instants: whole hours at best.
fn check_window_interval(e: &mut Vec<String>, kind: &str, iv: Interval) {
    if iv.ms() > HOUR_MS {
        e.push(format!(
            "interval {iv}: {kind} instants are local wall-clock times — use 1m, 5m, 15m or 1h"
        ));
    }
}

fn check_hm_on_grid(e: &mut Vec<String>, field: &str, hm: &str, iv: Interval) {
    match parse_hm(hm) {
        None => e.push(format!("{field} `{hm}` is not HH:MM (00:00-23:59)")),
        Some(m) if iv.ms() <= HOUR_MS && (i64::from(m) * MIN_MS) % iv.ms() != 0 => {
            e.push(format!("{field} `{hm}` is not on the {iv} bar grid"))
        }
        Some(_) => {}
    }
}

/// How a run's trades split into in-sample and holdout (module table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum SplitSpec {
    /// Holdout = decided at or after this instant (epoch ms).
    Time(i64),
    /// Holdout = trades on these ids.
    Instruments(Vec<String>),
}

impl SplitSpec {
    /// `time:<RFC 3339 | date | ms>` or `instruments:<id,id,…>`.
    pub fn parse(s: &str) -> Result<SplitSpec, String> {
        let s = s.trim();
        let bad = || format!("split `{s}` is time:<RFC 3339 | date | ms> or instruments:<id,id,…>");
        let (kind, rest) = s.split_once(':').ok_or_else(bad)?;
        match kind {
            "time" => parse_time(rest)
                .map(SplitSpec::Time)
                .map_err(|e| format!("split: {e}")),
            "instruments" => {
                let ids: Vec<String> = rest
                    .split(',')
                    .map(|x| x.trim().to_string())
                    .filter(|x| !x.is_empty())
                    .collect();
                let mut e = Vec::new();
                if ids.is_empty() {
                    e.push("split instruments: list at least one id".to_string());
                }
                check_ids(&mut e, "split instruments", &ids);
                if e.is_empty() {
                    Ok(SplitSpec::Instruments(ids))
                } else {
                    Err(e.join("; "))
                }
            }
            _ => Err(bad()),
        }
    }

    /// A trade decided at `decided_at_ms` on `instruments` (its legs) is in
    /// the holdout.
    pub fn is_holdout(&self, decided_at_ms: i64, instruments: &[&str]) -> bool {
        match self {
            SplitSpec::Time(t) => decided_at_ms >= *t,
            SplitSpec::Instruments(ids) => instruments.iter().any(|i| ids.iter().any(|x| x == i)),
        }
    }
}

impl fmt::Display for SplitSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SplitSpec::Time(t) => {
                let text = chrono::DateTime::from_timestamp_millis(*t).map_or_else(
                    || t.to_string(),
                    |d| {
                        if t.rem_euclid(1000) == 0 {
                            d.format("%Y-%m-%dT%H:%M:%SZ").to_string()
                        } else {
                            d.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
                        }
                    },
                );
                write!(f, "time:{text}")
            }
            SplitSpec::Instruments(ids) => write!(f, "instruments:{}", ids.join(",")),
        }
    }
}

impl TryFrom<String> for SplitSpec {
    type Error = String;
    fn try_from(s: String) -> Result<Self, String> {
        SplitSpec::parse(&s)
    }
}

impl From<SplitSpec> for String {
    fn from(s: SplitSpec) -> String {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TSLA: &str = "hyperliquid:xyz:TSLA";
    const NVDA: &str = "hyperliquid:xyz:NVDA";

    fn parse(v: Value) -> Result<StrategySpec, Vec<String>> {
        StrategySpec::from_value("s", &v)
    }

    fn errors(v: Value) -> String {
        parse(v).unwrap_err().join("\n")
    }

    /// One valid spec of every kind; each round-trips through `to_value`.
    fn every_kind() -> Vec<Value> {
        vec![
            json!({"kind": "weekend_window", "universe": "@xyz_stocks", "interval": "1h",
                   "calendar": "us_equity", "direction": "fade", "min_abs_signal_bps": 0,
                   "top_n": 4, "entry_offset_mins": -60}),
            json!({"kind": "daily_window", "universe": [TSLA, NVDA], "interval": "15m",
                   "days": "trading", "calendar": "us_equity", "tz": "America/New_York",
                   "anchor": "16:00", "entry": "20:00", "exit": "09:30", "direction": "follow"}),
            json!({"kind": "move_trigger", "universe": "hyperliquid:BTC", "interval": "5m",
                   "lookback_bars": 12, "threshold_bps": 150, "min_volume_ratio": 2,
                   "volume_baseline_bars": 288, "direction": "fade", "hold_bars": 24,
                   "take_profit_bps": 100, "stop_loss_bps": 80, "notional_usd": 25}),
            json!({"kind": "funding_carry", "universe": ["hyperliquid:SOL"], "interval": "1h",
                   "min_apr_pct": 30, "exit_apr_pct": 10, "hold_hours": 72,
                   "costs": {"taker_fee_bps": 4.5, "funding": true}}),
            json!({"kind": "pair_spread", "interval": "1h", "legs": ["hyperliquid:BTC", "hyperliquid:ETH"],
                   "lookback_bars": 48, "entry_z": 2, "exit_z": 0.5, "max_hold_bars": 72,
                   "exclude": [TSLA]}),
            json!({"kind": "event_window", "interval": "5m",
                   "events": [{"instrument": TSLA, "t": "2026-10-22T20:05:00Z", "label": "Q3"}],
                   "entry_delay_mins": 10, "direction": "follow", "min_abs_move_bps": 50,
                   "exit_at": "16:00", "tz": "America/New_York"}),
        ]
    }

    #[test]
    fn every_kind_parses_validates_and_round_trips() {
        for v in every_kind() {
            let spec = parse(v.clone()).unwrap_or_else(|e| panic!("{v}: {e:?}"));
            assert_eq!(spec.name, "s");
            assert!(spec.validation_errors().is_empty());
            let back = StrategySpec::from_value("s", &spec.to_value()).unwrap();
            assert_eq!(back, spec, "{}", spec.kind_name());
            assert!(KINDS.contains(&spec.kind_name()));
        }
        let s = parse(every_kind()[0].clone()).unwrap();
        assert_eq!(s.universe, Some(Universe::Named("@xyz_stocks".into())));
        assert_eq!(
            s.universe.as_ref().and_then(Universe::name),
            Some("xyz_stocks")
        );
        assert_eq!(s.period_kind(), PeriodKind::Weekend);
        assert_eq!(s.calendar(), Some("us_equity"));
        assert_eq!(s.notional(100.0), 100.0);
        let m = parse(every_kind()[2].clone()).unwrap();
        assert_eq!(
            m.universe,
            Some(Universe::Ids(vec!["hyperliquid:BTC".into()]))
        );
        assert_eq!(m.notional(100.0), 25.0);
        let StrategyKind::MoveTrigger(p) = &m.kind else {
            panic!()
        };
        assert_eq!(p.cooldown(), 24, "cooldown defaults to hold_bars");
        let d = parse(every_kind()[1].clone()).unwrap();
        assert_eq!(d.period_kind(), PeriodKind::TradingDay);
        let pair = parse(every_kind()[4].clone()).unwrap();
        assert_eq!(
            pair.named_instruments(),
            vec!["hyperliquid:BTC", "hyperliquid:ETH"]
        );
        assert_eq!(pair.period_kind(), PeriodKind::Day);
        assert!(m.named_instruments().is_empty());
    }

    /// The committed example (`docs/xlab-2026-10-01.md` § 5) from TOML.
    #[test]
    fn a_toml_strategy_table_parses() {
        let table: Value = toml::from_str(
            r#"
            kind = "weekend_window"
            universe = "@xyz_stocks"
            interval = "1h"
            calendar = "us_equity"
            direction = "fade"
            min_abs_signal_bps = 0
            "#,
        )
        .unwrap();
        let s = StrategySpec::from_value("weekend_fade", &table).unwrap();
        let StrategyKind::WeekendWindow(p) = &s.kind else {
            panic!()
        };
        assert_eq!((p.top_n, p.min_abs_signal_bps), (None, 0.0));
        assert_eq!(s.interval, Interval::H1);
    }

    #[test]
    fn unknown_fields_kinds_and_names_are_refused() {
        let e = errors(
            json!({"kind": "weekend_window", "universe": "@x", "interval": "1h",
                              "calendar": "us_equity", "direction": "fade", "topn": 3}),
        );
        assert!(
            e.contains("unknown field `topn`") && e.contains("common fields"),
            "{e}"
        );
        assert!(e.starts_with("strategy `s`: weekend_window:"), "{e}");
        let e = errors(json!({"kind": "grid_bot", "universe": "@x", "interval": "1h"}));
        assert!(
            e.contains("kind `grid_bot` is not one of weekend_window"),
            "{e}"
        );
        let e = errors(
            json!({"kind": "funding_carry", "universe": "@x", "interval": "2h",
                              "min_apr_pct": 10, "hold_hours": 1}),
        );
        assert!(e.contains("not one of 1m"), "{e}");
        let e = errors(json!([1, 2]));
        assert!(e.contains("a spec is an object"), "{e}");
        // The value's own name must match the key; with no key it is taken.
        let v = json!({"name": "other", "kind": "funding_carry", "universe": "@x",
                       "interval": "1h", "min_apr_pct": 10, "hold_hours": 1});
        let e = errors(v.clone());
        assert!(
            e.contains("name `other` differs from the strategy's key `s`"),
            "{e}"
        );
        assert_eq!(StrategySpec::from_value("", &v).unwrap().name, "other");
        let e = StrategySpec::from_value(
            "Bad-Name",
            &json!({"kind": "funding_carry",
            "universe": "@x", "interval": "1h", "min_apr_pct": 10, "hold_hours": 1}),
        )
        .unwrap_err()
        .join("\n");
        assert!(e.contains("name `Bad-Name`"), "{e}");
    }

    /// Bounds: every message names its field.
    #[test]
    fn bounds_name_their_field() {
        let cases: Vec<(Value, &[&str])> = vec![
            (
                json!({"kind": "weekend_window", "universe": ["hyperliquid:xyz:TSLA", "hyperliquid:xyz:TSLA"],
                       "interval": "4h", "calendar": " ", "direction": "fade",
                       "min_abs_signal_bps": -1, "top_n": 0, "entry_offset_mins": 7,
                       "exit_offset_mins": 5000, "notional_usd": 0}),
                &[
                    "is listed twice",
                    "interval 4h",
                    "calendar:",
                    "min_abs_signal_bps",
                    "top_n",
                    "entry_offset_mins = 7 is not a whole number of 4h bars",
                    "exit_offset_mins must be within",
                    "notional_usd",
                ],
            ),
            (
                json!({"kind": "daily_window", "universe": "@x", "interval": "1h", "days": "trading",
                       "tz": "Asia/Almaty", "anchor": "16:00", "entry": "20:30", "exit": "25:00",
                       "direction": "fade"}),
                &[
                    "calendar is required",
                    "tz `Asia/Almaty`",
                    "entry `20:30` is not on the 1h bar grid",
                    "exit `25:00` is not HH:MM",
                ],
            ),
            (
                json!({"kind": "daily_window", "universe": "@x", "interval": "1h", "days": "all",
                       "calendar": "us_equity", "tz": "UTC", "anchor": "00:00", "entry": "01:00",
                       "exit": "02:00", "direction": "fade"}),
                &["calendar is read only"],
            ),
            (
                json!({"kind": "move_trigger", "universe": "@x", "interval": "1h", "lookback_bars": 0,
                       "threshold_bps": -5, "min_volume_ratio": 2, "direction": "fade",
                       "hold_bars": 20000, "cooldown_bars": 10001, "stop_loss_bps": 0}),
                &[
                    "lookback_bars must be within 1..=10000",
                    "threshold_bps",
                    "volume_baseline_bars is required",
                    "hold_bars must be within 1..=10000",
                    "cooldown_bars must be within 0..=10000",
                    "stop_loss_bps must be finite, > 0",
                ],
            ),
            (
                json!({"kind": "funding_carry", "universe": "@x", "interval": "1h", "min_apr_pct": 10,
                       "exit_apr_pct": 10, "hold_hours": 0}),
                &[
                    "exit_apr_pct must be finite, ≥ 0 and < min_apr_pct",
                    "hold_hours must be within 1..=8760",
                ],
            ),
            (
                json!({"kind": "pair_spread", "universe": "@x", "interval": "1h",
                       "legs": ["hyperliquid:BTC", "hyperliquid:BTC", "nowhere:X"],
                       "lookback_bars": 1, "entry_z": 0, "exit_z": 1, "max_hold_bars": 0}),
                &[
                    "universe: pair_spread trades the instruments it names (`legs`)",
                    "legs lists 3 ids",
                    "`hyperliquid:BTC` is listed twice",
                    "unknown venue `nowhere`",
                    "lookback_bars must be within 2..=10000",
                    "entry_z",
                    "exit_z",
                    "max_hold_bars",
                ],
            ),
            (
                json!({"kind": "event_window", "interval": "5m",
                       "events": [{"instrument": "TSLA", "t": "yesterday", "label": "x".repeat(121)}],
                       "entry_delay_mins": 20000, "direction": "fade", "exit_after_mins": 5,
                       "exit_at": "16:00"}),
                &[
                    "events[0].instrument",
                    "events[0].t `yesterday` is not RFC 3339",
                    "events[0].label is longer",
                    "entry_delay_mins",
                    "not both",
                ],
            ),
            (
                json!({"kind": "event_window", "interval": "5m",
                       "events": [{"instrument": TSLA, "t": "2026-10-22T20:05:00Z"}],
                       "direction": "fade", "exit_at": "16:00"}),
                &["tz is required with exit_at"],
            ),
            (
                json!({"kind": "funding_carry", "interval": "1h", "min_apr_pct": 10, "hold_hours": 1,
                       "costs": {"taker_fee_bps": -1}}),
                &[
                    "universe is required for funding_carry",
                    "costs.taker_fee_bps",
                ],
            ),
        ];
        for (v, wants) in cases {
            let e = errors(v.clone());
            for want in wants {
                assert!(e.contains(want), "want `{want}` for {v}:\n{e}");
            }
        }
        let e = errors(
            json!({"kind": "funding_carry", "universe": "@Bad", "interval": "1h",
                              "min_apr_pct": 10, "hold_hours": 1}),
        );
        assert!(e.contains("universe `@Bad`"), "{e}");
        // A long event list reports 20 problems and the count of the rest.
        let events: Vec<Value> = (0..25)
            .map(|_| json!({"instrument": TSLA, "t": "never"}))
            .collect();
        let e = parse(
            json!({"kind": "event_window", "interval": "1h", "events": events,
                             "direction": "fade", "exit_after_mins": 60}),
        )
        .unwrap_err();
        assert_eq!(e.len(), 21, "{e:?}");
        assert!(e[20].ends_with("… and 5 more problems"), "{e:?}");
    }

    #[test]
    fn directions_pick_sides() {
        assert_eq!(Direction::Fade.side(12.0), Some(Side::Sell));
        assert_eq!(Direction::Fade.side(-12.0), Some(Side::Buy));
        assert_eq!(Direction::Follow.side(12.0), Some(Side::Buy));
        assert_eq!(Direction::Follow.side(-0.5), Some(Side::Sell));
        assert_eq!(Direction::Follow.side(0.0), None);
        assert_eq!(Direction::Fade.side(f64::NAN), None);
        assert_eq!(Direction::Follow.as_str(), "follow");
    }

    #[test]
    fn splits_parse_and_assign_halves() {
        let t = SplitSpec::parse("time:2026-07-01T00:00:00Z").unwrap();
        assert_eq!(t, SplitSpec::parse("time:2026-07-01").unwrap());
        let at = parse_time("2026-07-01").unwrap();
        assert!(t.is_holdout(at, &[TSLA]) && !t.is_holdout(at - 1, &[TSLA]));
        assert_eq!(t.to_string(), "time:2026-07-01T00:00:00Z");
        let ms = SplitSpec::parse("time:1782864000123").unwrap();
        assert_eq!(SplitSpec::parse(&ms.to_string()).unwrap(), ms, "ms survive");
        let i = SplitSpec::parse(&format!("instruments:{TSLA}, {NVDA}")).unwrap();
        assert_eq!(i, SplitSpec::Instruments(vec![TSLA.into(), NVDA.into()]));
        assert!(i.is_holdout(0, &["hyperliquid:BTC", NVDA]));
        assert!(!i.is_holdout(0, &["hyperliquid:xyz:TSL"]), "full ids only");
        assert_eq!(i.to_string(), format!("instruments:{TSLA},{NVDA}"));
        let json = serde_json::to_string(&i).unwrap();
        assert_eq!(serde_json::from_str::<SplitSpec>(&json).unwrap(), i);
        for bad in [
            "holdout:x",
            "time:friday",
            "instruments:",
            "instruments:TSLA",
            "nothing",
        ] {
            assert!(SplitSpec::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn data_ranges_reach_back_for_features_and_forward_to_exits() {
        let from = parse_time("2026-09-01").unwrap();
        let to = parse_time("2026-10-01").unwrap();
        let m = parse(every_kind()[2].clone()).unwrap();
        let (lo, hi) = m.data_range(from, to);
        assert_eq!(
            from - lo,
            8 * DAY_MS + 2 * 300_000,
            "features dominate a 5m lookback"
        );
        assert_eq!(hi - to, 24 * 300_000 + 300_000);
        let f = parse(every_kind()[3].clone()).unwrap();
        assert_eq!(f.data_range(from, to).1 - to, 72 * HOUR_MS + HOUR_MS);
        let w = parse(every_kind()[0].clone()).unwrap();
        assert!(w.data_range(from, to).1 - to >= 2 * DAY_MS);
    }
}
