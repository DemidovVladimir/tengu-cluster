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
    /// to get out.
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
#[path = "paper_tests.rs"]
mod tests;
