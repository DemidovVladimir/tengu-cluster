//! `lp_snapshot/1`: [`compose_snapshot`] builds one wallet × DLMM pool snapshot from the family builders + the `price_oracle/1` row.

use super::*;

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

pub(super) fn oracle_ref(pair: &PairRoles, oracle: Option<&Observation>, now_ms: i64) -> OracleRef {
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

pub(super) fn relabel<T: Clone>(f: &Field<T>, field: &str) -> Field<T> {
    match f {
        Field::Error { error } => Field::err(ReadError {
            field: field.to_string(),
            ..error.clone()
        }),
        other => other.clone(),
    }
}

pub(super) fn balance_field(inv: &WalletInventory, mint: &str, field: &str) -> Field<TokenAmount> {
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

pub(super) fn finite(x: f64) -> Option<f64> {
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

    pub(super) fn perps_label(&self) -> &'static str {
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
pub(super) fn fmt_sig(x: f64, sig: i32) -> String {
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

pub(super) fn regime_str(r: LpRegime) -> &'static str {
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
