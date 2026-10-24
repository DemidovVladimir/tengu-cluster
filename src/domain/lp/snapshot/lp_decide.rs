//! `lp_decide/1`: [`decide_lp`] — storm, trend confirm, recenter / re-entry wait, open.

use super::*;

// ---------------------------------------------------------------------------

/// Composition of one position at the pool's active price.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PositionHealth {
    pub position: String,
    pub lower_bin_id: i32,
    pub upper_bin_id: i32,
    /// Base share by price position in the range, percent
    /// (`calculateTokenPercentages`).
    pub base_pct_linear: f64,
    pub quote_pct_linear: f64,
    pub base_pct_value: Option<f64>,
    pub in_range: bool,
    /// `below` = active bin under the range (all base), `above` = over it.
    pub out_of_range: Option<LpRegime>,
    pub is_imbalanced: bool,
    pub threshold_pct: f64,
    /// `active − lower` (negative = out of range below).
    pub bins_to_lower_edge: i32,
    /// `upper − active` (negative = out of range above).
    pub bins_to_upper_edge: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct StormState {
    /// |price / reference − 1| × 100 (reference ≥ 4 min old); `None`
    /// without history.
    pub move_5m_pct: Option<f64>,
    pub active: bool,
    pub threshold_pct: f64,
}

/// Wallet composition for a deposit (`isWalletBalancedFor5050`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct WalletSplit {
    pub base_units: f64,
    pub quote_units: f64,
    /// Base value share of the usable wallet (reserves excluded).
    pub base_value_ratio: f64,
    /// Within 50/50 ± 10 %.
    pub balanced: bool,
    pub total_value_quote: f64,
    /// Quote value to move across to reach 50/50.
    pub swap_needed_quote: f64,
}

/// Re-entry wait progress (after this evaluation).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ReentryView {
    pub anchor_price: f64,
    pub stable_since_ms: i64,
    pub width_frac: f64,
    /// Corridor half width, price fraction.
    pub tol_price_frac: f64,
    pub held_ms: i64,
    pub remaining_ms: i64,
}

/// A centered range for a new position (`gates::centered_range`, ≤ 70 bins).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct RangePlan {
    pub active_id: i32,
    pub min_bin_id: i32,
    pub max_bin_id: i32,
    pub width: u32,
    pub lower_price: f64,
    pub upper_price: f64,
    pub bin_array_indexes: Vec<i64>,
    pub clamped_to_max_width: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PauseReason {
    Storm,
    Divergence,
    InvalidRead,
    MultiplePositions,
}

impl PauseReason {
    fn as_str(self) -> &'static str {
        match self {
            PauseReason::Storm => "storm",
            PauseReason::Divergence => "divergence",
            PauseReason::InvalidRead => "invalid_read",
            PauseReason::MultiplePositions => "multiple_positions",
        }
    }
}

/// One LP verdict per evaluation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub(crate) enum LpVerdict {
    /// Position healthy.
    Hold {
        reason: String,
    },
    /// Recenter `position`. `close_only` = `reentry_confirm_ms > 0`: close
    /// and arm the re-entry wait instead of reopening (A15); else `plan` is
    /// the new range.
    Recenter {
        position: String,
        reason: String,
        close_only: bool,
        plan: Option<RangePlan>,
    },
    /// Imbalanced; the trend confirmation window is running (ADR-023).
    WaitTrend {
        since_ms: i64,
        remaining_ms: i64,
    },
    Paused {
        reason: PauseReason,
    },
    /// No position, re-entry wait armed and not yet satisfied (`hold`) or
    /// re-anchored (`rearm`).
    Reentry {
        decision: ReentryDecision,
    },
    /// No position: open `plan` (`reentry` = the re-entry wait released it).
    Open {
        plan: RangePlan,
        reentry: bool,
    },
    Blocked {
        reason: String,
    },
}

impl LpVerdict {
    pub(crate) fn name(&self) -> &'static str {
        match self {
            LpVerdict::Hold { .. } => "hold",
            LpVerdict::Recenter { .. } => "recenter",
            LpVerdict::WaitTrend { .. } => "wait_trend",
            LpVerdict::Paused { .. } => "paused",
            LpVerdict::Reentry { .. } => "reentry",
            LpVerdict::Open { .. } => "open",
            LpVerdict::Blocked { .. } => "blocked",
        }
    }

    fn plan(&self) -> Option<&RangePlan> {
        match self {
            LpVerdict::Open { plan, .. } => Some(plan),
            LpVerdict::Recenter { plan, .. } => plan.as_ref(),
            _ => None,
        }
    }
}

/// `lp_decide/1:<wallet>:<pool>` (never cached).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct LpDecision {
    pub wallet: String,
    pub pool: String,
    /// The `lp_snapshot` row decided on; `None` = no row.
    pub snapshot: Option<ObsMeta>,
    pub snapshot_age_ms: Option<u64>,
    pub knobs: LpKnobs,
    /// Price the LP logic ran on (oracle for a USDC quote, else pool).
    pub cycle_price: Option<f64>,
    pub health: Vec<PositionHealth>,
    pub storm: StormState,
    pub reentry: Option<ReentryView>,
    pub wallet_split: Option<WalletSplit>,
    pub invalid_fields: Vec<String>,
    pub verdict: LpVerdict,
}

impl LpDecision {
    /// No usable `lp_snapshot` row: `Blocked`.
    pub(crate) fn without_snapshot(wallet: &str, pool: &str, knobs: &LpKnobs, why: &str) -> Self {
        LpDecision {
            wallet: wallet.to_string(),
            pool: pool.to_string(),
            snapshot: None,
            snapshot_age_ms: None,
            knobs: knobs.clone(),
            cycle_price: None,
            health: Vec::new(),
            storm: StormState {
                move_5m_pct: None,
                active: false,
                threshold_pct: knobs.storm_pct_5m,
            },
            reentry: None,
            wallet_split: None,
            invalid_fields: Vec::new(),
            verdict: LpVerdict::Blocked {
                reason: why.to_string(),
            },
        }
    }
}

/// Critical fields for the LP verdict.
pub(super) fn lp_invalid_fields(s: &LpSnapshot) -> Vec<String> {
    let mut v = position_invalid_fields(s);
    if s.cycle_price().is_none() {
        v.push("oracle.usd");
    }
    if s.wallet_balances.native_sol.value().is_none() {
        v.push("wallet.native_sol");
    }
    if s.wallet_base_units().is_none() {
        v.push("wallet.base");
    }
    if s.wallet_quote_units().is_none() {
        v.push("wallet.quote");
    }
    v.into_iter().map(String::from).collect()
}

pub(super) fn position_health(
    p: &DlmmPosition,
    active_id: i32,
    pool_price: f64,
    threshold: f64,
) -> PositionHealth {
    let imb = gates::check_position_imbalance(pool_price, p.lower_price, p.upper_price, threshold);
    PositionHealth {
        position: p.position.clone(),
        lower_bin_id: p.lower_bin_id,
        upper_bin_id: p.upper_bin_id,
        base_pct_linear: imb.as_ref().map_or(p.base_pct_linear, |i| i.x_percent),
        quote_pct_linear: imb
            .as_ref()
            .map_or(100.0 - p.base_pct_linear, |i| i.y_percent),
        base_pct_value: p.base_pct_value,
        in_range: p.lower_bin_id <= active_id && active_id <= p.upper_bin_id,
        out_of_range: if active_id < p.lower_bin_id {
            Some(LpRegime::Below)
        } else if active_id > p.upper_bin_id {
            Some(LpRegime::Above)
        } else {
            None
        },
        is_imbalanced: imb.as_ref().is_some_and(|i| i.is_imbalanced),
        threshold_pct: threshold * 100.0,
        bins_to_lower_edge: active_id - p.lower_bin_id,
        bins_to_upper_edge: p.upper_bin_id - active_id,
        reason: imb.and_then(|i| i.reason),
    }
}

pub(super) fn wallet_split(
    snap: &LpSnapshot,
    knobs: &LpKnobs,
    price: Option<f64>,
) -> Option<WalletSplit> {
    let price = price?;
    let base = snap.wallet_base_units()?;
    let quote = snap.wallet_quote_units()?;
    let reserve = knobs.min_wallet_sol + knobs.rent_reserve_sol;
    let base_reserve = if snap.pair.base_is_native_sol {
        reserve
    } else {
        0.0
    };
    // X/SOL pools: the SOL reserves ride on the quote leg
    // (`quoteSideReserveExtra`, `autoTuneOrchestrator.ts:1585-1592`).
    let quote_reserve = if snap.pair.quote_is_native_sol {
        reserve + POSITION_RENT_SOL
    } else {
        0.0
    };
    let b = gates::wallet_balanced_for_5050(
        base,
        (quote - quote_reserve).max(0.0),
        price,
        base_reserve,
        WALLET_5050_TOLERANCE,
    );
    Some(WalletSplit {
        base_units: base,
        quote_units: quote,
        base_value_ratio: b.wallet_sol_ratio,
        balanced: b.balanced,
        total_value_quote: b.wallet_total_usd,
        swap_needed_quote: (b.wallet_sol_ratio - 0.5).abs() * b.wallet_total_usd,
    })
}

pub(super) fn range_plan(snap: &LpSnapshot, bin_count: u32) -> Option<RangePlan> {
    let r = gates::centered_range(snap.active_id, bin_count)?;
    let (dx, dy) = (snap.pair.base_decimals, snap.pair.quote_decimals);
    Some(RangePlan {
        active_id: snap.active_id,
        min_bin_id: r.min_bin_id,
        max_bin_id: r.max_bin_id,
        width: r.width,
        lower_price: gates::price_from_bin(r.min_bin_id, snap.bin_step, dx, dy),
        upper_price: gates::price_from_bin(r.max_bin_id, snap.bin_step, dx, dy),
        bin_array_indexes: gates::bin_array_indexes(r.min_bin_id, r.max_bin_id),
        clamped_to_max_width: r.clamped_to_max_width,
    })
}

/// `Open` when the pool is enabled and the wallet can fund a position
/// (`autoTuneOrchestrator.ts:1882-1893`), else `Blocked`.
pub(super) fn open_or_blocked(
    snap: &LpSnapshot,
    knobs: &LpKnobs,
    split: Option<&WalletSplit>,
    reentry: bool,
) -> LpVerdict {
    let blocked = |reason: String| LpVerdict::Blocked { reason };
    if !snap.pool_enabled {
        return blocked(format!("pool {} is disabled", snap.pool));
    }
    let Some(s) = split else {
        return blocked("wallet split unavailable".into());
    };
    if snap.pair.base_is_native_sol {
        let need = knobs.min_wallet_sol + knobs.rent_reserve_sol + POSITION_RENT_SOL;
        if s.base_units - need <= 0.0 {
            return blocked(format!(
                "wallet SOL {} does not cover reserves + position rent {}",
                fmt_sig(s.base_units, 9),
                fmt_sig(need, 9)
            ));
        }
    }
    if s.total_value_quote.is_nan() || s.total_value_quote <= 0.0 {
        return blocked("wallet holds nothing to deposit above reserves".into());
    }
    match range_plan(snap, knobs.bin_count) {
        Some(plan) => LpVerdict::Open { plan, reentry },
        None => blocked(format!(
            "no centered range of {} bins around bin {}",
            knobs.bin_count, snap.active_id
        )),
    }
}

/// `lp_decide`: validity gates, then storm → imbalance выдержка → recenter
/// for a position, or the re-entry wait → open without one. `price_samples`
/// = the cycle-price series (the `price_oracle` row's samples; ignored for a
/// pool whose quote is not USDC, where the cycle price is the pool's own).
pub(crate) fn decide_lp(
    snap: &LpSnapshot,
    snap_meta: &ObsMeta,
    snap_age_ms: u64,
    knobs: &LpKnobs,
    state: &LpControllerState,
    price_samples: &[PriceSample],
    now_ms: i64,
) -> (LpDecision, LpControllerState) {
    let cycle_price = snap.cycle_price();
    let storm = LpCycle::new(knobs, price_samples).storm(snap, state.storm_active, now_ms);
    let health: Vec<PositionHealth> = snap
        .positions
        .iter()
        .map(|p| {
            position_health(
                p,
                snap.active_id,
                snap.pool_price,
                knobs.imbalance_threshold,
            )
        })
        .collect();
    let split = wallet_split(snap, knobs, cycle_price);
    let invalid = lp_invalid_fields(snap);
    let mut out = LpDecision {
        wallet: snap.wallet.clone(),
        pool: snap.pool.clone(),
        snapshot: Some(snap_meta.clone()),
        snapshot_age_ms: Some(snap_age_ms),
        knobs: knobs.clone(),
        cycle_price,
        health,
        storm,
        reentry: None,
        wallet_split: split,
        invalid_fields: invalid.clone(),
        verdict: LpVerdict::Hold {
            reason: String::new(),
        },
    };

    // Validity gates: state unchanged but for the position observation.
    let observed = state.observed(snap, now_ms, snap_age_ms);
    let gate = if !invalid.is_empty() {
        Some(LpVerdict::Paused {
            reason: PauseReason::InvalidRead,
        })
    } else if snap.positions.len() > 1 {
        Some(LpVerdict::Paused {
            reason: PauseReason::MultiplePositions,
        })
    } else if divergence_reason(snap, knobs.max_divergence_bps).is_some() {
        Some(LpVerdict::Paused {
            reason: PauseReason::Divergence,
        })
    } else {
        stale_reason(snap, snap_age_ms, knobs.max_snapshot_age_secs)
            .map(|reason| LpVerdict::Blocked { reason })
    };
    if let Some(v) = gate {
        out.verdict = v;
        return (out, observed);
    }

    let price = cycle_price.unwrap_or(f64::NAN);
    let mut next = observed;
    next.storm_active = out.storm.active;
    next.updated_at_ms = now_ms;
    next.known_positions = snap.positions.iter().map(|p| p.position.clone()).collect();

    out.verdict = match (snap.positions.first(), out.health.first()) {
        (Some(p), Some(h)) => {
            // A position on chain wins over an armed wait (self-heal, A15).
            next.reentry = None;
            let t = gates::trend_confirm(&TrendConfirmInput {
                now_ms,
                is_imbalanced: h.is_imbalanced,
                imbalance_since_ms: state.imbalance_since_ms,
                confirm_ms: i64::try_from(knobs.trend_confirm_ms).unwrap_or(i64::MAX),
                storm_active: out.storm.active,
            });
            next.imbalance_since_ms = t.imbalance_since_ms;
            match t.action {
                ImbalanceAction::Balanced => LpVerdict::Hold {
                    reason: format!(
                        "composition base {:.2}% / quote {:.2}% within {:.2}%",
                        h.base_pct_linear, h.quote_pct_linear, h.threshold_pct
                    ),
                },
                ImbalanceAction::StormPaused => LpVerdict::Paused {
                    reason: PauseReason::Storm,
                },
                ImbalanceAction::Waiting => LpVerdict::WaitTrend {
                    since_ms: t.imbalance_since_ms.unwrap_or(now_ms),
                    remaining_ms: t.remaining_ms,
                },
                ImbalanceAction::Recenter => {
                    let close_only = knobs.reentry_confirm_ms > 0;
                    LpVerdict::Recenter {
                        position: p.position.clone(),
                        reason: h
                            .reason
                            .clone()
                            .unwrap_or_else(|| "position imbalanced".into()),
                        close_only,
                        plan: if close_only {
                            None
                        } else {
                            range_plan(snap, knobs.bin_count)
                        },
                    }
                }
            }
        }
        _ => {
            next.imbalance_since_ms = None;
            match &state.reentry {
                Some(w) => {
                    let tol = knobs.reentry_tol_frac * w.width_frac;
                    let confirm_ms = i64::try_from(knobs.reentry_confirm_ms).unwrap_or(i64::MAX);
                    let d = gates::evaluate_reentry_gate(&ReentryGateInput {
                        now_ms,
                        price,
                        anchor_price: w.anchor_price,
                        stable_since_ms: w.stable_since_ms,
                        tol_price_frac: tol,
                        confirm_ms,
                        storm_active: out.storm.active,
                    });
                    let mut wait = w.clone();
                    if let ReentryDecision::Rearm {
                        anchor_price,
                        stable_since_ms,
                    } = &d
                    {
                        wait.anchor_price = *anchor_price;
                        wait.stable_since_ms = *stable_since_ms;
                    }
                    let held_ms = now_ms - wait.stable_since_ms;
                    out.reentry = Some(ReentryView {
                        anchor_price: wait.anchor_price,
                        stable_since_ms: wait.stable_since_ms,
                        width_frac: wait.width_frac,
                        tol_price_frac: tol,
                        held_ms,
                        remaining_ms: (confirm_ms - held_ms).max(0),
                    });
                    // The wait stays armed until the open lands (A15).
                    next.reentry = Some(wait);
                    match d {
                        ReentryDecision::Open => {
                            open_or_blocked(snap, knobs, out.wallet_split.as_ref(), true)
                        }
                        other => LpVerdict::Reentry { decision: other },
                    }
                }
                None => open_or_blocked(snap, knobs, out.wallet_split.as_ref(), false),
            }
        }
    };
    (out, next)
}

impl Observed for LpDecision {
    const SCHEMA: &'static str = "lp_decide/1";

    fn subject(&self) -> String {
        format!("{}:{}", self.wallet, self.pool)
    }

    /// `lp_decide <wallet> <pool> verdict=… […]`
    fn headline(&self) -> String {
        let detail = match &self.verdict {
            LpVerdict::Paused { reason } => format!(" reason={}", reason.as_str()),
            LpVerdict::WaitTrend { remaining_ms, .. } => {
                format!(" remaining_ms={remaining_ms}")
            }
            LpVerdict::Reentry { decision } => format!(
                " decision={}",
                match decision {
                    ReentryDecision::Hold => "hold",
                    ReentryDecision::Rearm { .. } => "rearm",
                    ReentryDecision::Open => "open",
                }
            ),
            LpVerdict::Recenter { close_only, .. } if *close_only => " close_only".into(),
            _ => String::new(),
        };
        let range = self
            .verdict
            .plan()
            .map(|p| format!(" bins={}..{}", p.min_bin_id, p.max_bin_id))
            .unwrap_or_default();
        format!(
            "lp_decide {} {} verdict={}{detail}{range}",
            self.wallet,
            self.pool,
            self.verdict.name()
        )
    }

    fn features(&self) -> Features {
        let mut f = Features::new();
        set_str(&mut f, "verdict", Some(self.verdict.name()));
        if let LpVerdict::Paused { reason } = &self.verdict {
            set_str(&mut f, "pause_reason", Some(reason.as_str()));
        }
        if let LpVerdict::WaitTrend { remaining_ms, .. } = &self.verdict {
            set_int(&mut f, "wait_remaining_ms", Some(*remaining_ms));
        }
        if let LpVerdict::Recenter { close_only, .. } = &self.verdict {
            set_bool(&mut f, "close_only", Some(*close_only));
        }
        set_num(&mut f, "cycle_price", self.cycle_price);
        let n = self.health.len();
        set_int(&mut f, "position_count", Some(n as i64));
        if let [h] = self.health.as_slice() {
            set_bool(&mut f, "in_range", Some(h.in_range));
            set_bool(&mut f, "is_imbalanced", Some(h.is_imbalanced));
            set_num(&mut f, "base_pct_linear", Some(h.base_pct_linear));
            set_num(&mut f, "base_pct_value", h.base_pct_value);
            set_int(
                &mut f,
                "bins_to_lower_edge",
                Some(i64::from(h.bins_to_lower_edge)),
            );
            set_int(
                &mut f,
                "bins_to_upper_edge",
                Some(i64::from(h.bins_to_upper_edge)),
            );
        }
        set_int(
            &mut f,
            "bins_to_edge_min",
            self.health
                .iter()
                .map(|h| i64::from(h.bins_to_lower_edge.min(h.bins_to_upper_edge)))
                .min(),
        );
        set_bool(&mut f, "storm_active", Some(self.storm.active));
        set_num(&mut f, "move_5m_pct", self.storm.move_5m_pct);
        set_str(
            &mut f,
            "reentry",
            Some(match &self.verdict {
                LpVerdict::Reentry {
                    decision: ReentryDecision::Rearm { .. },
                } => "rearm",
                LpVerdict::Reentry { .. } => "hold",
                LpVerdict::Open { reentry: true, .. } => "open",
                _ if self.reentry.is_some() => "armed",
                _ => "none",
            }),
        );
        set_int(
            &mut f,
            "reentry_remaining_ms",
            self.reentry.as_ref().map(|r| r.remaining_ms),
        );
        if let Some(s) = &self.wallet_split {
            set_bool(&mut f, "wallet_balanced", Some(s.balanced));
            set_num(&mut f, "wallet_base_value_ratio", Some(s.base_value_ratio));
            set_num(&mut f, "swap_needed_quote", Some(s.swap_needed_quote));
        }
        if let Some(p) = self.verdict.plan() {
            set_int(&mut f, "plan_min_bin_id", Some(i64::from(p.min_bin_id)));
            set_int(&mut f, "plan_max_bin_id", Some(i64::from(p.max_bin_id)));
            set_int(&mut f, "plan_width", Some(i64::from(p.width)));
        }
        set_int(
            &mut f,
            "n_invalid_fields",
            Some(self.invalid_fields.len() as i64),
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
