//! Session token: claims signed by the Worker with HMAC-SHA256 under its
//! `SESSION_KEY` secret, so the Worker stores nothing per session. tengu
//! keeps the token in RAM only.
//!
//! Wire form: `tss1.<b64url claims JSON>.<b64url HMAC(prefix "." claims)>`.
//! Rotating `SESSION_KEY` ends every session at once; removing a client from
//! `CLIENTS` ends its sessions too (checked on every request).

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::{b64d, b64e, Code, Error};

pub const PREFIX: &str = "tss1";
/// Shortest `SESSION_KEY` the Worker accepts.
pub const MIN_KEY_BYTES: usize = 32;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
    /// OpenSSH fingerprint of the client key that signed `/session`.
    pub fp: String,
    /// Label from the `CLIENTS` entry.
    pub label: String,
    pub exp_ms: u64,
    /// Routes granted: the signed request ∩ the client's `CLIENTS` routes.
    pub routes: Vec<String>,
}

fn mac(key: &[u8], body_b64: &str) -> HmacSha256 {
    let mut m = HmacSha256::new_from_slice(key).expect("HMAC takes any key length");
    m.update(PREFIX.as_bytes());
    m.update(b".");
    m.update(body_b64.as_bytes());
    m
}

/// Check the `SESSION_KEY` length (the Worker refuses to issue otherwise).
pub fn check_key(key: &[u8]) -> Result<(), Error> {
    if key.len() < MIN_KEY_BYTES {
        return Err(Error::new(
            Code::Misconfigured,
            format!("SESSION_KEY must be at least {MIN_KEY_BYTES} characters"),
        ));
    }
    Ok(())
}

pub fn issue(key: &[u8], claims: &Claims) -> String {
    let body = b64e(&serde_json::to_vec(claims).expect("claims serialize"));
    let tag = mac(key, &body).finalize().into_bytes();
    format!("{PREFIX}.{body}.{}", b64e(&tag))
}

/// Check a token: authentic under `key`, not expired.
pub fn verify(key: &[u8], token: &str, now_ms: u64) -> Result<Claims, Error> {
    let unauth = |m: &str| Error::new(Code::Unauthorized, m.to_string());
    let mut parts = token.trim().split('.');
    if parts.next() != Some(PREFIX) {
        return Err(unauth("not a session token"));
    }
    let body = parts.next().ok_or_else(|| unauth("malformed session"))?;
    let tag = b64d(parts.next().ok_or_else(|| unauth("malformed session"))?)?;
    if parts.next().is_some() {
        return Err(unauth("malformed session"));
    }
    mac(key, body)
        .verify_slice(&tag)
        .map_err(|_| unauth("session is forged or SESSION_KEY was rotated"))?;
    let claims: Claims =
        serde_json::from_slice(&b64d(body)?).map_err(|_| unauth("malformed session claims"))?;
    if now_ms >= claims.exp_ms {
        return Err(unauth("session expired — restart tengu for a new one"));
    }
    Ok(claims)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = b"0123456789abcdef0123456789abcdef";

    fn claims(exp_ms: u64) -> Claims {
        Claims {
            fp: "SHA256:abc".into(),
            label: "mac".into(),
            exp_ms,
            routes: vec!["openrouter".into()],
        }
    }

    #[test]
    fn issue_verify_round_trip() {
        let t = issue(KEY, &claims(2_000));
        assert!(t.starts_with("tss1."));
        assert_eq!(verify(KEY, &t, 1_000).unwrap().label, "mac");
    }

    #[test]
    fn expired_forged_and_rotated_tokens_fail() {
        let t = issue(KEY, &claims(2_000));
        assert_eq!(verify(KEY, &t, 2_000).unwrap_err().code, Code::Unauthorized);
        let other = b"ffffffffffffffffffffffffffffffff";
        assert_eq!(
            verify(other, &t, 1_000).unwrap_err().code,
            Code::Unauthorized
        );
        // Changed claims with the old tag.
        let mut parts: Vec<&str> = t.split('.').collect();
        let forged_body = b64e(&serde_json::to_vec(&claims(9_999_999)).unwrap());
        parts[1] = &forged_body;
        assert!(verify(KEY, &parts.join("."), 1_000).is_err());
        assert!(verify(KEY, "tss1.x", 1_000).is_err());
        assert!(verify(KEY, "Bearer x", 1_000).is_err());
    }

    #[test]
    fn short_keys_are_refused() {
        assert!(check_key(b"short").is_err());
        assert!(check_key(KEY).is_ok());
    }
}
