//! Solana primitives — `Pubkey` / `Signature` (hand-rolled base58), program
//! ids, raw account reads (`AccountRead`, cached as `acct/1:<pubkey>`) and the
//! `AccountSet` a typed builder decodes. Pure: no IO. PDA derivation lives
//! here too (`find_program_address`, `ata`).
//!
//! Identifiers are always rendered in full base58 — never shortened.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use base64::Engine as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

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
    pub fn to_bytes(self) -> [u8; 32] {
        self.0
    }
    /// Read 32 bytes at `offset` (account layouts). `None` if out of range.
    pub fn read(data: &[u8], offset: usize) -> Option<Pubkey> {
        let s = data.get(offset..offset + 32)?;
        let mut b = [0u8; 32];
        b.copy_from_slice(s);
        Some(Pubkey(b))
    }
}

impl FromStr for Pubkey {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let v = bs58_decode(s.trim())?;
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
        let v = bs58_decode(s.trim())?;
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
    pub const DOVES: &str = "DoVEsk76QybCEHQGzkvYPWLQu9gzNoZZZt3TPiL597e";

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
    /// Build an `Ok` read from raw bytes (fixtures, streams).
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
    pub fn data_owned_by(&self, key: &Pubkey, owner: &Pubkey, min_len: usize) -> Option<Vec<u8>> {
        let read = self.get(key)?;
        if read.owner() != Some(owner) {
            return None;
        }
        read.data().filter(|d| d.len() >= min_len)
    }
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
            ids::DOVES,
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
