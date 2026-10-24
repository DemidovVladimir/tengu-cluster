//! `hedge_decide` / `lp_decide` knobs: every field required (no serde defaults, no hidden code defaults).

use super::*;

// ---------------------------------------------------------------------------

/// LP figure the hedge controller hedges (`HEDGE_LP_INPUT`, ADR-019).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LpInput {
    /// Current LP SOL amount.
    Live,
    /// SOL half of the LP value, with the ADR-021 out-of-range clamp.
    Midpoint,
}

impl LpInput {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            LpInput::Live => "live",
            LpInput::Midpoint => "midpoint",
        }
    }
}

/// `hedge_decide` knobs — field names = the `knobs` schema in
/// `tools/solana/defs.rs::hedge_knobs` (bot names in `env.ts:336-454`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HedgeKnobs {
    /// `HEDGE_TARGET_DELTA_SOL`.
    pub target_delta_sol: f64,
    /// `DELTA_THRESHOLD_SOL` — floor of the auto band.
    pub delta_threshold_sol: f64,
    /// `HEDGE_BAND_BINS` (0 = fixed band).
    pub band_bins: u32,
    /// `AUTO_TUNE_BIN_COUNT`.
    pub bin_count: u32,
    /// `HEDGE_NOTIONAL_CAP_MULT`.
    pub cap_mult: f64,
    /// `MAX_HEDGE_NOTIONAL_USD` (0 = auto cap only).
    pub max_notional_usd: f64,
    pub min_collateral_ratio: f64,
    /// `HEDGE_TARGET_COLLATERAL_RATIO`.
    pub target_collateral_ratio: f64,
    /// `HEDGE_CARRY_CAP_BPS` (0 = disabled).
    pub carry_cap_bps: f64,
    /// `HEDGE_COOLDOWN_MS`.
    pub cooldown_ms: u64,
    pub lp_input: LpInput,
    /// `HEDGE_INCLUDE_WALLET_SOL`.
    pub include_wallet_sol: bool,
    /// `MINIMUM_WALLET_BALANCE_SOL`.
    pub min_wallet_sol: f64,
    /// `RENT_RESERVE_SOL`.
    pub rent_reserve_sol: f64,
    pub max_divergence_bps: f64,
    pub max_snapshot_age_secs: u64,
    /// `TREND_CONFIRM_MS` (ADR-023), midpoint only: a computed clamp regime
    /// commits after the candidate persisted this long (storms bypass it).
    pub trend_confirm_ms: u64,
    /// BUG-011 grace (`NO_LP_HEDGE_GRACE_CYCLES` × cycle): with no LP
    /// position but one seen less than this long ago, hold the hedge
    /// (`None`) instead of trading on the no-LP read; 0 = off.
    pub no_lp_grace_ms: u64,
}

/// `lp_decide` knobs — field names = the `knobs` schema in
/// `tools/solana/defs.rs::lp_knobs`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LpKnobs {
    /// `AUTO_TUNE_IMBALANCE_THRESHOLD` (fraction, 0-1).
    pub imbalance_threshold: f64,
    /// `AUTO_TUNE_BIN_COUNT` (a new range is capped at 70 bins).
    pub bin_count: u32,
    /// `LP_VOL_PAUSE_PCT_5M` (0 = disabled).
    pub storm_pct_5m: f64,
    /// `TREND_CONFIRM_MS`.
    pub trend_confirm_ms: u64,
    /// `REENTRY_CONFIRM_MS` (0 = recenter in one step).
    pub reentry_confirm_ms: u64,
    /// `REENTRY_TOL_FRAC`.
    pub reentry_tol_frac: f64,
    pub max_divergence_bps: f64,
    pub max_snapshot_age_secs: u64,
    pub min_wallet_sol: f64,
    pub rent_reserve_sol: f64,
}

/// Range check of one knob; `None` bounds are open.
pub(super) fn check_knob(
    errs: &mut Vec<String>,
    name: &str,
    v: f64,
    min: Option<f64>,
    max: Option<f64>,
) {
    if !v.is_finite() {
        errs.push(format!("knobs.{name} must be finite"));
    } else if min.is_some_and(|m| v < m) || max.is_some_and(|m| v > m) {
        let range = match (min, max) {
            (Some(a), Some(b)) => format!("in [{a}, {b}]"),
            (Some(a), None) => format!(">= {a}"),
            (None, Some(b)) => format!("<= {b}"),
            (None, None) => String::new(),
        };
        errs.push(format!("knobs.{name} = {v} must be {range}"));
    }
}

pub(super) fn parse_knobs<T: serde::de::DeserializeOwned>(
    v: &Value,
    validate: fn(&T) -> Result<(), String>,
) -> Result<T, String> {
    let k: T = serde_json::from_value(v.clone()).map_err(|e| format!("knobs: {e}"))?;
    validate(&k)?;
    Ok(k)
}

impl HedgeKnobs {
    /// Deserialize (every field required, unknown fields rejected) and
    /// validate the `knobs` argument.
    pub(crate) fn parse(v: &Value) -> Result<Self, String> {
        parse_knobs(v, Self::validate)
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        let mut e = Vec::new();
        check_knob(
            &mut e,
            "target_delta_sol",
            self.target_delta_sol,
            None,
            None,
        );
        check_knob(
            &mut e,
            "delta_threshold_sol",
            self.delta_threshold_sol,
            Some(0.0),
            None,
        );
        check_knob(
            &mut e,
            "bin_count",
            f64::from(self.bin_count),
            Some(1.0),
            None,
        );
        check_knob(&mut e, "cap_mult", self.cap_mult, Some(0.0), None);
        check_knob(
            &mut e,
            "max_notional_usd",
            self.max_notional_usd,
            Some(0.0),
            None,
        );
        check_knob(
            &mut e,
            "min_collateral_ratio",
            self.min_collateral_ratio,
            Some(0.0),
            Some(1.0),
        );
        check_knob(
            &mut e,
            "target_collateral_ratio",
            self.target_collateral_ratio,
            Some(0.0),
            None,
        );
        check_knob(&mut e, "carry_cap_bps", self.carry_cap_bps, Some(0.0), None);
        check_knob(
            &mut e,
            "min_wallet_sol",
            self.min_wallet_sol,
            Some(0.0),
            None,
        );
        check_knob(
            &mut e,
            "rent_reserve_sol",
            self.rent_reserve_sol,
            Some(0.0),
            None,
        );
        check_knob(
            &mut e,
            "max_divergence_bps",
            self.max_divergence_bps,
            Some(0.0),
            None,
        );
        if self.target_collateral_ratio.is_finite()
            && self.min_collateral_ratio.is_finite()
            && (self.target_collateral_ratio <= 0.0
                || self.target_collateral_ratio < self.min_collateral_ratio)
        {
            e.push(format!(
                "knobs.target_collateral_ratio = {} must be > 0 and >= knobs.min_collateral_ratio = {}",
                self.target_collateral_ratio, self.min_collateral_ratio
            ));
        }
        if e.is_empty() {
            Ok(())
        } else {
            Err(e.join("; "))
        }
    }
}

impl LpKnobs {
    /// Deserialize (every field required, unknown fields rejected) and
    /// validate the `knobs` argument.
    pub(crate) fn parse(v: &Value) -> Result<Self, String> {
        parse_knobs(v, Self::validate)
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        let mut e = Vec::new();
        check_knob(
            &mut e,
            "imbalance_threshold",
            self.imbalance_threshold,
            Some(0.0),
            Some(1.0),
        );
        check_knob(
            &mut e,
            "bin_count",
            f64::from(self.bin_count),
            Some(1.0),
            None,
        );
        check_knob(&mut e, "storm_pct_5m", self.storm_pct_5m, Some(0.0), None);
        check_knob(
            &mut e,
            "reentry_tol_frac",
            self.reentry_tol_frac,
            Some(0.0),
            Some(1.0),
        );
        check_knob(
            &mut e,
            "max_divergence_bps",
            self.max_divergence_bps,
            Some(0.0),
            None,
        );
        check_knob(
            &mut e,
            "min_wallet_sol",
            self.min_wallet_sol,
            Some(0.0),
            None,
        );
        check_knob(
            &mut e,
            "rent_reserve_sol",
            self.rent_reserve_sol,
            Some(0.0),
            None,
        );
        if e.is_empty() {
            Ok(())
        } else {
            Err(e.join("; "))
        }
    }
}
