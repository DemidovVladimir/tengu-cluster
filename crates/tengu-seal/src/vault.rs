//! The Worker's key material, all derived from one 32-byte seed that the
//! `KeyVault` Durable Object generates once and never exports.
//!
//! | Derived | How |
//! |---|---|
//! | HPKE key pair (X25519) | `Kem::derive_keypair(seed)` (RFC 9180 §7.1.3) |
//! | `kid` | full lowercase hex SHA-256 of the 32-byte public key |
//! | session key (ChaCha20-Poly1305) | HKDF-SHA256(seed, info = `tengu-seal/session/v1`) |

use hpke::{Deserializable, Kem as _, Serializable};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::{b64d, b64e, Code, Error};

/// HPKE suite: DHKEM(X25519, HKDF-SHA256), HKDF-SHA256, ChaCha20-Poly1305.
pub type Kem = hpke::kem::X25519HkdfSha256;
pub type Kdf = hpke::kdf::HkdfSha256;
pub type Aead = hpke::aead::ChaCha20Poly1305;

/// Name of the suite, published next to the public key.
pub const SUITE: &str = "hpke-x25519-hkdfsha256-chacha20poly1305";

const SESSION_INFO: &[u8] = b"tengu-seal/session/v1";

/// The recipient side (Worker only).
pub struct Vault {
    private: <Kem as hpke::Kem>::PrivateKey,
    public: <Kem as hpke::Kem>::PublicKey,
    session_key: Zeroizing<[u8; 32]>,
}

impl Vault {
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        let (private, public) = Kem::derive_keypair(seed);
        let hk = hkdf::Hkdf::<Sha256>::new(None, seed);
        let mut session_key = Zeroizing::new([0u8; 32]);
        hk.expand(SESSION_INFO, session_key.as_mut())
            .expect("32 bytes is a valid HKDF-SHA256 output length");
        Self {
            private,
            public,
            session_key,
        }
    }

    pub fn public_key(&self) -> PublicKey {
        PublicKey(self.public.clone())
    }

    pub(crate) fn private(&self) -> &<Kem as hpke::Kem>::PrivateKey {
        &self.private
    }

    pub(crate) fn session_key(&self) -> &[u8; 32] {
        &self.session_key
    }
}

/// The sealing side (tengu): what `GET /pubkey` returns.
#[derive(Clone)]
pub struct PublicKey(pub(crate) <Kem as hpke::Kem>::PublicKey);

impl PublicKey {
    pub fn to_b64(&self) -> String {
        b64e(&self.0.to_bytes())
    }

    pub fn from_b64(s: &str) -> Result<Self, Error> {
        let bytes = b64d(s)?;
        <Kem as hpke::Kem>::PublicKey::from_bytes(&bytes)
            .map(PublicKey)
            .map_err(|_| Error::new(Code::BadRequest, "invalid X25519 public key"))
    }

    /// Full lowercase hex SHA-256 of the public key bytes.
    pub fn kid(&self) -> String {
        let digest = Sha256::digest(self.0.to_bytes());
        digest.iter().map(|b| format!("{b:02x}")).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_keys_and_kid() {
        let a = Vault::from_seed(&[7u8; 32]);
        let b = Vault::from_seed(&[7u8; 32]);
        assert_eq!(a.public_key().to_b64(), b.public_key().to_b64());
        assert_eq!(a.public_key().kid(), b.public_key().kid());
        assert_eq!(a.public_key().kid().len(), 64);
        assert_eq!(a.session_key(), b.session_key());
    }

    #[test]
    fn different_seed_different_keys() {
        let a = Vault::from_seed(&[1u8; 32]);
        let b = Vault::from_seed(&[2u8; 32]);
        assert_ne!(a.public_key().kid(), b.public_key().kid());
        assert_ne!(a.session_key(), b.session_key());
    }

    #[test]
    fn public_key_round_trips_through_b64() {
        let v = Vault::from_seed(&[3u8; 32]);
        let pk = PublicKey::from_b64(&v.public_key().to_b64()).unwrap();
        assert_eq!(pk.kid(), v.public_key().kid());
        assert!(PublicKey::from_b64("AAAA").is_err());
    }
}
