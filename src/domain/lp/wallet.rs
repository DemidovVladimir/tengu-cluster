//! Wallet typed outputs + SPL decoders — pure, no IO. Parsers take RPC JSON
//! or an `AccountSet`; the `solana_wallet` / `solana_tx` glue fetches.
//!
//! | Item | Serves |
//! |---|---|
//! | [`decode_mint`], [`decode_token_account`] | SPL Mint (82 B) / Token account (165 B) under Tokenkeg or Token-2022 (extended accounts: account-type byte @165) |
//! | [`parse_token_accounts_by_owner`] → [`TokenAccountRow`] | `getTokenAccountsByOwner` jsonParsed |
//! | [`wallet_keys`], [`lamports_from_set`], [`build_wallet_balances`] | one `AccountSet` (`lp_snapshot` composes these over one GMA) |
//! | [`build_wallet_inventory`] → [`WalletInventory`] | `solana_wallet/1:<wallet>`, TTL [`WALLET_TTL_MS`] |
//! | [`parse_tx_status`] → [`TxStatus`] | `solana_tx/1:<signature>`, TTL [`TxStatus::ttl_ms`] (1 day once finalized) |
//!
//! Failed reads never become 0: an ATA that does not exist is a legitimate
//! `Ok(0)`; an ATA that was not read, is undecodable or belongs to another
//! mint / owner is `Field::Error` (class `Decode` / `Fatal`).
//!
//! Features (`WalletInventory`): `sol`, `lamports`, `token_accounts`,
//! `token_2022_accounts`, `wsol`, `usdc`, `sol_total` (= `sol` + `wsol`),
//! `balances`, `balance_errors`. Features (`TxStatus`): `found`,
//! `confirmation`, `final`, `succeeded`, `tx_slot`, `confirmations`,
//! `fee_lamports`, `compute_units`, `block_time`, `err_kind`, `err_ix`,
//! `err_custom`. Missing values are omitted, never 0.

// Consumed by tools/solana/wallet.rs and lp_snapshot (stage 3 glue).
#![allow(dead_code)]

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::observation::{
    set_bool, set_int, set_num, set_str, ErrorClass, Features, Field, ObsStatus, Observed,
    ReadError, MAX_FEATURE_STR,
};
use crate::domain::solana::{ids, AccountSet, Pubkey, Signature};

/// `solana_wallet` TTL.
pub const WALLET_TTL_MS: u64 = 5_000;
/// `solana_tx` TTL while not finalized.
pub const TX_TTL_MS: u64 = 2_000;
/// `solana_tx` TTL once finalized (the answer can no longer change).
pub const TX_FINAL_TTL_MS: u64 = 86_400_000;

const LAMPORTS_PER_SOL: u64 = 1_000_000_000;

// ---------------------------------------------------------------------------
// SPL layouts (spl-token `state.rs`; Token-2022 base layout is identical)
// ---------------------------------------------------------------------------

/// `Mint::LEN`.
pub const MINT_LEN: usize = 82;
/// `Account::LEN`.
pub const TOKEN_ACCOUNT_LEN: usize = 165;
/// `Multisig::LEN` — never a mint or token account.
const MULTISIG_LEN: usize = 355;
/// Token-2022 extended accounts: `AccountType` byte right after the base
/// token-account length (mints are zero-padded up to it).
const ACCOUNT_TYPE_OFFSET: usize = 165;
const ACCOUNT_TYPE_MINT: u8 = 1;
const ACCOUNT_TYPE_ACCOUNT: u8 = 2;

// Mint offsets.
const MINT_AUTHORITY: usize = 0; // COption<Pubkey>: u32 tag + 32
const MINT_SUPPLY: usize = 36; // u64
const MINT_DECIMALS: usize = 44; // u8
const MINT_IS_INITIALIZED: usize = 45; // bool
const MINT_FREEZE_AUTHORITY: usize = 46; // COption<Pubkey>

// Token account offsets.
const ACC_MINT: usize = 0;
const ACC_OWNER: usize = 32;
const ACC_AMOUNT: usize = 64; // u64
const ACC_DELEGATE: usize = 72; // COption<Pubkey>
const ACC_STATE: usize = 108; // u8
const ACC_IS_NATIVE: usize = 109; // COption<u64>: u32 tag + 8
const ACC_DELEGATED_AMOUNT: usize = 121; // u64
const ACC_CLOSE_AUTHORITY: usize = 129; // COption<Pubkey>

/// Tokenkeg or Token-2022.
pub fn is_token_program(program: &Pubkey) -> bool {
    *program == ids::key(ids::TOKEN) || *program == ids::key(ids::TOKEN_2022)
}

/// Decimals of well-known mints (protocol constants): wSOL 9, USDC 6.
pub fn known_decimals(mint: &Pubkey) -> Option<u8> {
    if *mint == ids::key(ids::WSOL) {
        Some(9)
    } else if *mint == ids::key(ids::USDC) {
        Some(6)
    } else {
        None
    }
}

/// Short label of a well-known mint for headlines (`wSOL`, `USDC`); other
/// mints are never abbreviated — they stay in `data` in full.
fn mint_label(mint: &Pubkey) -> Option<&'static str> {
    if *mint == ids::key(ids::WSOL) {
        Some("wSOL")
    } else if *mint == ids::key(ids::USDC) {
        Some("USDC")
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SplKind {
    Mint,
    Account,
    Multisig,
    Unknown,
}

fn spl_kind(program: &Pubkey, data: &[u8]) -> SplKind {
    match data.len() {
        MINT_LEN => SplKind::Mint,
        TOKEN_ACCOUNT_LEN => SplKind::Account,
        MULTISIG_LEN => SplKind::Multisig,
        n if n > ACCOUNT_TYPE_OFFSET && *program == ids::key(ids::TOKEN_2022) => {
            match data[ACCOUNT_TYPE_OFFSET] {
                ACCOUNT_TYPE_MINT => SplKind::Mint,
                ACCOUNT_TYPE_ACCOUNT => SplKind::Account,
                _ => SplKind::Unknown,
            }
        }
        _ => SplKind::Unknown,
    }
}

fn u32_le(data: &[u8], off: usize) -> Result<u32, String> {
    data.get(off..off + 4)
        .map(|b| u32::from_le_bytes(b.try_into().expect("4 bytes")))
        .ok_or_else(|| format!("u32 @{off} out of range"))
}

fn u64_le(data: &[u8], off: usize) -> Result<u64, String> {
    data.get(off..off + 8)
        .map(|b| u64::from_le_bytes(b.try_into().expect("8 bytes")))
        .ok_or_else(|| format!("u64 @{off} out of range"))
}

fn pubkey_le(data: &[u8], off: usize) -> Result<Pubkey, String> {
    Pubkey::read(data, off).ok_or_else(|| format!("pubkey @{off} out of range"))
}

fn coption_pubkey(data: &[u8], off: usize) -> Result<Option<Pubkey>, String> {
    match u32_le(data, off)? {
        0 => Ok(None),
        1 => pubkey_le(data, off + 4).map(Some),
        t => Err(format!("invalid COption tag {t} @{off}")),
    }
}

fn coption_u64(data: &[u8], off: usize) -> Result<Option<u64>, String> {
    match u32_le(data, off)? {
        0 => Ok(None),
        1 => u64_le(data, off + 4).map(Some),
        t => Err(format!("invalid COption tag {t} @{off}")),
    }
}

/// Decoded SPL Mint (base fields; Token-2022 extensions are not decoded).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SplMint {
    pub mint_authority: Option<Pubkey>,
    pub supply: u64,
    pub decimals: u8,
    pub freeze_authority: Option<Pubkey>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenAccountState {
    Uninitialized,
    Initialized,
    Frozen,
}

/// Decoded SPL Token account (base fields).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SplTokenAccount {
    pub mint: Pubkey,
    pub owner: Pubkey,
    pub amount: u64,
    pub delegate: Option<Pubkey>,
    pub state: TokenAccountState,
    /// `Some(rent_exempt_reserve)` for a native (wSOL) account.
    pub is_native: Option<u64>,
    pub delegated_amount: u64,
    pub close_authority: Option<Pubkey>,
}

fn decode_err(field: &str, msg: String) -> ReadError {
    ReadError::new(field, ErrorClass::Decode, msg)
}

/// SPL Mint from account `data` owned by `program`. Checks owner (Tokenkeg /
/// Token-2022), min length 82, account kind (82 B, or a Token-2022 extended
/// account with type byte 1) and `is_initialized`.
pub fn decode_mint(program: &Pubkey, data: &[u8]) -> Result<SplMint, ReadError> {
    let fail = |m: String| decode_err("mint", m);
    if !is_token_program(program) {
        return Err(fail(format!("owner {program} is not an SPL token program")));
    }
    if data.len() < MINT_LEN {
        return Err(fail(format!("{} bytes < {MINT_LEN}", data.len())));
    }
    if spl_kind(program, data) != SplKind::Mint {
        return Err(fail(format!(
            "{} bytes under {program} is not a mint",
            data.len()
        )));
    }
    match data[MINT_IS_INITIALIZED] {
        1 => {}
        0 => return Err(fail("mint is not initialized".into())),
        b => return Err(fail(format!("invalid is_initialized byte {b}"))),
    }
    Ok(SplMint {
        mint_authority: coption_pubkey(data, MINT_AUTHORITY).map_err(fail)?,
        supply: u64_le(data, MINT_SUPPLY).map_err(fail)?,
        decimals: data[MINT_DECIMALS],
        freeze_authority: coption_pubkey(data, MINT_FREEZE_AUTHORITY).map_err(fail)?,
    })
}

/// SPL Token account from account `data` owned by `program`. Checks owner,
/// min length 165, account kind (165 B, or a Token-2022 extended account
/// with type byte 2) and state (uninitialized is an error).
pub fn decode_token_account(program: &Pubkey, data: &[u8]) -> Result<SplTokenAccount, ReadError> {
    let fail = |m: String| decode_err("token_account", m);
    if !is_token_program(program) {
        return Err(fail(format!("owner {program} is not an SPL token program")));
    }
    if data.len() < TOKEN_ACCOUNT_LEN {
        return Err(fail(format!("{} bytes < {TOKEN_ACCOUNT_LEN}", data.len())));
    }
    if spl_kind(program, data) != SplKind::Account {
        return Err(fail(format!(
            "{} bytes under {program} is not a token account",
            data.len()
        )));
    }
    let state = match data[ACC_STATE] {
        1 => TokenAccountState::Initialized,
        2 => TokenAccountState::Frozen,
        0 => return Err(fail("token account is not initialized".into())),
        b => return Err(fail(format!("invalid state byte {b}"))),
    };
    Ok(SplTokenAccount {
        mint: pubkey_le(data, ACC_MINT).map_err(fail)?,
        owner: pubkey_le(data, ACC_OWNER).map_err(fail)?,
        amount: u64_le(data, ACC_AMOUNT).map_err(fail)?,
        delegate: coption_pubkey(data, ACC_DELEGATE).map_err(fail)?,
        state,
        is_native: coption_u64(data, ACC_IS_NATIVE).map_err(fail)?,
        delegated_amount: u64_le(data, ACC_DELEGATED_AMOUNT).map_err(fail)?,
        close_authority: coption_pubkey(data, ACC_CLOSE_AUTHORITY).map_err(fail)?,
    })
}

// ---------------------------------------------------------------------------
// Amounts
// ---------------------------------------------------------------------------

/// A token balance. `raw` is the exact base-unit amount (decimal string: a
/// u64 does not survive a JSON f64); `ui` = raw / 10^decimals for display
/// and features — decisions that need exactness use `raw`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenAmount {
    pub mint: Pubkey,
    pub raw: String,
    pub decimals: u8,
    pub ui: f64,
}

impl TokenAmount {
    pub fn new(mint: Pubkey, raw: u64, decimals: u8) -> Self {
        Self {
            mint,
            raw: raw.to_string(),
            decimals,
            ui: ui_amount(raw, decimals),
        }
    }
    pub fn raw_u64(&self) -> Option<u64> {
        self.raw.parse().ok()
    }
    /// Exact decimal rendering (`107.808931`, `0`), trailing zeros trimmed.
    pub fn ui_string(&self) -> String {
        match self.raw_u64() {
            Some(r) => fmt_units(r, self.decimals),
            None => self.raw.clone(),
        }
    }
}

/// `raw / 10^decimals` — the same arithmetic as the RPC's `uiAmount`.
pub fn ui_amount(raw: u64, decimals: u8) -> f64 {
    let scale = if decimals <= 19 {
        10u64.pow(decimals as u32) as f64
    } else {
        10f64.powi(decimals as i32)
    };
    raw as f64 / scale
}

/// Exact decimal string of `raw` base units (no float), trailing zeros
/// trimmed.
pub fn fmt_units(raw: u64, decimals: u8) -> String {
    let digits = raw.to_string();
    let d = decimals as usize;
    if d == 0 {
        return digits;
    }
    let padded = if digits.len() <= d {
        format!("{}{digits}", "0".repeat(d + 1 - digits.len()))
    } else {
        digits
    };
    let (int, frac) = padded.split_at(padded.len() - d);
    let frac = frac.trim_end_matches('0');
    if frac.is_empty() {
        int.to_string()
    } else {
        format!("{int}.{frac}")
    }
}

// ---------------------------------------------------------------------------
// JSON-RPC helpers
// ---------------------------------------------------------------------------

/// Unwrap a JSON-RPC envelope: `{"result": X}` ⇒ X; an `{"error": ..}`
/// response ⇒ Err (message only — never a URL); anything else is taken as
/// the payload itself.
fn rpc_result(v: &Value) -> Result<&Value, String> {
    if let Some(e) = v.get("error") {
        let code = e.get("code").and_then(Value::as_i64).unwrap_or(0);
        let msg = e.get("message").and_then(Value::as_str).unwrap_or("");
        return Err(format!("rpc error {code}: {msg}"));
    }
    Ok(v.get("result").unwrap_or(v))
}

/// `{"context": {"slot": N}, "value": X}` ⇒ `(Some(N), X)`; else `(None, v)`.
fn context_value(v: &Value) -> (Option<u64>, &Value) {
    match (v.as_object(), v.get("value")) {
        (Some(_), Some(inner)) => (v.pointer("/context/slot").and_then(Value::as_u64), inner),
        _ => (None, v),
    }
}

fn pubkey_at(v: &Value, ptr: &str) -> Result<Pubkey, String> {
    let s = v
        .pointer(ptr)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing {ptr}"))?;
    s.parse().map_err(|e| format!("{ptr}: {e}"))
}

// ---------------------------------------------------------------------------
// Token accounts (getTokenAccountsByOwner, jsonParsed)
// ---------------------------------------------------------------------------

/// One SPL token account of a wallet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenAccountRow {
    pub address: Pubkey,
    /// Owning token program (Tokenkeg or Token-2022).
    pub program: Pubkey,
    pub mint: Pubkey,
    /// Token-account authority (the wallet).
    pub owner: Pubkey,
    pub amount: TokenAmount,
    pub state: TokenAccountState,
    pub is_native: bool,
    pub lamports: u64,
}

fn parse_row(item: &Value, program: &Pubkey) -> Result<TokenAccountRow, String> {
    let address = pubkey_at(item, "/pubkey")?;
    let ctx = |m: String| format!("token account {address}: {m}");
    let owner_program = pubkey_at(item, "/account/owner").map_err(ctx)?;
    if owner_program != *program {
        return Err(ctx(format!("owned by {owner_program}, expected {program}")));
    }
    let lamports = item
        .pointer("/account/lamports")
        .and_then(Value::as_u64)
        .ok_or_else(|| ctx("missing lamports".into()))?;
    let parsed = item
        .pointer("/account/data/parsed")
        .ok_or_else(|| ctx("data is not jsonParsed".into()))?;
    let kind = parsed.get("type").and_then(Value::as_str).unwrap_or("");
    if kind != "account" {
        return Err(ctx(format!("parsed type {kind:?}, expected \"account\"")));
    }
    let mint = pubkey_at(parsed, "/info/mint").map_err(ctx)?;
    let owner = pubkey_at(parsed, "/info/owner").map_err(ctx)?;
    let state = match parsed.pointer("/info/state").and_then(Value::as_str) {
        Some("initialized") => TokenAccountState::Initialized,
        Some("frozen") => TokenAccountState::Frozen,
        Some("uninitialized") => TokenAccountState::Uninitialized,
        other => return Err(ctx(format!("state {other:?}"))),
    };
    let is_native = parsed
        .pointer("/info/isNative")
        .and_then(Value::as_bool)
        .ok_or_else(|| ctx("missing isNative".into()))?;
    let raw: u64 = parsed
        .pointer("/info/tokenAmount/amount")
        .and_then(Value::as_str)
        .ok_or_else(|| ctx("missing tokenAmount.amount".into()))?
        .parse()
        .map_err(|e| ctx(format!("tokenAmount.amount: {e}")))?;
    let decimals = parsed
        .pointer("/info/tokenAmount/decimals")
        .and_then(Value::as_u64)
        .and_then(|d| u8::try_from(d).ok())
        .ok_or_else(|| ctx("missing tokenAmount.decimals".into()))?;
    Ok(TokenAccountRow {
        address,
        program: *program,
        mint,
        owner,
        amount: TokenAmount::new(mint, raw, decimals),
        state,
        is_native,
        lamports,
    })
}

/// Rows of a `getTokenAccountsByOwner(owner, {programId: program},
/// {encoding: "jsonParsed"})` answer. Accepts the full JSON-RPC response,
/// its `result` (`{context, value}`) or the bare `value` array. Strict: any
/// undecodable row fails the whole list (a partial list would under-report
/// balances). Rows are sorted by (mint, address).
pub fn parse_token_accounts_by_owner(
    v: &Value,
    program: &Pubkey,
) -> Result<Vec<TokenAccountRow>, ReadError> {
    let field = "token_accounts";
    let payload = rpc_result(v).map_err(|m| decode_err(field, m))?;
    let (_, value) = context_value(payload);
    let items = value.as_array().ok_or_else(|| {
        decode_err(
            field,
            "getTokenAccountsByOwner value is not an array".into(),
        )
    })?;
    let mut rows = items
        .iter()
        .map(|item| parse_row(item, program))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|m| decode_err(field, m))?;
    rows.sort_by(|a, b| (a.mint, a.address).cmp(&(b.mint, b.address)));
    Ok(rows)
}

// ---------------------------------------------------------------------------
// Balances over one AccountSet
// ---------------------------------------------------------------------------

/// Keys a wallet read needs in one `getMultipleAccounts`: the wallet (native
/// lamports), then each `(mint, ata, token_program)`'s ATA and mint
/// (decimals). ATAs are derived by the caller. Deduplicated, order kept.
pub fn wallet_keys(wallet: &Pubkey, ata_by_mint: &[(Pubkey, Pubkey, Pubkey)]) -> Vec<Pubkey> {
    let mut out = vec![*wallet];
    for (mint, ata, _) in ata_by_mint {
        for k in [*ata, *mint] {
            if !out.contains(&k) {
                out.push(k);
            }
        }
    }
    out
}

/// Native lamports of `wallet` from the set: absent account ⇒ `Ok(0)`
/// (unfunded — legitimate); not read ⇒ `Error`.
pub fn lamports_from_set(set: &AccountSet, wallet: &Pubkey) -> Field<u64> {
    match set.get(wallet) {
        None => Field::err(ReadError::new(
            "lamports",
            ErrorClass::Fatal,
            format!("wallet {wallet} not in the read set"),
        )),
        Some(read) => Field::ok(read.lamports().unwrap_or(0)),
    }
}

/// Decimals of `mint`: the mint account in the set (decoded, must be owned
/// by `program`) wins; else `decimals` (caller-known); else
/// [`known_decimals`]; else an error.
fn mint_decimals(
    set: &AccountSet,
    mint: &Pubkey,
    program: &Pubkey,
    decimals: &BTreeMap<Pubkey, u8>,
) -> Result<u8, ReadError> {
    match set.get(mint) {
        Some(read) => {
            let owner = read
                .owner()
                .ok_or_else(|| decode_err("mint", format!("mint {mint} does not exist")))?;
            if owner != program {
                return Err(decode_err(
                    "mint",
                    format!("mint {mint} is owned by {owner}, not {program}"),
                ));
            }
            let data = read
                .data()
                .ok_or_else(|| decode_err("mint", format!("mint {mint}: invalid base64")))?;
            decode_mint(program, &data)
                .map(|m| m.decimals)
                .map_err(|mut e| {
                    e.message = format!("mint {mint}: {}", e.message);
                    e
                })
        }
        None => decimals
            .get(mint)
            .copied()
            .or_else(|| known_decimals(mint))
            .ok_or_else(|| {
                ReadError::new(
                    "mint",
                    ErrorClass::Fatal,
                    format!("decimals of mint {mint} unknown: mint account not read"),
                )
            }),
    }
}

/// Balance of each requested mint from its ATA in `set`.
/// `ata_by_mint` = `(mint, ata, token_program)` (ATAs derived by the caller).
/// ATA absent ⇒ `Ok(0)`; ATA not read, wrong program, undecodable, or
/// holding another mint / owner ⇒ `Error` (field `balances.<mint>`).
pub fn build_wallet_balances(
    set: &AccountSet,
    wallet: &Pubkey,
    ata_by_mint: &[(Pubkey, Pubkey, Pubkey)],
    decimals: &BTreeMap<Pubkey, u8>,
) -> BTreeMap<Pubkey, Field<TokenAmount>> {
    let mut out = BTreeMap::new();
    for (mint, ata, program) in ata_by_mint {
        let field = format!("balances.{mint}");
        out.insert(
            *mint,
            balance_of(set, wallet, mint, ata, program, decimals).unwrap_or_else(|mut e| {
                e.field = field;
                Field::err(e)
            }),
        );
    }
    out
}

fn balance_of(
    set: &AccountSet,
    wallet: &Pubkey,
    mint: &Pubkey,
    ata: &Pubkey,
    program: &Pubkey,
    decimals: &BTreeMap<Pubkey, u8>,
) -> Result<Field<TokenAmount>, ReadError> {
    if !is_token_program(program) {
        return Err(ReadError::new(
            "balance",
            ErrorClass::Fatal,
            format!("{program} is not an SPL token program"),
        ));
    }
    let dec = mint_decimals(set, mint, program, decimals)?;
    let read = set.get(ata).ok_or_else(|| {
        ReadError::new(
            "balance",
            ErrorClass::Fatal,
            format!("ATA {ata} not in the read set"),
        )
    })?;
    let Some(owner) = read.owner() else {
        // The ATA does not exist: the wallet legitimately holds 0.
        return Ok(Field::ok(TokenAmount::new(*mint, 0, dec)));
    };
    if owner != program {
        return Err(decode_err(
            "balance",
            format!("ATA {ata} is owned by {owner}, not {program}"),
        ));
    }
    let data = read
        .data()
        .ok_or_else(|| decode_err("balance", format!("ATA {ata}: invalid base64")))?;
    let acc = decode_token_account(program, &data).map_err(|mut e| {
        e.message = format!("ATA {ata}: {}", e.message);
        e
    })?;
    if acc.mint != *mint || acc.owner != *wallet {
        return Err(decode_err(
            "balance",
            format!(
                "ATA {ata} holds mint {} for owner {}, expected mint {mint} for owner {wallet}",
                acc.mint, acc.owner
            ),
        ));
    }
    Ok(Field::ok(TokenAmount::new(*mint, acc.amount, dec)))
}

// ---------------------------------------------------------------------------
// WalletInventory — solana_wallet/1:<wallet>
// ---------------------------------------------------------------------------

/// `solana_wallet` output: native SOL, every SPL token account (Tokenkeg +
/// Token-2022) and one balance per requested mint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WalletInventory {
    pub wallet: Pubkey,
    /// Highest context slot among the reads.
    pub slot: u64,
    pub lamports: Field<u64>,
    /// Tokenkeg accounts (`getTokenAccountsByOwner`).
    pub token_accounts: Field<Vec<TokenAccountRow>>,
    /// Token-2022 accounts.
    pub token_2022_accounts: Field<Vec<TokenAccountRow>>,
    /// Requested mints (full base58 keys) ⇒ ATA balance.
    pub balances: BTreeMap<Pubkey, Field<TokenAmount>>,
}

/// Assemble a `WalletInventory`. Rows are validated: a row whose authority
/// is not `wallet` or whose program does not match its list turns that list
/// into an `Error` (never silently dropped).
pub fn build_wallet_inventory(
    wallet: Pubkey,
    slot: u64,
    lamports: Field<u64>,
    token_accounts: Field<Vec<TokenAccountRow>>,
    token_2022_accounts: Field<Vec<TokenAccountRow>>,
    balances: BTreeMap<Pubkey, Field<TokenAmount>>,
) -> WalletInventory {
    WalletInventory {
        wallet,
        slot,
        lamports,
        token_accounts: validate_rows(token_accounts, &wallet, ids::TOKEN, "token_accounts"),
        token_2022_accounts: validate_rows(
            token_2022_accounts,
            &wallet,
            ids::TOKEN_2022,
            "token_2022_accounts",
        ),
        balances,
    }
}

fn validate_rows(
    rows: Field<Vec<TokenAccountRow>>,
    wallet: &Pubkey,
    program: &str,
    field: &str,
) -> Field<Vec<TokenAccountRow>> {
    let program = ids::key(program);
    match rows {
        Field::Ok { value } => {
            if let Some(bad) = value
                .iter()
                .find(|r| r.owner != *wallet || r.program != program)
            {
                return Field::err(decode_err(
                    field,
                    format!(
                        "token account {} has owner {} / program {}, expected {wallet} / {program}",
                        bad.address, bad.owner, bad.program
                    ),
                ));
            }
            Field::ok(value)
        }
        Field::Error { mut error } => {
            error.field = field.to_string();
            Field::err(error)
        }
        Field::Absent => Field::Absent,
    }
}

impl WalletInventory {
    /// Native SOL (lamports / 1e9).
    pub fn sol(&self) -> Option<f64> {
        self.lamports
            .value()
            .map(|l| *l as f64 / LAMPORTS_PER_SOL as f64)
    }
    pub fn balance(&self, mint: &Pubkey) -> Option<&Field<TokenAmount>> {
        self.balances.get(mint)
    }
    fn ui_of(&self, mint: &str) -> Option<f64> {
        self.balance(&ids::key(mint))
            .and_then(Field::value)
            .map(|a| a.ui)
    }
    fn field_states(&self) -> Vec<bool> {
        // true = ok, false = error (Absent is neither).
        let mut v = Vec::new();
        let mut push = |ok: bool, err: bool| {
            if ok || err {
                v.push(ok);
            }
        };
        push(self.lamports.value().is_some(), self.lamports.is_error());
        push(
            self.token_accounts.value().is_some(),
            self.token_accounts.is_error(),
        );
        push(
            self.token_2022_accounts.value().is_some(),
            self.token_2022_accounts.is_error(),
        );
        for b in self.balances.values() {
            push(b.value().is_some(), b.is_error());
        }
        v
    }
}

fn count_text<T>(f: &Field<Vec<T>>) -> String {
    match f {
        Field::Ok { value } => value.len().to_string(),
        Field::Absent => "absent".into(),
        Field::Error { .. } => "error".into(),
    }
}

impl Observed for WalletInventory {
    const SCHEMA: &'static str = "solana_wallet/1";

    fn subject(&self) -> String {
        self.wallet.to_string()
    }

    /// `wallet <wallet> sol=<exact> token_accounts=<n>+<n2022> wSOL=<x> USDC=<y> +<k> mints`
    fn headline(&self) -> String {
        let sol = match &self.lamports {
            Field::Ok { value } => fmt_units(*value, 9),
            Field::Absent => "absent".into(),
            Field::Error { .. } => "error".into(),
        };
        let mut h = format!(
            "wallet {} sol={sol} token_accounts={}+{}",
            self.wallet,
            count_text(&self.token_accounts),
            count_text(&self.token_2022_accounts)
        );
        let mut unlabeled = 0usize;
        for (mint, b) in &self.balances {
            let Some(label) = mint_label(mint) else {
                unlabeled += 1;
                continue;
            };
            let v = match b {
                Field::Ok { value } => value.ui_string(),
                Field::Absent => "absent".into(),
                Field::Error { .. } => "error".into(),
            };
            h.push_str(&format!(" {label}={v}"));
        }
        if unlabeled > 0 {
            h.push_str(&format!(" +{unlabeled} mints"));
        }
        h
    }

    fn features(&self) -> Features {
        let mut f = Features::new();
        set_num(&mut f, "sol", self.sol());
        set_int(&mut f, "lamports", self.lamports.value().map(|l| *l as i64));
        set_int(
            &mut f,
            "token_accounts",
            self.token_accounts.value().map(|r| r.len() as i64),
        );
        set_int(
            &mut f,
            "token_2022_accounts",
            self.token_2022_accounts.value().map(|r| r.len() as i64),
        );
        let wsol = self.ui_of(ids::WSOL);
        set_num(&mut f, "wsol", wsol);
        set_num(&mut f, "usdc", self.ui_of(ids::USDC));
        set_num(
            &mut f,
            "sol_total",
            self.sol().zip(wsol).map(|(s, w)| s + w),
        );
        set_int(&mut f, "balances", Some(self.balances.len() as i64));
        set_int(
            &mut f,
            "balance_errors",
            Some(self.balances.values().filter(|b| b.is_error()).count() as i64),
        );
        f
    }

    fn slot(&self) -> Option<u64> {
        Some(self.slot)
    }

    /// `Error` when no field is usable, `Partial` when any field failed.
    fn status(&self) -> ObsStatus {
        let states = self.field_states();
        if !states.is_empty() && states.iter().all(|ok| !ok) {
            ObsStatus::Error
        } else if states.iter().any(|ok| !ok) {
            ObsStatus::Partial
        } else {
            ObsStatus::Ok
        }
    }

    fn errors(&self) -> Vec<ReadError> {
        let mut out = Vec::new();
        out.extend(self.lamports.error().cloned());
        out.extend(self.token_accounts.error().cloned());
        out.extend(self.token_2022_accounts.error().cloned());
        out.extend(self.balances.values().filter_map(|b| b.error().cloned()));
        out
    }
}

// ---------------------------------------------------------------------------
// TxStatus — solana_tx/1:<signature>
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confirmation {
    Processed,
    Confirmed,
    Finalized,
}

impl Confirmation {
    pub fn as_str(self) -> &'static str {
        match self {
            Confirmation::Processed => "processed",
            Confirmation::Confirmed => "confirmed",
            Confirmation::Finalized => "finalized",
        }
    }
}

/// `solana_tx` output. `found = false` with no `error` is a legitimate
/// "not landed / not in the searched history" (status `Absent`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TxStatus {
    pub signature: Signature,
    /// Context slot of the `getSignatureStatuses` read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_slot: Option<u64>,
    pub found: bool,
    /// Slot the transaction landed in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmation: Option<Confirmation>,
    /// Blocks since confirmation; `None` once rooted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmations: Option<u64>,
    /// `TransactionError` JSON; `None` = succeeded (when found).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub err: Option<Value>,
    /// `meta.fee` (Absent until `getTransaction` returns the tx).
    pub fee_lamports: Field<u64>,
    /// `meta.computeUnitsConsumed` — never parsed from log lines.
    pub compute_units: Field<u64>,
    /// `blockTime` (unix s).
    pub block_time: Field<i64>,
    /// The status read itself failed or was undecodable (status `Error`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ReadError>,
}

impl TxStatus {
    /// The status read failed (RPC error): observation status `Error`.
    pub fn failed(signature: Signature, context_slot: Option<u64>, mut error: ReadError) -> Self {
        error.field = "status".into();
        Self {
            signature,
            context_slot,
            found: false,
            slot: None,
            confirmation: None,
            confirmations: None,
            err: None,
            fee_lamports: Field::Absent,
            compute_units: Field::Absent,
            block_time: Field::Absent,
            error: Some(error),
        }
    }

    /// Finalized ⇒ the answer can no longer change (TTL [`TX_FINAL_TTL_MS`]).
    pub fn is_final(&self) -> bool {
        self.error.is_none() && self.found && self.confirmation == Some(Confirmation::Finalized)
    }

    /// Cache TTL: 1 day once finalized, else [`TX_TTL_MS`].
    pub fn ttl_ms(&self) -> u64 {
        if self.is_final() {
            TX_FINAL_TTL_MS
        } else {
            TX_TTL_MS
        }
    }

    /// `Some(true)` landed without error, `Some(false)` landed with `err`,
    /// `None` when not found / unknown.
    pub fn succeeded(&self) -> Option<bool> {
        (self.found && self.error.is_none()).then_some(self.err.is_none())
    }

    fn err_kind(&self) -> Option<String> {
        match self.err.as_ref()? {
            Value::String(s) => Some(s.clone()),
            Value::Object(o) => o.keys().next().cloned(),
            _ => None,
        }
    }

    /// `InstructionError: [index, {"Custom": code}]` ⇒ (index, code).
    fn instruction_error(&self) -> (Option<i64>, Option<i64>) {
        let Some(ie) = self.err.as_ref().and_then(|e| e.get("InstructionError")) else {
            return (None, None);
        };
        (
            ie.get(0).and_then(Value::as_i64),
            ie.pointer("/1/Custom").and_then(Value::as_i64),
        )
    }
}

fn tx_fields_absent() -> (Field<u64>, Field<u64>, Field<i64>) {
    (Field::Absent, Field::Absent, Field::Absent)
}

fn tx_fields_error(e: ReadError) -> (Field<u64>, Field<u64>, Field<i64>) {
    let with = |name: &str| {
        let mut e = e.clone();
        e.field = name.to_string();
        e
    };
    (
        Field::err(with("fee_lamports")),
        Field::err(with("compute_units")),
        Field::err(with("block_time")),
    )
}

/// Parse `getSignatureStatuses([signature])` (full response, `result` or
/// the bare `value` array; entry 0 is used) and optionally
/// `getTransaction(signature, {encoding: json, maxSupportedTransactionVersion: 0})`.
///
/// `context_slot`: the status read's slot when the caller already split it
/// off; otherwise taken from `statuses.context.slot`.
/// `tx`: `None` = not fetched; `Some(Ok(v))` = the response (a `null`
/// result = not available at that commitment ⇒ fee / CU / block time
/// `Absent`); `Some(Err(e))` = the fetch failed ⇒ those fields `Error`.
pub fn parse_tx_status(
    signature: Signature,
    context_slot: Option<u64>,
    statuses: &Value,
    tx: Option<Result<&Value, ReadError>>,
) -> TxStatus {
    let payload = match rpc_result(statuses) {
        Ok(p) => p,
        Err(m) => return TxStatus::failed(signature, context_slot, decode_err("status", m)),
    };
    let (ctx_slot, value) = context_value(payload);
    let context_slot = context_slot.or(ctx_slot);
    let fail = |m: String| TxStatus::failed(signature, context_slot, decode_err("status", m));
    let Some(entry) = value.as_array().and_then(|a| a.first()) else {
        return fail("getSignatureStatuses value is not a non-empty array".into());
    };

    let mut st = TxStatus {
        signature,
        context_slot,
        found: false,
        slot: None,
        confirmation: None,
        confirmations: None,
        err: None,
        fee_lamports: Field::Absent,
        compute_units: Field::Absent,
        block_time: Field::Absent,
        error: None,
    };

    if !entry.is_null() {
        let Some(slot) = entry.get("slot").and_then(Value::as_u64) else {
            return fail("status entry without slot".into());
        };
        st.found = true;
        st.slot = Some(slot);
        st.confirmations = entry.get("confirmations").and_then(Value::as_u64);
        st.confirmation = match entry.get("confirmationStatus").and_then(Value::as_str) {
            Some("processed") => Some(Confirmation::Processed),
            Some("confirmed") => Some(Confirmation::Confirmed),
            Some("finalized") => Some(Confirmation::Finalized),
            Some(other) => return fail(format!("unknown confirmationStatus {other:?}")),
            // Older nodes: `confirmations: null` means rooted.
            None if entry.get("confirmations").is_some_and(Value::is_null) => {
                Some(Confirmation::Finalized)
            }
            None => None,
        };
        st.err = entry.get("err").filter(|e| !e.is_null()).cloned();
    }

    let (fee, cu, bt) = match tx {
        None => tx_fields_absent(),
        Some(Err(e)) => tx_fields_error(e),
        Some(Ok(v)) => match parse_tx_meta(&signature, v) {
            Ok(None) => tx_fields_absent(),
            Ok(Some(m)) => {
                if !st.found {
                    // Status cache miss (no history search) but the ledger has it.
                    st.found = true;
                    st.slot = Some(m.slot);
                    st.err = m.err.clone();
                }
                (m.fee, m.compute_units, m.block_time)
            }
            Err(m) => tx_fields_error(decode_err("tx", m)),
        },
    };
    st.fee_lamports = fee;
    st.compute_units = cu;
    st.block_time = bt;
    st
}

struct TxMeta {
    slot: u64,
    err: Option<Value>,
    fee: Field<u64>,
    compute_units: Field<u64>,
    block_time: Field<i64>,
}

/// `Ok(None)` for a null result (tx not available).
fn parse_tx_meta(signature: &Signature, v: &Value) -> Result<Option<TxMeta>, String> {
    let tx = rpc_result(v)?;
    if tx.is_null() {
        return Ok(None);
    }
    let sig0 = tx
        .pointer("/transaction/signatures/0")
        .and_then(Value::as_str)
        .ok_or("getTransaction: missing transaction.signatures[0]")?;
    if sig0 != signature.to_string() {
        return Err(format!(
            "getTransaction returned signature {sig0}, expected {signature}"
        ));
    }
    let slot = tx
        .get("slot")
        .and_then(Value::as_u64)
        .ok_or("getTransaction: missing slot")?;
    let block_time = match tx.get("blockTime") {
        Some(Value::Null) | None => Field::Absent,
        Some(b) => b
            .as_i64()
            .map(Field::ok)
            .ok_or("getTransaction: blockTime is not an integer")?,
    };
    let meta = tx.get("meta").filter(|m| !m.is_null());
    let (fee, compute_units, err) = match meta {
        None => (Field::Absent, Field::Absent, None),
        Some(m) => {
            let fee = m
                .get("fee")
                .and_then(Value::as_u64)
                .map(Field::ok)
                .ok_or("getTransaction: missing meta.fee")?;
            let cu = match m.get("computeUnitsConsumed") {
                Some(Value::Null) | None => Field::Absent,
                Some(c) => c
                    .as_u64()
                    .map(Field::ok)
                    .ok_or("getTransaction: meta.computeUnitsConsumed is not an integer")?,
            };
            (fee, cu, m.get("err").filter(|e| !e.is_null()).cloned())
        }
    };
    Ok(Some(TxMeta {
        slot,
        err,
        fee,
        compute_units,
        block_time,
    }))
}

impl Observed for TxStatus {
    const SCHEMA: &'static str = "solana_tx/1";

    fn subject(&self) -> String {
        self.signature.to_string()
    }

    /// `tx <signature> finalized ok slot=<n> fee=<lamports> cu=<units>`
    fn headline(&self) -> String {
        if self.error.is_some() {
            return format!("tx {} status unavailable", self.signature);
        }
        if !self.found {
            return format!("tx {} not found", self.signature);
        }
        let conf = self
            .confirmation
            .map(Confirmation::as_str)
            .unwrap_or("landed");
        let outcome = if self.err.is_none() { "ok" } else { "failed" };
        let mut h = format!("tx {} {conf} {outcome}", self.signature);
        if let Some(s) = self.slot {
            h.push_str(&format!(" slot={s}"));
        }
        if let Some(fee) = self.fee_lamports.value() {
            h.push_str(&format!(" fee={fee}"));
        }
        if let Some(cu) = self.compute_units.value() {
            h.push_str(&format!(" cu={cu}"));
        }
        h
    }

    fn features(&self) -> Features {
        let mut f = Features::new();
        if self.error.is_some() {
            return f;
        }
        set_bool(&mut f, "found", Some(self.found));
        set_str(
            &mut f,
            "confirmation",
            self.confirmation.map(Confirmation::as_str),
        );
        set_bool(&mut f, "final", Some(self.is_final()));
        set_bool(&mut f, "succeeded", self.succeeded());
        set_int(&mut f, "tx_slot", self.slot.map(|s| s as i64));
        set_int(
            &mut f,
            "confirmations",
            self.confirmations.map(|c| c as i64),
        );
        set_int(
            &mut f,
            "fee_lamports",
            self.fee_lamports.value().map(|v| *v as i64),
        );
        set_int(
            &mut f,
            "compute_units",
            self.compute_units.value().map(|v| *v as i64),
        );
        set_int(&mut f, "block_time", self.block_time.value().copied());
        let kind = self.err_kind();
        set_str(
            &mut f,
            "err_kind",
            kind.as_deref()
                .filter(|k| k.chars().count() <= MAX_FEATURE_STR),
        );
        let (ix, custom) = self.instruction_error();
        set_int(&mut f, "err_ix", ix);
        set_int(&mut f, "err_custom", custom);
        f
    }

    fn slot(&self) -> Option<u64> {
        self.context_slot.or(self.slot)
    }

    fn status(&self) -> ObsStatus {
        if self.error.is_some() {
            ObsStatus::Error
        } else if !self.found {
            ObsStatus::Absent
        } else if self.fee_lamports.is_error()
            || self.compute_units.is_error()
            || self.block_time.is_error()
        {
            ObsStatus::Partial
        } else {
            ObsStatus::Ok
        }
    }

    fn errors(&self) -> Vec<ReadError> {
        let mut out: Vec<ReadError> = self.error.iter().cloned().collect();
        out.extend(self.fee_lamports.error().cloned());
        out.extend(self.compute_units.error().cloned());
        out.extend(self.block_time.error().cloned());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::observation::{assert_features_ok, ObsSource, Observation, MAX_LINE1_CHARS};
    use crate::domain::solana::{AccountRead, AccountState};
    use serde_json::json;

    macro_rules! fixture {
        ($name:literal) => {
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/solana/wallet/",
                $name
            ))
        };
    }

    const WALLET: &str = "F3YvPiLdniRPGpeKrbeGWR2zg2wPpzVuvqBA5BBJBQ5S";
    const MINT_98: &str = "98sMhvDwXj1RQi5c5Mndm3vPe9cBqPrbLaufMXFNMh5g";
    const PYUSD: &str = "2b1kV6DkPAnxd5ixfnxCpjxmKwqjjaYmCZfHsFu24GXo";
    const ATA_USDC: &str = "D9ScKYy15cw1tpkkuwEnDKv62nCyuETwrvRSdP4usGg1";
    const ATA_98: &str = "DQsXEvePuhSGqJZzsKrg3QTSJcRWFcBvTL6F8THrCiVF";
    const ATA_WSOL: &str = "E4PCnfEconGJW6vf7GDycEnkFe1VWC7teiNWJzQv3NTA";
    const ATA_PYUSD: &str = "BUACmTyTknjx6zdRwazawArchbQehNUYkZjjweMvyNLb";
    const SIG_OK: &str =
        "L8TEY2sSvscX2R2EBChD1p1o3HApdBUHqVfTJKLxJ4zK4M6pebL3diuYKcKjPF5deW7GF6DVnMdPU2tBwgz1Mfi";
    const SIG_FAILED: &str =
        "24vJBCdpUCW5nAb9JQZL4E5ptwSCyQ7QYvrrTTYp13esMq5q26LdZ9mzC1qZX6MDF31hsAcr3HWLHa3kaa9P1yPE";
    const SIG_UNKNOWN: &str =
        "99eUso3aSbE9tqGSTXzo3TLfKb9RkMTURrHKQ1K7Zh3BbeqPevr5E1iCbpTjqHuTFLtfxTTD5ekfVuZFzQyEQf8";
    /// Slot of `gma_base64.json` (meta.json).
    const GMA_SLOT: u64 = 450_101_767;

    fn pk(s: &str) -> Pubkey {
        s.parse().unwrap()
    }
    fn sig(s: &str) -> Signature {
        s.parse().unwrap()
    }
    fn json_of(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }
    fn token() -> Pubkey {
        ids::key(ids::TOKEN)
    }
    fn token_2022() -> Pubkey {
        ids::key(ids::TOKEN_2022)
    }

    /// `gma_base64.json` as an `AccountSet`, keyed by the request order in
    /// `meta.json`.
    fn gma_set() -> AccountSet {
        let meta = json_of(fixture!("meta.json"));
        let keys = meta["files"]["gma_base64.json"]["keys"].as_array().unwrap();
        let v = json_of(fixture!("gma_base64.json"));
        let slot = v["result"]["context"]["slot"].as_u64().unwrap();
        assert_eq!(slot, GMA_SLOT);
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
                pubkey: pk(k.as_str().unwrap()),
                slot,
                state,
            });
        }
        set
    }

    /// The RPC's own jsonParsed view of key `i` (independent decoder).
    fn parsed_info(i: usize) -> Value {
        let v = json_of(fixture!("gma_json_parsed.json"));
        v["result"]["value"][i]["data"]["parsed"]["info"].clone()
    }

    fn data_of(set: &AccountSet, key: &str) -> (Pubkey, Vec<u8>) {
        let r = set.get(&pk(key)).unwrap();
        (*r.owner().unwrap(), r.data().unwrap())
    }

    fn opt_pk(v: &Value) -> Option<Pubkey> {
        v.as_str().map(pk)
    }

    fn requested() -> Vec<(Pubkey, Pubkey, Pubkey)> {
        vec![
            (ids::key(ids::WSOL), pk(ATA_WSOL), token()),
            (ids::key(ids::USDC), pk(ATA_USDC), token()),
        ]
    }

    fn assert_line1_fits(obs: &Observation, id: &str) {
        let text = obs.render_text(obs.observed_at_ms + 1_000);
        let line1 = text.lines().next().unwrap();
        assert!(line1.chars().count() <= MAX_LINE1_CHARS, "{line1}");
        assert!(line1.contains(id), "{line1}");
    }

    // ── SPL decoders vs the RPC's jsonParsed view ─────────────────────

    #[test]
    fn decode_mints_match_rpc_json_parsed() {
        let set = gma_set();
        // (key, index in gma, pinned supply, pinned decimals)
        for (key, i, supply, decimals) in [
            (ids::USDC, 1, 8_049_317_756_550_024u64, 6u8),
            (ids::WSOL, 2, 0, 9),
            (MINT_98, 3, 714_975_584_439_102, 9),
            (PYUSD, 4, 724_160_269_219_128, 6),
        ] {
            let (owner, data) = data_of(&set, key);
            let m = decode_mint(&owner, &data).unwrap_or_else(|e| panic!("{key}: {e:?}"));
            let info = parsed_info(i);
            assert_eq!(m.supply, supply, "{key}");
            assert_eq!(m.decimals, decimals, "{key}");
            assert_eq!(
                m.supply.to_string(),
                info["supply"].as_str().unwrap(),
                "{key}"
            );
            assert_eq!(
                m.decimals as u64,
                info["decimals"].as_u64().unwrap(),
                "{key}"
            );
            assert_eq!(m.mint_authority, opt_pk(&info["mintAuthority"]), "{key}");
            assert_eq!(
                m.freeze_authority,
                opt_pk(&info["freezeAuthority"]),
                "{key}"
            );
        }
        let (_, usdc) = data_of(&set, ids::USDC);
        let m = decode_mint(&token(), &usdc).unwrap();
        assert_eq!(
            m.mint_authority,
            Some(pk("BJE5MMbqXjVwjAF7oxwPYXnTXDyspzZyt4vwenNw5ruG"))
        );
        assert_eq!(
            m.freeze_authority,
            Some(pk("7dGbd2QZcCKcTndnHcTL8q7SMVXAkp688NTQYwrRCrar"))
        );
        let (owner, pyusd) = data_of(&set, PYUSD);
        assert_eq!(owner, token_2022());
        assert_eq!(pyusd.len(), 866, "Token-2022 mint with extensions");
    }

    #[test]
    fn decode_token_accounts_match_rpc_json_parsed() {
        let set = gma_set();
        for (key, i, mint, amount) in [
            (ATA_USDC, 5, ids::USDC, 107_808_931u64),
            (ATA_98, 6, MINT_98, 8_488_252),
        ] {
            let (owner, data) = data_of(&set, key);
            let a = decode_token_account(&owner, &data).unwrap();
            let info = parsed_info(i);
            assert_eq!(a.mint, pk(mint));
            assert_eq!(a.owner, pk(WALLET));
            assert_eq!(a.amount, amount);
            assert_eq!(
                a.amount.to_string(),
                info["tokenAmount"]["amount"].as_str().unwrap()
            );
            assert_eq!(info["state"], "initialized");
            assert_eq!(a.state, TokenAccountState::Initialized);
            assert_eq!(a.is_native, None);
            assert_eq!(a.delegate, None);
            assert_eq!(a.delegated_amount, 0);
            assert_eq!(a.close_authority, None);
        }
    }

    #[test]
    fn decoders_reject_wrong_kind_owner_length_and_tags() {
        let set = gma_set();
        let (_, usdc_mint) = data_of(&set, ids::USDC);
        let (_, ata) = data_of(&set, ATA_USDC);
        let (_, pyusd) = data_of(&set, PYUSD);

        let e = decode_mint(&token(), &ata).unwrap_err();
        assert_eq!(e.class, ErrorClass::Decode);
        assert!(e.message.contains("not a mint"), "{}", e.message);
        assert!(decode_token_account(&token(), &usdc_mint)
            .unwrap_err()
            .message
            .contains("< 165"));
        let e = decode_mint(&ids::key(ids::DLMM), &usdc_mint).unwrap_err();
        assert!(
            e.message.contains("not an SPL token program"),
            "{}",
            e.message
        );
        assert!(decode_mint(&token(), &usdc_mint[..81]).is_err(), "short");
        // A Token-2022 extended mint is not a token account, and Tokenkeg has
        // no extended layout at all.
        assert!(decode_token_account(&token_2022(), &pyusd)
            .unwrap_err()
            .message
            .contains("not a token account"));
        assert!(
            decode_mint(&token(), &pyusd).is_err(),
            "866 B under Tokenkeg"
        );
        // Invalid COption tag / uninitialized.
        let mut bad = usdc_mint.clone();
        bad[0] = 7;
        assert!(decode_mint(&token(), &bad)
            .unwrap_err()
            .message
            .contains("COption"));
        let mut bad = usdc_mint.clone();
        bad[MINT_IS_INITIALIZED] = 0;
        assert!(decode_mint(&token(), &bad).is_err());
        let mut bad = ata.clone();
        bad[ACC_STATE] = 0;
        assert!(decode_token_account(&token(), &bad)
            .unwrap_err()
            .message
            .contains("not initialized"));
        let mut frozen = ata.clone();
        frozen[ACC_STATE] = 2;
        assert_eq!(
            decode_token_account(&token(), &frozen).unwrap().state,
            TokenAccountState::Frozen
        );
    }

    #[test]
    fn amount_formatting_is_exact() {
        assert_eq!(fmt_units(107_808_931, 6), "107.808931");
        assert_eq!(fmt_units(8_488_252, 9), "0.008488252");
        assert_eq!(fmt_units(2_748_145_289, 9), "2.748145289");
        assert_eq!(fmt_units(1_000_000, 6), "1");
        assert_eq!(fmt_units(0, 6), "0");
        assert_eq!(fmt_units(5, 0), "5");
        assert_eq!(fmt_units(u64::MAX, 9), "18446744073.709551615");
        let a = TokenAmount::new(ids::key(ids::USDC), 107_808_931, 6);
        assert_eq!(a.ui, 107.808931);
        assert_eq!(a.raw_u64(), Some(107_808_931));
        assert_eq!(a.ui_string(), "107.808931");
    }

    // ── getTokenAccountsByOwner ───────────────────────────────────────

    #[test]
    fn parse_token_accounts_fixture() {
        let v = json_of(fixture!("token_accounts_tokenkeg.json"));
        let rows = parse_token_accounts_by_owner(&v, &token()).unwrap();
        assert_eq!(rows.len(), 2);
        let by_mint: BTreeMap<Pubkey, &TokenAccountRow> =
            rows.iter().map(|r| (r.mint, r)).collect();
        let usdc = by_mint[&ids::key(ids::USDC)];
        assert_eq!(usdc.address, pk(ATA_USDC));
        assert_eq!(usdc.owner, pk(WALLET));
        assert_eq!(usdc.program, token());
        assert_eq!(usdc.amount.raw, "107808931");
        assert_eq!(usdc.amount.decimals, 6);
        assert_eq!(usdc.amount.ui, 107.808931, "== RPC uiAmount");
        assert_eq!(usdc.lamports, 2_039_280);
        assert!(!usdc.is_native);
        assert_eq!(usdc.state, TokenAccountState::Initialized);
        let other = by_mint[&pk(MINT_98)];
        assert_eq!(other.address, pk(ATA_98));
        assert_eq!(other.amount.raw, "8488252");
        assert_eq!(other.amount.ui, 0.008488252, "== RPC uiAmount");
        assert!(rows.windows(2).all(|w| w[0].mint <= w[1].mint));

        // Same answer from `result` or the bare `value` array.
        assert_eq!(
            parse_token_accounts_by_owner(&v["result"], &token()).unwrap(),
            rows
        );
        assert_eq!(
            parse_token_accounts_by_owner(&v["result"]["value"], &token()).unwrap(),
            rows
        );

        let v22 = json_of(fixture!("token_accounts_token2022.json"));
        assert!(parse_token_accounts_by_owner(&v22, &token_2022())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn parse_token_accounts_is_strict() {
        let v = json_of(fixture!("token_accounts_tokenkeg.json"));
        // Wrong program for these rows.
        let e = parse_token_accounts_by_owner(&v, &token_2022()).unwrap_err();
        assert_eq!(e.class, ErrorClass::Decode);
        assert!(e.message.contains(ATA_98) || e.message.contains(ATA_USDC));
        // One undecodable row fails the list (never a silent under-report).
        let mut bad = v.clone();
        bad["result"]["value"][1]["account"]["data"]["parsed"]["info"]["tokenAmount"]["amount"] =
            json!(12);
        assert!(parse_token_accounts_by_owner(&bad, &token()).is_err());
        // JSON-RPC error envelope.
        let e = parse_token_accounts_by_owner(
            &json!({"jsonrpc": "2.0", "error": {"code": -32429, "message": "max usage reached"}, "id": 1}),
            &token(),
        )
        .unwrap_err();
        assert!(e.message.contains("-32429"), "{}", e.message);
        assert!(
            parse_token_accounts_by_owner(&json!({"result": {"value": {}}}), &token()).is_err()
        );
    }

    // ── balances over one AccountSet ──────────────────────────────────

    #[test]
    fn wallet_keys_dedup_in_order() {
        let keys = wallet_keys(&pk(WALLET), &requested());
        assert_eq!(
            keys,
            vec![
                pk(WALLET),
                pk(ATA_WSOL),
                ids::key(ids::WSOL),
                pk(ATA_USDC),
                ids::key(ids::USDC)
            ]
        );
        let mut dup = requested();
        dup.push(dup[1]);
        assert_eq!(wallet_keys(&pk(WALLET), &dup).len(), 5);
    }

    #[test]
    fn balances_from_gma_fixture() {
        let set = gma_set();
        let mut req = requested();
        req.push((pk(MINT_98), pk(ATA_98), token()));
        req.push((pk(PYUSD), pk(ATA_PYUSD), token_2022()));
        let b = build_wallet_balances(&set, &pk(WALLET), &req, &BTreeMap::new());
        assert_eq!(b.len(), 4);
        let usdc = b[&ids::key(ids::USDC)].value().unwrap();
        assert_eq!((usdc.raw.as_str(), usdc.decimals), ("107808931", 6));
        // wSOL ATA does not exist: a legitimate zero, decimals from the mint.
        let wsol = b[&ids::key(ids::WSOL)].value().unwrap();
        assert_eq!((wsol.raw.as_str(), wsol.decimals, wsol.ui), ("0", 9, 0.0));
        assert_eq!(b[&pk(MINT_98)].value().unwrap().raw, "8488252");
        // Token-2022 ATA absent: zero with the Token-2022 mint's decimals.
        let py = b[&pk(PYUSD)].value().unwrap();
        assert_eq!((py.raw.as_str(), py.decimals), ("0", 6));
        assert_eq!(
            lamports_from_set(&set, &pk(WALLET)),
            Field::ok(2_748_145_289)
        );
    }

    #[test]
    fn balances_error_instead_of_zero() {
        let set = gma_set();
        let w = pk(WALLET);
        let none = BTreeMap::new();
        let err_of = |req: &[(Pubkey, Pubkey, Pubkey)]| {
            let b = build_wallet_balances(&set, &w, req, &none);
            b.values().next().unwrap().error().cloned().unwrap()
        };
        // ATA of another mint.
        let e = err_of(&[(pk(MINT_98), pk(ATA_USDC), token())]);
        assert_eq!(e.class, ErrorClass::Decode);
        assert_eq!(e.field, format!("balances.{MINT_98}"));
        assert!(e.message.contains(ATA_USDC), "{}", e.message);
        // ATA not read at all.
        let unread = pk("BUACmTyTknjx6zdRwazawArchbQehNUYkZjjweMvyNLb");
        let mut partial = set.clone();
        partial.accounts.remove(&unread);
        let b = build_wallet_balances(&partial, &w, &[(pk(PYUSD), unread, token_2022())], &none);
        assert_eq!(b[&pk(PYUSD)].error().unwrap().class, ErrorClass::Fatal);
        // Mint owned by a different token program than supplied.
        let e = err_of(&[(pk(PYUSD), pk(ATA_PYUSD), token())]);
        assert!(e.message.contains("is owned by"), "{}", e.message);
        // Not a token program.
        let e = err_of(&[(ids::key(ids::USDC), pk(ATA_USDC), ids::key(ids::DLMM))]);
        assert!(e.message.contains("not an SPL token program"));
        // Unknown decimals (mint not read, not well-known) is an error, even
        // for an absent ATA.
        let mut no_mint = set.clone();
        no_mint.accounts.remove(&pk(MINT_98));
        let b = build_wallet_balances(&no_mint, &w, &[(pk(MINT_98), pk(ATA_98), token())], &none);
        assert!(b[&pk(MINT_98)].is_error());
        // ... unless the caller knows them.
        let known = BTreeMap::from([(pk(MINT_98), 9u8)]);
        let b = build_wallet_balances(&no_mint, &w, &[(pk(MINT_98), pk(ATA_98), token())], &known);
        assert_eq!(b[&pk(MINT_98)].value().unwrap().ui, 0.008488252);
        // Well-known mints resolve without the mint account.
        let mut bare = AccountSet::default();
        bare.insert(AccountRead {
            pubkey: pk(ATA_WSOL),
            slot: 1,
            state: AccountState::Absent,
        });
        let b = build_wallet_balances(&bare, &w, &requested()[..1], &none);
        assert_eq!(b[&ids::key(ids::WSOL)].value().unwrap().decimals, 9);
        // Wallet not read ⇒ lamports error; absent wallet ⇒ 0.
        assert!(lamports_from_set(&bare, &w).is_error());
        bare.insert(AccountRead {
            pubkey: w,
            slot: 1,
            state: AccountState::Absent,
        });
        assert_eq!(lamports_from_set(&bare, &w), Field::ok(0));
    }

    // ── WalletInventory ───────────────────────────────────────────────

    fn inventory() -> WalletInventory {
        let bal = json_of(fixture!("get_balance.json"));
        let lamports = bal["result"]["value"].as_u64().unwrap();
        let rows = parse_token_accounts_by_owner(
            &json_of(fixture!("token_accounts_tokenkeg.json")),
            &token(),
        )
        .unwrap();
        let rows22 = parse_token_accounts_by_owner(
            &json_of(fixture!("token_accounts_token2022.json")),
            &token_2022(),
        )
        .unwrap();
        let balances =
            build_wallet_balances(&gma_set(), &pk(WALLET), &requested(), &BTreeMap::new());
        build_wallet_inventory(
            pk(WALLET),
            GMA_SLOT,
            Field::ok(lamports),
            Field::ok(rows),
            Field::ok(rows22),
            balances,
        )
    }

    #[test]
    fn wallet_inventory_observation() {
        let inv = inventory();
        assert_eq!(inv.status(), ObsStatus::Ok);
        assert!(inv.errors().is_empty());
        assert_eq!(inv.sol(), Some(2.748145289));
        assert_eq!(
            inv.headline(),
            format!("wallet {WALLET} sol=2.748145289 token_accounts=2+0 wSOL=0 USDC=107.808931")
        );
        let f = inv.features();
        assert_features_ok(&f);
        assert_eq!(f["sol"], json!(2.748145289));
        assert_eq!(f["lamports"], json!(2_748_145_289i64));
        assert_eq!(f["token_accounts"], json!(2));
        assert_eq!(f["token_2022_accounts"], json!(0));
        assert_eq!(f["usdc"], json!(107.808931));
        assert_eq!(f["wsol"], json!(0.0));
        assert_eq!(f["sol_total"], json!(2.748145289));
        assert_eq!(f["balance_errors"], json!(0));

        let obs = Observation::of("solana_wallet", &inv, 1_000, WALLET_TTL_MS, ObsSource::Live);
        assert_eq!(obs.key, format!("solana_wallet/1:{WALLET}"));
        assert_eq!(obs.slot, Some(GMA_SLOT));
        assert_line1_fits(&obs, WALLET);
        assert_eq!(obs.typed::<WalletInventory>().unwrap(), inv);
        // Balances map keys are full mint strings.
        assert_eq!(
            obs.data["balances"][ids::USDC]["value"]["raw"],
            json!("107808931")
        );
    }

    #[test]
    fn wallet_inventory_partial_and_error() {
        let mut inv = inventory();
        let rpc_err = || ReadError::new("x", ErrorClass::RateLimited, "429");
        inv.lamports = Field::err(rpc_err());
        assert_eq!(inv.status(), ObsStatus::Partial);
        assert!(inv.headline().contains("sol=error"));
        let f = inv.features();
        assert!(!f.contains_key("sol") && !f.contains_key("sol_total"));
        assert_features_ok(&f);

        let inv = build_wallet_inventory(
            pk(WALLET),
            GMA_SLOT,
            Field::err(rpc_err()),
            Field::err(rpc_err()),
            Field::err(rpc_err()),
            BTreeMap::from([(ids::key(ids::USDC), Field::err(rpc_err()))]),
        );
        assert_eq!(inv.status(), ObsStatus::Error);
        assert_eq!(inv.errors().len(), 4);
        assert_eq!(inv.token_accounts.error().unwrap().field, "token_accounts");
        let obs = Observation::of("solana_wallet", &inv, 0, WALLET_TTL_MS, ObsSource::Live);
        assert_eq!(obs.status, ObsStatus::Error);
        assert_line1_fits(&obs, WALLET);
        assert_features_ok(&obs.features);
        assert!(
            obs.headline.contains("USDC=error")
                && obs.headline.contains("token_accounts=error+error"),
            "{}",
            obs.headline
        );
    }

    #[test]
    fn wallet_inventory_rejects_foreign_rows() {
        let inv = inventory();
        let Field::Ok { value: mut rows } = inv.token_accounts.clone() else {
            panic!()
        };
        rows[0].owner = pk(ATA_USDC);
        let inv = build_wallet_inventory(
            pk(WALLET),
            GMA_SLOT,
            inv.lamports.clone(),
            Field::ok(rows),
            inv.token_2022_accounts.clone(),
            inv.balances.clone(),
        );
        assert!(inv.token_accounts.is_error());
        assert_eq!(inv.status(), ObsStatus::Partial);
    }

    #[test]
    fn wallet_headline_counts_unlabeled_mints() {
        let set = gma_set();
        let mut req = requested();
        req.push((pk(MINT_98), pk(ATA_98), token()));
        let balances = build_wallet_balances(&set, &pk(WALLET), &req, &BTreeMap::new());
        let inv = build_wallet_inventory(
            pk(WALLET),
            GMA_SLOT,
            Field::ok(1),
            Field::ok(vec![]),
            Field::ok(vec![]),
            balances,
        );
        assert!(inv.headline().ends_with(" +1 mints"), "{}", inv.headline());
        assert!(inv.headline().contains("sol=0.000000001"));
    }

    // ── TxStatus ──────────────────────────────────────────────────────

    #[test]
    fn tx_status_ok_finalized() {
        let statuses = json_of(fixture!("sig_statuses_history.json"));
        let tx = json_of(fixture!("tx_ok.json"));
        let st = parse_tx_status(sig(SIG_OK), None, &statuses, Some(Ok(&tx)));
        assert!(st.found);
        assert_eq!(st.context_slot, Some(450_101_639));
        assert_eq!(st.slot, Some(447_559_707));
        assert_eq!(st.confirmation, Some(Confirmation::Finalized));
        assert_eq!(st.confirmations, None);
        assert_eq!(st.err, None);
        assert_eq!(st.succeeded(), Some(true));
        assert_eq!(st.fee_lamports, Field::ok(7_600));
        // meta.computeUnitsConsumed (the first log line is a ComputeBudget
        // invoke, not a CU count).
        assert_eq!(st.compute_units, Field::ok(190_822));
        assert_eq!(st.block_time, Field::ok(1_789_573_439));
        assert!(st.is_final());
        assert_eq!(st.ttl_ms(), TX_FINAL_TTL_MS);
        assert_eq!(st.status(), ObsStatus::Ok);
        assert_eq!(
            st.headline(),
            format!("tx {SIG_OK} finalized ok slot=447559707 fee=7600 cu=190822")
        );
        let f = st.features();
        assert_features_ok(&f);
        assert_eq!(f["final"], json!(true));
        assert_eq!(f["compute_units"], json!(190_822));
        assert!(!f.contains_key("err_kind"));

        let obs = Observation::of("solana_tx", &st, 5_000, st.ttl_ms(), ObsSource::Live);
        assert_eq!(obs.key, format!("solana_tx/1:{SIG_OK}"));
        assert_eq!(obs.slot, Some(450_101_639));
        assert_line1_fits(&obs, SIG_OK);
        assert_eq!(obs.typed::<TxStatus>().unwrap(), st);
    }

    #[test]
    fn tx_status_failed_tx() {
        // Entry 1 of the history fixture is the failed tx; the parser uses
        // entry 0, so build a one-entry response from it.
        let full = json_of(fixture!("sig_statuses_history.json"));
        let statuses = json!({
            "context": full["result"]["context"],
            "value": [full["result"]["value"][1]],
        });
        let tx = json_of(fixture!("tx_failed.json"));
        let st = parse_tx_status(sig(SIG_FAILED), None, &statuses, Some(Ok(&tx)));
        assert_eq!(st.slot, Some(439_814_004));
        assert_eq!(st.succeeded(), Some(false));
        assert_eq!(
            st.err,
            Some(json!({"InstructionError": [5, {"Custom": 3012}]}))
        );
        assert_eq!(st.fee_lamports, Field::ok(12_178));
        assert_eq!(st.compute_units, Field::ok(47_262));
        assert_eq!(st.status(), ObsStatus::Ok, "a failed tx is a valid answer");
        let f = st.features();
        assert_features_ok(&f);
        assert_eq!(f["succeeded"], json!(false));
        assert_eq!(f["err_kind"], json!("InstructionError"));
        assert_eq!(f["err_ix"], json!(5));
        assert_eq!(f["err_custom"], json!(3012));
        assert!(st.headline().contains("finalized failed"));
        let obs = Observation::of("solana_tx", &st, 0, st.ttl_ms(), ObsSource::Live);
        assert_line1_fits(&obs, SIG_FAILED);
    }

    #[test]
    fn tx_status_not_found_and_fallbacks() {
        // Never landed.
        let st = parse_tx_status(
            sig(SIG_UNKNOWN),
            None,
            &json_of(fixture!("sig_statuses_unknown.json")),
            Some(Ok(&json_of(fixture!("tx_unknown.json")))),
        );
        assert!(!st.found);
        assert_eq!(st.status(), ObsStatus::Absent);
        assert!(!st.is_final());
        assert_eq!(st.ttl_ms(), TX_TTL_MS);
        assert_eq!(st.succeeded(), None);
        assert_eq!(st.fee_lamports, Field::Absent);
        assert_eq!(st.headline(), format!("tx {SIG_UNKNOWN} not found"));
        let obs = Observation::of("solana_tx", &st, 0, st.ttl_ms(), ObsSource::Live);
        assert_line1_fits(&obs, SIG_UNKNOWN);
        assert_features_ok(&obs.features);

        // Status cache miss (no history search) but the ledger has the tx.
        let recent = json_of(fixture!("sig_statuses_recent_only.json"));
        let tx = json_of(fixture!("tx_ok.json"));
        let st = parse_tx_status(sig(SIG_OK), None, &recent, Some(Ok(&tx)));
        assert!(st.found);
        assert_eq!(st.slot, Some(447_559_707));
        assert_eq!(st.confirmation, None, "commitment unknown");
        assert!(!st.is_final());
        assert_eq!(st.compute_units, Field::ok(190_822));
        assert!(st.headline().contains(" landed ok "), "{}", st.headline());
        // Same without getTransaction: legitimately not found.
        let st = parse_tx_status(sig(SIG_OK), Some(7), &recent, None);
        assert!(!st.found);
        assert_eq!(st.context_slot, Some(7), "caller slot wins");
    }

    #[test]
    fn tx_status_errors_are_not_zeros() {
        let statuses = json_of(fixture!("sig_statuses_history.json"));
        // getTransaction failed: found (from statuses) but fee/CU errors.
        let st = parse_tx_status(
            sig(SIG_OK),
            None,
            &statuses,
            Some(Err(ReadError::new("tx", ErrorClass::RateLimited, "429"))),
        );
        assert!(st.found && st.is_final());
        assert_eq!(st.status(), ObsStatus::Partial);
        assert_eq!(st.errors().len(), 3);
        assert_eq!(st.compute_units.error().unwrap().field, "compute_units");
        assert!(!st.features().contains_key("fee_lamports"));
        // getTransaction answered for another signature.
        let tx = json_of(fixture!("tx_failed.json"));
        let st = parse_tx_status(sig(SIG_OK), None, &statuses, Some(Ok(&tx)));
        assert_eq!(st.status(), ObsStatus::Partial);
        assert!(st
            .fee_lamports
            .error()
            .unwrap()
            .message
            .contains(SIG_FAILED));
        // Undecodable / error statuses ⇒ Error, never "not found".
        for bad in [
            json!({"jsonrpc": "2.0", "error": {"code": 429, "message": "Too many requests for a specific RPC call"}, "id": 1}),
            json!({"result": {"context": {"slot": 1}, "value": []}}),
            json!({"result": {"context": {"slot": 1}, "value": [{"confirmationStatus": "finalized"}]}}),
            json!({"result": {"context": {"slot": 1}, "value": [{"slot": 5, "confirmationStatus": "rooted?"}]}}),
        ] {
            let st = parse_tx_status(sig(SIG_OK), None, &bad, None);
            assert_eq!(st.status(), ObsStatus::Error, "{bad}");
            assert!(!st.found);
            let obs = Observation::of("solana_tx", &st, 0, st.ttl_ms(), ObsSource::Live);
            assert_eq!(obs.errors.len(), 1);
            assert!(obs.headline.contains("status unavailable"));
            assert_line1_fits(&obs, SIG_OK);
        }
        let st = TxStatus::failed(
            sig(SIG_OK),
            None,
            ReadError::new("rpc", ErrorClass::QuotaExhausted, "max usage reached"),
        );
        assert_eq!(st.errors()[0].field, "status");
        assert_eq!(st.errors()[0].class, ErrorClass::QuotaExhausted);
        assert!(st.features().is_empty());
    }

    #[test]
    fn tx_status_legacy_confirmations_null_is_finalized() {
        let v = json!({"context": {"slot": 10}, "value": [{"slot": 9, "confirmations": null, "err": null, "status": {"Ok": null}}]});
        let st = parse_tx_status(sig(SIG_OK), None, &v, None);
        assert_eq!(st.confirmation, Some(Confirmation::Finalized));
        let v = json!([{"slot": 9, "confirmations": 3, "err": null, "confirmationStatus": "confirmed"}]);
        let st = parse_tx_status(sig(SIG_OK), Some(12), &v, None);
        assert_eq!(st.confirmation, Some(Confirmation::Confirmed));
        assert_eq!(st.confirmations, Some(3));
        assert_eq!(st.ttl_ms(), TX_TTL_MS);
        assert_eq!(st.context_slot, Some(12));
    }
}
