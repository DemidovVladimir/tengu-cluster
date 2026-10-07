//! LP gates — pure policy ported from `delta_neutral_bot` (each item names the
//! bot file:line it ports). Plain structs in, plain structs out; no clocks
//! (`now_ms` / `now_s` are inputs), no IO.
//!
//! | Gate | Ports |
//! |---|---|
//! | [`evaluate_reentry_gate`], [`reentry_width_frac`] | `src/modules/reentryGate.ts:48-68`, `autoTuneOrchestrator.ts:589-591,1826-1831` |
//! | [`storm_update`] (5-min move + hysteresis) | `autoTuneOrchestrator.ts:411-441` |
//! | [`trend_confirm`] (imbalance выдержка) | `autoTuneOrchestrator.ts:770-826` |
//! | [`regime_confirm`] (clamp-regime выдержка + freeze) | `autoTuneOrchestrator.ts:1117-1173` |
//! | [`token_percentages`], [`check_position_imbalance`] | `src/utils/meteoraUtils.ts:121-152,305-339` |
//! | [`wallet_balanced_for_5050`] | `meteoraUtils.ts:369-402` |
//! | [`plan_swap_for_deposit`] | `src/modules/swapPlanner.ts:147-286` |
//! | [`price_from_bin`], [`bin_from_price`], [`bin_array_index`], [`centered_range`] | `meteoraUtils.ts:70-79,418-436`; SDK `getBinIdFromPrice`, `binIdToBinArrayIndex`; `meteoraAdapter.ts:594-596` (71-bin bug, fixed) |
//! | [`dlmm_fee_rates`], [`dynamic_volatility_accumulator`] | `@meteora-ag/dlmm` `getBaseFee` / `getVariableFee` / `getTotalFee` / `calculateFeeInfo` / `updateReference` / `updateVolatilityAccumulator` |
//! | [`check_swap_oracle_gate`] (fail-closed) | `src/modules/swapPlanner.ts:325-337` |
//!
//! Deliberate deviations (failed reads never become 0 / never act on garbage):
//! storm `move_5m_pct` is `None` (bot: 0) without a reference sample and a
//! non-positive price is not evaluated (bot: `0 / ref` reads as a 100 % move);
//! composition returns `None` on non-finite inputs and 50/50 for a zero-width
//! (1-bin) range (bot: NaN); the swap gate reports `None` instead of
//! NaN/Infinity and also rejects non-finite inputs.

// Consumed by the lp_decide / hedge_decide tools (WP-COMPOSE). `!(x > 0.0)`
// is the TS NaN-rejecting guard, kept verbatim.
#![allow(dead_code, clippy::neg_cmp_op_on_partial_ord)]

use serde::{Deserialize, Serialize};

use crate::domain::observation::{set_bool, set_num, set_str, Features, Observed};

use super::hedge::{js_to_fixed, LpRegime};

// ---------------------------------------------------------------------------
// Re-entry gate («выдержка на вход», BACKLOG A15)
// ---------------------------------------------------------------------------

/// `ReentryGateInput` (`reentryGate.ts:27-46`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ReentryGateInput {
    pub now_ms: i64,
    /// Current (cross-validated) oracle price.
    pub price: f64,
    /// Anchor set at close time or at the last breakout.
    pub anchor_price: f64,
    /// When the price last (re)entered the calm corridor around the anchor.
    pub stable_since_ms: i64,
    /// Corridor half-width as a price fraction (`reentry_tol_frac × width_frac`).
    pub tol_price_frac: f64,
    /// ≤ 0 opens immediately (feature off / rollback mid-wait).
    pub confirm_ms: i64,
    /// Never open into a storm (the anchor logic still runs).
    pub storm_active: bool,
}

/// `ReentryDecision` (`reentryGate.ts:22-25`); serialises with an `action` tag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub(crate) enum ReentryDecision {
    Hold,
    Rearm {
        anchor_price: f64,
        stable_since_ms: i64,
    },
    Open,
}

/// `evaluateReentryGate` (`reentryGate.ts:48-68`).
pub(crate) fn evaluate_reentry_gate(input: &ReentryGateInput) -> ReentryDecision {
    // Never act on garbage — a broken oracle read must not open a position
    // (nor destroy the anchor).
    if !input.price.is_finite()
        || input.price <= 0.0
        || !input.anchor_price.is_finite()
        || input.anchor_price <= 0.0
    {
        return ReentryDecision::Hold;
    }
    if (input.price / input.anchor_price - 1.0).abs() > input.tol_price_frac {
        // Breakout — re-anchor at the new level and restart the calm clock.
        return ReentryDecision::Rearm {
            anchor_price: input.price,
            stable_since_ms: input.now_ms,
        };
    }
    if !input.storm_active && input.now_ms - input.stable_since_ms >= input.confirm_ms {
        return ReentryDecision::Open;
    }
    ReentryDecision::Hold
}

/// Closed position's range width as a price fraction
/// (`autoTuneOrchestrator.ts:1826-1831`); 0.02 (≈ 20 bins × 10 bps) when the
/// range or price is unusable. Corridor = `reentry_tol_frac × width_frac`
/// (`autoTuneOrchestrator.ts:589-591`).
pub(crate) fn reentry_width_frac(lower_price: f64, upper_price: f64, price: f64) -> f64 {
    if upper_price > lower_price && price > 0.0 {
        (upper_price - lower_price) / price
    } else {
        0.02
    }
}

// ---------------------------------------------------------------------------
// Storm hysteresis (ADR-021)
// ---------------------------------------------------------------------------

/// Samples older than this are dropped (`autoTuneOrchestrator.ts:414`).
pub(crate) const STORM_WINDOW_MS: i64 = 6 * 60 * 1000;
/// The reference is the OLDEST sample at least this old (`:423`).
pub(crate) const STORM_REF_MIN_AGE_MS: i64 = 4 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub(crate) struct PriceSample {
    pub t_ms: i64,
    pub usd: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct StormInput {
    pub now_ms: i64,
    pub price: f64,
    /// Samples carried from the previous call (any order).
    pub samples: Vec<PriceSample>,
    /// `lpVolPausePct5m`; ≤ 0 or non-finite disables the storm pause.
    pub threshold_pct: f64,
    /// Storm state from the previous call.
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct StormOutput {
    /// Pruned window (≤ 6 min, oldest first) including this price — carry it
    /// into the next call.
    pub samples: Vec<PriceSample>,
    /// |price / reference − 1| × 100; `None` without a ≥ 4-min-old reference
    /// or a usable price.
    pub move_5m_pct: Option<f64>,
    pub active: bool,
}

/// `recordPriceSample` (`autoTuneOrchestrator.ts:411-441`): enter the storm
/// when the move exceeds the threshold (strictly `>`, as the bot and the
/// simulator do), leave it when the move drops below threshold / 2. Without a
/// reference (or a usable price) the previous state is kept.
pub(crate) fn storm_update(input: &StormInput) -> StormOutput {
    let price_ok = input.price.is_finite() && input.price > 0.0;
    let mut samples = input.samples.clone();
    if price_ok {
        samples.push(PriceSample {
            t_ms: input.now_ms,
            usd: input.price,
        });
    }
    samples.sort_by_key(|s| s.t_ms);
    let cutoff = input.now_ms - STORM_WINDOW_MS;
    samples.retain(|s| s.t_ms >= cutoff);

    let reference = samples
        .iter()
        .find(|s| input.now_ms - s.t_ms >= STORM_REF_MIN_AGE_MS);
    let move_5m_pct = match reference {
        Some(r) if price_ok && r.usd > 0.0 && r.usd.is_finite() => {
            Some((input.price / r.usd - 1.0).abs() * 100.0)
        }
        _ => None,
    };

    let threshold = input.threshold_pct;
    let active = if !(threshold > 0.0) || !threshold.is_finite() {
        false
    } else {
        match move_5m_pct {
            None => input.active, // not enough history — keep current state
            Some(m) if input.active => m >= threshold / 2.0,
            Some(m) => m > threshold,
        }
    };
    StormOutput {
        samples,
        move_5m_pct,
        active,
    }
}

// ---------------------------------------------------------------------------
// Trend confirm — imbalance выдержка (ADR-023)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct TrendConfirmInput {
    pub now_ms: i64,
    pub is_imbalanced: bool,
    /// When the composition first went out of threshold (previous call).
    pub imbalance_since_ms: Option<i64>,
    /// `TREND_CONFIRM_MS`; ≤ 0 acts on the first imbalanced read.
    pub confirm_ms: i64,
    pub storm_active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ImbalanceAction {
    /// Composition within threshold.
    Balanced,
    /// Imbalanced but a storm pauses recentering (`:793-799`).
    StormPaused,
    /// Imbalanced, confirmation window still running (`:800-808`).
    Waiting,
    /// Imbalance held for the full window — recenter (`:809-818`).
    Recenter,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct TrendConfirmOutput {
    /// Carry into the next call; `None` once balanced (whipsaw resets it).
    pub imbalance_since_ms: Option<i64>,
    pub held_ms: i64,
    /// `max(0, confirm_ms − held_ms)` while imbalanced, else 0.
    pub remaining_ms: i64,
    pub confirmed: bool,
    pub action: ImbalanceAction,
}

/// Imbalance timer (`autoTuneOrchestrator.ts:770-818`).
pub(crate) fn trend_confirm(input: &TrendConfirmInput) -> TrendConfirmOutput {
    let since = if input.is_imbalanced {
        Some(input.imbalance_since_ms.unwrap_or(input.now_ms))
    } else {
        None
    };
    let held_ms = since.map_or(0, |s| input.now_ms - s);
    let confirmed = input.is_imbalanced && since.is_some() && held_ms >= input.confirm_ms;
    let action = if !input.is_imbalanced {
        ImbalanceAction::Balanced
    } else if input.storm_active {
        ImbalanceAction::StormPaused
    } else if !confirmed {
        ImbalanceAction::Waiting
    } else {
        ImbalanceAction::Recenter
    };
    TrendConfirmOutput {
        imbalance_since_ms: since,
        held_ms,
        remaining_ms: if input.is_imbalanced {
            (input.confirm_ms - held_ms).max(0)
        } else {
            0
        },
        confirmed,
        action,
    }
}

// ---------------------------------------------------------------------------
// Clamp-regime confirm (ADR-023 выдержка + ADR-025 freeze)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub(crate) struct PendingRegime {
    pub regime: LpRegime,
    pub since_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct RegimeConfirmInput {
    pub now_ms: i64,
    /// Regime the hedge currently prices.
    pub committed: LpRegime,
    /// `lp_hedge_delta(.., sticky = committed).regime` for this read.
    pub computed: LpRegime,
    pub pending: Option<PendingRegime>,
    /// `TREND_CONFIRM_MS`.
    pub confirm_ms: i64,
    pub storm_active: bool,
    /// The imbalance timer is running (`imbalance_since_ms.is_some()`).
    pub imbalance_pending: bool,
    /// The last recenter attempt failed (bot: `lastRebalanceFailedAt != null`).
    pub last_rebalance_failed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RegimeOutcome {
    /// Computed regime equals the committed one.
    Unchanged,
    /// Candidate confirmed and committed.
    Committed,
    /// Confirmed, but the healthy recenter pipeline owns the signal (ADR-025).
    Frozen,
    /// Candidate waiting out its window.
    Pending,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct RegimeConfirmOutput {
    pub committed: LpRegime,
    pub pending: Option<PendingRegime>,
    pub outcome: RegimeOutcome,
    pub held_ms: i64,
}

/// Candidate clamp regime → commit only after it persists `confirm_ms`
/// (immediate in a storm), unless the recenter pipeline owns the imbalance
/// (`autoTuneOrchestrator.ts:1117-1173`). Price the returned `committed`.
pub(crate) fn regime_confirm(input: &RegimeConfirmInput) -> RegimeConfirmOutput {
    if input.computed == input.committed {
        return RegimeConfirmOutput {
            committed: input.committed,
            pending: None,
            outcome: RegimeOutcome::Unchanged,
            held_ms: 0,
        };
    }
    let pending = match input.pending {
        Some(p) if p.regime == input.computed => p,
        _ => PendingRegime {
            regime: input.computed,
            since_ms: input.now_ms,
        },
    };
    let held_ms = input.now_ms - pending.since_ms;
    let confirmed = input.storm_active || held_ms >= input.confirm_ms;
    let recenter_owns_signal =
        input.imbalance_pending && !input.storm_active && !input.last_rebalance_failed;
    if confirmed && !recenter_owns_signal {
        return RegimeConfirmOutput {
            committed: input.computed,
            pending: None,
            outcome: RegimeOutcome::Committed,
            held_ms,
        };
    }
    RegimeConfirmOutput {
        committed: input.committed,
        pending: Some(pending),
        outcome: if confirmed {
            RegimeOutcome::Frozen
        } else {
            RegimeOutcome::Pending
        },
        held_ms,
    }
}

// ---------------------------------------------------------------------------
// Composition + imbalance (linear, by price position in the range)
// ---------------------------------------------------------------------------

/// Token X / token Y share of a position, percent, rounded like the bot
/// (`Number(x.toFixed(2))`). Token X = base (SOL), token Y = quote (USDC).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub(crate) struct TokenPercentages {
    pub token_x: f64,
    pub token_y: f64,
}

/// `calculateTokenPercentages` (`meteoraUtils.ts:121-152`). `None` on
/// non-finite inputs; a zero-width in-range position (1 bin) reads 50/50
/// instead of the bot's NaN.
pub(crate) fn token_percentages(
    current_price: f64,
    start_bin_price: f64,
    end_bin_price: f64,
) -> Option<TokenPercentages> {
    if !current_price.is_finite() || !start_bin_price.is_finite() || !end_bin_price.is_finite() {
        return None;
    }
    if current_price >= start_bin_price && current_price <= end_bin_price {
        let range_size = end_bin_price - start_bin_price;
        if range_size <= 0.0 {
            return Some(TokenPercentages {
                token_x: 50.0,
                token_y: 50.0,
            });
        }
        let position_in_range = current_price - start_bin_price;
        let x = (1.0 - position_in_range / range_size) * 100.0;
        let y = (position_in_range / range_size) * 100.0;
        Some(TokenPercentages {
            token_x: js_round(x, 2),
            token_y: js_round(y, 2),
        })
    } else if current_price < start_bin_price {
        Some(TokenPercentages {
            token_x: 100.0,
            token_y: 0.0,
        })
    } else {
        Some(TokenPercentages {
            token_x: 0.0,
            token_y: 100.0,
        })
    }
}

/// `Number(x.toFixed(digits))`.
fn js_round(x: f64, digits: usize) -> f64 {
    js_to_fixed(x, digits).parse().unwrap_or(f64::NAN)
}

/// `checkPositionImbalance` result (`meteoraUtils.ts:305-339`); the bot's
/// `solPercent` / `usdcPercent` are `x_percent` / `y_percent` here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PositionImbalance {
    pub is_imbalanced: bool,
    pub x_percent: f64,
    pub y_percent: f64,
    pub threshold_percent: f64,
    pub reason: Option<String>,
}

/// Imbalanced when either side is ≥ `imbalance_threshold × 100` percent
/// (`meteoraUtils.ts:305-339`). `None` when the composition is not evaluable.
pub(crate) fn check_position_imbalance(
    current_price: f64,
    lower_bin_price: f64,
    upper_bin_price: f64,
    imbalance_threshold: f64,
) -> Option<PositionImbalance> {
    let c = token_percentages(current_price, lower_bin_price, upper_bin_price)?;
    let threshold_percent = imbalance_threshold * 100.0;
    let is_imbalanced = c.token_x >= threshold_percent || c.token_y >= threshold_percent;
    let reason = if !is_imbalanced {
        None
    } else if c.token_x >= threshold_percent {
        Some(format!(
            "token X concentration {}% exceeds {threshold_percent}% threshold",
            c.token_x
        ))
    } else {
        Some(format!(
            "token Y concentration {}% exceeds {threshold_percent}% threshold",
            c.token_y
        ))
    };
    Some(PositionImbalance {
        is_imbalanced,
        x_percent: c.token_x,
        y_percent: c.token_y,
        threshold_percent,
        reason,
    })
}

// ---------------------------------------------------------------------------
// Wallet 50/50 gate (alignment-swap skip)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub(crate) struct WalletBalance5050 {
    pub balanced: bool,
    /// USD-weighted SOL share of the usable wallet (reserves excluded).
    pub wallet_sol_ratio: f64,
    pub wallet_total_usd: f64,
}

/// `isWalletBalancedFor5050` (`meteoraUtils.ts:369-402`): balanced when the
/// SOL share (after `total_reserve_sol`) is within `tolerance_fraction` of
/// 0.5 (+1e-9 FP slack); an empty wallet reads balanced (ratio 0.5).
pub(crate) fn wallet_balanced_for_5050(
    wallet_sol: f64,
    wallet_usdc: f64,
    current_price: f64,
    total_reserve_sol: f64,
    tolerance_fraction: f64,
) -> WalletBalance5050 {
    let available_sol = (wallet_sol - total_reserve_sol).max(0.0);
    let sol_usd = available_sol * current_price;
    let total_usd = sol_usd + wallet_usdc;
    if total_usd <= 0.0 {
        return WalletBalance5050 {
            balanced: true,
            wallet_sol_ratio: 0.5,
            wallet_total_usd: 0.0,
        };
    }
    let wallet_sol_ratio = sol_usd / total_usd;
    const FP_EPSILON: f64 = 1e-9;
    WalletBalance5050 {
        balanced: (wallet_sol_ratio - 0.5).abs() <= tolerance_fraction + FP_EPSILON,
        wallet_sol_ratio,
        wallet_total_usd: total_usd,
    }
}

// ---------------------------------------------------------------------------
// Deposit / recenter swap planner (BUG-020 + position-rent regression)
// ---------------------------------------------------------------------------

/// Why the reserve-aware deposit plan cannot be funded. These are decisions,
/// not read failures: callers may wait, reduce the target or add funds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DepositSwapBlockCode {
    InvalidInput,
    InsufficientTotalValue,
    InsufficientSol,
    InsufficientUsdc,
}

impl DepositSwapBlockCode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            DepositSwapBlockCode::InvalidInput => "invalid_input",
            DepositSwapBlockCode::InsufficientTotalValue => "insufficient_total_value",
            DepositSwapBlockCode::InsufficientSol => "insufficient_sol",
            DepositSwapBlockCode::InsufficientUsdc => "insufficient_usdc",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DepositSwapContext {
    InitialPosition,
    Rebalance,
}

impl DepositSwapContext {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            DepositSwapContext::InitialPosition => "initial_position",
            DepositSwapContext::Rebalance => "rebalance",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DepositSwapDirection {
    SolToUsdc,
    UsdcToSol,
}

impl DepositSwapDirection {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            DepositSwapDirection::SolToUsdc => "sol_to_usdc",
            DepositSwapDirection::UsdcToSol => "usdc_to_sol",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct DepositSwapInput {
    pub wallet_sol: f64,
    pub wallet_usdc: f64,
    pub target_sol: f64,
    pub target_usdc: f64,
    pub permanent_minimum_sol: f64,
    pub rent_reserve_sol: f64,
    pub position_rent_sol: f64,
    pub reserve_usdc: f64,
    pub current_price: f64,
    pub slippage_buffer_pct: f64,
    pub context: DepositSwapContext,
    pub base_mint: String,
    pub quote_mint: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub(crate) struct DepositSwapShortfall {
    pub sol: f64,
    pub usdc: f64,
}

/// At most one swap is emitted. It is a vector so a decision-loop slot can
/// consume `/data/swaps/*/{input_mint,output_mint,amount}` directly; an empty
/// vector makes the live `jupiter_swap` action illegal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct DepositSwap {
    pub direction: DepositSwapDirection,
    pub input_mint: String,
    pub output_mint: String,
    pub amount: f64,
    pub expected_output: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct DepositSwapBlock {
    pub code: DepositSwapBlockCode,
    pub reason: String,
}

/// Pure, reserve-aware funding plan for one LP deposit/recenter. A blocked
/// plan is returned as data (not an exception), so JEV can choose hold or
/// escalation without losing the deterministic reason.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct DepositSwapPlan {
    pub context: DepositSwapContext,
    pub base_mint: String,
    pub quote_mint: String,
    pub feasible: bool,
    pub needed: bool,
    pub available_sol_for_swap: f64,
    pub spendable_usdc: f64,
    pub wallet_value_usd: Option<f64>,
    pub required_value_usd: Option<f64>,
    pub shortfall: DepositSwapShortfall,
    pub swaps: Vec<DepositSwap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block: Option<DepositSwapBlock>,
}

impl DepositSwapPlan {
    fn blocked(
        input: &DepositSwapInput,
        code: DepositSwapBlockCode,
        reason: impl Into<String>,
        available_sol_for_swap: f64,
        spendable_usdc: f64,
        wallet_value_usd: Option<f64>,
        required_value_usd: Option<f64>,
        shortfall: DepositSwapShortfall,
    ) -> Self {
        Self {
            context: input.context,
            base_mint: input.base_mint.clone(),
            quote_mint: input.quote_mint.clone(),
            feasible: false,
            needed: false,
            available_sol_for_swap,
            spendable_usdc,
            wallet_value_usd,
            required_value_usd,
            shortfall,
            swaps: Vec::new(),
            block: Some(DepositSwapBlock {
                code,
                reason: reason.into(),
            }),
        }
    }
}

/// Port of `delta_neutral_bot::planSwapForDeposit`. Reserve SOL, refundable
/// position rent and hedge-collateral USDC are unavailable to the deposit.
/// The buffer increases swap input only; Jupiter's live tool independently
/// gates the resulting quote against the oracle before it may send.
pub(crate) fn plan_swap_for_deposit(input: &DepositSwapInput) -> DepositSwapPlan {
    let non_negative = [
        input.wallet_sol,
        input.wallet_usdc,
        input.target_sol,
        input.target_usdc,
        input.permanent_minimum_sol,
        input.rent_reserve_sol,
        input.position_rent_sol,
        input.reserve_usdc,
        input.slippage_buffer_pct,
    ]
    .into_iter()
    .all(|x| x.is_finite() && x >= 0.0);
    if !non_negative || !input.current_price.is_finite() || input.current_price <= 0.0 {
        return DepositSwapPlan::blocked(
            input,
            DepositSwapBlockCode::InvalidInput,
            "amounts and reserves must be finite and non-negative; current_price must be finite and > 0",
            0.0,
            0.0,
            None,
            None,
            DepositSwapShortfall {
                sol: 0.0,
                usdc: 0.0,
            },
        );
    }

    let total_sol_reserve = input.permanent_minimum_sol + input.rent_reserve_sol;
    let available_sol_for_swap = (input.wallet_sol - total_sol_reserve).max(0.0);
    let spendable_usdc = (input.wallet_usdc - input.reserve_usdc).max(0.0);
    let required_sol = input.target_sol + input.position_rent_sol;
    let shortfall = DepositSwapShortfall {
        sol: (required_sol - available_sol_for_swap).max(0.0),
        usdc: (input.target_usdc - spendable_usdc).max(0.0),
    };
    let wallet_value_usd = available_sol_for_swap * input.current_price + spendable_usdc;
    let required_value_usd = required_sol * input.current_price + input.target_usdc;

    if shortfall.sol == 0.0 && shortfall.usdc == 0.0 {
        return DepositSwapPlan {
            context: input.context,
            base_mint: input.base_mint.clone(),
            quote_mint: input.quote_mint.clone(),
            feasible: true,
            needed: false,
            available_sol_for_swap,
            spendable_usdc,
            wallet_value_usd: Some(wallet_value_usd),
            required_value_usd: Some(required_value_usd),
            shortfall,
            swaps: Vec::new(),
            block: None,
        };
    }

    if wallet_value_usd < required_value_usd {
        return DepositSwapPlan::blocked(
            input,
            DepositSwapBlockCode::InsufficientTotalValue,
            format!(
                "wallet value ${wallet_value_usd:.2} after reserves is below required ${required_value_usd:.2} for {}",
                input.context.as_str()
            ),
            available_sol_for_swap,
            spendable_usdc,
            Some(wallet_value_usd),
            Some(required_value_usd),
            shortfall,
        );
    }

    let buffer = 1.0 + input.slippage_buffer_pct;
    let sol_shortfall_usd = shortfall.sol * input.current_price;
    let swap = if shortfall.usdc >= sol_shortfall_usd {
        let amount = shortfall.usdc / input.current_price * buffer;
        if available_sol_for_swap < amount {
            return DepositSwapPlan::blocked(
                input,
                DepositSwapBlockCode::InsufficientSol,
                format!(
                    "need {amount:.8} SOL swap input but only {available_sol_for_swap:.8} SOL is available after reserves"
                ),
                available_sol_for_swap,
                spendable_usdc,
                Some(wallet_value_usd),
                Some(required_value_usd),
                shortfall,
            );
        }
        DepositSwap {
            direction: DepositSwapDirection::SolToUsdc,
            input_mint: input.base_mint.clone(),
            output_mint: input.quote_mint.clone(),
            amount,
            expected_output: shortfall.usdc,
        }
    } else {
        let amount = shortfall.sol * input.current_price * buffer;
        if spendable_usdc < amount {
            return DepositSwapPlan::blocked(
                input,
                DepositSwapBlockCode::InsufficientUsdc,
                format!(
                    "need {amount:.6} USDC swap input but only {spendable_usdc:.6} USDC is spendable after hedge reserve"
                ),
                available_sol_for_swap,
                spendable_usdc,
                Some(wallet_value_usd),
                Some(required_value_usd),
                shortfall,
            );
        }
        DepositSwap {
            direction: DepositSwapDirection::UsdcToSol,
            input_mint: input.quote_mint.clone(),
            output_mint: input.base_mint.clone(),
            amount,
            expected_output: shortfall.sol,
        }
    };

    DepositSwapPlan {
        context: input.context,
        base_mint: input.base_mint.clone(),
        quote_mint: input.quote_mint.clone(),
        feasible: true,
        needed: true,
        available_sol_for_swap,
        spendable_usdc,
        wallet_value_usd: Some(wallet_value_usd),
        required_value_usd: Some(required_value_usd),
        shortfall,
        swaps: vec![swap],
        block: None,
    }
}

impl Observed for DepositSwapPlan {
    const SCHEMA: &'static str = "lp_swap_plan/1";

    fn subject(&self) -> String {
        format!(
            "{}:{}:{}",
            self.context.as_str(),
            self.base_mint,
            self.quote_mint
        )
    }

    fn headline(&self) -> String {
        if let Some(block) = &self.block {
            return format!(
                "lp_swap_plan {} blocked={}",
                self.context.as_str(),
                block.code.as_str()
            );
        }
        match self.swaps.first() {
            Some(s) => format!(
                "lp_swap_plan {} direction={} amount={:.8}",
                self.context.as_str(),
                s.direction.as_str(),
                s.amount
            ),
            None => format!("lp_swap_plan {} no_swap", self.context.as_str()),
        }
    }

    fn features(&self) -> Features {
        let mut f = Features::new();
        set_str(&mut f, "context", Some(self.context.as_str()));
        set_bool(&mut f, "feasible", Some(self.feasible));
        set_bool(&mut f, "needed", Some(self.needed));
        set_num(&mut f, "available_sol", Some(self.available_sol_for_swap));
        set_num(&mut f, "spendable_usdc", Some(self.spendable_usdc));
        set_num(&mut f, "wallet_value_usd", self.wallet_value_usd);
        set_num(&mut f, "required_value_usd", self.required_value_usd);
        set_num(&mut f, "shortfall_sol", Some(self.shortfall.sol));
        set_num(&mut f, "shortfall_usdc", Some(self.shortfall.usdc));
        set_str(
            &mut f,
            "blocked",
            self.block.as_ref().map(|b| b.code.as_str()),
        );
        if let Some(s) = self.swaps.first() {
            set_str(&mut f, "direction", Some(s.direction.as_str()));
            set_num(&mut f, "amount", Some(s.amount));
            set_num(&mut f, "expected_output", Some(s.expected_output));
        }
        f
    }
}

// ---------------------------------------------------------------------------
// DLMM bin math
// ---------------------------------------------------------------------------

/// Bins per bin array (`MAX_BIN_PER_ARRAY`, DLMM IDL constant).
pub(crate) const MAX_BIN_PER_ARRAY: i64 = 70;
/// Max bins a position may span (`METEORA_LIMITS.MAX_POSITION_WIDTH_BINS`,
/// `src/config/constants.ts:153`).
pub(crate) const MAX_POSITION_WIDTH_BINS: u32 = 70;
const BASIS_POINT_MAX: f64 = 10_000.0;
/// |log-ratio − nearest integer| below this snaps to the integer before
/// rounding, so an exact bin price maps to its own bin under every rounding.
const BIN_SNAP_EPS: f64 = 1e-9;

/// `getPriceFromBinId` (`meteoraUtils.ts:70-79`):
/// `(1 + bin_step/1e4)^bin_id × 10^(dec_x − dec_y)` — quote per base in UI
/// units. Computed as `exp(bin_id × ln_1p(step))` (≈ 1e-15 relative vs the
/// bot's Decimal.js; `powi` accumulates |bin_id| × ε).
pub(crate) fn price_from_bin(bin_id: i32, bin_step: u16, dec_x: u8, dec_y: u8) -> f64 {
    let per_lamport = (f64::from(bin_id) * (f64::from(bin_step) / BASIS_POINT_MAX).ln_1p()).exp();
    per_lamport * 10f64.powi(i32::from(dec_x) - i32::from(dec_y))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Rounding {
    /// Floor — SDK `getBinIdFromPrice(.., min = true)`.
    Down,
    /// Ceil — SDK `min = false` (the bot's `priceToNearestBinId`, despite its name).
    Up,
    /// Round half away from zero.
    Nearest,
}

/// SDK `getBinIdFromPrice(toPricePerLamport(price), bin_step, min)`:
/// `ln(price × 10^(dec_y − dec_x)) / ln(1 + bin_step/1e4)`, rounded as asked.
/// Log ratios within 1e-9 of an integer snap to it first (the SDK's own
/// float input makes `ceil` of an exact bin price land one bin high).
/// `None` for a non-positive / non-finite price, a zero bin step, or an id
/// outside `i32`.
pub(crate) fn bin_from_price(
    price: f64,
    bin_step: u16,
    dec_x: u8,
    dec_y: u8,
    rounding: Rounding,
) -> Option<i32> {
    if !price.is_finite() || price <= 0.0 || bin_step == 0 {
        return None;
    }
    let per_lamport = price * 10f64.powi(i32::from(dec_y) - i32::from(dec_x));
    let raw = per_lamport.ln() / (f64::from(bin_step) / BASIS_POINT_MAX).ln_1p();
    if !raw.is_finite() {
        return None;
    }
    let snapped = if (raw - raw.round()).abs() < BIN_SNAP_EPS {
        raw.round()
    } else {
        raw
    };
    let id = match rounding {
        Rounding::Down => snapped.floor(),
        Rounding::Up => snapped.ceil(),
        Rounding::Nearest => snapped.round(),
    };
    if id < f64::from(i32::MIN) || id > f64::from(i32::MAX) {
        return None;
    }
    Some(id as i32)
}

/// SDK `binIdToBinArrayIndex`: `floor(bin_id / 70)`, negatives included
/// (-1 → -1, -70 → -1, -71 → -2).
pub(crate) fn bin_array_index(bin_id: i32) -> i64 {
    i64::from(bin_id).div_euclid(MAX_BIN_PER_ARRAY)
}

/// SDK `getBinArrayLowerUpperBinId`: inclusive bin id bounds of an array.
pub(crate) fn bin_array_bounds(index: i64) -> (i64, i64) {
    let lower = index * MAX_BIN_PER_ARRAY;
    (lower, lower + MAX_BIN_PER_ARRAY - 1)
}

/// Every bin array index a `[min_bin_id, max_bin_id]` range touches, ascending.
pub(crate) fn bin_array_indexes(min_bin_id: i32, max_bin_id: i32) -> Vec<i64> {
    if max_bin_id < min_bin_id {
        return Vec::new();
    }
    (bin_array_index(min_bin_id)..=bin_array_index(max_bin_id)).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CenteredRange {
    pub min_bin_id: i32,
    pub max_bin_id: i32,
    /// `max − min + 1`, ≤ [`MAX_POSITION_WIDTH_BINS`].
    pub width: u32,
    /// `bin_count` exceeded the 70-bin position limit.
    pub clamped_to_max_width: bool,
}

/// `calculateCenteredPriceRange` bin ids (`meteoraUtils.ts:434-436`):
/// `min = active − floor(n/2)`, `max = min + n − 1`, with `n` capped at 70.
/// The bot's over-width fallback (`meteoraAdapter.ts:594-596`,
/// `active ± 35`) spans 71 bins; this never exceeds 70. `None` for 0 bins.
pub(crate) fn centered_range(active_id: i32, bin_count: u32) -> Option<CenteredRange> {
    if bin_count == 0 {
        return None;
    }
    let width = bin_count.min(MAX_POSITION_WIDTH_BINS);
    let half = i64::from(width / 2);
    let min = i64::from(active_id) - half;
    let max = min + i64::from(width) - 1;
    Some(CenteredRange {
        min_bin_id: i32::try_from(min).ok()?,
        max_bin_id: i32::try_from(max).ok()?,
        width,
        clamped_to_max_width: bin_count > MAX_POSITION_WIDTH_BINS,
    })
}

// ---------------------------------------------------------------------------
// DLMM fee rate
// ---------------------------------------------------------------------------

/// Fee rates are fractions of 1e9 (`FEE_PRECISION`).
pub(crate) const FEE_PRECISION: u128 = 1_000_000_000;
/// Total fee cap, 10 % (`MAX_FEE_RATE`).
pub(crate) const MAX_FEE_RATE: u128 = 100_000_000;

/// LbPair static + variable fee parameters the rate depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DlmmFeeParams {
    pub bin_step: u16,
    pub base_factor: u16,
    pub base_fee_power_factor: u8,
    pub variable_fee_control: u32,
    pub volatility_accumulator: u32,
    /// Protocol share of the fee, basis points.
    pub protocol_share: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub(crate) struct DlmmFeeRates {
    pub base_rate: u128,
    pub variable_rate: u128,
    /// `min(base + variable, MAX_FEE_RATE)`.
    pub total_rate: u128,
    pub base_fee_pct: f64,
    pub variable_fee_pct: f64,
    pub total_fee_pct: f64,
    pub max_fee_pct: f64,
    pub protocol_share_pct: f64,
}

/// SDK `getBaseFee`: `base_factor × bin_step × 10 × 10^power` (saturating;
/// the SDK uses arbitrary-precision BN).
pub(crate) fn base_fee_rate(bin_step: u16, base_factor: u16, base_fee_power_factor: u8) -> u128 {
    u128::from(base_factor)
        .saturating_mul(u128::from(bin_step))
        .saturating_mul(10)
        .saturating_mul(
            10u128
                .checked_pow(u32::from(base_fee_power_factor))
                .unwrap_or(u128::MAX),
        )
}

/// SDK `getVariableFee`: `ceil(vfc × (va × bin_step)² / 1e11)` when
/// `variable_fee_control > 0`, else 0 (saturating).
pub(crate) fn variable_fee_rate(
    bin_step: u16,
    variable_fee_control: u32,
    volatility_accumulator: u32,
) -> u128 {
    if variable_fee_control == 0 {
        return 0;
    }
    let vb = u128::from(volatility_accumulator) * u128::from(bin_step);
    let v_fee = u128::from(variable_fee_control).saturating_mul(vb.saturating_mul(vb));
    v_fee.saturating_add(99_999_999_999) / 100_000_000_000
}

/// Rate (fraction of 1e9) → percent: `rate / 1e9 × 100`.
pub(crate) fn fee_rate_pct(rate: u128) -> f64 {
    rate as f64 / FEE_PRECISION as f64 * 100.0
}

/// SDK `getTotalFee` + `calculateFeeInfo` + `getFeeInfo` in one struct.
pub(crate) fn dlmm_fee_rates(p: &DlmmFeeParams) -> DlmmFeeRates {
    let base_rate = base_fee_rate(p.bin_step, p.base_factor, p.base_fee_power_factor);
    let variable_rate =
        variable_fee_rate(p.bin_step, p.variable_fee_control, p.volatility_accumulator);
    let total_rate = base_rate.saturating_add(variable_rate).min(MAX_FEE_RATE);
    DlmmFeeRates {
        base_rate,
        variable_rate,
        total_rate,
        base_fee_pct: fee_rate_pct(base_rate),
        variable_fee_pct: fee_rate_pct(variable_rate),
        total_fee_pct: fee_rate_pct(total_rate),
        max_fee_pct: fee_rate_pct(MAX_FEE_RATE),
        protocol_share_pct: f64::from(p.protocol_share) * 100.0 / BASIS_POINT_MAX,
    }
}

/// LbPair volatility state needed to project the accumulator to "now".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct VolatilityState {
    pub active_id: i32,
    pub index_reference: i32,
    pub volatility_reference: u32,
    pub volatility_accumulator: u32,
    pub last_update_timestamp: i64,
    pub filter_period: u16,
    pub decay_period: u16,
    pub reduction_factor: u16,
    pub max_volatility_accumulator: u32,
}

/// SDK `getDynamicFee`'s projection (`updateReference` +
/// `updateVolatilityAccumulator`): the accumulator a swap at `now_s` would
/// see. Feed it to [`DlmmFeeParams::volatility_accumulator`] for the live fee.
pub(crate) fn dynamic_volatility_accumulator(v: &VolatilityState, now_s: i64) -> u32 {
    let elapsed = now_s - v.last_update_timestamp;
    let (index_reference, volatility_reference) = if elapsed >= i64::from(v.filter_period) {
        let reference = if elapsed < i64::from(v.decay_period) {
            u64::from(v.volatility_accumulator) * u64::from(v.reduction_factor) / 10_000
        } else {
            0
        };
        (v.active_id, reference)
    } else {
        (v.index_reference, u64::from(v.volatility_reference))
    };
    let delta_id = (i64::from(index_reference) - i64::from(v.active_id)).unsigned_abs();
    let projected = volatility_reference.saturating_add(delta_id.saturating_mul(10_000));
    projected.min(u64::from(v.max_volatility_accumulator)) as u32
}

// ---------------------------------------------------------------------------
// Swap oracle gate (ADR-020, fail-closed)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum SwapDirection {
    /// Sell base (SOL) for quote (USDC).
    SolToUsdc,
    /// Buy base with quote.
    UsdcToSol,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub(crate) struct SwapOracleGateInput {
    pub direction: SwapDirection,
    /// Human-unit input amount (SOL for SOL_TO_USDC, USDC for USDC_TO_SOL).
    pub input_amount: f64,
    /// Human-unit quoted output amount.
    pub output_amount: f64,
    /// Fair SOL/USD price from the cross-validated oracle.
    pub oracle_price_usd: f64,
    /// Max |implied − oracle| deviation, bps.
    pub tolerance_bps: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub(crate) struct SwapOracleGateResult {
    pub ok: bool,
    /// SOL/USD price implied by the quote; `None` when not evaluable.
    pub implied_price_usd: Option<f64>,
    /// Absolute deviation from the oracle, bps; `None` when not evaluable.
    pub deviation_bps: Option<f64>,
}

/// `checkSwapOracleGate` (`swapPlanner.ts:325-337`): a swap executes only when
/// the quote's implied price is within `tolerance_bps` of the oracle.
/// Non-evaluable inputs (non-positive or non-finite) fail closed.
pub(crate) fn check_swap_oracle_gate(input: &SwapOracleGateInput) -> SwapOracleGateResult {
    let usable = |x: f64| x.is_finite() && x > 0.0;
    if !usable(input.input_amount)
        || !usable(input.output_amount)
        || !usable(input.oracle_price_usd)
    {
        return SwapOracleGateResult {
            ok: false,
            implied_price_usd: None,
            deviation_bps: None,
        };
    }
    let implied = match input.direction {
        SwapDirection::SolToUsdc => input.output_amount / input.input_amount,
        SwapDirection::UsdcToSol => input.input_amount / input.output_amount,
    };
    let deviation = (implied - input.oracle_price_usd).abs() / input.oracle_price_usd * 10_000.0;
    SwapOracleGateResult {
        ok: deviation <= input.tolerance_bps,
        implied_price_usd: Some(implied),
        deviation_bps: Some(deviation),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: i64 = 60_000;

    fn close_to(a: f64, b: f64, digits: i32) -> bool {
        (a - b).abs() < 10f64.powi(-digits) / 2.0
    }

    fn rel_close(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() <= tol * a.abs().max(b.abs())
    }

    // --- reentry gate (reentryGate.test.ts) --------------------------------

    fn reentry() -> ReentryGateInput {
        ReentryGateInput {
            now_ms: 1_000_000,
            price: 80.0,
            anchor_price: 80.0,
            stable_since_ms: 1_000_000 - 60 * MIN,
            tol_price_frac: 0.003,
            confirm_ms: 120 * MIN,
            storm_active: false,
        }
    }

    #[test]
    fn reentry_holds_until_window_elapses_then_opens() {
        assert_eq!(evaluate_reentry_gate(&reentry()), ReentryDecision::Hold);
        let since = 1_000_000 - 120 * MIN;
        let at = |stable_since_ms| ReentryGateInput {
            stable_since_ms,
            ..reentry()
        };
        assert_eq!(evaluate_reentry_gate(&at(since)), ReentryDecision::Open);
        assert_eq!(evaluate_reentry_gate(&at(since + 1)), ReentryDecision::Hold);
    }

    #[test]
    fn reentry_rearms_on_breakout_either_way() {
        for price in [80.5, 79.5] {
            assert_eq!(
                evaluate_reentry_gate(&ReentryGateInput { price, ..reentry() }),
                ReentryDecision::Rearm {
                    anchor_price: price,
                    stable_since_ms: 1_000_000
                }
            );
        }
        assert_eq!(
            evaluate_reentry_gate(&ReentryGateInput {
                price: 80.23,
                ..reentry()
            }),
            ReentryDecision::Hold
        );
    }

    #[test]
    fn reentry_storm_blocks_open_not_rearm() {
        assert_eq!(
            evaluate_reentry_gate(&ReentryGateInput {
                stable_since_ms: 1_000_000 - 180 * MIN,
                storm_active: true,
                ..reentry()
            }),
            ReentryDecision::Hold
        );
        assert_eq!(
            evaluate_reentry_gate(&ReentryGateInput {
                price: 82.0,
                storm_active: true,
                ..reentry()
            }),
            ReentryDecision::Rearm {
                anchor_price: 82.0,
                stable_since_ms: 1_000_000
            }
        );
    }

    #[test]
    fn reentry_confirm_zero_opens_immediately() {
        assert_eq!(
            evaluate_reentry_gate(&ReentryGateInput {
                confirm_ms: 0,
                stable_since_ms: 1_000_000,
                ..reentry()
            }),
            ReentryDecision::Open
        );
    }

    #[test]
    fn reentry_never_acts_on_garbage() {
        for price in [f64::NAN, 0.0, -5.0, f64::INFINITY] {
            assert_eq!(
                evaluate_reentry_gate(&ReentryGateInput {
                    price,
                    stable_since_ms: 1_000_000 - 500 * MIN,
                    ..reentry()
                }),
                ReentryDecision::Hold
            );
        }
        assert_eq!(
            evaluate_reentry_gate(&ReentryGateInput {
                anchor_price: f64::NAN,
                stable_since_ms: 1_000_000 - 500 * MIN,
                ..reentry()
            }),
            ReentryDecision::Hold
        );
    }

    #[test]
    fn reentry_decision_serde_and_width() {
        let v = serde_json::to_value(ReentryDecision::Rearm {
            anchor_price: 80.5,
            stable_since_ms: 7,
        })
        .unwrap();
        assert_eq!(
            v,
            serde_json::json!({"action": "rearm", "anchor_price": 80.5, "stable_since_ms": 7})
        );
        assert_eq!(
            serde_json::to_value(ReentryDecision::Hold).unwrap(),
            serde_json::json!({"action": "hold"})
        );
        assert!(close_to(reentry_width_frac(79.0, 81.0, 80.0), 0.025, 12));
        assert_eq!(reentry_width_frac(81.0, 79.0, 80.0), 0.02);
        assert_eq!(reentry_width_frac(79.0, 81.0, 0.0), 0.02);
    }

    // --- storm hysteresis --------------------------------------------------

    fn storm(now_ms: i64, price: f64, samples: Vec<PriceSample>, active: bool) -> StormOutput {
        storm_update(&StormInput {
            now_ms,
            price,
            samples,
            threshold_pct: 2.0,
            active,
        })
    }

    fn s(t_ms: i64, usd: f64) -> PriceSample {
        PriceSample { t_ms, usd }
    }

    #[test]
    fn storm_needs_a_reference_at_least_four_minutes_old() {
        let now = 10 * MIN;
        // Only a 3-min-old sample: no reference → move None, state kept.
        let out = storm(now, 90.0, vec![s(now - 3 * MIN, 80.0)], false);
        assert_eq!(out.move_5m_pct, None);
        assert!(!out.active);
        let out = storm(now, 90.0, vec![s(now - 3 * MIN, 80.0)], true);
        assert!(out.active, "no reference keeps the previous state");
        assert_eq!(out.samples.len(), 2);
    }

    #[test]
    fn storm_reference_is_oldest_qualifying_sample_within_six_minutes() {
        let now = 20 * MIN;
        let samples = vec![
            s(now - 7 * MIN, 50.0),  // outside the 6-min window: pruned
            s(now - 5 * MIN, 100.0), // oldest ≥ 4 min: the reference
            s(now - 4 * MIN, 200.0),
            s(now - MIN, 300.0),
        ];
        let out = storm(now, 101.0, samples, false);
        assert_eq!(out.samples.first().unwrap().t_ms, now - 5 * MIN);
        assert_eq!(out.samples.last().unwrap(), &s(now, 101.0));
        assert!(close_to(out.move_5m_pct.unwrap(), 1.0, 9));
        assert!(!out.active);
        // Exactly the 6-min boundary is kept (bot drops t < cutoff).
        let out = storm(now, 100.0, vec![s(now - 6 * MIN, 90.0)], false);
        assert_eq!(out.samples.len(), 2);
    }

    #[test]
    fn storm_enters_above_threshold_and_exits_below_half() {
        let now = 20 * MIN;
        let window = |p: f64| vec![s(now - 5 * MIN, 100.0), s(now - MIN, p)];
        assert!(!storm(now, 101.9, window(101.0), false).active);
        assert!(storm(now, 102.5, window(101.0), false).active);
        assert!(
            storm(now, 97.0, window(99.0), false).active,
            "|move| counts both ways"
        );
        // Active: stays while ≥ thr/2, exits below it.
        assert!(storm(now, 101.5, window(101.0), true).active);
        assert!(!storm(now, 100.5, window(100.0), true).active);
        // Exact boundaries (dyadic moves, threshold 50 %): entry is strictly
        // `>` (bot + simulator), exit strictly `<` thr/2.
        let exact = |price: f64, active: bool| {
            storm_update(&StormInput {
                now_ms: now,
                price,
                samples: vec![s(now - 5 * MIN, 100.0)],
                threshold_pct: 50.0,
                active,
            })
        };
        let at_threshold = exact(150.0, false);
        assert_eq!(at_threshold.move_5m_pct, Some(50.0));
        assert!(!at_threshold.active, "move == threshold does not enter");
        let at_half = exact(125.0, true);
        assert_eq!(at_half.move_5m_pct, Some(25.0));
        assert!(at_half.active, "move == threshold/2 does not exit");
    }

    #[test]
    fn storm_disabled_or_garbage_price() {
        let now = 20 * MIN;
        let out = storm_update(&StormInput {
            now_ms: now,
            price: 150.0,
            samples: vec![s(now - 5 * MIN, 100.0)],
            threshold_pct: 0.0,
            active: true,
        });
        assert!(!out.active);
        assert!(close_to(out.move_5m_pct.unwrap(), 50.0, 9));
        for price in [0.0, -1.0, f64::NAN] {
            let out = storm(now, price, vec![s(now - 5 * MIN, 100.0)], false);
            assert_eq!(out.move_5m_pct, None);
            assert!(!out.active);
            assert_eq!(out.samples.len(), 1, "garbage price is not recorded");
        }
    }

    // --- trend confirm -----------------------------------------------------

    fn trend(
        now_ms: i64,
        is_imbalanced: bool,
        since: Option<i64>,
        storm_active: bool,
    ) -> TrendConfirmOutput {
        trend_confirm(&TrendConfirmInput {
            now_ms,
            is_imbalanced,
            imbalance_since_ms: since,
            confirm_ms: 5 * MIN,
            storm_active,
        })
    }

    #[test]
    fn trend_starts_timer_waits_then_recenters() {
        let first = trend(100 * MIN, true, None, false);
        assert_eq!(first.imbalance_since_ms, Some(100 * MIN));
        assert_eq!(first.action, ImbalanceAction::Waiting);
        assert_eq!(first.remaining_ms, 5 * MIN);
        let mid = trend(103 * MIN, true, first.imbalance_since_ms, false);
        assert_eq!(mid.action, ImbalanceAction::Waiting);
        assert_eq!((mid.held_ms, mid.remaining_ms), (3 * MIN, 2 * MIN));
        let done = trend(105 * MIN, true, first.imbalance_since_ms, false);
        assert!(done.confirmed);
        assert_eq!(done.action, ImbalanceAction::Recenter);
    }

    #[test]
    fn trend_whipsaw_resets_and_storm_pauses() {
        let reset = trend(103 * MIN, false, Some(100 * MIN), false);
        assert_eq!(reset.imbalance_since_ms, None);
        assert_eq!(reset.action, ImbalanceAction::Balanced);
        let paused = trend(110 * MIN, true, Some(100 * MIN), true);
        assert!(paused.confirmed);
        assert_eq!(paused.action, ImbalanceAction::StormPaused);
        let zero = trend_confirm(&TrendConfirmInput {
            now_ms: 1,
            is_imbalanced: true,
            imbalance_since_ms: None,
            confirm_ms: 0,
            storm_active: false,
        });
        assert_eq!(zero.action, ImbalanceAction::Recenter);
    }

    // --- regime confirm ----------------------------------------------------

    fn regime(
        committed: LpRegime,
        computed: LpRegime,
        pending: Option<PendingRegime>,
    ) -> RegimeConfirmInput {
        RegimeConfirmInput {
            now_ms: 100 * MIN,
            committed,
            computed,
            pending,
            confirm_ms: 5 * MIN,
            storm_active: false,
            imbalance_pending: false,
            last_rebalance_failed: false,
        }
    }

    #[test]
    fn regime_candidate_waits_then_commits() {
        let out = regime_confirm(&regime(LpRegime::In, LpRegime::Below, None));
        assert_eq!(out.outcome, RegimeOutcome::Pending);
        assert_eq!(out.committed, LpRegime::In);
        assert_eq!(
            out.pending,
            Some(PendingRegime {
                regime: LpRegime::Below,
                since_ms: 100 * MIN
            })
        );
        let aged = PendingRegime {
            regime: LpRegime::Below,
            since_ms: 95 * MIN,
        };
        let out = regime_confirm(&regime(LpRegime::In, LpRegime::Below, Some(aged)));
        assert_eq!(out.outcome, RegimeOutcome::Committed);
        assert_eq!((out.committed, out.pending), (LpRegime::Below, None));
        // A different candidate restarts the clock.
        let out = regime_confirm(&regime(LpRegime::In, LpRegime::Above, Some(aged)));
        assert_eq!(out.outcome, RegimeOutcome::Pending);
        assert_eq!(out.pending.unwrap().since_ms, 100 * MIN);
        // Back to the committed regime clears the candidate.
        let out = regime_confirm(&regime(LpRegime::In, LpRegime::In, Some(aged)));
        assert_eq!((out.outcome, out.pending), (RegimeOutcome::Unchanged, None));
    }

    #[test]
    fn regime_storm_is_immediate_and_recenter_freezes() {
        let out = regime_confirm(&RegimeConfirmInput {
            storm_active: true,
            imbalance_pending: true,
            ..regime(LpRegime::In, LpRegime::Below, None)
        });
        assert_eq!(out.outcome, RegimeOutcome::Committed);
        let aged = Some(PendingRegime {
            regime: LpRegime::Below,
            since_ms: 90 * MIN,
        });
        let frozen = regime_confirm(&RegimeConfirmInput {
            imbalance_pending: true,
            ..regime(LpRegime::In, LpRegime::Below, aged)
        });
        assert_eq!(frozen.outcome, RegimeOutcome::Frozen);
        assert_eq!(frozen.committed, LpRegime::In);
        assert_eq!(
            frozen.pending.unwrap().since_ms,
            90 * MIN,
            "candidate keeps aging"
        );
        let lifted = regime_confirm(&RegimeConfirmInput {
            imbalance_pending: true,
            last_rebalance_failed: true,
            ..regime(LpRegime::In, LpRegime::Below, aged)
        });
        assert_eq!(lifted.outcome, RegimeOutcome::Committed);
    }

    // --- composition + imbalance -------------------------------------------

    #[test]
    fn composition_matches_live_bot_logs() {
        // (current, lower, upper, sol_percent) verbatim from the bot's
        // 2026-07-06 `Position balance checked` logs (simulator/tests/golden.rs).
        for (cur, lo, hi, sol) in [
            (
                80.6612869634628,
                80.46797060784459,
                81.11416647995036,
                70.08,
            ),
            (
                80.33935050069807,
                79.98670427356109,
                80.56457080234985,
                38.97,
            ),
            (80.8227386470824, 80.40363483539471, 81.0169072978923, 31.66),
        ] {
            let c = token_percentages(cur, lo, hi).unwrap();
            assert_eq!(c.token_x, sol);
            assert!(close_to(c.token_x + c.token_y, 100.0, 9));
        }
    }

    #[test]
    fn composition_out_of_range_and_degenerate() {
        assert_eq!(
            token_percentages(79.0, 80.0, 81.0),
            Some(TokenPercentages {
                token_x: 100.0,
                token_y: 0.0
            })
        );
        assert_eq!(
            token_percentages(82.0, 80.0, 81.0),
            Some(TokenPercentages {
                token_x: 0.0,
                token_y: 100.0
            })
        );
        // 1-bin range: bot divides 0/0 → NaN; here 50/50.
        assert_eq!(
            token_percentages(80.0, 80.0, 80.0),
            Some(TokenPercentages {
                token_x: 50.0,
                token_y: 50.0
            })
        );
        assert_eq!(token_percentages(f64::NAN, 80.0, 81.0), None);
        assert_eq!(check_position_imbalance(f64::NAN, 80.0, 81.0, 0.92), None);
    }

    #[test]
    fn imbalance_threshold_is_inclusive_on_either_side() {
        // 92 % token X exactly at a 0.92 threshold → imbalanced (>=).
        let lo = 80.0;
        let hi = 81.0;
        let at = |x_share: f64| lo + (1.0 - x_share) * (hi - lo);
        let r = check_position_imbalance(at(0.92), lo, hi, 0.92).unwrap();
        assert!(r.is_imbalanced);
        assert_eq!(r.x_percent, 92.0);
        assert_eq!(
            r.reason.as_deref(),
            Some("token X concentration 92% exceeds 92% threshold")
        );
        let r = check_position_imbalance(at(0.05), lo, hi, 0.92).unwrap();
        assert!(r.is_imbalanced);
        assert!(r.reason.unwrap().starts_with("token Y concentration 95%"));
        let r = check_position_imbalance(at(0.5), lo, hi, 0.92).unwrap();
        assert!(!r.is_imbalanced);
        assert_eq!(r.reason, None);
    }

    // --- wallet 50/50 (meteoraUtils.test.ts) -------------------------------

    #[test]
    fn wallet_5050_skewed_and_balanced_cases() {
        let r = wallet_balanced_for_5050(0.05, 200.0, 90.0, 0.3, 0.10);
        assert!(!r.balanced);
        assert_eq!(r.wallet_sol_ratio, 0.0);
        assert!(close_to(r.wallet_total_usd, 200.0, 6));
        let r = wallet_balanced_for_5050(5.0, 5.0, 90.0, 0.3, 0.10);
        assert!(!r.balanced && r.wallet_sol_ratio > 0.9);
        let r = wallet_balanced_for_5050(1.3, 90.0, 90.0, 0.3, 0.10);
        assert!(r.balanced);
        assert!(close_to(r.wallet_sol_ratio, 0.5, 6));
    }

    #[test]
    fn wallet_5050_tolerance_edges() {
        for (sol, usdc, ratio, balanced) in [
            (0.7, 60.0, 0.4, true),
            (0.9, 40.0, 0.6, true),
            (0.69, 61.0, 0.39, false),
            (0.91, 39.0, 0.61, false),
        ] {
            let r = wallet_balanced_for_5050(sol, usdc, 100.0, 0.3, 0.10);
            assert_eq!(r.balanced, balanced, "{sol} SOL / {usdc} USDC");
            assert!(close_to(r.wallet_sol_ratio, ratio, 6));
        }
        let r = wallet_balanced_for_5050(0.86, 44.0, 100.0, 0.3, 0.10);
        assert!(close_to(r.wallet_sol_ratio, 0.56, 6));
        assert!(r.balanced);
        assert!(!wallet_balanced_for_5050(0.86, 44.0, 100.0, 0.3, 0.05).balanced);
    }

    #[test]
    fn wallet_5050_reserves_and_empty() {
        let r = wallet_balanced_for_5050(0.3, 100.0, 90.0, 0.3, 0.10);
        assert!(!r.balanced);
        assert_eq!(r.wallet_sol_ratio, 0.0);
        let r = wallet_balanced_for_5050(0.1, 100.0, 90.0, 0.3, 0.10);
        assert!(!r.balanced);
        assert_eq!(r.wallet_sol_ratio, 0.0);
        assert!(close_to(r.wallet_total_usd, 100.0, 6));
        for (sol, usdc) in [(0.0, 0.0), (0.3, 0.0)] {
            let r = wallet_balanced_for_5050(sol, usdc, 90.0, 0.3, 0.10);
            assert!(r.balanced);
            assert_eq!(r.wallet_sol_ratio, 0.5);
            assert_eq!(r.wallet_total_usd, 0.0);
        }
    }

    // --- deposit swap planner (swapPlanner.test.ts) ------------------------

    fn deposit(overrides: impl FnOnce(&mut DepositSwapInput)) -> DepositSwapPlan {
        let mut input = DepositSwapInput {
            wallet_sol: 10.0,
            wallet_usdc: 1_000.0,
            target_sol: 1.0,
            target_usdc: 100.0,
            permanent_minimum_sol: 0.2,
            rent_reserve_sol: 0.1,
            position_rent_sol: 0.0,
            reserve_usdc: 0.0,
            current_price: 100.0,
            slippage_buffer_pct: 0.02,
            context: DepositSwapContext::Rebalance,
            base_mint: "SOL".into(),
            quote_mint: "USDC".into(),
        };
        overrides(&mut input);
        plan_swap_for_deposit(&input)
    }

    #[test]
    fn deposit_swap_noop_and_both_directions_match_bot() {
        let noop = deposit(|i| {
            i.wallet_sol = 5.0;
            i.wallet_usdc = 500.0;
        });
        assert!(noop.feasible && !noop.needed && noop.swaps.is_empty());
        assert!(close_to(noop.available_sol_for_swap, 4.7, 8));

        let to_usdc = deposit(|i| {
            i.wallet_sol = 5.0;
            i.wallet_usdc = 50.0;
        });
        let swap = &to_usdc.swaps[0];
        assert_eq!(swap.direction, DepositSwapDirection::SolToUsdc);
        assert!(close_to(swap.amount, 0.51, 8));
        assert!(close_to(swap.expected_output, 50.0, 8));

        let to_sol = deposit(|i| {
            i.wallet_sol = 0.5;
        });
        let swap = &to_sol.swaps[0];
        assert_eq!(swap.direction, DepositSwapDirection::UsdcToSol);
        assert!(close_to(swap.amount, 81.6, 8));
        assert!(close_to(swap.expected_output, 0.8, 8));
    }

    #[test]
    fn deposit_swap_blocks_the_production_underfunded_case() {
        let p = deposit(|i| {
            i.wallet_sol = 0.258;
            i.wallet_usdc = 9.43;
            i.target_sol = 4.0;
            i.target_usdc = 400.0;
        });
        assert!(!p.feasible && p.swaps.is_empty());
        assert_eq!(
            p.block.unwrap().code,
            DepositSwapBlockCode::InsufficientTotalValue
        );
    }

    #[test]
    fn deposit_swap_budgets_position_rent_and_hedge_usdc() {
        let rent = deposit(|i| {
            i.wallet_sol = 0.47394;
            i.wallet_usdc = 145.0;
            i.target_sol = 0.611809;
            i.target_usdc = 48.703757;
            i.position_rent_sol = 0.0575;
            i.current_price = 79.29;
            i.slippage_buffer_pct = 0.03;
        });
        assert!(close_to(rent.shortfall.sol, 0.495369, 6));
        assert_eq!(rent.swaps[0].direction, DepositSwapDirection::UsdcToSol);

        let collateral = deposit(|i| {
            i.wallet_sol = 3.4;
            i.wallet_usdc = 54.05;
            i.target_sol = 0.668;
            i.target_usdc = 46.54;
            i.permanent_minimum_sol = 0.3;
            i.position_rent_sol = 0.0575;
            i.reserve_usdc = 16.1;
            i.current_price = 76.2;
        });
        assert_eq!(
            collateral.swaps[0].direction,
            DepositSwapDirection::SolToUsdc
        );
        assert!(close_to(collateral.shortfall.usdc, 8.59, 6));
    }

    #[test]
    fn deposit_swap_invalid_input_fails_closed() {
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let p = deposit(|i| i.current_price = bad);
            assert_eq!(p.block.unwrap().code, DepositSwapBlockCode::InvalidInput);
        }
    }

    // --- bin math ----------------------------------------------------------

    #[test]
    fn price_from_bin_matches_decimal_js() {
        // (bin_id, bin_step, dec_x, dec_y, Decimal.js getPriceFromBinId().toNumber())
        for (id, step, dx, dy, want) in [
            (-4744, 4, 9, 6, 149.98491131558055),
            (0, 4, 9, 6, 1000.0),
            (1, 10, 9, 6, 1001.0),
            (-1, 10, 9, 6, 999.000999000999),
            (5000, 1, 6, 6, 1.6486800559311758),
            (-2345, 20, 9, 6, 9.229815277234804),
            (123, 100, 6, 9, 0.0034003919178661416),
            (-70, 80, 9, 9, 0.5724832077400298),
            // Live SOL/USDC 4-bps range (bot logs 2026-07-06).
            (-6301, 4, 9, 6, 80.46797060784459),
            (-6281, 4, 9, 6, 81.11416647995036),
        ] {
            let got = price_from_bin(id, step, dx, dy);
            assert!(
                rel_close(got, want, 1e-14),
                "bin {id} step {step}: {got} vs {want}"
            );
        }
    }

    #[test]
    fn bin_from_price_roundings() {
        let exact = price_from_bin(-6301, 4, 9, 6);
        for r in [Rounding::Down, Rounding::Up, Rounding::Nearest] {
            assert_eq!(bin_from_price(exact, 4, 9, 6, r), Some(-6301), "{r:?}");
        }
        // Decimal.js: ln(0.150)/ln(1.0004) = -4743.748… → floor -4744, ceil -4743.
        assert_eq!(bin_from_price(150.0, 4, 9, 6, Rounding::Down), Some(-4744));
        assert_eq!(bin_from_price(150.0, 4, 9, 6, Rounding::Up), Some(-4743));
        assert_eq!(
            bin_from_price(150.0, 4, 9, 6, Rounding::Nearest),
            Some(-4744)
        );
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(bin_from_price(bad, 4, 9, 6, Rounding::Down), None);
        }
        assert_eq!(bin_from_price(150.0, 0, 9, 6, Rounding::Down), None);
        for id in [-20_000, -6301, -1, 0, 1, 70, 12_345] {
            let p = price_from_bin(id, 10, 9, 6);
            assert_eq!(bin_from_price(p, 10, 9, 6, Rounding::Up), Some(id));
        }
    }

    #[test]
    fn bin_array_index_floors_negatives() {
        for (id, idx) in [
            (0, 0),
            (69, 0),
            (70, 1),
            (-1, -1),
            (-70, -1),
            (-71, -2),
            (-6301, -91),
            (-6281, -90),
        ] {
            assert_eq!(bin_array_index(id), idx, "bin {id}");
        }
        assert_eq!(bin_array_bounds(-1), (-70, -1));
        assert_eq!(bin_array_bounds(1), (70, 139));
        assert_eq!(bin_array_indexes(-6301, -6281), vec![-91, -90]);
        assert_eq!(bin_array_indexes(0, 69), vec![0]);
        assert!(bin_array_indexes(5, 4).is_empty());
    }

    #[test]
    fn centered_range_caps_at_seventy_bins() {
        let r = centered_range(-6291, 20).unwrap();
        assert_eq!(
            (r.min_bin_id, r.max_bin_id, r.width, r.clamped_to_max_width),
            (-6301, -6282, 20, false)
        );
        let r = centered_range(100, 21).unwrap();
        assert_eq!((r.min_bin_id, r.max_bin_id, r.width), (90, 110, 21));
        let r = centered_range(0, 70).unwrap();
        assert_eq!(
            (r.min_bin_id, r.max_bin_id, r.width, r.clamped_to_max_width),
            (-35, 34, 70, false)
        );
        // The bot's over-width fallback spans active ± 35 = 71 bins; never here.
        let r = centered_range(0, 500).unwrap();
        assert_eq!(
            (r.min_bin_id, r.max_bin_id, r.width, r.clamped_to_max_width),
            (-35, 34, 70, true)
        );
        assert_eq!(i64::from(r.max_bin_id) - i64::from(r.min_bin_id) + 1, 70);
        assert_eq!(centered_range(5, 0), None);
        assert_eq!(centered_range(i32::MAX, 70), None);
    }

    // --- fee rate ----------------------------------------------------------

    #[test]
    fn fee_rate_matches_sdk_formulas() {
        // SOL/USDC 4-bps pool: base_factor 10000 → 0.04 % base fee.
        let p = DlmmFeeParams {
            bin_step: 4,
            base_factor: 10_000,
            base_fee_power_factor: 0,
            variable_fee_control: 0,
            volatility_accumulator: 123_456,
            protocol_share: 1000,
        };
        let r = dlmm_fee_rates(&p);
        assert_eq!(r.base_rate, 400_000);
        assert_eq!(r.variable_rate, 0, "vfc 0 disables the variable fee");
        assert!(close_to(r.base_fee_pct, 0.04, 12));
        assert!(close_to(r.max_fee_pct, 10.0, 12));
        assert!(close_to(r.protocol_share_pct, 10.0, 12));
        // Variable: ceil(vfc × (va × step)² / 1e11).
        let p = DlmmFeeParams {
            variable_fee_control: 7500,
            volatility_accumulator: 10_000,
            ..p
        };
        // (10_000 × 4)² = 1.6e9; × 7500 = 1.2e13; / 1e11 = 120 exactly.
        assert_eq!(variable_fee_rate(4, 7500, 10_000), 120);
        // 1 unit over → ceil adds one.
        assert_eq!(variable_fee_rate(4, 7500, 10_001), 121);
        let r = dlmm_fee_rates(&p);
        assert_eq!(r.total_rate, 400_120);
        assert!(close_to(r.total_fee_pct, 0.040012, 12));
        // Power factor + cap.
        assert_eq!(base_fee_rate(80, 10_000, 1), 80_000_000);
        let capped = dlmm_fee_rates(&DlmmFeeParams {
            bin_step: 400,
            base_factor: 60_000,
            base_fee_power_factor: 0,
            variable_fee_control: 0,
            volatility_accumulator: 0,
            protocol_share: 0,
        });
        assert_eq!(capped.base_rate, 240_000_000);
        assert_eq!(capped.total_rate, MAX_FEE_RATE);
        assert!(close_to(capped.total_fee_pct, 10.0, 12));
        // Extremes never overflow.
        assert_eq!(base_fee_rate(u16::MAX, u16::MAX, u8::MAX), u128::MAX);
        let extreme = dlmm_fee_rates(&DlmmFeeParams {
            bin_step: u16::MAX,
            base_factor: u16::MAX,
            base_fee_power_factor: u8::MAX,
            variable_fee_control: u32::MAX,
            volatility_accumulator: u32::MAX,
            protocol_share: u16::MAX,
        });
        assert!(extreme.variable_rate > 0);
        assert_eq!(extreme.total_rate, MAX_FEE_RATE);
    }

    #[test]
    fn volatility_projection_matches_sdk() {
        let v = VolatilityState {
            active_id: 105,
            index_reference: 100,
            volatility_reference: 20_000,
            volatility_accumulator: 70_000,
            last_update_timestamp: 1_000,
            filter_period: 30,
            decay_period: 600,
            reduction_factor: 5000,
            max_volatility_accumulator: 350_000,
        };
        // Inside the filter period: reference kept, |100 − 105| × 1e4 added.
        assert_eq!(dynamic_volatility_accumulator(&v, 1_010), 20_000 + 50_000);
        // Past filter, before decay: reference = floor(70_000 × 0.5), index = active.
        assert_eq!(dynamic_volatility_accumulator(&v, 1_100), 35_000);
        // Past decay: reference 0.
        assert_eq!(dynamic_volatility_accumulator(&v, 2_000), 0);
        // Capped at max.
        let far = VolatilityState {
            index_reference: 0,
            ..v
        };
        assert_eq!(dynamic_volatility_accumulator(&far, 1_010), 350_000);
    }

    // --- swap oracle gate (swapPlanner.test.ts) ----------------------------

    fn swap(
        direction: SwapDirection,
        input_amount: f64,
        output_amount: f64,
        oracle: f64,
    ) -> SwapOracleGateResult {
        check_swap_oracle_gate(&SwapOracleGateInput {
            direction,
            input_amount,
            output_amount,
            oracle_price_usd: oracle,
            tolerance_bps: 50.0,
        })
    }

    #[test]
    fn swap_gate_passes_and_blocks() {
        let r = swap(SwapDirection::SolToUsdc, 1.0, 82.0, 82.0);
        assert!(r.ok);
        assert!(close_to(r.deviation_bps.unwrap(), 0.0, 5));
        let r = swap(SwapDirection::UsdcToSol, 82.2, 1.0, 82.0);
        assert!(r.ok);
        let d = r.deviation_bps.unwrap();
        assert!(d > 20.0 && d < 30.0, "{d}");
        let r = swap(SwapDirection::SolToUsdc, 1.0, 81.0, 82.0);
        assert!(!r.ok && r.deviation_bps.unwrap() > 100.0);
        assert!(
            !swap(SwapDirection::SolToUsdc, 1.0, 83.5, 82.0).ok,
            "too good is suspicious too"
        );
    }

    #[test]
    fn swap_gate_fails_closed() {
        for r in [
            swap(SwapDirection::SolToUsdc, 1.0, 0.0, 82.0),
            swap(SwapDirection::UsdcToSol, 82.0, 1.0, 0.0),
            swap(SwapDirection::SolToUsdc, f64::INFINITY, 82.0, 82.0),
            swap(SwapDirection::SolToUsdc, 1.0, f64::NAN, 82.0),
        ] {
            assert!(!r.ok);
            assert_eq!((r.implied_price_usd, r.deviation_bps), (None, None));
        }
        assert_eq!(
            serde_json::to_value(SwapDirection::SolToUsdc).unwrap(),
            serde_json::json!("SOL_TO_USDC")
        );
    }
}
