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
//! | `InvalidRead` — a critical field is `Error` / missing | `Paused{InvalidRead}` |
//! | `WalletSolZero` — `include_wallet_sol` and native SOL exactly 0 | `Paused{MultiplePositions}` |
//! | `NotApplicable` — base is not native SOL or quote is not USDC | `Paused{Divergence}` — \|pool vs oracle\| > `max_divergence_bps` |
//! | `PendingRequest` — keeper request open, age < max exec + 15 s | `Blocked` — snapshot or price row older than `max_snapshot_age_secs` |
//! | `Divergence` — \|pool vs oracle\| > `max_divergence_bps` | storm → trend confirm → recenter / re-entry wait → open |
//! | `StaleInput` — snapshot or price row older than `max_snapshot_age_secs` | |
//! | `hedge::decide`, then `VenuePermission` (custody disallows the action) | |
//!
//! A gated evaluation returns the controller state unchanged; only an
//! evaluation that passes the gates advances timers / regimes. The caller
//! persists the returned state only with `commit = true`.

// Consumed by the lp_snapshot / hedge_decide / lp_decide tools (stage 4).
#![allow(dead_code)]

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
use super::perps::{CustodyRates, PerpSide, PerpsState, RequestStatus};
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
/// Clamp-regime confirmation window of the hedge. The hedge knob schema
/// (`tools/solana/defs.rs`) carries no `trend_confirm_ms`, so a computed
/// regime commits at once (the bot default `TREND_CONFIRM_MS = 0`); the
/// ADR-025 freeze still applies.
pub(crate) const HEDGE_REGIME_CONFIRM_MS: i64 = 0;
/// `isWalletBalancedFor5050` default tolerance (`meteoraUtils.ts:369`).
pub(crate) const WALLET_5050_TOLERANCE: f64 = 0.10;
/// Rent of a new DLMM position (`METEORA_POSITION_RENT_SOL`,
/// `swapPlanner.ts:27`).
pub(crate) const POSITION_RENT_SOL: f64 = 0.0575;
const LAMPORTS_PER_SOL: f64 = 1_000_000_000.0;

// ---------------------------------------------------------------------------
// lp_snapshot/1
// ---------------------------------------------------------------------------

/// The `price_oracle/1` row a snapshot used. `needed` = the pool's quote is
/// USDC (the oracle prices the LP and the hedge); otherwise the pool's own
/// active price is the cycle price and the oracle is informational.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct OracleRef {
    /// `price_oracle/1:<base mint>`.
    pub key: String,
    pub needed: bool,
    /// USD per base token; `None` (never 0) when the row is missing, for
    /// another mint, or has no usable source.
    pub usd: Option<f64>,
    pub source: Option<PriceSource>,
    pub degraded: bool,
    pub move_5m_pct: Option<f64>,
    /// Row status; `None` = no row.
    pub status: Option<ObsStatus>,
    /// Row age when the snapshot was composed; `None` = no row.
    pub age_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ReadError>,
}

/// Wallet balances the LP / hedge logic reads. `base` / `quote` / `wsol`
/// are ATA balances of the pair mints and wSOL; a native-SOL side uses
/// `native_sol` (the bot's `getBaseBalance`, `autoTuneOrchestrator.ts:1478`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct WalletBalances {
    pub native_sol: Field<f64>,
    pub base: Field<TokenAmount>,
    pub quote: Field<TokenAmount>,
    pub wsol: Field<TokenAmount>,
}

/// The Jupiter perps side of a snapshot. `applicable` = base is native SOL
/// and quote is USDC (the only market the hedge trades).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct HedgeRead {
    pub applicable: bool,
    /// `Absent` = flat.
    pub long: Field<PerpSide>,
    pub short: Field<PerpSide>,
    pub sol_custody: Field<CustodyRates>,
    pub usdc_custody: Field<CustodyRates>,
    pub max_request_execution_sec: Field<i64>,
    pub both_sides_open: bool,
    /// Σ collateral / Σ notional over open sides; `None` when flat.
    pub collateral_ratio: Option<f64>,
    /// Keeper request recorded in `lp_state` (set by the glue).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_request: Option<Field<RequestStatus>>,
}

/// Snapshot-wide anomalies (DLMM ones mapped from [`DlmmAnomaly`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Anomaly {
    BothSidesOpen,
    MultiplePositions { count: u32 },
    HedgeNotApplicable,
    ExtendedPosition { position: String },
    PoolDisabled,
    WalletSolZero,
}

/// `lp_snapshot/1:<wallet>:<pool>` — canonical wallet × pool state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct LpSnapshot {
    pub wallet: String,
    pub pool: String,
    /// Oldest slot among the component reads.
    pub slot: u64,
    pub slot_max: u64,
    pub pair: PairRoles,
    pub pool_enabled: bool,
    /// Quote per base at the active bin.
    pub pool_price: f64,
    pub active_id: i32,
    pub bin_step: u16,
    pub oracle: OracleRef,
    /// (pool − oracle) / oracle × 10⁴ (positive = pool above oracle); only
    /// when the oracle is needed and usable.
    pub pool_vs_oracle_bps: Option<f64>,
    pub wallet_balances: WalletBalances,
    pub discovery: Discovery,
    pub positions: Vec<DlmmPosition>,
    pub exposure: Field<LpExposure>,
    pub hedge: HedgeRead,
    pub anomalies: Vec<Anomaly>,
    /// Every pubkey read, full base58, sorted (the phase-5 subscription set).
    pub watch: Vec<String>,
    /// Errors not carried by a field (discovery / positions / component
    /// mismatches).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<ReadError>,
}

/// Compose the snapshot of `wallet` in `pool` from the family builders'
/// outputs (the glue builds them over ONE `AccountSet`) and the cached
/// `price_oracle/1:<base mint>` row. Components for another wallet / pool
/// are rejected (their fields become `Error`), never mixed in.
pub(crate) fn compose_snapshot(
    wallet: &Pubkey,
    pool: &DlmmPoolState,
    positions: &DlmmPositions,
    perps: Option<&PerpsState>,
    wallet_inv: &WalletInventory,
    oracle: Option<&Observation>,
    now_ms: i64,
) -> LpSnapshot {
    let wallet_s = wallet.to_string();
    let pair = pool.pair.clone();
    let applicable = pair.base_is_native_sol && pair.quote_is_usd;
    let mut errors = Vec::new();
    let mut slots = vec![pool.slot];
    let mut watch: BTreeSet<String> = [
        pool.pool.clone(),
        pair.base_mint.clone(),
        pair.quote_mint.clone(),
        pool.reserve_x.clone(),
        pool.reserve_y.clone(),
    ]
    .into_iter()
    .collect();
    if let (Some(d), Ok(pool_key)) = (pool.depth.value(), pool.pool.parse::<Pubkey>()) {
        for idx in d
            .bin_arrays
            .iter()
            .filter(|i| !d.missing_bin_arrays.contains(i))
        {
            watch.insert(bin_array_pda(&pool_key, *idx).to_string());
        }
    }

    // DLMM positions.
    let mut anomalies = Vec::new();
    let (discovery, dlmm_positions, exposure) =
        if positions.pool != pool.pool || positions.wallet != wallet_s {
            let e = ReadError::new(
                "discovery",
                ErrorClass::Fatal,
                format!(
                    "dlmm_positions is for wallet {} pool {}, expected wallet {wallet_s} pool {}",
                    positions.wallet, positions.pool, pool.pool
                ),
            );
            errors.push(e.clone());
            let exposure = Field::err(ReadError {
                field: "exposure".into(),
                ..e.clone()
            });
            (
                Discovery::Error {
                    error: e,
                    fallback_count: 0,
                },
                Vec::new(),
                exposure,
            )
        } else {
            errors.extend(positions.errors.iter().cloned());
            slots.push(positions.slot);
            watch.extend(positions.watch.iter().cloned());
            anomalies.extend(positions.anomalies.iter().map(|a| match a {
                DlmmAnomaly::MultiplePositions { count } => {
                    Anomaly::MultiplePositions { count: *count }
                }
                DlmmAnomaly::ExtendedPosition { position } => Anomaly::ExtendedPosition {
                    position: position.clone(),
                },
                DlmmAnomaly::PoolDisabled => Anomaly::PoolDisabled,
            }));
            (
                positions.discovery.clone(),
                positions.positions.clone(),
                positions.exposure.clone(),
            )
        };
    if !pool.enabled && !anomalies.contains(&Anomaly::PoolDisabled) {
        anomalies.push(Anomaly::PoolDisabled);
    }

    // Jupiter perps.
    let perps = match perps {
        Some(p) if p.wallet != wallet_s => {
            errors.push(ReadError::new(
                "hedge",
                ErrorClass::Fatal,
                format!("jup_perps is for wallet {}, expected {wallet_s}", p.wallet),
            ));
            None
        }
        other => other,
    };
    let hedge = match perps {
        Some(p) => {
            slots.push(p.slot);
            watch.extend(p.watch.iter().cloned());
            HedgeRead {
                applicable,
                long: relabel(&p.long, "hedge.long"),
                short: relabel(&p.short, "hedge.short"),
                sol_custody: relabel(&p.sol, "hedge.sol_custody"),
                usdc_custody: relabel(&p.usdc, "hedge.usdc_custody"),
                max_request_execution_sec: relabel(
                    &p.max_request_execution_sec,
                    "hedge.max_request_execution_sec",
                ),
                both_sides_open: p.both_sides_open,
                collateral_ratio: p.collateral_ratio,
                pending_request: None,
            }
        }
        None => {
            let class = if applicable {
                ErrorClass::Fatal
            } else {
                ErrorClass::NotApplicable
            };
            let miss = |f: &str| ReadError::new(f, class, "jup_perps not read for this snapshot");
            HedgeRead {
                applicable,
                long: Field::err(miss("hedge.long")),
                short: Field::err(miss("hedge.short")),
                sol_custody: Field::err(miss("hedge.sol_custody")),
                usdc_custody: Field::err(miss("hedge.usdc_custody")),
                max_request_execution_sec: Field::err(miss("hedge.max_request_execution_sec")),
                both_sides_open: false,
                collateral_ratio: None,
                pending_request: None,
            }
        }
    };
    if hedge.both_sides_open {
        anomalies.push(Anomaly::BothSidesOpen);
    }
    if !applicable {
        anomalies.push(Anomaly::HedgeNotApplicable);
    }

    // Wallet balances.
    let wallet_balances = if wallet_inv.wallet == *wallet {
        slots.push(wallet_inv.slot);
        watch.insert(wallet_s.clone());
        let token = ids::TOKEN.to_string();
        for (mint, program) in [
            (&pair.base_mint, &pair.base_token_program),
            (&pair.quote_mint, &pair.quote_token_program),
            (&ids::WSOL.to_string(), &token),
        ] {
            if let (Ok(m), Ok(p)) = (mint.parse::<Pubkey>(), program.parse::<Pubkey>()) {
                if wallet_inv.balance(&m).is_some() {
                    watch.insert(ata(wallet, &m, &p).to_string());
                }
            }
        }
        WalletBalances {
            native_sol: match &wallet_inv.lamports {
                Field::Ok { value } => Field::ok(*value as f64 / LAMPORTS_PER_SOL),
                Field::Absent => Field::Absent,
                Field::Error { error } => Field::err(ReadError {
                    field: "wallet.native_sol".into(),
                    ..error.clone()
                }),
            },
            base: balance_field(wallet_inv, &pair.base_mint, "wallet.base"),
            quote: balance_field(wallet_inv, &pair.quote_mint, "wallet.quote"),
            wsol: balance_field(wallet_inv, ids::WSOL, "wallet.wsol"),
        }
    } else {
        let e = |f: &str| {
            ReadError::new(
                f,
                ErrorClass::Fatal,
                format!(
                    "solana_wallet is for wallet {}, expected {wallet_s}",
                    wallet_inv.wallet
                ),
            )
        };
        WalletBalances {
            native_sol: Field::err(e("wallet.native_sol")),
            base: Field::err(e("wallet.base")),
            quote: Field::err(e("wallet.quote")),
            wsol: Field::err(e("wallet.wsol")),
        }
    };
    if wallet_balances.native_sol.value() == Some(&0.0) {
        anomalies.push(Anomaly::WalletSolZero);
    }

    let oracle = oracle_ref(&pair, oracle, now_ms);
    let pool_vs_oracle_bps = match (oracle.needed, oracle.usd) {
        (true, Some(u)) => finite((pool.active_price - u) / u * 1e4),
        _ => None,
    };

    LpSnapshot {
        wallet: wallet_s,
        pool: pool.pool.clone(),
        slot: slots.iter().copied().min().unwrap_or(pool.slot),
        slot_max: slots.iter().copied().max().unwrap_or(pool.slot),
        pool_enabled: pool.enabled,
        pool_price: pool.active_price,
        active_id: pool.active_id,
        bin_step: pool.bin_step,
        pair,
        oracle,
        pool_vs_oracle_bps,
        wallet_balances,
        discovery,
        positions: dlmm_positions,
        exposure,
        hedge,
        anomalies,
        watch: watch.into_iter().collect(),
        errors,
    }
}

fn oracle_ref(pair: &PairRoles, oracle: Option<&Observation>, now_ms: i64) -> OracleRef {
    let mut r = OracleRef {
        key: Observation::key_for(OraclePrice::SCHEMA, &pair.base_mint),
        needed: pair.quote_is_usd,
        usd: None,
        source: None,
        degraded: true,
        move_5m_pct: None,
        status: None,
        age_ms: None,
        error: None,
    };
    let Some(obs) = oracle else {
        if r.needed {
            r.error = Some(ReadError::new(
                "oracle",
                ErrorClass::Transient,
                format!("no {} row", r.key),
            ));
        }
        return r;
    };
    r.status = Some(obs.status);
    r.age_ms = Some(obs.age_ms(now_ms));
    if obs.key != r.key {
        if r.needed {
            r.error = Some(ReadError::new(
                "oracle",
                ErrorClass::Fatal,
                format!("oracle row {} is not {}", obs.key, r.key),
            ));
        }
        return r;
    }
    match obs.typed::<OraclePrice>() {
        Err(e) => {
            r.error = Some(ReadError::new(
                "oracle",
                ErrorClass::Decode,
                format!("{e:#}"),
            ));
        }
        Ok(p) => {
            r.usd = p.usd.filter(|u| u.is_finite() && *u > 0.0);
            r.source = p.source;
            r.degraded = p.degraded || r.usd.is_none();
            r.move_5m_pct = p.move_5m_pct;
            if r.usd.is_none() && r.needed {
                let mut e = p.errors().into_iter().next().unwrap_or_else(|| {
                    ReadError::new("oracle", ErrorClass::Transient, "no usable price source")
                });
                e.field = "oracle".into();
                r.error = Some(e);
            }
        }
    }
    r
}

fn relabel<T: Clone>(f: &Field<T>, field: &str) -> Field<T> {
    match f {
        Field::Error { error } => Field::err(ReadError {
            field: field.to_string(),
            ..error.clone()
        }),
        other => other.clone(),
    }
}

fn balance_field(inv: &WalletInventory, mint: &str, field: &str) -> Field<TokenAmount> {
    let Ok(m) = mint.parse::<Pubkey>() else {
        return Field::err(ReadError::new(
            field,
            ErrorClass::Decode,
            format!("mint {mint} is not a pubkey"),
        ));
    };
    match inv.balance(&m) {
        Some(f) => relabel(f, field),
        None => Field::err(ReadError::new(
            field,
            ErrorClass::NotApplicable,
            format!("mint {mint} was not requested from solana_wallet"),
        )),
    }
}

fn finite(x: f64) -> Option<f64> {
    x.is_finite().then_some(x)
}

impl LpSnapshot {
    /// Record the keeper request `lp_state` points at (the glue reads it
    /// with `perps::request_status`); adds its PDA to `watch`.
    pub(crate) fn set_pending_request(&mut self, request: &Pubkey, status: Field<RequestStatus>) {
        self.hedge.pending_request = Some(relabel(&status, "hedge.pending_request"));
        let key = request.to_string();
        if let Err(i) = self.watch.binary_search(&key) {
            self.watch.insert(i, key);
        }
    }

    /// Price the LP logic runs on, quote per base: the oracle for a
    /// USDC-quoted pool, else the pool's active price
    /// (`getCyclePrice`, `autoTuneOrchestrator.ts:1505-1528`).
    pub(crate) fn cycle_price(&self) -> Option<f64> {
        if self.oracle.needed {
            self.oracle.usd
        } else {
            Some(self.pool_price)
        }
    }

    /// Wallet base units: native SOL for a SOL base, else the base ATA.
    pub(crate) fn wallet_base_units(&self) -> Option<f64> {
        if self.pair.base_is_native_sol {
            self.wallet_balances.native_sol.value().copied()
        } else {
            self.wallet_balances.base.value().map(|a| a.ui)
        }
    }

    /// Wallet quote units: native SOL for a SOL quote, else the quote ATA.
    pub(crate) fn wallet_quote_units(&self) -> Option<f64> {
        if self.pair.quote_is_native_sol {
            self.wallet_balances.native_sol.value().copied()
        } else {
            self.wallet_balances.quote.value().map(|a| a.ui)
        }
    }

    fn in_range_count(&self) -> usize {
        self.positions.iter().filter(|p| p.in_range).count()
    }

    fn perps_label(&self) -> &'static str {
        match (&self.hedge.long, &self.hedge.short) {
            (Field::Error { .. }, _) | (_, Field::Error { .. }) => {
                if self.hedge.applicable {
                    "error"
                } else {
                    "n/a"
                }
            }
            (Field::Ok { .. }, Field::Ok { .. }) => "both",
            (Field::Ok { .. }, _) => "long",
            (_, Field::Ok { .. }) => "short",
            _ => "flat",
        }
    }

    fn field_errors(&self) -> Vec<ReadError> {
        let mut out = self.errors.clone();
        let w = &self.wallet_balances;
        out.extend(w.native_sol.error().cloned());
        out.extend(w.base.error().cloned());
        out.extend(w.quote.error().cloned());
        out.extend(w.wsol.error().cloned());
        if self.oracle.needed {
            out.extend(self.oracle.error.clone());
        }
        if self.hedge.applicable {
            let h = &self.hedge;
            out.extend(h.long.error().cloned());
            out.extend(h.short.error().cloned());
            out.extend(h.sol_custody.error().cloned());
            out.extend(h.usdc_custody.error().cloned());
            out.extend(h.max_request_execution_sec.error().cloned());
            if let Some(f) = &h.pending_request {
                out.extend(f.error().cloned());
            }
        }
        out
    }
}

/// `x` with `sig` significant digits, plain notation (exponent outside
/// 1e-9..1e15).
fn fmt_sig(x: f64, sig: i32) -> String {
    if !x.is_finite() || x == 0.0 {
        return format!("{x}");
    }
    let mag = x.abs().log10().floor() as i32;
    if !(-9..15).contains(&mag) {
        let digits = (sig - 1).max(0) as usize;
        return format!("{x:.digits$e}");
    }
    let decimals = (sig - 1 - mag).max(0) as usize;
    format!("{x:.decimals$}")
}

fn regime_str(r: LpRegime) -> &'static str {
    match r {
        LpRegime::Below => "below",
        LpRegime::In => "in",
        LpRegime::Above => "above",
    }
}

impl Observed for LpSnapshot {
    const SCHEMA: &'static str = "lp_snapshot/1";

    fn subject(&self) -> String {
        format!("{}:{}", self.wallet, self.pool)
    }

    /// `lp_snapshot <wallet> <pool> px=<pool price> usd=<oracle> pos=n in_range=k perps=…`
    /// (≤ 165 chars for 44-char ids and 12-char prices).
    fn headline(&self) -> String {
        let oracle = self
            .oracle
            .usd
            .map(|u| fmt_sig(u, 6))
            .unwrap_or_else(|| "none".into());
        format!(
            "lp_snapshot {} {} px={} usd={oracle} pos={} in_range={} perps={}",
            self.wallet,
            self.pool,
            fmt_sig(self.pool_price, 6),
            self.positions.len(),
            self.in_range_count(),
            self.perps_label(),
        )
    }

    fn features(&self) -> Features {
        let mut f = Features::new();
        let n = self.positions.len();
        set_num(&mut f, "pool_price", Some(self.pool_price));
        set_int(&mut f, "active_id", Some(i64::from(self.active_id)));
        set_num(&mut f, "oracle_usd", self.oracle.usd);
        set_num(
            &mut f,
            "oracle_age_s",
            self.oracle.age_ms.map(|a| a as f64 / 1000.0),
        );
        set_bool(&mut f, "oracle_degraded", Some(self.oracle.degraded));
        set_num(&mut f, "pool_vs_oracle_bps", self.pool_vs_oracle_bps);
        set_str(
            &mut f,
            "discovery",
            Some(match self.discovery {
                Discovery::Found { .. } => "found",
                Discovery::Empty { .. } => "empty",
                Discovery::Error { .. } => "error",
            }),
        );
        set_int(&mut f, "position_count", Some(n as i64));
        set_bool(
            &mut f,
            "in_range",
            (n > 0).then(|| self.in_range_count() == n),
        );
        set_int(
            &mut f,
            "bins_to_lower_min",
            self.positions
                .iter()
                .map(|p| i64::from(p.bins_below_active))
                .min(),
        );
        set_int(
            &mut f,
            "bins_to_upper_min",
            self.positions
                .iter()
                .map(|p| i64::from(p.bins_above_active))
                .min(),
        );
        if let Some(e) = self.exposure.value() {
            set_num(&mut f, "lp_base", Some(e.base));
            set_num(&mut f, "lp_quote", Some(e.quote));
            set_num(&mut f, "lp_value_quote", Some(e.value_quote));
            set_num(
                &mut f,
                "claimable_value_quote",
                Some(e.claimable_base * self.pool_price + e.claimable_quote),
            );
            set_num(&mut f, "lp_full_value_base", Some(e.full_value_base));
            set_num(
                &mut f,
                "base_pct_value",
                (e.value_quote > 0.0).then(|| e.base * self.pool_price / e.value_quote * 100.0),
            );
        }
        let h = &self.hedge;
        let base_sol = |s: &Field<PerpSide>| match s {
            Field::Ok { value } => Some(value.base_sol),
            Field::Absent => Some(0.0),
            Field::Error { .. } => None,
        };
        set_num(&mut f, "perp_long_sol", base_sol(&h.long));
        set_num(&mut f, "perp_short_sol", base_sol(&h.short));
        if !h.long.is_error() && !h.short.is_error() {
            let notional = h.long.value().map_or(0.0, |s| s.notional_usd)
                + h.short.value().map_or(0.0, |s| s.notional_usd);
            set_num(&mut f, "perp_notional_usd", Some(notional));
        }
        set_num(&mut f, "collateral_ratio", h.collateral_ratio);
        set_num(
            &mut f,
            "liq_distance_min",
            [h.long.value(), h.short.value()]
                .into_iter()
                .flatten()
                .filter_map(|s| s.liq_distance_ratio)
                .reduce(f64::min),
        );
        set_num(
            &mut f,
            "carry_long_bps",
            h.sol_custody.value().map(|c| c.borrow_apr_pct * 100.0),
        );
        set_num(
            &mut f,
            "carry_short_bps",
            h.usdc_custody.value().map(|c| c.borrow_apr_pct * 100.0),
        );
        let w = &self.wallet_balances;
        set_num(&mut f, "wallet_sol", w.native_sol.value().copied());
        set_num(&mut f, "wallet_quote", self.wallet_quote_units());
        set_num(&mut f, "wallet_wsol", w.wsol.value().map(|a| a.ui));
        set_bool(&mut f, "hedge_applicable", Some(h.applicable));
        set_bool(&mut f, "both_sides_open", Some(h.both_sides_open));
        set_bool(
            &mut f,
            "pending_request",
            match &h.pending_request {
                None => Some(false),
                Some(Field::Ok { value }) => Some(value.exists && !value.executed),
                Some(Field::Absent) => Some(false),
                Some(Field::Error { .. }) => None,
            },
        );
        set_int(
            &mut f,
            "n_invalid_fields",
            Some(self.field_errors().len() as i64),
        );
        f
    }

    fn slot(&self) -> Option<u64> {
        Some(self.slot)
    }

    /// `Partial` when any read failed (the decisions gate on the fields);
    /// never `Error`: the glue returns an `Error` row itself when the pool
    /// cannot be read at all.
    fn status(&self) -> ObsStatus {
        if self.field_errors().is_empty() {
            ObsStatus::Ok
        } else {
            ObsStatus::Partial
        }
    }

    fn errors(&self) -> Vec<ReadError> {
        self.field_errors()
    }
}

// ---------------------------------------------------------------------------
// Knobs (ALL fields required: no serde defaults, no hidden code defaults)
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
    fn as_str(self) -> &'static str {
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
fn check_knob(errs: &mut Vec<String>, name: &str, v: f64, min: Option<f64>, max: Option<f64>) {
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

fn parse_knobs<T: serde::de::DeserializeOwned>(
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

// ---------------------------------------------------------------------------
// lp_state/1 — controller state
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
            updated_at_ms: 0,
        }
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
        set_int(&mut f, "updated_at_ms", Some(self.updated_at_ms));
        f
    }
}

// ---------------------------------------------------------------------------
// hedge_decide/1
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
    fn of_core_reason(reason: &str) -> Guard {
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
        }
    }
}

/// Critical fields for the hedge; a non-empty list blocks `InvalidRead`.
fn hedge_invalid_fields(s: &LpSnapshot) -> Vec<String> {
    let mut v = Vec::new();
    if matches!(s.discovery, Discovery::Error { .. }) {
        v.push("discovery");
    }
    if s.exposure.value().is_none() {
        v.push("exposure");
    }
    if s.positions.iter().any(|p| !p.complete) {
        v.push("positions.complete");
    }
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
fn stale_reason(snap: &LpSnapshot, snap_age_ms: u64, max_age_secs: u64) -> Option<String> {
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

fn divergence_reason(snap: &LpSnapshot, max_bps: f64) -> Option<String> {
    let bps = snap.pool_vs_oracle_bps?;
    (bps.abs() > max_bps)
        .then(|| format!("pool price vs oracle {bps:+.1} bps exceeds max_divergence_bps {max_bps}"))
}

/// View + exact core input (`jupiterPerpsEngine.ts:840-913`,
/// `autoTuneOrchestrator.ts:1104-1180`). Callers guarantee the critical
/// fields are readable (`hedge_invalid_fields` is empty).
fn hedge_view(
    snap: &LpSnapshot,
    knobs: &HedgeKnobs,
    state: &LpControllerState,
    price: f64,
    now_ms: i64,
) -> (HedgeView, HedgeInput) {
    let (live, quote) = snap
        .exposure
        .value()
        .map_or((0.0, 0.0), |e| (e.base, e.quote));
    let (used, regime, computed, pending, outcome) = match knobs.lp_input {
        LpInput::Live => (
            live,
            state.committed_regime,
            None,
            state.pending_regime,
            None,
        ),
        LpInput::Midpoint => {
            let c = hedge::lp_hedge_delta(live, quote, price, state.committed_regime);
            let rc = gates::regime_confirm(&RegimeConfirmInput {
                now_ms,
                committed: state.committed_regime,
                computed: c.regime,
                pending: state.pending_regime,
                confirm_ms: HEDGE_REGIME_CONFIRM_MS,
                storm_active: state.storm_active,
                imbalance_pending: state.imbalance_since_ms.is_some(),
                last_rebalance_failed: state.last_rebalance_failed_at_ms.is_some(),
            });
            let used = hedge::lp_delta_for_regime(rc.committed, live, quote, price);
            (
                used,
                rc.committed,
                Some(c.regime),
                rc.pending,
                Some(rc.outcome),
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
fn guard_trace(input: &HedgeInput, action: &HedgeAction) -> GuardTrace {
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

/// `hedge_decide`: tengu validity gates, then the verbatim
/// [`hedge::decide`], then the venue-permission check. Returns the decision
/// and the next controller state (unchanged when a gate fired before the
/// core; the caller persists it only with `commit = true`).
pub(crate) fn decide_hedge(
    snap: &LpSnapshot,
    snap_meta: &ObsMeta,
    snap_age_ms: u64,
    knobs: &HedgeKnobs,
    state: &LpControllerState,
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
    };
    let blocked = |guard: Guard, reason: String| HedgeAction::Blocked { reason, guard };

    // 1-3: gates that need no view.
    let invalid = hedge_invalid_fields(snap);
    if !invalid.is_empty() {
        out.action = blocked(
            Guard::InvalidRead,
            format!("critical reads failed: {}", invalid.join(", ")),
        );
        out.trace.invalid_fields = invalid;
        return (out, state.clone());
    }
    if knobs.include_wallet_sol && snap.wallet_balances.native_sol.value() == Some(&0.0) {
        out.action = blocked(
            Guard::WalletSolZero,
            "wallet native SOL reads exactly 0 with include_wallet_sol (BUG-023: never hedge a read that may have failed)".into(),
        );
        return (out, state.clone());
    }
    if !snap.hedge.applicable {
        out.action = blocked(
            Guard::NotApplicable,
            format!(
                "the SOL perps hedge needs a native-SOL base and a USDC quote; pair is {} / {}",
                snap.pair.base_mint, snap.pair.quote_mint
            ),
        );
        return (out, state.clone());
    }

    let price = snap.oracle.usd.unwrap_or(f64::NAN);
    let (view, input) = hedge_view(snap, knobs, state, price, now_ms);
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
        return (out, state.clone());
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

    let mut next = state.clone();
    next.committed_regime = view.regime;
    next.pending_regime = view.pending_regime;
    next.updated_at_ms = now_ms;
    (out, next)
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

// ---------------------------------------------------------------------------
// lp_decide/1
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
fn lp_invalid_fields(s: &LpSnapshot) -> Vec<String> {
    let mut v = Vec::new();
    if matches!(s.discovery, Discovery::Error { .. }) {
        v.push("discovery");
    }
    if s.exposure.value().is_none() {
        v.push("exposure");
    }
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

fn position_health(
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

fn wallet_split(snap: &LpSnapshot, knobs: &LpKnobs, price: Option<f64>) -> Option<WalletSplit> {
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

fn range_plan(snap: &LpSnapshot, bin_count: u32) -> Option<RangePlan> {
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
fn open_or_blocked(
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
    if !(s.total_value_quote > 0.0) {
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
    let storm_out = gates::storm_update(&StormInput {
        now_ms,
        price: cycle_price.unwrap_or(f64::NAN),
        samples: if snap.oracle.needed {
            price_samples.to_vec()
        } else {
            Vec::new()
        },
        threshold_pct: knobs.storm_pct_5m,
        active: state.storm_active,
    });
    let storm = StormState {
        move_5m_pct: storm_out.move_5m_pct,
        active: storm_out.active,
        threshold_pct: knobs.storm_pct_5m,
    };
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

    // Validity gates: state unchanged.
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
        return (out, state.clone());
    }

    let price = cycle_price.unwrap_or(f64::NAN);
    let mut next = state.clone();
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

#[cfg(test)]
mod tests {
    //! Fixtures: `tests/fixtures/solana/{dlmm,perps,wallet,market}` (stage 2,
    //! live mainnet captures) decoded through the real builders.

    use std::collections::BTreeMap;
    use std::sync::OnceLock;

    use serde_json::json;

    use super::*;
    use crate::domain::lp::dlmm::{self, DiscoverySource};
    use crate::domain::lp::{market, perps, wallet};
    use crate::domain::observation::{
        assert_features_ok, ObsSource, MAX_FEATURES, MAX_LINE1_CHARS,
    };
    use crate::domain::solana::{AccountRead, AccountSet, AccountState};

    macro_rules! fixture {
        ($rel:literal) => {
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/solana/",
                $rel
            ))
        };
    }

    const WALLET: &str = "F3YvPiLdniRPGpeKrbeGWR2zg2wPpzVuvqBA5BBJBQ5S";
    const POOL: &str = "5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS6";
    /// Fixture position (bins -5440..-5371, active -5373: in range, near
    /// the upper edge), re-owned by `WALLET` for the composed snapshot.
    const POSITION: &str = "H9fmcxgheDvVSn9iUeRSvZPAgTY5WXqvroNpkZ2HCVRW";
    /// Owner of the three standard positions in the dlmm fixture.
    const FIXTURE_OWNER: &str = "JBggt27MzM4eohjumT9Tuec7MBoWAgDM4BJjkoisDUcs";
    const FLAT_LONG: &str = "FqymRcB92t63jpwh7om4RLbxMNUGoHnZPQMkkAA8ksVY";
    const FLAT_SHORT: &str = "6HFhuYzQGcqdj4NGwC6vfVETRvMA3pXaVeZnHgWSKsJK";
    const BOTH_WALLET: &str = "2xxyBSRyi1KVxwuZcFkU74c8HvhjBdV8YJ6F4gkdKk3i";
    const BOTH_LONG: &str = "2DNqvKcgnx5Huk6VkjeoZbjhna6hZd7GRuG8RCH2dPEV";
    const BOTH_SHORT: &str = "HCZsYUEGtiGVvFJJYcuqhrq1L2MQ7GwWXmQRQSxFNWWX";
    const ATA_USDC: &str = "D9ScKYy15cw1tpkkuwEnDKv62nCyuETwrvRSdP4usGg1";
    const ATA_WSOL: &str = "E4PCnfEconGJW6vf7GDycEnkFe1VWC7teiNWJzQv3NTA";
    const REQUEST: &str = "11q9teW5JiHhWeY8ak79i72C4qpDtppzVgH1ZeEUWp3";
    /// A 44-char pool id for the max-length rendering checks.
    const ID_44: &str = "G18jKKXQwBbrHeiK3C9MRXhkHsLHf7XgCSisykV46EZa";
    /// dlmm fixture clock (`golden.json` `clock_unix_timestamp`), ms.
    const NOW: i64 = 1_790_272_406_000;
    /// perps fixture wall clock at capture.
    const PERPS_NOW_S: i64 = 1_790_272_211;
    const SOL_USD_AT_PERPS: f64 = 116.658_916_455_958_6;

    fn pk(s: &str) -> Pubkey {
        s.parse().unwrap()
    }

    fn json_of(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    /// One raw getMultipleAccounts response + its request key order.
    fn gma_set(gma: &str, keys: &[String]) -> AccountSet {
        let v = json_of(gma);
        let slot = v["result"]["context"]["slot"].as_u64().unwrap();
        let values = v["result"]["value"].as_array().unwrap();
        assert_eq!(keys.len(), values.len());
        let mut set = AccountSet::default();
        for (k, a) in keys.iter().zip(values) {
            let state = if a.is_null() {
                AccountState::Absent
            } else {
                AccountState::Ok {
                    owner: pk(a["owner"].as_str().unwrap()),
                    lamports: a["lamports"].as_u64().unwrap(),
                    data_b64: a["data"][0].as_str().unwrap().to_string(),
                    executable: a["executable"].as_bool().unwrap(),
                }
            };
            set.insert(AccountRead {
                pubkey: pk(k),
                slot,
                state,
            });
        }
        set
    }

    fn strings(v: &Value) -> Vec<String> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|k| k.as_str().unwrap().to_string())
            .collect()
    }

    fn dlmm_set() -> AccountSet {
        let meta = json_of(fixture!("dlmm/meta.json"));
        gma_set(fixture!("dlmm/gma.json"), &strings(&meta["keys"]))
    }

    fn dlmm_array_keys() -> Vec<(i64, Pubkey)> {
        let meta = json_of(fixture!("dlmm/meta.json"));
        meta["roles"]["bin_array_indexes"]
            .as_array()
            .unwrap()
            .iter()
            .zip(strings(&meta["roles"]["bin_arrays"]))
            .map(|(i, k)| (i.as_i64().unwrap(), pk(&k)))
            .collect()
    }

    /// The dlmm fixture with `POSITION` re-owned by `WALLET` (owner @40).
    fn owned_dlmm_set() -> AccountSet {
        let mut set = dlmm_set();
        let read = set.get(&pk(POSITION)).unwrap().clone();
        let mut data = read.data().unwrap();
        data[40..72].copy_from_slice(&pk(WALLET).0);
        set.insert(AccountRead::from_bytes(
            pk(POSITION),
            read.slot,
            *read.owner().unwrap(),
            read.lamports().unwrap(),
            &data,
        ));
        set
    }

    fn perps_set() -> AccountSet {
        let meta = json_of(fixture!("perps/meta.json"));
        let keys: Vec<String> = meta["gma"]["keys"]
            .as_array()
            .unwrap()
            .iter()
            .map(|k| k["pubkey"].as_str().unwrap().to_string())
            .collect();
        gma_set(fixture!("perps/gma.json"), &keys)
    }

    fn wallet_set() -> AccountSet {
        let meta = json_of(fixture!("wallet/meta.json"));
        gma_set(
            fixture!("wallet/gma_base64.json"),
            &strings(&meta["files"]["gma_base64.json"]["keys"]),
        )
    }

    fn pool_state(set: &AccountSet) -> DlmmPoolState {
        dlmm::build_dlmm_pool(set, &pk(POOL), &dlmm_array_keys(), NOW).unwrap()
    }

    fn positions_of(set: &AccountSet, wallet: &str) -> DlmmPositions {
        dlmm::build_positions(
            set,
            &pk(wallet),
            &pk(POOL),
            Discovery::Found {
                count: 1,
                source: DiscoverySource::Gpa,
                at_ms: NOW,
            },
        )
        .unwrap()
    }

    fn perps_flat() -> PerpsState {
        perps::build_perps(
            &perps_set(),
            &pk(WALLET),
            &pk(FLAT_LONG),
            &pk(FLAT_SHORT),
            Some(SOL_USD_AT_PERPS),
            PERPS_NOW_S,
        )
    }

    fn wallet_inv() -> WalletInventory {
        let set = wallet_set();
        let w = pk(WALLET);
        let token = ids::key(ids::TOKEN);
        let atas = [
            (ids::key(ids::WSOL), pk(ATA_WSOL), token),
            (ids::key(ids::USDC), pk(ATA_USDC), token),
        ];
        let balances = wallet::build_wallet_balances(&set, &w, &atas, &BTreeMap::new());
        wallet::build_wallet_inventory(
            w,
            set.slot_max,
            wallet::lamports_from_set(&set, &w),
            Field::Absent,
            Field::Absent,
            balances,
        )
    }

    /// `price_oracle/1:<wSOL>` built from the Jupiter fixture at `at_ms`.
    fn oracle_obs(at_ms: i64) -> Observation {
        let v = json_of(fixture!("market/jupiter_price_v3.json"));
        let jup = market::parse_jupiter_price(&v, ids::WSOL);
        let p = market::combine_price(ids::WSOL, jup, Field::Absent, None, None, at_ms);
        Observation::of(
            "sol_price",
            &p,
            at_ms,
            market::PRICE_TTL_MS,
            ObsSource::Live,
        )
    }

    /// The composed fixture snapshot: `WALLET` with one in-range position,
    /// a flat hedge, 2.748145289 SOL + 107.808931 USDC, a 2 s old price.
    fn composed() -> LpSnapshot {
        static SNAP: OnceLock<LpSnapshot> = OnceLock::new();
        SNAP.get_or_init(|| {
            let set = owned_dlmm_set();
            compose_snapshot(
                &pk(WALLET),
                &pool_state(&set),
                &positions_of(&set, WALLET),
                Some(&perps_flat()),
                &wallet_inv(),
                Some(&oracle_obs(NOW - 2_000)),
                NOW,
            )
        })
        .clone()
    }

    fn meta_of(s: &LpSnapshot, age_ms: u64) -> ObsMeta {
        Observation::of(
            "lp_snapshot",
            s,
            NOW - age_ms as i64,
            LP_SNAPSHOT_TTL_MS,
            ObsSource::Live,
        )
        .meta(NOW)
    }

    /// Bot code defaults (`env.ts:336-454`) except a 0.5 SOL fixed band so
    /// the fixture LP (1.86 SOL) is out of band.
    fn hedge_knobs() -> HedgeKnobs {
        HedgeKnobs {
            target_delta_sol: 0.0,
            delta_threshold_sol: 0.5,
            band_bins: 0,
            bin_count: 20,
            cap_mult: 1.25,
            max_notional_usd: 0.0,
            min_collateral_ratio: 0.15,
            target_collateral_ratio: 1.0,
            carry_cap_bps: 5000.0,
            cooldown_ms: 600_000,
            lp_input: LpInput::Live,
            include_wallet_sol: false,
            min_wallet_sol: 0.2,
            rent_reserve_sol: 0.1,
            max_divergence_bps: 50.0,
            max_snapshot_age_secs: 30,
        }
    }

    fn lp_knobs() -> LpKnobs {
        LpKnobs {
            imbalance_threshold: 0.9,
            bin_count: 20,
            storm_pct_5m: 0.0,
            trend_confirm_ms: 0,
            reentry_confirm_ms: 0,
            reentry_tol_frac: 0.15,
            max_divergence_bps: 50.0,
            max_snapshot_age_secs: 30,
            min_wallet_sol: 0.2,
            rent_reserve_sol: 0.1,
        }
    }

    fn fresh_state() -> LpControllerState {
        LpControllerState::new(WALLET, POOL)
    }

    fn hedge(
        s: &LpSnapshot,
        k: &HedgeKnobs,
        st: &LpControllerState,
    ) -> (HedgeDecision, LpControllerState) {
        decide_hedge(s, &meta_of(s, 1_000), 1_000, k, st, NOW)
    }

    fn lp(
        s: &LpSnapshot,
        k: &LpKnobs,
        st: &LpControllerState,
        samples: &[PriceSample],
    ) -> (LpDecision, LpControllerState) {
        decide_lp(s, &meta_of(s, 1_000), 1_000, k, st, samples, NOW)
    }

    fn err(field: &str) -> ReadError {
        ReadError::new(field, ErrorClass::Timeout, "rpc timed out")
    }

    fn guard_of(d: &HedgeDecision) -> Option<Guard> {
        d.action.guard()
    }

    fn check_observed<T: Observed>(v: &T) {
        let f = v.features();
        assert_features_ok(&f);
        assert!(f.len() <= MAX_FEATURES, "{} features", f.len());
        let o = Observation::of("test", v, NOW, 5_000, ObsSource::Live);
        let text = o.render_text(NOW + 12_345);
        let line1 = text.lines().next().unwrap();
        assert!(
            line1.chars().count() <= MAX_LINE1_CHARS,
            "{} chars: {line1}",
            line1.chars().count()
        );
        assert!(
            v.headline().chars().count() <= 165,
            "headline {} chars: {}",
            v.headline().chars().count(),
            v.headline()
        );
        assert!(o.typed::<T>().is_ok(), "round trip {}", T::SCHEMA);
    }

    // ── composition ────────────────────────────────────────────────────

    #[test]
    fn composes_the_fixture_snapshot_through_the_real_builders() {
        let s = composed();
        assert_eq!(s.wallet, WALLET);
        assert_eq!(s.pool, POOL);
        assert!(s.pair.base_is_native_sol && s.pair.quote_is_usd);
        assert_eq!(s.pair.base_mint, ids::WSOL);
        assert_eq!(s.pair.quote_mint, ids::USDC);
        assert!(s.pool_enabled);
        assert_eq!(s.active_id, -5373);
        assert_eq!(s.bin_step, 4);
        assert!((s.pool_price - 116.627_489_413_434_7).abs() < 1e-9);
        let jup_usd = 116.608_416_065_151_2;
        // Oracle: the Jupiter fixture row, 2 s old at compose time.
        assert_eq!(s.oracle.key, format!("price_oracle/1:{}", ids::WSOL));
        assert!((s.oracle.usd.unwrap() - jup_usd).abs() < 1e-12);
        assert_eq!(s.oracle.age_ms, Some(2_000));
        assert_eq!(s.oracle.source, Some(PriceSource::Jupiter));
        assert!(s.oracle.degraded, "single source");
        let bps = s.pool_vs_oracle_bps.unwrap();
        let want = (116.627_489_413_434_7 / jup_usd - 1.0) * 1e4;
        assert!((bps - want).abs() < 1e-6, "{bps} vs {want}");
        // Wallet.
        let w = &s.wallet_balances;
        assert_eq!(w.native_sol.value(), Some(&2.748_145_289));
        assert_eq!(w.quote.value().unwrap().raw, "107808931");
        assert_eq!(w.wsol.value().unwrap().raw, "0", "wSOL ATA absent = 0");
        assert_eq!(w.base, w.wsol, "SOL pair: base mint is wSOL");
        // Position + exposure.
        assert!(matches!(s.discovery, Discovery::Found { count: 1, .. }));
        assert_eq!(s.positions.len(), 1);
        let p = &s.positions[0];
        assert_eq!(p.position, POSITION);
        assert!(p.in_range && p.complete);
        assert_eq!((p.lower_bin_id, p.upper_bin_id), (-5440, -5371));
        let e = s.exposure.value().unwrap();
        assert!((e.base - 1.863_240_114).abs() < 1e-12);
        assert!((e.quote - 14_782.778_388).abs() < 1e-9);
        // Hedge: flat, both custodies read.
        assert!(s.hedge.applicable);
        assert_eq!(s.hedge.long, Field::Absent);
        assert_eq!(s.hedge.short, Field::Absent);
        assert!(s.hedge.sol_custody.value().is_some());
        assert!(s.hedge.usdc_custody.value().is_some());
        assert_eq!(s.hedge.max_request_execution_sec.value(), Some(&45));
        assert!(s.anomalies.is_empty(), "{:?}", s.anomalies);
        // Slot range spans the three captures.
        assert_eq!(s.slot, 450_101_361, "perps capture is the oldest");
        assert_eq!(s.slot_max, 450_102_095, "dlmm capture is the newest");
        assert_eq!(s.status(), ObsStatus::Ok, "{:?}", s.errors());
    }

    #[test]
    fn watch_is_the_union_of_every_key_read() {
        let s = composed();
        for key in [
            POOL,
            ids::WSOL,
            ids::USDC,
            "EYj9xKw6ZszwpyNibHY7JD5o3QgTVrSdcBp1fMJhrR9o",
            "CoaxzEh8p5YyGLcj36Eo3cUThVJxeKCs7qvLAGDYwBcz",
            POSITION,
            // Bin arrays -78 / -77 hold the position and the depth bands.
            "HP15ZCgcgunsV9ypHKpDJSURFn4dn7i63uCNB4k7K5MV",
            "Vc6P6kjaRUgnQCwycL4QzTG3tiNR4CgC3gqWHvvMMJh",
            FLAT_LONG,
            FLAT_SHORT,
            ids::JUP_CUSTODY_SOL,
            ids::JUP_CUSTODY_USDC,
            ids::JLP_POOL,
            WALLET,
            ATA_USDC,
            ATA_WSOL,
        ] {
            assert!(s.watch.contains(&key.to_string()), "watch lacks {key}");
        }
        let mut sorted = s.watch.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted, s.watch, "sorted + unique");
        assert!(s.watch.iter().all(|k| k.parse::<Pubkey>().is_ok()));
    }

    #[test]
    fn snapshot_observation_fits_the_render_and_feature_contract() {
        let s = composed();
        check_observed(&s);
        let h = s.headline();
        assert!(h.contains(WALLET) && h.contains(POOL), "{h}");
        assert!(
            h.contains("perps=flat") && h.contains("pos=1 in_range=1"),
            "{h}"
        );
        let o = Observation::of("lp_snapshot", &s, NOW, LP_SNAPSHOT_TTL_MS, ObsSource::Live);
        assert_eq!(o.key, format!("lp_snapshot/1:{WALLET}:{POOL}"));
        assert_eq!(o.slot, Some(450_101_361));
        let back: LpSnapshot = o.typed().unwrap();
        assert_eq!(back.positions.len(), 1);
        assert_eq!(back.watch, s.watch);
        let f = &o.features;
        assert_eq!(f["hedge_applicable"], json!(true));
        assert_eq!(f["perp_short_sol"], json!(0.0));
        assert_eq!(f["n_invalid_fields"], json!(0));
        assert!(
            f.get("collateral_ratio").is_none(),
            "flat: omitted, never Infinity"
        );
        // Max-length ids still fit line 1.
        let mut long = s.clone();
        long.pool = ID_44.into();
        long.oracle.usd = Some(0.000_012_345_678_9);
        long.pool_price = 123_456_789.123;
        check_observed(&long);
    }

    #[test]
    fn missing_or_foreign_oracle_is_a_partial_read() {
        let set = owned_dlmm_set();
        let (pool, pos, perps, inv) = (
            pool_state(&set),
            positions_of(&set, WALLET),
            perps_flat(),
            wallet_inv(),
        );
        let none = compose_snapshot(&pk(WALLET), &pool, &pos, Some(&perps), &inv, None, NOW);
        assert_eq!(none.oracle.usd, None);
        assert_eq!(none.pool_vs_oracle_bps, None);
        assert_eq!(none.status(), ObsStatus::Partial);
        assert!(none.errors().iter().any(|e| e.field == "oracle"));
        // A USDC price row is not the base's oracle.
        let v = json_of(fixture!("market/jupiter_price_v3.json"));
        let usdc = market::combine_price(
            ids::USDC,
            market::parse_jupiter_price(&v, ids::USDC),
            Field::Absent,
            None,
            None,
            NOW,
        );
        let usdc_obs = Observation::of("sol_price", &usdc, NOW, 10_000, ObsSource::Live);
        let foreign = compose_snapshot(
            &pk(WALLET),
            &pool,
            &pos,
            Some(&perps),
            &inv,
            Some(&usdc_obs),
            NOW,
        );
        assert_eq!(foreign.oracle.usd, None);
        let e = foreign.oracle.error.as_ref().unwrap();
        assert_eq!(e.class, ErrorClass::Fatal);
        assert!(
            e.message.contains(ids::USDC) && e.message.contains(ids::WSOL),
            "{}",
            e.message
        );
    }

    #[test]
    fn components_for_another_wallet_are_rejected_not_mixed() {
        let set = owned_dlmm_set();
        let pool = pool_state(&set);
        let foreign_pos = positions_of(&set, FIXTURE_OWNER);
        let s = compose_snapshot(
            &pk(WALLET),
            &pool,
            &foreign_pos,
            None,
            &wallet_inv(),
            Some(&oracle_obs(NOW)),
            NOW,
        );
        assert!(matches!(s.discovery, Discovery::Error { .. }));
        assert!(s.positions.is_empty() && s.exposure.is_error());
        // Perps not read on an applicable pool: sides are errors, not flat.
        assert_eq!(s.hedge.long.error().unwrap().class, ErrorClass::Fatal);
        assert_eq!(s.status(), ObsStatus::Partial);
        // Wallet inventory of another wallet.
        let mut inv = wallet_inv();
        inv.wallet = pk(FIXTURE_OWNER);
        let s = compose_snapshot(
            &pk(WALLET),
            &pool,
            &positions_of(&set, WALLET),
            Some(&perps_flat()),
            &inv,
            Some(&oracle_obs(NOW)),
            NOW,
        );
        assert!(s.wallet_balances.native_sol.is_error());
        assert!(s.wallet_balances.quote.is_error());
        assert!(!s.watch.contains(&WALLET.to_string()));
    }

    #[test]
    fn anomalies_map_from_every_component() {
        let set = dlmm_set();
        let pool = pool_state(&set);
        // Three standard positions for the fixture owner → MultiplePositions.
        let pos = positions_of(&set, FIXTURE_OWNER);
        let mut both = perps::build_perps(
            &perps_set(),
            &pk(BOTH_WALLET),
            &pk(BOTH_LONG),
            &pk(BOTH_SHORT),
            Some(SOL_USD_AT_PERPS),
            PERPS_NOW_S,
        );
        both.wallet = FIXTURE_OWNER.into();
        let mut inv = wallet_inv();
        inv.wallet = pk(FIXTURE_OWNER);
        inv.lamports = Field::ok(0);
        let s = compose_snapshot(
            &pk(FIXTURE_OWNER),
            &pool,
            &pos,
            Some(&both),
            &inv,
            Some(&oracle_obs(NOW)),
            NOW,
        );
        assert!(s
            .anomalies
            .contains(&Anomaly::MultiplePositions { count: 3 }));
        assert!(s.anomalies.contains(&Anomaly::BothSidesOpen));
        assert!(s.anomalies.contains(&Anomaly::WalletSolZero));
        assert!(s.hedge.collateral_ratio.is_some());
        check_observed(&s);
        // Extended position + disabled pool + not-applicable hedge.
        let mut pool2 = pool.clone();
        pool2.enabled = false;
        pool2.pair.quote_is_usd = false;
        let ext = positions_of(&set, "D5HPLvKdJHwKGaAkn99aFDj4BJSAbXyexKececgRQWss");
        let mut inv2 = wallet_inv();
        inv2.wallet = pk("D5HPLvKdJHwKGaAkn99aFDj4BJSAbXyexKececgRQWss");
        let s = compose_snapshot(
            &pk("D5HPLvKdJHwKGaAkn99aFDj4BJSAbXyexKececgRQWss"),
            &pool2,
            &ext,
            None,
            &inv2,
            None,
            NOW,
        );
        assert!(s.anomalies.contains(&Anomaly::ExtendedPosition {
            position: "14JU64KbNMLmFiS8qHuZ9ZF1swmzmiBYCRZidM24rH1m".into()
        }));
        assert!(s.anomalies.contains(&Anomaly::PoolDisabled));
        assert!(s.anomalies.contains(&Anomaly::HedgeNotApplicable));
        // Not applicable: missing perps / oracle do not make it Partial.
        assert!(!s.hedge.applicable && !s.oracle.needed);
        assert_eq!(s.status(), ObsStatus::Ok, "{:?}", s.errors());
        assert_eq!(s.cycle_price(), Some(s.pool_price));
        assert_eq!(s.perps_label(), "n/a");
    }

    #[test]
    fn pending_request_joins_the_watch_list_in_order() {
        let mut s = composed();
        s.set_pending_request(
            &pk(REQUEST),
            Field::ok(RequestStatus {
                position_request: REQUEST.into(),
                exists: true,
                executed: false,
                age_secs: Some(3),
                expired: false,
            }),
        );
        assert!(s.watch.contains(&REQUEST.to_string()));
        let mut sorted = s.watch.clone();
        sorted.sort();
        assert_eq!(sorted, s.watch);
        assert_eq!(s.features()["pending_request"], json!(true));
        s.set_pending_request(&pk(REQUEST), Field::err(err("x")));
        assert_eq!(
            s.hedge
                .pending_request
                .as_ref()
                .unwrap()
                .error()
                .unwrap()
                .field,
            "hedge.pending_request"
        );
        assert_eq!(s.status(), ObsStatus::Partial);
    }

    // ── hedge_decide: parity with the core ─────────────────────────────

    #[test]
    fn flat_hedge_in_range_position_matches_hedge_decide_exactly() {
        let s = composed();
        let k = hedge_knobs();
        let (d, next) = hedge(&s, &k, &fresh_state());
        // The equivalent input, assembled by hand (jupiterPerpsEngine.ts:840-913).
        let e = s.exposure.value().unwrap();
        let price = s.oracle.usd.unwrap();
        let full = e.base + e.quote / price;
        let sol_apr = s.hedge.sol_custody.value().unwrap().borrow_apr_pct;
        let usdc_apr = s.hedge.usdc_custody.value().unwrap().borrow_apr_pct;
        let usdc = s.wallet_balances.quote.value().unwrap().ui;
        assert!((usdc - 107.808_931).abs() < 1e-12);
        let expected = HedgeInput {
            lp_sol: e.base,
            long_sol: 0.0,
            short_sol: 0.0,
            long_notional_usd: 0.0,
            short_notional_usd: 0.0,
            long_collateral_usd: 0.0,
            short_collateral_usd: 0.0,
            carry_cost_bps_long: sol_apr * 100.0,
            carry_cost_bps_short: usdc_apr * 100.0,
            oracle_price_usd: Some(price),
            wallet_sol: 2_748_145_289.0 / 1e9,
            wallet_reserve_sol: 0.2 + 0.1,
            wallet_usdc: usdc,
            target_delta_sol: 0.0,
            band_sol: 0.5,
            carry_cap_bps: 5000.0,
            max_hedge_notional_usd: full * price * 1.25,
            min_collateral_ratio: 0.15,
            target_collateral_ratio: 1.0,
            now_ms: NOW,
            last_action_at_ms: None,
            cooldown_ms: 600_000,
        };
        assert_eq!(d.input.as_ref(), Some(&expected));
        let core = hedge::decide(&expected);
        assert_eq!(d.action, HedgeAction::from_core(&core));
        // Short 1.86 SOL wanted; the USDC wallet (107.81) caps it (BUG-013).
        match &d.action {
            HedgeAction::IncreaseShort {
                size_usd,
                collateral_tokens,
                adjust_sol,
            } => {
                assert!((size_usd - usdc).abs() < 1e-9);
                assert!((collateral_tokens - usdc).abs() < 1e-9);
                assert!((adjust_sol + usdc / price).abs() < 1e-12);
            }
            other => panic!("expected increase_short, got {other:?}"),
        }
        assert_eq!(d.trace.clamped_by, vec!["collateral".to_string()]);
        assert_eq!(d.trace.projected_ratio, Some(1.0));
        let v = d.view.as_ref().unwrap();
        assert_eq!(v.band_source, BandSource::Floor);
        assert_eq!(v.cap_source, CapSource::Auto);
        assert!(v.out_of_band && v.net_delta_sol == e.base);
        assert_eq!(next.updated_at_ms, NOW);
        assert_eq!(
            next.committed_regime,
            LpRegime::In,
            "live mode keeps the regime"
        );
        check_observed(&d);
        let o = Observation::of("hedge_decide", &d, NOW, 0, ObsSource::Live);
        assert_eq!(o.key, format!("hedge_decide/1:{WALLET}:{POOL}"));
        assert_eq!(o.features["action"], json!("increase_short"));
    }

    #[test]
    fn core_block_reasons_map_to_guards() {
        let base = HedgeInput {
            lp_sol: 10.0,
            long_sol: 0.0,
            short_sol: 0.0,
            long_notional_usd: 0.0,
            short_notional_usd: 0.0,
            long_collateral_usd: 0.0,
            short_collateral_usd: 0.0,
            carry_cost_bps_long: 1200.0,
            carry_cost_bps_short: 1200.0,
            oracle_price_usd: Some(100.0),
            wallet_sol: 10.0,
            wallet_reserve_sol: 0.3,
            wallet_usdc: 1_000_000.0,
            target_delta_sol: 0.0,
            band_sol: 0.5,
            carry_cap_bps: 5000.0,
            max_hedge_notional_usd: 12_000.0,
            min_collateral_ratio: 0.15,
            target_collateral_ratio: 1.0,
            now_ms: 0,
            last_action_at_ms: None,
            cooldown_ms: 0,
        };
        let cases: [(&str, HedgeInput, Guard); 5] = [
            (
                "oracle",
                HedgeInput {
                    oracle_price_usd: None,
                    ..base.clone()
                },
                Guard::Oracle,
            ),
            (
                "carry",
                HedgeInput {
                    carry_cost_bps_short: 6000.0,
                    ..base.clone()
                },
                Guard::Carry,
            ),
            (
                "headroom",
                HedgeInput {
                    max_hedge_notional_usd: 5.0,
                    ..base.clone()
                },
                Guard::Headroom,
            ),
            (
                "collateral",
                HedgeInput {
                    wallet_usdc: 5.0,
                    ..base.clone()
                },
                Guard::Collateral,
            ),
            (
                "ratio",
                HedgeInput {
                    min_collateral_ratio: 0.5,
                    target_collateral_ratio: 0.2,
                    ..base.clone()
                },
                Guard::CollateralRatio,
            ),
        ];
        for (name, input, want) in cases {
            let a = HedgeAction::from_core(&hedge::decide(&input));
            assert_eq!(a.guard(), Some(want), "{name}: {a:?}");
        }
        assert_eq!(Guard::of_core_reason("something new"), Guard::Core);
    }

    // ── hedge_decide: gate table ───────────────────────────────────────

    #[test]
    fn invalid_reads_block_before_anything_else() {
        type Mutate = fn(&mut LpSnapshot);
        let cases: [(&str, Mutate); 8] = [
            ("discovery", |s| {
                s.discovery = Discovery::Error {
                    error: err("discovery"),
                    fallback_count: 0,
                };
            }),
            ("exposure", |s| s.exposure = Field::err(err("exposure"))),
            ("positions.complete", |s| s.positions[0].complete = false),
            ("wallet.native_sol", |s| {
                s.wallet_balances.native_sol = Field::err(err("wallet.native_sol"))
            }),
            ("wallet.quote", |s| {
                s.wallet_balances.quote = Field::err(err("wallet.quote"))
            }),
            ("oracle.usd", |s| s.oracle.usd = None),
            ("hedge.short", |s| {
                s.hedge.short = Field::err(err("hedge.short"))
            }),
            ("hedge.usdc_custody", |s| {
                s.hedge.usdc_custody = Field::err(err("hedge.usdc_custody"))
            }),
        ];
        let state = fresh_state();
        for (field, mutate) in cases {
            let mut s = composed();
            mutate(&mut s);
            // Every later gate would also fire: InvalidRead wins.
            s.pool_vs_oracle_bps = Some(500.0);
            let (d, next) = decide_hedge(
                &s,
                &meta_of(&s, 99_000),
                99_000,
                &hedge_knobs(),
                &state,
                NOW,
            );
            assert_eq!(guard_of(&d), Some(Guard::InvalidRead), "{field}");
            assert!(
                d.trace.invalid_fields.contains(&field.to_string()),
                "{field}: {:?}",
                d.trace.invalid_fields
            );
            assert!(d.view.is_none() && d.input.is_none(), "{field}");
            assert_eq!(next, state, "{field}: state unchanged");
            check_observed(&d);
        }
        // Pending request unreadable, or its expiry unknown.
        let mut s = composed();
        s.set_pending_request(&pk(REQUEST), Field::err(err("req")));
        let (d, _) = hedge(&s, &hedge_knobs(), &state);
        assert_eq!(
            d.trace.invalid_fields,
            vec!["hedge.pending_request".to_string()]
        );
        let mut s = composed();
        s.hedge.max_request_execution_sec = Field::err(err("pool"));
        let (d, _) = hedge(&s, &hedge_knobs(), &state);
        assert!(
            guard_of(&d) != Some(Guard::InvalidRead),
            "no request: max exec unused"
        );
        s.set_pending_request(&pk(REQUEST), Field::ok(request(true, false, 1)));
        let (d, _) = hedge(&s, &hedge_knobs(), &state);
        assert_eq!(
            d.trace.invalid_fields,
            vec!["hedge.max_request_execution_sec".to_string()]
        );
    }

    fn request(exists: bool, executed: bool, age_secs: i64) -> RequestStatus {
        RequestStatus {
            position_request: REQUEST.into(),
            exists,
            executed,
            age_secs: exists.then_some(age_secs),
            expired: false,
        }
    }

    #[test]
    fn wallet_sol_zero_blocks_only_when_wallet_sol_is_hedged() {
        let mut s = composed();
        s.wallet_balances.native_sol = Field::ok(0.0);
        let mut k = hedge_knobs();
        k.include_wallet_sol = true;
        let (d, _) = hedge(&s, &k, &fresh_state());
        assert_eq!(guard_of(&d), Some(Guard::WalletSolZero));
        k.include_wallet_sol = false;
        let (d, _) = hedge(&s, &k, &fresh_state());
        assert_ne!(guard_of(&d), Some(Guard::WalletSolZero));
    }

    #[test]
    fn hedge_not_applicable_outside_sol_usdc() {
        let mut s = composed();
        s.hedge.applicable = false;
        s.hedge.long = Field::err(err("hedge.long"));
        s.oracle.usd = None;
        let (d, next) = hedge(&s, &hedge_knobs(), &fresh_state());
        assert_eq!(guard_of(&d), Some(Guard::NotApplicable));
        assert!(
            d.trace.invalid_fields.is_empty(),
            "perps/oracle not critical"
        );
        assert_eq!(next, fresh_state());
    }

    #[test]
    fn pending_request_blocks_until_max_execution_plus_grace() {
        // JLP pool max request execution = 45 s (fixture) + 15 s grace.
        let cases = [
            (request(true, false, 10), Some(Guard::PendingRequest)),
            (request(true, false, 59), Some(Guard::PendingRequest)),
            (request(true, false, 60), None),
            (request(true, true, 10), None),
            (request(false, false, 0), None),
        ];
        for (r, want) in cases {
            let mut s = composed();
            s.set_pending_request(&pk(REQUEST), Field::ok(r.clone()));
            let (d, next) = hedge(&s, &hedge_knobs(), &fresh_state());
            match want {
                Some(g) => {
                    assert_eq!(guard_of(&d), Some(g), "{r:?}");
                    assert!(d.view.is_some() && d.input.is_some(), "view computed");
                    assert_eq!(next, fresh_state());
                }
                None => assert_eq!(d.action.name(), "increase_short", "{r:?}"),
            }
        }
    }

    #[test]
    fn divergence_and_staleness_block_after_the_view() {
        let k = hedge_knobs();
        for (bps, blocked) in [(80.0, true), (-80.0, true), (50.0, false), (-40.0, false)] {
            let mut s = composed();
            s.pool_vs_oracle_bps = Some(bps);
            let (d, _) = hedge(&s, &k, &fresh_state());
            assert_eq!(guard_of(&d) == Some(Guard::Divergence), blocked, "{bps}");
        }
        let mut s = composed();
        s.oracle.age_ms = Some(0);
        let (d, _) = decide_hedge(&s, &meta_of(&s, 31_000), 31_000, &k, &fresh_state(), NOW);
        assert_eq!(guard_of(&d), Some(Guard::StaleInput));
        let (d, _) = decide_hedge(&s, &meta_of(&s, 30_000), 30_000, &k, &fresh_state(), NOW);
        assert_ne!(guard_of(&d), Some(Guard::StaleInput), "30 s is not > 30 s");
        // The composed price row was 2 s old: 2 s + 30 s > 30 s.
        let s = composed();
        let (d, _) = decide_hedge(&s, &meta_of(&s, 30_000), 30_000, &k, &fresh_state(), NOW);
        assert_eq!(guard_of(&d), Some(Guard::StaleInput));
        // The price row ages with the snapshot: 29 s + 2 s > 30 s.
        let mut s = composed();
        s.oracle.age_ms = Some(29_000);
        let (d, _) = decide_hedge(&s, &meta_of(&s, 2_000), 2_000, &k, &fresh_state(), NOW);
        match &d.action {
            HedgeAction::Blocked { guard, reason } => {
                assert_eq!(*guard, Guard::StaleInput);
                assert!(reason.contains(ids::WSOL), "{reason}");
            }
            other => panic!("{other:?}"),
        }
        assert!(d.trace.headroom_usd.is_some(), "trace on a computed view");
    }

    #[test]
    fn in_band_and_cooldown_come_from_the_core() {
        let s = composed();
        let mut k = hedge_knobs();
        k.band_bins = 4;
        k.delta_threshold_sol = 2.0;
        let (d, _) = hedge(&s, &k, &fresh_state());
        assert_eq!(
            d.action,
            HedgeAction::None {
                reason: "in band".into()
            }
        );
        let v = d.view.unwrap();
        assert_eq!(
            v.band_source,
            BandSource::Auto,
            "LP full value / 20 × 4 > 2"
        );
        assert!(!v.out_of_band);
        // A live mutation 1 s ago starts the 600 s cooldown; a dry one does not.
        let mut st = fresh_state();
        st.last_hedge_action = Some(HedgeActionRecord {
            at_ms: NOW - 1_000,
            action: "increase_short".into(),
            live: true,
            signatures: Vec::new(),
            position_request: None,
            counter: None,
        });
        let (d, _) = hedge(&s, &hedge_knobs(), &st);
        match &d.action {
            HedgeAction::None { reason } => assert!(reason.starts_with("cooldown"), "{reason}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(d.view.unwrap().cooldown_remaining_ms, Some(599_000));
        st.last_hedge_action.as_mut().unwrap().live = false;
        let (d, _) = hedge(&s, &hedge_knobs(), &st);
        assert_eq!(d.action.name(), "increase_short");
    }

    #[test]
    fn venue_permission_blocks_a_disallowed_increase() {
        let mut s = composed();
        if let Field::Ok { value } = &mut s.hedge.sol_custody {
            value.allow_increase = false;
        }
        let (d, next) = hedge(&s, &hedge_knobs(), &fresh_state());
        assert_eq!(guard_of(&d), Some(Guard::VenuePermission));
        assert_eq!(next.updated_at_ms, NOW, "post-core check: state advances");
    }

    #[test]
    fn midpoint_clamps_and_the_recenter_freeze_holds_the_regime() {
        // The fixture position sits 2 bins under its upper edge: ~1.4 % of
        // its value is SOL, below the 2 % enter-above line → regime Above.
        let s = composed();
        let mut k = hedge_knobs();
        k.lp_input = LpInput::Midpoint;
        let (d, next) = hedge(&s, &k, &fresh_state());
        let v = d.view.as_ref().unwrap();
        assert_eq!(v.computed_regime, Some(LpRegime::Above));
        assert_eq!(v.regime_outcome, Some(RegimeOutcome::Committed));
        assert_eq!(v.lp_delta_used, 0.0, "above the range: no LP delta");
        assert_eq!(d.action.name(), "none");
        assert_eq!(next.committed_regime, LpRegime::Above);
        assert_eq!(next.pending_regime, None);
        // Imbalance timer running, no storm → the recenter owns the signal.
        let mut st = fresh_state();
        st.imbalance_since_ms = Some(NOW - 5_000);
        let (d, next) = hedge(&s, &k, &st);
        let v = d.view.as_ref().unwrap();
        assert_eq!(v.regime_outcome, Some(RegimeOutcome::Frozen));
        assert_eq!(v.regime, LpRegime::In);
        let e = s.exposure.value().unwrap();
        let mid = hedge::lp_midpoint_sol(e.base, e.quote, v.price_usd);
        assert_eq!(v.lp_delta_used, mid);
        assert_eq!(next.committed_regime, LpRegime::In);
        assert_eq!(next.pending_regime.unwrap().regime, LpRegime::Above);
        assert_eq!(
            d.action,
            HedgeAction::from_core(&hedge::decide(d.input.as_ref().unwrap()))
        );
        // A storm lifts the freeze.
        st.storm_active = true;
        let (_, next) = hedge(&s, &k, &st);
        assert_eq!(next.committed_regime, LpRegime::Above);
    }

    #[test]
    fn missing_snapshot_blocks_as_stale() {
        let d = HedgeDecision::without_snapshot(
            WALLET,
            POOL,
            &hedge_knobs(),
            "no fresh lp_snapshot row",
        );
        assert_eq!(guard_of(&d), Some(Guard::StaleInput));
        check_observed(&d);
        let d = LpDecision::without_snapshot(WALLET, POOL, &lp_knobs(), "no fresh lp_snapshot row");
        assert_eq!(d.verdict.name(), "blocked");
        check_observed(&d);
    }

    // ── lp_decide ──────────────────────────────────────────────────────

    /// The composed snapshot with its position re-centred 10 bins either
    /// side of the active bin (50 / 50 composition).
    fn balanced() -> LpSnapshot {
        let mut s = composed();
        let (a, step) = (s.active_id, s.bin_step);
        let p = &mut s.positions[0];
        p.lower_bin_id = a - 10;
        p.upper_bin_id = a + 10;
        p.lower_price = gates::price_from_bin(a - 10, step, 9, 6);
        p.upper_price = gates::price_from_bin(a + 10, step, 9, 6);
        p.bins_below_active = 10;
        p.bins_above_active = 10;
        s
    }

    fn no_position() -> LpSnapshot {
        let mut s = composed();
        s.positions.clear();
        s.discovery = Discovery::Empty { at_ms: NOW };
        s.exposure = Field::ok(LpExposure {
            base: 0.0,
            quote: 0.0,
            claimable_base: 0.0,
            claimable_quote: 0.0,
            value_quote: 0.0,
            full_value_base: 0.0,
            position_count: 0,
        });
        s
    }

    #[test]
    fn fixture_position_is_imbalanced_and_recenters() {
        let s = composed();
        let (d, next) = lp(&s, &lp_knobs(), &fresh_state(), &[]);
        let h = &d.health[0];
        assert!(h.in_range && h.is_imbalanced);
        assert!(h.quote_pct_linear >= 90.0, "{h:?}");
        assert_eq!((h.bins_to_lower_edge, h.bins_to_upper_edge), (67, 2));
        match &d.verdict {
            LpVerdict::Recenter {
                position,
                close_only,
                plan,
                reason,
            } => {
                assert_eq!(position, POSITION);
                assert!(!close_only);
                assert!(reason.contains("token Y"), "{reason}");
                let p = plan.as_ref().unwrap();
                assert_eq!((p.min_bin_id, p.max_bin_id, p.width), (-5383, -5364, 20));
                assert_eq!(p.bin_array_indexes, vec![-77]);
                assert!(p.lower_price < s.pool_price && s.pool_price < p.upper_price);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(next.imbalance_since_ms, Some(NOW));
        assert_eq!(next.known_positions, vec![POSITION.to_string()]);
        // Wallet 2.748 SOL (2.448 above reserves) + 107.81 USDC: SOL-heavy.
        let w = d.wallet_split.as_ref().unwrap();
        let sol_usd = (2.748_145_289 - 0.3) * s.oracle.usd.unwrap();
        let total = sol_usd + 107.808_931;
        assert!((w.base_value_ratio - sol_usd / total).abs() < 1e-9);
        assert!(!w.balanced);
        assert!((w.swap_needed_quote - (sol_usd / total - 0.5).abs() * total).abs() < 1e-9);
        check_observed(&d);
        // close-only when the re-entry wait is enabled.
        let mut k = lp_knobs();
        k.reentry_confirm_ms = 60_000;
        let (d, _) = lp(&s, &k, &fresh_state(), &[]);
        assert!(matches!(
            d.verdict,
            LpVerdict::Recenter {
                close_only: true,
                plan: None,
                ..
            }
        ));
    }

    #[test]
    fn trend_confirm_waits_then_recenters() {
        let s = composed();
        let mut k = lp_knobs();
        k.trend_confirm_ms = 60_000;
        let (d, st) = lp(&s, &k, &fresh_state(), &[]);
        assert_eq!(
            d.verdict,
            LpVerdict::WaitTrend {
                since_ms: NOW,
                remaining_ms: 60_000
            }
        );
        let later = NOW + 61_000;
        let (d, st2) = decide_lp(&s, &meta_of(&s, 0), 0, &k, &st, &[], later);
        assert_eq!(d.verdict.name(), "recenter");
        assert_eq!(st2.imbalance_since_ms, Some(NOW));
        // Balanced again → the timer resets.
        let (d, st3) = lp(&balanced(), &k, &st, &[]);
        assert_eq!(d.verdict.name(), "hold");
        assert_eq!(st3.imbalance_since_ms, None);
    }

    #[test]
    fn storm_pauses_an_imbalanced_position_and_latches() {
        let s = composed();
        let price = s.oracle.usd.unwrap();
        let mut k = lp_knobs();
        k.storm_pct_5m = 1.0;
        // Reference 5 min old, 5 % lower: move ≈ 5.3 % > 1 %.
        let samples = [PriceSample {
            t_ms: NOW - 300_000,
            usd: price / 1.05,
        }];
        let (d, next) = lp(&s, &k, &fresh_state(), &samples);
        assert_eq!(
            d.verdict,
            LpVerdict::Paused {
                reason: PauseReason::Storm
            }
        );
        assert!(d.storm.active && next.storm_active);
        assert!((d.storm.move_5m_pct.unwrap() - 5.0).abs() < 1e-6);
        // Balanced position in a storm just holds.
        let (d, _) = lp(&balanced(), &k, &fresh_state(), &samples);
        assert_eq!(d.verdict.name(), "hold");
        // No reference sample: the latch keeps its state.
        let mut st = fresh_state();
        st.storm_active = true;
        let (d, next) = lp(&balanced(), &k, &st, &[]);
        assert!(d.storm.move_5m_pct.is_none() && next.storm_active);
    }

    #[test]
    fn lp_validity_gates_pause_or_block_and_keep_state() {
        let k = lp_knobs();
        let state = fresh_state();
        let mut s = composed();
        s.oracle.usd = None;
        let (d, next) = lp(&s, &k, &state, &[]);
        assert_eq!(
            d.verdict,
            LpVerdict::Paused {
                reason: PauseReason::InvalidRead
            }
        );
        assert!(d.invalid_fields.contains(&"oracle.usd".to_string()));
        assert_eq!(next, state);

        let mut s = composed();
        s.discovery = Discovery::Error {
            error: err("discovery"),
            fallback_count: 1,
        };
        let (d, _) = lp(&s, &k, &state, &[]);
        assert_eq!(
            d.verdict,
            LpVerdict::Paused {
                reason: PauseReason::InvalidRead
            }
        );

        let mut s = composed();
        s.wallet_balances.quote = Field::err(err("wallet.quote"));
        let (d, _) = lp(&s, &k, &state, &[]);
        assert_eq!(d.invalid_fields, vec!["wallet.quote".to_string()]);

        let mut s = balanced();
        let mut second = s.positions[0].clone();
        second.position = "Ui6V49xptXnsgay1h75MVch6zh2B3ahP2yb7iRTwX6x".into();
        s.positions.push(second);
        let (d, next) = lp(&s, &k, &state, &[]);
        assert_eq!(
            d.verdict,
            LpVerdict::Paused {
                reason: PauseReason::MultiplePositions
            }
        );
        assert_eq!(next, state);
        check_observed(&d);

        let mut s = balanced();
        s.pool_vs_oracle_bps = Some(-51.0);
        let (d, next) = lp(&s, &k, &state, &[]);
        assert_eq!(
            d.verdict,
            LpVerdict::Paused {
                reason: PauseReason::Divergence
            }
        );
        assert_eq!(next, state);

        let s = balanced();
        let (d, next) = decide_lp(&s, &meta_of(&s, 31_000), 31_000, &k, &state, &[], NOW);
        assert_eq!(d.verdict.name(), "blocked");
        assert_eq!(next, state);
    }

    #[test]
    fn balanced_position_holds_and_clears_an_armed_wait() {
        let mut st = fresh_state();
        st.reentry = Some(ReentryWait::arm(100.0, 102.0, 101.0, NOW - 1));
        let (d, next) = lp(&balanced(), &lp_knobs(), &st, &[]);
        match &d.verdict {
            LpVerdict::Hold { reason } => assert!(reason.contains("within 90.00%"), "{reason}"),
            other => panic!("{other:?}"),
        }
        assert!(!d.health[0].is_imbalanced);
        assert!(
            next.reentry.is_none(),
            "a position on chain wins (self-heal)"
        );
    }

    #[test]
    fn no_position_opens_a_centered_range_capped_at_70_bins() {
        let s = no_position();
        let (d, next) = lp(&s, &lp_knobs(), &fresh_state(), &[]);
        match &d.verdict {
            LpVerdict::Open { plan, reentry } => {
                assert!(!reentry);
                assert_eq!(plan.width, 20);
                assert_eq!(plan.min_bin_id, s.active_id - 10);
            }
            other => panic!("{other:?}"),
        }
        assert!(next.known_positions.is_empty() && next.imbalance_since_ms.is_none());
        let mut k = lp_knobs();
        k.bin_count = 80;
        let (d, _) = lp(&s, &k, &fresh_state(), &[]);
        match &d.verdict {
            LpVerdict::Open { plan, .. } => {
                assert_eq!(plan.width, 70);
                assert!(plan.clamped_to_max_width);
                assert_eq!(plan.max_bin_id - plan.min_bin_id + 1, 70);
            }
            other => panic!("{other:?}"),
        }
        check_observed(&d);
    }

    #[test]
    fn open_is_blocked_on_a_disabled_pool_or_an_unfunded_wallet() {
        let mut s = no_position();
        s.pool_enabled = false;
        let (d, _) = lp(&s, &lp_knobs(), &fresh_state(), &[]);
        assert_eq!(d.verdict.name(), "blocked");
        let mut s = no_position();
        // 0.35 SOL < 0.2 + 0.1 reserves + 0.0575 position rent.
        s.wallet_balances.native_sol = Field::ok(0.35);
        let (d, _) = lp(&s, &lp_knobs(), &fresh_state(), &[]);
        match &d.verdict {
            LpVerdict::Blocked { reason } => assert!(reason.contains("position rent"), "{reason}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn reentry_wait_holds_rearms_then_opens() {
        let s = no_position();
        let price = s.oracle.usd.unwrap();
        let mut k = lp_knobs();
        k.reentry_confirm_ms = 60_000;
        let wait = |anchor: f64, since: i64| {
            let mut st = fresh_state();
            st.reentry = Some(ReentryWait {
                anchor_price: anchor,
                stable_since_ms: since,
                width_frac: 0.02,
                closed_at_ms: Some(since),
            });
            st
        };
        // Inside the 0.15 × 2 % = 0.3 % corridor, 10 s of 60 s held.
        let (d, next) = lp(&s, &k, &wait(price, NOW - 10_000), &[]);
        assert_eq!(
            d.verdict,
            LpVerdict::Reentry {
                decision: ReentryDecision::Hold
            }
        );
        let r = d.reentry.as_ref().unwrap();
        assert_eq!((r.held_ms, r.remaining_ms), (10_000, 50_000));
        assert!((r.tol_price_frac - 0.003).abs() < 1e-15);
        assert_eq!(next.reentry.as_ref().unwrap().anchor_price, price);
        // Breakout: 1 % away → re-anchor at the current price.
        let (d, next) = lp(&s, &k, &wait(price * 1.01, NOW - 50_000), &[]);
        assert_eq!(
            d.verdict,
            LpVerdict::Reentry {
                decision: ReentryDecision::Rearm {
                    anchor_price: price,
                    stable_since_ms: NOW
                }
            }
        );
        let w = next.reentry.as_ref().unwrap();
        assert_eq!((w.anchor_price, w.stable_since_ms), (price, NOW));
        // Held 70 s ≥ 60 s → open; the wait stays armed until the open lands.
        let (d, next) = lp(&s, &k, &wait(price, NOW - 70_000), &[]);
        assert!(matches!(d.verdict, LpVerdict::Open { reentry: true, .. }));
        assert!(next.reentry.is_some());
        check_observed(&d);
    }

    #[test]
    fn non_usd_quote_runs_on_the_pool_price_and_ignores_oracle_samples() {
        let mut s = composed();
        s.pair.quote_is_usd = false;
        s.oracle.needed = false;
        s.oracle.usd = None;
        s.pool_vs_oracle_bps = None;
        let mut k = lp_knobs();
        k.storm_pct_5m = 1.0;
        let samples = [PriceSample {
            t_ms: NOW - 300_000,
            usd: 1.0,
        }];
        let (d, _) = lp(&s, &k, &fresh_state(), &samples);
        assert_eq!(d.cycle_price, Some(s.pool_price));
        assert!(
            d.storm.move_5m_pct.is_none(),
            "SOL/USD samples are not this pool's series"
        );
        assert_eq!(d.verdict.name(), "recenter");
    }

    // ── controller state ───────────────────────────────────────────────

    #[test]
    fn controller_state_is_an_observation_that_round_trips() {
        let mut st = LpControllerState::new(WALLET, ID_44);
        st.pending_regime = Some(PendingRegime {
            regime: LpRegime::Below,
            since_ms: NOW,
        });
        // Simple decimals: serde_json's default float parser is not
        // round-trip exact for every double.
        st.reentry = Some(ReentryWait {
            anchor_price: 111.5,
            stable_since_ms: NOW,
            width_frac: 0.02,
            closed_at_ms: Some(NOW),
        });
        st.last_hedge_action = Some(HedgeActionRecord {
            at_ms: NOW,
            action: "increase_short".into(),
            live: true,
            signatures: vec![
                "L8TEY2sSvscX2R2EBChD1p1o3HApdBUHqVfTJKLxJ4zK4M6pebL3diuYKcKjPF5deW7GF6DVnMdPU2tBwgz1Mfi".into(),
            ],
            position_request: Some(REQUEST.into()),
            counter: Some(473_577_047),
        });
        check_observed(&st);
        let o = Observation::of("lp_decide", &st, NOW, LP_STATE_TTL_MS, ObsSource::Live);
        assert_eq!(o.key, format!("lp_state/1:{WALLET}:{ID_44}"));
        assert_eq!(o.typed::<LpControllerState>().unwrap(), st);
        let w = ReentryWait::arm(110.0, 112.0, 111.0, NOW);
        assert!((w.width_frac - 2.0 / 111.0).abs() < 1e-15);
        assert_eq!(
            (w.anchor_price, w.stable_since_ms, w.closed_at_ms),
            (111.0, NOW, Some(NOW))
        );
        assert_eq!(
            ReentryWait::arm(0.0, 0.0, 111.0, NOW).width_frac,
            0.02,
            "fallback"
        );
        // A bare row (only the required fields) still decodes.
        let bare: LpControllerState = serde_json::from_value(json!({
            "wallet": WALLET, "pool": POOL, "committed_regime": "in"
        }))
        .unwrap();
        assert_eq!(bare, LpControllerState::new(WALLET, POOL));
    }

    // ── knobs ──────────────────────────────────────────────────────────

    /// A valid sample value for every property of a defs.rs knob schema.
    fn schema_sample(schema: &Value) -> Value {
        let mut o = serde_json::Map::new();
        for (name, p) in schema["properties"].as_object().unwrap() {
            let v = match p["type"].as_str() {
                Some("number") => {
                    let lo = p["minimum"].as_f64().unwrap_or(0.0);
                    match p["maximum"].as_f64() {
                        Some(hi) => json!((lo + hi) / 2.0),
                        None => json!(lo + 1.0),
                    }
                }
                Some("integer") => json!(p["minimum"].as_i64().unwrap_or(0) + 1),
                Some("boolean") => json!(true),
                Some("string") => p["enum"][0].clone(),
                other => panic!("{name}: unexpected schema type {other:?}"),
            };
            o.insert(name.clone(), v);
        }
        Value::Object(o)
    }

    fn keys_of(v: &Value) -> Vec<String> {
        let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
        k.sort();
        k
    }

    #[test]
    fn knobs_deserialize_exactly_the_defs_schema_properties() {
        use crate::adapters::outbound::tools::solana::defs::def;
        use crate::domain::tools as names;
        let hedge_schema = def(names::HEDGE_DECIDE).parameters["properties"]["knobs"].clone();
        let lp_schema = def(names::LP_DECIDE).parameters["properties"]["knobs"].clone();

        let sample = schema_sample(&hedge_schema);
        let k = HedgeKnobs::parse(&sample).unwrap();
        assert_eq!(
            keys_of(&serde_json::to_value(&k).unwrap()),
            keys_of(&sample)
        );
        for mode in hedge_schema["properties"]["lp_input"]["enum"]
            .as_array()
            .unwrap()
        {
            let mut v = sample.clone();
            v["lp_input"] = mode.clone();
            HedgeKnobs::parse(&v).unwrap();
        }
        for name in keys_of(&sample) {
            let mut v = sample.clone();
            v.as_object_mut().unwrap().remove(&name);
            let e = HedgeKnobs::parse(&v).unwrap_err();
            assert!(e.contains(&name), "missing {name}: {e}");
        }

        let sample = schema_sample(&lp_schema);
        let k = LpKnobs::parse(&sample).unwrap();
        assert_eq!(
            keys_of(&serde_json::to_value(&k).unwrap()),
            keys_of(&sample)
        );
        for name in keys_of(&sample) {
            let mut v = sample.clone();
            v.as_object_mut().unwrap().remove(&name);
            assert!(LpKnobs::parse(&v).is_err(), "missing {name}");
        }

        // Unknown fields are rejected.
        let mut v = schema_sample(&lp_schema);
        v["imbalance_threshhold"] = json!(0.9);
        assert!(LpKnobs::parse(&v)
            .unwrap_err()
            .contains("imbalance_threshhold"));
    }

    #[test]
    fn knob_validation_names_the_knob() {
        let mut k = hedge_knobs();
        k.target_collateral_ratio = 0.1;
        let e = k.validate().unwrap_err();
        assert!(
            e.contains("knobs.target_collateral_ratio") && e.contains("knobs.min_collateral_ratio"),
            "{e}"
        );
        k = hedge_knobs();
        k.delta_threshold_sol = -1.0;
        assert!(k
            .validate()
            .unwrap_err()
            .contains("knobs.delta_threshold_sol"));
        k = hedge_knobs();
        k.cap_mult = f64::NAN;
        assert!(k.validate().unwrap_err().contains("finite"));
        let mut l = lp_knobs();
        l.imbalance_threshold = 1.5;
        assert!(l
            .validate()
            .unwrap_err()
            .contains("knobs.imbalance_threshold"));
        l = lp_knobs();
        l.bin_count = 0;
        assert!(l.validate().unwrap_err().contains("knobs.bin_count"));
        assert!(hedge_knobs().validate().is_ok() && lp_knobs().validate().is_ok());
        assert!(HedgeKnobs::parse(&json!({"target_delta_sol": 0}))
            .unwrap_err()
            .starts_with("knobs:"));
    }
}
