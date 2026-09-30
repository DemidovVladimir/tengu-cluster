//! Execution costs (`risk-calc-costs`, absorbs `hl-fee-model`): HL tick /
//! lot rounding, fee schedules, funding carry, EVM gas, the taker cost of a
//! depth walk and edge after costs. Pure. Every rate is a documented
//! constant below or an input (knob / market row) — nothing is guessed.
//!
//! | Piece | Rule | Source (Hyperliquid docs, read 2026-09-30) |
//! |---|---|---|
//! | Price | ≤ 5 significant figures and ≤ `MAX_DECIMALS − szDecimals` decimals (perps 6, spot 8); integer prices always valid (`1234.5` ok, `1234.56` not; `0.001234` ok, `0.0012345` not) | "Tick and lot size" |
//! | Size | ≤ `szDecimals` decimals | "Tick and lot size" |
//! | Tier rates (bps, taker / maker) | perps 4.5 / 1.5 · 4.0 / 1.2 · 3.5 / 0.8 · 3.0 / 0.4 · 2.8 / 0 · 2.6 / 0 · 2.4 / 0 (tiers 0–6); spot 7.0 / 4.0 · 6.0 / 3.0 · 5.0 / 2.0 · 4.0 / 1.0 · 3.5 / 0 · 3.0 / 0 · 2.5 / 0 | "Fees" |
//! | Staking | 0 · 5 · 10 · 15 · 20 · 30 · 40 % off both rates (none, Wood … Diamond) | "Fees" § Staking tiers |
//! | HIP-3 | deployer fee scale `s`: × (1 + s) below 1, × 2s from 1 ("0-300 %"; above 100 % "the protocol fee is also increased to be equal to the deployer fee"); growth mode × 0.1 ("≥ 90 % reduction on the all-in fees", on top of staking); a maker rebate is never deployer-scaled | "Fees" § Fee formula for developers (`feeRates`, ported 1:1 in [`hl_fee_schedule`]) |
//! | Other `feeRates` terms | spot stable pair × 0.2; referral discount; aligned quote: taker × ((1 − d) × 0.8 + d), rebate × ((1 − d) × 1.5 + d), deployer share d = s / (1 + s) below 1, else 0.5 | same |
//! | Funding | hourly payment = size × oracle × rate; positive rate ⇒ longs pay shorts | "Funding" |
//!
//! Worked (tier 0, no staking): `xyz:TSLA` (`deployerFeeScale` "1.0",
//! `growthMode` "enabled") = 4.5 × 2 × 0.1 = 0.9 bps taker; `xyz:MSTR`
//! (scale 1.0, no growth mode) = 9 bps; a validator-operated perp = 4.5 bps.
//! Formula-derived — confirm against a testnet fill (M3b). Maker-rebate
//! volume tiers are not tabled: pass a negative [`HlUserRates::maker`] read
//! from `userFees`.

// Consumers land in wave W1 (`hl-ctx-tool` taker_fee_bps,
// `risk-calc-tools`, `risk-paper-fill-engine`).
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

use crate::domain::book::{BookError, L2Book, Side, Walk, WalkTarget};

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{0}")]
pub struct CostError(pub String);

impl From<BookError> for CostError {
    fn from(e: BookError) -> Self {
        CostError(e.to_string())
    }
}

fn fail<T>(msg: impl Into<String>) -> Result<T, CostError> {
    Err(CostError(msg.into()))
}

fn finite(name: &str, v: f64) -> Result<f64, CostError> {
    if v.is_finite() {
        Ok(v)
    } else {
        fail(format!("{name} {v} is not finite"))
    }
}

// ── HL tick / lot ────────────────────────────────────────────────

/// HL product line (sets `MAX_DECIMALS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HlKind {
    Perp,
    Spot,
}

impl HlKind {
    /// `MAX_DECIMALS`: 6 perps, 8 spot.
    pub fn max_decimals(self) -> u32 {
        match self {
            HlKind::Perp => 6,
            HlKind::Spot => 8,
        }
    }
}

/// Significant figures a non-integer HL price may carry.
pub const HL_PRICE_SIG_FIGS: u32 = 5;

/// Rounding direction for [`hl_round_px`] / [`hl_round_sz`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Round {
    Nearest,
    /// Toward zero (a buy limit that must not pay more; an order size that
    /// must not exceed a budget).
    Down,
    /// Away from zero (a sell limit that must not receive less).
    Up,
}

/// Decimals a price near `px` may carry: `min(MAX_DECIMALS − szDecimals,
/// 4 − exponent)`, at least 0 (integers are always valid). `None` for a
/// non-positive / non-finite `px` or `sz_decimals > MAX_DECIMALS`.
pub fn hl_price_decimals(px: f64, sz_decimals: u32, kind: HlKind) -> Option<u32> {
    let max = kind.max_decimals().checked_sub(sz_decimals)?;
    if !(px.is_finite() && px > 0.0) {
        return None;
    }
    let by_sig_figs = HL_PRICE_SIG_FIGS as i32 - 1 - decimal_exponent(px);
    Some(by_sig_figs.clamp(0, max as i32) as u32)
}

/// `px` rounded to a valid HL price. `None` when [`hl_price_decimals`] is,
/// or when rounding down reaches 0.
pub fn hl_round_px(px: f64, sz_decimals: u32, kind: HlKind, round: Round) -> Option<f64> {
    let decimals = hl_price_decimals(px, sz_decimals, kind)?;
    let out = round_to(px, decimals, round);
    (out > 0.0).then_some(out)
}

/// `sz` rounded to `sz_decimals` decimals (may be 0 — callers refuse a
/// zero-size order). `None` for a negative / non-finite `sz`.
pub fn hl_round_sz(sz: f64, sz_decimals: u32, round: Round) -> Option<f64> {
    (sz.is_finite() && sz >= 0.0).then(|| round_to(sz, sz_decimals, round))
}

/// Exact check of a wire price string (plain decimal, > 0) against the
/// tick rule.
pub fn hl_px_ok(px: &str, sz_decimals: u32, kind: HlKind) -> bool {
    let Some(max) = kind.max_decimals().checked_sub(sz_decimals) else {
        return false;
    };
    let Some((int, frac)) = split_decimal(px) else {
        return false;
    };
    let frac = frac.trim_end_matches('0');
    if frac.is_empty() {
        return true; // integer prices are always valid
    }
    let digits = format!("{}{frac}", int.trim_start_matches('0'));
    let sig_figs = digits.trim_start_matches('0').len();
    sig_figs as u32 <= HL_PRICE_SIG_FIGS && frac.len() as u32 <= max
}

/// Exact check of a wire size string (plain decimal, > 0).
pub fn hl_sz_ok(sz: &str, sz_decimals: u32) -> bool {
    split_decimal(sz)
        .is_some_and(|(_, frac)| frac.trim_end_matches('0').len() as u32 <= sz_decimals)
}

/// Wire form of a rounded number: `decimals` digits, trailing zeros
/// removed (HL: "if implementing signing, trailing zeroes should be removed").
pub fn hl_wire(x: f64, decimals: u32) -> String {
    let s = format!("{x:.*}", decimals as usize);
    let s = if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s
    };
    if s == "-0" {
        "0".to_string()
    } else {
        s
    }
}

/// Power of ten of the leading digit, from the shortest round-trip decimal
/// form (`347.16` → 2, `0.001234` → −3) — no `log10` edge cases.
fn decimal_exponent(x: f64) -> i32 {
    let s = format!("{:e}", x.abs());
    s.rsplit('e')
        .next()
        .and_then(|e| e.parse().ok())
        .unwrap_or(0)
}

/// `x` rounded to `decimals`; values within 1e-9 (relative) of a grid point
/// snap to it first, so f64 noise (0.1 + 0.2) never moves a tick.
fn round_to(x: f64, decimals: u32, round: Round) -> f64 {
    let scale = 10f64.powi(decimals as i32);
    let scaled = x * scale;
    let nearest = scaled.round();
    let units = if (scaled - nearest).abs() <= 1e-9 * nearest.abs().max(1.0) {
        nearest
    } else {
        match round {
            Round::Nearest => nearest,
            Round::Down => scaled.trunc(),
            Round::Up => scaled.trunc() + scaled.signum(),
        }
    };
    let v = units / scale;
    format!("{v:.*}", decimals as usize).parse().unwrap_or(v)
}

/// `"123.450"` → `("123", "450")`: digits and at most one `.`, value > 0;
/// no sign, no exponent.
fn split_decimal(s: &str) -> Option<(&str, &str)> {
    let s = s.trim();
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    let digits = |p: &str| p.bytes().all(|b| b.is_ascii_digit());
    if (int.is_empty() && frac.is_empty()) || !digits(int) || !digits(frac) {
        return None;
    }
    let zero = int.bytes().all(|b| b == b'0') && frac.bytes().all(|b| b == b'0');
    (!zero).then_some((int, frac))
}

// ── Fees ─────────────────────────────────────────────────────────

/// Tier 0–6 (taker, maker) bps, perps.
pub const HL_PERP_TIERS_BPS: [(f64, f64); 7] = [
    (4.5, 1.5),
    (4.0, 1.2),
    (3.5, 0.8),
    (3.0, 0.4),
    (2.8, 0.0),
    (2.6, 0.0),
    (2.4, 0.0),
];
/// Tier 0–6 (taker, maker) bps, spot.
pub const HL_SPOT_TIERS_BPS: [(f64, f64); 7] = [
    (7.0, 4.0),
    (6.0, 3.0),
    (5.0, 2.0),
    (4.0, 1.0),
    (3.5, 0.0),
    (3.0, 0.0),
    (2.5, 0.0),
];
/// Staking discounts: none, Wood, Bronze, Silver, Gold, Platinum, Diamond.
pub const HL_STAKING_DISCOUNT_PCT: [f64; 7] = [0.0, 5.0, 10.0, 15.0, 20.0, 30.0, 40.0];
/// Growth mode multiplier on HIP-3 all-in fees and rebates.
pub const HL_GROWTH_MODE_SCALE: f64 = 0.1;
/// Documented ceiling of `deployerFeeScale` ("0-300 %").
pub const HL_MAX_DEPLOYER_FEE_SCALE: f64 = 3.0;
/// Spot pairs between two quote assets.
pub const HL_STABLE_PAIR_SCALE: f64 = 0.2;

/// Liquidity role of a fill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Liquidity {
    Taker,
    Maker,
}

/// Effective rates in bps of notional; positive = paid, negative = rebate.
/// Venues without a formula (CEX) give it as a required knob, no defaults.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeeSchedule {
    pub taker_bps: f64,
    pub maker_bps: f64,
}

impl FeeSchedule {
    pub fn new(taker_bps: f64, maker_bps: f64) -> Result<Self, CostError> {
        let s = Self {
            taker_bps,
            maker_bps,
        };
        s.validate()?;
        Ok(s)
    }

    /// Finite rates, taker ≥ 0 (a taker is never paid).
    pub fn validate(&self) -> Result<(), CostError> {
        finite("maker_bps", self.maker_bps)?;
        if !(finite("taker_bps", self.taker_bps)? >= 0.0) {
            return fail(format!("taker_bps {} is negative", self.taker_bps));
        }
        Ok(())
    }

    pub fn bps(&self, liquidity: Liquidity) -> f64 {
        match liquidity {
            Liquidity::Taker => self.taker_bps,
            Liquidity::Maker => self.maker_bps,
        }
    }

    /// Fee on `|notional_usd|`; negative = rebate.
    pub fn fee_usd(&self, liquidity: Liquidity, notional_usd: f64) -> f64 {
        notional_usd.abs() * self.bps(liquidity) / 1e4
    }
}

/// A user's base rates as fractions of notional — HL `userFees`
/// `userCrossRate` / `userAddRate` (spot: `userSpotCrossRate` /
/// `userSpotAddRate`): the tier rate after the staking discount.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct HlUserRates {
    pub taker: f64,
    pub maker: f64,
}

impl HlUserRates {
    /// Paper account: tier row × (1 − staking discount). `tier` 0–6;
    /// `staking_discount_pct` one of [`HL_STAKING_DISCOUNT_PCT`].
    pub fn from_tier(kind: HlKind, tier: u8, staking_discount_pct: f64) -> Result<Self, CostError> {
        let table = match kind {
            HlKind::Perp => &HL_PERP_TIERS_BPS,
            HlKind::Spot => &HL_SPOT_TIERS_BPS,
        };
        let Some(&(taker_bps, maker_bps)) = table.get(tier as usize) else {
            return fail(format!("fee tier {tier} is not 0-6"));
        };
        if !HL_STAKING_DISCOUNT_PCT.contains(&staking_discount_pct) {
            return fail(format!(
                "staking_discount_pct {staking_discount_pct} is not one of {HL_STAKING_DISCOUNT_PCT:?}"
            ));
        }
        let keep = 1.0 - staking_discount_pct / 100.0;
        Ok(Self {
            taker: taker_bps / 1e4 * keep,
            maker: maker_bps / 1e4 * keep,
        })
    }
}

/// Market side of `feeRates`, from the `mkt_instrument/1` row.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HlFeeMarket {
    /// HIP-3 perps carry `deployerFeeScale` / `growthMode` from `meta`;
    /// a validator-operated perp is `deployer_fee_scale = 0`, no growth mode.
    Perp {
        deployer_fee_scale: f64,
        growth_mode: bool,
    },
    Spot {
        /// Both tokens are spot quote assets.
        stable_pair: bool,
    },
}

/// HL effective fee rates — a 1:1 port of `feeRates` ("Fees" § Fee formula
/// for developers). `referral_discount` = `userFees.activeReferralDiscount`
/// (a fraction, 0 on paper); `aligned_quote` = the market's quote token is
/// aligned.
pub fn hl_fee_schedule(
    user: HlUserRates,
    referral_discount: f64,
    aligned_quote: bool,
    market: HlFeeMarket,
) -> Result<FeeSchedule, CostError> {
    if !(finite("taker rate", user.taker)? >= 0.0) {
        return fail(format!("taker rate {} is negative", user.taker));
    }
    finite("maker rate", user.maker)?;
    if !(0.0..1.0).contains(&finite("referral_discount", referral_discount)?) {
        return fail(format!(
            "referral_discount {referral_discount} is not in [0, 1)"
        ));
    }
    let (stable, hip3, deployer_share, growth) = match market {
        HlFeeMarket::Spot { stable_pair } => (
            if stable_pair {
                HL_STABLE_PAIR_SCALE
            } else {
                1.0
            },
            1.0,
            0.0,
            1.0,
        ),
        HlFeeMarket::Perp {
            deployer_fee_scale: s,
            growth_mode,
        } => {
            if !(0.0..=HL_MAX_DEPLOYER_FEE_SCALE).contains(&finite("deployer_fee_scale", s)?) {
                return fail(format!(
                    "deployer_fee_scale {s} is not in [0, {HL_MAX_DEPLOYER_FEE_SCALE}]"
                ));
            }
            (
                1.0,
                if s < 1.0 { s + 1.0 } else { s * 2.0 },
                if s < 1.0 { s / (1.0 + s) } else { 0.5 },
                if growth_mode {
                    HL_GROWTH_MODE_SCALE
                } else {
                    1.0
                },
            )
        }
    };
    let mut maker_bps = user.maker * 1e4 * stable * growth;
    if maker_bps > 0.0 {
        maker_bps *= hip3 * (1.0 - referral_discount);
    } else if aligned_quote {
        maker_bps *= (1.0 - deployer_share) * 1.5 + deployer_share;
    }
    let mut taker_bps = user.taker * 1e4 * stable * hip3 * growth * (1.0 - referral_discount);
    if aligned_quote {
        taker_bps *= (1.0 - deployer_share) * 0.8 + deployer_share;
    }
    FeeSchedule::new(taker_bps, maker_bps)
}

// ── Funding, gas ─────────────────────────────────────────────────

/// Expected funding carry of holding `side` for `hours` at `rate_1h` (HL
/// `funding`, per hour), bps of oracle notional; positive = the position
/// pays. HL: payment = size × oracle × rate, positive ⇒ longs pay. Realized
/// funding accrues per hour boundary in the paper ledger.
pub fn funding_carry_bps(rate_1h: f64, hours: f64, side: Side) -> Result<f64, CostError> {
    finite("rate_1h", rate_1h)?;
    if !(finite("hours", hours)? >= 0.0) {
        return fail(format!("hours {hours} is negative"));
    }
    Ok(side.sign() * rate_1h * hours * 1e4)
}

/// EVM transaction cost in USD: `gas_units × gas_price_wei / 1e18 ×
/// eth_usd`. Robinhood Chain (Arbitrum Orbit) folds the L1 data fee into
/// `gasUsed`, so this is the whole fee.
pub fn gas_usd(gas_units: u64, gas_price_wei: u128, eth_usd: f64) -> Result<f64, CostError> {
    if !(finite("eth_usd", eth_usd)? > 0.0) {
        return fail(format!("eth_usd {eth_usd} is not > 0"));
    }
    Ok(gas_units as f64 * gas_price_wei as f64 / 1e18 * eth_usd)
}

// ── Taker cost, round trip, edge ─────────────────────────────────

/// Cost of taking `target` from a book: the walk plus the taker fee.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TakerCost {
    pub walk: Walk,
    pub fee_bps: f64,
    /// Fee on the filled notional.
    pub fee_usd: f64,
    /// Slippage vs mid + fee; `None` without a fill or a mid (never 0).
    pub total_cost_bps: Option<f64>,
}

pub fn taker_cost(
    book: &L2Book,
    side: Side,
    target: WalkTarget,
    fees: &FeeSchedule,
) -> Result<TakerCost, CostError> {
    fees.validate()?;
    let walk = book.walk(side, target, None)?;
    let fee_bps = fees.bps(Liquidity::Taker);
    Ok(TakerCost {
        fee_usd: fees.fee_usd(Liquidity::Taker, walk.filled_notional),
        total_cost_bps: walk.slippage_bps_vs_mid.map(|s| s + fee_bps),
        fee_bps,
        walk,
    })
}

/// One round trip (enter + exit); every bps field is relative to
/// `notional_usd`, positive = cost.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RoundTripCost {
    pub notional_usd: f64,
    pub entry_fee_bps: f64,
    pub exit_fee_bps: f64,
    /// Slippage vs mid of each leg ([`Walk::slippage_bps_vs_mid`]).
    pub entry_slippage_bps: f64,
    pub exit_slippage_bps: f64,
    /// [`funding_carry_bps`] over the holding period (negative = received).
    pub carry_bps: f64,
    /// Gas of both legs (EVM venues; 0 on HL).
    pub gas_usd: f64,
}

impl RoundTripCost {
    /// Σ fees + slippage + carry + gas as bps of the notional.
    pub fn total_bps(&self) -> Result<f64, CostError> {
        if !(finite("notional_usd", self.notional_usd)? > 0.0) {
            return fail(format!("notional_usd {} is not > 0", self.notional_usd));
        }
        if !(finite("gas_usd", self.gas_usd)? >= 0.0) {
            return fail(format!("gas_usd {} is negative", self.gas_usd));
        }
        let mut total = self.gas_usd / self.notional_usd * 1e4;
        for (name, v) in [
            ("entry_fee_bps", self.entry_fee_bps),
            ("exit_fee_bps", self.exit_fee_bps),
            ("entry_slippage_bps", self.entry_slippage_bps),
            ("exit_slippage_bps", self.exit_slippage_bps),
            ("carry_bps", self.carry_bps),
        ] {
            total += finite(name, v)?;
        }
        Ok(total)
    }
}

/// `gross_edge_bps − cost.total_bps()`. The gross edge is measured mid to
/// mid (or vs the reference), so slippage vs mid covers the half spreads.
pub fn edge_after_costs_bps(gross_edge_bps: f64, cost: &RoundTripCost) -> Result<f64, CostError> {
    Ok(finite("gross_edge_bps", gross_edge_bps)? - cost.total_bps()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::book::fixture::tsla_book;

    fn close(a: f64, b: f64, what: &str) {
        assert!(
            (a - b).abs() <= 1e-9 * b.abs().max(1.0),
            "{what}: got {a}, want {b}"
        );
    }

    fn tier0(kind: HlKind) -> HlUserRates {
        HlUserRates::from_tier(kind, 0, 0.0).unwrap()
    }

    fn perp(s: f64, growth_mode: bool) -> HlFeeMarket {
        HlFeeMarket::Perp {
            deployer_fee_scale: s,
            growth_mode,
        }
    }

    // ── tick / lot ──

    #[test]
    fn hl_doc_price_examples() {
        use HlKind::{Perp, Spot};
        // "Tick and lot size", perps (szDecimals 0 unless noted).
        assert!(hl_px_ok("1234.5", 0, Perp));
        assert!(
            !hl_px_ok("1234.56", 0, Perp),
            "too many significant figures"
        );
        assert!(hl_px_ok("0.001234", 0, Perp));
        assert!(!hl_px_ok("0.0012345", 0, Perp), "more than 6 decimals");
        assert!(hl_px_ok("0.01234", 1, Perp));
        assert!(!hl_px_ok("0.012345", 1, Perp));
        // Integer prices are always valid.
        assert!(hl_px_ok("123456", 0, Perp));
        assert!(hl_px_ok("123456.0", 0, Perp));
        assert!(!hl_px_ok("12345.6", 0, Perp));
        // Spot: 0.0001234 valid for szDecimals 0 or 1; 8 − 2 = 6 decimals is too few.
        assert!(hl_px_ok("0.0001234", 0, Spot));
        assert!(hl_px_ok("0.0001234", 1, Spot));
        assert!(!hl_px_ok("0.0001234", 2, Spot));
        assert!(!hl_px_ok("0.0001234", 3, Spot));
        // xyz:TSLA (szDecimals 3): fixture prices are valid.
        for px in ["347.16", "347.1", "347", "346.93", "347.44"] {
            assert!(hl_px_ok(px, 3, Perp), "{px}");
        }
        assert!(!hl_px_ok("347.165", 3, Perp), "6 significant figures");
        assert!(!hl_px_ok("34.7165", 3, Perp), "4 decimals > 6 − 3");
        // Not plain positive decimals.
        for bad in ["", ".", "0", "0.000", "-1", "1e3", "abc", "1.2.3", " "] {
            assert!(!hl_px_ok(bad, 0, Perp), "{bad:?}");
        }
        assert!(!hl_px_ok("1", 7, Perp), "szDecimals above MAX_DECIMALS");
    }

    #[test]
    fn hl_size_rule() {
        assert!(hl_sz_ok("0.288", 3));
        assert!(hl_sz_ok("1.000", 3));
        assert!(hl_sz_ok("20", 0));
        assert!(!hl_sz_ok("0.2885", 3));
        assert!(!hl_sz_ok("1.5", 0));
        assert!(!hl_sz_ok("0", 3));
        assert!(!hl_sz_ok("-1", 3));
    }

    #[test]
    fn price_decimals_by_magnitude() {
        use HlKind::{Perp, Spot};
        assert_eq!(hl_price_decimals(347.16, 3, Perp), Some(2));
        assert_eq!(
            hl_price_decimals(34.716, 3, Perp),
            Some(3),
            "capped by 6 − 3"
        );
        assert_eq!(hl_price_decimals(0.001234, 0, Perp), Some(6));
        assert_eq!(hl_price_decimals(1234.56, 0, Perp), Some(1));
        assert_eq!(hl_price_decimals(123_456.7, 0, Perp), Some(0));
        assert_eq!(
            hl_price_decimals(1000.0, 0, Perp),
            Some(1),
            "exact power of ten"
        );
        assert_eq!(hl_price_decimals(0.0001234, 2, Spot), Some(6));
        assert_eq!(hl_price_decimals(0.0, 0, Perp), None);
        assert_eq!(hl_price_decimals(f64::NAN, 0, Perp), None);
        assert_eq!(hl_price_decimals(1.0, 7, Perp), None);
    }

    #[test]
    fn rounding_prices_and_sizes() {
        use HlKind::Perp;
        let px = |x, sz, r| hl_round_px(x, sz, Perp, r).unwrap();
        assert_eq!(px(347.1649, 3, Round::Nearest), 347.16);
        assert_eq!(px(347.1649, 3, Round::Down), 347.16);
        assert_eq!(px(347.1649, 3, Round::Up), 347.17);
        assert_eq!(px(1234.56, 0, Round::Nearest), 1234.6);
        assert_eq!(px(123_456.7, 0, Round::Nearest), 123_457.0);
        assert_eq!(px(123_456.7, 0, Round::Down), 123_456.0);
        assert_eq!(px(0.0012345, 0, Round::Down), 0.001234);
        assert_eq!(px(0.0012345, 0, Round::Up), 0.001235);
        assert_eq!(
            px(99_999.7, 0, Round::Up),
            100_000.0,
            "integer after a carry"
        );
        // f64 noise never moves a tick.
        assert_eq!(px(0.07, 0, Round::Up), 0.07);
        assert_eq!(px(0.1 + 0.2, 0, Round::Down), 0.3);
        assert_eq!(
            hl_round_px(0.000_000_1, 0, Perp, Round::Down),
            None,
            "rounds to 0"
        );

        assert_eq!(hl_round_sz(0.28799, 3, Round::Down), Some(0.287));
        assert_eq!(hl_round_sz(0.28799, 3, Round::Nearest), Some(0.288));
        assert_eq!(hl_round_sz(0.1 + 0.2, 1, Round::Down), Some(0.3));
        assert_eq!(hl_round_sz(0.0004, 3, Round::Down), Some(0.0));
        assert_eq!(hl_round_sz(-1.0, 3, Round::Down), None);

        assert_eq!(hl_wire(347.16, 6), "347.16");
        assert_eq!(hl_wire(100.0, 3), "100");
        assert_eq!(hl_wire(0.001234, 6), "0.001234");
        assert_eq!(hl_wire(-0.0, 2), "0");
    }

    #[test]
    fn rounded_prices_are_valid_and_bracket_the_input() {
        let inputs = [
            347.1649,
            0.012_345_678,
            9.999_97,
            99_999.95,
            123_456.789,
            1.000_001,
            0.5,
            42.0,
            0.000_123_456_7,
            2_718.281_828,
        ];
        for kind in [HlKind::Perp, HlKind::Spot] {
            for sz in 0..=4 {
                for &x in &inputs {
                    // Below one tick (e.g. 0.0001234567 with 6 − 4 decimals)
                    // Down / Nearest have no valid price; Up always does.
                    let down = hl_round_px(x, sz, kind, Round::Down);
                    let near = hl_round_px(x, sz, kind, Round::Nearest);
                    let up = hl_round_px(x, sz, kind, Round::Up).unwrap();
                    for r in [down, near, Some(up)].into_iter().flatten() {
                        let wire = hl_wire(r, kind.max_decimals());
                        assert!(hl_px_ok(&wire, sz, kind), "{kind:?} sz {sz} {x} → {wire}");
                    }
                    if let Some(d) = down {
                        assert!(d <= x && x <= up, "{kind:?} sz {sz}: {d} <= {x} <= {up}");
                    }
                    if let Some(n) = near {
                        assert!(down.unwrap_or(0.0) <= n && n <= up, "{kind:?} sz {sz} {x}");
                    }
                }
            }
        }
    }

    // ── fees ──

    #[test]
    fn hip3_fee_formula_matches_documented_examples() {
        let t = tier0(HlKind::Perp);
        let fee = |m| hl_fee_schedule(t, 0.0, false, m).unwrap();
        // Validator-operated perp.
        let f = fee(perp(0.0, false));
        close(f.taker_bps, 4.5, "validator taker");
        close(f.maker_bps, 1.5, "validator maker");
        // xyz:TSLA today: deployerFeeScale "1.0", growthMode "enabled".
        let f = fee(perp(1.0, true));
        close(f.taker_bps, 0.9, "xyz growth taker");
        close(f.maker_bps, 0.3, "xyz growth maker");
        // xyz:MSTR today: scale 1.0, no growth mode.
        let f = fee(perp(1.0, false));
        close(f.taker_bps, 9.0, "xyz no growth taker");
        close(f.maker_bps, 3.0, "xyz no growth maker");
        // A para market at scale 0.5: × 1.5.
        close(fee(perp(0.5, false)).taker_bps, 6.75, "scale 0.5");
        // 300 %: protocol fee raised to the deployer's ⇒ × 6.
        close(fee(perp(3.0, false)).taker_bps, 27.0, "scale 3");
        // "baseline all-in taker rate under growth mode will be between 0.0045%-0.009%".
        close(fee(perp(0.0, true)).taker_bps, 0.45, "growth floor");
        close(fee(perp(1.0, true)).taker_bps, 0.9, "growth ceiling");
    }

    #[test]
    fn tier_and_staking_rows_match_the_fee_table() {
        // Diamond (40 %) column of the perps table: 0.0270 % … 0.0144 %.
        let diamond = [2.7, 2.4, 2.1, 1.8, 1.68, 1.56, 1.44];
        let diamond_maker = [0.9, 0.72, 0.48, 0.24, 0.0, 0.0, 0.0];
        for tier in 0..7u8 {
            let u = HlUserRates::from_tier(HlKind::Perp, tier, 40.0).unwrap();
            let f = hl_fee_schedule(u, 0.0, false, perp(0.0, false)).unwrap();
            close(
                f.taker_bps,
                diamond[tier as usize],
                &format!("tier {tier} taker"),
            );
            close(
                f.maker_bps,
                diamond_maker[tier as usize],
                &format!("tier {tier} maker"),
            );
        }
        // Wood (5 %), tier 0: 0.04275 % / 0.01425 % (tabled rounded: 0.0428 / 0.0143).
        let u = HlUserRates::from_tier(HlKind::Perp, 0, 5.0).unwrap();
        close(u.taker * 1e4, 4.275, "wood taker");
        close(u.maker * 1e4, 1.425, "wood maker");
        // Spot tier 3, Gold (20 %): 0.0320 % / 0.0080 %.
        let u = HlUserRates::from_tier(HlKind::Spot, 3, 20.0).unwrap();
        let f = hl_fee_schedule(u, 0.0, false, HlFeeMarket::Spot { stable_pair: false }).unwrap();
        close(f.taker_bps, 3.2, "spot gold taker");
        close(f.maker_bps, 0.8, "spot gold maker");
        // Spot tier 0 and a stable pair (× 0.2).
        let s0 = tier0(HlKind::Spot);
        let f = hl_fee_schedule(s0, 0.0, false, HlFeeMarket::Spot { stable_pair: false }).unwrap();
        close(f.taker_bps, 7.0, "spot taker");
        close(f.maker_bps, 4.0, "spot maker");
        let f = hl_fee_schedule(s0, 0.0, false, HlFeeMarket::Spot { stable_pair: true }).unwrap();
        close(f.taker_bps, 1.4, "stable pair taker");
        close(f.maker_bps, 0.8, "stable pair maker");
        // Growth mode applies on top of staking: 4.5 × 0.6 × 2 × 0.1.
        let u = HlUserRates::from_tier(HlKind::Perp, 0, 40.0).unwrap();
        close(
            hl_fee_schedule(u, 0.0, false, perp(1.0, true))
                .unwrap()
                .taker_bps,
            0.54,
            "growth + diamond",
        );
    }

    #[test]
    fn referral_aligned_quote_and_rebates_follow_fee_rates() {
        let t = tier0(HlKind::Perp);
        // Referral 4 %: both positive rates.
        let f = hl_fee_schedule(t, 0.04, false, perp(1.0, false)).unwrap();
        close(f.taker_bps, 8.64, "referral taker");
        close(f.maker_bps, 2.88, "referral maker");
        // Aligned quote: taker × ((1 − d) × 0.8 + d), d = 0.5 at scale 1, 0 at scale 0.
        close(
            hl_fee_schedule(t, 0.0, true, perp(1.0, false))
                .unwrap()
                .taker_bps,
            8.1,
            "aligned hip3",
        );
        close(
            hl_fee_schedule(t, 0.0, true, perp(0.0, false))
                .unwrap()
                .taker_bps,
            3.6,
            "aligned validator",
        );
        // A rebate (−0.001 %) is cut by growth mode, never deployer-scaled.
        let rebate = HlUserRates {
            taker: t.taker,
            maker: -0.000_01,
        };
        let f = hl_fee_schedule(rebate, 0.0, false, perp(1.0, true)).unwrap();
        close(f.maker_bps, -0.01, "rebate growth");
        let f = hl_fee_schedule(rebate, 0.0, true, perp(1.0, true)).unwrap();
        close(f.maker_bps, -0.0125, "rebate aligned (× 1.25)");
    }

    #[test]
    fn fee_inputs_fail_closed() {
        let t = tier0(HlKind::Perp);
        assert!(HlUserRates::from_tier(HlKind::Perp, 7, 0.0).is_err());
        for pct in [25.0, -5.0, 50.0, f64::NAN] {
            assert!(
                HlUserRates::from_tier(HlKind::Perp, 0, pct).is_err(),
                "{pct}"
            );
        }
        for s in [-0.1, 3.5, f64::NAN] {
            assert!(
                hl_fee_schedule(t, 0.0, false, perp(s, false)).is_err(),
                "{s}"
            );
        }
        for r in [1.0, -0.1] {
            assert!(
                hl_fee_schedule(t, r, false, perp(0.0, false)).is_err(),
                "{r}"
            );
        }
        let neg = HlUserRates {
            taker: -0.0001,
            maker: 0.0,
        };
        assert!(hl_fee_schedule(neg, 0.0, false, perp(0.0, false)).is_err());
        assert!(FeeSchedule::new(-1.0, 0.0).is_err());
        assert!(FeeSchedule::new(f64::INFINITY, 0.0).is_err());
    }

    #[test]
    fn fee_schedule_knobs_are_required_and_strict() {
        let f: FeeSchedule = serde_json::from_str(r#"{"taker_bps":5.0,"maker_bps":2.0}"#).unwrap();
        assert_eq!(f, FeeSchedule::new(5.0, 2.0).unwrap());
        assert!(serde_json::from_str::<FeeSchedule>(r#"{"taker_bps":5.0}"#).is_err());
        assert!(serde_json::from_str::<FeeSchedule>(
            r#"{"taker_bps":5.0,"maker_bps":2.0,"tier":1}"#
        )
        .is_err());
        close(
            f.fee_usd(Liquidity::Taker, -1_000.0),
            0.5,
            "fee on |notional|",
        );
        close(f.fee_usd(Liquidity::Maker, 1_000.0), 0.2, "maker fee");
    }

    // ── funding, gas ──

    #[test]
    fn funding_carry_sign_follows_hl() {
        // HL's interest baseline 0.00125 %/h = 0.01 % per 8 h = 1 bps.
        close(
            funding_carry_bps(0.000_012_5, 8.0, Side::Buy).unwrap(),
            1.0,
            "long pays",
        );
        close(
            funding_carry_bps(0.000_012_5, 8.0, Side::Sell).unwrap(),
            -1.0,
            "short receives",
        );
        close(
            funding_carry_bps(-0.0001, 24.0, Side::Buy).unwrap(),
            -24.0,
            "negative rate",
        );
        assert_eq!(funding_carry_bps(0.0001, 0.0, Side::Buy).unwrap(), 0.0);
        assert!(funding_carry_bps(f64::NAN, 1.0, Side::Buy).is_err());
        assert!(funding_carry_bps(0.0001, -1.0, Side::Buy).is_err());
    }

    #[test]
    fn gas_in_usd() {
        // 150k gas × 0.01 gwei × $4000/ETH = 1.5e-6 ETH = $0.006.
        close(gas_usd(150_000, 10_000_000, 4_000.0).unwrap(), 0.006, "gas");
        assert_eq!(gas_usd(0, 10_000_000, 4_000.0).unwrap(), 0.0);
        assert!(gas_usd(1, 1, 0.0).is_err());
        assert!(gas_usd(1, 1, f64::NAN).is_err());
    }

    // ── taker cost, edge ──

    #[test]
    fn taker_cost_on_the_fixture_book() {
        let book = tsla_book();
        let fees = hl_fee_schedule(tier0(HlKind::Perp), 0.0, false, perp(1.0, true)).unwrap();
        let c = taker_cost(&book, Side::Buy, WalkTarget::Notional(1_000.0), &fees).unwrap();
        close(c.fee_bps, 0.9, "fee bps");
        close(c.fee_usd, 0.09, "fee usd");
        // Slippage vs mid 1.008079033396218 (hand-computed) + 0.9.
        close(c.total_cost_bps.unwrap(), 1.908_079_033_396_218, "total");
        assert!(c.walk.is_complete());
        // Past visible depth: fee on what filled only; the walk says `depth`.
        let c = taker_cost(&book, Side::Buy, WalkTarget::Qty(1_000.0), &fees).unwrap();
        close(
            c.fee_usd,
            184_357.911_6 * 0.9 / 1e4,
            "fee on filled notional",
        );
        assert!(!c.walk.is_complete());
        // No liquidity: no cost number at all.
        let empty = L2Book::new(vec![], vec![], 0).unwrap();
        let c = taker_cost(&empty, Side::Sell, WalkTarget::Qty(1.0), &fees).unwrap();
        assert_eq!((c.total_cost_bps, c.fee_usd), (None, 0.0));
        assert!(taker_cost(&book, Side::Buy, WalkTarget::Qty(-1.0), &fees).is_err());
    }

    #[test]
    fn edge_after_round_trip_costs() {
        let rt = RoundTripCost {
            notional_usd: 100.0,
            entry_fee_bps: 0.9,
            exit_fee_bps: 0.9,
            entry_slippage_bps: 1.0,
            exit_slippage_bps: 1.2,
            carry_bps: 0.5,
            gas_usd: 0.0,
        };
        close(rt.total_bps().unwrap(), 4.5, "total");
        close(edge_after_costs_bps(50.0, &rt).unwrap(), 45.5, "edge");
        // $0.01 of gas on $100 = 1 bps; received funding lowers the cost.
        let rt2 = RoundTripCost {
            gas_usd: 0.01,
            carry_bps: -2.0,
            ..rt
        };
        close(rt2.total_bps().unwrap(), 3.0, "gas + received carry");
        close(
            edge_after_costs_bps(2.0, &rt2).unwrap(),
            -1.0,
            "negative edge",
        );
        for bad in [
            RoundTripCost {
                notional_usd: 0.0,
                ..rt
            },
            RoundTripCost {
                entry_fee_bps: f64::NAN,
                ..rt
            },
            RoundTripCost {
                gas_usd: -1.0,
                ..rt
            },
        ] {
            assert!(bad.total_bps().is_err(), "{bad:?}");
        }
        assert!(edge_after_costs_bps(f64::INFINITY, &rt).is_err());
    }
}
