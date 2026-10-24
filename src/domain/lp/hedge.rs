//! Hedge controller — the pure decision core of the perps hedge. Behaviour
//! port of `delta_neutral_bot/simulator/src/hedge.rs:1-303`, itself a port of
//! the production `delta_neutral_bot/src/modules/hedgeController.ts:94-375`
//! (ADR-017/018/019/021/022/025, BUG-012/013). TypeScript is the source of
//! truth — it trades real money; where the simulator and TS differ this file
//! follows TS:
//!
//! | Item | Simulator | Here (= TS) |
//! |---|---|---|
//! | `Decision` reasons | short fixed strings | TS text, `toFixed` via [`js_to_fixed`] (`hedgeController.ts:105-230`) |
//! | LP delta with empty exposure | via regime → midpoint | [`lp_hedge_delta`] returns 0 / `In` (`hedgeController.ts:292`) |
//! | `auto_band_sol` | absent | ported (`hedgeController.ts:365-375`) |
//!
//! `tests/fixtures/hedge-vectors.jsonl` (1027 vectors exported from the TS
//! controller by `scripts/export-hedge-vectors.ts`) is replayed by the unit
//! test below with the simulator's 1e-9 relative tolerance.
//!
//! Pure: no IO, no clocks — `now_ms` is an input.

// `!(x > 0.0)` is the TS NaN-rejecting guard, kept verbatim.
#![allow(clippy::neg_cmp_op_on_partial_ord)]

use serde::{Deserialize, Serialize};

/// Residual position (SOL) below which a side counts as fully closed.
pub(crate) const EPSILON_SOL: f64 = 1e-9;
/// Smallest increase worth sending, USD (fees + rent eat a smaller fill).
pub(crate) const MIN_HEDGE_INCREASE_USD: f64 = 10.0;

/// One controller snapshot — `HedgeDecisionInput` (`hedgeController.ts:33-69`)
/// with the simulator's flat snake_case fields (the vector fixture shape).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct HedgeInput {
    /// SOL held long via the LP (after the midpoint/clamp transform).
    pub lp_sol: f64,
    /// Open perp sides, positive magnitudes in SOL (0 = side not open).
    pub long_sol: f64,
    pub short_sol: f64,
    pub long_notional_usd: f64,
    pub short_notional_usd: f64,
    pub long_collateral_usd: f64,
    pub short_collateral_usd: f64,
    /// Carry COST per side, bps APR (positive = the side pays).
    pub carry_cost_bps_long: f64,
    pub carry_cost_bps_short: f64,
    /// `None` encodes the TS `NaN` case (JSON cannot carry NaN).
    pub oracle_price_usd: Option<f64>,
    /// Wallet native SOL (long collateral comes from here).
    pub wallet_sol: f64,
    /// SOL that must stay in the wallet (minimum balance + rent reserve).
    pub wallet_reserve_sol: f64,
    /// Wallet USDC (short collateral comes from here).
    pub wallet_usdc: f64,
    pub target_delta_sol: f64,
    pub band_sol: f64,
    /// 0 disables the carry gate.
    pub carry_cap_bps: f64,
    pub max_hedge_notional_usd: f64,
    pub min_collateral_ratio: f64,
    pub target_collateral_ratio: f64,
    pub now_ms: i64,
    pub last_action_at_ms: Option<i64>,
    pub cooldown_ms: i64,
}

/// Exactly one action per cycle — `HedgeDecision` (`hedgeController.ts:71-92`).
/// Serialises as `{"action": "increase_short", "size_usd": …}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub(crate) enum Decision {
    None {
        reason: String,
    },
    Blocked {
        reason: String,
    },
    DecreaseLong {
        size_usd: f64,
        entire_position: bool,
        withdraw_collateral_usd: f64,
        adjust_sol: f64,
    },
    DecreaseShort {
        size_usd: f64,
        entire_position: bool,
        withdraw_collateral_usd: f64,
        adjust_sol: f64,
    },
    IncreaseLong {
        size_usd: f64,
        collateral_tokens: f64,
        adjust_sol: f64,
    },
    IncreaseShort {
        size_usd: f64,
        collateral_tokens: f64,
        adjust_sol: f64,
    },
}

impl Decision {
    #[cfg(test)]
    pub(crate) fn action(&self) -> &'static str {
        match self {
            Decision::None { .. } => "none",
            Decision::Blocked { .. } => "blocked",
            Decision::DecreaseLong { .. } => "decrease_long",
            Decision::DecreaseShort { .. } => "decrease_short",
            Decision::IncreaseLong { .. } => "increase_long",
            Decision::IncreaseShort { .. } => "increase_short",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Side {
    Long,
    Short,
}

impl Side {
    fn label(self) -> &'static str {
        match self {
            Side::Long => "long",
            Side::Short => "short",
        }
    }
}

/// `decideHedgeAction` (`hedgeController.ts:94-161`). Drives
/// `error = (lp + long − short) − target` to 0 when `|error| > band`,
/// decrease-first, one mutation per call.
pub(crate) fn decide(input: &HedgeInput) -> Decision {
    let price = input.oracle_price_usd.unwrap_or(f64::NAN);
    if !(price > 0.0) || !price.is_finite() {
        return Decision::Blocked {
            reason: "no oracle SOL price available".into(),
        };
    }

    let net_delta = input.lp_sol + input.long_sol - input.short_sol;
    let error = net_delta - input.target_delta_sol;

    if error.abs() <= input.band_sol {
        return Decision::None {
            reason: "in band".into(),
        };
    }

    // Cooldown AFTER the band check: an in-band read still reports the honest
    // reason; only actual mutations are suppressed.
    if let Some(last) = input.last_action_at_ms {
        if input.cooldown_ms > 0 {
            let elapsed = input.now_ms - last;
            if elapsed < input.cooldown_ms {
                return Decision::None {
                    reason: format!(
                        "cooldown: previous hedge request may still be filling ({}ms remaining)",
                        (input.cooldown_ms - elapsed).max(0)
                    ),
                };
            }
        }
    }

    if error > 0.0 {
        // Too much delta → decrease the long first, else grow the short.
        if input.long_sol > EPSILON_SOL {
            let adjust = error.min(input.long_sol);
            let entire = adjust >= input.long_sol - EPSILON_SOL;
            let size_usd = adjust * price;
            return Decision::DecreaseLong {
                size_usd,
                entire_position: entire,
                withdraw_collateral_usd: if entire {
                    0.0
                } else {
                    size_usd * input.target_collateral_ratio
                },
                adjust_sol: -adjust,
            };
        }
        return guard_increase(input, Side::Short, error, price);
    }

    // Too little delta → decrease the short first, else open a long.
    let deficit = -error;
    if input.short_sol > EPSILON_SOL {
        let adjust = deficit.min(input.short_sol);
        let entire = adjust >= input.short_sol - EPSILON_SOL;
        let size_usd = adjust * price;
        return Decision::DecreaseShort {
            size_usd,
            entire_position: entire,
            withdraw_collateral_usd: if entire {
                0.0
            } else {
                size_usd * input.target_collateral_ratio
            },
            adjust_sol: adjust,
        };
    }
    guard_increase(input, Side::Long, deficit, price)
}

/// `guardIncrease` (`hedgeController.ts:163-245`): carry cap, BUG-012 headroom
/// fill, BUG-013 collateral fill, projected collateral ratio floor.
fn guard_increase(input: &HedgeInput, side: Side, mut adjust_sol: f64, price: f64) -> Decision {
    let carry_cost = match side {
        Side::Long => input.carry_cost_bps_long,
        Side::Short => input.carry_cost_bps_short,
    };
    if input.carry_cap_bps > 0.0 && carry_cost > input.carry_cap_bps {
        return Decision::Blocked {
            reason: format!(
                "{} carry {}% APR exceeds cap {}%",
                side.label(),
                js_to_fixed(carry_cost / 100.0, 2),
                js_to_fixed(input.carry_cap_bps / 100.0, 2)
            ),
        };
    }

    // BUG-012: fill the remaining notional-cap headroom instead of blocking.
    let current_notional = match side {
        Side::Long => input.long_notional_usd,
        Side::Short => input.short_notional_usd,
    };
    let mut size_usd = adjust_sol * price;
    let headroom = input.max_hedge_notional_usd - current_notional;
    if size_usd > headroom {
        if headroom < MIN_HEDGE_INCREASE_USD {
            return Decision::Blocked {
                reason: format!(
                    "projected {} notional ${} exceeds max ${} and headroom ${} is below the ${} minimum increase",
                    side.label(),
                    js_to_fixed(current_notional + size_usd, 2),
                    js_to_fixed(input.max_hedge_notional_usd, 2),
                    js_to_fixed(headroom.max(0.0), 2),
                    MIN_HEDGE_INCREASE_USD
                ),
            };
        }
        size_usd = headroom;
        adjust_sol = size_usd / price;
    }

    // BUG-013: the collateral must physically exist — fill the affordable size.
    let available_collateral = match side {
        Side::Short => input.wallet_usdc.max(0.0),
        Side::Long => ((input.wallet_sol - input.wallet_reserve_sol) * price).max(0.0),
    };
    if size_usd * input.target_collateral_ratio > available_collateral {
        let affordable = available_collateral / input.target_collateral_ratio;
        if affordable < MIN_HEDGE_INCREASE_USD {
            let reason = match side {
                Side::Short => format!(
                    "short collateral ${} exceeds wallet USDC ${} and the affordable size ${} is below the ${} minimum increase",
                    js_to_fixed(size_usd * input.target_collateral_ratio, 2),
                    js_to_fixed(input.wallet_usdc, 2),
                    js_to_fixed(affordable, 2),
                    MIN_HEDGE_INCREASE_USD
                ),
                Side::Long => format!(
                    "long collateral {} SOL exceeds available wallet SOL {} above reserves and the affordable size ${} is below the ${} minimum increase",
                    js_to_fixed(size_usd * input.target_collateral_ratio / price, 4),
                    js_to_fixed((input.wallet_sol - input.wallet_reserve_sol).max(0.0), 4),
                    js_to_fixed(affordable, 2),
                    MIN_HEDGE_INCREASE_USD
                ),
            };
            return Decision::Blocked { reason };
        }
        size_usd = affordable;
        adjust_sol = size_usd / price;
    }

    let projected_notional = current_notional + size_usd;
    let collateral_usd = size_usd * input.target_collateral_ratio;
    let current_collateral = match side {
        Side::Long => input.long_collateral_usd,
        Side::Short => input.short_collateral_usd,
    };
    let projected_ratio = if projected_notional > 0.0 {
        (current_collateral + collateral_usd) / projected_notional
    } else {
        f64::INFINITY
    };
    if projected_ratio < input.min_collateral_ratio {
        return Decision::Blocked {
            reason: format!(
                "projected collateral ratio {} below min {}",
                js_to_fixed(projected_ratio, 3),
                input.min_collateral_ratio
            ),
        };
    }

    match side {
        Side::Long => {
            // The min() only absorbs float rounding at the fill boundary.
            let collateral_sol = (collateral_usd / price)
                .min((input.wallet_sol - input.wallet_reserve_sol).max(0.0));
            Decision::IncreaseLong {
                size_usd,
                collateral_tokens: collateral_sol,
                adjust_sol,
            }
        }
        Side::Short => Decision::IncreaseShort {
            size_usd,
            collateral_tokens: collateral_usd,
            adjust_sol: -adjust_sol,
        },
    }
}

// ---------------------------------------------------------------------------
// LP hedge-input transforms (ADR-019/021/022/023/025).
// ---------------------------------------------------------------------------

/// Clamp regime of the LP hedge input — `LpHedgeRegime`
/// (`hedgeController.ts:281`). Serialises as `below` / `in` / `above`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LpRegime {
    /// Out of range below: composition ≈ pure SOL → hedge the full bag.
    Below,
    /// In range → the midpoint approximation.
    In,
    /// Out of range above: pure USDC → zero delta.
    Above,
}

/// `computeLpHedgeDelta` result (`hedgeController.ts:283-310`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub(crate) struct LpHedgeDelta {
    pub delta_sol: f64,
    pub regime: LpRegime,
}

/// ADR-019 midpoint: the SOL-denominated half of the LP's total value
/// (`computeLpMidpointSol`, `hedgeController.ts:255-262`).
pub(crate) fn lp_midpoint_sol(lp_sol: f64, lp_usdc: f64, price: f64) -> f64 {
    if !(price > 0.0) {
        return lp_sol; // defensive: fall back to live
    }
    (lp_sol + lp_usdc / price) / 2.0
}

/// Hedge-input SOL delta a given clamp regime implies (`lpDeltaForRegime`,
/// `hedgeController.ts:317-328`).
pub(crate) fn lp_delta_for_regime(regime: LpRegime, lp_sol: f64, lp_usdc: f64, price: f64) -> f64 {
    match regime {
        LpRegime::Below => lp_sol,
        LpRegime::Above => 0.0,
        LpRegime::In => lp_midpoint_sol(lp_sol, lp_usdc, price),
    }
}

/// Clamp regime with hysteresis 98/90 (enter/exit below) and 2/10
/// (enter/exit above) on the SOL share of position value; `sticky` is the
/// previous regime (`hedgeController.ts:289-307`).
pub(crate) fn lp_hedge_regime(lp_sol: f64, lp_usdc: f64, price: f64, sticky: LpRegime) -> LpRegime {
    if !(price > 0.0) {
        return sticky;
    }
    let sol_value = lp_sol * price;
    let total = sol_value + lp_usdc;
    if total <= 0.0 {
        return LpRegime::In;
    }
    let share = sol_value / total;
    const ENTER_BELOW: f64 = 0.98;
    const EXIT_BELOW: f64 = 0.9;
    const ENTER_ABOVE: f64 = 0.02;
    const EXIT_ABOVE: f64 = 0.1;
    match sticky {
        LpRegime::Below => {
            if share >= EXIT_BELOW {
                LpRegime::Below
            } else if share <= ENTER_ABOVE {
                LpRegime::Above
            } else {
                LpRegime::In
            }
        }
        LpRegime::Above => {
            if share <= EXIT_ABOVE {
                LpRegime::Above
            } else if share >= ENTER_BELOW {
                LpRegime::Below
            } else {
                LpRegime::In
            }
        }
        LpRegime::In => {
            if share >= ENTER_BELOW {
                LpRegime::Below
            } else if share <= ENTER_ABOVE {
                LpRegime::Above
            } else {
                LpRegime::In
            }
        }
    }
}

/// ADR-021 true SOL delta of the LP for hedging (`computeLpHedgeDelta`,
/// `hedgeController.ts:283-310`): bad price → live amount with the sticky
/// regime; empty exposure → 0 / `In`; else the regime's delta.
pub(crate) fn lp_hedge_delta(
    lp_sol: f64,
    lp_usdc: f64,
    price: f64,
    sticky: LpRegime,
) -> LpHedgeDelta {
    if !(price > 0.0) {
        return LpHedgeDelta {
            delta_sol: lp_sol,
            regime: sticky,
        };
    }
    if lp_sol * price + lp_usdc <= 0.0 {
        return LpHedgeDelta {
            delta_sol: 0.0,
            regime: LpRegime::In,
        };
    }
    let regime = lp_hedge_regime(lp_sol, lp_usdc, price, sticky);
    LpHedgeDelta {
        delta_sol: lp_delta_for_regime(regime, lp_sol, lp_usdc, price),
        regime,
    }
}

/// ADR-022 per-side notional cap from the measured bag
/// (`computeAutoNotionalCapUsd`, `hedgeController.ts:343-354`):
/// `cap_bag_sol × price × cap_mult`, optionally ceilinged by
/// `absolute_cap_usd` (> 0); degenerate auto → the ceiling, else 0.
pub(crate) fn auto_notional_cap_usd(
    cap_bag_sol: f64,
    price: f64,
    cap_mult: f64,
    absolute_cap_usd: f64,
) -> f64 {
    let auto = cap_bag_sol * price * cap_mult;
    if !auto.is_finite() || auto <= 0.0 {
        return if absolute_cap_usd > 0.0 {
            absolute_cap_usd
        } else {
            0.0
        };
    }
    if absolute_cap_usd > 0.0 {
        auto.min(absolute_cap_usd)
    } else {
        auto
    }
}

/// ADR-025 dead-band from the LP size (`computeAutoBandSol`,
/// `hedgeController.ts:365-375`): `band_bins` bins' worth of LP delta,
/// `(lp_full_value_sol / bin_count) × band_bins`, floored at `floor_sol`;
/// degenerate inputs or `band_bins = 0` → the floor.
pub(crate) fn auto_band_sol(
    lp_full_value_sol: f64,
    bin_count: f64,
    band_bins: f64,
    floor_sol: f64,
) -> f64 {
    if !(band_bins > 0.0)
        || !(bin_count > 0.0)
        || !lp_full_value_sol.is_finite()
        || lp_full_value_sol <= 0.0
    {
        return floor_sol;
    }
    floor_sol.max((lp_full_value_sol / bin_count) * band_bins)
}

// ---------------------------------------------------------------------------
// JS number formatting (shared with `gates.rs`).
// ---------------------------------------------------------------------------

/// JavaScript `x.toFixed(digits)`: round the EXACT binary value half-up
/// (ties away from zero). Rust's `{:.N}` rounds exact ties half-to-even
/// (`0.125` → `"0.12"`; JS gives `"0.13"`), so this rounds a 40-extra-digit
/// exact expansion itself. `digits` ≤ 17. `|x| ≥ 1e21` falls back to
/// Display, as JS falls back to `ToString`.
pub(crate) fn js_to_fixed(x: f64, digits: usize) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    if x.abs() >= 1e21 {
        return format!("{x}");
    }
    let digits = digits.min(17);
    let sign = if x < 0.0 { "-" } else { "" };
    let expansion = format!("{:.*}", digits + 40, x.abs());
    let Some((int_part, frac_part)) = expansion.split_once('.') else {
        return format!("{sign}{expansion}");
    };
    let kept = format!("{int_part}{}", &frac_part[..digits]);
    let mut n: u128 = kept.parse().unwrap_or(0);
    if frac_part.as_bytes()[digits] >= b'5' {
        n += 1;
    }
    if digits == 0 {
        return format!("{sign}{n}");
    }
    let scale = 10u128.pow(digits as u32);
    format!("{sign}{}.{:0width$}", n / scale, n % scale, width = digits)
}

#[cfg(test)]
#[path = "hedge_tests.rs"]
mod tests;
