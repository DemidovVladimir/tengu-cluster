//! Meteora DLMM — `LbPair` / `PositionV2` / `BinArray` decoders, the
//! `dlmm_pool/1` + `dlmm_positions/1` typed outputs, and the share / fee math
//! (U256). Pure: bytes and an [`AccountSet`] in, typed structs out. PDA
//! derivation, RPC and caching live in the glue (`tools/solana/dlmm.rs`).
//!
//! Layouts (absolute offsets incl. the 8-byte Anchor discriminator), checked
//! against the SDK IDL (`lb_clmm` 0.11.0 in `@meteora-ag/dlmm` 1.9.7, dump:
//! `scripts/golden/dlmm_offsets.js`). Decoders check owner + discriminator +
//! `len >= min` (live accounts may be longer than the IDL).
//!
//! | Account | Disc | Min | Fields read |
//! |---|---|---|---|
//! | LbPair | `[33,11,49,98,181,101,177,13]` | 904 | base_factor u16@8, filter_period u16@10, decay_period u16@12, reduction_factor u16@14, variable_fee_control u32@16, max_volatility_accumulator u32@20, protocol_share u16@32, base_fee_power_factor u8@34, volatility_accumulator u32@40, volatility_reference u32@44, index_reference i32@48, last_update_timestamp i64@56, pair_type u8@75, active_id i32@76, bin_step u16@80, status u8@82, activation_type u8@86, token_x_mint@88, token_y_mint@120, reserve_x@152, reserve_y@184, activation_point u64@816, token_mint_x/y_program_flag u8@880/881 |
//! | PositionV2 | `[117,176,212,199,245,180,133,182]` | 8120 | lb_pair@8, owner@40, liquidity_shares [u128;70]@72, fee_infos [48 B;70]@4552 (x/y per-token complete u128 +0/+16, x/y pending u64 +32/+40), lower_bin_id i32@7912, upper_bin_id i32@7916, last_updated_at i64@7920, total_claimed_fee_x/y u64@7928/7936, fee_owner@8001; bins beyond 70 ("extended") follow at 8120 + 112·j (share u128 +0, fee_info +64) |
//! | BinArray | `[92,142,92,220,5,148,70,181]` | 10136 | index i64@8, lb_pair@24, bins [144 B;70]@56 (amount_x u64 +0, amount_y u64 +8, price u128 +16, liquidity_supply u128 +32, fee_amount_x/y_per_token_stored u128 +80/+96) |
//! | SPL Mint (Token / Token-2022) | owner | 82 | supply u64@36, decimals u8@44, is_initialized u8@45 |
//! | SPL token account | owner | 165 | mint@0, amount u64@64 |
//!
//! Math ports the SDK (`DLMM.processPosition`): per bin
//! `share × amount / liquidity_supply` (floor) and claimable fee
//! `((share >> 64) × (stored − complete)) >> 64 + pending` (Q64.64, floor).
//! Deliberate deviations (failed reads never become 0): a bin array missing
//! for bins that hold shares marks the position `complete = false` (the SDK
//! silently treats it as empty); `stored < complete` (inconsistent read)
//! is an error, not a negative fee; Token-2022 transfer fees are not
//! deducted (amounts are gross).

// Consumed by the dlmm_pool / dlmm_positions / lp_snapshot tools (stage 3).
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};

use alloy::primitives::U256;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::gates::{
    bin_array_indexes, dlmm_fee_rates, dynamic_volatility_accumulator, price_from_bin,
    token_percentages, DlmmFeeParams, VolatilityState, MAX_BIN_PER_ARRAY,
};
use crate::domain::observation::{
    set_bool, set_int, set_num, set_str, ErrorClass, Features, Field, ObsStatus, Observed,
    ReadError,
};
use crate::domain::solana::{bs58_encode, ids, AccountSet, AccountState, Pubkey};

// ---------------------------------------------------------------------------
// Layout constants
// ---------------------------------------------------------------------------

pub(crate) const LB_PAIR_DISC: [u8; 8] = [33, 11, 49, 98, 181, 101, 177, 13];
pub(crate) const LB_PAIR_MIN_LEN: usize = 904;
pub(crate) const POSITION_V2_DISC: [u8; 8] = [117, 176, 212, 199, 245, 180, 133, 182];
pub(crate) const POSITION_V2_MIN_LEN: usize = 8120;
pub(crate) const BIN_ARRAY_DISC: [u8; 8] = [92, 142, 92, 220, 5, 148, 70, 181];
pub(crate) const BIN_ARRAY_MIN_LEN: usize = 10136;
/// Bins stored inline in a PositionV2 (`DEFAULT_BIN_PER_POSITION`).
pub(crate) const POSITION_BASE_BINS: usize = 70;
/// Bytes per extended bin after the 8120-byte base (`POSITION_BIN_DATA_SIZE`).
pub(crate) const POSITION_BIN_DATA_SIZE: usize = 112;
/// Max bins in one position (`POSITION_MAX_LENGTH`).
pub(crate) const POSITION_MAX_LENGTH: i64 = 1400;
/// Half widths (bins either side of the active bin) of the depth bands.
pub(crate) const DEPTH_HALF_WIDTHS: [u32; 3] = [10, 25, 50];
const BIN_SIZE: usize = 144;
const BINS_OFFSET: usize = 56;
const SPL_MINT_MIN_LEN: usize = 82;
const SPL_ACCOUNT_MIN_LEN: usize = 165;
/// `PairStatus::Enabled`.
const PAIR_STATUS_ENABLED: u8 = 0;

// ---------------------------------------------------------------------------
// Byte readers (callers check `len >= min` first; offsets are constants)
// ---------------------------------------------------------------------------

fn le<const N: usize>(d: &[u8], o: usize) -> [u8; N] {
    let mut b = [0u8; N];
    b.copy_from_slice(&d[o..o + N]);
    b
}
fn u16_at(d: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(le(d, o))
}
fn u32_at(d: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(le(d, o))
}
fn i32_at(d: &[u8], o: usize) -> i32 {
    i32::from_le_bytes(le(d, o))
}
fn u64_at(d: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(le(d, o))
}
fn i64_at(d: &[u8], o: usize) -> i64 {
    i64::from_le_bytes(le(d, o))
}
fn u128_at(d: &[u8], o: usize) -> u128 {
    u128::from_le_bytes(le(d, o))
}
fn key_at(d: &[u8], o: usize) -> Pubkey {
    Pubkey(le(d, o))
}

fn check_header(d: &[u8], disc: &[u8; 8], min_len: usize, name: &str) -> Result<(), String> {
    if d.len() < min_len {
        return Err(format!("{name} data is {} bytes < {min_len}", d.len()));
    }
    if d[..8] != disc[..] {
        return Err(format!("{name} discriminator mismatch: {:?}", &d[..8]));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Raw decoders
// ---------------------------------------------------------------------------

/// The LbPair fields tengu reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LbPair {
    pub base_factor: u16,
    pub filter_period: u16,
    pub decay_period: u16,
    pub reduction_factor: u16,
    pub variable_fee_control: u32,
    pub max_volatility_accumulator: u32,
    pub protocol_share: u16,
    pub base_fee_power_factor: u8,
    pub volatility_accumulator: u32,
    pub volatility_reference: u32,
    pub index_reference: i32,
    pub last_update_timestamp: i64,
    pub pair_type: u8,
    pub active_id: i32,
    pub bin_step: u16,
    /// `PairStatus`: 0 enabled, 1 disabled.
    pub status: u8,
    pub activation_type: u8,
    pub token_x_mint: Pubkey,
    pub token_y_mint: Pubkey,
    pub reserve_x: Pubkey,
    pub reserve_y: Pubkey,
    pub activation_point: u64,
    /// `TokenProgramFlags`: 0 Token, 1 Token-2022.
    pub token_mint_x_program_flag: u8,
    pub token_mint_y_program_flag: u8,
}

pub(crate) fn decode_lb_pair(d: &[u8]) -> Result<LbPair, String> {
    check_header(d, &LB_PAIR_DISC, LB_PAIR_MIN_LEN, "LbPair")?;
    let p = LbPair {
        base_factor: u16_at(d, 8),
        filter_period: u16_at(d, 10),
        decay_period: u16_at(d, 12),
        reduction_factor: u16_at(d, 14),
        variable_fee_control: u32_at(d, 16),
        max_volatility_accumulator: u32_at(d, 20),
        protocol_share: u16_at(d, 32),
        base_fee_power_factor: d[34],
        volatility_accumulator: u32_at(d, 40),
        volatility_reference: u32_at(d, 44),
        index_reference: i32_at(d, 48),
        last_update_timestamp: i64_at(d, 56),
        pair_type: d[75],
        active_id: i32_at(d, 76),
        bin_step: u16_at(d, 80),
        status: d[82],
        activation_type: d[86],
        token_x_mint: key_at(d, 88),
        token_y_mint: key_at(d, 120),
        reserve_x: key_at(d, 152),
        reserve_y: key_at(d, 184),
        activation_point: u64_at(d, 816),
        token_mint_x_program_flag: d[880],
        token_mint_y_program_flag: d[881],
    };
    if p.bin_step == 0 {
        return Err("LbPair bin_step is 0".into());
    }
    Ok(p)
}

/// One bin of a position: liquidity share + fee checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct PositionBin {
    pub liquidity_share: u128,
    pub fee_x_per_token_complete: u128,
    pub fee_y_per_token_complete: u128,
    pub fee_x_pending: u64,
    pub fee_y_pending: u64,
}

/// A PositionV2 incl. its extended bins (`bins.len() == width`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PositionV2 {
    pub lb_pair: Pubkey,
    pub owner: Pubkey,
    pub lower_bin_id: i32,
    pub upper_bin_id: i32,
    pub last_updated_at: i64,
    pub total_claimed_fee_x: u64,
    pub total_claimed_fee_y: u64,
    pub fee_owner: Pubkey,
    /// Bin `lower_bin_id + i` at index `i`.
    pub bins: Vec<PositionBin>,
}

impl PositionV2 {
    pub fn width(&self) -> u32 {
        self.bins.len() as u32
    }
    pub fn is_extended(&self) -> bool {
        self.bins.len() > POSITION_BASE_BINS
    }
}

pub(crate) fn decode_position_v2(d: &[u8]) -> Result<PositionV2, String> {
    check_header(d, &POSITION_V2_DISC, POSITION_V2_MIN_LEN, "PositionV2")?;
    let lower = i32_at(d, 7912);
    let upper = i32_at(d, 7916);
    let width = i64::from(upper) - i64::from(lower) + 1;
    if !(1..=POSITION_MAX_LENGTH).contains(&width) {
        return Err(format!(
            "PositionV2 bin range [{lower}, {upper}] is not 1..={POSITION_MAX_LENGTH} bins"
        ));
    }
    let width = width as usize;
    let extended = width.saturating_sub(POSITION_BASE_BINS);
    let need = POSITION_V2_MIN_LEN + extended * POSITION_BIN_DATA_SIZE;
    if d.len() < need {
        return Err(format!(
            "PositionV2 spans {width} bins and needs {need} bytes, has {}",
            d.len()
        ));
    }
    let mut bins = Vec::with_capacity(width);
    for i in 0..width.min(POSITION_BASE_BINS) {
        let f = 4552 + 48 * i;
        bins.push(PositionBin {
            liquidity_share: u128_at(d, 72 + 16 * i),
            fee_x_per_token_complete: u128_at(d, f),
            fee_y_per_token_complete: u128_at(d, f + 16),
            fee_x_pending: u64_at(d, f + 32),
            fee_y_pending: u64_at(d, f + 40),
        });
    }
    for j in 0..extended {
        let o = POSITION_V2_MIN_LEN + POSITION_BIN_DATA_SIZE * j;
        bins.push(PositionBin {
            liquidity_share: u128_at(d, o),
            fee_x_per_token_complete: u128_at(d, o + 64),
            fee_y_per_token_complete: u128_at(d, o + 80),
            fee_x_pending: u64_at(d, o + 96),
            fee_y_pending: u64_at(d, o + 104),
        });
    }
    Ok(PositionV2 {
        lb_pair: key_at(d, 8),
        owner: key_at(d, 40),
        lower_bin_id: lower,
        upper_bin_id: upper,
        last_updated_at: i64_at(d, 7920),
        total_claimed_fee_x: u64_at(d, 7928),
        total_claimed_fee_y: u64_at(d, 7936),
        fee_owner: key_at(d, 8001),
        bins,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Bin {
    pub amount_x: u64,
    pub amount_y: u64,
    /// Q64.64 price per lamport.
    pub price: u128,
    pub liquidity_supply: u128,
    pub fee_amount_x_per_token_stored: u128,
    pub fee_amount_y_per_token_stored: u128,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BinArray {
    pub index: i64,
    pub lb_pair: Pubkey,
    /// Bin `index × 70 + i` at `i`; always 70.
    pub bins: Vec<Bin>,
}

pub(crate) fn decode_bin_array(d: &[u8]) -> Result<BinArray, String> {
    check_header(d, &BIN_ARRAY_DISC, BIN_ARRAY_MIN_LEN, "BinArray")?;
    let bins = (0..MAX_BIN_PER_ARRAY as usize)
        .map(|i| {
            let o = BINS_OFFSET + BIN_SIZE * i;
            Bin {
                amount_x: u64_at(d, o),
                amount_y: u64_at(d, o + 8),
                price: u128_at(d, o + 16),
                liquidity_supply: u128_at(d, o + 32),
                fee_amount_x_per_token_stored: u128_at(d, o + 80),
                fee_amount_y_per_token_stored: u128_at(d, o + 96),
            }
        })
        .collect();
    Ok(BinArray {
        index: i64_at(d, 8),
        lb_pair: key_at(d, 24),
        bins,
    })
}

// ---------------------------------------------------------------------------
// Account-set access
// ---------------------------------------------------------------------------

fn decode_err(field: &str, msg: impl Into<String>) -> ReadError {
    ReadError::new(field, ErrorClass::Decode, msg)
}

fn not_read(field: &str, key: &Pubkey) -> ReadError {
    ReadError::new(
        field,
        ErrorClass::Fatal,
        format!("account {key} is not in the read set"),
    )
}

/// Bytes of `key` when it exists and is owned by one of `owners` with at
/// least `min_len` bytes. `Ok(None)` = absent on chain; `Err` = not read,
/// wrong owner, short or bad base64.
fn account_bytes(
    set: &AccountSet,
    key: &Pubkey,
    owners: &[&str],
    min_len: usize,
    field: &str,
) -> Result<Option<(Pubkey, Vec<u8>)>, ReadError> {
    let read = set.get(key).ok_or_else(|| not_read(field, key))?;
    let AccountState::Ok { owner, .. } = &read.state else {
        return Ok(None);
    };
    if !owners.iter().any(|o| ids::key(o) == *owner) {
        return Err(decode_err(
            field,
            format!("account {key} owner {owner} is not {}", owners.join(" or ")),
        ));
    }
    let data = read
        .data()
        .ok_or_else(|| decode_err(field, format!("account {key} data is not valid base64")))?;
    if data.len() < min_len {
        return Err(decode_err(
            field,
            format!("account {key} data is {} bytes < {min_len}", data.len()),
        ));
    }
    Ok(Some((*owner, data)))
}

/// The pool's LbPair from the set. `NotApplicable` when the account does not
/// exist, `Decode` on owner / discriminator / length problems.
pub(crate) fn lb_pair_from_set(set: &AccountSet, pool: &Pubkey) -> Result<LbPair, ReadError> {
    match account_bytes(set, pool, &[ids::DLMM], LB_PAIR_MIN_LEN, "pool")? {
        None => Err(ReadError::new(
            "pool",
            ErrorClass::NotApplicable,
            format!("DLMM pool {pool} does not exist"),
        )),
        Some((_, d)) => decode_lb_pair(&d).map_err(|e| decode_err("pool", format!("{pool}: {e}"))),
    }
}

/// `(token program, decimals)` of an SPL mint.
fn mint_from_set(set: &AccountSet, mint: &Pubkey, field: &str) -> Result<(Pubkey, u8), ReadError> {
    match account_bytes(
        set,
        mint,
        &[ids::TOKEN, ids::TOKEN_2022],
        SPL_MINT_MIN_LEN,
        field,
    )? {
        None => Err(ReadError::new(
            field,
            ErrorClass::NotApplicable,
            format!("mint {mint} does not exist"),
        )),
        Some((program, d)) => {
            if d[45] != 1 {
                return Err(decode_err(field, format!("mint {mint} is not initialized")));
            }
            Ok((program, d[44]))
        }
    }
}

/// Amount of an SPL token account holding `mint`.
fn token_amount_from_set(
    set: &AccountSet,
    account: &Pubkey,
    mint: &Pubkey,
    decimals: u8,
    field: &str,
) -> Field<TokenAmount> {
    match account_bytes(
        set,
        account,
        &[ids::TOKEN, ids::TOKEN_2022],
        SPL_ACCOUNT_MIN_LEN,
        field,
    ) {
        Err(e) => Field::err(e),
        Ok(None) => Field::err(ReadError::new(
            field,
            ErrorClass::NotApplicable,
            format!("token account {account} does not exist"),
        )),
        Ok(Some((_, d))) => {
            let held = key_at(&d, 0);
            if held != *mint {
                return Field::err(decode_err(
                    field,
                    format!("token account {account} holds mint {held}, expected {mint}"),
                ));
            }
            Field::ok(TokenAmount::new(mint, u64_at(&d, 64), decimals))
        }
    }
}

// ---------------------------------------------------------------------------
// Typed outputs
// ---------------------------------------------------------------------------

/// An SPL amount: exact raw integer + UI value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct TokenAmount {
    pub mint: String,
    /// u64 as a decimal string.
    pub raw: String,
    pub decimals: u8,
    pub ui: f64,
}

impl TokenAmount {
    pub fn new(mint: &Pubkey, raw: u64, decimals: u8) -> Self {
        Self {
            mint: mint.to_string(),
            raw: raw.to_string(),
            decimals,
            ui: ui_amount(u128::from(raw), decimals),
        }
    }
}

/// Pair roles: base = token X (volatile side), quote = token Y
/// (`delta_neutral_bot/src/config/pairConfig.ts:5-16,78-85`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PairRoles {
    pub pool: String,
    pub base_mint: String,
    pub base_decimals: u8,
    pub base_token_program: String,
    pub quote_mint: String,
    pub quote_decimals: u8,
    pub quote_token_program: String,
    pub base_is_native_sol: bool,
    pub quote_is_native_sol: bool,
    /// Quote is USDC — every "usd" figure is really USD.
    pub quote_is_usd: bool,
}

/// Swap fee at the pool, percent. `variable_fee_pct` / `total_fee_pct` use
/// the volatility accumulator projected to the build time (SDK
/// `getDynamicFee`); `volatility_accumulator` is the stored value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct DlmmFeeRate {
    pub base_fee_pct: f64,
    pub variable_fee_pct: f64,
    pub total_fee_pct: f64,
    pub max_fee_pct: f64,
    pub protocol_share_pct: f64,
    pub volatility_accumulator: u32,
    pub volatility_accumulator_now: u32,
    pub v_last_update_ts: i64,
}

/// Liquidity within `half_width_bins` of the active bin (inclusive).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct DepthBand {
    pub half_width_bins: u32,
    pub base: f64,
    pub quote: f64,
    /// `base × active_price + quote`.
    pub value_quote: f64,
    /// false = a bin array of the band was not read (values are a floor).
    pub complete: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct DlmmDepth {
    /// Bin array indexes the bands cover (read or absent on chain).
    pub bin_arrays: Vec<i64>,
    /// Indexes the bands need that were not in the read set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_bin_arrays: Vec<i64>,
    /// Half widths [`DEPTH_HALF_WIDTHS`].
    pub bands: Vec<DepthBand>,
}

/// `dlmm_pool/1:<pool>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct DlmmPoolState {
    pub pool: String,
    pub slot: u64,
    pub pair: PairRoles,
    /// `pair_status == 0`.
    pub enabled: bool,
    pub pair_status: u8,
    pub pair_type: u8,
    pub bin_step: u16,
    pub active_id: i32,
    /// Quote per base, UI units.
    pub active_price: f64,
    pub fee: DlmmFeeRate,
    pub reserve_x: String,
    pub reserve_y: String,
    pub reserve_base: Field<TokenAmount>,
    pub reserve_quote: Field<TokenAmount>,
    /// `reserve_base × active_price + reserve_quote`; `None` unless both read.
    pub tvl_quote_onchain: Option<f64>,
    pub depth: Field<DlmmDepth>,
}

/// Position discovery outcome — an input from the glue (gPA / explicit
/// args / cached row). Tri-state so a failed discovery never reads as
/// "no positions" (fixes `meteoraAdapter.ts:164-240` fail-open).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum Discovery {
    Found {
        count: u32,
        source: DiscoverySource,
        at_ms: i64,
    },
    Empty {
        at_ms: i64,
    },
    /// `fallback_count` = positions still supplied (e.g. last known set).
    Error {
        error: ReadError,
        fallback_count: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DiscoverySource {
    Gpa,
    Args,
    Cached,
}

impl Discovery {
    fn label(&self) -> &'static str {
        match self {
            Discovery::Found { .. } => "found",
            Discovery::Empty { .. } => "empty",
            Discovery::Error { .. } => "error",
        }
    }
}

/// One PositionV2 valued at the pool's active price.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct DlmmPosition {
    pub position: String,
    pub owner: String,
    pub fee_owner: String,
    pub lower_bin_id: i32,
    pub upper_bin_id: i32,
    pub width: u32,
    pub lower_price: f64,
    pub upper_price: f64,
    pub in_range: bool,
    /// `active − lower`.
    pub bins_below_active: i32,
    /// `upper − active`.
    pub bins_above_active: i32,
    pub amount_base: f64,
    pub amount_quote: f64,
    /// `amount_base × active_price + amount_quote` (fees excluded).
    pub value_quote: f64,
    /// Claimable (unclaimed) swap fees.
    pub fee_base: f64,
    pub fee_quote: f64,
    pub total_claimed_fee_base: f64,
    pub total_claimed_fee_quote: f64,
    /// Base share of `value_quote`, percent; `None` when the value is 0.
    pub base_pct_value: Option<f64>,
    /// Base share by price position in the range, percent
    /// (`calculateTokenPercentages`, `meteoraUtils.ts:121-152`).
    pub base_pct_linear: f64,
    pub last_updated_at: i64,
    /// false = a bin array holding its shares was not read or a fee
    /// checkpoint was inconsistent: amounts / fees are a floor.
    pub complete: bool,
}

/// Sum over a wallet's positions in one pool, valued at the active price.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct LpExposure {
    pub base: f64,
    pub quote: f64,
    pub claimable_base: f64,
    pub claimable_quote: f64,
    /// `base × active_price + quote` (fees excluded).
    pub value_quote: f64,
    /// `base + quote / active_price` (`lpFullValueSol`,
    /// `jupiterPerpsEngine.ts:858`).
    pub full_value_base: f64,
    pub position_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum DlmmAnomaly {
    /// More than one position for the wallet in this pool.
    MultiplePositions { count: u32 },
    /// Position wider than 70 bins (the strategy never opens one).
    ExtendedPosition { position: String },
    /// `pair_status != 0`.
    PoolDisabled,
}

/// `dlmm_positions/1:<wallet>:<pool>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct DlmmPositions {
    pub wallet: String,
    pub pool: String,
    pub slot: u64,
    pub active_id: i32,
    pub active_price: f64,
    pub discovery: Discovery,
    pub positions: Vec<DlmmPosition>,
    /// `Error` when discovery failed and no fallback positions were given.
    pub exposure: Field<LpExposure>,
    pub anomalies: Vec<DlmmAnomaly>,
    /// Every pubkey read (the phase-5 subscription set).
    pub watch: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<ReadError>,
}

// ---------------------------------------------------------------------------
// Math (SDK `processPosition`, U256)
// ---------------------------------------------------------------------------

/// `floor(share × amount / supply)`; 0 when `supply == 0`.
pub(crate) fn share_amount(share: u128, amount: u64, supply: u128) -> U256 {
    if supply == 0 {
        return U256::ZERO;
    }
    U256::from(share) * U256::from(amount) / U256::from(supply)
}

/// Claimable fee of one bin: `((share >> 64) × (stored − complete)) >> 64 +
/// pending` (SDK `mulShr(.., SCALE_OFFSET, Down)`); `None` when
/// `stored < complete` (inconsistent read).
pub(crate) fn claimable_fee(
    share: u128,
    stored: u128,
    complete: u128,
    pending: u64,
) -> Option<U256> {
    let new_fee = if share == 0 {
        U256::ZERO
    } else {
        let delta = stored.checked_sub(complete)?;
        (U256::from(share >> 64) * U256::from(delta)) >> 64
    };
    Some(new_fee + U256::from(pending))
}

/// Raw position totals (smallest units). `missing_bin_arrays` = indexes of
/// bins holding shares whose array was not read; `inconsistent_bins` = bins
/// with `stored < complete`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct PositionTotals {
    pub amount_x: U256,
    pub amount_y: U256,
    pub fee_x: U256,
    pub fee_y: U256,
    pub missing_bin_arrays: BTreeSet<i64>,
    pub inconsistent_bins: Vec<i32>,
}

impl PositionTotals {
    pub fn complete(&self) -> bool {
        self.missing_bin_arrays.is_empty() && self.inconsistent_bins.is_empty()
    }
}

pub(crate) fn position_totals(
    pos: &PositionV2,
    arrays: &BTreeMap<i64, BinArray>,
) -> PositionTotals {
    let mut t = PositionTotals::default();
    for (i, pb) in pos.bins.iter().enumerate() {
        let bin_id = i64::from(pos.lower_bin_id) + i as i64;
        let idx = bin_id.div_euclid(MAX_BIN_PER_ARRAY);
        let bin = arrays
            .get(&idx)
            .map(|a| a.bins[(bin_id - idx * MAX_BIN_PER_ARRAY) as usize]);
        let Some(bin) = bin else {
            if pb.liquidity_share != 0 {
                t.missing_bin_arrays.insert(idx);
            } else {
                t.fee_x += U256::from(pb.fee_x_pending);
                t.fee_y += U256::from(pb.fee_y_pending);
            }
            continue;
        };
        t.amount_x += share_amount(pb.liquidity_share, bin.amount_x, bin.liquidity_supply);
        t.amount_y += share_amount(pb.liquidity_share, bin.amount_y, bin.liquidity_supply);
        let fx = claimable_fee(
            pb.liquidity_share,
            bin.fee_amount_x_per_token_stored,
            pb.fee_x_per_token_complete,
            pb.fee_x_pending,
        );
        let fy = claimable_fee(
            pb.liquidity_share,
            bin.fee_amount_y_per_token_stored,
            pb.fee_y_per_token_complete,
            pb.fee_y_pending,
        );
        match (fx, fy) {
            (Some(x), Some(y)) => {
                t.fee_x += x;
                t.fee_y += y;
            }
            _ => t.inconsistent_bins.push(bin_id as i32),
        }
    }
    t
}

fn ui_amount(raw: u128, decimals: u8) -> f64 {
    raw as f64 / 10f64.powi(i32::from(decimals))
}

fn ui_u256(raw: U256, decimals: u8) -> f64 {
    let x = u128::try_from(raw)
        .map(|v| v as f64)
        .unwrap_or_else(|_| raw.to_string().parse::<f64>().unwrap_or(f64::INFINITY));
    x / 10f64.powi(i32::from(decimals))
}

// ---------------------------------------------------------------------------
// Key planning (PDA derivation happens in the glue)
// ---------------------------------------------------------------------------

/// Mints + reserves the pool builder reads besides the LbPair.
pub(crate) fn pool_account_keys(pair: &LbPair) -> Vec<Pubkey> {
    vec![
        pair.token_x_mint,
        pair.token_y_mint,
        pair.reserve_x,
        pair.reserve_y,
    ]
}

/// Bin array indexes for the depth bands (active ± 50) plus every
/// `(lower, upper)` bin range given, ascending and unique.
pub(crate) fn bin_array_keys_needed(pair: &LbPair, ranges: &[(i32, i32)]) -> Vec<i64> {
    let max = *DEPTH_HALF_WIDTHS.iter().max().unwrap_or(&0) as i32;
    let mut out: BTreeSet<i64> = bin_array_indexes(
        pair.active_id.saturating_sub(max),
        pair.active_id.saturating_add(max),
    )
    .into_iter()
    .collect();
    for (lo, hi) in ranges {
        out.extend(bin_array_indexes(*lo, *hi));
    }
    out.into_iter().collect()
}

/// Bin array indexes covering the wallet's positions in `pool` found in the
/// set (second read after discovery).
pub(crate) fn plan_position_keys(set: &AccountSet, wallet: &Pubkey, pool: &Pubkey) -> Vec<i64> {
    let mut out = BTreeSet::new();
    for (_, pos) in scan_positions(set, pool).0 {
        if pos.owner == *wallet {
            out.extend(bin_array_indexes(pos.lower_bin_id, pos.upper_bin_id));
        }
    }
    out.into_iter().collect()
}

/// `getProgramAccounts` filters for a wallet's PositionV2 accounts in one
/// pool (SDK `positionV2Filter` + `positionLbPairFilter` +
/// `positionOwnerFilter`). No `dataSize`: extended positions are longer.
pub(crate) fn position_gpa_filters(wallet: &Pubkey, pool: &Pubkey) -> Vec<Value> {
    vec![
        json!({"memcmp": {"offset": 0, "bytes": bs58_encode(&POSITION_V2_DISC)}}),
        json!({"memcmp": {"offset": 8, "bytes": pool.to_string()}}),
        json!({"memcmp": {"offset": 40, "bytes": wallet.to_string()}}),
    ]
}

/// PositionV2 accounts of `pool` in the set (any owner), plus decode errors
/// for DLMM-owned accounts that carry the PositionV2 discriminator but do
/// not decode.
fn scan_positions(set: &AccountSet, pool: &Pubkey) -> (Vec<(Pubkey, PositionV2)>, Vec<ReadError>) {
    let dlmm = ids::key(ids::DLMM);
    let mut out = Vec::new();
    let mut errors = Vec::new();
    for (key, read) in &set.accounts {
        if read.owner() != Some(&dlmm) {
            continue;
        }
        let Some(d) = read.data() else { continue };
        if d.len() < 8 || d[..8] != POSITION_V2_DISC[..] {
            continue;
        }
        if d.len() >= 40 && key_at(&d, 8) != *pool {
            continue;
        }
        match decode_position_v2(&d) {
            Ok(p) => out.push((*key, p)),
            Err(e) => errors.push(decode_err("positions", format!("{key}: {e}"))),
        }
    }
    (out, errors)
}

/// BinArray accounts of `pool` in the set, by index.
fn scan_bin_arrays(
    set: &AccountSet,
    pool: &Pubkey,
) -> (BTreeMap<i64, (Pubkey, BinArray)>, Vec<ReadError>) {
    let dlmm = ids::key(ids::DLMM);
    let mut out = BTreeMap::new();
    let mut errors = Vec::new();
    for (key, read) in &set.accounts {
        if read.owner() != Some(&dlmm) {
            continue;
        }
        let Some(d) = read.data() else { continue };
        if d.len() < 8 || d[..8] != BIN_ARRAY_DISC[..] {
            continue;
        }
        match decode_bin_array(&d) {
            Ok(a) if a.lb_pair == *pool => {
                out.insert(a.index, (*key, a));
            }
            Ok(_) => {}
            Err(e) => errors.push(decode_err("bin_arrays", format!("{key}: {e}"))),
        }
    }
    (out, errors)
}

// ---------------------------------------------------------------------------
// Builders
// ---------------------------------------------------------------------------

/// The pool context both builders need: LbPair + roles + active price.
struct PoolCtx {
    pair: LbPair,
    roles: PairRoles,
    price: f64,
}

fn pool_ctx(set: &AccountSet, pool: &Pubkey) -> Result<PoolCtx, ReadError> {
    let pair = lb_pair_from_set(set, pool)?;
    let (x_program, dx) = mint_from_set(set, &pair.token_x_mint, "base_mint")?;
    let (y_program, dy) = mint_from_set(set, &pair.token_y_mint, "quote_mint")?;
    let base = pair.token_x_mint.to_string();
    let quote = pair.token_y_mint.to_string();
    let roles = PairRoles {
        pool: pool.to_string(),
        base_is_native_sol: base == ids::WSOL,
        quote_is_native_sol: quote == ids::WSOL,
        quote_is_usd: quote == ids::USDC,
        base_mint: base,
        base_decimals: dx,
        base_token_program: x_program.to_string(),
        quote_mint: quote,
        quote_decimals: dy,
        quote_token_program: y_program.to_string(),
    };
    let price = price_from_bin(pair.active_id, pair.bin_step, dx, dy);
    if !price.is_finite() || price <= 0.0 {
        return Err(decode_err(
            "active_price",
            format!("active bin {} gives price {price}", pair.active_id),
        ));
    }
    Ok(PoolCtx { pair, roles, price })
}

fn fee_rate(pair: &LbPair, now_ms: i64) -> DlmmFeeRate {
    let now_s = now_ms.div_euclid(1000);
    let va_now = dynamic_volatility_accumulator(
        &VolatilityState {
            active_id: pair.active_id,
            index_reference: pair.index_reference,
            volatility_reference: pair.volatility_reference,
            volatility_accumulator: pair.volatility_accumulator,
            last_update_timestamp: pair.last_update_timestamp,
            filter_period: pair.filter_period,
            decay_period: pair.decay_period,
            reduction_factor: pair.reduction_factor,
            max_volatility_accumulator: pair.max_volatility_accumulator,
        },
        now_s,
    );
    let r = dlmm_fee_rates(&DlmmFeeParams {
        bin_step: pair.bin_step,
        base_factor: pair.base_factor,
        base_fee_power_factor: pair.base_fee_power_factor,
        variable_fee_control: pair.variable_fee_control,
        volatility_accumulator: va_now,
        protocol_share: pair.protocol_share,
    });
    DlmmFeeRate {
        base_fee_pct: r.base_fee_pct,
        variable_fee_pct: r.variable_fee_pct,
        total_fee_pct: r.total_fee_pct,
        max_fee_pct: r.max_fee_pct,
        protocol_share_pct: r.protocol_share_pct,
        volatility_accumulator: pair.volatility_accumulator,
        volatility_accumulator_now: va_now,
        v_last_update_ts: pair.last_update_timestamp,
    }
}

fn depth(
    set: &AccountSet,
    pool: &Pubkey,
    ctx: &PoolCtx,
    bin_array_keys: &[(i64, Pubkey)],
) -> Field<DlmmDepth> {
    let (found, errors) = scan_bin_arrays(set, pool);
    if let Some(e) = errors.into_iter().next() {
        return Field::err(ReadError {
            field: "depth".into(),
            ..e
        });
    }
    // Arrays known to be absent on chain = uninitialized = empty bins.
    let absent: BTreeSet<i64> = bin_array_keys
        .iter()
        .filter(|(_, k)| matches!(set.get(k).map(|r| &r.state), Some(AccountState::Absent)))
        .map(|(i, _)| *i)
        .collect();
    let active = i64::from(ctx.pair.active_id);
    let max = i64::from(*DEPTH_HALF_WIDTHS.iter().max().unwrap_or(&0));
    let needed: Vec<i64> = ((active - max).div_euclid(MAX_BIN_PER_ARRAY)
        ..=(active + max).div_euclid(MAX_BIN_PER_ARRAY))
        .collect();
    let missing: Vec<i64> = needed
        .iter()
        .copied()
        .filter(|i| !found.contains_key(i) && !absent.contains(i))
        .collect();
    let (dx, dy) = (ctx.roles.base_decimals, ctx.roles.quote_decimals);
    let bands = DEPTH_HALF_WIDTHS
        .iter()
        .map(|&h| {
            let (mut x, mut y) = (0u128, 0u128);
            let mut complete = true;
            for id in active - i64::from(h)..=active + i64::from(h) {
                let idx = id.div_euclid(MAX_BIN_PER_ARRAY);
                match found.get(&idx) {
                    Some((_, a)) => {
                        let b = a.bins[(id - idx * MAX_BIN_PER_ARRAY) as usize];
                        x += u128::from(b.amount_x);
                        y += u128::from(b.amount_y);
                    }
                    None if absent.contains(&idx) => {}
                    None => complete = false,
                }
            }
            let base = ui_amount(x, dx);
            let quote = ui_amount(y, dy);
            DepthBand {
                half_width_bins: h,
                base,
                quote,
                value_quote: base * ctx.price + quote,
                complete,
            }
        })
        .collect();
    Field::ok(DlmmDepth {
        bin_arrays: needed,
        missing_bin_arrays: missing,
        bands,
    })
}

/// `dlmm_pool/1` from one account set: the LbPair, both mints, both
/// reserves and the bin arrays around the active bin. `bin_array_keys` =
/// the `(index, PDA)` pairs the glue fetched (`bin_array_keys_needed`), so
/// an array absent on chain counts as empty rather than missing.
///
/// `Err` when the LbPair or a mint is unreadable (primary answer
/// unavailable); reserve / depth failures give `Partial`.
pub(crate) fn build_dlmm_pool(
    set: &AccountSet,
    pool: &Pubkey,
    bin_array_keys: &[(i64, Pubkey)],
    now_ms: i64,
) -> Result<DlmmPoolState, ReadError> {
    let ctx = pool_ctx(set, pool)?;
    let p = &ctx.pair;
    let reserve_base = token_amount_from_set(
        set,
        &p.reserve_x,
        &p.token_x_mint,
        ctx.roles.base_decimals,
        "reserve_base",
    );
    let reserve_quote = token_amount_from_set(
        set,
        &p.reserve_y,
        &p.token_y_mint,
        ctx.roles.quote_decimals,
        "reserve_quote",
    );
    let tvl_quote_onchain = match (reserve_base.value(), reserve_quote.value()) {
        (Some(b), Some(q)) => Some(b.ui * ctx.price + q.ui),
        _ => None,
    };
    let depth = depth(set, pool, &ctx, bin_array_keys);
    // The LbPair read's slot (the pool is in the set: `pool_ctx` succeeded).
    let slot = set.get(pool).map(|r| r.slot).unwrap_or(set.slot_max);
    Ok(DlmmPoolState {
        pool: pool.to_string(),
        slot,
        enabled: p.status == PAIR_STATUS_ENABLED,
        pair_status: p.status,
        pair_type: p.pair_type,
        bin_step: p.bin_step,
        active_id: p.active_id,
        active_price: ctx.price,
        fee: fee_rate(p, now_ms),
        reserve_x: p.reserve_x.to_string(),
        reserve_y: p.reserve_y.to_string(),
        reserve_base,
        reserve_quote,
        tvl_quote_onchain,
        depth,
        pair: ctx.roles,
    })
}

/// `dlmm_positions/1`: the wallet's PositionV2 accounts for `pool` found in
/// the set (owner DLMM + discriminator + `lb_pair` + `owner` fields), valued
/// with the pool's bin arrays in the set. `discovery` comes from the glue.
///
/// `Err` when the LbPair or a mint is unreadable. Status: `Error` when
/// discovery failed with no fallback positions, `Partial` when a position is
/// incomplete or discovery failed with a fallback, `Absent` when there are
/// no positions, else `Ok`.
pub(crate) fn build_positions(
    set: &AccountSet,
    wallet: &Pubkey,
    pool: &Pubkey,
    discovery: Discovery,
) -> Result<DlmmPositions, ReadError> {
    let ctx = pool_ctx(set, pool)?;
    let (dx, dy) = (ctx.roles.base_decimals, ctx.roles.quote_decimals);
    let active = ctx.pair.active_id;
    let (all_positions, mut errors) = scan_positions(set, pool);
    let (found_arrays, array_errors) = scan_bin_arrays(set, pool);
    errors.extend(array_errors);
    let arrays: BTreeMap<i64, BinArray> = found_arrays
        .iter()
        .map(|(i, (_, a))| (*i, a.clone()))
        .collect();

    let mut watch: BTreeSet<String> = [
        pool.to_string(),
        ctx.pair.token_x_mint.to_string(),
        ctx.pair.token_y_mint.to_string(),
    ]
    .into_iter()
    .collect();

    let mut positions = Vec::new();
    let mut anomalies = Vec::new();
    for (key, pos) in &all_positions {
        if pos.owner != *wallet {
            errors.push(ReadError::new(
                "positions",
                ErrorClass::NotApplicable,
                format!("position {key} is owned by {}, not {wallet}", pos.owner),
            ));
            continue;
        }
        watch.insert(key.to_string());
        for idx in bin_array_indexes(pos.lower_bin_id, pos.upper_bin_id) {
            if let Some((k, _)) = found_arrays.get(&idx) {
                watch.insert(k.to_string());
            }
        }
        let t = position_totals(pos, &arrays);
        if !t.missing_bin_arrays.is_empty() {
            errors.push(ReadError::new(
                "positions",
                ErrorClass::Fatal,
                format!(
                    "position {key}: bin arrays {:?} holding its shares are not in the read set",
                    t.missing_bin_arrays
                ),
            ));
        }
        if !t.inconsistent_bins.is_empty() {
            errors.push(decode_err(
                "positions",
                format!(
                    "position {key}: fee checkpoint ahead of the bin in bins {:?}",
                    t.inconsistent_bins
                ),
            ));
        }
        if pos.is_extended() {
            anomalies.push(DlmmAnomaly::ExtendedPosition {
                position: key.to_string(),
            });
        }
        let lower_price = price_from_bin(pos.lower_bin_id, ctx.pair.bin_step, dx, dy);
        let upper_price = price_from_bin(pos.upper_bin_id, ctx.pair.bin_step, dx, dy);
        let amount_base = ui_u256(t.amount_x, dx);
        let amount_quote = ui_u256(t.amount_y, dy);
        let value_quote = amount_base * ctx.price + amount_quote;
        let base_pct_linear = token_percentages(ctx.price, lower_price, upper_price)
            .map(|c| c.token_x)
            .unwrap_or(f64::NAN);
        positions.push(DlmmPosition {
            position: key.to_string(),
            owner: pos.owner.to_string(),
            fee_owner: pos.fee_owner.to_string(),
            lower_bin_id: pos.lower_bin_id,
            upper_bin_id: pos.upper_bin_id,
            width: pos.width(),
            lower_price,
            upper_price,
            in_range: pos.lower_bin_id <= active && active <= pos.upper_bin_id,
            bins_below_active: active - pos.lower_bin_id,
            bins_above_active: pos.upper_bin_id - active,
            amount_base,
            amount_quote,
            value_quote,
            fee_base: ui_u256(t.fee_x, dx),
            fee_quote: ui_u256(t.fee_y, dy),
            total_claimed_fee_base: ui_amount(u128::from(pos.total_claimed_fee_x), dx),
            total_claimed_fee_quote: ui_amount(u128::from(pos.total_claimed_fee_y), dy),
            base_pct_value: (value_quote > 0.0)
                .then(|| amount_base * ctx.price / value_quote * 100.0),
            base_pct_linear,
            last_updated_at: pos.last_updated_at,
            complete: t.complete() && base_pct_linear.is_finite(),
        });
    }
    positions.sort_by(|a, b| {
        a.lower_bin_id
            .cmp(&b.lower_bin_id)
            .then(a.position.cmp(&b.position))
    });
    if positions.len() > 1 {
        anomalies.insert(
            0,
            DlmmAnomaly::MultiplePositions {
                count: positions.len() as u32,
            },
        );
    }
    if ctx.pair.status != PAIR_STATUS_ENABLED {
        anomalies.push(DlmmAnomaly::PoolDisabled);
    }

    let exposure = match &discovery {
        Discovery::Error { error, .. } if positions.is_empty() => Field::err(ReadError {
            field: "exposure".into(),
            ..error.clone()
        }),
        _ => {
            let base: f64 = positions.iter().map(|p| p.amount_base).sum();
            let quote: f64 = positions.iter().map(|p| p.amount_quote).sum();
            Field::ok(LpExposure {
                base,
                quote,
                claimable_base: positions.iter().map(|p| p.fee_base).sum(),
                claimable_quote: positions.iter().map(|p| p.fee_quote).sum(),
                value_quote: base * ctx.price + quote,
                full_value_base: base + quote / ctx.price,
                position_count: positions.len() as u32,
            })
        }
    };
    if let Discovery::Error { error, .. } = &discovery {
        errors.insert(
            0,
            ReadError {
                field: "discovery".into(),
                ..error.clone()
            },
        );
    }
    // The LbPair read's slot (the pool is in the set: `pool_ctx` succeeded).
    let slot = set.get(pool).map(|r| r.slot).unwrap_or(set.slot_max);
    Ok(DlmmPositions {
        wallet: wallet.to_string(),
        pool: pool.to_string(),
        slot,
        active_id: active,
        active_price: ctx.price,
        discovery,
        positions,
        exposure,
        anomalies,
        watch: watch.into_iter().collect(),
        errors,
    })
}

// ---------------------------------------------------------------------------
// Observed
// ---------------------------------------------------------------------------

fn symbol(mint: &str) -> Option<&'static str> {
    match mint {
        ids::WSOL => Some("SOL"),
        ids::USDC => Some("USDC"),
        _ => None,
    }
}

/// `x` with `sig` significant digits, plain notation (no exponent).
fn fmt_sig(x: f64, sig: i32) -> String {
    if !x.is_finite() || x == 0.0 {
        return format!("{x}");
    }
    let mag = x.abs().log10().floor() as i32;
    let decimals = (sig - 1 - mag).clamp(0, 12) as usize;
    format!("{x:.decimals$}")
}

impl Observed for DlmmPoolState {
    const SCHEMA: &'static str = "dlmm_pool/1";

    fn subject(&self) -> String {
        self.pool.clone()
    }

    fn headline(&self) -> String {
        let pair = match (symbol(&self.pair.base_mint), symbol(&self.pair.quote_mint)) {
            (Some(b), Some(q)) => format!(" {b}/{q}"),
            _ => String::new(),
        };
        let tvl = self
            .tvl_quote_onchain
            .map(|t| format!(" tvl={}", fmt_sig(t, 6)))
            .unwrap_or_default();
        let disabled = if self.enabled { "" } else { " DISABLED" };
        format!(
            "dlmm_pool {}{pair}{disabled} active_id={} price={} fee={}%{tvl}",
            self.pool,
            self.active_id,
            fmt_sig(self.active_price, 7),
            fmt_sig(self.fee.total_fee_pct, 4),
        )
    }

    fn features(&self) -> Features {
        let mut f = Features::new();
        set_int(&mut f, "active_id", Some(i64::from(self.active_id)));
        set_num(&mut f, "active_price", Some(self.active_price));
        set_int(&mut f, "bin_step", Some(i64::from(self.bin_step)));
        set_num(&mut f, "base_fee_pct", Some(self.fee.base_fee_pct));
        set_num(&mut f, "variable_fee_pct", Some(self.fee.variable_fee_pct));
        set_num(&mut f, "total_fee_pct", Some(self.fee.total_fee_pct));
        set_int(
            &mut f,
            "volatility_accumulator",
            Some(i64::from(self.fee.volatility_accumulator_now)),
        );
        set_num(&mut f, "tvl_quote", self.tvl_quote_onchain);
        let base_value = self.reserve_base.value().map(|b| b.ui * self.active_price);
        set_num(
            &mut f,
            "base_value_share",
            base_value
                .zip(self.tvl_quote_onchain)
                .filter(|(_, t)| *t > 0.0)
                .map(|(b, t)| b / t),
        );
        if let Some(d) = self.depth.value() {
            for band in d.bands.iter().filter(|b| b.complete) {
                set_num(
                    &mut f,
                    &format!("depth_value_quote_{}", band.half_width_bins),
                    Some(band.value_quote),
                );
            }
        }
        set_bool(&mut f, "enabled", Some(self.enabled));
        set_bool(&mut f, "quote_is_usd", Some(self.pair.quote_is_usd));
        set_bool(
            &mut f,
            "base_is_native_sol",
            Some(self.pair.base_is_native_sol),
        );
        f
    }

    fn slot(&self) -> Option<u64> {
        Some(self.slot)
    }

    fn status(&self) -> ObsStatus {
        if self.errors().is_empty() {
            ObsStatus::Ok
        } else {
            ObsStatus::Partial
        }
    }

    fn errors(&self) -> Vec<ReadError> {
        let mut out: Vec<ReadError> = [&self.reserve_base, &self.reserve_quote]
            .into_iter()
            .filter_map(|f| f.error().cloned())
            .collect();
        match &self.depth {
            Field::Error { error } => out.push(error.clone()),
            Field::Ok { value } if !value.missing_bin_arrays.is_empty() => {
                out.push(ReadError::new(
                    "depth",
                    ErrorClass::Fatal,
                    format!(
                        "bin arrays {:?} are not in the read set",
                        value.missing_bin_arrays
                    ),
                ))
            }
            _ => {}
        }
        out
    }
}

impl DlmmPositions {
    fn in_range_count(&self) -> usize {
        self.positions.iter().filter(|p| p.in_range).count()
    }
}

impl Observed for DlmmPositions {
    const SCHEMA: &'static str = "dlmm_positions/1";

    fn subject(&self) -> String {
        format!("{}:{}", self.wallet, self.pool)
    }

    fn headline(&self) -> String {
        let value = self
            .exposure
            .value()
            .map(|e| format!(" value_quote={}", fmt_sig(e.value_quote, 6)))
            .unwrap_or_default();
        format!(
            "dlmm_positions {} {} n={} in_range={}{value} discovery={}",
            self.wallet,
            self.pool,
            self.positions.len(),
            self.in_range_count(),
            self.discovery.label(),
        )
    }

    fn features(&self) -> Features {
        let mut f = Features::new();
        let n = self.positions.len();
        set_int(&mut f, "position_count", Some(n as i64));
        set_int(&mut f, "n_in_range", Some(self.in_range_count() as i64));
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
        if n == 1 {
            set_num(
                &mut f,
                "base_pct_linear",
                Some(self.positions[0].base_pct_linear),
            );
        }
        if let Some(e) = self.exposure.value() {
            set_num(&mut f, "lp_base", Some(e.base));
            set_num(&mut f, "lp_quote", Some(e.quote));
            set_num(&mut f, "lp_value_quote", Some(e.value_quote));
            set_num(&mut f, "lp_full_value_base", Some(e.full_value_base));
            set_num(&mut f, "claimable_base", Some(e.claimable_base));
            set_num(&mut f, "claimable_quote", Some(e.claimable_quote));
            set_num(
                &mut f,
                "claimable_value_quote",
                Some(e.claimable_base * self.active_price + e.claimable_quote),
            );
            set_num(
                &mut f,
                "base_pct_value",
                (e.value_quote > 0.0).then(|| e.base * self.active_price / e.value_quote * 100.0),
            );
        }
        set_int(&mut f, "active_id", Some(i64::from(self.active_id)));
        set_num(&mut f, "active_price", Some(self.active_price));
        set_str(&mut f, "discovery", Some(self.discovery.label()));
        set_bool(
            &mut f,
            "complete",
            Some(self.positions.iter().all(|p| p.complete)),
        );
        set_int(&mut f, "n_anomalies", Some(self.anomalies.len() as i64));
        f
    }

    fn slot(&self) -> Option<u64> {
        Some(self.slot)
    }

    fn status(&self) -> ObsStatus {
        if self.exposure.is_error() {
            return ObsStatus::Error;
        }
        if !self.errors.is_empty() || self.positions.iter().any(|p| !p.complete) {
            return ObsStatus::Partial;
        }
        if self.positions.is_empty() {
            return ObsStatus::Absent;
        }
        ObsStatus::Ok
    }

    fn errors(&self) -> Vec<ReadError> {
        self.errors.clone()
    }
}
