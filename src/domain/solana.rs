//! Solana primitives — `Pubkey` / `Signature` (hand-rolled base58), program
//! ids, raw account reads (`AccountRead`, cached as `acct/1:<pubkey>`) and the
//! `AccountSet` a typed builder decodes. Pure: no IO. PDA derivation lives
//! here too (`find_program_address`, `ata`).
//!
//! Identifiers are always rendered in full base58 — never shortened.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use alloy::primitives::U256;
use base64::Engine as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

use crate::domain::observation::{set_int, set_str, Features, Observed};

// ---------------------------------------------------------------------------
// base58 (Bitcoin alphabet, as Solana uses)
// ---------------------------------------------------------------------------

const ALPHABET: &[u8; 58] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

/// Encode bytes as base58. Leading zero bytes become leading `1`s.
pub fn bs58_encode(bytes: &[u8]) -> String {
    let zeros = bytes.iter().take_while(|b| **b == 0).count();
    // Base-58 digits, little-endian.
    let mut digits: Vec<u8> = Vec::with_capacity(bytes.len() * 138 / 100 + 1);
    for &byte in &bytes[zeros..] {
        let mut carry = byte as u32;
        for d in digits.iter_mut() {
            carry += (*d as u32) << 8;
            *d = (carry % 58) as u8;
            carry /= 58;
        }
        while carry > 0 {
            digits.push((carry % 58) as u8);
            carry /= 58;
        }
    }
    let mut out = String::with_capacity(zeros + digits.len());
    out.extend(std::iter::repeat('1').take(zeros));
    out.extend(digits.iter().rev().map(|d| ALPHABET[*d as usize] as char));
    out
}

/// Decode base58. Errors on characters outside the alphabet.
pub fn bs58_decode(s: &str) -> Result<Vec<u8>, String> {
    let ones = s.bytes().take_while(|c| *c == b'1').count();
    // Base-256 bytes, little-endian.
    let mut bytes: Vec<u8> = Vec::with_capacity(s.len());
    for c in s.bytes().skip(ones) {
        let v = ALPHABET
            .iter()
            .position(|a| *a == c)
            .ok_or_else(|| format!("invalid base58 character {:?}", c as char))?
            as u32;
        let mut carry = v;
        for b in bytes.iter_mut() {
            carry += (*b as u32) * 58;
            *b = (carry & 0xff) as u8;
            carry >>= 8;
        }
        while carry > 0 {
            bytes.push((carry & 0xff) as u8);
            carry >>= 8;
        }
    }
    let mut out = vec![0u8; ones];
    out.extend(bytes.iter().rev());
    Ok(out)
}

// ---------------------------------------------------------------------------
// Pubkey / Signature
// ---------------------------------------------------------------------------

/// 32-byte account address. `Display` / serde = full base58.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Pubkey(pub [u8; 32]);

impl Pubkey {
    /// Read 32 bytes at `offset` (account layouts). `None` if out of range.
    pub fn read(data: &[u8], offset: usize) -> Option<Pubkey> {
        let s = data.get(offset..offset + 32)?;
        let mut b = [0u8; 32];
        b.copy_from_slice(s);
        Some(Pubkey(b))
    }
}

/// Longest base58 rendering of 32 bytes. `bs58_decode` is O(n²), so longer
/// (untrusted) input is refused before decoding.
const PUBKEY_MAX_B58: usize = 44;
/// Longest base58 rendering of 64 bytes (see [`PUBKEY_MAX_B58`]).
const SIGNATURE_MAX_B58: usize = 88;

/// `s` trimmed, or an error (full input kept) when it is longer than `max`.
fn bounded<'a>(s: &'a str, what: &str, max: usize) -> Result<&'a str, String> {
    let t = s.trim();
    if t.len() > max {
        return Err(format!(
            "{what} must be at most {max} base58 chars, got {}: {s}",
            t.len()
        ));
    }
    Ok(t)
}

impl FromStr for Pubkey {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let v = bs58_decode(bounded(s, "pubkey", PUBKEY_MAX_B58)?)?;
        let b: [u8; 32] = v
            .try_into()
            .map_err(|v: Vec<u8>| format!("pubkey must be 32 bytes, got {}: {s}", v.len()))?;
        Ok(Pubkey(b))
    }
}

impl fmt::Display for Pubkey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&bs58_encode(&self.0))
    }
}

impl fmt::Debug for Pubkey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Pubkey({self})")
    }
}

impl Serialize for Pubkey {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Pubkey {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// 64-byte transaction signature. `Display` / serde = full base58.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Signature(pub [u8; 64]);

impl FromStr for Signature {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let v = bs58_decode(bounded(s, "signature", SIGNATURE_MAX_B58)?)?;
        let b: [u8; 64] = v
            .try_into()
            .map_err(|v: Vec<u8>| format!("signature must be 64 bytes, got {}: {s}", v.len()))?;
        Ok(Signature(b))
    }
}

impl fmt::Display for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&bs58_encode(&self.0))
    }
}

impl fmt::Debug for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Signature({self})")
    }
}

impl Serialize for Signature {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Signature {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// Program ids and mints (full strings, never shortened)
// ---------------------------------------------------------------------------

pub mod ids {
    pub const SYSTEM: &str = "11111111111111111111111111111111";
    pub const TOKEN: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
    pub const TOKEN_2022: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";
    pub const ATA: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
    pub const WSOL: &str = "So11111111111111111111111111111111111111112";
    pub const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
    pub const DLMM: &str = "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo";
    pub const JUP_PERPS: &str = "PERPHjGBqRHArX4DySjwM6UJHiR3sWAatqfdBS2qQJu";
    pub const JLP_POOL: &str = "5BUwFW4nRbftYTDMbgxykoFWqWHPzahFSNAaaaJtVKsq";
    pub const JUP_CUSTODY_SOL: &str = "7xS2gz2bTp3fwCC7knJvUWTEU9Tycczu6VhJYKgi1wdz";
    pub const JUP_CUSTODY_USDC: &str = "G18jKKXQwBbrHeiK3C9MRXhkHsLHf7XgCSisykV46EZa";
    pub const COMPUTE_BUDGET: &str = "ComputeBudget111111111111111111111111111111";
    pub const MEMO: &str = "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr";
    pub const SYSVAR_RENT: &str = "SysvarRent111111111111111111111111111111111";
    /// DLMM `["__event_authority"]` PDA (Anchor events CPI).
    pub const DLMM_EVENT_AUTHORITY: &str = "D1ZN9Wj1fRSUQfCjhvnu1hqDMT7hzjzBBpi12nVniYD6";
    /// Jupiter perps `["__event_authority"]` PDA.
    pub const JUP_PERPS_EVENT_AUTHORITY: &str = "37hJBDnntwqhGbK7L6M1bLyvccj4u55CCUiLPdYkiqBN";
    /// Jupiter perps `["perpetuals"]` PDA (global config account).
    pub const JUP_PERPETUALS: &str = "H4ND9aYttUVLFmNypZqLjZ52FYiGvdEB45GmwNoKEjTj";

    /// Parse a constant from this module (they are valid by construction).
    pub fn key(id: &str) -> super::Pubkey {
        id.parse()
            .expect("ids:: constants are valid base58 pubkeys")
    }
}

// ---------------------------------------------------------------------------
// Raw account reads
// ---------------------------------------------------------------------------

/// Account contents at a slot. `Absent` = the account does not exist (a
/// legitimate answer, e.g. a flat perp side); RPC failures never produce it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AccountState {
    Ok {
        owner: Pubkey,
        lamports: u64,
        /// Account data, base64 (standard alphabet).
        data_b64: String,
        executable: bool,
    },
    Absent,
}

/// One account as read by `getMultipleAccounts` (or a phase-5 stream),
/// cached under `acct/1:<pubkey>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountRead {
    pub pubkey: Pubkey,
    /// `context.slot` of the read that produced it.
    pub slot: u64,
    #[serde(flatten)]
    pub state: AccountState,
}

impl AccountRead {
    /// Decoded data bytes; `None` when absent or the base64 is invalid.
    pub fn data(&self) -> Option<Vec<u8>> {
        match &self.state {
            AccountState::Ok { data_b64, .. } => base64::engine::general_purpose::STANDARD
                .decode(data_b64)
                .ok(),
            AccountState::Absent => None,
        }
    }
    pub fn owner(&self) -> Option<&Pubkey> {
        match &self.state {
            AccountState::Ok { owner, .. } => Some(owner),
            AccountState::Absent => None,
        }
    }
    pub fn lamports(&self) -> Option<u64> {
        match &self.state {
            AccountState::Ok { lamports, .. } => Some(*lamports),
            AccountState::Absent => None,
        }
    }
    pub fn exists(&self) -> bool {
        matches!(self.state, AccountState::Ok { .. })
    }
    /// Build an `Ok` read from raw bytes (fixtures; phase-5 streams).
    #[cfg(test)]
    pub fn from_bytes(
        pubkey: Pubkey,
        slot: u64,
        owner: Pubkey,
        lamports: u64,
        data: &[u8],
    ) -> Self {
        AccountRead {
            pubkey,
            slot,
            state: AccountState::Ok {
                owner,
                lamports,
                data_b64: base64::engine::general_purpose::STANDARD.encode(data),
                executable: false,
            },
        }
    }
}

impl Observed for AccountRead {
    const SCHEMA: &'static str = "acct/1";
    fn subject(&self) -> String {
        self.pubkey.to_string()
    }
    fn headline(&self) -> String {
        match &self.state {
            AccountState::Ok {
                owner, lamports, ..
            } => {
                format!("account {} owner {owner} lamports {lamports}", self.pubkey)
            }
            AccountState::Absent => format!("account {} absent", self.pubkey),
        }
    }
    fn features(&self) -> Features {
        let mut f = Features::new();
        set_str(
            &mut f,
            "state",
            Some(if self.exists() { "ok" } else { "absent" }),
        );
        set_int(&mut f, "lamports", self.lamports().map(|l| l as i64));
        set_int(&mut f, "data_len", self.data().map(|d| d.len() as i64));
        f
    }
    fn slot(&self) -> Option<u64> {
        Some(self.slot)
    }
}

/// The accounts a typed builder decodes, with the slot range they span.
/// `slot_min == slot_max` when all came from one `getMultipleAccounts`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AccountSet {
    pub accounts: BTreeMap<Pubkey, AccountRead>,
    pub slot_min: u64,
    pub slot_max: u64,
}

impl AccountSet {
    pub fn get(&self, key: &Pubkey) -> Option<&AccountRead> {
        self.accounts.get(key)
    }
    /// Insert and widen the slot range.
    pub fn insert(&mut self, read: AccountRead) {
        if self.accounts.is_empty() {
            self.slot_min = read.slot;
            self.slot_max = read.slot;
        } else {
            self.slot_min = self.slot_min.min(read.slot);
            self.slot_max = self.slot_max.max(read.slot);
        }
        self.accounts.insert(read.pubkey, read);
    }
    /// Data bytes of an existing account owned by `owner` with at least
    /// `min_len` bytes; `None` otherwise (absent, wrong owner, short).
    #[cfg(test)]
    pub fn data_owned_by(&self, key: &Pubkey, owner: &Pubkey, min_len: usize) -> Option<Vec<u8>> {
        let read = self.get(key)?;
        if read.owner() != Some(owner) {
            return None;
        }
        read.data().filter(|d| d.len() >= min_len)
    }
}

// ---------------------------------------------------------------------------
// Program derived addresses (PDA) — hand-rolled, no solana-sdk
// ---------------------------------------------------------------------------

/// Max seeds per address (`solana_program::pubkey::MAX_SEEDS`).
pub const MAX_SEEDS: usize = 16;
/// Max bytes per seed (`solana_program::pubkey::MAX_SEED_LEN`).
pub const MAX_SEED_LEN: usize = 32;
const PDA_MARKER: &[u8] = b"ProgramDerivedAddress";

/// p = 2^255 − 19 (little-endian limbs).
const FIELD_P: U256 = U256::from_limbs([
    0xffff_ffff_ffff_ffed,
    0xffff_ffff_ffff_ffff,
    0xffff_ffff_ffff_ffff,
    0x7fff_ffff_ffff_ffff,
]);
/// Edwards d = −121665 / 121666 mod p (checked in `pda_tests`).
const EDWARDS_D: U256 = U256::from_limbs([
    0x75eb_4dca_1359_78a3,
    0x0070_0a4d_4141_d8ab,
    0x8cc7_4079_7779_e898,
    0x5203_6cee_2b6f_fe73,
]);

/// Whether 32 bytes decode to an ed25519 point — exactly
/// curve25519-dalek 4.1 `CompressedEdwardsY::decompress().is_some()`, which
/// Solana's `bytes_are_curve_point` calls: y = the low 255 bits reduced
/// mod p (a non-canonical y ≥ p is accepted; the sign bit is ignored),
/// u = y² − 1, v = d·y² + 1, on the curve iff u = 0 or u/v is a square in
/// GF(p). v ≠ 0 always (−1/d is a non-square), so the Euler criterion on
/// u·v (= u/v · v²) decides without an inversion. Cross-checked against
/// dalek in `pda_tests`.
pub fn is_on_curve(bytes: &[u8; 32]) -> bool {
    let mut b = *bytes;
    b[31] &= 0x7f;
    let one = U256::from(1u8);
    let y = U256::from_le_bytes(b).reduce_mod(FIELD_P);
    let yy = y.mul_mod(y, FIELD_P);
    let u = yy.add_mod(FIELD_P - one, FIELD_P);
    let v = EDWARDS_D.mul_mod(yy, FIELD_P).add_mod(one, FIELD_P);
    if u.is_zero() {
        return true;
    }
    if v.is_zero() {
        return false;
    }
    u.mul_mod(v, FIELD_P).pow_mod((FIELD_P - one) >> 1, FIELD_P) == one
}

fn seeds_valid(seeds: &[&[u8]], max_seeds: usize) -> bool {
    seeds.len() <= max_seeds && seeds.iter().all(|s| s.len() <= MAX_SEED_LEN)
}

/// `sha256(seeds ‖ program ‖ "ProgramDerivedAddress")` when that hash is
/// off the ed25519 curve (`Pubkey::create_program_address`). `None` when it
/// is on the curve, or when the seeds are invalid (> 16 seeds or a seed
/// > 32 bytes).
pub fn create_program_address(seeds: &[&[u8]], program: &Pubkey) -> Option<Pubkey> {
    if !seeds_valid(seeds, MAX_SEEDS) {
        return None;
    }
    let mut h = Sha256::new();
    for s in seeds {
        h.update(s);
    }
    h.update(program.0);
    h.update(PDA_MARKER);
    let bytes: [u8; 32] = h.finalize().into();
    (!is_on_curve(&bytes)).then_some(Pubkey(bytes))
}

/// The canonical PDA: the first bump from 255 down to 1 whose address is
/// off the curve (`Pubkey::try_find_program_address`; like solana-program
/// and web3.js, bump 0 is never tried). `None` for invalid seeds (> 15
/// seeds — the bump is the 16th — or a seed > 32 bytes) or, with
/// probability ~2^-255, no off-curve bump.
pub fn try_find_program_address(seeds: &[&[u8]], program: &Pubkey) -> Option<(Pubkey, u8)> {
    if !seeds_valid(seeds, MAX_SEEDS - 1) {
        return None;
    }
    for bump in (1..=u8::MAX).rev() {
        let bump_seed = [bump];
        let mut with_bump: Vec<&[u8]> = Vec::with_capacity(seeds.len() + 1);
        with_bump.extend_from_slice(seeds);
        with_bump.push(&bump_seed);
        if let Some(k) = create_program_address(&with_bump, program) {
            return Some((k, bump));
        }
    }
    None
}

/// [`try_find_program_address`] for seeds the caller builds from fixed-size
/// parts (pubkeys, short literals, integers). Panics on invalid seeds —
/// a programming error, as in `Pubkey::find_program_address`.
pub fn find_program_address(seeds: &[&[u8]], program: &Pubkey) -> (Pubkey, u8) {
    try_find_program_address(seeds, program)
        .expect("valid seeds (<= 15, each <= 32 bytes) have an off-curve bump")
}

/// Associated token account of `owner` for `mint` under `token_program`
/// (Tokenkeg or Token-2022): seeds `[owner, token_program, mint]`, program
/// `ids::ATA`. Works for off-curve (PDA) owners too.
pub fn ata(owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    find_program_address(&[&owner.0, &token_program.0, &mint.0], &ids::key(ids::ATA)).0
}

/// Meteora DLMM bin array PDA: seeds `["bin_array", lb_pair, index i64 LE]`,
/// program `ids::DLMM`. `index` = `gates::bin_array_index(bin_id)`
/// (floor(bin_id / 70)).
pub fn bin_array_pda(lb_pair: &Pubkey, index: i64) -> Pubkey {
    find_program_address(
        &[b"bin_array", &lb_pair.0, &index.to_le_bytes()],
        &ids::key(ids::DLMM),
    )
    .0
}

/// Jupiter perps SOL position PDA of `wallet` (`jupiterPerps.ts:86-101`):
/// seeds `["position", wallet, JLP_POOL, JUP_CUSTODY_SOL, collateral
/// custody, [side]]` — long = SOL collateral custody + side 1, short = USDC
/// collateral custody + side 2; program `ids::JUP_PERPS`.
pub fn jup_position_pda(wallet: &Pubkey, side_long: bool) -> Pubkey {
    let (collateral, side) = if side_long {
        (ids::key(ids::JUP_CUSTODY_SOL), 1u8)
    } else {
        (ids::key(ids::JUP_CUSTODY_USDC), 2u8)
    };
    find_program_address(
        &[
            b"position",
            &wallet.0,
            &ids::key(ids::JLP_POOL).0,
            &ids::key(ids::JUP_CUSTODY_SOL).0,
            &collateral.0,
            &[side],
        ],
        &ids::key(ids::JUP_PERPS),
    )
    .0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base58_round_trips_known_ids() {
        for id in [
            ids::SYSTEM,
            ids::TOKEN,
            ids::TOKEN_2022,
            ids::ATA,
            ids::WSOL,
            ids::USDC,
            ids::DLMM,
            ids::JUP_PERPS,
            ids::JLP_POOL,
            ids::JUP_CUSTODY_SOL,
            ids::JUP_CUSTODY_USDC,
        ] {
            let k: Pubkey = id.parse().unwrap();
            assert_eq!(k.to_string(), id);
        }
        assert_eq!(ids::key(ids::SYSTEM).0, [0u8; 32]);
    }

    #[test]
    fn base58_edge_cases() {
        assert_eq!(bs58_encode(&[]), "");
        assert_eq!(bs58_encode(&[0, 0, 1]), "112");
        assert_eq!(bs58_decode("112").unwrap(), vec![0, 0, 1]);
        assert_eq!(bs58_encode(&[0xff; 4]), "7YXq9G");
        assert!(bs58_decode("0OIl").is_err());
        assert!("abc".parse::<Pubkey>().is_err(), "wrong length rejected");
    }

    #[test]
    fn over_long_ids_are_refused_before_decoding() {
        // Valid base58 that would decode to more bytes: refused on length,
        // the full input echoed.
        let long_pk = "z".repeat(PUBKEY_MAX_B58 + 1);
        let e = long_pk.parse::<Pubkey>().unwrap_err();
        assert!(e.contains("at most 44 base58 chars, got 45"), "{e}");
        assert!(e.contains(&long_pk), "full input kept: {e}");
        let e = "z"
            .repeat(SIGNATURE_MAX_B58 + 1)
            .parse::<Signature>()
            .unwrap_err();
        assert!(e.contains("at most 88 base58 chars, got 89"), "{e}");
        // The longest valid ids still parse; surrounding whitespace is free.
        let max_pk = bs58_encode(&[0xff; 32]);
        assert_eq!(max_pk.len(), PUBKEY_MAX_B58);
        let k: Pubkey = format!("  {max_pk}\n").parse().unwrap();
        assert_eq!(k.0, [0xff; 32]);
        let sig = Signature([0xff; 64]);
        assert_eq!(sig.to_string().len(), SIGNATURE_MAX_B58);
        assert_eq!(sig.to_string().parse::<Signature>().unwrap(), sig);
        // A megabyte of base58 fails fast (was a quadratic decode: minutes).
        let started = std::time::Instant::now();
        assert!("z".repeat(1 << 20).parse::<Pubkey>().is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn signature_round_trip() {
        let mut b = [0u8; 64];
        for (i, x) in b.iter_mut().enumerate() {
            *x = (i * 7 + 3) as u8;
        }
        let s = Signature(b);
        let parsed: Signature = s.to_string().parse().unwrap();
        assert_eq!(parsed, s);
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<Signature>(&json).unwrap(), s);
    }

    #[test]
    fn account_read_serde_and_data() {
        let k = ids::key(ids::JLP_POOL);
        let r = AccountRead::from_bytes(k, 42, ids::key(ids::JUP_PERPS), 7, &[1, 2, 3]);
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["pubkey"], ids::JLP_POOL);
        assert_eq!(v["state"], "ok");
        let back: AccountRead = serde_json::from_value(v).unwrap();
        assert_eq!(back.data().unwrap(), vec![1, 2, 3]);
        let mut set = AccountSet::default();
        set.insert(back);
        set.insert(AccountRead {
            pubkey: ids::key(ids::USDC),
            slot: 40,
            state: AccountState::Absent,
        });
        assert_eq!((set.slot_min, set.slot_max), (40, 42));
        assert!(set
            .data_owned_by(&k, &ids::key(ids::JUP_PERPS), 3)
            .is_some());
        assert!(
            set.data_owned_by(&k, &ids::key(ids::DLMM), 3).is_none(),
            "wrong owner"
        );
        assert!(
            set.data_owned_by(&k, &ids::key(ids::JUP_PERPS), 4)
                .is_none(),
            "too short"
        );
    }
}

/// PDA vectors below were derived with `@solana/web3.js` and checked
/// against mainnet on
/// 2026-09-24 (slot 450101020): the bin arrays and the Jupiter short
/// position exist with the expected owner / discriminator / index, the long
/// position and the wSOL ATA are absent (never opened), and the ATAs match
/// `getTokenAccountsByOwner` (slot 450100789). `live_solcore_*` in
/// `adapters/outbound/solana/rpc.rs` re-checks them.
#[cfg(test)]
mod pda_tests {
    use super::*;
    use curve25519_dalek::edwards::CompressedEdwardsY;

    const WALLET: &str = "F3YvPiLdniRPGpeKrbeGWR2zg2wPpzVuvqBA5BBJBQ5S";
    const POOL: &str = "5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS6";

    fn k(s: &str) -> Pubkey {
        s.parse().unwrap()
    }

    fn dalek(b: &[u8; 32]) -> bool {
        CompressedEdwardsY(*b).decompress().is_some()
    }

    fn le(x: U256) -> [u8; 32] {
        x.to_le_bytes()
    }

    #[test]
    fn edwards_d_constant() {
        // d · 121666 ≡ −121665 (mod p)
        let lhs = EDWARDS_D.mul_mod(U256::from(121_666u32), FIELD_P);
        let rhs = FIELD_P - U256::from(121_665u32);
        assert_eq!(lhs, rhs);
        assert_eq!(FIELD_P, (U256::from(1u8) << 255) - U256::from(19u8));
    }

    #[test]
    fn is_on_curve_matches_dalek_on_2048_hashes() {
        let mut on = 0;
        for i in 0u32..2048 {
            let mut b: [u8; 32] = Sha256::digest(i.to_le_bytes()).into();
            assert_eq!(is_on_curve(&b), dalek(&b), "sha256({i})");
            on += usize::from(dalek(&b));
            // The sign bit never changes validity.
            b[31] ^= 0x80;
            assert_eq!(is_on_curve(&b), dalek(&b), "sha256({i}) sign flipped");
        }
        // 4096 checks; about half of all y are valid, so both branches run.
        assert!((800..1250).contains(&on), "on-curve count {on}");
    }

    #[test]
    fn is_on_curve_matches_dalek_on_edge_cases() {
        let p = FIELD_P;
        let one = U256::from(1u8);
        let mut cases: Vec<[u8; 32]> = vec![
            [0u8; 32],   // y = 0
            le(one),     // y = 1 (u = 0)
            le(p - one), // y = −1 (u = 0)
            le(p),       // non-canonical 0
            le(p + one), // non-canonical 1
            le(p + U256::from(2u8)),
            le((one << 255) - one), // 2^255 − 1 = non-canonical 18
            [0xff; 32],             // 2^255 − 1 with the sign bit set
            le(U256::from(2u8)),
            le(EDWARDS_D),
            le(p - EDWARDS_D),
        ];
        // Every non-canonical encoding p..2^255−1.
        for extra in 0u8..19 {
            cases.push(le(p + U256::from(extra)));
        }
        for i in 0u64..200 {
            cases.push(le(U256::from(i)));
            cases.push(le(p - U256::from(i + 1)));
        }
        for ids in [
            ids::SYSTEM,
            ids::TOKEN,
            ids::TOKEN_2022,
            ids::ATA,
            ids::WSOL,
            ids::USDC,
            ids::DLMM,
            ids::JUP_PERPS,
            ids::JLP_POOL,
            WALLET,
        ] {
            cases.push(k(ids).0);
        }
        let n = cases.len();
        for (i, b) in cases.into_iter().enumerate() {
            assert_eq!(is_on_curve(&b), dalek(&b), "case {i}: {b:?}");
            let mut s = b;
            s[31] |= 0x80;
            assert_eq!(is_on_curve(&s), dalek(&s), "case {i} sign set");
        }
        assert!(n > 400);
        // Known answers: u = 0 is valid, a wallet is on the curve.
        assert!(is_on_curve(&le(one)) && is_on_curve(&le(p - one)));
        assert!(is_on_curve(&k(WALLET).0));
    }

    #[test]
    fn pda_jupiter_positions_match_web3js() {
        let w = k(WALLET);
        let short = jup_position_pda(&w, false);
        let long = jup_position_pda(&w, true);
        assert_eq!(
            short.to_string(),
            "6HFhuYzQGcqdj4NGwC6vfVETRvMA3pXaVeZnHgWSKsJK"
        );
        assert_eq!(
            long.to_string(),
            "FqymRcB92t63jpwh7om4RLbxMNUGoHnZPQMkkAA8ksVY"
        );
        for pda in [short, long] {
            assert!(!is_on_curve(&pda.0) && !dalek(&pda.0));
        }
        // Both need bump 253: 255 and 254 hash onto the curve.
        let pool = ids::key(ids::JLP_POOL);
        let sol = ids::key(ids::JUP_CUSTODY_SOL);
        let usdc = ids::key(ids::JUP_CUSTODY_USDC);
        let prog = ids::key(ids::JUP_PERPS);
        let seeds = |bump: u8| -> Option<Pubkey> {
            create_program_address(
                &[b"position", &w.0, &pool.0, &sol.0, &usdc.0, &[2], &[bump]],
                &prog,
            )
        };
        assert_eq!(seeds(255), None);
        assert_eq!(seeds(254), None);
        assert_eq!(seeds(253), Some(short));
        assert_eq!(
            find_program_address(&[b"position", &w.0, &pool.0, &sol.0, &sol.0, &[1]], &prog),
            (long, 253)
        );
    }

    #[test]
    fn pda_atas_match_token_accounts_by_owner() {
        let w = k(WALLET);
        let token = ids::key(ids::TOKEN);
        // Live token accounts of the wallet (getTokenAccountsByOwner).
        assert_eq!(
            ata(&w, &ids::key(ids::USDC), &token).to_string(),
            "D9ScKYy15cw1tpkkuwEnDKv62nCyuETwrvRSdP4usGg1"
        );
        assert_eq!(
            ata(
                &w,
                &k("98sMhvDwXj1RQi5c5Mndm3vPe9cBqPrbLaufMXFNMh5g"),
                &token
            )
            .to_string(),
            "DQsXEvePuhSGqJZzsKrg3QTSJcRWFcBvTL6F8THrCiVF"
        );
        // Derived only (the account does not exist on chain).
        assert_eq!(
            ata(&w, &ids::key(ids::WSOL), &token).to_string(),
            "E4PCnfEconGJW6vf7GDycEnkFe1VWC7teiNWJzQv3NTA"
        );
        // The token program is a seed: Token-2022 gives another address.
        assert_ne!(
            ata(&w, &ids::key(ids::USDC), &ids::key(ids::TOKEN_2022)),
            ata(&w, &ids::key(ids::USDC), &token)
        );
    }

    #[test]
    fn pda_dlmm_bin_arrays_match_web3js() {
        let pool = k(POOL);
        for (index, want, bump) in [
            (
                -79i64,
                "2E855k5fegqhZppWQms1XrYyQQSVFNFipHg3igabX1Y3",
                252u8,
            ),
            (-78, "HP15ZCgcgunsV9ypHKpDJSURFn4dn7i63uCNB4k7K5MV", 255),
            (-77, "Vc6P6kjaRUgnQCwycL4QzTG3tiNR4CgC3gqWHvvMMJh", 254),
            (-76, "Ehkf9XQLVnY8HV6jbbDU25fTxF1qQ3NuScWfawSb79pu", 255),
            (-75, "G9QNw5nwv6JMkLSQ8ignWWEXybUfwBoJm4z5goGbU7d", 253),
            (0, "41J5yxxAQbkoCPFoxcGA9QhsvEEEVnDihmEyQPYPwWzQ", 255),
            (1, "HKCz5fKmPKxEvsgd68W9EBHpDz8o5FfEjQUyasXUCsnh", 254),
            (-1, "FaEgvDgeDxdKFDrZnDt7W6qzJTSLQrg4orMLCG35GFxz", 255),
        ] {
            assert_eq!(
                bin_array_pda(&pool, index).to_string(),
                want,
                "index {index}"
            );
            let (_, b) = find_program_address(
                &[b"bin_array", &pool.0, &index.to_le_bytes()],
                &ids::key(ids::DLMM),
            );
            assert_eq!(b, bump, "index {index}");
        }
        // Active bin -5373 at capture time lies in array -77.
        assert_eq!(crate::domain::lp::gates::bin_array_index(-5373), -77);
    }

    #[test]
    fn pda_single_seed_program_accounts() {
        let prog = ids::key(ids::JUP_PERPS);
        assert_eq!(
            find_program_address(&[b"perpetuals"], &prog),
            (k("H4ND9aYttUVLFmNypZqLjZ52FYiGvdEB45GmwNoKEjTj"), 255)
        );
        assert_eq!(
            find_program_address(&[b"__event_authority"], &prog),
            (k("37hJBDnntwqhGbK7L6M1bLyvccj4u55CCUiLPdYkiqBN"), 253)
        );
    }

    #[test]
    fn pda_seed_limits() {
        let prog = ids::key(ids::DLMM);
        let s32 = [7u8; 32];
        let s33 = [7u8; 33];
        let sixteen: Vec<&[u8]> = vec![&s32[..]; 16];
        let seventeen: Vec<&[u8]> = vec![&s32[..]; 17];
        let fifteen: Vec<&[u8]> = vec![&s32[..]; 15];
        assert!(create_program_address(&[&s33], &prog).is_none());
        assert!(create_program_address(&seventeen, &prog).is_none());
        // 16 seeds are allowed for create; find needs room for the bump.
        let sixteen_ok = (0u8..=255).any(|b| {
            let last = [b];
            let mut s = fifteen.clone();
            s.push(&last);
            create_program_address(&s, &prog).is_some()
        });
        assert!(sixteen_ok, "16 seeds are accepted");
        assert!(try_find_program_address(&sixteen, &prog).is_none());
        assert!(try_find_program_address(&[&s33], &prog).is_none());
        let (pda, _) = try_find_program_address(&fifteen, &prog).unwrap();
        assert!(!is_on_curve(&pda.0));
    }

    /// `acct/1` rows (`AccountRead`) honour the observation contract: full
    /// ids in line 1 (≤ 200 chars), scalar features only.
    #[test]
    fn account_read_observation_contract() {
        use crate::domain::observation::{
            assert_features_ok, ObsSource, Observation, MAX_LINE1_CHARS,
        };
        let key = k("FqymRcB92t63jpwh7om4RLbxMNUGoHnZPQMkkAA8ksVY");
        let owner = ids::key(ids::TOKEN_2022);
        for read in [
            AccountRead::from_bytes(key, 450_104_084, owner, u64::MAX, &[7u8; 165]),
            AccountRead {
                pubkey: key,
                slot: u64::MAX,
                state: AccountState::Absent,
            },
        ] {
            assert_features_ok(&read.features());
            let obs = Observation::of("solana_accounts", &read, 0, 60_000, ObsSource::Live);
            assert_eq!(
                obs.key,
                "acct/1:FqymRcB92t63jpwh7om4RLbxMNUGoHnZPQMkkAA8ksVY"
            );
            let text = obs.render_text(i64::MAX / 2);
            let line1 = text.lines().next().unwrap();
            assert!(line1.chars().count() <= MAX_LINE1_CHARS, "{line1}");
            assert!(line1.contains("FqymRcB92t63jpwh7om4RLbxMNUGoHnZPQMkkAA8ksVY"));
            if read.exists() {
                assert!(line1.contains(ids::TOKEN_2022), "{line1}");
            }
            assert_eq!(obs.typed::<AccountRead>().unwrap(), read);
        }
    }
}
