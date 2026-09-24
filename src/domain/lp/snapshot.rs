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

    /// `lp_snapshot <wallet> <pool> price=… oracle=… positions=n in_range=k perps=…`
    fn headline(&self) -> String {
        let oracle = self
            .oracle
            .usd
            .map(|u| fmt_sig(u, 7))
            .unwrap_or_else(|| "none".into());
        format!(
            "lp_snapshot {} {} price={} oracle={oracle} positions={} in_range={} perps={}",
            self.wallet,
            self.pool,
            fmt_sig(self.pool_price, 7),
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
                " size_usd={:.2} adjust_sol={}",
                a.size_usd().unwrap_or(f64::NAN),
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
                        "composition base {:.2}% / quote {:.2}% within {}%",
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
