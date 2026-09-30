//! Pre-trade risk gate (`risk-gate-domain`, PRD §28, tracker conventions
//! 9–10): every rule is a [`Check`], any missing input fails closed. Pure —
//! the exec tool (`risk-gate-enforcement`) builds the [`RiskContext`] inside
//! the ledger transaction (`ports/paper.rs`), calls [`evaluate`] and writes
//! the verdict. Limits come from `[risk]` through `RiskConfig::limits`
//! (`config/risk.rs`).
//!
//! | Class | When | Gated by |
//! |---|---|---|
//! | exit | `reduce_only` and it reduces the open position without flipping | `intent`, `account`, `reduce_only`, `order_rate`, `open_orders`; `kill_switch`, `halted`, `book_age`, `ctx_age` are waived when `allow_reduce_degraded` (§ 7 #7: verdict `allow_reduce_degraded`) |
//! | entry | everything else — a `reduce_only` order that would open or flip is denied `reduce_only` | every rule |
//!
//! | Rule (code) | An entry passes when | Exit |
//! |---|---|---|
//! | `intent` | qty and notional finite > 0; `<venue>:<native>` ids; underlying set and equal to the open position's; hedge ≠ instrument | same |
//! | `account` | `intent.account` = `limits.account` | same |
//! | `kill_switch` | `kill_switch_file` absent; present ⇒ trips `file` | waived |
//! | `halted` | no persisted halt (`risk_state`) | waived |
//! | `reduce_only` | (flagged orders) reduces without flipping | pass |
//! | `venue` · `instrument` | venue in `venues`; id in `instruments_allow`, not in `instruments_deny` (M0 permission, convention 10) | pass: an open position can always be closed |
//! | `lifecycle` | §29 state ≥ `min_lifecycle`; only when the limits set one (M1 — M0 maps it to none) | skipped |
//! | `daily_loss` · `total_loss` | day-start equity − equity ≤ limit · initial cash − equity ≤ limit; a breach trips the halt (exits too) | not gated |
//! | `order_rate` · `open_orders` | accepted orders in the last 60 s + 1 ≤ max · resting orders + 1 ≤ max | same |
//! | `book_age` · `ctx_age` | row age ≤ `max_data_age_ms.book` / `.ctx`; a book is as old as the older of its row and venue time | waived |
//! | `market_status` | `mkt_ctx` listed with a book, not halted; at the OI cap only if the position does not grow; the growth ≤ `oi_cap_usd − oi_usd` when the row has both | skipped |
//! | `min_edge` | `edge_after_costs_bps` of the `opportunity_key` row — its key names the order's instrument as whole `:` segments, any schema with that feature, within the row TTL — ≥ `min_edge_bps` | skipped |
//! | `depth` · `slippage` | taker-side depth within `max_slippage_bps` of mid ≥ `min_depth_usd` · the walk of `notional_usd` fills fully, VWAP ≤ `max_slippage_bps` from mid | skipped |
//! | `order_notional` · `position_notional` · `asset_exposure` · `venue_exposure` · `gross_exposure` · `net_exposure` | after the fill ≤ cap: the order · \|position\| · \|net per underlying\| · gross per venue · gross · \|net\| | skipped |
//! | `leverage` | gross after / equity ≤ `max_leverage`; equity ≤ 0 fails | skipped |
//! | `hedge` · `skew` | `require_hedge_for` strategies: the hedge leg is permitted, fresh, open, deep enough, and the leg books are ≤ `max_skew_ms` apart | skipped |
//!
//! | Detail | Rule |
//! |---|---|
//! | Missing input (`Absent` / `Error`) | the check fails `missing:<field>`: `kill_switch`, `mark`, `equity`, `day_start_equity`, `book`, `ctx`, `edge_after_costs_bps`, `lifecycle`, `hedge_book`, `hedge_ctx` |
//! | After-fill values | existing positions and the order's delta at mark; a new position at the order leg's book mid (no book: `notional_usd / qty`); `order_notional` takes the larger of `notional_usd` and qty × that price — an intent never understates its size |
//! | Comparison | 1e-12 relative tolerance: at the limit passes, limit + 1e-6 fails |
//! | Verdict `rule` | the first failing check in table order; else `allow_reduce_degraded` when a check was waived; else `ok` |
//! | `trips` | halts the state should record (`risk_state.rs` keeps them): `file`, `total_loss`, `daily_loss` |
//! | Ids | full, in every detail (`hyperliquid:xyz:TSLA`) — never shortened |

// Consumers land in wave W1 (`risk-paper-ledger-store`, `risk-kill-switch`,
// `risk-gate-enforcement`).
#![allow(dead_code)]

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::domain::book::{depth_within, L2Book, Side, WalkTarget};
use crate::domain::market::{Listing, MarketCtx};
use crate::domain::observation::{ErrorClass, Field, ObsStatus, Observation, ReadError};
use crate::domain::xm::ledger::{Exposure, GroupExposure, PaperPositions};

/// Relative tolerance of every limit comparison (f64 residue of summed
/// notionals): a value at the limit passes.
const LIMIT_TOL: f64 = 1e-12;

/// Rule codes (verdict `rule`, `Check::rule`).
pub mod rules {
    pub const OK: &str = "ok";
    pub const ALLOW_REDUCE_DEGRADED: &str = "allow_reduce_degraded";
    pub const INTENT: &str = "intent";
    pub const ACCOUNT: &str = "account";
    pub const KILL_SWITCH: &str = "kill_switch";
    pub const HALTED: &str = "halted";
    pub const REDUCE_ONLY: &str = "reduce_only";
    pub const VENUE: &str = "venue";
    pub const INSTRUMENT: &str = "instrument";
    pub const LIFECYCLE: &str = "lifecycle";
    pub const DAILY_LOSS: &str = "daily_loss";
    pub const TOTAL_LOSS: &str = "total_loss";
    pub const ORDER_RATE: &str = "order_rate";
    pub const OPEN_ORDERS: &str = "open_orders";
    pub const BOOK_AGE: &str = "book_age";
    pub const CTX_AGE: &str = "ctx_age";
    pub const MARKET_STATUS: &str = "market_status";
    pub const MIN_EDGE: &str = "min_edge";
    pub const DEPTH: &str = "depth";
    pub const SLIPPAGE: &str = "slippage";
    pub const ORDER_NOTIONAL: &str = "order_notional";
    pub const POSITION_NOTIONAL: &str = "position_notional";
    pub const ASSET_EXPOSURE: &str = "asset_exposure";
    pub const VENUE_EXPOSURE: &str = "venue_exposure";
    pub const GROSS_EXPOSURE: &str = "gross_exposure";
    pub const NET_EXPOSURE: &str = "net_exposure";
    pub const LEVERAGE: &str = "leverage";
    pub const HEDGE: &str = "hedge";
    pub const SKEW: &str = "skew";
    /// Prefix of every missing-input code (`missing:<field>`).
    pub const MISSING: &str = "missing:";
}

/// Feature an opportunity row carries (`xm_compare/1`, a strategy row).
pub const EDGE_FEATURE: &str = "edge_after_costs_bps";

/// §29 lifecycle states, in order (`kg-lifecycle` owns the transitions;
/// `config::risk` re-exports this as the `min_lifecycle` type).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    Discovered,
    Identified,
    Mapped,
    Observed,
    Validated,
    PaperTradable,
    LiveApproved,
}

impl Lifecycle {
    pub fn as_str(self) -> &'static str {
        match self {
            Lifecycle::Discovered => "discovered",
            Lifecycle::Identified => "identified",
            Lifecycle::Mapped => "mapped",
            Lifecycle::Observed => "observed",
            Lifecycle::Validated => "validated",
            Lifecycle::PaperTradable => "paper_tradable",
            Lifecycle::LiveApproved => "live_approved",
        }
    }
}

/// `[risk] max_data_age_ms`, milliseconds per input kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaxAges {
    pub book: u64,
    pub ctx: u64,
    pub reference: u64,
    pub quote: u64,
}

/// The gate's limits — `[risk]` mapped by `RiskConfig::limits`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RiskLimits {
    pub account: String,
    pub venues: Vec<String>,
    /// `None` = no lifecycle rule (M0: permission is the allow-list only).
    pub min_lifecycle: Option<Lifecycle>,
    pub instruments_allow: Vec<String>,
    pub instruments_deny: Vec<String>,
    pub max_order_notional_usd: f64,
    pub max_position_notional_usd: f64,
    pub max_asset_exposure_usd: f64,
    pub max_venue_exposure_usd: f64,
    pub max_gross_exposure_usd: f64,
    pub max_net_exposure_usd: f64,
    pub max_leverage: f64,
    pub daily_loss_limit_usd: f64,
    pub total_loss_limit_usd: f64,
    pub min_edge_bps: f64,
    pub max_slippage_bps: f64,
    pub min_depth_usd: f64,
    pub require_hedge_for: Vec<String>,
    pub max_data_age_ms: MaxAges,
    pub max_skew_ms: u64,
    pub max_orders_per_min: u32,
    pub max_open_orders: u32,
    pub allow_reduce_degraded: bool,
}

/// Why an account is halted (§ 7 #8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HaltReason {
    /// `daily_loss_limit_usd` breached; clears at 00:00 UTC.
    DailyLoss,
    /// `total_loss_limit_usd` breached; clears only by `tengu risk resume`.
    TotalLoss,
    /// `tengu risk halt`; clears only by `tengu risk resume`.
    Operator,
    /// `kill_switch_file` seen; clears only by `tengu risk resume` (file gone).
    File,
}

impl HaltReason {
    pub fn as_str(self) -> &'static str {
        match self {
            HaltReason::DailyLoss => "daily_loss",
            HaltReason::TotalLoss => "total_loss",
            HaltReason::Operator => "operator",
            HaltReason::File => "file",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "daily_loss" => Some(HaltReason::DailyLoss),
            "total_loss" => Some(HaltReason::TotalLoss),
            "operator" => Some(HaltReason::Operator),
            "file" => Some(HaltReason::File),
            _ => None,
        }
    }

    /// Only `tengu risk resume` clears it (everything but the daily loss).
    pub fn is_sticky(self) -> bool {
        self != HaltReason::DailyLoss
    }
}

/// A persisted halt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Halt {
    pub reason: HaltReason,
    pub since_ms: i64,
}

/// One order as the gate sees it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderIntent {
    /// Ledger account (`[risk] account`).
    pub account: String,
    /// Full instrument id (`hyperliquid:xyz:TSLA`).
    pub instrument: String,
    /// Full underlying id the asset exposure nets across venues.
    pub underlying: String,
    pub side: Side,
    /// Base quantity, > 0.
    pub qty: f64,
    /// USD notional (requested, or the caller's book-walk estimate), > 0.
    pub notional_usd: f64,
    pub reduce_only: bool,
    /// §21 opportunity type (`[risk] require_hedge_for` names).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy: Option<String>,
    /// Full id of the hedge leg.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hedge_instrument: Option<String>,
    /// Observation key of the row carrying `edge_after_costs_bps`
    /// (`xm_compare/1:<a>:<b>`, a strategy row).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opportunity_key: Option<String>,
}

impl OrderIntent {
    /// `hyperliquid` of `hyperliquid:xyz:TSLA`; `None` for a malformed id.
    pub fn venue(&self) -> Option<&str> {
        venue_of(&self.instrument)
    }
}

/// Venue part of a full instrument id.
pub fn venue_of(id: &str) -> Option<&str> {
    match id.split_once(':') {
        Some((venue, native)) if !venue.is_empty() && !native.is_empty() => Some(venue),
        _ => None,
    }
}

/// Entry or exit (module table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderClass {
    Entry,
    Exit,
}

impl OrderClass {
    pub fn as_str(self) -> &'static str {
        match self {
            OrderClass::Entry => "entry",
            OrderClass::Exit => "exit",
        }
    }
}

/// Tradability of a market from its `mkt_ctx/1` row — the gate's view.
/// The fill engine's `xm::paper::MarketStatus` is the venue session state
/// the fill is simulated in (the exec tool maps this one onto it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarketStatus {
    Open,
    /// At the venue's open-interest cap: orders that grow OI are rejected.
    AtOiCap,
    /// Listed without a book (HL: null `midPx` / `impactPxs`).
    NoBook,
    /// Trading halted (a `us_halt/1` row, M2).
    Halted,
    /// Delisted, or unknown to the venue.
    Delisted,
}

impl MarketStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            MarketStatus::Open => "open",
            MarketStatus::AtOiCap => "at_oi_cap",
            MarketStatus::NoBook => "no_book",
            MarketStatus::Halted => "halted",
            MarketStatus::Delisted => "delisted",
        }
    }

    pub fn of_ctx(ctx: &MarketCtx) -> Self {
        if ctx.listing != Listing::Listed {
            MarketStatus::Delisted
        } else if ctx.no_book {
            MarketStatus::NoBook
        } else if ctx.at_oi_cap == Some(true) {
            MarketStatus::AtOiCap
        } else {
            MarketStatus::Open
        }
    }
}

/// A book row (`hl_book/1:<id>`): when it was read and the book.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BookInput {
    pub key: String,
    pub observed_at_ms: i64,
    pub book: L2Book,
}

impl BookInput {
    /// Age by the older of the row time and the venue time (saturating).
    pub fn age_ms(&self, now_ms: i64) -> u64 {
        let at = self.observed_at_ms.min(self.book.venue_ts_ms);
        now_ms.saturating_sub(at).max(0) as u64
    }
}

/// A context row (`mkt_ctx/1:<id>`): when it was read and the market status.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CtxInput {
    pub key: String,
    pub observed_at_ms: i64,
    pub status: MarketStatus,
    /// `oi_cap_usd − oi_usd` when the row carries both; `None` = no cap known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oi_cap_headroom_usd: Option<f64>,
}

impl CtxInput {
    pub fn age_ms(&self, now_ms: i64) -> u64 {
        now_ms.saturating_sub(self.observed_at_ms).max(0) as u64
    }

    /// From a stored `mkt_ctx/1` row: `Error` for an error row or one whose
    /// data does not decode as `MarketCtx`.
    pub fn from_row(row: &Observation) -> Field<CtxInput> {
        if row.status == ObsStatus::Error {
            return Field::err(first_error(row, "ctx"));
        }
        match row.typed::<MarketCtx>() {
            Ok(ctx) => Field::ok(CtxInput {
                key: row.key.clone(),
                observed_at_ms: row.observed_at_ms,
                status: MarketStatus::of_ctx(&ctx),
                oi_cap_headroom_usd: ctx
                    .oi_cap_usd
                    .filter(|c| c.is_finite() && *c > 0.0)
                    .zip(ctx.oi_usd())
                    .map(|(cap, oi)| cap - oi),
            }),
            Err(e) => Field::err(ReadError::new("ctx", ErrorClass::Decode, e.to_string())),
        }
    }
}

/// Market inputs of one leg.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LegMarket {
    pub book: Field<BookInput>,
    pub ctx: Field<CtxInput>,
}

/// The opportunity row an entry names.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EdgeInput {
    pub key: String,
    pub observed_at_ms: i64,
    /// The row's TTL: older rows are stale (missing).
    pub ttl_ms: u64,
    pub edge_after_costs_bps: Field<f64>,
}

impl EdgeInput {
    /// From the stored row at `key` (`None` = no row ⇒ `Absent`). An error
    /// row ⇒ `Error`; no finite `edge_after_costs_bps` feature ⇒ the edge
    /// is `Absent` / `Error`.
    pub fn from_row(key: &str, row: Option<&Observation>) -> Field<EdgeInput> {
        let Some(row) = row else {
            return Field::Absent;
        };
        if row.key != key {
            return Field::err(ReadError::new(
                EDGE_FEATURE,
                ErrorClass::Decode,
                format!("row {} is not {key}", row.key),
            ));
        }
        if row.status == ObsStatus::Error {
            return Field::err(first_error(row, EDGE_FEATURE));
        }
        let edge = match row.features.get(EDGE_FEATURE) {
            None | Some(Value::Null) => Field::Absent,
            Some(v) => match v.as_f64().filter(|x| x.is_finite()) {
                Some(x) => Field::ok(x),
                None => Field::err(ReadError::new(
                    EDGE_FEATURE,
                    ErrorClass::Decode,
                    format!("{EDGE_FEATURE} is {v}, not a number"),
                )),
            },
        };
        Field::ok(EdgeInput {
            key: key.to_string(),
            observed_at_ms: row.observed_at_ms,
            ttl_ms: row.ttl_ms,
            edge_after_costs_bps: edge,
        })
    }
}

fn first_error(row: &Observation, field: &str) -> ReadError {
    row.errors.first().cloned().unwrap_or_else(|| {
        ReadError::new(
            field,
            ErrorClass::Transient,
            format!("row {} has status error", row.key),
        )
    })
}

/// Everything the gate reads, assembled by the caller inside the ledger
/// transaction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RiskContext {
    /// The account at fresh marks (`PaperPositions::build` with
    /// `max_data_age_ms.ctx` and the risk state's day-start equity).
    pub account: PaperPositions,
    /// Persisted halt (`risk_state`); `None` = trading.
    pub halt: Option<Halt>,
    /// `kill_switch_file` present? `Error` = could not tell (fails closed).
    pub kill_switch: Field<bool>,
    /// §29 state of the order's instrument; read only when the limits set
    /// `min_lifecycle`.
    pub lifecycle: Field<Lifecycle>,
    /// Orders accepted in the last 60 s.
    pub orders_last_min: u32,
    /// Resting orders.
    pub open_orders: u32,
    /// Market inputs by full instrument id (order leg, hedge leg).
    pub legs: BTreeMap<String, LegMarket>,
    /// The row `OrderIntent::opportunity_key` names.
    pub opportunity: Field<EdgeInput>,
}

impl RiskContext {
    /// Audit digest: row keys, ages and values the gate used (no books).
    pub fn digest(&self, now_ms: i64) -> Value {
        let a = &self.account;
        let legs: serde_json::Map<String, Value> = self
            .legs
            .iter()
            .map(|(id, leg)| {
                let book = match &leg.book {
                    Field::Ok { value: b } => json!({
                        "key": b.key,
                        "observed_at_ms": b.observed_at_ms,
                        "venue_ts_ms": b.book.venue_ts_ms,
                        "age_ms": b.age_ms(now_ms),
                        "best_bid": b.book.best_bid(),
                        "best_ask": b.book.best_ask(),
                        "mid": b.book.mid(),
                    }),
                    other => json!(other),
                };
                let ctx = match &leg.ctx {
                    Field::Ok { value: c } => json!({
                        "key": c.key,
                        "observed_at_ms": c.observed_at_ms,
                        "age_ms": c.age_ms(now_ms),
                        "status": c.status,
                    }),
                    other => json!(other),
                };
                (id.clone(), json!({"book": book, "ctx": ctx}))
            })
            .collect();
        let opportunity = match &self.opportunity {
            Field::Ok { value: o } => json!({
                "key": o.key,
                "observed_at_ms": o.observed_at_ms,
                "age_ms": now_ms.saturating_sub(o.observed_at_ms).max(0),
                "ttl_ms": o.ttl_ms,
                "edge_after_costs_bps": o.edge_after_costs_bps,
            }),
            other => json!(other),
        };
        let marks: Vec<Value> = a
            .positions
            .iter()
            .map(|p| json!({"instrument": p.instrument, "qty": p.qty, "mark_px": p.mark_px}))
            .collect();
        json!({
            "now_ms": now_ms,
            "account": {
                "account": a.account,
                "cash_usd": a.cash_usd,
                "equity_usd": a.equity_usd,
                "daily_pnl_usd": a.daily_pnl_usd,
                "exposure": a.exposure,
                "marks": marks,
            },
            "halt": self.halt,
            "kill_switch": self.kill_switch,
            "lifecycle": self.lifecycle,
            "orders_last_min": self.orders_last_min,
            "open_orders": self.open_orders,
            "legs": legs,
            "opportunity": opportunity,
        })
    }
}

/// Outcome of one check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Pass,
    Fail,
    /// Failed, but waived for a reduce-only exit (`allow_reduce_degraded`).
    Waived,
    /// Not applicable to this order.
    Skipped,
}

/// One rule applied to one order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Check {
    /// The rule code, or `missing:<field>` when an input was absent.
    pub rule: String,
    pub status: CheckStatus,
    /// Values vs limits, full ids.
    pub detail: String,
}

/// Room left under each limit after this order (limit − value; negative =
/// over). `None` = not computed (missing input or not applicable).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Headroom {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub venue_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gross_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leverage: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily_loss_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_loss_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orders_per_min: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_orders: Option<i64>,
}

/// The gate's answer for one order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RiskVerdict {
    pub allow: bool,
    /// First failing check's rule; else `allow_reduce_degraded` / `ok`.
    pub rule: String,
    pub class: OrderClass,
    /// Allowed with waived checks (§ 7 #7).
    pub degraded: bool,
    pub checks: Vec<Check>,
    pub headroom: Headroom,
    /// Halts to record (kill-switch file, loss breaches).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trips: Vec<HaltReason>,
}

impl RiskVerdict {
    /// The first failing check.
    pub fn failed(&self) -> Option<&Check> {
        self.checks.iter().find(|c| c.status == CheckStatus::Fail)
    }

    pub fn check(&self, rule: &str) -> Option<&Check> {
        self.checks.iter().find(|c| c.rule == rule)
    }
}

/// Loss since the day start and since inception, from the valued account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LossState {
    /// Day-start equity − equity (negative = profit).
    pub daily_loss_usd: Field<f64>,
    /// Initial cash − equity.
    pub total_loss_usd: Field<f64>,
}

impl LossState {
    pub fn of(account: &PaperPositions) -> Self {
        let daily_loss_usd = match &account.daily_pnl_usd {
            Field::Ok { value } => Field::ok(-value),
            other => other.clone(),
        };
        let total_loss_usd = match &account.equity_usd {
            Field::Ok { value } => Field::ok(account.initial_cash_usd - value),
            other => other.clone(),
        };
        Self {
            daily_loss_usd,
            total_loss_usd,
        }
    }

    /// Smallest room under the daily and total loss limits; `None` when
    /// either loss is unknown.
    pub fn headroom_usd(&self, limits: &RiskLimits) -> Option<f64> {
        let d = limits.daily_loss_limit_usd - self.daily_loss_usd.value()?;
        let t = limits.total_loss_limit_usd - self.total_loss_usd.value()?;
        Some(d.min(t))
    }

    /// Halts the current losses call for (total first).
    pub fn breaches(&self, limits: &RiskLimits) -> Vec<HaltReason> {
        let mut out = Vec::new();
        if self
            .total_loss_usd
            .value()
            .is_some_and(|l| !within_max(*l, limits.total_loss_limit_usd))
        {
            out.push(HaltReason::TotalLoss);
        }
        if self
            .daily_loss_usd
            .value()
            .is_some_and(|l| !within_max(*l, limits.daily_loss_limit_usd))
        {
            out.push(HaltReason::DailyLoss);
        }
        out
    }
}

/// `value ≤ limit` with the relative tolerance.
fn within_max(value: f64, limit: f64) -> bool {
    value.is_finite() && value <= limit + LIMIT_TOL * limit.abs().max(1.0)
}

/// `value ≥ min` with the relative tolerance.
fn at_least(value: f64, min: f64) -> bool {
    value.is_finite() && value >= min - LIMIT_TOL * min.abs().max(1.0)
}

fn missing(field: &str) -> String {
    format!("{}{field}", rules::MISSING)
}

/// `missing:<field>` for a failed input, the field up to its first `:`
/// (`mark:hyperliquid:xyz:TSLA` ⇒ `missing:mark`).
fn missing_of(e: &ReadError) -> String {
    missing(e.field.split(':').next().unwrap_or("input"))
}

fn describe(e: &ReadError) -> String {
    format!("{} {} {}", e.field, e.class.as_str(), e.message)
}

/// The failed read behind a non-`Ok` field (`Absent` ⇒ a named "absent").
fn read_error<T>(field: &Field<T>, name: &str) -> ReadError {
    match field {
        Field::Error { error } => error.clone(),
        _ => ReadError::new(name, ErrorClass::Transient, "absent"),
    }
}

/// Order values after the fill (module table).
struct After {
    qty_before: f64,
    qty_after: f64,
    /// Signed notional at the reference price.
    pos_before: f64,
    pos_after: f64,
    delta: f64,
}

struct Gate<'a> {
    intent: &'a OrderIntent,
    ctx: &'a RiskContext,
    limits: &'a RiskLimits,
    now_ms: i64,
    class: OrderClass,
    /// USD size the liquidity and cap rules judge: the larger of
    /// `notional_usd` and qty at the reference price (module table).
    order_usd: f64,
    checks: Vec<Check>,
    headroom: Headroom,
    trips: Vec<HaltReason>,
}

impl Gate<'_> {
    fn push(&mut self, rule: impl Into<String>, status: CheckStatus, detail: impl Into<String>) {
        self.checks.push(Check {
            rule: rule.into(),
            status,
            detail: detail.into(),
        });
    }

    fn pass(&mut self, rule: &str, detail: impl Into<String>) {
        self.push(rule, CheckStatus::Pass, detail);
    }

    fn fail(&mut self, rule: impl Into<String>, detail: impl Into<String>) {
        self.push(rule, CheckStatus::Fail, detail);
    }

    fn skip(&mut self, rule: &str, detail: impl Into<String>) {
        self.push(rule, CheckStatus::Skipped, detail);
    }

    fn is_exit(&self) -> bool {
        self.class == OrderClass::Exit
    }

    /// A failure an exit may pass with `allow_reduce_degraded`.
    fn degradable(&mut self, rule: impl Into<String>, detail: impl Into<String>) {
        if self.is_exit() && self.limits.allow_reduce_degraded {
            let detail = format!("{} (exit: allow_reduce_degraded)", detail.into());
            self.push(rule, CheckStatus::Waived, detail);
        } else {
            self.fail(rule, detail);
        }
    }

    fn row(&self) -> Option<&crate::domain::xm::ledger::PositionRow> {
        self.ctx
            .account
            .positions
            .iter()
            .find(|r| r.instrument == self.intent.instrument)
    }

    fn leg(&self, id: &str) -> Option<&LegMarket> {
        self.ctx.legs.get(id)
    }

    // ── checks ──────────────────────────────────────────────────

    fn check_intent(&mut self) {
        let i = self.intent;
        let mut problems = Vec::new();
        if i.account.trim().is_empty() {
            problems.push("no account".to_string());
        }
        if i.venue().is_none() {
            problems.push(format!(
                "instrument `{}` is not a full `<venue>:<native id>`",
                i.instrument
            ));
        }
        if i.underlying.trim().is_empty() {
            problems.push("no underlying".to_string());
        }
        if !(i.qty.is_finite() && i.qty > 0.0) {
            problems.push(format!("qty {} is not > 0", i.qty));
        }
        if !(i.notional_usd.is_finite() && i.notional_usd > 0.0) {
            problems.push(format!("notional_usd {} is not > 0", i.notional_usd));
        }
        if let Some(h) = &i.hedge_instrument {
            if venue_of(h).is_none() {
                problems.push(format!(
                    "hedge_instrument `{h}` is not a full `<venue>:<native id>`"
                ));
            }
            if *h == i.instrument {
                problems.push(format!("hedge_instrument is the instrument {h}"));
            }
        }
        if let Some(row) = self.row() {
            if row.underlying != i.underlying {
                problems.push(format!(
                    "underlying {} differs from the open position's {} on {}",
                    i.underlying, row.underlying, i.instrument
                ));
            }
        }
        if problems.is_empty() {
            let detail = format!(
                "{} {} qty {} notional {} USD on {}",
                i.class_word(),
                i.side.as_str(),
                i.qty,
                i.notional_usd,
                i.instrument
            );
            self.pass(rules::INTENT, detail);
        } else {
            self.fail(rules::INTENT, problems.join("; "));
        }
    }

    fn check_account(&mut self) {
        if self.intent.account == self.limits.account {
            self.pass(rules::ACCOUNT, format!("account {}", self.intent.account));
        } else {
            self.fail(
                rules::ACCOUNT,
                format!(
                    "order for account {}, limits are for {}",
                    self.intent.account, self.limits.account
                ),
            );
        }
    }

    fn check_kill_switch(&mut self) {
        let ctx = self.ctx;
        match &ctx.kill_switch {
            Field::Ok { value: false } => self.pass(rules::KILL_SWITCH, "kill-switch file absent"),
            Field::Ok { value: true } => {
                self.trips.push(HaltReason::File);
                self.degradable(rules::KILL_SWITCH, "kill-switch file present");
            }
            other => {
                let e = read_error(other, "kill_switch");
                self.degradable(
                    missing("kill_switch"),
                    format!("kill-switch file state unknown: {}", describe(&e)),
                );
            }
        }
    }

    fn check_halted(&mut self) {
        let ctx = self.ctx;
        match &ctx.halt {
            None => self.pass(rules::HALTED, "not halted"),
            Some(h) => {
                let detail = format!("halted: {} since {} ms", h.reason.as_str(), h.since_ms);
                self.degradable(rules::HALTED, detail);
            }
        }
    }

    fn check_reduce_only(&mut self, reduces: bool) {
        let i = self.intent;
        if !i.reduce_only {
            self.skip(rules::REDUCE_ONLY, "not reduce-only");
            return;
        }
        let pos = self.row().map_or(0.0, |r| r.qty);
        if reduces {
            self.pass(
                rules::REDUCE_ONLY,
                format!(
                    "{} {} of position {pos} on {}",
                    i.side.as_str(),
                    i.qty,
                    i.instrument
                ),
            );
        } else {
            self.fail(
                rules::REDUCE_ONLY,
                format!(
                    "{} {} would open or flip position {pos} on {}",
                    i.side.as_str(),
                    i.qty,
                    i.instrument
                ),
            );
        }
    }

    fn check_permission(&mut self) {
        let i = self.intent;
        if self.is_exit() {
            self.pass(rules::VENUE, "exit of an open position");
            self.pass(rules::INSTRUMENT, "exit of an open position");
            return;
        }
        match i.venue() {
            Some(v) if self.limits.venues.iter().any(|x| x == v) => {
                self.pass(rules::VENUE, format!("venue {v} permitted"))
            }
            Some(v) => self.fail(
                rules::VENUE,
                format!("venue {v} is not in [risk] venues {:?}", self.limits.venues),
            ),
            None => self.fail(rules::VENUE, format!("no venue in `{}`", i.instrument)),
        }
        if let Some(why) = permission_error(self.limits, &i.instrument) {
            self.fail(rules::INSTRUMENT, why);
        } else {
            self.pass(
                rules::INSTRUMENT,
                format!("{} is allow-listed", i.instrument),
            );
        }
    }

    fn check_lifecycle(&mut self) {
        let Some(min) = self.limits.min_lifecycle else {
            self.skip(
                rules::LIFECYCLE,
                "no lifecycle rule (M0: permission = instruments_allow)",
            );
            return;
        };
        if self.is_exit() {
            self.skip(rules::LIFECYCLE, "exit");
            return;
        }
        let ctx = self.ctx;
        match &ctx.lifecycle {
            Field::Ok { value } if *value >= min => self.pass(
                rules::LIFECYCLE,
                format!("{} ≥ {}", value.as_str(), min.as_str()),
            ),
            Field::Ok { value } => self.fail(
                rules::LIFECYCLE,
                format!(
                    "{} is {} < {}",
                    self.intent.instrument,
                    value.as_str(),
                    min.as_str()
                ),
            ),
            other => {
                let e = read_error(other, "lifecycle");
                self.fail(
                    missing("lifecycle"),
                    format!("{}: {}", self.intent.instrument, describe(&e)),
                );
            }
        }
    }

    fn check_losses(&mut self, suppress_trips: bool) {
        let loss = LossState::of(&self.ctx.account);
        let breaches = loss.breaches(self.limits);
        if !suppress_trips {
            self.trips.extend(breaches.iter().copied());
        }
        for (rule, field, limit, reason) in [
            (
                rules::DAILY_LOSS,
                &loss.daily_loss_usd,
                self.limits.daily_loss_limit_usd,
                HaltReason::DailyLoss,
            ),
            (
                rules::TOTAL_LOSS,
                &loss.total_loss_usd,
                self.limits.total_loss_limit_usd,
                HaltReason::TotalLoss,
            ),
        ] {
            match field {
                Field::Ok { value } => {
                    let room = limit - value;
                    if rule == rules::DAILY_LOSS {
                        self.headroom.daily_loss_usd = Some(room);
                    } else {
                        self.headroom.total_loss_usd = Some(room);
                    }
                    let detail = format!("loss {value} USD, limit {limit} USD");
                    if !breaches.contains(&reason) {
                        self.pass(rule, detail);
                    } else if self.is_exit() {
                        self.pass(rule, format!("{detail} — breached (exit: not gated)"));
                    } else {
                        self.fail(rule, format!("{detail} — breached, halts entries"));
                    }
                }
                other => {
                    let (code, detail) = match other {
                        Field::Absent if rule == rules::DAILY_LOSS => (
                            missing("day_start_equity"),
                            "no day-start equity yet".to_string(),
                        ),
                        Field::Absent => (missing("equity"), "no equity".to_string()),
                        _ => {
                            let e = read_error(other, "equity");
                            (missing_of(&e), describe(&e))
                        }
                    };
                    if self.is_exit() {
                        self.skip(rule, format!("exit: not gated ({code}: {detail})"));
                    } else {
                        self.fail(code, detail);
                    }
                }
            }
        }
    }

    fn check_rates(&mut self) {
        let (n, max) = (self.ctx.orders_last_min, self.limits.max_orders_per_min);
        self.headroom.orders_per_min = Some(i64::from(max) - i64::from(n) - 1);
        if n < max {
            self.pass(
                rules::ORDER_RATE,
                format!("{n} orders in the last 60 s, max {max}"),
            );
        } else {
            self.fail(
                rules::ORDER_RATE,
                format!("{n} orders in the last 60 s: another exceeds {max}"),
            );
        }
        let (n, max) = (self.ctx.open_orders, self.limits.max_open_orders);
        self.headroom.open_orders = Some(i64::from(max) - i64::from(n) - 1);
        if n < max {
            self.pass(rules::OPEN_ORDERS, format!("{n} resting orders, max {max}"));
        } else {
            self.fail(
                rules::OPEN_ORDERS,
                format!("{n} resting orders: another exceeds {max}"),
            );
        }
    }

    /// `book_age`, `ctx_age` of the order leg.
    fn check_ages(&mut self) {
        let id = self.intent.instrument.clone();
        let (book, ctx) = match self.leg(&id) {
            Some(leg) => (leg.book.clone(), leg.ctx.clone()),
            None => (Field::Absent, Field::Absent),
        };
        let ages = self.limits.max_data_age_ms;
        match &book {
            Field::Ok { value: b } => {
                let age = b.age_ms(self.now_ms);
                let detail = format!("{} age {age} ms, max {} ms", b.key, ages.book);
                if age <= ages.book {
                    self.pass(rules::BOOK_AGE, detail);
                } else {
                    self.degradable(rules::BOOK_AGE, format!("stale: {detail}"));
                }
            }
            other => {
                let e = read_error(other, "book");
                self.degradable(missing("book"), format!("{id}: {}", describe(&e)));
            }
        }
        match &ctx {
            Field::Ok { value: c } => {
                let age = c.age_ms(self.now_ms);
                let detail = format!("{} age {age} ms, max {} ms", c.key, ages.ctx);
                if age <= ages.ctx {
                    self.pass(rules::CTX_AGE, detail);
                } else {
                    self.degradable(rules::CTX_AGE, format!("stale: {detail}"));
                }
            }
            other => {
                let e = read_error(other, "ctx");
                self.degradable(missing("ctx"), format!("{id}: {}", describe(&e)));
            }
        }
    }

    /// `growth_usd`: how much the order grows the position (USD); `None` =
    /// unknown (treated as growing without a size).
    fn check_market_status(&mut self, growth_usd: Option<f64>) {
        if self.is_exit() {
            self.skip(rules::MARKET_STATUS, "exit: the fill engine decides");
            return;
        }
        let id = self.intent.instrument.clone();
        let ctx = self
            .leg(&id)
            .map(|l| l.ctx.clone())
            .unwrap_or(Field::Absent);
        match &ctx {
            Field::Ok { value: c } => {
                let s = c.status;
                let grows = growth_usd.is_none_or(|g| g > 0.0);
                let mut detail = format!("{id} {}", s.as_str());
                let mut ok = s == MarketStatus::Open || (s == MarketStatus::AtOiCap && !grows);
                if let (true, true, Some(room)) = (ok, grows, c.oi_cap_headroom_usd) {
                    match growth_usd {
                        Some(g) => {
                            ok = within_max(g, room);
                            detail =
                                format!("{detail}; grows OI by {g} USD, cap headroom {room} USD");
                        }
                        None => {
                            ok = false;
                            detail =
                                format!("{detail}; OI growth unknown, cap headroom {room} USD");
                        }
                    }
                }
                if ok {
                    self.pass(rules::MARKET_STATUS, detail);
                } else {
                    self.fail(rules::MARKET_STATUS, detail);
                }
            }
            other => {
                let e = read_error(other, "ctx");
                self.fail(missing("ctx"), format!("{id}: {}", describe(&e)));
            }
        }
    }

    fn check_min_edge(&mut self) {
        if self.is_exit() {
            self.skip(rules::MIN_EDGE, "exit");
            return;
        }
        let Some(key) = self.intent.opportunity_key.clone() else {
            self.fail(
                missing(EDGE_FEATURE),
                "the order names no opportunity row (opportunity_key)",
            );
            return;
        };
        if !key_names(&key, &self.intent.instrument) {
            self.fail(
                missing(EDGE_FEATURE),
                format!(
                    "opportunity row {key} is not about {}",
                    self.intent.instrument
                ),
            );
            return;
        }
        let ctx = self.ctx;
        match &ctx.opportunity {
            Field::Ok { value: o } if o.key != key => self.fail(
                missing(EDGE_FEATURE),
                format!("context row {} is not the order's {key}", o.key),
            ),
            Field::Ok { value: o } => {
                let age = self.now_ms.saturating_sub(o.observed_at_ms).max(0) as u64;
                if o.ttl_ms == 0 || age > o.ttl_ms {
                    self.fail(
                        missing(EDGE_FEATURE),
                        format!("stale: {key} age {age} ms > ttl {} ms", o.ttl_ms),
                    );
                    return;
                }
                match &o.edge_after_costs_bps {
                    Field::Ok { value } => {
                        let detail = format!(
                            "{key} {EDGE_FEATURE} {value} bps, min {} bps",
                            self.limits.min_edge_bps
                        );
                        if at_least(*value, self.limits.min_edge_bps) {
                            self.pass(rules::MIN_EDGE, detail);
                        } else {
                            self.fail(rules::MIN_EDGE, detail);
                        }
                    }
                    other => {
                        let e = read_error(other, EDGE_FEATURE);
                        self.fail(missing(EDGE_FEATURE), format!("{key}: {}", describe(&e)));
                    }
                }
            }
            other => {
                let e = read_error(other, EDGE_FEATURE);
                self.fail(missing(EDGE_FEATURE), format!("{key}: {}", describe(&e)));
            }
        }
    }

    /// Depth + slippage of `notional` on `side` of `book` — `(depth, slip)`
    /// check results as (ok, detail).
    fn liquidity(
        &self,
        id: &str,
        book: &BookInput,
        side: Side,
        notional: f64,
    ) -> ((bool, String), (bool, String)) {
        let l = self.limits;
        let depth = match depth_within(&book.book, l.max_slippage_bps) {
            Ok(d) => {
                let usd = d.for_taker(side);
                (
                    at_least(usd, l.min_depth_usd),
                    format!(
                        "{id} {} depth {usd} USD within {} bps, min {} USD",
                        side.as_str(),
                        l.max_slippage_bps,
                        l.min_depth_usd
                    ),
                )
            }
            Err(e) => (false, format!("{id}: {e}")),
        };
        let slip = match book.book.walk(side, WalkTarget::Notional(notional), None) {
            Ok(w) if !w.is_complete() => (
                false,
                format!(
                    "{id} {} walk of {notional} USD leaves {} USD unfilled (visible depth)",
                    side.as_str(),
                    w.unfilled
                ),
            ),
            Ok(w) => match w.slippage_bps_vs_mid {
                Some(s) => (
                    within_max(s, l.max_slippage_bps),
                    format!(
                        "{id} {} {notional} USD vwap {} slippage {s} bps vs mid, max {} bps",
                        side.as_str(),
                        w.vwap.unwrap_or(f64::NAN),
                        l.max_slippage_bps
                    ),
                ),
                None => (false, format!("{id}: no mid (a side of the book is empty)")),
            },
            Err(e) => (false, format!("{id}: {e}")),
        };
        (depth, slip)
    }

    fn check_liquidity(&mut self) {
        if self.is_exit() {
            self.skip(rules::DEPTH, "exit");
            self.skip(rules::SLIPPAGE, "exit");
            return;
        }
        let id = self.intent.instrument.clone();
        let book = self
            .leg(&id)
            .map(|l| l.book.clone())
            .unwrap_or(Field::Absent);
        match &book {
            Field::Ok { value: b } => {
                let ((d_ok, d), (s_ok, s)) =
                    self.liquidity(&id, b, self.intent.side, self.order_usd);
                if d_ok {
                    self.pass(rules::DEPTH, d);
                } else {
                    self.fail(rules::DEPTH, d);
                }
                if s_ok {
                    self.pass(rules::SLIPPAGE, s);
                } else {
                    self.fail(rules::SLIPPAGE, s);
                }
            }
            other => {
                let e = read_error(other, "book");
                let detail = format!("{id}: {}", describe(&e));
                self.fail(missing("book"), format!("depth: {detail}"));
                self.fail(missing("book"), format!("slippage: {detail}"));
            }
        }
    }

    /// Price that values a new position: the order leg's book mid (market
    /// data), else the intent's own `notional_usd / qty`.
    fn reference_px(&self) -> f64 {
        let i = self.intent;
        self.leg(&i.instrument)
            .and_then(|l| l.book.value())
            .and_then(|b| b.book.mid())
            .unwrap_or(i.notional_usd / i.qty)
    }

    fn after(&self) -> Result<After, (String, String)> {
        let i = self.intent;
        let (qty_before, px) = match self.row() {
            None => (0.0, self.reference_px()),
            Some(r) => match &r.mark_px {
                Field::Ok { value } => (r.qty, *value),
                other => {
                    let e = read_error(other, &format!("mark:{}", i.instrument));
                    return Err((missing_of(&e), describe(&e)));
                }
            },
        };
        let qty_after = qty_before + i.side.sign() * i.qty;
        let (pos_before, pos_after) = (qty_before * px, qty_after * px);
        Ok(After {
            qty_before,
            qty_after,
            pos_before,
            pos_after,
            delta: pos_after - pos_before,
        })
    }

    fn cap(&mut self, rule: &str, value: f64, limit: f64, what: String) -> f64 {
        let detail = format!("{what} {value} USD, cap {limit} USD");
        if within_max(value, limit) {
            self.pass(rule, detail);
        } else {
            self.fail(rule, detail);
        }
        limit - value
    }

    fn check_exposure(&mut self, after: &Result<After, (String, String)>) {
        const CAPS: [&str; 7] = [
            rules::ORDER_NOTIONAL,
            rules::POSITION_NOTIONAL,
            rules::ASSET_EXPOSURE,
            rules::VENUE_EXPOSURE,
            rules::GROSS_EXPOSURE,
            rules::NET_EXPOSURE,
            rules::LEVERAGE,
        ];
        if self.is_exit() {
            for rule in CAPS {
                self.skip(rule, "exit: reduces the position");
            }
            return;
        }
        let (i, l) = (self.intent, self.limits);
        self.headroom.order_usd = Some(self.cap(
            rules::ORDER_NOTIONAL,
            self.order_usd,
            l.max_order_notional_usd,
            format!("order on {}", i.instrument),
        ));
        let a = match after {
            Ok(a) => a,
            Err((code, detail)) => {
                for rule in &CAPS[1..] {
                    self.fail(code.clone(), format!("{rule}: {detail}"));
                }
                return;
            }
        };
        self.headroom.position_usd = Some(self.cap(
            rules::POSITION_NOTIONAL,
            a.pos_after.abs(),
            l.max_position_notional_usd,
            format!(
                "position {} → {} on {}",
                a.qty_before, a.qty_after, i.instrument
            ),
        ));
        let ctx = self.ctx;
        let acct = &ctx.account;
        let venue = i.venue().unwrap_or_default().to_string();
        let groups = [
            (
                rules::ASSET_EXPOSURE,
                group(&acct.by_underlying, &i.underlying),
                i.underlying.clone(),
            ),
            (
                rules::VENUE_EXPOSURE,
                group(&acct.by_venue, &venue),
                venue.clone(),
            ),
        ];
        for (rule, exposure, key) in groups {
            match exposure {
                Field::Ok { value: e } => {
                    if rule == rules::ASSET_EXPOSURE {
                        let v = (e.net_usd + a.delta).abs();
                        self.headroom.asset_usd =
                            Some(self.cap(rule, v, l.max_asset_exposure_usd, format!("net {key}")));
                    } else {
                        let v = e.gross_usd - a.pos_before.abs() + a.pos_after.abs();
                        self.headroom.venue_usd = Some(self.cap(
                            rule,
                            v,
                            l.max_venue_exposure_usd,
                            format!("gross {key}"),
                        ));
                    }
                }
                other => {
                    let e = read_error(&other, "exposure");
                    self.fail(missing_of(&e), format!("{rule} {key}: {}", describe(&e)));
                }
            }
        }
        match &acct.exposure {
            Field::Ok { value: e } => {
                let gross = e.gross_usd - a.pos_before.abs() + a.pos_after.abs();
                let net = (e.net_usd + a.delta).abs();
                self.headroom.gross_usd = Some(self.cap(
                    rules::GROSS_EXPOSURE,
                    gross,
                    l.max_gross_exposure_usd,
                    "gross".to_string(),
                ));
                self.headroom.net_usd = Some(self.cap(
                    rules::NET_EXPOSURE,
                    net,
                    l.max_net_exposure_usd,
                    "net".to_string(),
                ));
                match &acct.equity_usd {
                    Field::Ok { value: eq } if *eq > 0.0 => {
                        let lev = gross / eq;
                        self.headroom.leverage = Some(l.max_leverage - lev);
                        let detail = format!(
                            "gross {gross} USD / equity {eq} USD = {lev}x, max {}x",
                            l.max_leverage
                        );
                        if within_max(lev, l.max_leverage) {
                            self.pass(rules::LEVERAGE, detail);
                        } else {
                            self.fail(rules::LEVERAGE, detail);
                        }
                    }
                    Field::Ok { value: eq } => {
                        self.fail(rules::LEVERAGE, format!("equity {eq} USD ≤ 0"))
                    }
                    other => {
                        let e = read_error(other, "equity");
                        self.fail(missing_of(&e), format!("leverage: {}", describe(&e)));
                    }
                }
            }
            other => {
                let e = read_error(other, "exposure");
                for rule in [rules::GROSS_EXPOSURE, rules::NET_EXPOSURE, rules::LEVERAGE] {
                    self.fail(missing_of(&e), format!("{rule}: {}", describe(&e)));
                }
            }
        }
    }

    /// `hedge` + `skew` (entries of `require_hedge_for` strategies).
    fn check_hedge(&mut self) {
        let needs = self
            .intent
            .strategy
            .as_ref()
            .is_some_and(|s| self.limits.require_hedge_for.contains(s));
        if self.is_exit() || !needs {
            let why = if self.is_exit() {
                "exit"
            } else {
                "strategy needs no hedge"
            };
            self.skip(rules::HEDGE, why);
            self.skip(rules::SKEW, why);
            return;
        }
        let strategy = self.intent.strategy.clone().unwrap_or_default();
        let Some(h) = self.intent.hedge_instrument.clone() else {
            let detail = format!("strategy {strategy} requires a hedge_instrument");
            self.fail(rules::HEDGE, detail.clone());
            self.fail(rules::SKEW, detail);
            return;
        };
        let leg = self.leg(&h).cloned().unwrap_or(LegMarket {
            book: Field::Absent,
            ctx: Field::Absent,
        });
        let side = self.intent.side.opposite();
        let ages = self.limits.max_data_age_ms;
        let mut problems: Vec<String> = Vec::new();
        let mut code = rules::HEDGE.to_string();
        if venue_of(&h).is_none_or(|v| !self.limits.venues.iter().any(|x| x == v)) {
            problems.push(format!("{h}: venue not in [risk] venues"));
        }
        if let Some(why) = permission_error(self.limits, &h) {
            problems.push(why);
        }
        match &leg.ctx {
            Field::Ok { value: c } => {
                let age = c.age_ms(self.now_ms);
                if age > ages.ctx {
                    problems.push(format!("{} stale: {age} ms > {} ms", c.key, ages.ctx));
                }
                if c.status != MarketStatus::Open {
                    problems.push(format!("{h} {}", c.status.as_str()));
                }
            }
            other => {
                code = missing("hedge_ctx");
                problems.push(format!("{h}: {}", describe(&read_error(other, "ctx"))));
            }
        }
        let hedge_book_at = match &leg.book {
            Field::Ok { value: b } => {
                let age = b.age_ms(self.now_ms);
                if age > ages.book {
                    problems.push(format!("{} stale: {age} ms > {} ms", b.key, ages.book));
                }
                let ((d_ok, d), (s_ok, s)) = self.liquidity(&h, b, side, self.order_usd);
                if !d_ok {
                    problems.push(d);
                }
                if !s_ok {
                    problems.push(s);
                }
                Some(b.observed_at_ms.min(b.book.venue_ts_ms))
            }
            other => {
                code = missing("hedge_book");
                problems.push(format!("{h}: {}", describe(&read_error(other, "book"))));
                None
            }
        };
        if problems.is_empty() {
            self.pass(
                rules::HEDGE,
                format!("{} hedge on {h} available", side.as_str()),
            );
        } else {
            self.fail(code, problems.join("; "));
        }
        let order_book_at = match self.leg(&self.intent.instrument).map(|l| &l.book) {
            Some(Field::Ok { value: b }) => Some(b.observed_at_ms.min(b.book.venue_ts_ms)),
            _ => None,
        };
        match (order_book_at, hedge_book_at) {
            (Some(a), Some(b)) => {
                let skew = (a - b).unsigned_abs();
                let detail = format!(
                    "books of {} and {h} {skew} ms apart, max {} ms",
                    self.intent.instrument, self.limits.max_skew_ms
                );
                if skew <= self.limits.max_skew_ms {
                    self.pass(rules::SKEW, detail);
                } else {
                    self.fail(rules::SKEW, detail);
                }
            }
            (None, _) => self.fail(
                missing("book"),
                format!("skew: no book for {}", self.intent.instrument),
            ),
            (_, None) => self.fail(missing("hedge_book"), format!("skew: no book for {h}")),
        }
    }
}

impl OrderIntent {
    fn class_word(&self) -> &'static str {
        if self.reduce_only {
            "reduce-only"
        } else {
            "order"
        }
    }
}

/// Whether observation `key` (`<schema>:<subject>`, ids joined by `:`)
/// names the full instrument `id` as whole `:`-segments — so
/// `hyperliquid:xyz:TSL` never matches a `hyperliquid:xyz:TSLA` row.
pub fn key_names(key: &str, id: &str) -> bool {
    !id.is_empty() && format!("{key}:").contains(&format!(":{id}:"))
}

/// Why `id` may not be traded under the allow / deny lists; `None` = allowed.
pub fn permission_error(limits: &RiskLimits, id: &str) -> Option<String> {
    if limits.instruments_deny.iter().any(|d| d == id) {
        Some(format!("{id} is in instruments_deny"))
    } else if !limits.instruments_allow.iter().any(|a| a == id) {
        Some(format!("{id} is not in instruments_allow"))
    } else {
        None
    }
}

/// Exposure of `key` in `groups`; a key without positions is flat (0).
fn group(groups: &[GroupExposure], key: &str) -> Field<Exposure> {
    groups
        .iter()
        .find(|g| g.key == key)
        .map(|g| g.exposure.clone())
        .unwrap_or_else(|| {
            Field::ok(Exposure {
                gross_usd: 0.0,
                net_usd: 0.0,
            })
        })
}

/// Gate one order (module tables). Pure; never panics on bad input — a bad
/// intent is a `deny` on `intent`.
pub fn evaluate(
    intent: &OrderIntent,
    ctx: &RiskContext,
    limits: &RiskLimits,
    now_ms: i64,
) -> RiskVerdict {
    let pos_qty = ctx
        .account
        .positions
        .iter()
        .find(|r| r.instrument == intent.instrument)
        .map_or(0.0, |r| r.qty);
    let reduces = pos_qty != 0.0
        && pos_qty.signum() == -intent.side.sign()
        && intent.qty.is_finite()
        && intent.qty > 0.0
        && intent.qty <= pos_qty.abs() * (1.0 + LIMIT_TOL);
    let class = if intent.reduce_only && reduces {
        OrderClass::Exit
    } else {
        OrderClass::Entry
    };
    let mut g = Gate {
        intent,
        ctx,
        limits,
        now_ms,
        class,
        order_usd: intent.notional_usd,
        checks: Vec::new(),
        headroom: Headroom::default(),
        trips: Vec::new(),
    };
    g.check_intent();
    g.check_account();
    let wrong_account = g.checks.iter().any(|c| c.status == CheckStatus::Fail);
    g.check_kill_switch();
    g.check_halted();
    g.check_reduce_only(reduces);
    g.check_permission();
    g.check_lifecycle();
    g.check_losses(wrong_account);
    g.check_rates();
    g.check_ages();
    let after = g.after();
    if let Ok(a) = &after {
        // An intent whose notional understates its qty is judged by the qty.
        g.order_usd = intent.notional_usd.max(a.delta.abs());
    }
    let growth_usd = after
        .as_ref()
        .ok()
        .map(|a| a.pos_after.abs() - a.pos_before.abs());
    g.check_market_status(growth_usd);
    g.check_min_edge();
    g.check_liquidity();
    g.check_exposure(&after);
    g.check_hedge();
    if wrong_account {
        g.trips.clear();
    }
    let mut trips = Vec::new();
    for t in g.trips {
        if !trips.contains(&t) {
            trips.push(t);
        }
    }
    let first_fail = g
        .checks
        .iter()
        .find(|c| c.status == CheckStatus::Fail)
        .map(|c| c.rule.clone());
    let degraded = g.checks.iter().any(|c| c.status == CheckStatus::Waived);
    let (allow, rule) = match first_fail {
        Some(rule) => (false, rule),
        None if degraded => (true, rules::ALLOW_REDUCE_DEGRADED.to_string()),
        None => (true, rules::OK.to_string()),
    };
    RiskVerdict {
        allow,
        rule,
        class,
        degraded: allow && degraded,
        checks: g.checks,
        headroom: g.headroom,
        trips,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::book::L2Level;
    use crate::domain::market::InstrumentId;
    use crate::domain::observation::{ObsSource, Observed};
    use crate::domain::xm::ledger::{Fill, Mark, PaperAccount};

    const ACCOUNT: &str = "xmarket";
    const TSLA: &str = "hyperliquid:xyz:TSLA";
    const NVDA: &str = "hyperliquid:xyz:NVDA";
    const TSLA_RH: &str = "robinhood:0x322F0929c4625eD5bAd873c95208D54E1c003b2d";
    const TESLA: &str = "company:tesla";
    const NVIDIA: &str = "company:nvidia";
    const OPP: &str = "xm_compare/1:hyperliquid:xyz:TSLA:hyperliquid:xyz:TSLA";
    /// 2026-10-03 00:00 UTC.
    const NOW: i64 = 1_790_985_600_000;

    /// The $100 budget (tracker § 7 #3), min edge 10 bps.
    fn limits() -> RiskLimits {
        RiskLimits {
            account: ACCOUNT.into(),
            venues: vec!["hyperliquid".into()],
            min_lifecycle: None,
            instruments_allow: vec![TSLA.into(), NVDA.into()],
            instruments_deny: vec![],
            max_order_notional_usd: 25.0,
            max_position_notional_usd: 50.0,
            max_asset_exposure_usd: 50.0,
            max_venue_exposure_usd: 100.0,
            max_gross_exposure_usd: 100.0,
            max_net_exposure_usd: 100.0,
            max_leverage: 1.0,
            daily_loss_limit_usd: 10.0,
            total_loss_limit_usd: 25.0,
            min_edge_bps: 10.0,
            max_slippage_bps: 30.0,
            min_depth_usd: 250.0,
            require_hedge_for: vec!["convergence".into()],
            max_data_age_ms: MaxAges {
                book: 5_000,
                ctx: 20_000,
                reference: 60_000,
                quote: 20_000,
            },
            max_skew_ms: 5_000,
            max_orders_per_min: 6,
            max_open_orders: 4,
            allow_reduce_degraded: true,
        }
    }

    /// bid 399.9 / ask 400.1 (mid 400), 2 + 2 + 5 per side.
    fn tsla_book(at_ms: i64) -> L2Book {
        let l = |px: f64, sz: f64| L2Level { px, sz, n: 1 };
        L2Book::new(
            vec![l(399.9, 2.0), l(399.5, 2.0), l(398.0, 5.0)],
            vec![l(400.1, 2.0), l(400.5, 2.0), l(402.0, 5.0)],
            at_ms,
        )
        .unwrap()
    }

    fn leg(id: &str, book: L2Book, book_at: i64, ctx_at: i64, status: MarketStatus) -> LegMarket {
        LegMarket {
            book: Field::ok(BookInput {
                key: format!("hl_book/1:{id}"),
                observed_at_ms: book_at,
                book,
            }),
            ctx: Field::ok(CtxInput {
                key: format!("mkt_ctx/1:{id}"),
                observed_at_ms: ctx_at,
                status,
                oi_cap_headroom_usd: None,
            }),
        }
    }

    /// The order leg's market inputs, when the case still has them.
    fn tsla_leg(c: &mut Case) -> Option<&mut LegMarket> {
        c.ctx.legs.get_mut(TSLA)
    }

    /// $100 cash; long NVDA 0.25 @ 100 marked 100 (= $25 gross / net).
    fn account(marks: &[(&str, f64, i64)], day_start: Option<f64>) -> PaperPositions {
        let mut a = PaperAccount::new(ACCOUNT, 100.0).unwrap();
        a.apply_fill(&Fill {
            instrument: NVDA.into(),
            underlying: NVIDIA.into(),
            venue: "hyperliquid".into(),
            side: Side::Buy,
            qty: 0.25,
            px: 100.0,
            fee_usd: 0.0,
            ts_ms: NOW - 3_600_000,
        })
        .unwrap();
        let m: BTreeMap<String, Field<Mark>> = marks
            .iter()
            .map(|&(id, px, at_ms)| (id.to_string(), Field::ok(Mark { px, at_ms })))
            .collect();
        PaperPositions::build(&a, &m, NOW, 20_000, day_start, None)
    }

    struct Case {
        intent: OrderIntent,
        ctx: RiskContext,
        limits: RiskLimits,
    }

    impl Case {
        /// Buy $25 of TSLA (qty 0.0625 at 400) with a fresh book, ctx and a
        /// 12 bps opportunity row: allowed.
        fn entry() -> Self {
            let mut legs = BTreeMap::new();
            legs.insert(
                TSLA.to_string(),
                leg(
                    TSLA,
                    tsla_book(NOW - 1_000),
                    NOW - 1_000,
                    NOW - 2_000,
                    MarketStatus::Open,
                ),
            );
            Case {
                intent: OrderIntent {
                    account: ACCOUNT.into(),
                    instrument: TSLA.into(),
                    underlying: TESLA.into(),
                    side: Side::Buy,
                    qty: 0.0625,
                    notional_usd: 25.0,
                    reduce_only: false,
                    strategy: Some("overreaction".into()),
                    hedge_instrument: None,
                    opportunity_key: Some(OPP.into()),
                },
                ctx: RiskContext {
                    account: account(&[(NVDA, 100.0, NOW - 1_000)], Some(100.0)),
                    halt: None,
                    kill_switch: Field::ok(false),
                    lifecycle: Field::Absent,
                    orders_last_min: 0,
                    open_orders: 0,
                    legs,
                    opportunity: Field::ok(EdgeInput {
                        key: OPP.into(),
                        observed_at_ms: NOW - 1_000,
                        ttl_ms: 5_000,
                        edge_after_costs_bps: Field::ok(12.0),
                    }),
                },
                limits: limits(),
            }
        }

        /// Reduce-only sell of the whole NVDA position (0.25), fresh data.
        fn exit() -> Self {
            let mut c = Case::entry();
            c.intent = OrderIntent {
                account: ACCOUNT.into(),
                instrument: NVDA.into(),
                underlying: NVIDIA.into(),
                side: Side::Sell,
                qty: 0.25,
                notional_usd: 25.0,
                reduce_only: true,
                strategy: None,
                hedge_instrument: None,
                opportunity_key: None,
            };
            let nvda_book = L2Book::new(
                vec![L2Level {
                    px: 99.9,
                    sz: 10.0,
                    n: 1,
                }],
                vec![L2Level {
                    px: 100.1,
                    sz: 10.0,
                    n: 1,
                }],
                NOW - 1_000,
            )
            .unwrap();
            c.ctx.legs.insert(
                NVDA.to_string(),
                leg(
                    NVDA,
                    nvda_book,
                    NOW - 1_000,
                    NOW - 1_000,
                    MarketStatus::Open,
                ),
            );
            c
        }

        fn run(&self) -> RiskVerdict {
            evaluate(&self.intent, &self.ctx, &self.limits, NOW)
        }
    }

    fn err(field: &str) -> ReadError {
        ReadError::new(field, ErrorClass::Timeout, "slow")
    }

    #[track_caller]
    fn assert_allow(v: &RiskVerdict) {
        assert!(v.allow, "denied {}: {:#?}", v.rule, v.failed());
    }

    #[track_caller]
    fn assert_deny(v: &RiskVerdict, rule: &str) {
        assert!(!v.allow, "allowed, expected {rule}: {:#?}", v.checks);
        assert_eq!(v.rule, rule, "{:#?}", v.failed());
    }

    #[test]
    fn baseline_entry_is_allowed_with_every_rule_listed() {
        let v = Case::entry().run();
        assert_allow(&v);
        assert_eq!(
            (v.rule.as_str(), v.class, v.degraded),
            ("ok", OrderClass::Entry, false)
        );
        assert!(v.trips.is_empty());
        for rule in [
            rules::INTENT,
            rules::ACCOUNT,
            rules::KILL_SWITCH,
            rules::HALTED,
            rules::REDUCE_ONLY,
            rules::VENUE,
            rules::INSTRUMENT,
            rules::LIFECYCLE,
            rules::DAILY_LOSS,
            rules::TOTAL_LOSS,
            rules::ORDER_RATE,
            rules::OPEN_ORDERS,
            rules::BOOK_AGE,
            rules::CTX_AGE,
            rules::MARKET_STATUS,
            rules::MIN_EDGE,
            rules::DEPTH,
            rules::SLIPPAGE,
            rules::ORDER_NOTIONAL,
            rules::POSITION_NOTIONAL,
            rules::ASSET_EXPOSURE,
            rules::VENUE_EXPOSURE,
            rules::GROSS_EXPOSURE,
            rules::NET_EXPOSURE,
            rules::LEVERAGE,
            rules::HEDGE,
            rules::SKEW,
        ] {
            assert!(v.check(rule).is_some(), "no {rule} check");
        }
        // Hand-computed headroom: order 25/25, TSLA 25/50, tesla net 25/50,
        // HL gross 50/100, gross 50/100, net 50/100, leverage 0.5/1.
        let h = &v.headroom;
        assert_eq!(h.order_usd, Some(0.0));
        assert_eq!(h.position_usd, Some(25.0));
        assert_eq!(h.asset_usd, Some(25.0));
        assert_eq!(h.venue_usd, Some(50.0));
        assert_eq!(h.gross_usd, Some(50.0));
        assert_eq!(h.net_usd, Some(50.0));
        assert_eq!(h.leverage, Some(0.5));
        assert_eq!(h.daily_loss_usd, Some(10.0));
        assert_eq!(h.total_loss_usd, Some(25.0));
        assert_eq!((h.orders_per_min, h.open_orders), (Some(5), Some(3)));
        // Full ids in details.
        let d = &v.check(rules::POSITION_NOTIONAL).unwrap().detail;
        assert!(d.contains(TSLA), "{d}");
        let slip = &v.check(rules::SLIPPAGE).unwrap().detail;
        assert!(slip.contains("slippage 2.5"), "{slip}");
    }

    /// A boundary vector: `set(case, value)` moves one limit (or input) so
    /// that `value` sits exactly at it.
    type Setter = fn(&mut Case, f64);

    /// Slippage of the baseline $25 buy: the walk's VWAP (400.1) vs mid 400.
    fn baseline_slippage() -> f64 {
        tsla_book(NOW)
            .walk(Side::Buy, WalkTarget::Notional(25.0), None)
            .unwrap()
            .slippage_bps_vs_mid
            .unwrap()
    }

    #[test]
    fn slippage_of_the_baseline_walk() {
        assert!(
            (baseline_slippage() - 2.5).abs() < 1e-9,
            "{}",
            baseline_slippage()
        );
    }

    /// Allow at the limit, deny at limit + 1e-6 with `rule`. `value` is the
    /// hand-computed after-fill value of the baseline entry (see
    /// `baseline_entry_is_allowed_with_every_rule_listed`).
    #[test]
    fn caps_allow_at_the_limit_and_deny_just_above() {
        let rows: [(&str, f64, Setter); 9] = [
            (rules::ORDER_NOTIONAL, 25.0, |c, v| {
                c.limits.max_order_notional_usd = v
            }),
            (rules::POSITION_NOTIONAL, 25.0, |c, v| {
                c.limits.max_position_notional_usd = v
            }),
            (rules::ASSET_EXPOSURE, 25.0, |c, v| {
                c.limits.max_asset_exposure_usd = v
            }),
            (rules::VENUE_EXPOSURE, 50.0, |c, v| {
                c.limits.max_venue_exposure_usd = v
            }),
            (rules::GROSS_EXPOSURE, 50.0, |c, v| {
                c.limits.max_gross_exposure_usd = v
            }),
            (rules::NET_EXPOSURE, 50.0, |c, v| {
                c.limits.max_net_exposure_usd = v
            }),
            (rules::LEVERAGE, 0.5, |c, v| c.limits.max_leverage = v),
            // Slippage of the $25 walk (one level at 400.1 vs mid 400, see
            // `slippage_of_the_baseline_walk`); depth then no longer binds.
            (rules::SLIPPAGE, baseline_slippage(), |c, v| {
                c.limits.max_slippage_bps = v;
                c.limits.min_depth_usd = 0.0;
            }),
            // Loss: equity 100 (flat marks), so a day start of 110 = 10 lost.
            (rules::DAILY_LOSS, 10.0, |c, v| {
                c.limits.daily_loss_limit_usd = v
            }),
        ];
        for (rule, value, set) in rows {
            let mut at = Case::entry();
            if rule == rules::DAILY_LOSS {
                at.ctx.account = account(&[(NVDA, 100.0, NOW - 1_000)], Some(110.0));
            }
            set(&mut at, value);
            let v = at.run();
            assert_allow(&v);
            assert_eq!(v.check(rule).unwrap().status, CheckStatus::Pass, "{rule}");
            let mut over = at;
            set(&mut over, value - 1e-6);
            let v = over.run();
            assert_deny(&v, rule);
            if rule == rules::DAILY_LOSS {
                assert_eq!(v.trips, vec![HaltReason::DailyLoss]);
            }
        }
    }

    #[test]
    fn total_loss_min_edge_depth_ages_and_rates_boundaries() {
        // Total loss: NVDA marked 60 ⇒ equity 90, loss 10.
        let mut at = Case::entry();
        at.ctx.account = account(&[(NVDA, 60.0, NOW - 1_000)], Some(90.0));
        at.limits.total_loss_limit_usd = 10.0;
        at.limits.daily_loss_limit_usd = 10.0;
        assert_allow(&at.run());
        at.limits.total_loss_limit_usd = 10.0 - 1e-6;
        at.limits.daily_loss_limit_usd = 10.0 - 1e-6;
        let v = at.run();
        assert_deny(&v, rules::TOTAL_LOSS);
        assert_eq!(v.trips, vec![HaltReason::TotalLoss]);

        // Min edge: 12 bps row.
        let mut c = Case::entry();
        c.limits.min_edge_bps = 12.0;
        assert_allow(&c.run());
        c.limits.min_edge_bps = 12.0 + 1e-6;
        assert_deny(&c.run(), rules::MIN_EDGE);

        // Depth: asks within 30 bps of 400 = 400.1·2 + 400.5·2 = 1601.2.
        let mut c = Case::entry();
        c.limits.min_depth_usd = 400.1 * 2.0 + 400.5 * 2.0;
        assert_allow(&c.run());
        c.limits.min_depth_usd += 1e-6;
        assert_deny(&c.run(), rules::DEPTH);

        // Ages: book 1000 ms, ctx 2000 ms old.
        let mut c = Case::entry();
        c.limits.max_data_age_ms.book = 1_000;
        c.limits.max_data_age_ms.ctx = 2_000;
        assert_allow(&c.run());
        c.limits.max_data_age_ms.book = 999;
        assert_deny(&c.run(), rules::BOOK_AGE);
        c.limits.max_data_age_ms.book = 1_000;
        c.limits.max_data_age_ms.ctx = 1_999;
        assert_deny(&c.run(), rules::CTX_AGE);
        // A book is as old as its venue time when that is older.
        let mut c = Case::entry();
        if let Some(LegMarket {
            book: Field::Ok { value },
            ..
        }) = c.ctx.legs.get_mut(TSLA)
        {
            value.book.venue_ts_ms = NOW - 6_000;
        }
        assert_deny(&c.run(), rules::BOOK_AGE);

        // Rates: 5 orders in the last minute + this one = 6 = max.
        let mut c = Case::entry();
        c.ctx.orders_last_min = 5;
        c.ctx.open_orders = 3;
        assert_allow(&c.run());
        c.ctx.orders_last_min = 6;
        assert_deny(&c.run(), rules::ORDER_RATE);
        c.ctx.orders_last_min = 5;
        c.ctx.open_orders = 4;
        assert_deny(&c.run(), rules::OPEN_ORDERS);
    }

    #[test]
    fn permission_venue_lifecycle_market_status_and_account() {
        let mut c = Case::entry();
        c.limits.instruments_allow = vec![NVDA.into()];
        assert_deny(&c.run(), rules::INSTRUMENT);
        let mut c = Case::entry();
        c.limits.instruments_deny = vec![TSLA.into()];
        let v = c.run();
        assert_deny(&v, rules::INSTRUMENT);
        assert!(v.failed().unwrap().detail.contains("instruments_deny"));
        let mut c = Case::entry();
        c.limits.venues = vec!["robinhood".into()];
        assert_deny(&c.run(), rules::VENUE);

        // Lifecycle only once the limits carry one (M1).
        let mut c = Case::entry();
        c.limits.min_lifecycle = Some(Lifecycle::PaperTradable);
        c.ctx.lifecycle = Field::ok(Lifecycle::PaperTradable);
        assert_allow(&c.run());
        c.ctx.lifecycle = Field::ok(Lifecycle::Validated);
        assert_deny(&c.run(), rules::LIFECYCLE);
        c.ctx.lifecycle = Field::Absent;
        assert_deny(&c.run(), "missing:lifecycle");
        let v = Case::entry().run();
        assert_eq!(
            v.check(rules::LIFECYCLE).unwrap().status,
            CheckStatus::Skipped
        );

        for (status, want) in [
            (MarketStatus::NoBook, false),
            (MarketStatus::Delisted, false),
            (MarketStatus::Halted, false),
            (MarketStatus::AtOiCap, false),
            (MarketStatus::Open, true),
        ] {
            let mut c = Case::entry();
            if let Some(LegMarket {
                ctx: Field::Ok { value },
                ..
            }) = c.ctx.legs.get_mut(TSLA)
            {
                value.status = status;
            }
            let v = c.run();
            assert_eq!(v.allow, want, "{status:?}: {:?}", v.failed());
            if !want {
                assert_eq!(v.rule, rules::MARKET_STATUS);
            }
        }

        let mut c = Case::entry();
        c.intent.account = "shadow".into();
        let v = c.run();
        assert_deny(&v, rules::ACCOUNT);
        assert!(v.trips.is_empty());

        let mutations: [(fn(&mut OrderIntent), &str); 5] = [
            (|i| i.qty = 0.0, "qty"),
            (|i| i.notional_usd = f64::NAN, "notional_usd"),
            (|i| i.instrument = "TSLA".into(), "full"),
            (|i| i.underlying = NVIDIA.into(), "differs"),
            (
                |i| i.hedge_instrument = Some(TSLA.into()),
                "hedge_instrument",
            ),
        ];
        for (mutate, what) in mutations {
            let mut c = Case::entry();
            if what == "differs" {
                c.intent.instrument = NVDA.into();
                c.intent.underlying = TESLA.into();
            } else {
                mutate(&mut c.intent);
            }
            let v = c.run();
            assert_deny(&v, rules::INTENT);
            assert!(
                v.failed().unwrap().detail.contains(what),
                "{what}: {:?}",
                v.failed()
            );
        }
    }

    #[test]
    fn oi_cap_blocks_growth_only() {
        let mut c = Case::entry();
        c.intent.instrument = NVDA.into();
        c.intent.underlying = NVIDIA.into();
        c.intent.side = Side::Sell;
        c.intent.qty = 0.125;
        c.intent.notional_usd = 12.5;
        let nvda = L2Book::new(
            vec![L2Level {
                px: 99.9,
                sz: 10.0,
                n: 1,
            }],
            vec![L2Level {
                px: 100.1,
                sz: 10.0,
                n: 1,
            }],
            NOW - 1_000,
        )
        .unwrap();
        c.ctx.legs.insert(
            NVDA.into(),
            leg(NVDA, nvda, NOW - 1_000, NOW - 1_000, MarketStatus::AtOiCap),
        );
        c.limits.min_depth_usd = 100.0;
        // Selling half of the long does not grow OI: passes.
        let v = c.run();
        assert_eq!(
            v.check(rules::MARKET_STATUS).unwrap().status,
            CheckStatus::Pass
        );
        // Buying more does.
        c.intent.side = Side::Buy;
        assert_deny(&c.run(), rules::MARKET_STATUS);
    }

    /// An open market with a known OI cap: the order may grow the position by
    /// at most `oi_cap_usd − oi_usd` (the $25 baseline entry grows it by 25).
    #[test]
    fn oi_cap_headroom_bounds_growth() {
        let mut c = Case::entry();
        let set = |c: &mut Case, room: f64| {
            if let Some(LegMarket {
                ctx: Field::Ok { value },
                ..
            }) = c.ctx.legs.get_mut(TSLA)
            {
                value.oi_cap_headroom_usd = Some(room);
            }
        };
        set(&mut c, 25.0);
        let v = c.run();
        assert_allow(&v);
        assert!(v
            .check(rules::MARKET_STATUS)
            .unwrap()
            .detail
            .contains("headroom 25"));
        set(&mut c, 25.0 - 1e-6);
        assert_deny(&c.run(), rules::MARKET_STATUS);

        // From a stored row: cap 1,000,000 − OI 2,400 × 400 = 40,000.
        let id = InstrumentId::parse(TSLA).unwrap();
        let mut ctx = MarketCtx::new(id, NOW);
        ctx.mark = Field::ok(400.0);
        ctx.oi_base = Field::ok(2_400.0);
        ctx.oi_cap_usd = Some(1_000_000.0);
        let row = Observation::of("hl_ctx", &ctx, NOW, 15_000, ObsSource::Live);
        let input = CtxInput::from_row(&row);
        assert_eq!(input.value().unwrap().oi_cap_headroom_usd, Some(40_000.0));
        ctx.oi_base = Field::Absent;
        let row = Observation::of("hl_ctx", &ctx, NOW, 15_000, ObsSource::Live);
        assert_eq!(
            CtxInput::from_row(&row)
                .value()
                .unwrap()
                .oi_cap_headroom_usd,
            None
        );
    }

    /// Caps never trust the intent's own price: 0.125 TSLA at mid 400 is $50
    /// whatever `notional_usd` says.
    #[test]
    fn an_understated_notional_is_capped_by_qty_at_mid() {
        let mut c = Case::entry();
        c.intent.qty = 0.125;
        c.intent.notional_usd = 25.0;
        let v = c.run();
        assert_deny(&v, rules::ORDER_NOTIONAL);
        assert!(
            v.failed().unwrap().detail.contains("50 USD"),
            "{:?}",
            v.failed()
        );
        assert_eq!(v.headroom.order_usd, Some(-25.0));
        // Consistent intent at the same size is judged the same way.
        c.intent.notional_usd = 50.0;
        assert_deny(&c.run(), rules::ORDER_NOTIONAL);
    }

    #[test]
    fn opportunity_keys_name_instruments_as_whole_segments() {
        for (key, id, want) in [
            (OPP, TSLA, true),
            ("xm_compare/1:hyperliquid:xyz:TSLA:robinhood:0x322F0929c4625eD5bAd873c95208D54E1c003b2d", TSLA_RH, true),
            ("xm_compare/1:hyperliquid:xyz:TSLA:robinhood:0x322F0929c4625eD5bAd873c95208D54E1c003b2d", TSLA, true),
            ("xm_fade/1:hyperliquid:xyz:TSLA", TSLA, true),
            ("xm_fade/1:hyperliquid:xyz:TSLA", "hyperliquid:xyz:TSL", false),
            ("xm_fade/1:hyperliquid:xyz:TSLAX", TSLA, false),
            ("xm_fade/1:hyperliquid:xyz:NVDA", TSLA, false),
            ("xm_fade/1:hyperliquid:xyz:TSLA", "", false),
        ] {
            assert_eq!(key_names(key, id), want, "{key} names {id}");
        }
    }

    /// Every input an entry reads, knocked out (`Absent` and `Error`),
    /// denies with `missing:<field>` — never an allow.
    #[test]
    fn never_allows_an_entry_with_a_missing_input() {
        type Knock = fn(&mut Case);
        fn k(rule: &'static str, knock: Knock) -> (&'static str, Knock) {
            (rule, knock)
        }
        let knocks: Vec<(&str, Knock)> = vec![
            k("missing:kill_switch", |c| c.ctx.kill_switch = Field::Absent),
            k("missing:kill_switch", |c| {
                c.ctx.kill_switch = Field::err(err("kill_switch"))
            }),
            k("missing:day_start_equity", |c| {
                c.ctx.account = account(&[(NVDA, 100.0, NOW - 1_000)], None)
            }),
            k("missing:mark", |c| {
                c.ctx.account = account(&[], Some(100.0))
            }),
            k("missing:mark", |c| {
                c.ctx.account = account(&[(NVDA, 100.0, NOW - 60_000)], Some(100.0))
            }),
            k("missing:book", |c| c.ctx.legs.clear()),
            k("missing:book", |c| {
                if let Some(l) = tsla_leg(c) {
                    l.book = Field::Absent;
                }
            }),
            k("missing:book", |c| {
                if let Some(l) = tsla_leg(c) {
                    l.book = Field::err(err("hl_book"));
                }
            }),
            k("missing:ctx", |c| {
                if let Some(l) = tsla_leg(c) {
                    l.ctx = Field::Absent;
                }
            }),
            k("missing:ctx", |c| {
                if let Some(l) = tsla_leg(c) {
                    l.ctx = Field::err(err("mkt_ctx"));
                }
            }),
            k("missing:edge_after_costs_bps", |c| {
                // A row about another instrument is no edge for this one.
                c.intent.opportunity_key =
                    Some("xm_compare/1:hyperliquid:xyz:NVDA:hyperliquid:xyz:NVDA".into());
            }),
            k("missing:edge_after_costs_bps", |c| {
                c.ctx.opportunity = Field::Absent
            }),
            k("missing:edge_after_costs_bps", |c| {
                c.ctx.opportunity = Field::err(err("xm_compare"))
            }),
            k("missing:edge_after_costs_bps", |c| {
                c.intent.opportunity_key = None
            }),
            k("missing:edge_after_costs_bps", |c| {
                if let Field::Ok { value } = &mut c.ctx.opportunity {
                    value.edge_after_costs_bps = Field::Absent;
                }
            }),
            k("missing:edge_after_costs_bps", |c| {
                if let Field::Ok { value } = &mut c.ctx.opportunity {
                    value.edge_after_costs_bps = Field::err(err(EDGE_FEATURE));
                }
            }),
            k("missing:edge_after_costs_bps", |c| {
                // Past the row TTL.
                if let Field::Ok { value } = &mut c.ctx.opportunity {
                    value.observed_at_ms = NOW - 5_001;
                }
            }),
            k("missing:lifecycle", |c| {
                c.limits.min_lifecycle = Some(Lifecycle::Mapped);
                c.ctx.lifecycle = Field::err(err("catalog"));
            }),
        ];
        for (want, knock) in &knocks {
            let mut c = Case::entry();
            knock(&mut c);
            let v = c.run();
            assert!(!v.allow, "{want}: allowed");
            assert_eq!(&v.rule, want, "{:#?}", v.failed());
            assert!(v.rule.starts_with(rules::MISSING));
        }
        // Hedge legs of a `require_hedge_for` strategy too.
        let hedge_knocks: [(&str, fn(&mut LegMarket)); 4] = [
            ("missing:hedge_book", |l| l.book = Field::Absent),
            ("missing:hedge_book", |l| l.book = Field::err(err("book"))),
            ("missing:hedge_ctx", |l| l.ctx = Field::Absent),
            ("missing:hedge_ctx", |l| l.ctx = Field::err(err("ctx"))),
        ];
        for (want, knock) in hedge_knocks {
            let mut c = hedged();
            assert_allow(&c.run());
            knock(c.ctx.legs.get_mut(TSLA_RH).unwrap());
            assert_deny(&c.run(), want);
        }
        // Pairs of knock-outs (a sweep): still never an allow.
        for (i, (_, a)) in knocks.iter().enumerate() {
            for (_, b) in knocks.iter().skip(i + 1) {
                let mut c = Case::entry();
                a(&mut c);
                b(&mut c);
                assert!(!c.run().allow);
            }
        }
    }

    /// A `convergence` entry hedged on the RH TSLA token.
    fn hedged() -> Case {
        let mut c = Case::entry();
        c.limits.venues.push("robinhood".into());
        c.limits.instruments_allow.push(TSLA_RH.into());
        c.intent.strategy = Some("convergence".into());
        c.intent.hedge_instrument = Some(TSLA_RH.into());
        let rh = L2Book::new(
            vec![L2Level {
                px: 399.0,
                sz: 5.0,
                n: 1,
            }],
            vec![L2Level {
                px: 401.0,
                sz: 5.0,
                n: 1,
            }],
            NOW - 3_000,
        )
        .unwrap();
        c.ctx.legs.insert(
            TSLA_RH.into(),
            leg(TSLA_RH, rh, NOW - 3_000, NOW - 3_000, MarketStatus::Open),
        );
        c
    }

    #[test]
    fn hedge_and_skew_for_hedged_strategies() {
        let v = hedged().run();
        assert_allow(&v);
        assert_eq!(v.check(rules::HEDGE).unwrap().status, CheckStatus::Pass);
        assert!(v
            .check(rules::SKEW)
            .unwrap()
            .detail
            .contains("2000 ms apart"));
        // No hedge leg named.
        let mut c = hedged();
        c.intent.hedge_instrument = None;
        assert_deny(&c.run(), rules::HEDGE);
        // Hedge leg not permitted.
        let mut c = hedged();
        c.limits.instruments_allow.retain(|i| i != TSLA_RH);
        let v = c.run();
        assert_deny(&v, rules::HEDGE);
        assert!(v.failed().unwrap().detail.contains(TSLA_RH));
        // Books 2000 ms apart: skew limit at 2000 passes, 1999 fails.
        let mut c = hedged();
        c.limits.max_skew_ms = 2_000;
        assert_allow(&c.run());
        c.limits.max_skew_ms = 1_999;
        assert_deny(&c.run(), rules::SKEW);
        // A strategy outside require_hedge_for skips both.
        let v = Case::entry().run();
        assert_eq!(v.check(rules::HEDGE).unwrap().status, CheckStatus::Skipped);
        assert_eq!(v.check(rules::SKEW).unwrap().status, CheckStatus::Skipped);
    }

    #[test]
    fn reduce_only_must_reduce() {
        let v = Case::exit().run();
        assert_allow(&v);
        assert_eq!((v.class, v.rule.as_str()), (OrderClass::Exit, "ok"));
        // More than the position = a flip.
        let mut c = Case::exit();
        c.intent.qty = 0.25 + 1e-6;
        let v = c.run();
        assert_deny(&v, rules::REDUCE_ONLY);
        assert_eq!(v.class, OrderClass::Entry);
        // Same side = an increase; no position = an open.
        let mut c = Case::exit();
        c.intent.side = Side::Buy;
        assert_deny(&c.run(), rules::REDUCE_ONLY);
        let mut c = Case::exit();
        c.intent.instrument = TSLA.into();
        c.intent.underlying = TESLA.into();
        assert_deny(&c.run(), rules::REDUCE_ONLY);
        // Exits skip permission, edge, liquidity and caps.
        let mut c = Case::exit();
        c.limits.instruments_allow.clear();
        c.limits.max_order_notional_usd = 10.0;
        c.ctx.opportunity = Field::Absent;
        c.ctx.account = account(&[], Some(100.0)); // NVDA mark missing
        let v = c.run();
        assert_allow(&v);
        for rule in [
            rules::MIN_EDGE,
            rules::ORDER_NOTIONAL,
            rules::DEPTH,
            rules::HEDGE,
        ] {
            assert_eq!(
                v.check(rule).unwrap().status,
                CheckStatus::Skipped,
                "{rule}"
            );
        }
    }

    /// § 7 #7: halted, kill switch, stale / missing data — a reduce-only
    /// exit passes with `allow_reduce_degraded`, an entry never does.
    #[test]
    fn reduce_only_exits_pass_while_halted_or_stale() {
        type Degrade = fn(&mut Case);
        let rows: [(&str, Degrade); 9] = [
            (rules::HALTED, |c| {
                c.ctx.halt = Some(Halt {
                    reason: HaltReason::DailyLoss,
                    since_ms: NOW - 10,
                })
            }),
            (rules::HALTED, |c| {
                c.ctx.halt = Some(Halt {
                    reason: HaltReason::TotalLoss,
                    since_ms: NOW - 10,
                })
            }),
            (rules::HALTED, |c| {
                c.ctx.halt = Some(Halt {
                    reason: HaltReason::Operator,
                    since_ms: NOW - 10,
                })
            }),
            (rules::HALTED, |c| {
                c.ctx.halt = Some(Halt {
                    reason: HaltReason::File,
                    since_ms: NOW - 10,
                })
            }),
            (rules::KILL_SWITCH, |c| c.ctx.kill_switch = Field::ok(true)),
            ("missing:kill_switch", |c| {
                c.ctx.kill_switch = Field::err(err("kill_switch"))
            }),
            (rules::BOOK_AGE, |c| c.limits.max_data_age_ms.book = 10),
            (rules::CTX_AGE, |c| c.limits.max_data_age_ms.ctx = 10),
            ("missing:book", |c| {
                for leg in c.ctx.legs.values_mut() {
                    leg.book = Field::Absent;
                }
            }),
        ];
        for (rule, degrade) in rows {
            let mut exit = Case::exit();
            degrade(&mut exit);
            let v = exit.run();
            assert_allow(&v);
            assert_eq!(v.rule, rules::ALLOW_REDUCE_DEGRADED, "{rule}");
            assert!(v.degraded);
            let waived: Vec<&str> = v
                .checks
                .iter()
                .filter(|c| c.status == CheckStatus::Waived)
                .map(|c| c.rule.as_str())
                .collect();
            assert!(waived.contains(&rule), "{rule}: {waived:?}");

            let mut strict = Case::exit();
            degrade(&mut strict);
            strict.limits.allow_reduce_degraded = false;
            let v = strict.run();
            assert!(!v.allow, "{rule}: strict exit allowed");
            assert!(!v.degraded);

            let mut entry = Case::entry();
            degrade(&mut entry);
            assert!(!entry.run().allow, "{rule}: entry allowed");
        }
        // The kill switch trips the `file` halt whatever the order.
        let mut c = Case::exit();
        c.ctx.kill_switch = Field::ok(true);
        assert_eq!(c.run().trips, vec![HaltReason::File]);
        let mut c = Case::entry();
        c.ctx.kill_switch = Field::ok(true);
        let v = c.run();
        assert_deny(&v, rules::KILL_SWITCH);
        assert_eq!(v.trips, vec![HaltReason::File]);
        // A halt comes before every value rule.
        let mut c = Case::entry();
        c.ctx.halt = Some(Halt {
            reason: HaltReason::Operator,
            since_ms: NOW,
        });
        c.intent.notional_usd = 1_000.0;
        let v = c.run();
        assert_deny(&v, rules::HALTED);
        assert!(v.failed().unwrap().detail.contains("operator"));
    }

    #[test]
    fn loss_breach_trips_even_on_exits_and_reports_headroom() {
        // NVDA marked 40: equity 85 (loss 15 > daily 10), total 15 < 25.
        let mut c = Case::exit();
        c.ctx.account = account(&[(NVDA, 40.0, NOW - 1_000)], Some(100.0));
        let v = c.run();
        assert_allow(&v);
        assert_eq!(v.trips, vec![HaltReason::DailyLoss]);
        assert_eq!(v.headroom.daily_loss_usd, Some(-5.0));
        assert_eq!(v.headroom.total_loss_usd, Some(10.0));
        let loss = LossState::of(&c.ctx.account);
        assert_eq!(loss.headroom_usd(&c.limits), Some(-5.0));
        assert_eq!(loss.breaches(&c.limits), vec![HaltReason::DailyLoss]);
    }

    #[test]
    fn edge_and_ctx_inputs_read_store_rows() {
        let row = |features: Value, status: ObsStatus| Observation {
            key: OPP.into(),
            schema: "xm_compare/1".into(),
            tool: "xm_compare".into(),
            observed_at_ms: NOW - 500,
            slot: None,
            ttl_ms: 5_000,
            source: ObsSource::Live,
            status,
            errors: vec![],
            headline: "x".into(),
            features: serde_json::from_value(features).unwrap(),
            data: Value::Null,
        };
        let ok = EdgeInput::from_row(
            OPP,
            Some(&row(json!({"edge_after_costs_bps": 14.5}), ObsStatus::Ok)),
        );
        let e = ok.value().unwrap();
        assert_eq!(
            (e.edge_after_costs_bps.clone(), e.ttl_ms),
            (Field::ok(14.5), 5_000)
        );
        let partial = EdgeInput::from_row(OPP, Some(&row(json!({}), ObsStatus::Partial)));
        assert_eq!(partial.value().unwrap().edge_after_costs_bps, Field::Absent);
        assert!(EdgeInput::from_row(
            OPP,
            Some(&row(json!({"edge_after_costs_bps": 1}), ObsStatus::Error))
        )
        .is_error());
        assert!(
            EdgeInput::from_row("xm_compare/1:other", Some(&row(json!({}), ObsStatus::Ok)))
                .is_error()
        );
        assert_eq!(EdgeInput::from_row(OPP, None), Field::Absent);

        let id = InstrumentId::parse(TSLA).unwrap();
        let mut ctx = MarketCtx::new(id.clone(), NOW);
        ctx.mark = Field::ok(400.0);
        let obs = Observation::of("hl_ctx", &ctx, NOW - 100, 15_000, ObsSource::Live);
        let c = CtxInput::from_row(&obs);
        assert_eq!(c.value().unwrap().status, MarketStatus::Open);
        assert_eq!(c.value().unwrap().key, format!("mkt_ctx/1:{TSLA}"));
        ctx.at_oi_cap = Some(true);
        let at_cap = Observation::of("hl_ctx", &ctx, NOW, 15_000, ObsSource::Live);
        assert_eq!(
            CtxInput::from_row(&at_cap).value().unwrap().status,
            MarketStatus::AtOiCap
        );
        let gone = MarketCtx::not_found(id, NOW);
        let gone = Observation::of("hl_ctx", &gone, NOW, 15_000, ObsSource::Live);
        assert_eq!(gone.schema, MarketCtx::SCHEMA);
        assert_eq!(
            CtxInput::from_row(&gone).value().unwrap().status,
            MarketStatus::Delisted
        );
    }

    #[test]
    fn verdict_and_digest_serialise_with_full_ids() {
        let c = hedged();
        let v = c.run();
        let back: RiskVerdict = serde_json::from_value(serde_json::to_value(&v).unwrap()).unwrap();
        assert_eq!(back, v);
        let d = c.ctx.digest(NOW).to_string();
        for id in [TSLA, TSLA_RH, NVDA, OPP] {
            assert!(d.contains(id), "{id} not in {d}");
        }
        assert!(!d.contains("\"bids\""), "no books in the digest");
        assert_eq!(HaltReason::parse("file"), Some(HaltReason::File));
        assert!(HaltReason::Operator.is_sticky() && !HaltReason::DailyLoss.is_sticky());
    }
}
