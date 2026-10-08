//! Paper fill engine (`risk-paper-fill-engine`): a Hyperliquid market / IOC
//! order against an L2 book — the depth walk of `domain/book.rs`, HL tick /
//! lot rounding and the taker fee of `xm/cost.rs`, HL's documented rejection
//! codes. Pure: the book, its age, the position and the market state are
//! inputs. `application/paper.rs` adds the latency (sleep on the `Clock`,
//! THEN a fresh book from the `BookSource` — tracker convention 16);
//! [`FillResult::ledger_fill`] feeds `xm/ledger.rs`. Not here: ALO / resting
//! orders (P1), AMM / RFQ venues (`rh-paper-fill`, M6).
//!
//! | # | Check — the first failure is the reason | Rejected with |
//! |---|---|---|
//! | 1 | well-formed: size > 0, `limit_px` iff `limit`, 0 < `max_slippage_bps` < 10 000, `ref_mid` > 0, position of the same instrument, valid rules / fees | `invalid_order` |
//! | 2 | market open | `market_halted` · `market_closed` · `delisted` |
//! | 3 | `book_age_ms ≤ max_book_age_ms` — unless `stale_book_ok` (a reduce-only exit whose `book_age` the gate waived, `allow_reduce_degraded`): filled at that book, marked `stale_book`; the book passes `L2Book::validate` (a replayed book is deserialized, not built) | `stale_book` · `bad_book` |
//! | 4 | reference px = `ref_mid` (the mid the order was priced at), else the book mid; neither and an empty taker side ⇒ as 10; a one-sided book ⇒ refused | `missing:mid` |
//! | 5 | IOC bound on the HL tick grid: ref ± `max_slippage_bps` (buy rounded down, sell up); a limit order's `limit_px` must be a valid HL price, and the tighter of the two binds | `Tick` |
//! | 6 | `oracle_band`: \|bound / oracle − 1\| ≤ `max_bps`; unknown oracle ⇒ refused | `Oracle` · `missing:oracle` |
//! | 7 | size = `qty`, or `notional_usd / ref`, rounded down to `szDecimals`; reduce-only: only against an open position, clipped to it | `ReduceOnly` |
//! | 8 | size > 0 and size × bound ≥ `min_notional_usd` (HL $10); a reduce-only close of the whole position is exempt | `MinTradeNtl` |
//! | 9 | at the OI cap nothing opens, adds or flips; unknown cap state refuses those (reducing passes) | `PositionIncreaseAtOpenInterestCap` · `PositionFlipAtOpenInterestCap` · `missing:at_oi_cap` |
//! | 10 | walk `Qty(size)` to the bound; nothing taken | `MarketOrderNoLiquidity` (market) · `IocCancel` (limit) |
//!
//! | Outcome | `status` | `reason` |
//! |---|---|---|
//! | whole size taken | `filled` | — |
//! | bound reached, rest canceled | `partial` | `bound` |
//! | visible depth ran out, rest canceled — never hidden liquidity | `partial` | `depth` |
//!
//! Fills are taker fills at level prices: `avg_px` = VWAP, `slippage_bps` vs
//! the filled book's mid (positive = cost), `fee_usd` = taker bps × filled
//! notional. A missing value is `None`, never 0. Reasons: HL's error type
//! verbatim (`MinTradeNtl`), the engine's own in snake_case; `message` leads
//! with HL's documented error string ("Error responses", read 2026-09-30).
//! Assumed until a testnet fill (M3b): a market order that takes nothing is
//! `MarketOrderNoLiquidity`; a reduce-only close under $10 is accepted; the
//! oracle band width is an input (HL documents no single number).

// Consumers land next wave (`risk-gate-enforcement`, `risk-paper-tools`,
// `x-weekend-fade-strategy`).
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

use crate::domain::book::{L2Book, LevelFill, Side, WalkEnd, WalkTarget};
use crate::domain::market::InstrumentId;
use crate::domain::xm::cost::{
    hl_px_ok, hl_round_px, hl_round_sz, hl_wire, FeeSchedule, HlKind, Liquidity, Round,
};
use crate::domain::xm::ledger::{Fill, Position};

/// Hyperliquid's minimum order value, USD (`MinTradeNtl`).
pub(crate) const HL_MIN_TRADE_NTL_USD: f64 = 10.0;

/// Relative slack for "the whole position" and the minimum value (f64
/// residue of summed lot sizes).
const EPS: f64 = 1e-9;

// ── Order ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OrderKind {
    /// IOC bounded by `max_slippage_bps` from the reference mid.
    Market,
    /// IOC at `limit_px`, tightened to the slippage bound.
    Limit,
}

impl OrderKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            OrderKind::Market => "market",
            OrderKind::Limit => "limit",
        }
    }
}

/// Time in force. GTC and ALO (`BadAloPx`) are P1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Tif {
    Ioc,
}

/// Base quantity, or USD converted at the reference price.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OrderSize {
    Qty(f64),
    NotionalUsd(f64),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PaperOrder {
    /// Idempotency key (the tool's arg, else `ToolCtx.call_id`).
    pub client_order_id: String,
    /// Full id (`hyperliquid:xyz:TSLA`).
    pub instrument: InstrumentId,
    pub side: Side,
    pub size: OrderSize,
    pub kind: OrderKind,
    pub tif: Tif,
    /// Required for `limit`, refused for `market`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_px: Option<f64>,
    pub reduce_only: bool,
    /// Worst price vs the reference mid, bps (`[risk] max_slippage_bps`).
    pub max_slippage_bps: f64,
    /// Mid the order was priced at when sent (the decision's book). Anchors
    /// the notional → size conversion and the slippage bound, so a market
    /// that moves during the latency can leave the order unfilled. `None` ⇒
    /// the filled book's mid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ref_mid: Option<f64>,
}

// ── Venue rules, market state ────────────────────────────────────

/// Orders priced outside `oracle ± max_bps` are rejected (`Oracle`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OracleBand {
    /// Venue oracle (`mkt_ctx/1` oracle); `None` / not > 0 ⇒ every order
    /// is refused `missing:oracle`.
    pub oracle_px: Option<f64>,
    pub max_bps: f64,
}

/// What the venue enforces on this instrument now.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VenueRules {
    /// Perp (prices ≤ 6 − `sz_decimals` decimals) or spot (8 − …).
    pub kind: HlKind,
    /// HL `szDecimals` (`mkt_instrument/1`).
    pub sz_decimals: u32,
    /// HL: [`HL_MIN_TRADE_NTL_USD`].
    pub min_notional_usd: f64,
    /// `None` = no band.
    pub oracle_band: Option<OracleBand>,
    /// HL `perpsAtOpenInterestCap` (`mkt_instrument/1.at_oi_cap`); `None` =
    /// unknown ⇒ orders that open, add or flip are refused.
    pub at_oi_cap: Option<bool>,
}

impl VenueRules {
    /// Hyperliquid: $10 minimum, no oracle band, the OI cap state as read.
    pub(crate) fn hyperliquid(kind: HlKind, sz_decimals: u32, at_oi_cap: Option<bool>) -> Self {
        Self {
            kind,
            sz_decimals,
            min_notional_usd: HL_MIN_TRADE_NTL_USD,
            oracle_band: None,
            at_oi_cap,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MarketStatus {
    Open,
    /// Trading halted by the venue or the market's deployer.
    Halted,
    /// Outside the venue's session (calendar).
    Closed,
    /// Delisted, or unknown to the venue (`mkt_ctx/1` absent).
    Delisted,
}

impl MarketStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            MarketStatus::Open => "open",
            MarketStatus::Halted => "halted",
            MarketStatus::Closed => "closed",
            MarketStatus::Delisted => "delisted",
        }
    }
}

/// Everything a fill is checked against besides the order and the book.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FillEnv<'a> {
    pub rules: &'a VenueRules,
    pub status: MarketStatus,
    /// The account's position in the order's instrument (`Position::flat`
    /// when none).
    pub position: &'a Position,
    pub fees: &'a FeeSchedule,
    /// `[risk] max_data_age_ms.book`.
    pub max_book_age_ms: u64,
    /// The gate waived `book_age` for this order (a reduce-only exit under
    /// `allow_reduce_degraded`): a book past `max_book_age_ms` is filled,
    /// not refused, and the result says `stale_book`. An exit must be able
    /// to get out; the waiver used to end in a `stale_book` rejection.
    pub stale_book_ok: bool,
}

// ── Result ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FillStatus {
    Filled,
    Partial,
    Rejected,
}

impl FillStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            FillStatus::Filled => "filled",
            FillStatus::Partial => "partial",
            FillStatus::Rejected => "rejected",
        }
    }
}

/// Why an order was rejected, or why a partial fill's rest was canceled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) enum FillReason {
    // Hyperliquid order error types, verbatim.
    Tick,
    MinTradeNtl,
    ReduceOnly,
    IocCancel,
    MarketOrderNoLiquidity,
    Oracle,
    PositionIncreaseAtOpenInterestCap,
    PositionFlipAtOpenInterestCap,
    // Engine refusals.
    #[serde(rename = "invalid_order")]
    InvalidOrder,
    /// Kind not in `[paper] order_types` (`application/paper.rs`).
    #[serde(rename = "order_type")]
    OrderType,
    #[serde(rename = "market_halted")]
    MarketHalted,
    #[serde(rename = "market_closed")]
    MarketClosed,
    #[serde(rename = "delisted")]
    Delisted,
    #[serde(rename = "stale_book")]
    StaleBook,
    #[serde(rename = "bad_book")]
    BadBook,
    #[serde(rename = "missing:mid")]
    MissingMid,
    #[serde(rename = "missing:oracle")]
    MissingOracle,
    #[serde(rename = "missing:at_oi_cap")]
    MissingOiCap,
    // Partial fills: why the rest was canceled.
    #[serde(rename = "bound")]
    Bound,
    #[serde(rename = "depth")]
    Depth,
}

impl FillReason {
    pub(crate) const ALL: [FillReason; 20] = [
        FillReason::Tick,
        FillReason::MinTradeNtl,
        FillReason::ReduceOnly,
        FillReason::IocCancel,
        FillReason::MarketOrderNoLiquidity,
        FillReason::Oracle,
        FillReason::PositionIncreaseAtOpenInterestCap,
        FillReason::PositionFlipAtOpenInterestCap,
        FillReason::InvalidOrder,
        FillReason::OrderType,
        FillReason::MarketHalted,
        FillReason::MarketClosed,
        FillReason::Delisted,
        FillReason::StaleBook,
        FillReason::BadBook,
        FillReason::MissingMid,
        FillReason::MissingOracle,
        FillReason::MissingOiCap,
        FillReason::Bound,
        FillReason::Depth,
    ];

    /// The serde name.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            FillReason::Tick => "Tick",
            FillReason::MinTradeNtl => "MinTradeNtl",
            FillReason::ReduceOnly => "ReduceOnly",
            FillReason::IocCancel => "IocCancel",
            FillReason::MarketOrderNoLiquidity => "MarketOrderNoLiquidity",
            FillReason::Oracle => "Oracle",
            FillReason::PositionIncreaseAtOpenInterestCap => "PositionIncreaseAtOpenInterestCap",
            FillReason::PositionFlipAtOpenInterestCap => "PositionFlipAtOpenInterestCap",
            FillReason::InvalidOrder => "invalid_order",
            FillReason::OrderType => "order_type",
            FillReason::MarketHalted => "market_halted",
            FillReason::MarketClosed => "market_closed",
            FillReason::Delisted => "delisted",
            FillReason::StaleBook => "stale_book",
            FillReason::BadBook => "bad_book",
            FillReason::MissingMid => "missing:mid",
            FillReason::MissingOracle => "missing:oracle",
            FillReason::MissingOiCap => "missing:at_oi_cap",
            FillReason::Bound => "bound",
            FillReason::Depth => "depth",
        }
    }

    /// A refusal the next attempt may not meet — the market or the data, not
    /// the order, decided it. The weekend fade retries an entry rejected for
    /// one of these within its lateness (`tools/xm/weekend_fade.rs`); a final
    /// one is its outcome.
    ///
    /// | Class | Reasons |
    /// |---|---|
    /// | transient: missing data | `missing:mid`, `missing:oracle`, `missing:at_oi_cap`, `bad_book` |
    /// | transient: book age | `stale_book` |
    /// | transient: liquidity | `MarketOrderNoLiquidity`, `IocCancel`, `bound`, `depth` |
    /// | transient: price bound | `Oracle` |
    /// | transient: venue state | `PositionIncreaseAtOpenInterestCap`, `PositionFlipAtOpenInterestCap`, `market_halted`, `market_closed` |
    /// | final: size, lot and tick rules | `MinTradeNtl`, `Tick`, `ReduceOnly` |
    /// | final: the order itself | `invalid_order`, `order_type` |
    /// | final: listing | `delisted` |
    pub(crate) fn is_transient(self) -> bool {
        match self {
            FillReason::MissingMid
            | FillReason::MissingOracle
            | FillReason::MissingOiCap
            | FillReason::BadBook
            | FillReason::StaleBook
            | FillReason::MarketOrderNoLiquidity
            | FillReason::IocCancel
            | FillReason::Bound
            | FillReason::Depth
            | FillReason::Oracle
            | FillReason::PositionIncreaseAtOpenInterestCap
            | FillReason::PositionFlipAtOpenInterestCap
            | FillReason::MarketHalted
            | FillReason::MarketClosed => true,
            FillReason::MinTradeNtl
            | FillReason::Tick
            | FillReason::ReduceOnly
            | FillReason::InvalidOrder
            | FillReason::OrderType
            | FillReason::Delisted => false,
        }
    }

    /// HL's documented error string for a venue code; `None` for the
    /// engine's own reasons.
    pub(crate) fn hl_error(self) -> Option<&'static str> {
        Some(match self {
            FillReason::Tick => "Price must be divisible by tick size.",
            FillReason::MinTradeNtl => "Order must have minimum value of $10.",
            FillReason::ReduceOnly => "Reduce only order would increase position.",
            FillReason::IocCancel => {
                "Order could not immediately match against any resting orders."
            }
            FillReason::MarketOrderNoLiquidity => "No liquidity available for market order.",
            FillReason::Oracle => "Order price too far from oracle",
            FillReason::PositionIncreaseAtOpenInterestCap
            | FillReason::PositionFlipAtOpenInterestCap => {
                "Order would increase open interest while open interest is capped"
            }
            _ => return None,
        })
    }
}

/// One simulated order. Numbers the engine could not compute are `None`,
/// never 0; `filled_qty` / `filled_notional_usd` / `fee_usd` are 0 only
/// when nothing filled.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct FillResult {
    pub client_order_id: String,
    pub instrument: InstrumentId,
    pub side: Side,
    pub kind: OrderKind,
    pub reduce_only: bool,
    pub status: FillStatus,
    /// Rejection reason, or `bound` / `depth` for a partial fill.
    pub reason: Option<FillReason>,
    pub message: Option<String>,
    /// Size sent, after lot rounding and reduce-only clipping.
    pub order_qty: Option<f64>,
    /// `ref_mid`, else the book mid.
    pub ref_px: Option<f64>,
    /// IOC price bound on the tick grid.
    pub bound_px: Option<f64>,
    pub fills: Vec<LevelFill>,
    pub filled_qty: f64,
    pub filled_notional_usd: f64,
    /// VWAP of the fills.
    pub avg_px: Option<f64>,
    /// Mid of the book filled against.
    pub mid: Option<f64>,
    /// VWAP vs that mid, bps, positive = cost.
    pub slippage_bps: Option<f64>,
    /// Taker fee on the filled notional.
    pub fee_usd: f64,
    /// Age of the book filled against; `None` when refused before a book
    /// was read.
    pub book_age_ms: Option<u64>,
    /// Filled against a book older than `max_book_age_ms` (`stale_book_ok`):
    /// its prices may be stale.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stale_book: bool,
}

impl FillResult {
    /// A rejection before any book was read (`application/paper.rs`).
    pub(crate) fn refused(order: &PaperOrder, reason: FillReason, detail: &str) -> Self {
        Self::pending(order, None).reject(reason, detail)
    }

    fn pending(order: &PaperOrder, book_age_ms: Option<u64>) -> Self {
        Self {
            client_order_id: order.client_order_id.clone(),
            instrument: order.instrument.clone(),
            side: order.side,
            kind: order.kind,
            reduce_only: order.reduce_only,
            status: FillStatus::Rejected,
            reason: None,
            message: None,
            order_qty: None,
            ref_px: None,
            bound_px: None,
            fills: Vec::new(),
            filled_qty: 0.0,
            filled_notional_usd: 0.0,
            avg_px: None,
            mid: None,
            slippage_bps: None,
            fee_usd: 0.0,
            book_age_ms,
            stale_book: false,
        }
    }

    fn reject(mut self, reason: FillReason, detail: &str) -> Self {
        self.status = FillStatus::Rejected;
        self.reason = Some(reason);
        self.message = Some(message(reason, detail));
        self
    }

    /// The ledger fill (qty = `filled_qty`, px = VWAP, the fee); `None` when
    /// nothing filled. `underlying` = the full underlying id
    /// (`company:tesla`); the venue comes from the instrument id.
    pub(crate) fn ledger_fill(&self, underlying: &str, ts_ms: i64) -> Option<Fill> {
        let px = self.avg_px?;
        (self.filled_qty > 0.0).then(|| Fill {
            instrument: self.instrument.to_string(),
            underlying: underlying.to_string(),
            venue: self.instrument.venue().to_string(),
            side: self.side,
            qty: self.filled_qty,
            px,
            fee_usd: self.fee_usd,
            ts_ms,
        })
    }
}

fn message(reason: FillReason, detail: &str) -> String {
    match reason.hl_error() {
        Some(hl) => format!("{hl} ({detail})"),
        None => detail.to_string(),
    }
}

// ── Engine ───────────────────────────────────────────────────────

/// Check 1 of the module table: `Err` names the first malformed input.
/// `application/paper.rs` runs it before spending the latency.
pub(crate) fn check_order(order: &PaperOrder, env: &FillEnv) -> Result<(), String> {
    let pos = |x: f64| x.is_finite() && x > 0.0;
    if order.client_order_id.trim().is_empty() {
        return Err("client_order_id is empty".into());
    }
    if env.position.instrument != order.instrument.to_string() {
        return Err(format!(
            "position is for {}, the order for {}",
            env.position.instrument, order.instrument
        ));
    }
    if !env.position.qty.is_finite() {
        return Err(format!("position qty {} is not finite", env.position.qty));
    }
    match order.size {
        OrderSize::Qty(q) if !pos(q) => return Err(format!("qty {q} is not > 0")),
        OrderSize::NotionalUsd(n) if !pos(n) => return Err(format!("notional_usd {n} is not > 0")),
        _ => {}
    }
    let s = order.max_slippage_bps;
    if !(pos(s) && s < 1e4) {
        return Err(format!("max_slippage_bps {s} is not in (0, 10000)"));
    }
    if let Some(m) = order.ref_mid.filter(|m| !pos(*m)) {
        return Err(format!("ref_mid {m} is not > 0"));
    }
    match (order.kind, order.limit_px) {
        (OrderKind::Limit, None) => return Err("a limit order needs limit_px".into()),
        (OrderKind::Limit, Some(px)) if !pos(px) => {
            return Err(format!("limit_px {px} is not > 0"))
        }
        (OrderKind::Market, Some(_)) => {
            return Err("a market order takes no limit_px (max_slippage_bps bounds it)".into())
        }
        _ => {}
    }
    let r = env.rules;
    if r.sz_decimals > r.kind.max_decimals() {
        return Err(format!(
            "sz_decimals {} exceeds {}",
            r.sz_decimals,
            r.kind.max_decimals()
        ));
    }
    if !(r.min_notional_usd.is_finite() && r.min_notional_usd >= 0.0) {
        return Err(format!(
            "min_notional_usd {} is not >= 0",
            r.min_notional_usd
        ));
    }
    if let Some(b) = r.oracle_band.filter(|b| !pos(b.max_bps)) {
        return Err(format!("oracle band max_bps {} is not > 0", b.max_bps));
    }
    env.fees.validate().map_err(|e| format!("fees: {e}"))
}

/// Simulate `order` against `book`, `book_age_ms` old (the module table).
pub(crate) fn simulate_fill(
    order: &PaperOrder,
    book: &L2Book,
    book_age_ms: u64,
    env: &FillEnv,
) -> FillResult {
    let mut r = FillResult::pending(order, Some(book_age_ms));
    r.mid = book.mid();
    // 1–3: inputs, market, book age.
    if let Err(what) = check_order(order, env) {
        return r.reject(FillReason::InvalidOrder, &what);
    }
    let closed = match env.status {
        MarketStatus::Open => None,
        MarketStatus::Halted => Some(FillReason::MarketHalted),
        MarketStatus::Closed => Some(FillReason::MarketClosed),
        MarketStatus::Delisted => Some(FillReason::Delisted),
    };
    if let Some(reason) = closed {
        return r.reject(
            reason,
            &format!("{} is {}", order.instrument, env.status.as_str()),
        );
    }
    if book_age_ms > env.max_book_age_ms {
        if !(env.stale_book_ok && order.reduce_only) {
            return r.reject(
                FillReason::StaleBook,
                &format!("book age {book_age_ms} ms > {} ms", env.max_book_age_ms),
            );
        }
        r.stale_book = true;
    }
    if let Err(e) = book.validate() {
        return r.reject(FillReason::BadBook, &e.to_string());
    }
    // 4: reference price.
    let Some(ref_px) = order.ref_mid.or(r.mid) else {
        if book.liquidity(order.side).is_empty() {
            return r.reject(no_fill(order.kind), "the book side it would take is empty");
        }
        return r.reject(
            FillReason::MissingMid,
            "one-sided book and no ref_mid: no mid to bound slippage",
        );
    };
    r.ref_px = Some(ref_px);
    // 5: IOC bound on the tick grid.
    let rules = env.rules;
    let slip = order.max_slippage_bps / 1e4;
    let slip_bound = match order.side {
        Side::Buy => hl_round_px(
            ref_px * (1.0 + slip),
            rules.sz_decimals,
            rules.kind,
            Round::Down,
        ),
        Side::Sell => hl_round_px(
            ref_px * (1.0 - slip),
            rules.sz_decimals,
            rules.kind,
            Round::Up,
        ),
    };
    let Some(slip_bound) = slip_bound else {
        return r.reject(
            FillReason::Tick,
            &format!("no valid price for the slippage bound of ref {ref_px}"),
        );
    };
    let bound = match order.limit_px {
        Some(limit) => {
            let wire = hl_wire(limit, rules.kind.max_decimals());
            if !hl_px_ok(&wire, rules.sz_decimals, rules.kind) {
                return r.reject(
                    FillReason::Tick,
                    &format!(
                        "limit_px {limit}: at most 5 significant figures and {} decimals",
                        rules.kind.max_decimals() - rules.sz_decimals
                    ),
                );
            }
            match order.side {
                Side::Buy => limit.min(slip_bound),
                Side::Sell => limit.max(slip_bound),
            }
        }
        None => slip_bound,
    };
    r.bound_px = Some(bound);
    // 6: oracle band.
    if let Some(band) = rules.oracle_band {
        let Some(oracle) = band.oracle_px.filter(|o| o.is_finite() && *o > 0.0) else {
            return r.reject(
                FillReason::MissingOracle,
                &format!("oracle unknown, band {} bps", band.max_bps),
            );
        };
        let dev_bps = ((bound - oracle) / oracle).abs() * 1e4;
        if dev_bps > band.max_bps {
            return r.reject(
                FillReason::Oracle,
                &format!(
                    "px {bound} is {dev_bps:.1} bps from oracle {oracle} > {} bps",
                    band.max_bps
                ),
            );
        }
    }
    // 7: size.
    let held = env.position.qty;
    let open = held.abs();
    let mut want = match order.size {
        OrderSize::Qty(q) => q,
        OrderSize::NotionalUsd(n) => n / ref_px,
    };
    let mut whole_position = false;
    if order.reduce_only {
        if held == 0.0 || held.signum() == order.side.sign() {
            let pos_side = env.position.side().map_or("flat", Side::as_str);
            return r.reject(
                FillReason::ReduceOnly,
                &format!("{} against position {pos_side} {held}", order.side.as_str()),
            );
        }
        if want >= open * (1.0 - EPS) {
            want = open;
            whole_position = true;
        }
    }
    let Some(qty) = hl_round_sz(want, rules.sz_decimals, Round::Down) else {
        return r.reject(
            FillReason::InvalidOrder,
            &format!("size {want} is not >= 0"),
        );
    };
    r.order_qty = Some(qty);
    // 8: minimum value.
    let value = qty * bound;
    let min = rules.min_notional_usd;
    if qty <= 0.0 || (!whole_position && value + EPS * min.max(1.0) < min) {
        return r.reject(
            FillReason::MinTradeNtl,
            &format!(
                "size {qty} × px {bound} = ${value:.5} < ${min} (sizes round down to {} decimals)",
                rules.sz_decimals
            ),
        );
    }
    // 9: open-interest cap.
    let flips = held != 0.0 && held.signum() != order.side.sign() && qty > open * (1.0 + EPS);
    let adds = held == 0.0 || held.signum() == order.side.sign();
    if !order.reduce_only && (adds || flips) {
        match rules.at_oi_cap {
            Some(false) => {}
            Some(true) => {
                let reason = if flips {
                    FillReason::PositionFlipAtOpenInterestCap
                } else {
                    FillReason::PositionIncreaseAtOpenInterestCap
                };
                return r.reject(reason, &format!("{} is at its OI cap", order.instrument));
            }
            None => {
                return r.reject(
                    FillReason::MissingOiCap,
                    &format!(
                        "OI cap state of {} unknown; only reducing orders pass",
                        order.instrument
                    ),
                );
            }
        }
    }
    // 10: the walk.
    let walk = match book.walk(order.side, WalkTarget::Qty(qty), Some(bound)) {
        Ok(w) => w,
        Err(e) => return r.reject(FillReason::InvalidOrder, &e.to_string()),
    };
    if walk.filled_qty <= 0.0 {
        let why = match book.touch(order.side) {
            Some(touch) => format!("best {touch} is beyond the bound {bound}"),
            None => "the book side it would take is empty".to_string(),
        };
        return r.reject(no_fill(order.kind), &why);
    }
    let (status, reason) = match walk.end {
        WalkEnd::Filled => (FillStatus::Filled, None),
        WalkEnd::Limit => (FillStatus::Partial, Some(FillReason::Bound)),
        WalkEnd::Depth => (FillStatus::Partial, Some(FillReason::Depth)),
    };
    r.status = status;
    r.reason = reason;
    r.message = reason.map(|reason| {
        let why = if reason == FillReason::Bound {
            format!("the next level is beyond the bound {bound}")
        } else {
            format!("visible depth ran out after {} levels", walk.levels_used)
        };
        let filled = hl_wire(walk.filled_qty, rules.sz_decimals);
        format!("{filled} of {qty} filled, rest canceled: {why}")
    });
    r.fee_usd = env.fees.fee_usd(Liquidity::Taker, walk.filled_notional);
    r.filled_qty = walk.filled_qty;
    r.filled_notional_usd = walk.filled_notional;
    r.avg_px = walk.vwap;
    r.slippage_bps = walk.slippage_bps_vs_mid;
    r.fills = walk.fills;
    r
}

fn no_fill(kind: OrderKind) -> FillReason {
    match kind {
        OrderKind::Market => FillReason::MarketOrderNoLiquidity,
        OrderKind::Limit => FillReason::IocCancel,
    }
}

/// Simulated latency `base_ms ± jitter_ms`, uniform in `rand01` ∈ [0, 1]
/// (0 ⇒ base − jitter, 1 ⇒ base + jitter; out of range or NaN ⇒ 0.5, as
/// `domain/backoff.rs`), never below 0.
pub(crate) fn jittered_latency_ms(base_ms: u64, jitter_ms: u64, rand01: f64) -> u64 {
    let r = if (0.0..=1.0).contains(&rand01) {
        rand01
    } else {
        0.5
    };
    let ms = base_ms as f64 + (2.0 * r - 1.0) * jitter_ms as f64;
    ms.round().max(0.0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::book::fixture::tsla_book;
    use crate::domain::book::L2Level;
    use crate::domain::xm::ledger::PaperAccount;

    const TSLA: &str = "hyperliquid:xyz:TSLA";
    const TESLA: &str = "company:tesla";
    /// Fixture mid: (347.16 + 347.23) / 2.
    const MID: f64 = 347.195;

    fn close(a: f64, b: f64, what: &str) {
        let tol = 1e-9 * b.abs().max(1.0);
        assert!((a - b).abs() <= tol, "{what}: got {a}, want {b}");
    }

    fn id() -> InstrumentId {
        InstrumentId::parse(TSLA).unwrap()
    }

    /// xyz:TSLA at tier 0: 4.5 × 2 (deployer scale 1) × 0.1 (growth mode)
    /// = 0.9 bps taker (`xm/cost.rs` worked example).
    fn fees() -> FeeSchedule {
        FeeSchedule::new(0.9, 0.3).unwrap()
    }

    /// xyz:TSLA: perp, szDecimals 3, not at its OI cap.
    fn rules() -> VenueRules {
        VenueRules::hyperliquid(HlKind::Perp, 3, Some(false))
    }

    fn flat() -> Position {
        Position::flat(TSLA, TESLA, "hyperliquid")
    }

    fn held(qty: f64, avg_px: f64) -> Position {
        Position {
            qty,
            avg_px: Some(avg_px),
            opened_ms: Some(0),
            ..flat()
        }
    }

    fn order(kind: OrderKind, side: Side, size: OrderSize, limit_px: Option<f64>) -> PaperOrder {
        PaperOrder {
            client_order_id: "loop:session-1:7".to_string(),
            instrument: id(),
            side,
            size,
            kind,
            tif: Tif::Ioc,
            limit_px,
            reduce_only: false,
            max_slippage_bps: 30.0,
            ref_mid: None,
        }
    }

    fn market(side: Side, size: OrderSize) -> PaperOrder {
        order(OrderKind::Market, side, size, None)
    }

    fn limit(side: Side, qty: f64, px: f64) -> PaperOrder {
        order(OrderKind::Limit, side, OrderSize::Qty(qty), Some(px))
    }

    fn run_with(o: &PaperOrder, rules: &VenueRules, position: &Position) -> FillResult {
        let fees = fees();
        let env = FillEnv {
            rules,
            status: MarketStatus::Open,
            position,
            fees: &fees,
            max_book_age_ms: 5_000,
            stale_book_ok: false,
        };
        simulate_fill(o, &tsla_book(), 1_000, &env)
    }

    fn run(o: &PaperOrder) -> FillResult {
        run_with(o, &rules(), &flat())
    }

    fn rejected(r: &FillResult, reason: FillReason) {
        assert_eq!(r.status, FillStatus::Rejected, "{r:?}");
        assert_eq!(r.reason, Some(reason), "{r:?}");
        assert!(r.fills.is_empty() && r.filled_qty == 0.0 && r.fee_usd == 0.0);
        assert_eq!((r.avg_px, r.slippage_bps), (None, None), "never 0");
    }

    /// Hand-computed with bc outside the repo (`tests/fixtures/xm/meta.json`
    /// `fills`); fee 0.9 bps.
    #[test]
    fn fixture_fills_match_hand_computed_goldens() {
        use FillStatus::{Filled, Partial};
        use OrderSize::{NotionalUsd, Qty};
        use Side::{Buy, Sell};
        struct V {
            order: PaperOrder,
            status: FillStatus,
            reason: Option<FillReason>,
            order_qty: f64,
            bound: f64,
            filled_qty: f64,
            notional: f64,
            avg: f64,
            slip: f64,
            fee: f64,
            levels: usize,
        }
        let tight = PaperOrder {
            max_slippage_bps: 2.0,
            ..market(Buy, Qty(100.0))
        };
        let wide = PaperOrder {
            max_slippage_bps: 100.0,
            ..market(Buy, Qty(1_000.0))
        };
        let vectors = [
            // $1 000 → 2.880225… → 2.880; bound 347.195 × 1.003 = 348.236585 → 348.23.
            V {
                order: market(Buy, NotionalUsd(1_000.0)),
                status: Filled,
                reason: None,
                order_qty: 2.88,
                bound: 348.23,
                filled_qty: 2.88,
                notional: 1_000.022_4,
                avg: 347.23,
                slip: 1.008_079_033_396_218,
                fee: 0.090_002_016,
                levels: 1,
            },
            V {
                order: market(Buy, NotionalUsd(10_000.0)),
                status: Filled,
                reason: None,
                order_qty: 28.802,
                bound: 348.23,
                filled_qty: 28.802,
                notional: 10_000.999_87,
                avg: 347.232_826_539_824,
                slip: 1.089_489_762_917_766,
                fee: 0.900_089_988_3,
                levels: 2,
            },
            // Bound 347.195 × 0.997 = 346.153415 → rounded up to 346.16.
            V {
                order: market(Sell, Qty(100.0)),
                status: Filled,
                reason: None,
                order_qty: 100.0,
                bound: 346.16,
                filled_qty: 100.0,
                notional: 34_715.638_28,
                avg: 347.156_382_8,
                slip: 1.112_262_561_384_813,
                fee: 3.124_407_445_2,
                levels: 4,
            },
            V {
                order: market(Sell, NotionalUsd(50_000.0)),
                status: Filled,
                reason: None,
                order_qty: 144.011,
                bound: 346.16,
                filled_qty: 144.011,
                notional: 49_992.296_49,
                avg: 347.142_207_817_458,
                slip: 1.520_534_067_069_267,
                fee: 4.499_306_684_1,
                levels: 4,
            },
            // $25 → 0.0720056… → 0.072 (value 0.072 × 348.23 = 25.07256).
            V {
                order: market(Buy, NotionalUsd(25.0)),
                status: Filled,
                reason: None,
                order_qty: 0.072,
                bound: 348.23,
                filled_qty: 0.072,
                notional: 25.000_56,
                avg: 347.23,
                slip: 1.008_079_033_396_218,
                fee: 0.002_250_050_4,
                levels: 1,
            },
            // 2 bps: 347.264439 → 347.26 stops before 347.27 (partial, bound).
            V {
                order: tight,
                status: Partial,
                reason: Some(FillReason::Bound),
                order_qty: 100.0,
                bound: 347.26,
                filled_qty: 32.572,
                notional: 11_310.103_31,
                avg: 347.233_922_080_314,
                slip: 1.121_043_802_888_303,
                fee: 1.017_909_297_9,
                levels: 3,
            },
            // More than the 530.752 visible: partial, depth — never hidden liquidity.
            V {
                order: wide,
                status: Partial,
                reason: Some(FillReason::Depth),
                order_qty: 1_000.0,
                bound: 350.66,
                filled_qty: 530.752,
                notional: 184_357.911_6,
                avg: 347.352_269_233_088,
                slip: 4.529_709_042_127_526,
                fee: 16.592_212_044,
                levels: 20,
            },
            // Limit tighter than the 30 bps bound: it binds.
            V {
                order: limit(Buy, 100.0, 347.27),
                status: Partial,
                reason: Some(FillReason::Bound),
                order_qty: 100.0,
                bound: 347.27,
                filled_qty: 61.594,
                notional: 21_388.573_25,
                avg: 347.250_921_355_976,
                slip: 1.610_661_327_963_578,
                fee: 1.924_971_592_5,
                levels: 4,
            },
            V {
                order: limit(Sell, 100.0, 347.12),
                status: Partial,
                reason: Some(FillReason::Bound),
                order_qty: 100.0,
                bound: 347.12,
                filled_qty: 98.15,
                notional: 34_073.484_78,
                avg: 347.157_257_055_527,
                slip: 1.087_082_028_046_078,
                fee: 3.066_613_630_2,
                levels: 3,
            },
        ];
        for v in vectors {
            let r = run(&v.order);
            let what = format!("{:?} {:?} {:?}", v.order.kind, v.order.side, v.order.size);
            assert_eq!((r.status, r.reason), (v.status, v.reason), "{what}: {r:?}");
            assert_eq!(r.order_qty, Some(v.order_qty), "{what}");
            assert_eq!(r.bound_px, Some(v.bound), "{what}");
            close(r.ref_px.unwrap(), MID, &format!("{what} ref_px = book mid"));
            assert_eq!(r.fills.len(), v.levels, "{what}");
            close(r.filled_qty, v.filled_qty, &format!("{what} filled_qty"));
            close(
                r.filled_notional_usd,
                v.notional,
                &format!("{what} notional"),
            );
            close(r.avg_px.unwrap(), v.avg, &format!("{what} avg_px"));
            let slip = r.slippage_bps.unwrap();
            assert!(
                (slip - v.slip).abs() < 1e-9,
                "{what} slip: {slip} vs {}",
                v.slip
            );
            close(r.fee_usd, v.fee, &format!("{what} fee"));
            close(r.mid.unwrap(), MID, "mid");
            assert_eq!(r.book_age_ms, Some(1_000));
            assert_eq!(r.message.is_some(), v.reason.is_some(), "{what}");
        }
    }

    #[test]
    fn fills_are_the_walk_levels() {
        let r = run(&market(Side::Buy, OrderSize::NotionalUsd(10_000.0)));
        let got: Vec<(usize, f64)> = r.fills.iter().map(|f| (f.level, f.px)).collect();
        assert_eq!(got, [(0, 347.23), (1, 347.24)]);
        close(r.fills[0].qty, 20.661, "level 0 whole");
        close(r.fills[1].qty, 8.141, "level 1 rest");
        // A partial fill says what was canceled and why.
        let tight = PaperOrder {
            max_slippage_bps: 2.0,
            ..market(Side::Buy, OrderSize::Qty(100.0))
        };
        assert_eq!(
            run(&tight).message.as_deref(),
            Some("32.572 of 100 filled, rest canceled: the next level is beyond the bound 347.26")
        );
        let wide = PaperOrder {
            max_slippage_bps: 100.0,
            ..market(Side::Buy, OrderSize::Qty(1_000.0))
        };
        assert_eq!(
            run(&wide).message.as_deref(),
            Some("530.752 of 1000 filled, rest canceled: visible depth ran out after 20 levels")
        );
    }

    #[test]
    fn min_trade_ntl_counts_the_rounded_size_at_the_bound() {
        // $10 → 0.0288… → 0.028; 0.028 × 348.23 = $9.75044 < $10.
        let r = run(&market(Side::Buy, OrderSize::NotionalUsd(10.0)));
        rejected(&r, FillReason::MinTradeNtl);
        assert_eq!(r.order_qty, Some(0.028));
        let m = r.message.unwrap();
        assert!(
            m.starts_with("Order must have minimum value of $10.") && m.contains("9.75044"),
            "{m}"
        );
        // $10.50 → 0.030; 0.03 × 348.23 = $10.4469 passes.
        let r = run(&market(Side::Buy, OrderSize::NotionalUsd(10.5)));
        assert_eq!(r.status, FillStatus::Filled);
        close(r.filled_notional_usd, 10.4169, "notional");
        close(r.fee_usd, 0.000_937_521, "fee");
        // A size that rounds to 0.
        let r = run(&market(Side::Buy, OrderSize::Qty(0.0004)));
        rejected(&r, FillReason::MinTradeNtl);
        assert_eq!(r.order_qty, Some(0.0));
    }

    #[test]
    fn tick_rule_on_limit_prices() {
        // 347.165 has 6 significant figures.
        rejected(&run(&limit(Side::Buy, 1.0, 347.165)), FillReason::Tick);
        // 34.7165 would need 4 decimals > 6 − 3.
        let r = run(&limit(Side::Buy, 1.0, 34.7165));
        rejected(&r, FillReason::Tick);
        assert!(r
            .message
            .unwrap()
            .starts_with("Price must be divisible by tick size."));
        // f64 noise on a valid price is not a tick error.
        let r = run(&limit(Side::Buy, 1.0, 347.13 + 0.1));
        assert_eq!(r.status, FillStatus::Filled, "{r:?}");
        assert_eq!(r.avg_px, Some(347.23));
    }

    #[test]
    fn ioc_and_market_orders_that_take_nothing() {
        // 347.2 is on tick but under the best ask 347.23.
        let r = run(&limit(Side::Buy, 1.0, 347.2));
        rejected(&r, FillReason::IocCancel);
        assert_eq!(r.bound_px, Some(347.2));
        assert!(r
            .message
            .unwrap()
            .contains("best 347.23 is beyond the bound 347.2"));
        // Priced at 345 (the market moved up during the latency): the bound
        // 345 × 1.003 = 346.035 → 346.03 is under every ask.
        let anchored = PaperOrder {
            ref_mid: Some(345.0),
            ..market(Side::Buy, OrderSize::NotionalUsd(1_000.0))
        };
        let r = run(&anchored);
        rejected(&r, FillReason::MarketOrderNoLiquidity);
        assert_eq!(
            (r.ref_px, r.bound_px, r.order_qty),
            (Some(345.0), Some(346.03), Some(2.898))
        );
        // An empty book side: no liquidity.
        let bids_only = L2Book::new(
            vec![L2Level {
                px: 347.16,
                sz: 1.0,
                n: 1,
            }],
            vec![],
            0,
        )
        .unwrap();
        let fees = fees();
        let env = FillEnv {
            rules: &rules(),
            status: MarketStatus::Open,
            position: &flat(),
            fees: &fees,
            max_book_age_ms: 5_000,
            stale_book_ok: false,
        };
        let buy = market(Side::Buy, OrderSize::Qty(1.0));
        rejected(
            &simulate_fill(&buy, &bids_only, 0, &env),
            FillReason::MarketOrderNoLiquidity,
        );
        let buy_limit = limit(Side::Buy, 1.0, 347.3);
        rejected(
            &simulate_fill(&buy_limit, &bids_only, 0, &env),
            FillReason::IocCancel,
        );
        // Its liquid side, but no mid to bound slippage: refused.
        let sell = market(Side::Sell, OrderSize::Qty(1.0));
        rejected(
            &simulate_fill(&sell, &bids_only, 0, &env),
            FillReason::MissingMid,
        );
        // …unless the order carries the mid it was priced at.
        let sell = PaperOrder {
            ref_mid: Some(347.2),
            ..sell
        };
        let r = simulate_fill(&sell, &bids_only, 0, &env);
        assert_eq!(
            (r.status, r.mid, r.slippage_bps),
            (FillStatus::Filled, None, None)
        );
    }

    #[test]
    fn reduce_only_clips_to_the_position_and_never_adds() {
        let ro = |side, size| PaperOrder {
            reduce_only: true,
            ..market(side, size)
        };
        // Flat, or the same side as the position.
        let r = run(&ro(Side::Sell, OrderSize::Qty(1.0)));
        rejected(&r, FillReason::ReduceOnly);
        assert!(r
            .message
            .unwrap()
            .starts_with("Reduce only order would increase position."));
        let long = held(2.0, 340.0);
        rejected(
            &run_with(&ro(Side::Buy, OrderSize::Qty(1.0)), &rules(), &long),
            FillReason::ReduceOnly,
        );
        // Larger than the position: clipped to it.
        let r = run_with(&ro(Side::Sell, OrderSize::Qty(5.0)), &rules(), &long);
        assert_eq!((r.status, r.order_qty), (FillStatus::Filled, Some(2.0)));
        close(r.filled_notional_usd, 694.32, "2 × 347.16");
        // A $3 dust position closes whole, under the $10 minimum.
        let dust = held(0.009, 350.0);
        let r = run_with(
            &ro(Side::Sell, OrderSize::NotionalUsd(25.0)),
            &rules(),
            &dust,
        );
        assert_eq!((r.status, r.order_qty), (FillStatus::Filled, Some(0.009)));
        close(r.filled_notional_usd, 3.124_44, "0.009 × 347.16");
        // A partial reduce under $10 is still rejected: 0.02 × 346.16 = 6.9232.
        let r = run_with(&ro(Side::Sell, OrderSize::Qty(0.02)), &rules(), &long);
        rejected(&r, FillReason::MinTradeNtl);
        // A short reduces with buys.
        let short = held(-1.0, 350.0);
        let r = run_with(&ro(Side::Buy, OrderSize::Qty(3.0)), &rules(), &short);
        assert_eq!((r.status, r.order_qty), (FillStatus::Filled, Some(1.0)));
    }

    #[test]
    fn oracle_band_rejects_far_prices_and_unknown_oracles() {
        let band = |oracle_px| VenueRules {
            oracle_band: Some(OracleBand {
                oracle_px,
                max_bps: 100.0,
            }),
            ..rules()
        };
        let buy = market(Side::Buy, OrderSize::NotionalUsd(1_000.0));
        // Bound 348.23 vs oracle 340: 242.06 bps > 100.
        let r = run_with(&buy, &band(Some(340.0)), &flat());
        rejected(&r, FillReason::Oracle);
        assert!(r
            .message
            .unwrap()
            .starts_with("Order price too far from oracle"));
        // 35.4 bps from 347: inside the band.
        assert_eq!(
            run_with(&buy, &band(Some(347.0)), &flat()).status,
            FillStatus::Filled
        );
        for unknown in [None, Some(0.0), Some(f64::NAN)] {
            rejected(
                &run_with(&buy, &band(unknown), &flat()),
                FillReason::MissingOracle,
            );
        }
    }

    #[test]
    fn oi_cap_blocks_opening_adding_and_flipping_only() {
        let capped = VenueRules {
            at_oi_cap: Some(true),
            ..rules()
        };
        let unknown = VenueRules {
            at_oi_cap: None,
            ..rules()
        };
        let long = held(1.0, 340.0);
        let buy = market(Side::Buy, OrderSize::Qty(0.5));
        let r = run_with(&buy, &capped, &flat());
        rejected(&r, FillReason::PositionIncreaseAtOpenInterestCap);
        assert!(r
            .message
            .unwrap()
            .starts_with("Order would increase open interest while open interest is capped"));
        rejected(
            &run_with(&buy, &capped, &long),
            FillReason::PositionIncreaseAtOpenInterestCap,
        );
        let flip = market(Side::Sell, OrderSize::Qty(2.0));
        rejected(
            &run_with(&flip, &capped, &long),
            FillReason::PositionFlipAtOpenInterestCap,
        );
        // Reducing passes, capped or unknown.
        let reduce = market(Side::Sell, OrderSize::Qty(0.5));
        assert_eq!(run_with(&reduce, &capped, &long).status, FillStatus::Filled);
        assert_eq!(
            run_with(&reduce, &unknown, &long).status,
            FillStatus::Filled
        );
        let ro = PaperOrder {
            reduce_only: true,
            ..flip
        };
        assert_eq!(run_with(&ro, &unknown, &long).order_qty, Some(1.0));
        // Unknown cap state refuses what would add.
        rejected(&run_with(&buy, &unknown, &flat()), FillReason::MissingOiCap);
    }

    /// `stale_book_ok` (the gate waived `book_age` for a reduce-only exit):
    /// a stale book fills, marked `stale_book`. Without the flag, or for an
    /// order that is not reduce-only, it is still refused.
    #[test]
    fn a_waived_exit_fills_at_a_stale_book() {
        let (rules, fees, book, long) = (rules(), fees(), tsla_book(), held(2.0, 340.0));
        let env = |stale_book_ok| FillEnv {
            rules: &rules,
            status: MarketStatus::Open,
            position: &long,
            fees: &fees,
            max_book_age_ms: 5_000,
            stale_book_ok,
        };
        let exit = PaperOrder {
            reduce_only: true,
            ..market(Side::Sell, OrderSize::Qty(2.0))
        };
        let r = simulate_fill(&exit, &book, 9_000, &env(true));
        assert_eq!(
            (r.status, r.stale_book, r.book_age_ms, r.filled_qty),
            (FillStatus::Filled, true, Some(9_000), 2.0)
        );
        assert_eq!(serde_json::to_value(&r).unwrap()["stale_book"], true);
        rejected(
            &simulate_fill(&exit, &book, 9_000, &env(false)),
            FillReason::StaleBook,
        );
        let entry = market(Side::Buy, OrderSize::NotionalUsd(1_000.0));
        rejected(
            &simulate_fill(&entry, &book, 9_000, &env(true)),
            FillReason::StaleBook,
        );
        // A fresh book is never marked, and the field stays off the wire.
        let fresh = simulate_fill(&exit, &book, 1_000, &env(true));
        assert!(!fresh.stale_book);
        assert!(serde_json::to_value(&fresh)
            .unwrap()
            .get("stale_book")
            .is_none());
    }

    #[test]
    fn market_state_and_book_age_refuse_before_pricing() {
        let o = market(Side::Buy, OrderSize::NotionalUsd(1_000.0));
        let fees = fees();
        let book = tsla_book();
        for (status, reason) in [
            (MarketStatus::Halted, FillReason::MarketHalted),
            (MarketStatus::Closed, FillReason::MarketClosed),
            (MarketStatus::Delisted, FillReason::Delisted),
        ] {
            let env = FillEnv {
                rules: &rules(),
                status,
                position: &flat(),
                fees: &fees,
                max_book_age_ms: 5_000,
                stale_book_ok: false,
            };
            let r = simulate_fill(&o, &book, 0, &env);
            rejected(&r, reason);
            assert!(r.message.unwrap().contains(TSLA), "full id in the message");
        }
        let env = FillEnv {
            rules: &rules(),
            status: MarketStatus::Open,
            position: &flat(),
            fees: &fees,
            max_book_age_ms: 5_000,
            stale_book_ok: false,
        };
        let r = simulate_fill(&o, &book, 5_001, &env);
        rejected(&r, FillReason::StaleBook);
        assert_eq!((r.bound_px, r.book_age_ms), (None, Some(5_001)));
        assert_eq!(
            simulate_fill(&o, &book, 5_000, &env).status,
            FillStatus::Filled
        );
        // A deserialized book skips `L2Book::new`: a crossed one is refused.
        let lvl = |px| L2Level { px, sz: 1.0, n: 1 };
        let crossed = L2Book {
            bids: vec![lvl(347.3)],
            asks: vec![lvl(347.2)],
            venue_ts_ms: 0,
        };
        let r = simulate_fill(&o, &crossed, 0, &env);
        rejected(&r, FillReason::BadBook);
        assert!(r.message.unwrap().contains("crossed"));
    }

    #[test]
    fn malformed_orders_are_refused() {
        let base = market(Side::Buy, OrderSize::NotionalUsd(1_000.0));
        let bad = [
            PaperOrder {
                size: OrderSize::Qty(f64::NAN),
                ..base.clone()
            },
            PaperOrder {
                size: OrderSize::NotionalUsd(0.0),
                ..base.clone()
            },
            PaperOrder {
                max_slippage_bps: 0.0,
                ..base.clone()
            },
            PaperOrder {
                max_slippage_bps: 10_000.0,
                ..base.clone()
            },
            PaperOrder {
                ref_mid: Some(-1.0),
                ..base.clone()
            },
            PaperOrder {
                limit_px: Some(347.3),
                ..base.clone()
            },
            PaperOrder {
                kind: OrderKind::Limit,
                ..base.clone()
            },
            PaperOrder {
                client_order_id: " ".to_string(),
                ..base.clone()
            },
        ];
        for o in &bad {
            rejected(&run(o), FillReason::InvalidOrder);
        }
        let other = Position::flat("hyperliquid:xyz:NVDA", "company:nvidia", "hyperliquid");
        let r = run_with(&base, &rules(), &other);
        rejected(&r, FillReason::InvalidOrder);
        assert!(r.message.unwrap().contains("hyperliquid:xyz:NVDA"));
        let nan = Position {
            qty: f64::NAN,
            ..flat()
        };
        rejected(&run_with(&base, &rules(), &nan), FillReason::InvalidOrder);
        let wide = VenueRules {
            sz_decimals: 7,
            ..rules()
        };
        rejected(&run_with(&base, &wide, &flat()), FillReason::InvalidOrder);
    }

    #[test]
    fn a_fill_books_into_the_ledger() {
        let r = run(&market(Side::Buy, OrderSize::NotionalUsd(10_000.0)));
        let fill = r.ledger_fill(TESLA, 1_790_775_353_605).unwrap();
        assert_eq!(
            (fill.instrument.as_str(), fill.venue.as_str(), fill.side),
            (TSLA, "hyperliquid", Side::Buy)
        );
        let mut a = PaperAccount::new("paper-test", 100_000.0).unwrap();
        a.apply_fill(&fill).unwrap();
        let p = &a.positions[TSLA];
        close(p.qty, 28.802, "qty");
        close(p.avg_px.unwrap(), 347.232_826_539_824, "avg");
        close(a.cash_usd, 100_000.0 - 0.900_089_988_3, "cash pays the fee");
        let none = run(&limit(Side::Buy, 1.0, 347.2));
        assert_eq!(none.ledger_fill(TESLA, 0), None);
    }

    #[test]
    fn reasons_serialize_as_hl_codes_and_engine_names() {
        for r in FillReason::ALL {
            assert_eq!(serde_json::to_value(r).unwrap(), r.as_str(), "{r:?}");
            let back: FillReason = serde_json::from_value(r.as_str().into()).unwrap();
            assert_eq!(back, r);
            let venue = r.as_str().starts_with(char::is_uppercase);
            assert_eq!(r.hl_error().is_some(), venue, "{r:?}");
        }
        let r = run(&market(Side::Buy, OrderSize::NotionalUsd(10.0)));
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["reason"], "MinTradeNtl");
        assert_eq!(v["status"], "rejected");
        assert_eq!(v["instrument"], TSLA);
        assert_eq!(serde_json::from_value::<FillResult>(v).unwrap(), r);
    }

    /// Missing data, book age, liquidity and price bounds are transient;
    /// size / lot rules, a bad order and delisting are final.
    #[test]
    fn reasons_split_into_transient_and_final() {
        let transient: Vec<&str> = FillReason::ALL
            .into_iter()
            .filter(|r| r.is_transient())
            .map(FillReason::as_str)
            .collect();
        assert_eq!(
            transient,
            [
                "IocCancel",
                "MarketOrderNoLiquidity",
                "Oracle",
                "PositionIncreaseAtOpenInterestCap",
                "PositionFlipAtOpenInterestCap",
                "market_halted",
                "market_closed",
                "stale_book",
                "bad_book",
                "missing:mid",
                "missing:oracle",
                "missing:at_oi_cap",
                "bound",
                "depth"
            ]
        );
        for r in [
            FillReason::MinTradeNtl,
            FillReason::Tick,
            FillReason::ReduceOnly,
            FillReason::InvalidOrder,
            FillReason::OrderType,
            FillReason::Delisted,
        ] {
            assert!(!r.is_transient(), "{r:?}");
        }
    }

    #[test]
    fn orders_round_trip_and_reject_unknown_fields() {
        let o: PaperOrder = serde_json::from_str(
            r#"{"client_order_id":"c1","instrument":"hyperliquid:xyz:TSLA","side":"buy",
                "size":{"notional_usd":25},"kind":"market","tif":"ioc","reduce_only":false,
                "max_slippage_bps":30}"#,
        )
        .unwrap();
        assert_eq!(o.size, OrderSize::NotionalUsd(25.0));
        assert_eq!(o.instrument.to_string(), TSLA);
        let back: PaperOrder = serde_json::from_value(serde_json::to_value(&o).unwrap()).unwrap();
        assert_eq!(back, o);
        assert!(
            serde_json::from_str::<PaperOrder>(
                r#"{"client_order_id":"c1","instrument":"hyperliquid:xyz:TSLA","side":"buy",
                "size":{"qty":1},"kind":"market","tif":"gtc","reduce_only":false,
                "max_slippage_bps":30}"#
            )
            .is_err(),
            "gtc is P1"
        );
        assert!(serde_json::from_str::<PaperOrder>(
            r#"{"client_order_id":"c1","instrument":"hyperliquid:xyz:TSLA","side":"buy",
                "size":{"qty":1},"kind":"market","tif":"ioc","reduce_only":false,
                "max_slippage_bps":30,"post_only":true}"#
        )
        .is_err());
    }

    #[test]
    fn latency_jitter_is_uniform_around_the_base() {
        for (base, jitter, r, want) in [
            (250, 100, 0.5, 250),
            (250, 100, 0.0, 150),
            (250, 100, 1.0, 350),
            (250, 100, 0.25, 200),
            (250, 100, f64::NAN, 250),
            (250, 100, 1.5, 250),
            (250, 100, -0.1, 250),
            (250, 0, 0.9, 250),
            (100, 100, 0.0, 0),
            (0, 0, 0.3, 0),
        ] {
            assert_eq!(
                jittered_latency_ms(base, jitter, r),
                want,
                "{base} ± {jitter} @ {r}"
            );
        }
    }
}
