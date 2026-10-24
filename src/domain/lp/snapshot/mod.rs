//! `lp_snapshot/1` — one wallet × DLMM pool state composed from the family
//! builders — and the pure decisions on top of it: `hedge_decide/1`
//! ([`decide_hedge`]), `lp_decide/1` ([`decide_lp`]) and the controller
//! state both carry, `lp_state/1:<wallet>:<pool>` ([`LpControllerState`]).
//! Pure: typed structs in, typed structs out; no IO, no clocks (`now_ms` is
//! an input). The glue (`tools/solana/lp.rs`) reads, caches and persists.
//!
//! | Piece | Source of truth |
//! |---|---|
//! | [`compose_snapshot`] | builders of `dlmm_pool` / `dlmm_positions` / `jup_perps` / `solana_wallet` + the `price_oracle/1` row |
//! | hedge input assembly ([`decide_hedge`]) | `jupiterPerpsEngine.ts:840-913` (idle SOL, unclamped LP for the cap, auto band, carry) |
//! | midpoint / clamp regime | `autoTuneOrchestrator.ts:1104-1180` → [`gates::regime_confirm`] + [`hedge::lp_hedge_delta`] |
//! | hedge core | [`hedge::decide`] UNCHANGED (1027 production vectors) |
//! | LP verdict ([`decide_lp`]) | `autoTuneOrchestrator.ts:411-441` (storm), `:584-650` (re-entry wait), `:760-830` (imbalance выдержка), `:964-1040` (composition), `:1820-1850` (close-only arm), `meteoraUtils.ts:369` (wallet 50/50) |
//!
//! Gate order, first match wins (failed reads never become 0, BUG-023):
//!
//! | `hedge_decide` | `lp_decide` |
//! |---|---|
//! | `InvalidRead` — a critical field is `Error` / missing, or the position read is not whole ([`position_invalid_fields`]) | `Paused{InvalidRead}` (same position rule) |
//! | `WalletSolZero` — `include_wallet_sol` and native SOL exactly 0 | `Paused{MultiplePositions}` |
//! | `NotApplicable` — base is not native SOL or quote is not USDC | `Paused{Divergence}` — \|pool vs oracle\| > `max_divergence_bps` |
//! | `PendingRequest` — keeper request open, age < max exec + 15 s | `Blocked` — snapshot or price row older than `max_snapshot_age_secs` |
//! | `Divergence` — \|pool vs oracle\| > `max_divergence_bps` | storm → trend confirm → recenter / re-entry wait → open |
//! | `StaleInput` — snapshot or price row older than `max_snapshot_age_secs` | |
//! | `None{"no-LP grace…"}` — no LP position for < `no_lp_grace_ms` since the first no-LP read, no re-entry wait (BUG-011) | |
//! | `hedge::decide`, then `VenuePermission` (custody disallows the action) | |
//!
//! A gated evaluation returns the controller state unchanged except the
//! position observation (`last_position_seen_ms` / `no_lp_since_ms`,
//! recorded whenever the position read is whole — an observation, not a
//! decision); only an evaluation that passes the gates advances timers /
//! regimes. The caller persists the returned state only with `commit = true`.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::dlmm::{
    Discovery, DlmmAnomaly, DlmmPoolState, DlmmPosition, DlmmPositions, LpExposure, PairRoles,
};
use super::gates::{
    self, ImbalanceAction, PendingRegime, PriceSample, ReentryDecision, ReentryGateInput,
    RegimeConfirmInput, RegimeOutcome, StormInput, TrendConfirmInput,
};
use super::hedge::{self, Decision, HedgeInput, LpRegime};
use super::market::{OraclePrice, PriceSource};
use super::perps::{CustodyRates, PerpSide, PerpsState, RequestStatus, Side};
use super::wallet::{TokenAmount, WalletInventory};
use crate::domain::observation::{
    set_bool, set_int, set_num, set_str, ErrorClass, Features, Field, ObsMeta, ObsStatus,
    Observation, Observed, ReadError,
};
use crate::domain::solana::{ata, bin_array_pda, ids, Pubkey};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// `lp_snapshot` cache TTL (final-spec §C).
pub(crate) const LP_SNAPSHOT_TTL_MS: u64 = 10_000;
/// `lp_state` rows live until overwritten (the store purges rows > 7 days).
pub(crate) const LP_STATE_TTL_MS: u64 = 7 * 24 * 3_600_000;
/// Grace on top of the JLP pool's `maxRequestExecutionSec` before an
/// unexecuted keeper request stops blocking the hedge (design-1 §7.2).
pub(crate) const PENDING_REQUEST_GRACE_SECS: i64 = 15;
/// `isWalletBalancedFor5050` default tolerance (`meteoraUtils.ts:369`).
pub(crate) const WALLET_5050_TOLERANCE: f64 = 0.10;
/// Rent of a new DLMM position (`METEORA_POSITION_RENT_SOL`,
/// `swapPlanner.ts:27`).
pub(crate) const POSITION_RENT_SOL: f64 = 0.0575;
const LAMPORTS_PER_SOL: f64 = 1_000_000_000.0;

mod compose;
mod hedge_decide;
mod knobs;
mod lp_decide;
mod state;

pub(crate) use compose::*;
pub(crate) use hedge_decide::*;
pub(crate) use knobs::*;
pub(crate) use lp_decide::*;
pub(crate) use state::*;

#[cfg(test)]
mod tests;
