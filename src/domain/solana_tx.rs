//! Solana transaction wire format, hand-rolled (no solana-sdk): instructions,
//! legacy message compile + serialize, parsing of a transaction built
//! elsewhere (legacy or v0 — e.g. a Jupiter Ultra order) to find and fill our
//! signature slot, and the small System / SPL Token / ATA / ComputeBudget
//! instructions the write tools need. Pure: no IO, no signing (the
//! `SolanaSigner` port signs [`LegacyMessage::serialize`] / the parsed
//! message bytes).
//!
//! Account order follows web3.js `TransactionMessage.compileToLegacyMessage`
//! (`CompiledKeys`): payer first, then keys in first-appearance order (an
//! instruction's program id before its accounts), partitioned into writable
//! signers, read-only signers, writable non-signers, read-only non-signers.
//! Checked byte-for-byte against `tests/fixtures/solana/tx/golden.json`
//! (`scripts/golden/write_ixs.cjs`).

use crate::domain::solana::{ids, Pubkey};

/// Max serialized transaction size (`PACKET_DATA_SIZE`).
pub const PACKET_DATA_SIZE: usize = 1232;
/// Compute-unit ceiling of one transaction.
pub const MAX_COMPUTE_UNITS: u32 = 1_400_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountMeta {
    pub pubkey: Pubkey,
    pub is_signer: bool,
    pub is_writable: bool,
}

impl AccountMeta {
    pub fn writable(pubkey: Pubkey, is_signer: bool) -> Self {
        AccountMeta {
            pubkey,
            is_signer,
            is_writable: true,
        }
    }
    pub fn readonly(pubkey: Pubkey, is_signer: bool) -> Self {
        AccountMeta {
            pubkey,
            is_signer,
            is_writable: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instruction {
    pub program_id: Pubkey,
    pub accounts: Vec<AccountMeta>,
    pub data: Vec<u8>,
}

// ---------------------------------------------------------------------------
// compact-u16 ("shortvec")
// ---------------------------------------------------------------------------

pub fn encode_compact_u16(mut n: u16, out: &mut Vec<u8>) {
    loop {
        let mut b = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            out.push(b);
            return;
        }
        b |= 0x80;
        out.push(b);
    }
}

/// `(value, bytes read)`; `None` on truncated or over-long (> 3 bytes,
/// > u16) input.
pub fn decode_compact_u16(bytes: &[u8]) -> Option<(u16, usize)> {
    let mut v: u32 = 0;
    for i in 0..3 {
        let b = *bytes.get(i)?;
        v |= u32::from(b & 0x7f) << (7 * i);
        if b & 0x80 == 0 {
            return u16::try_from(v).ok().map(|v| (v, i + 1));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Legacy message
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageHeader {
    pub num_required_signatures: u8,
    pub num_readonly_signed: u8,
    pub num_readonly_unsigned: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledInstruction {
    pub program_id_index: u8,
    pub accounts: Vec<u8>,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyMessage {
    pub header: MessageHeader,
    pub account_keys: Vec<Pubkey>,
    pub recent_blockhash: [u8; 32],
    pub instructions: Vec<CompiledInstruction>,
}

impl LegacyMessage {
    /// Compile `ixs` with `payer` as fee payer (web3.js
    /// `compileToLegacyMessage` order — see the module doc).
    pub fn compile(
        payer: &Pubkey,
        ixs: &[Instruction],
        recent_blockhash: [u8; 32],
    ) -> Result<Self, String> {
        // (key, signer, writable) in first-appearance order.
        let mut keys: Vec<(Pubkey, bool, bool)> = vec![(*payer, true, true)];
        let mut upsert = |k: Pubkey, s: bool, w: bool| {
            if let Some(e) = keys.iter_mut().find(|e| e.0 == k) {
                e.1 |= s;
                e.2 |= w;
            } else {
                keys.push((k, s, w));
            }
        };
        for ix in ixs {
            upsert(ix.program_id, false, false);
            for m in &ix.accounts {
                upsert(m.pubkey, m.is_signer, m.is_writable);
            }
        }
        let part = |s: bool, w: bool| keys.iter().filter(move |e| e.1 == s && e.2 == w);
        let account_keys: Vec<Pubkey> = part(true, true)
            .chain(part(true, false))
            .chain(part(false, true))
            .chain(part(false, false))
            .map(|e| e.0)
            .collect();
        if account_keys.len() > 256 {
            return Err(format!("{} account keys (max 256)", account_keys.len()));
        }
        let count = |s: bool, w: bool| part(s, w).count() as u8;
        let header = MessageHeader {
            num_required_signatures: count(true, true) + count(true, false),
            num_readonly_signed: count(true, false),
            num_readonly_unsigned: count(false, false),
        };
        let index = |k: &Pubkey| -> u8 {
            account_keys
                .iter()
                .position(|x| x == k)
                .expect("every instruction key was collected") as u8
        };
        let instructions = ixs
            .iter()
            .map(|ix| CompiledInstruction {
                program_id_index: index(&ix.program_id),
                accounts: ix.accounts.iter().map(|m| index(&m.pubkey)).collect(),
                data: ix.data.clone(),
            })
            .collect();
        Ok(LegacyMessage {
            header,
            account_keys,
            recent_blockhash,
            instructions,
        })
    }

    /// The bytes every signer signs.
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = vec![
            self.header.num_required_signatures,
            self.header.num_readonly_signed,
            self.header.num_readonly_unsigned,
        ];
        encode_compact_u16(self.account_keys.len() as u16, &mut out);
        for k in &self.account_keys {
            out.extend_from_slice(&k.0);
        }
        out.extend_from_slice(&self.recent_blockhash);
        encode_compact_u16(self.instructions.len() as u16, &mut out);
        for ix in &self.instructions {
            out.push(ix.program_id_index);
            encode_compact_u16(ix.accounts.len() as u16, &mut out);
            out.extend_from_slice(&ix.accounts);
            encode_compact_u16(ix.data.len() as u16, &mut out);
            out.extend_from_slice(&ix.data);
        }
        out
    }

    /// Required signers, in signature-slot order.
    #[cfg(test)]
    pub fn signers(&self) -> &[Pubkey] {
        &self.account_keys[..usize::from(self.header.num_required_signatures)]
    }

    /// Serialized size of the full transaction (signatures included).
    pub fn tx_size(&self) -> usize {
        let n = usize::from(self.header.num_required_signatures);
        let mut sig_len = Vec::new();
        encode_compact_u16(n as u16, &mut sig_len);
        sig_len.len() + 64 * n + self.serialize().len()
    }
}

// ---------------------------------------------------------------------------
// Transactions (legacy or v0) — parse, sign a slot, serialize
// ---------------------------------------------------------------------------

/// What a signer needs from a message built elsewhere: the header, the
/// static keys (signers are always static) and the blockhash. `version` =
/// `None` for legacy, `Some(0)` for v0. Lookup tables are validated for
/// shape, never resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageView {
    pub version: Option<u8>,
    pub header: MessageHeader,
    pub static_keys: Vec<Pubkey>,
    pub recent_blockhash: [u8; 32],
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let s = self
            .b
            .get(self.at..self.at + n)
            .ok_or_else(|| format!("message truncated at byte {}", self.at))?;
        self.at += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }
    fn compact(&mut self) -> Result<usize, String> {
        let (v, n) = decode_compact_u16(&self.b[self.at.min(self.b.len())..])
            .ok_or_else(|| format!("bad compact-u16 at byte {}", self.at))?;
        self.at += n;
        Ok(usize::from(v))
    }
    fn key(&mut self) -> Result<Pubkey, String> {
        Ok(Pubkey::read(self.take(32)?, 0).expect("32 bytes"))
    }
}

impl MessageView {
    /// Parse a whole legacy or v0 message (trailing bytes are an error).
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let mut r = Reader { b: bytes, at: 0 };
        let first = r.u8()?;
        let (version, nrs) = if first & 0x80 != 0 {
            let v = first & 0x7f;
            if v != 0 {
                return Err(format!("unsupported message version {v}"));
            }
            (Some(0), r.u8()?)
        } else {
            (None, first)
        };
        let header = MessageHeader {
            num_required_signatures: nrs,
            num_readonly_signed: r.u8()?,
            num_readonly_unsigned: r.u8()?,
        };
        let n_keys = r.compact()?;
        let static_keys = (0..n_keys)
            .map(|_| r.key())
            .collect::<Result<Vec<_>, _>>()?;
        if usize::from(nrs) > static_keys.len() || nrs == 0 {
            return Err(format!(
                "{nrs} required signatures with {} static keys",
                static_keys.len()
            ));
        }
        let mut recent_blockhash = [0u8; 32];
        recent_blockhash.copy_from_slice(r.take(32)?);
        for _ in 0..r.compact()? {
            r.u8()?;
            let n = r.compact()?;
            r.take(n)?;
            let n = r.compact()?;
            r.take(n)?;
        }
        if version.is_some() {
            for _ in 0..r.compact()? {
                r.key()?;
                let n = r.compact()?;
                r.take(n)?;
                let n = r.compact()?;
                r.take(n)?;
            }
        }
        if r.at != bytes.len() {
            return Err(format!(
                "{} trailing bytes after the message",
                bytes.len() - r.at
            ));
        }
        Ok(MessageView {
            version,
            header,
            static_keys,
            recent_blockhash,
        })
    }

    pub fn signers(&self) -> &[Pubkey] {
        &self.static_keys[..usize::from(self.header.num_required_signatures)]
    }

    /// Signature slot of `key`, when it is a required signer.
    pub fn signer_index(&self, key: &Pubkey) -> Option<usize> {
        self.signers().iter().position(|k| k == key)
    }
}

/// A serialized transaction: signature slots + the message bytes they sign.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transaction {
    pub signatures: Vec<[u8; 64]>,
    pub message: Vec<u8>,
}

impl Transaction {
    /// Unsigned transaction (zeroed slots) for a compiled legacy message.
    pub fn unsigned(msg: &LegacyMessage) -> Self {
        Transaction {
            signatures: vec![[0u8; 64]; usize::from(msg.header.num_required_signatures)],
            message: msg.serialize(),
        }
    }

    /// Parse wire bytes; the slot count must match the message header.
    pub fn parse(bytes: &[u8]) -> Result<(Self, MessageView), String> {
        let (n, used) = decode_compact_u16(bytes).ok_or("bad signature count")?;
        let n = usize::from(n);
        let sig_end = used + 64 * n;
        if bytes.len() < sig_end {
            return Err("transaction truncated in signatures".into());
        }
        let signatures = bytes[used..sig_end]
            .chunks_exact(64)
            .map(|c| c.try_into().expect("64-byte chunk"))
            .collect();
        let message = bytes[sig_end..].to_vec();
        let view = MessageView::parse(&message)?;
        if usize::from(view.header.num_required_signatures) != n {
            return Err(format!(
                "{n} signature slots for {} required signatures",
                view.header.num_required_signatures
            ));
        }
        Ok((
            Transaction {
                signatures,
                message,
            },
            view,
        ))
    }

    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(1 + 64 * self.signatures.len() + self.message.len());
        encode_compact_u16(self.signatures.len() as u16, &mut out);
        for s in &self.signatures {
            out.extend_from_slice(s);
        }
        out.extend_from_slice(&self.message);
        out
    }

    /// First signature = the transaction id once signed.
    pub fn id(&self) -> Option<[u8; 64]> {
        self.signatures.first().copied()
    }
}

// ---------------------------------------------------------------------------
// System / SPL Token / ATA / ComputeBudget instructions
// ---------------------------------------------------------------------------

/// System `Transfer` (instruction 2): `from` signs.
pub fn system_transfer(from: &Pubkey, to: &Pubkey, lamports: u64) -> Instruction {
    let mut data = 2u32.to_le_bytes().to_vec();
    data.extend_from_slice(&lamports.to_le_bytes());
    Instruction {
        program_id: ids::key(ids::SYSTEM),
        accounts: vec![
            AccountMeta::writable(*from, true),
            AccountMeta::writable(*to, false),
        ],
        data,
    }
}

/// SPL Token `SyncNative` (17): credit lamports sent to a wSOL account.
pub fn spl_sync_native(account: &Pubkey) -> Instruction {
    Instruction {
        program_id: ids::key(ids::TOKEN),
        accounts: vec![AccountMeta::writable(*account, false)],
        data: vec![17],
    }
}

/// SPL Token `CloseAccount` (9) under the account's own token program
/// (Tokenkeg or Token-2022): rent goes to `destination`, `owner` signs.
pub fn spl_close_account(
    account: &Pubkey,
    destination: &Pubkey,
    owner: &Pubkey,
    token_program: &Pubkey,
) -> Instruction {
    Instruction {
        program_id: *token_program,
        accounts: vec![
            AccountMeta::writable(*account, false),
            AccountMeta::writable(*destination, false),
            AccountMeta::readonly(*owner, true),
        ],
        data: vec![9],
    }
}

/// ATA program `CreateIdempotent` (1): no-op when the account exists.
pub fn ata_create_idempotent(
    payer: &Pubkey,
    ata: &Pubkey,
    owner: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
) -> Instruction {
    Instruction {
        program_id: ids::key(ids::ATA),
        accounts: vec![
            AccountMeta::writable(*payer, true),
            AccountMeta::writable(*ata, false),
            AccountMeta::readonly(*owner, false),
            AccountMeta::readonly(*mint, false),
            AccountMeta::readonly(ids::key(ids::SYSTEM), false),
            AccountMeta::readonly(*token_program, false),
        ],
        data: vec![1],
    }
}

/// ComputeBudget `SetComputeUnitLimit` (2).
pub fn cu_limit(units: u32) -> Instruction {
    let mut data = vec![2];
    data.extend_from_slice(&units.to_le_bytes());
    Instruction {
        program_id: ids::key(ids::COMPUTE_BUDGET),
        accounts: vec![],
        data,
    }
}

/// ComputeBudget `SetComputeUnitPrice` (3), micro-lamports per CU.
pub fn cu_price(micro_lamports: u64) -> Instruction {
    let mut data = vec![3];
    data.extend_from_slice(&micro_lamports.to_le_bytes());
    Instruction {
        program_id: ids::key(ids::COMPUTE_BUDGET),
        accounts: vec![],
        data,
    }
}

#[cfg(test)]
pub(crate) mod golden {
    //! `tests/fixtures/solana/tx/golden.json` accessors shared by the
    //! instruction-builder tests (`lp::dlmm_ix`, `lp::perps_ix`).
    use super::*;
    use serde_json::Value;

    pub(crate) const GOLDEN: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/solana/tx/golden.json"
    ));

    pub(crate) fn golden() -> Value {
        serde_json::from_str(GOLDEN).expect("golden.json parses")
    }

    pub(crate) fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
            .collect()
    }

    /// `k(n)` of the golden script: byte i = (i*7 + n) % 256.
    pub(crate) fn k(n: u32) -> Pubkey {
        let mut b = [0u8; 32];
        for (i, x) in b.iter_mut().enumerate() {
            *x = ((i as u32 * 7 + n) % 256) as u8;
        }
        Pubkey(b)
    }

    pub(crate) fn key(s: &str) -> Pubkey {
        s.parse().expect("golden pubkey")
    }

    pub(crate) fn signer(name: &str) -> Pubkey {
        key(golden()["signers"][name]["pubkey"].as_str().unwrap())
    }

    pub(crate) fn pda(name: &str) -> Pubkey {
        key(golden()["pdas"][name].as_str().unwrap())
    }

    /// The golden instruction `name`, as built by web3.js / Anchor.
    pub(crate) fn ix(name: &str) -> Instruction {
        let g = golden();
        let j = g["ixs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["name"] == name)
            .unwrap_or_else(|| panic!("golden ix {name}"));
        Instruction {
            program_id: key(j["program"].as_str().unwrap()),
            data: unhex(j["data"].as_str().unwrap()),
            accounts: j["metas"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| AccountMeta {
                    pubkey: key(m[0].as_str().unwrap()),
                    is_writable: m[1].as_bool().unwrap(),
                    is_signer: m[2].as_bool().unwrap(),
                })
                .collect(),
        }
    }

    /// Field-by-field comparison so a failure names the first difference.
    pub(crate) fn assert_ix(name: &str, got: &Instruction) {
        let want = ix(name);
        assert_eq!(got.program_id, want.program_id, "{name}: program");
        assert_eq!(got.data, want.data, "{name}: data");
        assert_eq!(
            got.accounts.len(),
            want.accounts.len(),
            "{name}: account count"
        );
        for (i, (g, w)) in got.accounts.iter().zip(&want.accounts).enumerate() {
            assert_eq!(g, w, "{name}: account #{i}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::golden::*;
    use super::*;

    #[test]
    fn compact_u16_round_trips_boundaries() {
        for (n, bytes) in [
            (0u16, vec![0x00]),
            (0x7f, vec![0x7f]),
            (0x80, vec![0x80, 0x01]),
            (0x3fff, vec![0xff, 0x7f]),
            (0x4000, vec![0x80, 0x80, 0x01]),
            (0xffff, vec![0xff, 0xff, 0x03]),
        ] {
            let mut out = Vec::new();
            encode_compact_u16(n, &mut out);
            assert_eq!(out, bytes, "{n}");
            assert_eq!(decode_compact_u16(&bytes), Some((n, bytes.len())));
        }
        assert_eq!(decode_compact_u16(&[0x80]), None, "truncated");
        assert_eq!(decode_compact_u16(&[0xff, 0xff, 0x04]), None, "> u16");
        assert_eq!(decode_compact_u16(&[0x80, 0x80, 0x80, 0x01]), None);
    }

    #[test]
    fn small_instructions_match_web3_and_spl_token() {
        let wallet = signer("wallet");
        let wsol = ids::key(ids::WSOL);
        let token = ids::key(ids::TOKEN);
        assert_ix(
            "system_transfer",
            &system_transfer(&wallet, &k(10), 123_456_789),
        );
        assert_ix("sync_native", &spl_sync_native(&k(11)));
        assert_ix(
            "close_account",
            &spl_close_account(&k(12), &wallet, &wallet, &token),
        );
        assert_ix(
            "close_account_2022",
            &spl_close_account(&k(13), &wallet, &wallet, &ids::key(ids::TOKEN_2022)),
        );
        let ata = crate::domain::solana::ata(&wallet, &wsol, &token);
        assert_ix(
            "ata_idempotent",
            &ata_create_idempotent(&wallet, &ata, &wallet, &wsol, &token),
        );
        assert_ix("cu_limit", &cu_limit(MAX_COMPUTE_UNITS));
        assert_ix("cu_price", &cu_price(12_345));
    }

    fn message_case(name: &str) -> serde_json::Value {
        golden()["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["name"] == name)
            .cloned()
            .unwrap()
    }

    #[test]
    fn legacy_compile_matches_compile_to_legacy_message() {
        let m = message_case("legacy_open");
        let ixs: Vec<Instruction> = m["ixs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| ix(n.as_str().unwrap()))
            .collect();
        let payer = key(m["payer"].as_str().unwrap());
        let bh = key(m["blockhash"].as_str().unwrap()).0;
        let msg = LegacyMessage::compile(&payer, &ixs, bh).unwrap();
        let want = unhex(m["message"].as_str().unwrap());
        assert_eq!(msg.serialize(), want);
        assert_eq!(msg.signers(), &[payer, signer("position")]);
        let tx = unhex(m["tx"].as_str().unwrap());
        assert_eq!(msg.tx_size(), tx.len());
        assert!(msg.tx_size() <= PACKET_DATA_SIZE);
        // Parsing the signed web3.js transaction gives back the same message.
        let (parsed, view) = Transaction::parse(&tx).unwrap();
        assert_eq!(parsed.message, want);
        assert_eq!(parsed.serialize(), tx);
        assert_eq!(view.version, None);
        assert_eq!(view.header, msg.header);
        assert_eq!(view.static_keys, msg.account_keys);
        assert_eq!(view.recent_blockhash, bh);
    }

    #[test]
    fn v0_with_lookups_parses_and_finds_our_slot() {
        let m = message_case("v0_foreign_payer");
        let (tx, view) = Transaction::parse(&unhex(m["unsigned_tx"].as_str().unwrap())).unwrap();
        assert_eq!(tx.message, unhex(m["message"].as_str().unwrap()));
        assert_eq!(view.version, Some(0));
        assert_eq!(
            usize::from(view.header.num_required_signatures),
            m["num_required_signatures"].as_u64().unwrap() as usize
        );
        let want_static: Vec<Pubkey> = m["static_keys"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| key(s.as_str().unwrap()))
            .collect();
        assert_eq!(view.static_keys, want_static);
        assert_eq!(view.signers()[0], key(m["payer"].as_str().unwrap()));
        assert_eq!(view.signer_index(&signer("wallet")), Some(1));
        assert_eq!(view.signer_index(&k(10)), None, "non-signer");
        assert!(tx.signatures.iter().all(|s| s == &[0u8; 64]));
    }

    #[test]
    fn parse_rejects_malformed_input() {
        let m = message_case("legacy_open");
        let tx = unhex(m["tx"].as_str().unwrap());
        assert!(
            Transaction::parse(&tx[..tx.len() - 1]).is_err(),
            "truncated"
        );
        let mut extra = tx.clone();
        extra.push(0);
        assert!(Transaction::parse(&extra).is_err(), "trailing byte");
        let mut wrong_slots = tx.clone();
        wrong_slots[0] = 1;
        assert!(Transaction::parse(&wrong_slots).is_err(), "slot count");
        let mut v1 = unhex(
            message_case("v0_foreign_payer")["message"]
                .as_str()
                .unwrap(),
        );
        v1[0] = 0x81;
        assert!(MessageView::parse(&v1).unwrap_err().contains("version 1"));
    }

    #[test]
    fn compile_merges_duplicate_keys_and_keeps_payer_first() {
        let payer = k(1);
        let ix = Instruction {
            program_id: k(9),
            accounts: vec![
                AccountMeta::readonly(k(2), false),
                AccountMeta::writable(k(2), false),
                AccountMeta::readonly(payer, false),
                AccountMeta::readonly(k(3), true),
            ],
            data: vec![],
        };
        let msg = LegacyMessage::compile(&payer, &[ix], [0; 32]).unwrap();
        assert_eq!(msg.account_keys, vec![payer, k(3), k(2), k(9)]);
        assert_eq!(
            msg.header,
            MessageHeader {
                num_required_signatures: 2,
                num_readonly_signed: 1,
                num_readonly_unsigned: 1,
            }
        );
        assert_eq!(msg.instructions[0].accounts, vec![2, 2, 0, 1]);
    }
}
