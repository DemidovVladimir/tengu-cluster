//! `hedge_decide/1`: [`decide_hedge`] — the gates, then `hedge::decide` unchanged.

use super::*;

// ---------------------------------------------------------------------------

/// Why a hedge action was blocked. `Oracle` … `CollateralRatio` come from
/// [`hedge::decide`] itself; the rest are tengu gates around it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Guard {
    InvalidRead,
    WalletSolZero,
    NotApplicable,
    PendingRequest,
    Divergence,
    StaleInput,
    Oracle,
    Carry,
    Headroom,
    Collateral,
    CollateralRatio,
    VenuePermission,
    /// A core block whose reason text is not one of the five known ones.
    Core,
}

impl Guard {
    fn as_str(self) -> &'static str {
        match self {
            Guard::InvalidRead => "invalid_read",
            Guard::WalletSolZero => "wallet_sol_zero",
            Guard::NotApplicable => "not_applicable",
            Guard::PendingRequest => "pending_request",
            Guard::Divergence => "divergence",
            Guard::StaleInput => "stale_input",
            Guard::Oracle => "oracle",
            Guard::Carry => "carry",
            Guard::Headroom => "headroom",
            Guard::Collateral => "collateral",
            Guard::CollateralRatio => "collateral_ratio",
            Guard::VenuePermission => "venue_permission",
            Guard::Core => "core",
        }
    }

    /// The guard behind a `hedge::decide` block (`hedgeController.ts:94-245`
    /// reason texts).
    pub(super) fn of_core_reason(reason: &str) -> Guard {
        if reason.starts_with("no oracle") {
            Guard::Oracle
        } else if reason.contains(" carry ") {
            Guard::Carry
        } else if reason.starts_with("projected collateral ratio") {
            Guard::CollateralRatio
        } else if reason.starts_with("projected ") && reason.contains(" notional ") {
            Guard::Headroom
        } else if reason.contains(" collateral ") {
            Guard::Collateral
        } else {
            Guard::Core
        }
    }
}

/// One hedge action — [`hedge::Decision`] plus the guard of a block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub(crate) enum HedgeAction {
    None {
        reason: String,
    },
    Blocked {
        reason: String,
        guard: Guard,
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

impl HedgeAction {
    /// The envelope of a core decision (numbers unchanged).
    pub(crate) fn from_core(d: &Decision) -> Self {
        match d.clone() {
            Decision::None { reason } => HedgeAction::None { reason },
            Decision::Blocked { reason } => HedgeAction::Blocked {
                guard: Guard::of_core_reason(&reason),
                reason,
            },
            Decision::DecreaseLong {
                size_usd,
                entire_position,
                withdraw_collateral_usd,
                adjust_sol,
            } => HedgeAction::DecreaseLong {
                size_usd,
                entire_position,
                withdraw_collateral_usd,
                adjust_sol,
            },
            Decision::DecreaseShort {
                size_usd,
                entire_position,
                withdraw_collateral_usd,
                adjust_sol,
            } => HedgeAction::DecreaseShort {
                size_usd,
                entire_position,
                withdraw_collateral_usd,
                adjust_sol,
            },
            Decision::IncreaseLong {
                size_usd,
                collateral_tokens,
                adjust_sol,
            } => HedgeAction::IncreaseLong {
                size_usd,
                collateral_tokens,
                adjust_sol,
            },
            Decision::IncreaseShort {
                size_usd,
                collateral_tokens,
                adjust_sol,
            } => HedgeAction::IncreaseShort {
                size_usd,
                collateral_tokens,
                adjust_sol,
            },
        }
    }

    pub(crate) fn name(&self) -> &'static str {
        match self {
            HedgeAction::None { .. } => "none",
            HedgeAction::Blocked { .. } => "blocked",
            HedgeAction::DecreaseLong { .. } => "decrease_long",
            HedgeAction::DecreaseShort { .. } => "decrease_short",
            HedgeAction::IncreaseLong { .. } => "increase_long",
            HedgeAction::IncreaseShort { .. } => "increase_short",
        }
    }

    pub(crate) fn guard(&self) -> Option<Guard> {
        match self {
            HedgeAction::Blocked { guard, .. } => Some(*guard),
            _ => None,
        }
    }

    pub(crate) fn size_usd(&self) -> Option<f64> {
        match self {
            HedgeAction::DecreaseLong { size_usd, .. }
            | HedgeAction::DecreaseShort { size_usd, .. }
            | HedgeAction::IncreaseLong { size_usd, .. }
            | HedgeAction::IncreaseShort { size_usd, .. } => Some(*size_usd),
            _ => None,
        }
    }

    pub(crate) fn adjust_sol(&self) -> Option<f64> {
        match self {
            HedgeAction::DecreaseLong { adjust_sol, .. }
            | HedgeAction::DecreaseShort { adjust_sol, .. }
            | HedgeAction::IncreaseLong { adjust_sol, .. }
            | HedgeAction::IncreaseShort { adjust_sol, .. } => Some(*adjust_sol),
            _ => None,
        }
    }

    fn is_increase(&self) -> bool {
        matches!(
            self,
            HedgeAction::IncreaseLong { .. } | HedgeAction::IncreaseShort { .. }
        )
    }

    fn is_decrease(&self) -> bool {
        matches!(
            self,
            HedgeAction::DecreaseLong { .. } | HedgeAction::DecreaseShort { .. }
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BandSource {
    /// `band_bins` bins' worth of LP delta (ADR-025).
    Auto,
    /// `delta_threshold_sol`.
    Floor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CapSource {
    /// `cap_mult × bag × price` (ADR-022).
    Auto,
    /// `max_notional_usd` ceiling.
    Absolute,
    /// Nothing to cap from: 0.
    Zero,
}

/// What the controller saw (`DeltaView` + the ADR-019/021/022/025
/// transforms, `jupiterPerpsEngine.ts:840-880`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct HedgeView {
    pub price_usd: f64,
    pub lp_input: LpInput,
    /// Live LP base (SOL) amount.
    pub lp_delta_live: f64,
    pub lp_quote: f64,
    /// LP delta after the `lp_input` transform.
    pub lp_delta_used: f64,
    /// Committed clamp regime (after confirmation).
    pub regime: LpRegime,
    /// Regime this read computes (midpoint only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub computed_regime: Option<LpRegime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_regime: Option<PendingRegime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regime_outcome: Option<RegimeOutcome>,
    /// The LP signals the clamp confirm read (midpoint only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regime_signals: Option<RegimeSignals>,
    pub idle_wallet_sol: f64,
    pub perp_long_sol: f64,
    pub perp_short_sol: f64,
    pub net_delta_sol: f64,
    pub target_delta_sol: f64,
    pub error_sol: f64,
    pub band_sol: f64,
    pub band_source: BandSource,
    /// Unclamped LP value in SOL (sizes the cap and the band).
    pub lp_full_value_sol: f64,
    pub cap_bag_sol: f64,
    pub max_notional_usd: f64,
    pub cap_source: CapSource,
    pub out_of_band: bool,
    /// Cooldown left after the last live mutation; `None` when none runs.
    pub cooldown_remaining_ms: Option<i64>,
}

/// Guard arithmetic around the action.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct GuardTrace {
    /// Cap − current notional of the side that would grow.
    pub headroom_usd: Option<f64>,
    /// Collateral the growing side could post (USDC for a short, SOL above
    /// reserves × price for a long).
    pub available_collateral_usd: Option<f64>,
    /// Collateral / notional after an increase.
    pub projected_ratio: Option<f64>,
    /// `headroom` (BUG-012) / `collateral` (BUG-013) fills that shrank an
    /// increase.
    pub clamped_by: Vec<String>,
    /// Critical fields that failed to read.
    pub invalid_fields: Vec<String>,
}

/// `hedge_decide/1:<wallet>:<pool>` (never cached).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct HedgeDecision {
    pub wallet: String,
    pub pool: String,
    /// The `lp_snapshot` row decided on; `None` = no row.
    pub snapshot: Option<ObsMeta>,
    pub snapshot_age_ms: Option<u64>,
    pub knobs: HedgeKnobs,
    /// `None` when a gate fired before the view could be computed.
    pub view: Option<HedgeView>,
    /// The exact [`hedge::decide`] input (replayable).
    pub input: Option<HedgeInput>,
    pub action: HedgeAction,
    pub trace: GuardTrace,
    /// The `jup_perps_order` arguments the action maps to
    /// ([`HedgeDecision::perps_order`]); `None` = no order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<PerpsOrder>,
}

impl HedgeDecision {
    /// No usable `lp_snapshot` row: `Blocked{StaleInput}`.
    pub(crate) fn without_snapshot(
        wallet: &str,
        pool: &str,
        knobs: &HedgeKnobs,
        why: &str,
    ) -> Self {
        HedgeDecision {
            wallet: wallet.to_string(),
            pool: pool.to_string(),
            snapshot: None,
            snapshot_age_ms: None,
            knobs: knobs.clone(),
            view: None,
            input: None,
            action: HedgeAction::Blocked {
                reason: why.to_string(),
                guard: Guard::StaleInput,
            },
            trace: GuardTrace::default(),
            order: None,
        }
    }
}

/// Why the position read is not whole — both decisions gate on it (BUG-023:
/// a position that failed to read never becomes "no position" / exposure
/// 0): discovery failed, exposure unread, a position's bins incomplete, a
/// position-level read error (a discovered or requested key that did not
/// decode, is not the wallet's PositionV2 in the pool, or lacks bin
/// arrays), or discovery's count ≠ the positions valued.
pub(super) fn position_invalid_fields(s: &LpSnapshot) -> Vec<&'static str> {
    let mut v = Vec::new();
    let discovered = match &s.discovery {
        Discovery::Found { count, .. } => Some(*count as usize),
        Discovery::Empty { .. } => Some(0),
        Discovery::Error { .. } => None,
    };
    if discovered.is_none() || s.errors.iter().any(|e| e.field == "discovery") {
        v.push("discovery");
    }
    if s.exposure.value().is_none() {
        v.push("exposure");
    }
    if s.positions.iter().any(|p| !p.complete) {
        v.push("positions.complete");
    }
    if s.errors.iter().any(|e| e.field == "positions") {
        v.push("positions");
    }
    if discovered.is_some_and(|n| n != s.positions.len()) {
        v.push("discovery.count");
    }
    v
}

/// Critical fields for the hedge; a non-empty list blocks `InvalidRead`.
pub(super) fn hedge_invalid_fields(s: &LpSnapshot) -> Vec<String> {
    let mut v = position_invalid_fields(s);
    if s.wallet_balances.native_sol.value().is_none() {
        v.push("wallet.native_sol");
    }
    if s.hedge.applicable {
        if s.wallet_balances.quote.value().is_none() {
            v.push("wallet.quote");
        }
        if s.oracle.usd.is_none() {
            v.push("oracle.usd");
        }
        let h = &s.hedge;
        if h.long.is_error() {
            v.push("hedge.long");
        }
        if h.short.is_error() {
            v.push("hedge.short");
        }
        if h.sol_custody.value().is_none() {
            v.push("hedge.sol_custody");
        }
        if h.usdc_custody.value().is_none() {
            v.push("hedge.usdc_custody");
        }
        match &h.pending_request {
            Some(Field::Error { .. }) => v.push("hedge.pending_request"),
            Some(Field::Ok { value })
                if value.exists
                    && !value.executed
                    && h.max_request_execution_sec.value().is_none() =>
            {
                v.push("hedge.max_request_execution_sec")
            }
            _ => {}
        }
    }
    v.into_iter().map(String::from).collect()
}

/// The stale-input reason, if the snapshot or its price row is too old at
/// decision time.
pub(super) fn stale_reason(
    snap: &LpSnapshot,
    snap_age_ms: u64,
    max_age_secs: u64,
) -> Option<String> {
    let max_ms = max_age_secs.saturating_mul(1000);
    if snap_age_ms > max_ms {
        return Some(format!(
            "lp_snapshot row is {:.1} s old > max_snapshot_age_secs {max_age_secs}",
            snap_age_ms as f64 / 1000.0
        ));
    }
    if snap.oracle.needed {
        if let Some(a) = snap.oracle.age_ms {
            let age = a.saturating_add(snap_age_ms);
            if age > max_ms {
                return Some(format!(
                    "{} row is {:.1} s old > max_snapshot_age_secs {max_age_secs}",
                    snap.oracle.key,
                    age as f64 / 1000.0
                ));
            }
        }
    }
    None
}

pub(super) fn divergence_reason(snap: &LpSnapshot, max_bps: f64) -> Option<String> {
    let bps = snap.pool_vs_oracle_bps?;
    (bps.abs() > max_bps)
        .then(|| format!("pool price vs oracle {bps:+.1} bps exceeds max_divergence_bps {max_bps}"))
}

/// One LP cycle's inputs to its storm latch and imbalance timer — the
/// bot updates both before the hedge in the same cycle
/// (`recordPriceSample` `autoTuneOrchestrator.ts:515`, `imbalanceSince`
/// `:771-786`, then `maybeRebalanceHedge` `:928`). `lp_decide`'s knob
/// values; `price_samples` = the `price_oracle` row's samples.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LpCycle<'a> {
    /// `LpKnobs::storm_pct_5m` (`LP_VOL_PAUSE_PCT_5M`).
    pub storm_pct_5m: f64,
    /// `LpKnobs::imbalance_threshold` (`AUTO_TUNE_IMBALANCE_THRESHOLD`).
    pub imbalance_threshold: f64,
    pub price_samples: &'a [PriceSample],
}

impl<'a> LpCycle<'a> {
    pub(crate) fn new(knobs: &LpKnobs, price_samples: &'a [PriceSample]) -> Self {
        LpCycle {
            storm_pct_5m: knobs.storm_pct_5m,
            imbalance_threshold: knobs.imbalance_threshold,
            price_samples,
        }
    }

    /// Storm latch on the cycle price (`prev_active` = the latch from the
    /// previous cycle). The samples are SOL/USD: ignored for a pool whose
    /// quote is not USDC (its cycle price is the pool's own).
    pub(super) fn storm(&self, snap: &LpSnapshot, prev_active: bool, now_ms: i64) -> StormState {
        let out = gates::storm_update(&StormInput {
            now_ms,
            price: snap.cycle_price().unwrap_or(f64::NAN),
            samples: if snap.oracle.needed {
                self.price_samples.to_vec()
            } else {
                Vec::new()
            },
            threshold_pct: self.storm_pct_5m,
            active: prev_active,
        });
        StormState {
            move_5m_pct: out.move_5m_pct,
            active: out.active,
            threshold_pct: self.storm_pct_5m,
        }
    }

    /// The imbalance timer runs after this read: the one position is out of
    /// its composition threshold. No position resets it; with several,
    /// `lp_decide` pauses, so no recenter owns the signal.
    fn imbalance_running(&self, snap: &LpSnapshot) -> bool {
        match snap.positions.as_slice() {
            [p] => {
                position_health(p, snap.active_id, snap.pool_price, self.imbalance_threshold)
                    .is_imbalanced
            }
            _ => false,
        }
    }
}

/// The LP signals the midpoint clamp confirm reads (ADR-023 storm bypass,
/// ADR-025 freeze).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RegimeSignals {
    pub storm_active: bool,
    pub imbalance_pending: bool,
    /// `true` = computed from this snapshot + price samples ([`LpCycle`]);
    /// `false` = `lp_state`'s flags, as the last ungated `lp_decide` left
    /// them (possibly an earlier event).
    pub this_cycle: bool,
}

impl RegimeSignals {
    fn of(
        cycle: Option<&LpCycle<'_>>,
        snap: &LpSnapshot,
        state: &LpControllerState,
        now_ms: i64,
    ) -> Self {
        match cycle {
            Some(c) => RegimeSignals {
                storm_active: c.storm(snap, state.storm_active, now_ms).active,
                imbalance_pending: c.imbalance_running(snap),
                this_cycle: true,
            },
            None => RegimeSignals {
                storm_active: state.storm_active,
                imbalance_pending: state.imbalance_since_ms.is_some(),
                this_cycle: false,
            },
        }
    }
}

/// View + exact core input (`jupiterPerpsEngine.ts:840-913`,
/// `autoTuneOrchestrator.ts:1104-1180`). Callers guarantee the critical
/// fields are readable (`hedge_invalid_fields` is empty).
pub(super) fn hedge_view(
    snap: &LpSnapshot,
    knobs: &HedgeKnobs,
    state: &LpControllerState,
    signals: RegimeSignals,
    price: f64,
    now_ms: i64,
) -> (HedgeView, HedgeInput) {
    let (live, quote) = snap
        .exposure
        .value()
        .map_or((0.0, 0.0), |e| (e.base, e.quote));
    let (used, regime, computed, pending, outcome, regime_signals) = match knobs.lp_input {
        LpInput::Live => (
            live,
            state.committed_regime,
            None,
            state.pending_regime,
            None,
            None,
        ),
        LpInput::Midpoint => {
            let c = hedge::lp_hedge_delta(live, quote, price, state.committed_regime);
            let rc = gates::regime_confirm(&RegimeConfirmInput {
                now_ms,
                committed: state.committed_regime,
                computed: c.regime,
                pending: state.pending_regime,
                confirm_ms: i64::try_from(knobs.trend_confirm_ms).unwrap_or(i64::MAX),
                storm_active: signals.storm_active,
                imbalance_pending: signals.imbalance_pending,
                last_rebalance_failed: state.last_rebalance_failed_at_ms.is_some(),
            });
            let used = hedge::lp_delta_for_regime(rc.committed, live, quote, price);
            (
                used,
                rc.committed,
                Some(c.regime),
                rc.pending,
                Some(rc.outcome),
                Some(signals),
            )
        }
    };

    let native = snap
        .wallet_balances
        .native_sol
        .value()
        .copied()
        .unwrap_or(0.0);
    let reserve = knobs.min_wallet_sol + knobs.rent_reserve_sol;
    let idle = if knobs.include_wallet_sol {
        (native - reserve).max(0.0)
    } else {
        0.0
    };
    let lp_full_value_sol = if price > 0.0 {
        live + quote / price
    } else {
        live
    };
    let cap_bag_sol = idle + lp_full_value_sol + knobs.target_delta_sol.abs();
    let max_notional =
        hedge::auto_notional_cap_usd(cap_bag_sol, price, knobs.cap_mult, knobs.max_notional_usd);
    let auto_cap = cap_bag_sol * price * knobs.cap_mult;
    let cap_source = if max_notional <= 0.0 {
        CapSource::Zero
    } else if !(auto_cap.is_finite() && auto_cap > 0.0)
        || (knobs.max_notional_usd > 0.0 && knobs.max_notional_usd < auto_cap)
    {
        CapSource::Absolute
    } else {
        CapSource::Auto
    };
    let band = hedge::auto_band_sol(
        lp_full_value_sol,
        f64::from(knobs.bin_count),
        f64::from(knobs.band_bins),
        knobs.delta_threshold_sol,
    );
    let band_source = if band > knobs.delta_threshold_sol {
        BandSource::Auto
    } else {
        BandSource::Floor
    };

    let h = &snap.hedge;
    let (long, short) = (h.long.value(), h.short.value());
    let long_sol = long.map_or(0.0, |s| s.base_sol);
    let short_sol = short.map_or(0.0, |s| s.base_sol);
    // Carry COST, positive = pays; an open side's own rate, else the
    // collateral custody's borrow APR (`jupiterPerpsEngine.ts:894-897`).
    let carry = |side: Option<&PerpSide>, custody: &Field<CustodyRates>| {
        side.map(|s| s.carry_cost_bps)
            .or_else(|| custody.value().map(|c| c.borrow_apr_pct * 100.0))
            .unwrap_or(0.0)
            .max(0.0)
    };
    let last_action_at_ms = state
        .last_hedge_action
        .as_ref()
        .filter(|a| a.live)
        .map(|a| a.at_ms);
    let cooldown_ms = i64::try_from(knobs.cooldown_ms).unwrap_or(i64::MAX);
    let lp_sol = used + idle;
    let input = HedgeInput {
        lp_sol,
        long_sol,
        short_sol,
        long_notional_usd: long.map_or(0.0, |s| s.notional_usd),
        short_notional_usd: short.map_or(0.0, |s| s.notional_usd),
        long_collateral_usd: long.map_or(0.0, |s| s.collateral_usd),
        short_collateral_usd: short.map_or(0.0, |s| s.collateral_usd),
        carry_cost_bps_long: carry(long, &h.sol_custody),
        carry_cost_bps_short: carry(short, &h.usdc_custody),
        oracle_price_usd: Some(price),
        wallet_sol: native,
        wallet_reserve_sol: reserve,
        wallet_usdc: snap.wallet_balances.quote.value().map_or(0.0, |a| a.ui),
        target_delta_sol: knobs.target_delta_sol,
        band_sol: band,
        carry_cap_bps: knobs.carry_cap_bps,
        max_hedge_notional_usd: max_notional,
        min_collateral_ratio: knobs.min_collateral_ratio,
        target_collateral_ratio: knobs.target_collateral_ratio,
        now_ms,
        last_action_at_ms,
        cooldown_ms,
    };
    let net = lp_sol + long_sol - short_sol;
    let error = net - knobs.target_delta_sol;
    let view = HedgeView {
        price_usd: price,
        lp_input: knobs.lp_input,
        lp_delta_live: live,
        lp_quote: quote,
        lp_delta_used: used,
        regime,
        computed_regime: computed,
        pending_regime: pending,
        regime_outcome: outcome,
        regime_signals,
        idle_wallet_sol: idle,
        perp_long_sol: long_sol,
        perp_short_sol: short_sol,
        net_delta_sol: net,
        target_delta_sol: knobs.target_delta_sol,
        error_sol: error,
        band_sol: band,
        band_source,
        lp_full_value_sol,
        cap_bag_sol,
        max_notional_usd: max_notional,
        cap_source,
        out_of_band: error.abs() > band,
        cooldown_remaining_ms: last_action_at_ms
            .filter(|_| cooldown_ms > 0)
            .map(|at| (cooldown_ms - (now_ms - at)).max(0))
            .filter(|r| *r > 0),
    };
    (view, input)
}

/// Headroom / collateral arithmetic of the side that would grow, and the
/// fills (BUG-012 / BUG-013) that shrank an increase.
pub(super) fn guard_trace(input: &HedgeInput, action: &HedgeAction) -> GuardTrace {
    let price = input.oracle_price_usd.unwrap_or(f64::NAN);
    let error = input.lp_sol + input.long_sol - input.short_sol - input.target_delta_sol;
    let grow_short = error > 0.0;
    let (notional, collateral, available) = if grow_short {
        (
            input.short_notional_usd,
            input.short_collateral_usd,
            input.wallet_usdc.max(0.0),
        )
    } else {
        (
            input.long_notional_usd,
            input.long_collateral_usd,
            ((input.wallet_sol - input.wallet_reserve_sol) * price).max(0.0),
        )
    };
    let headroom = input.max_hedge_notional_usd - notional;
    let mut t = GuardTrace {
        headroom_usd: finite(headroom),
        available_collateral_usd: finite(available),
        ..GuardTrace::default()
    };
    if let (true, Some(size)) = (action.is_increase(), action.size_usd()) {
        let projected = notional + size;
        t.projected_ratio = (projected > 0.0)
            .then(|| (collateral + size * input.target_collateral_ratio) / projected)
            .and_then(finite);
        let desired = error.abs() * price;
        let tol = 1e-9 * desired.abs().max(1.0);
        if desired > headroom + tol {
            t.clamped_by.push("headroom".into());
        }
        if size + tol < desired.min(headroom) {
            t.clamped_by.push("collateral".into());
        }
    }
    t
}

/// [`decide_hedge_in_cycle`] without an [`LpCycle`]: the midpoint clamp
/// confirm reads `lp_state`'s storm / imbalance flags — the last ungated
/// `lp_decide`'s, possibly an earlier event (`view.regime_signals.this_cycle
/// = false`). Prefer the cycle form.
#[cfg(test)]
pub(crate) fn decide_hedge(
    snap: &LpSnapshot,
    snap_meta: &ObsMeta,
    snap_age_ms: u64,
    knobs: &HedgeKnobs,
    state: &LpControllerState,
    now_ms: i64,
) -> (HedgeDecision, LpControllerState) {
    decide_hedge_in_cycle(snap, snap_meta, snap_age_ms, knobs, state, None, now_ms)
}

/// `hedge_decide`: tengu validity gates, then the verbatim
/// [`hedge::decide`], then the venue-permission check. With `cycle`, the
/// midpoint clamp confirm sees this snapshot's storm latch and imbalance
/// timer, as the bot's hedge does (`hedge_decide` then needs no earlier
/// `lp_decide`). Returns the decision and the next controller state (only
/// the position observation when a gate fired before the core; the caller
/// persists it only with `commit = true`). `order` = the action as
/// `jup_perps_order` arguments ([`HedgeDecision::perps_order`]).
pub(crate) fn decide_hedge_in_cycle(
    snap: &LpSnapshot,
    snap_meta: &ObsMeta,
    snap_age_ms: u64,
    knobs: &HedgeKnobs,
    state: &LpControllerState,
    cycle: Option<&LpCycle<'_>>,
    now_ms: i64,
) -> (HedgeDecision, LpControllerState) {
    let (mut d, next) =
        decide_hedge_core(snap, snap_meta, snap_age_ms, knobs, state, cycle, now_ms);
    d.order = d.perps_order();
    (d, next)
}

/// `jup_perps_order` `action` argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PerpsOrderKind {
    Increase,
    Decrease,
    Close,
}

/// A hedge action as `jup_perps_order` arguments (`data.order` of
/// `hedge_decide/1`), so a decision loop binds each field one to one and
/// no translation lives in TOML. `slippage_bps`, `wallet`, `pool` and
/// `mode` stay with the caller.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PerpsOrder {
    pub side: Side,
    pub action: PerpsOrderKind,
    /// USD notional added (increase) or removed (decrease); the decision's
    /// size on a close (the tool ignores it).
    pub size_usd: f64,
    /// Increase: collateral in the side's token (SOL for a long, USDC for a
    /// short). Decrease / close: USD of collateral withdrawn.
    pub collateral: f64,
    /// Cap on the side's post-order size. Increase: the decision's cap
    /// (`view.max_notional_usd`). Decrease / close: at least the side's
    /// current notional — the tool checks the cap on increases only, and a
    /// zero auto cap (LP gone) must not block unwinding the hedge.
    pub max_notional_usd: f64,
}

impl HedgeDecision {
    /// The order this decision's action maps to: side and kind from the
    /// action, `close` for an entire-position decrease. `None` = no order:
    /// `none` / `blocked`, no view, or no positive, finite cap.
    pub(crate) fn perps_order(&self) -> Option<PerpsOrder> {
        let view = self.view.as_ref()?;
        let current = |side: Side| {
            self.input.as_ref().map_or(0.0, |i| match side {
                Side::Long => i.long_notional_usd,
                Side::Short => i.short_notional_usd,
            })
        };
        let (side, action, size_usd, collateral) = match &self.action {
            HedgeAction::None { .. } | HedgeAction::Blocked { .. } => return None,
            HedgeAction::IncreaseLong {
                size_usd,
                collateral_tokens,
                ..
            } => (
                Side::Long,
                PerpsOrderKind::Increase,
                *size_usd,
                *collateral_tokens,
            ),
            HedgeAction::IncreaseShort {
                size_usd,
                collateral_tokens,
                ..
            } => (
                Side::Short,
                PerpsOrderKind::Increase,
                *size_usd,
                *collateral_tokens,
            ),
            HedgeAction::DecreaseLong {
                size_usd,
                entire_position,
                withdraw_collateral_usd,
                ..
            } => (
                Side::Long,
                if *entire_position {
                    PerpsOrderKind::Close
                } else {
                    PerpsOrderKind::Decrease
                },
                *size_usd,
                *withdraw_collateral_usd,
            ),
            HedgeAction::DecreaseShort {
                size_usd,
                entire_position,
                withdraw_collateral_usd,
                ..
            } => (
                Side::Short,
                if *entire_position {
                    PerpsOrderKind::Close
                } else {
                    PerpsOrderKind::Decrease
                },
                *size_usd,
                *withdraw_collateral_usd,
            ),
        };
        let cap = match action {
            PerpsOrderKind::Increase => view.max_notional_usd,
            _ => view.max_notional_usd.max(current(side)),
        };
        let finite = [size_usd, collateral, cap].iter().all(|x| x.is_finite());
        (finite && cap > 0.0).then_some(PerpsOrder {
            side,
            action,
            size_usd,
            collateral,
            max_notional_usd: cap,
        })
    }
}

pub(super) fn decide_hedge_core(
    snap: &LpSnapshot,
    snap_meta: &ObsMeta,
    snap_age_ms: u64,
    knobs: &HedgeKnobs,
    state: &LpControllerState,
    cycle: Option<&LpCycle<'_>>,
    now_ms: i64,
) -> (HedgeDecision, LpControllerState) {
    let mut out = HedgeDecision {
        wallet: snap.wallet.clone(),
        pool: snap.pool.clone(),
        snapshot: Some(snap_meta.clone()),
        snapshot_age_ms: Some(snap_age_ms),
        knobs: knobs.clone(),
        view: None,
        input: None,
        action: HedgeAction::None {
            reason: String::new(),
        },
        trace: GuardTrace::default(),
        order: None,
    };
    let blocked = |guard: Guard, reason: String| HedgeAction::Blocked { reason, guard };
    // Seeing (or not seeing) a position is an observation: recorded before
    // any gate, whenever the position read is whole.
    let observed = state.observed(snap, now_ms, snap_age_ms);

    // 1-3: gates that need no view.
    let invalid = hedge_invalid_fields(snap);
    if !invalid.is_empty() {
        out.action = blocked(
            Guard::InvalidRead,
            format!("critical reads failed: {}", invalid.join(", ")),
        );
        out.trace.invalid_fields = invalid;
        return (out, observed);
    }
    if knobs.include_wallet_sol && snap.wallet_balances.native_sol.value() == Some(&0.0) {
        out.action = blocked(
            Guard::WalletSolZero,
            "wallet native SOL reads exactly 0 with include_wallet_sol (BUG-023: never hedge a read that may have failed)".into(),
        );
        return (out, observed);
    }
    if !snap.hedge.applicable {
        out.action = blocked(
            Guard::NotApplicable,
            format!(
                "the SOL perps hedge needs a native-SOL base and a USDC quote; pair is {} / {}",
                snap.pair.base_mint, snap.pair.quote_mint
            ),
        );
        return (out, observed);
    }

    let price = snap.oracle.usd.unwrap_or(f64::NAN);
    let signals = RegimeSignals::of(cycle, snap, state, now_ms);
    let (view, input) = hedge_view(snap, knobs, &observed, signals, price, now_ms);
    out.view = Some(view.clone());
    out.input = Some(input.clone());

    // 4-6: gates on a computed view.
    let pre = if let Some(Field::Ok { value: r }) = &snap.hedge.pending_request {
        let max = snap.hedge.max_request_execution_sec.value().copied();
        match (r.exists && !r.executed, max) {
            (true, Some(max)) if r.age_secs.unwrap_or(0) < max + PENDING_REQUEST_GRACE_SECS => {
                Some((
                    Guard::PendingRequest,
                    format!(
                        "keeper request {} not executed yet ({} s old < {} s max execution + {} s)",
                        r.position_request,
                        r.age_secs.unwrap_or(0),
                        max,
                        PENDING_REQUEST_GRACE_SECS
                    ),
                ))
            }
            _ => None,
        }
    } else {
        None
    };
    let pre = pre
        .or_else(|| {
            divergence_reason(snap, knobs.max_divergence_bps).map(|r| (Guard::Divergence, r))
        })
        .or_else(|| {
            stale_reason(snap, snap_age_ms, knobs.max_snapshot_age_secs)
                .map(|r| (Guard::StaleInput, r))
        });
    if let Some((guard, reason)) = pre {
        out.action = blocked(guard, reason);
        out.trace = guard_trace(&input, &out.action);
        return (out, observed);
    }
    if let Some(reason) = no_lp_grace_reason(snap, knobs, &observed, now_ms) {
        out.action = HedgeAction::None { reason };
        out.trace = guard_trace(&input, &out.action);
        return (out, observed);
    }

    // Core, unchanged.
    let mut action = HedgeAction::from_core(&hedge::decide(&input));
    if let Some(c) = snap.hedge.sol_custody.value() {
        let denied = (action.is_increase() && !c.allow_increase)
            || (action.is_decrease() && !c.allow_decrease);
        if denied {
            action = blocked(
                Guard::VenuePermission,
                format!(
                    "Jupiter custody {} does not allow {} now",
                    c.custody,
                    action.name()
                ),
            );
        }
    }
    out.trace = guard_trace(&input, &action);
    out.action = action;

    let mut next = observed;
    next.committed_regime = view.regime;
    next.pending_regime = view.pending_regime;
    next.updated_at_ms = now_ms;
    (out, next)
}

/// BUG-011 (`autoTuneOrchestrator.ts:692-716`): a recenter that closed the
/// old position but failed to open the new one reads as "no LP" while the
/// LP's SOL sits in the wallet — trading on that read would unwind the
/// protective hedge mid-move. Hold (`None`) until the no-LP state has
/// lasted `no_lp_grace_ms` since the first no-LP read (`state` = after this
/// read's observation: the clock starts at a fresh state's first no-LP
/// read, like the bot's counter from 0 — fail-closed). A re-entry wait
/// (A15) is a deliberate no-LP state: the hedge keeps running there.
pub(super) fn no_lp_grace_reason(
    snap: &LpSnapshot,
    knobs: &HedgeKnobs,
    observed: &LpControllerState,
    now_ms: i64,
) -> Option<String> {
    if knobs.no_lp_grace_ms == 0 || !snap.positions.is_empty() || observed.reentry.is_some() {
        return None;
    }
    // `None` only when this read predates the last seen position: no no-LP
    // run has been observed yet — hold.
    let since = observed.no_lp_since_ms.unwrap_or(now_ms);
    let lasted = now_ms.saturating_sub(since);
    let grace = i64::try_from(knobs.no_lp_grace_ms).unwrap_or(i64::MAX);
    (lasted < grace).then(|| {
        format!(
            "no-LP grace: no LP position for {} ms since the first no-LP read (< no_lp_grace_ms {}); \
             the hedge holds while the LP funds may be mid-rebalance (BUG-011)",
            lasted.max(0),
            knobs.no_lp_grace_ms
        )
    })
}

impl Observed for HedgeDecision {
    const SCHEMA: &'static str = "hedge_decide/1";

    fn subject(&self) -> String {
        format!("{}:{}", self.wallet, self.pool)
    }

    /// `hedge_decide <wallet> <pool> action=… [size_usd=…|guard=…]`
    fn headline(&self) -> String {
        let detail = match &self.action {
            HedgeAction::Blocked { guard, .. } => format!(" guard={}", guard.as_str()),
            HedgeAction::None { .. } => self
                .view
                .as_ref()
                .map(|v| {
                    format!(
                        " error_sol={} band_sol={}",
                        fmt_sig(v.error_sol, 4),
                        fmt_sig(v.band_sol, 4)
                    )
                })
                .unwrap_or_default(),
            a => format!(
                " size_usd={} adjust_sol={}",
                fmt_sig(a.size_usd().unwrap_or(f64::NAN), 7),
                fmt_sig(a.adjust_sol().unwrap_or(f64::NAN), 6)
            ),
        };
        format!(
            "hedge_decide {} {} action={}{detail}",
            self.wallet,
            self.pool,
            self.action.name()
        )
    }

    fn features(&self) -> Features {
        let mut f = Features::new();
        let a = &self.action;
        set_str(&mut f, "action", Some(a.name()));
        set_bool(&mut f, "blocked", Some(a.guard().is_some()));
        set_str(&mut f, "guard", a.guard().map(Guard::as_str));
        set_num(&mut f, "size_usd", a.size_usd());
        set_num(&mut f, "adjust_sol", a.adjust_sol());
        match a {
            HedgeAction::DecreaseLong {
                entire_position,
                withdraw_collateral_usd,
                ..
            }
            | HedgeAction::DecreaseShort {
                entire_position,
                withdraw_collateral_usd,
                ..
            } => {
                set_bool(&mut f, "entire_position", Some(*entire_position));
                set_num(
                    &mut f,
                    "withdraw_collateral_usd",
                    Some(*withdraw_collateral_usd),
                );
            }
            HedgeAction::IncreaseLong {
                collateral_tokens, ..
            }
            | HedgeAction::IncreaseShort {
                collateral_tokens, ..
            } => set_num(&mut f, "collateral_tokens", Some(*collateral_tokens)),
            _ => {}
        }
        if let Some(v) = &self.view {
            set_num(&mut f, "price_usd", Some(v.price_usd));
            set_str(&mut f, "lp_input", Some(v.lp_input.as_str()));
            set_num(&mut f, "lp_delta_live", Some(v.lp_delta_live));
            set_num(&mut f, "lp_delta_used", Some(v.lp_delta_used));
            set_str(&mut f, "regime", Some(regime_str(v.regime)));
            set_str(
                &mut f,
                "pending_regime",
                v.pending_regime.map(|p| regime_str(p.regime)),
            );
            set_num(&mut f, "idle_wallet_sol", Some(v.idle_wallet_sol));
            set_num(&mut f, "perp_long_sol", Some(v.perp_long_sol));
            set_num(&mut f, "perp_short_sol", Some(v.perp_short_sol));
            set_num(&mut f, "net_delta_sol", Some(v.net_delta_sol));
            set_num(&mut f, "target_delta_sol", Some(v.target_delta_sol));
            set_num(&mut f, "error_sol", Some(v.error_sol));
            set_num(&mut f, "band_sol", Some(v.band_sol));
            set_bool(&mut f, "out_of_band", Some(v.out_of_band));
            set_num(&mut f, "cap_usd", Some(v.max_notional_usd));
            set_int(&mut f, "cooldown_remaining_ms", v.cooldown_remaining_ms);
        }
        set_num(&mut f, "headroom_usd", self.trace.headroom_usd);
        set_num(&mut f, "projected_ratio", self.trace.projected_ratio);
        set_int(
            &mut f,
            "n_invalid_fields",
            Some(self.trace.invalid_fields.len() as i64),
        );
        set_num(
            &mut f,
            "snapshot_age_s",
            self.snapshot_age_ms.map(|a| a as f64 / 1000.0),
        );
        f
    }

    fn slot(&self) -> Option<u64> {
        self.snapshot.as_ref().and_then(|m| m.slot)
    }
}
