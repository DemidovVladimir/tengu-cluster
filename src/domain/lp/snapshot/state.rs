//! `lp_state/1:<wallet>:<pool>`: the controller state both decisions carry ([`LpControllerState`]).

use super::*;

// ---------------------------------------------------------------------------

/// «Выдержка на вход» (A15): armed when a recenter closes the position with
/// `reentry_confirm_ms > 0` (`autoTuneOrchestrator.ts:1820-1850`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ReentryWait {
    pub anchor_price: f64,
    pub stable_since_ms: i64,
    /// Closed range width as a price fraction; corridor =
    /// `reentry_tol_frac × width_frac`.
    pub width_frac: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_at_ms: Option<i64>,
}

impl ReentryWait {
    /// Arm the wait when a close-only recenter lands (phase-6 write path).
    #[allow(dead_code)] // phase-6 write path; used by tests today
    pub(crate) fn arm(lower_price: f64, upper_price: f64, price: f64, now_ms: i64) -> Self {
        ReentryWait {
            anchor_price: price,
            stable_since_ms: now_ms,
            width_frac: gates::reentry_width_frac(lower_price, upper_price, price),
            closed_at_ms: Some(now_ms),
        }
    }
}

/// Last hedge mutation (written by phase-6 writes).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct HedgeActionRecord {
    pub at_ms: i64,
    pub action: String,
    /// Only live mutations start the cooldown (`lastActionAtMs`).
    pub live: bool,
    #[serde(default)]
    pub signatures: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position_request: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub counter: Option<u64>,
}

/// `lp_state/1:<wallet>:<pool>` — what the bot kept in process memory and
/// `auto-tune-state.json`, persisted only with `commit = true` (or by
/// phase-6 writes).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct LpControllerState {
    pub wallet: String,
    pub pool: String,
    /// Clamp regime the hedge prices (`lpHedgeRegime`).
    pub committed_regime: LpRegime,
    /// Candidate regime waiting out its window (`pendingLpRegime` + since).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_regime: Option<PendingRegime>,
    /// `volStormActive`.
    #[serde(default)]
    pub storm_active: bool,
    /// `imbalanceSince`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imbalance_since_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reentry: Option<ReentryWait>,
    #[serde(default)]
    pub known_positions: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_hedge_action: Option<HedgeActionRecord>,
    /// `lastRebalanceFailedAt` — lifts the ADR-025 freeze.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_rebalance_failed_at_ms: Option<i64>,
    /// Observation time of the last snapshot that showed an LP position
    /// (recorded by every `hedge_decide` / `lp_decide` whose position read
    /// is whole, gated or not).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_position_seen_ms: Option<i64>,
    /// Observation time of the first no-LP read since a position was last
    /// seen (cleared when one is seen): the BUG-011 grace clock
    /// (`HedgeKnobs::no_lp_grace_ms`) — the bot's `consecutiveNoLpCycles`,
    /// counted from 0 at start-up and reset whenever an LP exists
    /// (`autoTuneOrchestrator.ts:345-346`, `:701-725`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_lp_since_ms: Option<i64>,
    #[serde(default)]
    pub updated_at_ms: i64,
}

impl LpControllerState {
    /// Fresh state (no row yet): regime `in`, nothing pending.
    pub(crate) fn new(wallet: &str, pool: &str) -> Self {
        LpControllerState {
            wallet: wallet.to_string(),
            pool: pool.to_string(),
            committed_regime: LpRegime::In,
            pending_regime: None,
            storm_active: false,
            imbalance_since_ms: None,
            reentry: None,
            known_positions: Vec::new(),
            last_hedge_action: None,
            last_rebalance_failed_at_ms: None,
            last_position_seen_ms: None,
            no_lp_since_ms: None,
            updated_at_ms: 0,
        }
    }

    /// Record what `snap`'s (whole) position read showed at its observation
    /// time (`now_ms − snap_age_ms`): a position → `last_position_seen_ms`
    /// and the no-LP clock stops; none → the clock starts at the first such
    /// read. A read older than the last seen position changes nothing.
    /// Returns whether the state changed.
    fn observe_positions(&mut self, snap: &LpSnapshot, now_ms: i64, snap_age_ms: u64) -> bool {
        let at = now_ms.saturating_sub(i64::try_from(snap_age_ms).unwrap_or(i64::MAX));
        let before = (self.last_position_seen_ms, self.no_lp_since_ms);
        let newer = self.last_position_seen_ms.map_or(true, |t| at > t);
        if snap.positions.is_empty() {
            if newer && self.no_lp_since_ms.is_none() {
                self.no_lp_since_ms = Some(at);
            }
        } else {
            if newer {
                self.last_position_seen_ms = Some(at);
            }
            if self.no_lp_since_ms.is_some_and(|t| at >= t) {
                self.no_lp_since_ms = None;
            }
        }
        (self.last_position_seen_ms, self.no_lp_since_ms) != before
    }

    /// `self` with `snap`'s position observation recorded when the position
    /// read is whole (`updated_at_ms` = `now_ms` if that changed anything) —
    /// the state every evaluation returns, gated or not.
    pub(super) fn observed(&self, snap: &LpSnapshot, now_ms: i64, snap_age_ms: u64) -> Self {
        let mut next = self.clone();
        if position_invalid_fields(snap).is_empty()
            && next.observe_positions(snap, now_ms, snap_age_ms)
        {
            next.updated_at_ms = now_ms;
        }
        next
    }
}

impl Observed for LpControllerState {
    const SCHEMA: &'static str = "lp_state/1";

    fn subject(&self) -> String {
        format!("{}:{}", self.wallet, self.pool)
    }

    fn headline(&self) -> String {
        format!(
            "lp_state {} {} regime={} storm={} imbalance={} reentry={}",
            self.wallet,
            self.pool,
            regime_str(self.committed_regime),
            self.storm_active,
            if self.imbalance_since_ms.is_some() {
                "pending"
            } else {
                "none"
            },
            if self.reentry.is_some() {
                "armed"
            } else {
                "none"
            },
        )
    }

    fn features(&self) -> Features {
        let mut f = Features::new();
        set_str(
            &mut f,
            "committed_regime",
            Some(regime_str(self.committed_regime)),
        );
        set_str(
            &mut f,
            "pending_regime",
            self.pending_regime.map(|p| regime_str(p.regime)),
        );
        set_bool(&mut f, "storm_active", Some(self.storm_active));
        set_bool(
            &mut f,
            "imbalance_pending",
            Some(self.imbalance_since_ms.is_some()),
        );
        set_int(&mut f, "imbalance_since_ms", self.imbalance_since_ms);
        set_bool(&mut f, "reentry_armed", Some(self.reentry.is_some()));
        set_num(
            &mut f,
            "reentry_anchor_price",
            self.reentry.as_ref().map(|r| r.anchor_price),
        );
        set_int(
            &mut f,
            "n_known_positions",
            Some(self.known_positions.len() as i64),
        );
        if let Some(a) = &self.last_hedge_action {
            set_str(&mut f, "last_hedge_action", Some(&a.action));
            set_int(&mut f, "last_hedge_at_ms", Some(a.at_ms));
            set_bool(&mut f, "last_hedge_live", Some(a.live));
        }
        set_bool(
            &mut f,
            "last_rebalance_failed",
            Some(self.last_rebalance_failed_at_ms.is_some()),
        );
        set_int(&mut f, "last_position_seen_ms", self.last_position_seen_ms);
        set_int(&mut f, "no_lp_since_ms", self.no_lp_since_ms);
        set_int(&mut f, "updated_at_ms", Some(self.updated_at_ms));
        f
    }
}
