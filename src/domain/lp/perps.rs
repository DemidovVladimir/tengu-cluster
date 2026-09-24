//! Jupiter perps — raw decoders (Position, Custody, JLP Pool, PositionRequest),
//! the borrow-rate / accrued-fee / liquidation math ported from
//! `delta_neutral_bot/src/utils/jupiterPerps.ts:204-318`, and the typed
//! `jup_perps/1` observation ([`PerpsState`]) built from one [`AccountSet`].
//! Pure: no IO, no clocks (`now_s` is an input). PDAs are inputs — derivation
//! lives in `domain/solana.rs`.
//!
//! Layouts = Anchor IDL `delta_neutral_bot/src/idl/jupiter-perps-idl.json`
//! (offsets: `scripts/golden/perps_idl_offsets.cjs`), checked against live
//! bytes at slot 450101361 with Anchor's own coder
//! (`scripts/golden/perps_anchor_decode.cjs`). Live accounts are LONGER than
//! the IDL; the extra bytes are trailing, so decoders check `len >= min`:
//!
//! | Account | Disc = sha256("account:<Name>")[..8] | IDL len | Live len | Extra |
//! |---|---|---|---|---|
//! | Position | `aabc8fe47a40f7d0` | 210 | 216 | 6 trailing zero bytes |
//! | Custody | `01b830515d833f91` | 1060 | 2000 | trailing fields after `minInterestFeeGracePeriodSeconds` (IDL prefix verified: pool@8, mint@40, decimals@104, is_stable@105) |
//! | Pool | `f19a6d0411b16dbc` | dynamic borsh | 2000 | trailing; `maxRequestExecutionSec` = 45 at byte 364 for `name = "Pool"` + 6 custodies |
//! | PositionRequest | `0c26fac72e9a20d8` | dynamic, ≥ 220 | 312 | 6 `Option`s before `executed` |
//!
//! | Position field | @ | Custody field | @ |
//! |---|---|---|---|
//! | owner | 8 | pool / mint | 8 / 40 |
//! | pool / custody / collateralCustody | 40 / 72 / 104 | decimals / isStable | 104 / 105 |
//! | openTime / updateTime i64 | 136 / 144 | oracle account / type / maxPriceAgeSec u32 | 106 / 138 / 147 |
//! | side u8 (1 long, 2 short) | 152 | tradeImpactFeeScalar / maxLeverage / maxGlobalLong / maxGlobalShort | 151 / 175 / 183 / 191 |
//! | price / sizeUsd / collateralUsd u64 | 153 / 161 / 169 | permissions increase / decrease / collateral withdrawal | 202 / 203 / 204 |
//! | realisedPnlUsd i64 | 177 | assets owned / locked / globalShortSizes | 222 / 230 / 246 |
//! | cumulativeInterestSnapshot u128 | 185 | cumulativeInterestRate u128 / lastUpdate / hourlyFundingDbps | 262 / 278 / 286 |
//! | lockedAmount u64 | 201 | increase / decreasePositionBps / maxPositionSizeUsd | 296 / 304 / 312 |
//! | | | dovesOracle / jumpRateState (min, max, target, targetUtil) / dovesAgOracle | 320 / 352 / 384 |
//! | | | debt u128 / borrowLendInterestsAccured u128 | 1004 / 1020 |
//!
//! Units: USD amounts and prices are 6-dp fixed point; utilization, rates and
//! cumulative interest use `RATE_POWER` = 1e9; `maxLeverage` is in bps
//! (5_000_000 = 500x). Integer math is u128 / i128 with checked ops.
//!
//! Differences from the bot (deliberate):
//!
//! | Item | Bot | Here |
//! |---|---|---|
//! | Failed custody / position read | `0` or `null` defaults | `Field::Error` (BUG-023) |
//! | `hourlyFundingDbps != 0` (linear mechanism) | jump curve anyway | custody rates `Error{NotApplicable}` |
//! | Side delta | `notional / entry`, spot fallback, else 0 | same, but no price at all ⇒ `Error` |
//! | Carry sign | negative = pays (`carryRateBps`) | `carry_cost_bps` positive = pays |
//! | Collateral ratio when flat | `Infinity` | `None` |

// Consumed by the jup_perps / lp_snapshot / hedge_decide glue (stage 3).
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

use crate::domain::observation::{
    set_bool, set_int, set_num, ErrorClass, Features, Field, ObsStatus, Observed, ReadError,
};
use crate::domain::solana::{ids, AccountSet, Pubkey};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

pub const POSITION_DISC: [u8; 8] = [170, 188, 143, 228, 122, 64, 247, 208];
pub const CUSTODY_DISC: [u8; 8] = [1, 184, 48, 81, 93, 131, 63, 145];
pub const POOL_DISC: [u8; 8] = [241, 154, 109, 4, 17, 177, 109, 188];
pub const POSITION_REQUEST_DISC: [u8; 8] = [12, 38, 250, 199, 46, 154, 32, 216];

/// IDL sizes (live accounts are longer: 216 / 2000).
pub const POSITION_MIN_LEN: usize = 210;
pub const CUSTODY_MIN_LEN: usize = 1060;
/// Fixed prefix (203) + 6 `None` tags + executed + counter + bump + `None` referral.
pub const POSITION_REQUEST_MIN_LEN: usize = 220;

/// 6-dp fixed point (sizeUsd, collateralUsd, price).
pub const USD_PRECISION: f64 = 1_000_000.0;
pub const RATE_POWER: u128 = 1_000_000_000;
pub const BPS_POWER: u128 = 10_000;
pub const HOURS_IN_A_YEAR: u128 = 24 * 365;

/// Pool `name` / `custodies` sanity caps for the dynamic borsh parse.
const MAX_POOL_NAME: usize = 64;
const MAX_POOL_CUSTODIES: usize = 32;

// ---------------------------------------------------------------------------
// Raw decoded accounts
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Long,
    Short,
}

impl Side {
    pub fn as_str(self) -> &'static str {
        match self {
            Side::Long => "long",
            Side::Short => "short",
        }
    }
    /// On-chain `Side` enum byte (0 = None).
    pub fn byte(self) -> u8 {
        match self {
            Side::Long => 1,
            Side::Short => 2,
        }
    }
    fn from_byte(b: u8) -> Option<Side> {
        match b {
            1 => Some(Side::Long),
            2 => Some(Side::Short),
            _ => None,
        }
    }
    /// Collateral custody of a SOL position: SOL for longs, USDC for shorts.
    pub fn collateral_custody(self) -> &'static str {
        match self {
            Side::Long => ids::JUP_CUSTODY_SOL,
            Side::Short => ids::JUP_CUSTODY_USDC,
        }
    }
}

/// `Position` account (IDL prefix, 210 bytes).
#[derive(Debug, Clone, PartialEq)]
pub struct JupPosition {
    pub owner: Pubkey,
    pub pool: Pubkey,
    pub custody: Pubkey,
    pub collateral_custody: Pubkey,
    pub open_time: i64,
    pub update_time: i64,
    /// Raw `Side` byte: 0 None, 1 Long, 2 Short.
    pub side: u8,
    pub price: u64,
    pub size_usd: u64,
    pub collateral_usd: u64,
    pub realised_pnl_usd: i64,
    pub cumulative_interest_snapshot: u128,
    pub locked_amount: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JumpRateState {
    pub min_rate_bps: u64,
    pub max_rate_bps: u64,
    pub target_rate_bps: u64,
    /// `RATE_POWER` units.
    pub target_utilization_rate: u64,
}

/// `Custody` account (IDL prefix, 1060 bytes; only the fields tengu uses).
#[derive(Debug, Clone, PartialEq)]
pub struct JupCustody {
    pub pool: Pubkey,
    pub mint: Pubkey,
    pub token_account: Pubkey,
    pub decimals: u8,
    pub is_stable: bool,
    pub oracle_account: Pubkey,
    pub oracle_type: u8,
    pub max_price_age_sec: u32,
    pub trade_impact_fee_scalar: u64,
    /// bps: 5_000_000 = 500x.
    pub max_leverage: u64,
    pub max_global_long_sizes: u64,
    pub max_global_short_sizes: u64,
    pub allow_increase_position: bool,
    pub allow_decrease_position: bool,
    pub allow_collateral_withdrawal: bool,
    pub owned: u64,
    pub locked: u64,
    pub global_short_sizes: u64,
    pub cumulative_interest_rate: u128,
    pub funding_last_update: i64,
    pub hourly_funding_dbps: u64,
    pub increase_position_bps: u64,
    pub decrease_position_bps: u64,
    pub max_position_size_usd: u64,
    pub doves_oracle: Pubkey,
    pub jump: JumpRateState,
    pub doves_ag_oracle: Pubkey,
    pub debt: u128,
    pub borrow_lend_interests_accured: u128,
}

/// `Pool` account — dynamic borsh prefix up to `maxRequestExecutionSec`.
#[derive(Debug, Clone, PartialEq)]
pub struct JlpPool {
    pub name: String,
    pub custodies: Vec<Pubkey>,
    pub aum_usd: u128,
    pub fee_apr_bps: u64,
    pub max_request_execution_sec: i64,
}

/// `PositionRequest` account (market / trigger request awaiting a keeper).
#[derive(Debug, Clone, PartialEq)]
pub struct JupPositionRequest {
    pub owner: Pubkey,
    pub pool: Pubkey,
    pub custody: Pubkey,
    pub position: Pubkey,
    pub mint: Pubkey,
    pub open_time: i64,
    pub update_time: i64,
    pub size_usd_delta: u64,
    pub collateral_delta: u64,
    /// 0 None, 1 Increase, 2 Decrease.
    pub request_change: u8,
    /// 0 Market, 1 Trigger.
    pub request_type: u8,
    pub side: u8,
    pub trigger_price: Option<u64>,
    pub entire_position: Option<bool>,
    pub executed: bool,
    pub counter: u64,
}

// ---------------------------------------------------------------------------
// Byte readers
// ---------------------------------------------------------------------------

fn bytes_at<const N: usize>(d: &[u8], off: usize) -> Result<[u8; N], String> {
    d.get(off..off + N)
        .and_then(|s| s.try_into().ok())
        .ok_or_else(|| format!("{N} bytes at {off} past end of {} bytes", d.len()))
}

fn u8_at(d: &[u8], off: usize) -> Result<u8, String> {
    Ok(bytes_at::<1>(d, off)?[0])
}

/// Borsh bool: exactly 0 or 1.
fn bool_at(d: &[u8], off: usize) -> Result<bool, String> {
    match u8_at(d, off)? {
        0 => Ok(false),
        1 => Ok(true),
        b => Err(format!("bool at {off} is {b}")),
    }
}

fn u32_at(d: &[u8], off: usize) -> Result<u32, String> {
    Ok(u32::from_le_bytes(bytes_at(d, off)?))
}

fn u64_at(d: &[u8], off: usize) -> Result<u64, String> {
    Ok(u64::from_le_bytes(bytes_at(d, off)?))
}

fn i64_at(d: &[u8], off: usize) -> Result<i64, String> {
    Ok(i64::from_le_bytes(bytes_at(d, off)?))
}

fn u128_at(d: &[u8], off: usize) -> Result<u128, String> {
    Ok(u128::from_le_bytes(bytes_at(d, off)?))
}

fn pk_at(d: &[u8], off: usize) -> Result<Pubkey, String> {
    Ok(Pubkey(bytes_at(d, off)?))
}

/// Sequential borsh reader for the dynamic layouts.
struct Cursor<'a> {
    d: &'a [u8],
    off: usize,
}

impl<'a> Cursor<'a> {
    fn new(d: &'a [u8], off: usize) -> Self {
        Self { d, off }
    }
    fn next<T>(
        &mut self,
        n: usize,
        f: impl Fn(&[u8], usize) -> Result<T, String>,
    ) -> Result<T, String> {
        let v = f(self.d, self.off)?;
        self.off += n;
        Ok(v)
    }
    fn u8(&mut self) -> Result<u8, String> {
        self.next(1, u8_at)
    }
    fn bool(&mut self) -> Result<bool, String> {
        self.next(1, bool_at)
    }
    fn u32(&mut self) -> Result<u32, String> {
        self.next(4, u32_at)
    }
    fn u64(&mut self) -> Result<u64, String> {
        self.next(8, u64_at)
    }
    fn i64(&mut self) -> Result<i64, String> {
        self.next(8, i64_at)
    }
    fn u128(&mut self) -> Result<u128, String> {
        self.next(16, u128_at)
    }
    fn pubkey(&mut self) -> Result<Pubkey, String> {
        self.next(32, pk_at)
    }
    fn skip(&mut self, n: usize) -> Result<(), String> {
        if self.off + n > self.d.len() {
            return Err(format!(
                "skip {n} at {} past end of {} bytes",
                self.off,
                self.d.len()
            ));
        }
        self.off += n;
        Ok(())
    }
    /// Borsh `Option<T>`: tag 0 = None, 1 = Some.
    fn option<T>(
        &mut self,
        read: impl Fn(&mut Self) -> Result<T, String>,
    ) -> Result<Option<T>, String> {
        let at = self.off;
        match self.u8()? {
            0 => Ok(None),
            1 => read(self).map(Some),
            t => Err(format!("option tag at {at} is {t}")),
        }
    }
}

fn check_disc(d: &[u8], disc: &[u8; 8], min_len: usize, name: &str) -> Result<(), String> {
    if d.len() < min_len {
        return Err(format!("{name}: {} bytes < min {min_len}", d.len()));
    }
    if d[..8] != disc[..] {
        return Err(format!(
            "{name}: discriminator {} != {}",
            hex8(&d[..8]),
            hex8(disc)
        ));
    }
    Ok(())
}

fn hex8(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

// ---------------------------------------------------------------------------
// Decoders (discriminator + minimum length; owner is checked by the caller)
// ---------------------------------------------------------------------------

pub fn decode_position(d: &[u8]) -> Result<JupPosition, String> {
    check_disc(d, &POSITION_DISC, POSITION_MIN_LEN, "Position")?;
    Ok(JupPosition {
        owner: pk_at(d, 8)?,
        pool: pk_at(d, 40)?,
        custody: pk_at(d, 72)?,
        collateral_custody: pk_at(d, 104)?,
        open_time: i64_at(d, 136)?,
        update_time: i64_at(d, 144)?,
        side: u8_at(d, 152)?,
        price: u64_at(d, 153)?,
        size_usd: u64_at(d, 161)?,
        collateral_usd: u64_at(d, 169)?,
        realised_pnl_usd: i64_at(d, 177)?,
        cumulative_interest_snapshot: u128_at(d, 185)?,
        locked_amount: u64_at(d, 201)?,
    })
}

pub fn decode_custody(d: &[u8]) -> Result<JupCustody, String> {
    check_disc(d, &CUSTODY_DISC, CUSTODY_MIN_LEN, "Custody")?;
    Ok(JupCustody {
        pool: pk_at(d, 8)?,
        mint: pk_at(d, 40)?,
        token_account: pk_at(d, 72)?,
        decimals: u8_at(d, 104)?,
        is_stable: bool_at(d, 105)?,
        oracle_account: pk_at(d, 106)?,
        oracle_type: u8_at(d, 138)?,
        max_price_age_sec: u32_at(d, 147)?,
        trade_impact_fee_scalar: u64_at(d, 151)?,
        max_leverage: u64_at(d, 175)?,
        max_global_long_sizes: u64_at(d, 183)?,
        max_global_short_sizes: u64_at(d, 191)?,
        allow_increase_position: bool_at(d, 202)?,
        allow_decrease_position: bool_at(d, 203)?,
        allow_collateral_withdrawal: bool_at(d, 204)?,
        owned: u64_at(d, 222)?,
        locked: u64_at(d, 230)?,
        global_short_sizes: u64_at(d, 246)?,
        cumulative_interest_rate: u128_at(d, 262)?,
        funding_last_update: i64_at(d, 278)?,
        hourly_funding_dbps: u64_at(d, 286)?,
        increase_position_bps: u64_at(d, 296)?,
        decrease_position_bps: u64_at(d, 304)?,
        max_position_size_usd: u64_at(d, 312)?,
        doves_oracle: pk_at(d, 320)?,
        jump: JumpRateState {
            min_rate_bps: u64_at(d, 352)?,
            max_rate_bps: u64_at(d, 360)?,
            target_rate_bps: u64_at(d, 368)?,
            target_utilization_rate: u64_at(d, 376)?,
        },
        doves_ag_oracle: pk_at(d, 384)?,
        debt: u128_at(d, 1004)?,
        borrow_lend_interests_accured: u128_at(d, 1020)?,
    })
}

/// Hand-parsed borsh: name string, custodies Vec<Pubkey>, aumUsd u128,
/// Limit (u128, u128, u64), Fees (9 × u64), PoolApr (i64, u64 feeAprBps, u64),
/// maxRequestExecutionSec i64.
pub fn decode_pool(d: &[u8]) -> Result<JlpPool, String> {
    check_disc(d, &POOL_DISC, 8, "Pool")?;
    let mut c = Cursor::new(d, 8);
    let name_len = c.u32()? as usize;
    if name_len > MAX_POOL_NAME {
        return Err(format!("Pool: name length {name_len} > {MAX_POOL_NAME}"));
    }
    let name_at = c.off;
    c.skip(name_len)?;
    let name = std::str::from_utf8(&d[name_at..name_at + name_len])
        .map_err(|e| format!("Pool: name not utf-8: {e}"))?
        .to_string();
    let n = c.u32()? as usize;
    if n > MAX_POOL_CUSTODIES {
        return Err(format!("Pool: {n} custodies > {MAX_POOL_CUSTODIES}"));
    }
    let custodies = (0..n).map(|_| c.pubkey()).collect::<Result<Vec<_>, _>>()?;
    let aum_usd = c.u128()?;
    c.skip(16 + 16 + 8)?; // Limit
    c.skip(9 * 8)?; // Fees
    c.skip(8)?; // PoolApr.lastUpdated
    let fee_apr_bps = c.u64()?;
    c.skip(8)?; // PoolApr.realizedFeeUsd
    let max_request_execution_sec = c.i64()?;
    Ok(JlpPool {
        name,
        custodies,
        aum_usd,
        fee_apr_bps,
        max_request_execution_sec,
    })
}

pub fn decode_position_request(d: &[u8]) -> Result<JupPositionRequest, String> {
    check_disc(
        d,
        &POSITION_REQUEST_DISC,
        POSITION_REQUEST_MIN_LEN,
        "PositionRequest",
    )?;
    let mut c = Cursor::new(d, 8);
    let owner = c.pubkey()?;
    let pool = c.pubkey()?;
    let custody = c.pubkey()?;
    let position = c.pubkey()?;
    let mint = c.pubkey()?;
    let open_time = c.i64()?;
    let update_time = c.i64()?;
    let size_usd_delta = c.u64()?;
    let collateral_delta = c.u64()?;
    let request_change = c.u8()?;
    let request_type = c.u8()?;
    let side = c.u8()?;
    let _price_slippage = c.option(Cursor::u64)?;
    let _jupiter_minimum_out = c.option(Cursor::u64)?;
    let _pre_swap_amount = c.option(Cursor::u64)?;
    let trigger_price = c.option(Cursor::u64)?;
    let _trigger_above_threshold = c.option(Cursor::bool)?;
    let entire_position = c.option(Cursor::bool)?;
    let executed = c.bool()?;
    let counter = c.u64()?;
    Ok(JupPositionRequest {
        owner,
        pool,
        custody,
        position,
        mint,
        open_time,
        update_time,
        size_usd_delta,
        collateral_delta,
        request_change,
        request_type,
        side,
        trigger_price,
        entire_position,
        executed,
        counter,
    })
}

// ---------------------------------------------------------------------------
// Math — faithful port of jupiterPerps.ts:204-318 (u128 / i128, checked)
// ---------------------------------------------------------------------------

/// Ceil division for non-negative operands (Jupiter's `divCeil`).
fn div_ceil(a: u128, b: u128) -> Result<u128, String> {
    if b == 0 {
        return Err("division by zero".into());
    }
    Ok(a / b + u128::from(a % b != 0))
}

fn ovf(what: &str) -> String {
    format!("{what}: arithmetic overflow")
}

/// `getDebt`: outstanding borrow-lend debt in token units,
/// `ceil(max(debt − borrowLendInterestsAccured, 0) / RATE_POWER)`.
pub fn debt_tokens(c: &JupCustody) -> u128 {
    let d = c.debt.saturating_sub(c.borrow_lend_interests_accured);
    d / RATE_POWER + u128::from(d % RATE_POWER != 0)
}

/// Jump-curve borrow rate of a custody.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BorrowRate {
    /// `(locked + debt) × RATE_POWER / (owned + debt)`; 0 when either is 0.
    pub utilization_raw: u128,
    /// Hourly rate in `RATE_POWER` units.
    pub hourly_rate: u128,
}

impl BorrowRate {
    pub fn utilization(&self) -> f64 {
        self.utilization_raw as f64 / RATE_POWER as f64
    }
    /// `borrowAprPct`: `(hourly / RATE_POWER) × 8760 × 100` (same float
    /// operation order as the TS).
    pub fn apr_pct(&self) -> f64 {
        (self.hourly_rate as f64 / RATE_POWER as f64) * HOURS_IN_A_YEAR as f64 * 100.0
    }
}

/// `hourlyBorrowRate` (jump mechanism). Like the TS it ignores
/// `hourlyFundingDbps`; [`rate_mechanism`] reports a linear custody.
pub fn hourly_borrow_rate(c: &JupCustody) -> Result<BorrowRate, String> {
    let debt = debt_tokens(c);
    let owned = u128::from(c.owned) + debt;
    let locked = u128::from(c.locked) + debt;
    if owned == 0 || locked == 0 {
        return Ok(BorrowRate {
            utilization_raw: 0,
            hourly_rate: 0,
        });
    }
    let util = locked
        .checked_mul(RATE_POWER)
        .ok_or_else(|| ovf("utilization"))?
        / owned;
    let j = c.jump;
    let (min, max, target, target_util) = (
        u128::from(j.min_rate_bps),
        u128::from(j.max_rate_bps),
        u128::from(j.target_rate_bps),
        u128::from(j.target_utilization_rate),
    );
    let yearly_bps = if util <= target_util {
        let span = target
            .checked_sub(min)
            .ok_or_else(|| format!("jump curve target {target} < min {min} bps"))?;
        let rise = span.checked_mul(util).ok_or_else(|| ovf("jump rise"))?;
        div_ceil(rise, target_util)
            .map_err(|e| format!("jump curve target utilization is 0: {e}"))?
            + min
    } else {
        let rate_diff = max.saturating_sub(target);
        let util_diff = util - target_util;
        let denom = RATE_POWER.saturating_sub(target_util);
        let rise = rate_diff
            .checked_mul(util_diff)
            .ok_or_else(|| ovf("jump rise"))?;
        div_ceil(rise, denom).map_err(|_| "borrow-rate denominator is 0".to_string())? + target
    };
    let yearly = yearly_bps
        .checked_mul(RATE_POWER)
        .ok_or_else(|| ovf("yearly rate"))?
        / BPS_POWER;
    Ok(BorrowRate {
        utilization_raw: util,
        hourly_rate: yearly / HOURS_IN_A_YEAR,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateMechanism {
    Jump,
    /// `hourlyFundingDbps != 0`: the linear path is not ported (as in the bot).
    LinearUnsupported,
}

pub fn rate_mechanism(c: &JupCustody) -> RateMechanism {
    if c.hourly_funding_dbps == 0 {
        RateMechanism::Jump
    } else {
        RateMechanism::LinearUnsupported
    }
}

/// `accruedBorrowFeeUsdBn`: `(collateral custody cumulativeInterestRate −
/// position snapshot) × sizeUsd / RATE_POWER`, 6-dp USD (truncating like BN).
pub fn accrued_borrow_fee_raw(p: &JupPosition, collateral: &JupCustody) -> Result<i128, String> {
    let rate =
        i128::try_from(collateral.cumulative_interest_rate).map_err(|_| ovf("cumulative rate"))?;
    let snap = i128::try_from(p.cumulative_interest_snapshot).map_err(|_| ovf("snapshot"))?;
    (rate - snap)
        .checked_mul(i128::from(p.size_usd))
        .map(|x| x / RATE_POWER as i128)
        .ok_or_else(|| ovf("accrued borrow fee"))
}

/// `computeLiquidationPrice` (Jupiter reference `getLiquidationPrice`), human
/// USD. `market` = SOL custody (fee bps, max leverage, impact scalar);
/// `collateral` = the side's collateral custody (accrued rate). `Ok(None)`
/// when there is no position (`sizeUsd == 0`) or `maxLeverage == 0`; a
/// non-positive result is `Some(0.0)` like the TS.
pub fn liquidation_price_usd(
    p: &JupPosition,
    market: &JupCustody,
    collateral: &JupCustody,
) -> Result<Option<f64>, String> {
    if p.size_usd == 0 || market.max_leverage == 0 {
        return Ok(None);
    }
    let size = i128::from(p.size_usd);
    let bps = BPS_POWER as i128;
    let impact_bps = if market.trade_impact_fee_scalar == 0 {
        0
    } else {
        div_ceil(
            p.size_usd as u128 * BPS_POWER,
            u128::from(market.trade_impact_fee_scalar),
        )? as i128
    };
    let total_fee_bps = i128::from(market.decrease_position_bps) + impact_bps;
    let close_fee = size
        .checked_mul(total_fee_bps)
        .ok_or_else(|| ovf("close fee"))?
        / bps;
    let borrow_fee = accrued_borrow_fee_raw(p, collateral)?;
    let max_loss = size * bps / i128::from(market.max_leverage) + close_fee + borrow_fee;
    let margin = i128::from(p.collateral_usd);
    let price = i128::from(p.price);
    let diff = (max_loss - margin)
        .abs()
        .checked_mul(price)
        .ok_or_else(|| ovf("price diff"))?
        / size;
    let under_margined = max_loss > margin;
    let long = p.side == Side::Long.byte();
    let liq = match (long, under_margined) {
        (true, false) | (false, true) => price - diff,
        (true, true) | (false, false) => price + diff,
    };
    let liq = liq as f64 / USD_PRECISION;
    Ok(Some(if liq > 0.0 { liq } else { 0.0 }))
}

/// BUG-025: a side's SOL size is `notional / entry` (entry is frozen, spot
/// only as fallback). `None` when neither price is usable — never 0.
pub fn side_base_sol(
    notional_usd: f64,
    entry_price_usd: f64,
    spot_usd: Option<f64>,
) -> Option<f64> {
    let basis = if entry_price_usd > 0.0 {
        Some(entry_price_usd)
    } else {
        spot_usd.filter(|p| *p > 0.0)
    };
    basis.map(|b| notional_usd / b)
}

// ---------------------------------------------------------------------------
// Typed outputs
// ---------------------------------------------------------------------------

/// Borrow / fee / limit view of one custody (design-1 §7.2 `CustodyRates` +
/// `funding_age_secs`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CustodyRates {
    pub custody: String,
    pub mint: String,
    pub decimals: u8,
    pub is_stable: bool,
    pub utilization: f64,
    pub borrow_apr_pct: f64,
    pub rate_mechanism: RateMechanism,
    /// u128 as a decimal string (`RATE_POWER` units).
    pub cumulative_interest_rate: String,
    pub open_fee_bps: u64,
    pub close_fee_bps: u64,
    pub max_leverage_x: f64,
    pub trade_impact_fee_scalar: String,
    pub max_position_size_usd: f64,
    pub global_short_usd: f64,
    pub max_global_short_usd: f64,
    pub max_global_long_usd: f64,
    pub allow_increase: bool,
    pub allow_decrease: bool,
    pub allow_collateral_withdrawal: bool,
    pub oracle_account: String,
    pub doves_oracle: String,
    pub doves_ag_oracle: String,
    pub max_price_age_sec: u32,
    /// `now_s − fundingRateState.lastUpdate` (≥ 0): age of the cumulative rate.
    pub funding_age_secs: i64,
}

impl CustodyRates {
    /// Error when the mechanism is not the ported jump curve or the math fails.
    pub fn from_custody(key: &Pubkey, c: &JupCustody, now_s: i64) -> Result<CustodyRates, String> {
        let mechanism = rate_mechanism(c);
        if mechanism != RateMechanism::Jump {
            return Err(format!(
                "custody {key}: hourlyFundingDbps={} — linear borrow-rate mechanism not ported",
                c.hourly_funding_dbps
            ));
        }
        let rate = hourly_borrow_rate(c).map_err(|e| format!("custody {key}: {e}"))?;
        Ok(CustodyRates {
            custody: key.to_string(),
            mint: c.mint.to_string(),
            decimals: c.decimals,
            is_stable: c.is_stable,
            utilization: rate.utilization(),
            borrow_apr_pct: rate.apr_pct(),
            rate_mechanism: mechanism,
            cumulative_interest_rate: c.cumulative_interest_rate.to_string(),
            open_fee_bps: c.increase_position_bps,
            close_fee_bps: c.decrease_position_bps,
            max_leverage_x: c.max_leverage as f64 / BPS_POWER as f64,
            trade_impact_fee_scalar: c.trade_impact_fee_scalar.to_string(),
            max_position_size_usd: c.max_position_size_usd as f64 / USD_PRECISION,
            global_short_usd: c.global_short_sizes as f64 / USD_PRECISION,
            max_global_short_usd: c.max_global_short_sizes as f64 / USD_PRECISION,
            max_global_long_usd: c.max_global_long_sizes as f64 / USD_PRECISION,
            allow_increase: c.allow_increase_position,
            allow_decrease: c.allow_decrease_position,
            allow_collateral_withdrawal: c.allow_collateral_withdrawal,
            oracle_account: c.oracle_account.to_string(),
            doves_oracle: c.doves_oracle.to_string(),
            doves_ag_oracle: c.doves_ag_oracle.to_string(),
            max_price_age_sec: c.max_price_age_sec,
            funding_age_secs: now_s.saturating_sub(c.funding_last_update).max(0),
        })
    }

    /// Short open-interest headroom, USD (`maxGlobalShortSizes − globalShortSizes`).
    pub fn short_oi_headroom_usd(&self) -> f64 {
        self.max_global_short_usd - self.global_short_usd
    }
}

/// One open perp side (design-1 §7.2 `PerpSide`). A flat side is
/// `Field::Absent` in [`PerpsState`], never a zero-sized `PerpSide`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PerpSide {
    pub side: Side,
    pub position_pda: String,
    /// `sizeUsd` — notional frozen at entry.
    pub notional_usd: f64,
    pub collateral_usd: f64,
    pub entry_price_usd: f64,
    /// BUG-025: `notional / entry` (spot only when entry is 0).
    pub base_sol: f64,
    /// Price PnL at `oracle_usd` (borrow fees separate); `None` without an oracle.
    pub unrealized_pnl_usd: Option<f64>,
    pub accrued_borrow_fee_usd: f64,
    pub liquidation_price_usd: Option<f64>,
    /// Signed distance to liquidation as a fraction of the oracle price:
    /// long `(oracle − liq) / oracle`, short `(liq − oracle) / oracle`;
    /// negative = past liquidation.
    pub liq_distance_ratio: Option<f64>,
    /// Borrow APR of the side's collateral custody in bps; positive = pays.
    pub carry_cost_bps: f64,
    pub open_time: i64,
    pub update_time: i64,
    pub realised_pnl_usd: f64,
}

/// Keeper request status (design-1 §7.2 `RequestStatus`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestStatus {
    pub position_request: String,
    /// `false` = the account is gone (executed and closed, or never created).
    pub exists: bool,
    pub executed: bool,
    /// `now_s − openTime` while the account exists.
    pub age_secs: Option<i64>,
    /// Exists, not executed, older than the pool's `maxRequestExecutionSec`
    /// (no grace — callers add theirs to `age_secs`).
    pub expired: bool,
}

/// `jup_perps/1:<wallet>` — both SOL position sides + both custodies' rates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PerpsState {
    pub wallet: String,
    /// Oldest slot among the accounts read.
    pub slot: u64,
    pub pool: String,
    /// Oracle price used for PnL / liquidation distance (input; `None` = none).
    pub oracle_usd: Option<f64>,
    /// `Absent` = flat (account missing or `sizeUsd == 0`).
    pub long: Field<PerpSide>,
    pub short: Field<PerpSide>,
    pub sol: Field<CustodyRates>,
    pub usdc: Field<CustodyRates>,
    pub max_request_execution_sec: Field<i64>,
    pub both_sides_open: bool,
    /// Σ collateral / Σ notional over open sides; `None` when flat or a side
    /// failed to read (never `Infinity`).
    pub collateral_ratio: Option<f64>,
    /// Every pubkey read (full base58): the phase-5 subscription set.
    pub watch: Vec<String>,
}

impl PerpsState {
    pub fn side(&self, side: Side) -> &Field<PerpSide> {
        match side {
            Side::Long => &self.long,
            Side::Short => &self.short,
        }
    }

    /// SOL held by a side: 0 when flat, `None` when the read failed.
    pub fn base_sol(&self, side: Side) -> Option<f64> {
        match self.side(side) {
            Field::Ok { value } => Some(value.base_sol),
            Field::Absent => Some(0.0),
            Field::Error { .. } => None,
        }
    }

    /// Long SOL − short SOL; `None` if either side failed.
    pub fn net_perp_sol(&self) -> Option<f64> {
        Some(self.base_sol(Side::Long)? - self.base_sol(Side::Short)?)
    }

    /// Carry cost of holding `side` (open or prospective), bps APR, positive
    /// = pays: borrow APR of its collateral custody (SOL long, USDC short).
    pub fn carry_cost_bps(&self, side: Side) -> Option<f64> {
        let rates = match side {
            Side::Long => &self.sol,
            Side::Short => &self.usdc,
        };
        rates.value().map(|r| r.borrow_apr_pct * 100.0)
    }

    fn field_errors(&self) -> Vec<ReadError> {
        [
            self.long.error(),
            self.short.error(),
            self.sol.error(),
            self.usdc.error(),
            self.max_request_execution_sec.error(),
        ]
        .into_iter()
        .flatten()
        .cloned()
        .collect()
    }
}

impl Observed for PerpsState {
    const SCHEMA: &'static str = "jup_perps/1";

    fn subject(&self) -> String {
        self.wallet.clone()
    }

    fn headline(&self) -> String {
        let side = |f: &Field<PerpSide>| match f {
            Field::Ok { value } => {
                format!("${:.2}@{:.2}", value.notional_usd, value.entry_price_usd)
            }
            Field::Absent => "flat".to_string(),
            Field::Error { .. } => "error".to_string(),
        };
        let apr = |f: &Field<CustodyRates>| match f.value() {
            Some(r) => format!("{:.2}%", r.borrow_apr_pct),
            None => "error".to_string(),
        };
        format!(
            "jup_perps wallet={} long={} short={} borrow_apr sol={} usdc={}",
            self.wallet,
            side(&self.long),
            side(&self.short),
            apr(&self.sol),
            apr(&self.usdc)
        )
    }

    fn features(&self) -> Features {
        let mut f = Features::new();
        // jup_perps_market rows (design-1 §7.3).
        let sol = self.sol.value();
        let usdc = self.usdc.value();
        set_num(&mut f, "sol_borrow_apr_pct", sol.map(|r| r.borrow_apr_pct));
        set_num(
            &mut f,
            "usdc_borrow_apr_pct",
            usdc.map(|r| r.borrow_apr_pct),
        );
        set_num(&mut f, "sol_util", sol.map(|r| r.utilization));
        set_num(&mut f, "usdc_util", usdc.map(|r| r.utilization));
        set_int(&mut f, "open_fee_bps", sol.map(|r| r.open_fee_bps as i64));
        set_int(&mut f, "close_fee_bps", sol.map(|r| r.close_fee_bps as i64));
        set_num(&mut f, "max_leverage_x", sol.map(|r| r.max_leverage_x));
        set_bool(&mut f, "increase_allowed", sol.map(|r| r.allow_increase));
        set_num(
            &mut f,
            "short_oi_headroom_usd",
            sol.map(|r| r.short_oi_headroom_usd()),
        );
        set_int(
            &mut f,
            "max_request_execution_sec",
            self.max_request_execution_sec.value().copied(),
        );
        // Perp side rows.
        let open = |s: &Field<PerpSide>| match s {
            Field::Ok { .. } => Some(true),
            Field::Absent => Some(false),
            Field::Error { .. } => None,
        };
        set_bool(&mut f, "long_open", open(&self.long));
        set_bool(&mut f, "short_open", open(&self.short));
        set_num(&mut f, "perp_long_sol", self.base_sol(Side::Long));
        set_num(&mut f, "perp_short_sol", self.base_sol(Side::Short));
        set_num(&mut f, "net_perp_sol", self.net_perp_sol());
        let long = self.long.value();
        let short = self.short.value();
        let sum = |g: fn(&PerpSide) -> f64| -> Option<f64> {
            if self.long.is_error() || self.short.is_error() {
                return None;
            }
            Some(long.map(g).unwrap_or(0.0) + short.map(g).unwrap_or(0.0))
        };
        set_num(&mut f, "perp_notional_usd", sum(|s| s.notional_usd));
        set_num(
            &mut f,
            "accrued_borrow_fee_usd",
            sum(|s| s.accrued_borrow_fee_usd),
        );
        set_num(&mut f, "collateral_ratio", self.collateral_ratio);
        let liq_min = [long, short]
            .into_iter()
            .flatten()
            .filter_map(|s| s.liq_distance_ratio)
            .fold(None, |m: Option<f64>, x| Some(m.map_or(x, |m| m.min(x))));
        set_num(&mut f, "liq_distance_min", liq_min);
        set_num(
            &mut f,
            "long_liq_price",
            long.and_then(|s| s.liquidation_price_usd),
        );
        set_num(
            &mut f,
            "short_liq_price",
            short.and_then(|s| s.liquidation_price_usd),
        );
        set_num(
            &mut f,
            "long_pnl_usd",
            long.and_then(|s| s.unrealized_pnl_usd),
        );
        set_num(
            &mut f,
            "short_pnl_usd",
            short.and_then(|s| s.unrealized_pnl_usd),
        );
        set_num(&mut f, "carry_long_bps", self.carry_cost_bps(Side::Long));
        set_num(&mut f, "carry_short_bps", self.carry_cost_bps(Side::Short));
        set_bool(&mut f, "both_sides_open", Some(self.both_sides_open));
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

    /// `Error` when neither side could be read; `Partial` when any field failed.
    fn status(&self) -> ObsStatus {
        if self.long.is_error() && self.short.is_error() {
            ObsStatus::Error
        } else if self.field_errors().is_empty() {
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
// Builders
// ---------------------------------------------------------------------------

/// Accounts [`build_perps`] reads, in GMA order: long PDA, short PDA, SOL
/// custody, USDC custody, JLP pool.
pub fn perps_keys(long_pda: &Pubkey, short_pda: &Pubkey) -> Vec<Pubkey> {
    vec![
        *long_pda,
        *short_pda,
        ids::key(ids::JUP_CUSTODY_SOL),
        ids::key(ids::JUP_CUSTODY_USDC),
        ids::key(ids::JLP_POOL),
    ]
}

fn decode_err(field: &str, message: impl Into<String>) -> ReadError {
    ReadError::new(field, ErrorClass::Decode, message)
}

/// Data of a perps-program account. `Ok(None)` = absent (and, when
/// `system_empty_is_absent`, a data-less System-owned account — lamports sent
/// to an unused PDA). Missing from the set, wrong owner, short or wrong
/// discriminator ⇒ `ReadError`.
fn perps_account(
    set: &AccountSet,
    key: &Pubkey,
    disc: &[u8; 8],
    min_len: usize,
    field: &str,
    system_empty_is_absent: bool,
) -> Result<Option<Vec<u8>>, ReadError> {
    let read = set.get(key).ok_or_else(|| {
        ReadError::new(
            field,
            ErrorClass::Fatal,
            format!("account {key} was not read"),
        )
    })?;
    let Some(owner) = read.owner() else {
        return Ok(None);
    };
    let data = read
        .data()
        .ok_or_else(|| decode_err(field, format!("account {key}: data is not valid base64")))?;
    let program = ids::key(ids::JUP_PERPS);
    if *owner != program {
        if system_empty_is_absent && *owner == ids::key(ids::SYSTEM) && data.is_empty() {
            return Ok(None);
        }
        return Err(decode_err(
            field,
            format!("account {key}: owner {owner}, expected {program}"),
        ));
    }
    check_disc(&data, disc, min_len, "account")
        .map_err(|e| decode_err(field, format!("account {key}: {e}")))?;
    Ok(Some(data))
}

fn load_custody(
    set: &AccountSet,
    key: &Pubkey,
    mint: &str,
    field: &str,
) -> Result<JupCustody, ReadError> {
    let data = perps_account(set, key, &CUSTODY_DISC, CUSTODY_MIN_LEN, field, false)?
        .ok_or_else(|| decode_err(field, format!("custody {key} is absent")))?;
    let c = decode_custody(&data).map_err(|e| decode_err(field, format!("custody {key}: {e}")))?;
    let pool = ids::key(ids::JLP_POOL);
    if c.pool != pool || c.mint != ids::key(mint) {
        return Err(decode_err(
            field,
            format!(
                "custody {key}: pool {} mint {}, expected pool {pool} mint {mint}",
                c.pool, c.mint
            ),
        ));
    }
    Ok(c)
}

fn load_pool(set: &AccountSet) -> Result<JlpPool, ReadError> {
    let field = "max_request_execution_sec";
    let key = ids::key(ids::JLP_POOL);
    let data = perps_account(set, &key, &POOL_DISC, 8, field, false)?
        .ok_or_else(|| decode_err(field, format!("pool {key} is absent")))?;
    decode_pool(&data).map_err(|e| decode_err(field, format!("pool {key}: {e}")))
}

fn rates_field(
    key: &Pubkey,
    c: &Result<JupCustody, ReadError>,
    field: &str,
    now_s: i64,
) -> Field<CustodyRates> {
    match c {
        Ok(c) => match CustodyRates::from_custody(key, c, now_s) {
            Ok(r) => Field::ok(r),
            Err(e) => {
                let class = if rate_mechanism(c) == RateMechanism::Jump {
                    ErrorClass::Decode
                } else {
                    ErrorClass::NotApplicable
                };
                Field::err(ReadError::new(field, class, e))
            }
        },
        Err(e) => Field::err(e.clone()),
    }
}

#[allow(clippy::too_many_arguments)]
fn build_side(
    set: &AccountSet,
    wallet: &Pubkey,
    pda: &Pubkey,
    side: Side,
    sol: &Result<JupCustody, ReadError>,
    usdc: &Result<JupCustody, ReadError>,
    oracle: Option<f64>,
    now_s: i64,
) -> Field<PerpSide> {
    let field = side.as_str();
    let data = match perps_account(set, pda, &POSITION_DISC, POSITION_MIN_LEN, field, true) {
        Ok(Some(d)) => d,
        Ok(None) => return Field::Absent,
        Err(e) => return Field::err(e),
    };
    let p = match decode_position(&data) {
        Ok(p) => p,
        Err(e) => return Field::err(decode_err(field, format!("position {pda}: {e}"))),
    };
    let sol_custody = ids::key(ids::JUP_CUSTODY_SOL);
    let collateral_custody = ids::key(side.collateral_custody());
    let pool = ids::key(ids::JLP_POOL);
    if p.owner != *wallet
        || p.pool != pool
        || p.custody != sol_custody
        || p.collateral_custody != collateral_custody
        || Side::from_byte(p.side) != Some(side)
    {
        return Field::err(decode_err(
            field,
            format!(
                "position {pda}: owner {} pool {} custody {} collateral {} side {}, expected \
                 owner {wallet} pool {pool} custody {sol_custody} collateral {collateral_custody} side {}",
                p.owner,
                p.pool,
                p.custody,
                p.collateral_custody,
                p.side,
                side.byte()
            ),
        ));
    }
    if p.size_usd == 0 {
        return Field::Absent;
    }

    let collateral = match side {
        Side::Long => sol,
        Side::Short => usdc,
    };
    let collateral = match collateral {
        Ok(c) => c,
        Err(e) => {
            return Field::err(ReadError::new(
                field,
                e.class,
                format!("collateral custody {collateral_custody}: {}", e.message),
            ))
        }
    };
    let carry = match CustodyRates::from_custody(&collateral_custody, collateral, now_s) {
        Ok(r) => r.borrow_apr_pct * 100.0,
        Err(e) => {
            let class = if rate_mechanism(collateral) == RateMechanism::Jump {
                ErrorClass::Decode
            } else {
                ErrorClass::NotApplicable
            };
            return Field::err(ReadError::new(field, class, e));
        }
    };
    let accrued = match accrued_borrow_fee_raw(&p, collateral) {
        Ok(a) => a as f64 / USD_PRECISION,
        Err(e) => return Field::err(decode_err(field, format!("position {pda}: {e}"))),
    };

    let notional_usd = p.size_usd as f64 / USD_PRECISION;
    let entry_price_usd = p.price as f64 / USD_PRECISION;
    let Some(base_sol) = side_base_sol(notional_usd, entry_price_usd, oracle) else {
        return Field::err(decode_err(
            field,
            format!("position {pda}: open with no entry price and no oracle price"),
        ));
    };
    let unrealized_pnl_usd = oracle.filter(|_| entry_price_usd > 0.0).map(|o| {
        let moved = match side {
            Side::Long => o - entry_price_usd,
            Side::Short => entry_price_usd - o,
        };
        notional_usd * moved / entry_price_usd
    });
    // Liquidation needs the market (SOL) custody too; the bot degrades only
    // this field when it fails.
    let liquidation_price_usd = sol
        .as_ref()
        .ok()
        .and_then(|m| liquidation_price_usd(&p, m, collateral).ok().flatten());
    let liq_distance_ratio = match (liquidation_price_usd, oracle) {
        (Some(l), Some(o)) => Some(match side {
            Side::Long => (o - l) / o,
            Side::Short => (l - o) / o,
        }),
        _ => None,
    };
    Field::ok(PerpSide {
        side,
        position_pda: pda.to_string(),
        notional_usd,
        collateral_usd: p.collateral_usd as f64 / USD_PRECISION,
        entry_price_usd,
        base_sol,
        unrealized_pnl_usd,
        accrued_borrow_fee_usd: accrued,
        liquidation_price_usd,
        liq_distance_ratio,
        carry_cost_bps: carry,
        open_time: p.open_time,
        update_time: p.update_time,
        realised_pnl_usd: p.realised_pnl_usd as f64 / USD_PRECISION,
    })
}

/// Σ collateral / Σ notional over open sides; `None` when flat or any side
/// failed.
pub fn collateral_ratio(long: &Field<PerpSide>, short: &Field<PerpSide>) -> Option<f64> {
    if long.is_error() || short.is_error() {
        return None;
    }
    let sides = [long.value(), short.value()];
    let notional: f64 = sides.iter().flatten().map(|s| s.notional_usd).sum();
    let collateral: f64 = sides.iter().flatten().map(|s| s.collateral_usd).sum();
    (notional > 0.0).then(|| collateral / notional)
}

/// `jup_perps/1` for `wallet` from one `AccountSet` holding [`perps_keys`].
/// `long_pda` / `short_pda` are the wallet's SOL position PDAs (derived by
/// the caller); `oracle_usd` prices PnL and liquidation distance.
pub fn build_perps(
    set: &AccountSet,
    wallet: &Pubkey,
    long_pda: &Pubkey,
    short_pda: &Pubkey,
    oracle_usd: Option<f64>,
    now_s: i64,
) -> PerpsState {
    let oracle = oracle_usd.filter(|p| p.is_finite() && *p > 0.0);
    let sol_key = ids::key(ids::JUP_CUSTODY_SOL);
    let usdc_key = ids::key(ids::JUP_CUSTODY_USDC);
    let sol_raw = load_custody(set, &sol_key, ids::WSOL, "sol");
    let usdc_raw = load_custody(set, &usdc_key, ids::USDC, "usdc");
    let long = build_side(
        set,
        wallet,
        long_pda,
        Side::Long,
        &sol_raw,
        &usdc_raw,
        oracle,
        now_s,
    );
    let short = build_side(
        set,
        wallet,
        short_pda,
        Side::Short,
        &sol_raw,
        &usdc_raw,
        oracle,
        now_s,
    );
    let max_request_execution_sec = match load_pool(set) {
        Ok(p) => Field::ok(p.max_request_execution_sec),
        Err(e) => Field::err(e),
    };
    let keys = perps_keys(long_pda, short_pda);
    let slot = keys
        .iter()
        .filter_map(|k| set.get(k))
        .map(|r| r.slot)
        .min()
        .unwrap_or(set.slot_min);
    PerpsState {
        wallet: wallet.to_string(),
        slot,
        pool: ids::JLP_POOL.to_string(),
        oracle_usd: oracle,
        both_sides_open: long.value().is_some() && short.value().is_some(),
        collateral_ratio: collateral_ratio(&long, &short),
        long,
        short,
        sol: rates_field(&sol_key, &sol_raw, "sol", now_s),
        usdc: rates_field(&usdc_key, &usdc_raw, "usdc", now_s),
        max_request_execution_sec,
        watch: keys.iter().map(Pubkey::to_string).collect(),
    }
}

/// Status of a keeper `PositionRequest` (PDA from the caller). Absent ⇒
/// `exists = false`; `max_request_execution_sec` comes from the JLP pool.
pub fn request_status(
    set: &AccountSet,
    request: &Pubkey,
    now_s: i64,
    max_request_execution_sec: Option<i64>,
) -> Field<RequestStatus> {
    let field = "pending_request";
    let data = match perps_account(
        set,
        request,
        &POSITION_REQUEST_DISC,
        POSITION_REQUEST_MIN_LEN,
        field,
        true,
    ) {
        Ok(Some(d)) => d,
        Ok(None) => {
            return Field::ok(RequestStatus {
                position_request: request.to_string(),
                exists: false,
                executed: false,
                age_secs: None,
                expired: false,
            })
        }
        Err(e) => return Field::err(e),
    };
    let r = match decode_position_request(&data) {
        Ok(r) => r,
        Err(e) => return Field::err(decode_err(field, format!("request {request}: {e}"))),
    };
    let age = now_s.saturating_sub(r.open_time).max(0);
    Field::ok(RequestStatus {
        position_request: request.to_string(),
        exists: true,
        executed: r.executed,
        age_secs: Some(age),
        expired: !r.executed && max_request_execution_sec.is_some_and(|m| age > m),
    })
}
