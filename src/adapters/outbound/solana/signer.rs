//! In-memory ed25519 signer (`ports::solana_signer::SolanaSigner`): the
//! one-shot keypair of a new DLMM position account (made from the OS RNG,
//! used for that one transaction, never stored), and test wallets. The
//! wallet itself signs through Privy (`solana/privy.rs`); no key file is
//! read. Signing is `ed25519-dalek` (never hand-rolled).
//!
//! | Rule | Why |
//! |---|---|
//! | `Debug` prints the public key only; the seed buffer is zeroized | no secret in logs |
//! | [`sign_transaction`] signs every slot first, then writes them | nothing signed when one signer fails |

use std::fmt;

use async_trait::async_trait;
use ed25519_dalek::{Signer as _, SigningKey};
use zeroize::Zeroizing;

use crate::domain::solana::{Pubkey, Signature};
use crate::domain::solana_tx::{MessageView, Transaction};
use crate::ports::solana_signer::SolanaSigner;

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

    /// The signature, synchronously (an in-memory key cannot fail).
    pub(crate) fn sign_now(&self, message: &[u8]) -> Signature {
        Signature(self.key.sign(message).to_bytes())
    }
}

impl fmt::Debug for LocalKeypair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LocalKeypair({})", self.pubkey())
    }
}

#[async_trait]
impl SolanaSigner for LocalKeypair {
    fn pubkey(&self) -> Pubkey {
        Pubkey(self.key.verifying_key().to_bytes())
    }
    async fn sign(&self, message: &[u8]) -> anyhow::Result<Signature> {
        Ok(self.sign_now(message))
    }
}

/// Fill the slots of `signers` in a parsed transaction. Every signer must
/// be a required signer of the message, and every one must sign — else
/// nothing is written.
pub(crate) async fn sign_transaction(
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
    let mut sigs = Vec::with_capacity(slots.len());
    for (i, s) in slots {
        sigs.push((i, s.sign(&tx.message).await?));
    }
    for (i, sig) in sigs {
        tx.signatures[i] = sig.0;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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
            hex(&k.sign_now(b"").0),
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
            assert_eq!(hex(&wallet.sign_now(&msg).0), case["sig"].as_str().unwrap());
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

    #[tokio::test]
    async fn signed_legacy_tx_matches_web3_byte_for_byte() {
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
        sign_transaction(&mut tx, &view, &[&p, &w]).await.unwrap();
        assert_eq!(hex(&tx.serialize()), m["tx"].as_str().unwrap());
    }

    #[tokio::test]
    async fn fills_only_our_slot_in_a_foreign_v0_tx() {
        let m = message("v0_foreign_payer");
        let (mut tx, view) =
            Transaction::parse(&unhex(m["unsigned_tx"].as_str().unwrap())).unwrap();
        sign_transaction(&mut tx, &view, &[&LocalKeypair::from_seed(&[1; 32])])
            .await
            .unwrap();
        assert_eq!(
            hex(&tx.serialize()),
            m["wallet_signed_tx"].as_str().unwrap()
        );
        assert_eq!(tx.signatures[0], [0u8; 64], "payer slot untouched");
        let stranger = LocalKeypair::from_seed(&[9; 32]);
        let before = tx.clone();
        assert!(sign_transaction(&mut tx, &view, &[&stranger])
            .await
            .is_err());
        assert_eq!(tx, before, "nothing signed on error");
    }

    struct Failing(Pubkey);

    #[async_trait]
    impl SolanaSigner for Failing {
        fn pubkey(&self) -> Pubkey {
            self.0
        }
        async fn sign(&self, _: &[u8]) -> anyhow::Result<Signature> {
            anyhow::bail!("signer down")
        }
    }

    #[tokio::test]
    async fn one_failing_signer_writes_nothing() {
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
        let p = LocalKeypair::from_seed(&[2; 32]);
        let w = Failing(signer("wallet"));
        let before = tx.clone();
        let err = sign_transaction(&mut tx, &view, &[&p, &w])
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "signer down");
        assert_eq!(tx, before, "the position slot is not written either");
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
