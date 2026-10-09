//! Sealed blob: one secret, HPKE-sealed to the Worker's public key.
//!
//! Wire form (one line, safe in a header and in git):
//! `tsb1.<b64url meta JSON>.<b64url encapsulated key>.<b64url ciphertext>`.
//! The exact meta JSON bytes are the HPKE AAD, so the upstream, inject
//! mode, allowed clients and `kid` cannot be changed without the Worker
//! refusing the blob. The plaintext is the secret alone.

use hpke::{Deserializable, OpModeR, OpModeS, Serializable};
use rand_core::{CryptoRng, RngCore};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::vault::{Aead, Kdf, Kem, PublicKey, Vault};
use crate::{b64d, b64e, Code, Error};

pub const PREFIX: &str = "tsb1";
const INFO: &[u8] = b"tengu-seal/blob/v1";
/// Longest secret a blob may carry (a URL with a key, a bot token, a JWT).
pub const MAX_SECRET_BYTES: usize = 4096;

/// How the Worker puts the secret into the upstream request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Inject {
    /// `Authorization: Bearer <secret>`.
    Bearer,
    /// `<name>: <secret>`.
    Header { name: String },
    /// `Authorization: Basic base64(<secret>)`; the secret is `user:pass`.
    Basic,
    /// Every `token` in the target path/query is replaced by the secret
    /// (Telegram `bot<token>`, `?api-key=`).
    Placeholder { token: String },
    /// The secret is the whole upstream URL (an RPC URL with a key in it);
    /// its host must equal the upstream host and the request path is empty.
    Url,
}

/// Authenticated, not secret: readable by anyone who has the blob.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedMeta {
    pub v: u32,
    /// Short name tengu refers to (`openrouter`, `telegram`).
    pub name: String,
    /// `https://host[:port][/base]`, no query, no fragment.
    pub upstream: String,
    pub inject: Inject,
    /// OpenSSH fingerprints (`SHA256:…`) allowed to use the blob; empty =
    /// any allow-listed client.
    #[serde(default)]
    pub clients: Vec<String>,
    /// Which Worker key the blob is sealed to.
    pub kid: String,
    pub created_ms: u64,
}

impl SealedMeta {
    /// `name` charset and the upstream / inject shape, checked at seal time
    /// and again by the Worker at open time.
    pub fn validate(&self) -> Result<(), Error> {
        if self.v != 1 {
            return Err(Error::new(Code::BadRequest, "unsupported blob version"));
        }
        if self.name.is_empty()
            || self.name.len() > 64
            || !self
                .name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(Error::new(
                Code::BadRequest,
                "blob name must be 1-64 chars of [A-Za-z0-9_-]",
            ));
        }
        crate::target::parse_upstream(&self.upstream)?;
        match &self.inject {
            Inject::Header { name } => crate::target::check_header_name(name)?,
            Inject::Placeholder { token } => {
                if token.len() < 6
                    || !token
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                {
                    return Err(Error::new(
                        Code::BadRequest,
                        "placeholder token must be 6+ chars of [A-Za-z0-9_-]",
                    ));
                }
            }
            Inject::Bearer | Inject::Basic | Inject::Url => {}
        }
        Ok(())
    }

    pub fn allows_client(&self, fingerprint: &str) -> bool {
        self.clients.is_empty() || self.clients.iter().any(|c| c == fingerprint)
    }
}

/// A parsed blob; `meta` is trusted only after [`open`] succeeds.
#[derive(Debug, Clone)]
pub struct Blob {
    pub meta: SealedMeta,
    meta_bytes: Vec<u8>,
    enc: Vec<u8>,
    ct: Vec<u8>,
}

impl Blob {
    pub fn parse(s: &str) -> Result<Self, Error> {
        let bad = || Error::new(Code::BadRequest, "malformed sealed blob");
        let mut parts = s.trim().split('.');
        if parts.next() != Some(PREFIX) {
            return Err(bad());
        }
        let meta_bytes = b64d(parts.next().ok_or_else(bad)?)?;
        let enc = b64d(parts.next().ok_or_else(bad)?)?;
        let ct = b64d(parts.next().ok_or_else(bad)?)?;
        if parts.next().is_some() {
            return Err(bad());
        }
        let meta: SealedMeta = serde_json::from_slice(&meta_bytes).map_err(|_| bad())?;
        Ok(Self {
            meta,
            meta_bytes,
            enc,
            ct,
        })
    }

    pub fn encode(&self) -> String {
        format!(
            "{PREFIX}.{}.{}.{}",
            b64e(&self.meta_bytes),
            b64e(&self.enc),
            b64e(&self.ct)
        )
    }
}

/// Seal `secret` to the Worker's public key (tengu side).
pub fn seal<R: CryptoRng + RngCore>(
    pk: &PublicKey,
    mut meta: SealedMeta,
    secret: &[u8],
    rng: &mut R,
) -> Result<Blob, Error> {
    if secret.is_empty() || secret.len() > MAX_SECRET_BYTES {
        return Err(Error::new(
            Code::BadRequest,
            format!("secret must be 1-{MAX_SECRET_BYTES} bytes"),
        ));
    }
    meta.kid = pk.kid();
    meta.validate()?;
    if matches!(meta.inject, Inject::Url) {
        let s = std::str::from_utf8(secret)
            .map_err(|_| Error::new(Code::BadRequest, "url secret must be UTF-8"))?;
        crate::target::check_url_secret(&meta.upstream, s)?;
    }
    let meta_bytes = serde_json::to_vec(&meta)
        .map_err(|_| Error::new(Code::BadRequest, "meta serialization failed"))?;
    let (enc, ct) = hpke::single_shot_seal::<Aead, Kdf, Kem, R>(
        &OpModeS::Base,
        &pk.0,
        INFO,
        secret,
        &meta_bytes,
        rng,
    )
    .map_err(|_| Error::new(Code::BadRequest, "HPKE seal failed"))?;
    Ok(Blob {
        meta,
        meta_bytes,
        enc: enc.to_bytes().to_vec(),
        ct,
    })
}

/// Open a blob with the vault (Worker side). Checks `kid` and the meta.
pub fn open(vault: &Vault, blob: &Blob) -> Result<Zeroizing<Vec<u8>>, Error> {
    if blob.meta.kid != vault.public_key().kid() {
        return Err(Error::new(
            Code::Forbidden,
            "blob is sealed to another Worker key (kid mismatch) — reseal it",
        ));
    }
    blob.meta.validate()?;
    let enc = <Kem as hpke::Kem>::EncappedKey::from_bytes(&blob.enc)
        .map_err(|_| Error::new(Code::BadRequest, "malformed encapsulated key"))?;
    let pt = hpke::single_shot_open::<Aead, Kdf, Kem>(
        &OpModeR::Base,
        vault.private(),
        &enc,
        INFO,
        &blob.ct,
        &blob.meta_bytes,
    )
    .map_err(|_| Error::new(Code::Forbidden, "sealed blob failed to open (tampered?)"))?;
    Ok(Zeroizing::new(pt))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand_core::OsRng;

    pub(crate) fn meta(name: &str, upstream: &str, inject: Inject) -> SealedMeta {
        SealedMeta {
            v: 1,
            name: name.into(),
            upstream: upstream.into(),
            inject,
            clients: vec![],
            kid: String::new(),
            created_ms: 1,
        }
    }

    #[test]
    fn seal_open_round_trip() {
        let vault = Vault::from_seed(&[9u8; 32]);
        let m = meta("openrouter", "https://openrouter.ai/api", Inject::Bearer);
        let blob = seal(&vault.public_key(), m, b"sk-test-value", &mut OsRng).unwrap();
        let wire = blob.encode();
        assert!(wire.starts_with("tsb1."));
        let parsed = Blob::parse(&wire).unwrap();
        assert_eq!(parsed.meta.kid, vault.public_key().kid());
        assert_eq!(open(&vault, &parsed).unwrap().as_slice(), b"sk-test-value");
    }

    #[test]
    fn tampered_meta_fails_to_open() {
        let vault = Vault::from_seed(&[9u8; 32]);
        let m = meta("openrouter", "https://openrouter.ai/api", Inject::Bearer);
        let blob = seal(&vault.public_key(), m, b"k", &mut OsRng).unwrap();
        // Re-point the upstream: same ciphertext, new meta bytes.
        let mut evil = blob.meta.clone();
        evil.upstream = "https://evil.example".into();
        let forged = format!(
            "tsb1.{}.{}.{}",
            b64e(&serde_json::to_vec(&evil).unwrap()),
            b64e(&blob.enc),
            b64e(&blob.ct)
        );
        let err = open(&vault, &Blob::parse(&forged).unwrap()).unwrap_err();
        assert_eq!(err.code, Code::Forbidden);
    }

    #[test]
    fn other_vault_cannot_open() {
        let a = Vault::from_seed(&[1u8; 32]);
        let b = Vault::from_seed(&[2u8; 32]);
        let m = meta("x", "https://api.example.com", Inject::Bearer);
        let blob = seal(&a.public_key(), m, b"k", &mut OsRng).unwrap();
        assert_eq!(open(&b, &blob).unwrap_err().code, Code::Forbidden);
    }

    #[test]
    fn rejects_bad_names_upstreams_and_secrets() {
        let v = Vault::from_seed(&[1u8; 32]);
        let pk = v.public_key();
        for (name, up) in [
            ("bad name", "https://a.example"),
            ("ok", "http://a.example"),
            ("ok", "https://a.example/?q=1"),
            ("ok", "https://user@a.example"),
        ] {
            assert!(
                seal(&pk, meta(name, up, Inject::Bearer), b"k", &mut OsRng).is_err(),
                "{name} {up}"
            );
        }
        assert!(seal(
            &pk,
            meta("ok", "https://a.example", Inject::Bearer),
            b"",
            &mut OsRng
        )
        .is_err());
        let url_meta = meta("rpc", "https://rpc.example", Inject::Url);
        assert!(seal(
            &pk,
            url_meta.clone(),
            b"https://other.example/?k=1",
            &mut OsRng
        )
        .is_err());
        assert!(seal(&pk, url_meta, b"https://rpc.example/?api-key=1", &mut OsRng).is_ok());
    }

    #[test]
    fn client_allow_list() {
        let mut m = meta("x", "https://a.example", Inject::Bearer);
        assert!(m.allows_client("SHA256:any"));
        m.clients = vec!["SHA256:abc".into()];
        assert!(m.allows_client("SHA256:abc"));
        assert!(!m.allows_client("SHA256:other"));
    }

    #[test]
    fn parse_rejects_garbage() {
        for s in [
            "",
            "tsb1",
            "tsb2.a.b.c",
            "tsb1.a.b",
            "tsb1.a.b.c.d",
            "tsb1.!!.b.c",
        ] {
            assert!(Blob::parse(s).is_err(), "{s}");
        }
    }
}
