//! Session token: claims sealed by the Worker with its own session key, so
//! the Worker stores nothing per session. tengu keeps it in RAM only.
//!
//! Wire form: `tss1.<b64url 12-byte nonce>.<b64url ChaCha20-Poly1305(claims JSON)>`,
//! AAD `tss1`. Revocation does not wait for expiry: the Worker re-checks the
//! client allow-list (`clients:<fp>`) on every request.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use serde::{Deserialize, Serialize};

use crate::vault::Vault;
use crate::{b64d, b64e, Code, Error};

pub const PREFIX: &str = "tss1";
/// Session lifetime the Worker issues.
pub const TTL_MS: u64 = 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
    /// OpenSSH fingerprint of the client key that signed `/session`.
    pub fp: String,
    /// Operator-chosen label from the allow-list entry.
    pub label: String,
    pub exp_ms: u64,
    /// Worker key the session belongs to.
    pub kid: String,
}

/// Seal `claims` into a token; `nonce` must be fresh random bytes.
pub fn issue(vault: &Vault, claims: &Claims, nonce: [u8; 12]) -> String {
    let cipher = ChaCha20Poly1305::new(Key::from_slice(vault.session_key()));
    let pt = serde_json::to_vec(claims).expect("claims serialize");
    let ct = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &pt,
                aad: PREFIX.as_bytes(),
            },
        )
        .expect("ChaCha20-Poly1305 encrypt cannot fail for small inputs");
    format!("{PREFIX}.{}.{}", b64e(&nonce), b64e(&ct))
}

/// Open and check a token: authentic, this Worker key, not expired.
pub fn verify(vault: &Vault, token: &str, now_ms: u64) -> Result<Claims, Error> {
    let unauth = |m: &str| Error::new(Code::Unauthorized, m.to_string());
    let mut parts = token.trim().split('.');
    if parts.next() != Some(PREFIX) {
        return Err(unauth("not a session token"));
    }
    let nonce = b64d(parts.next().ok_or_else(|| unauth("malformed session"))?)?;
    let ct = b64d(parts.next().ok_or_else(|| unauth("malformed session"))?)?;
    if parts.next().is_some() || nonce.len() != 12 {
        return Err(unauth("malformed session"));
    }
    let cipher = ChaCha20Poly1305::new(Key::from_slice(vault.session_key()));
    let pt = cipher
        .decrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &ct,
                aad: PREFIX.as_bytes(),
            },
        )
        .map_err(|_| unauth("session is forged or from another Worker key"))?;
    let claims: Claims =
        serde_json::from_slice(&pt).map_err(|_| unauth("malformed session claims"))?;
    if claims.kid != vault.public_key().kid() {
        return Err(unauth("session is from another Worker key"));
    }
    if now_ms >= claims.exp_ms {
        return Err(unauth("session expired — tengu renews it on the next call"));
    }
    Ok(claims)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims(v: &Vault, exp_ms: u64) -> Claims {
        Claims {
            fp: "SHA256:abc".into(),
            label: "mac".into(),
            exp_ms,
            kid: v.public_key().kid(),
        }
    }

    #[test]
    fn issue_verify_round_trip() {
        let v = Vault::from_seed(&[4u8; 32]);
        let t = issue(&v, &claims(&v, 2_000), [1u8; 12]);
        assert!(t.starts_with("tss1."));
        assert_eq!(verify(&v, &t, 1_000).unwrap().label, "mac");
    }

    #[test]
    fn expired_forged_and_foreign_tokens_fail() {
        let v = Vault::from_seed(&[4u8; 32]);
        let other = Vault::from_seed(&[5u8; 32]);
        let t = issue(&v, &claims(&v, 2_000), [1u8; 12]);
        assert_eq!(verify(&v, &t, 2_000).unwrap_err().code, Code::Unauthorized);
        assert_eq!(
            verify(&other, &t, 1_000).unwrap_err().code,
            Code::Unauthorized
        );
        let mut flipped = t.clone();
        flipped.pop();
        flipped.push(if t.ends_with('A') { 'B' } else { 'A' });
        assert!(verify(&v, &flipped, 1_000).is_err());
        assert!(verify(&v, "tss1.x", 1_000).is_err());
        assert!(verify(&v, "Bearer x", 1_000).is_err());
    }
}
