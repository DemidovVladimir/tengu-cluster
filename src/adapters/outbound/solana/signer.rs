//! Local ed25519 signer (`ports::solana_signer::SolanaSigner`) — the
//! `[solana] signer_key_file` key, or a one-shot keypair for a new DLMM
//! position. Signing is `ed25519-dalek` (never hand-rolled).
//!
//! | Rule | Why |
//! |---|---|
//! | Key file must be mode 0600 (no group / other bits) | like `ssh`: a readable key is a leaked key |
//! | Formats: solana-keygen JSON array of 64 bytes, or a base58 64-byte key (the bot's `PRIVATE_KEY`) | bot parity |
//! | Second half of the 64 bytes must equal the key derived from the first | catches a truncated / mixed-up file |
//! | Errors are fixed strings — never file content, never a parser message | a parse error can echo input (review H2) |
//! | `Debug` prints the public key only; buffers are zeroized | no secret in logs |

use std::fmt;
use std::path::Path;

use ed25519_dalek::{Signer as _, SigningKey};
use zeroize::{Zeroize, Zeroizing};

use crate::domain::solana::{bs58_decode, Pubkey, Signature};
use crate::domain::solana_tx::{MessageView, Transaction};
use crate::ports::solana_signer::SolanaSigner;

/// Largest key file read (a 64-byte JSON array is ~250 bytes).
const MAX_KEY_FILE_BYTES: u64 = 1024;

pub(crate) struct LocalKeypair {
    key: SigningKey,
}

impl LocalKeypair {
    pub(crate) fn from_seed(seed: &[u8; 32]) -> Self {
        LocalKeypair {
            key: SigningKey::from_bytes(seed),
        }
    }

    /// Fresh keypair from the OS RNG (a new DLMM position account).
    pub(crate) fn generate() -> anyhow::Result<Self> {
        let mut seed = Zeroizing::new([0u8; 32]);
        getrandom::getrandom(seed.as_mut())
            .map_err(|e| anyhow::anyhow!("OS random source failed: {e}"))?;
        Ok(Self::from_seed(&seed))
    }
}

impl fmt::Debug for LocalKeypair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LocalKeypair({})", self.pubkey())
    }
}

impl SolanaSigner for LocalKeypair {
    fn pubkey(&self) -> Pubkey {
        Pubkey(self.key.verifying_key().to_bytes())
    }
    fn sign(&self, message: &[u8]) -> Signature {
        Signature(self.key.sign(message).to_bytes())
    }
}

/// Why a key file was refused. `Display` is a fixed sentence per variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyFileError {
    Missing,
    NotAFile,
    Unreadable,
    /// Group / other permission bits set (the octal mode is not secret).
    OpenPermissions(u32),
    TooLarge,
    Format,
    Mismatch,
}

impl fmt::Display for KeyFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyFileError::Missing => f.write_str("key file does not exist"),
            KeyFileError::NotAFile => f.write_str("key file is not a regular file"),
            KeyFileError::Unreadable => f.write_str("key file could not be read"),
            KeyFileError::OpenPermissions(mode) => write!(
                f,
                "key file mode is {mode:o}; group / other must have no access (chmod 600)"
            ),
            KeyFileError::TooLarge => {
                write!(f, "key file is larger than {MAX_KEY_FILE_BYTES} bytes")
            }
            KeyFileError::Format => f.write_str(
                "key file is not a solana-keygen JSON array of 64 bytes or a base58 64-byte key",
            ),
            KeyFileError::Mismatch => {
                f.write_str("key file's public half does not match its secret half")
            }
        }
    }
}

impl std::error::Error for KeyFileError {}

/// Load the signer key. Never echoes content (see the module table).
pub(crate) fn load_key_file(path: &Path) -> Result<LocalKeypair, KeyFileError> {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(KeyFileError::Missing),
        Err(_) => return Err(KeyFileError::Unreadable),
    };
    if !meta.is_file() {
        return Err(KeyFileError::NotAFile);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(KeyFileError::OpenPermissions(mode));
        }
    }
    if meta.len() > MAX_KEY_FILE_BYTES {
        return Err(KeyFileError::TooLarge);
    }
    let raw = Zeroizing::new(std::fs::read(path).map_err(|_| KeyFileError::Unreadable)?);
    parse_key(&raw)
}

/// Parse key bytes (file content). Every failure is `Format` / `Mismatch`.
pub(crate) fn parse_key(raw: &[u8]) -> Result<LocalKeypair, KeyFileError> {
    let text = std::str::from_utf8(raw)
        .map_err(|_| KeyFileError::Format)?
        .trim();
    let mut bytes: Zeroizing<Vec<u8>> = if text.starts_with('[') {
        Zeroizing::new(serde_json::from_str::<Vec<u8>>(text).map_err(|_| KeyFileError::Format)?)
    } else if text.len() <= 90 {
        Zeroizing::new(bs58_decode(text).map_err(|_| KeyFileError::Format)?)
    } else {
        return Err(KeyFileError::Format);
    };
    if bytes.len() != 64 {
        return Err(KeyFileError::Format);
    }
    let mut seed = Zeroizing::new([0u8; 32]);
    seed.copy_from_slice(&bytes[..32]);
    let pair = LocalKeypair::from_seed(&seed);
    let matches = pair.pubkey().0[..] == bytes[32..];
    bytes.zeroize();
    if !matches {
        return Err(KeyFileError::Mismatch);
    }
    Ok(pair)
}

/// Fill the slots of `signers` in a parsed transaction. Every signer must
/// be a required signer of the message (else nothing is signed).
pub(crate) fn sign_transaction(
    tx: &mut Transaction,
    view: &MessageView,
    signers: &[&dyn SolanaSigner],
) -> anyhow::Result<()> {
    let mut slots = Vec::with_capacity(signers.len());
    for s in signers {
        let k = s.pubkey();
        let i = view
            .signer_index(&k)
            .ok_or_else(|| anyhow::anyhow!("{k} is not a required signer of this transaction"))?;
        slots.push((i, *s));
    }
    for (i, s) in slots {
        tx.signatures[i] = s.sign(&tx.message).0;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::solana::bs58_encode;
    use crate::domain::solana_tx::golden::{golden, ix, key, signer, unhex};
    use crate::domain::solana_tx::{Instruction, LegacyMessage};

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn rfc8032_test_vector_1() {
        let seed: [u8; 32] =
            unhex("9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60")
                .try_into()
                .unwrap();
        let k = LocalKeypair::from_seed(&seed);
        assert_eq!(
            hex(&k.pubkey().0),
            "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a"
        );
        assert_eq!(
            hex(&k.sign(b"").0),
            "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b"
        );
    }

    #[test]
    fn signatures_match_web3() {
        let g = golden();
        let wallet = LocalKeypair::from_seed(&[1; 32]);
        assert_eq!(wallet.pubkey(), signer("wallet"));
        assert_eq!(
            LocalKeypair::from_seed(&[2; 32]).pubkey(),
            signer("position")
        );
        for case in g["ed25519"].as_array().unwrap() {
            let msg = unhex(case["msg"].as_str().unwrap());
            assert_eq!(hex(&wallet.sign(&msg).0), case["sig"].as_str().unwrap());
        }
    }

    fn message(name: &str) -> serde_json::Value {
        golden()["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["name"] == name)
            .cloned()
            .unwrap()
    }

    #[test]
    fn signed_legacy_tx_matches_web3_byte_for_byte() {
        let m = message("legacy_open");
        let ixs: Vec<Instruction> = m["ixs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| ix(n.as_str().unwrap()))
            .collect();
        let payer = key(m["payer"].as_str().unwrap());
        let bh = key(m["blockhash"].as_str().unwrap()).0;
        let msg = LegacyMessage::compile(&payer, &ixs, bh).unwrap();
        let mut tx = Transaction::unsigned(&msg);
        let (_, view) = Transaction::parse(&tx.serialize()).unwrap();
        let (w, p) = (
            LocalKeypair::from_seed(&[1; 32]),
            LocalKeypair::from_seed(&[2; 32]),
        );
        sign_transaction(&mut tx, &view, &[&p, &w]).unwrap();
        assert_eq!(hex(&tx.serialize()), m["tx"].as_str().unwrap());
    }

    #[test]
    fn fills_only_our_slot_in_a_foreign_v0_tx() {
        let m = message("v0_foreign_payer");
        let (mut tx, view) =
            Transaction::parse(&unhex(m["unsigned_tx"].as_str().unwrap())).unwrap();
        sign_transaction(&mut tx, &view, &[&LocalKeypair::from_seed(&[1; 32])]).unwrap();
        assert_eq!(
            hex(&tx.serialize()),
            m["wallet_signed_tx"].as_str().unwrap()
        );
        assert_eq!(tx.signatures[0], [0u8; 64], "payer slot untouched");
        let stranger = LocalKeypair::from_seed(&[9; 32]);
        let before = tx.clone();
        assert!(sign_transaction(&mut tx, &view, &[&stranger]).is_err());
        assert_eq!(tx, before, "nothing signed on error");
    }

    fn keygen_bytes(seed: u8) -> Vec<u8> {
        let k = LocalKeypair::from_seed(&[seed; 32]);
        let mut v = vec![seed; 32];
        v.extend_from_slice(&k.pubkey().0);
        v
    }

    #[test]
    fn parses_keygen_json_and_base58() {
        let bytes = keygen_bytes(7);
        let json = serde_json::to_string(&bytes).unwrap();
        let want = LocalKeypair::from_seed(&[7; 32]).pubkey();
        assert_eq!(parse_key(json.as_bytes()).unwrap().pubkey(), want);
        assert_eq!(
            parse_key(format!("  {json}\n").as_bytes())
                .unwrap()
                .pubkey(),
            want
        );
        let b58 = bs58_encode(&bytes);
        assert_eq!(parse_key(b58.as_bytes()).unwrap().pubkey(), want);
    }

    #[test]
    fn refuses_bad_keys_without_echoing_them() {
        let mut mismatched = keygen_bytes(7);
        mismatched[40] ^= 1;
        let secret_json = serde_json::to_string(&keygen_bytes(5)[..40]).unwrap();
        let cases: Vec<(Vec<u8>, KeyFileError)> = vec![
            (
                serde_json::to_vec(&mismatched).unwrap(),
                KeyFileError::Mismatch,
            ),
            (secret_json.clone().into_bytes(), KeyFileError::Format),
            (b"[1,2,300]".to_vec(), KeyFileError::Format),
            (
                b"4vJ9JU1bJJE96FWSJKvHsmmFADCg4gpZQff4P3bkLKi0OlI".to_vec(),
                KeyFileError::Format,
            ),
            (
                bs58_encode(&keygen_bytes(3)[..63]).into_bytes(),
                KeyFileError::Format,
            ),
            (vec![0xff, 0xfe], KeyFileError::Format),
            (vec![b'x'; 200], KeyFileError::Format),
        ];
        for (raw, want) in cases {
            let err = parse_key(&raw).unwrap_err();
            assert_eq!(err, want);
            let msg = format!("{err} {err:?}");
            let text = String::from_utf8_lossy(&raw);
            for w in text.as_bytes().windows(6) {
                let w = String::from_utf8_lossy(w);
                assert!(!msg.contains(w.as_ref()), "error echoes input: {msg}");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn key_file_permissions_and_size_are_enforced() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("signer.json");
        std::fs::write(&p, serde_json::to_vec(&keygen_bytes(4)).unwrap()).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            load_key_file(&p).unwrap_err(),
            KeyFileError::OpenPermissions(0o644)
        );
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            load_key_file(&p).unwrap().pubkey(),
            LocalKeypair::from_seed(&[4; 32]).pubkey()
        );
        assert_eq!(
            load_key_file(&dir.path().join("nope")).unwrap_err(),
            KeyFileError::Missing
        );
        assert_eq!(
            load_key_file(dir.path()).unwrap_err(),
            KeyFileError::NotAFile
        );
        let big = dir.path().join("big");
        std::fs::write(&big, vec![b' '; 2000]).unwrap();
        std::fs::set_permissions(&big, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(load_key_file(&big).unwrap_err(), KeyFileError::TooLarge);
    }

    #[test]
    fn debug_shows_the_public_key_only() {
        let k = LocalKeypair::from_seed(&[1; 32]);
        let d = format!("{k:?}");
        assert_eq!(d, format!("LocalKeypair({})", signer("wallet")));
        assert!(!d.contains(&hex(&[1; 32])));
    }

    #[test]
    fn generated_keys_differ() {
        let a = LocalKeypair::generate().unwrap();
        let b = LocalKeypair::generate().unwrap();
        assert_ne!(a.pubkey(), b.pubkey());
    }
}
