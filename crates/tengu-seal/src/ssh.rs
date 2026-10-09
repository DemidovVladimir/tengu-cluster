//! Client identity = an SSH public key whose private half stays in an
//! agent (Secretive → Secure Enclave, Touch ID; any ssh-agent for tests).
//!
//! Supported key types: `ecdsa-sha2-nistp256` (Secure Enclave) and
//! `ssh-ed25519`. Fingerprints use the OpenSSH form `SHA256:<base64, no pad>`
//! (what `ssh-add -l` prints), shown and stored in full.

use base64::Engine;
use sha2::{Digest, Sha256};

use crate::{b64d, Code, Error};

const ECDSA_P256: &str = "ecdsa-sha2-nistp256";
const ED25519: &str = "ssh-ed25519";

/// Domain separator of the `/session` signature.
pub const SESSION_DOMAIN: &str = "tengu-seal/session/v1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyKind {
    EcdsaP256,
    Ed25519,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicKey {
    pub kind: KeyKind,
    /// SSH wire-format key blob (what the agent lists and fingerprints hash).
    pub blob: Vec<u8>,
    /// SEC1 point (P-256, 65 bytes) or raw key (Ed25519, 32 bytes).
    key: Vec<u8>,
}

impl PublicKey {
    /// Parse an `authorized_keys` line: `<type> <base64> [comment]`.
    pub fn from_openssh(line: &str) -> Result<Self, Error> {
        let mut it = line.split_whitespace();
        let (ty, b64) = match (it.next(), it.next()) {
            (Some(t), Some(b)) => (t, b),
            _ => return Err(bad("expected '<type> <base64> [comment]'")),
        };
        let blob = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|_| bad("public key is not base64"))?;
        let key = Self::from_blob(&blob)?;
        if key.type_name() != ty {
            return Err(bad("key type does not match its blob"));
        }
        Ok(key)
    }

    /// Parse the SSH wire-format blob (as returned by an agent).
    pub fn from_blob(blob: &[u8]) -> Result<Self, Error> {
        let mut r = Reader(blob);
        let ty = r.string()?;
        let (kind, key) = match ty {
            t if t == ECDSA_P256.as_bytes() => {
                if r.string()? != b"nistp256" {
                    return Err(bad("unsupported ECDSA curve"));
                }
                let q = r.string()?.to_vec();
                p256::ecdsa::VerifyingKey::from_sec1_bytes(&q)
                    .map_err(|_| bad("invalid P-256 point"))?;
                (KeyKind::EcdsaP256, q)
            }
            t if t == ED25519.as_bytes() => {
                let k = r.string()?.to_vec();
                if k.len() != 32 {
                    return Err(bad("invalid Ed25519 key length"));
                }
                (KeyKind::Ed25519, k)
            }
            _ => {
                return Err(bad(
                    "unsupported key type (use ecdsa-sha2-nistp256 or ssh-ed25519)",
                ))
            }
        };
        r.end()?;
        Ok(Self {
            kind,
            blob: blob.to_vec(),
            key,
        })
    }

    pub fn type_name(&self) -> &'static str {
        match self.kind {
            KeyKind::EcdsaP256 => ECDSA_P256,
            KeyKind::Ed25519 => ED25519,
        }
    }

    /// `SHA256:<base64 no pad>`, as `ssh-keygen -lf` prints it.
    pub fn fingerprint(&self) -> String {
        let d = Sha256::digest(&self.blob);
        format!(
            "SHA256:{}",
            base64::engine::general_purpose::STANDARD_NO_PAD.encode(d)
        )
    }

    pub fn to_openssh(&self) -> String {
        format!(
            "{} {}",
            self.type_name(),
            base64::engine::general_purpose::STANDARD.encode(&self.blob)
        )
    }

    /// Verify an SSH signature blob (`string alg, string sig`) over `msg`.
    pub fn verify(&self, msg: &[u8], sig_blob: &[u8]) -> Result<(), Error> {
        let unauth = |m: &str| Error::new(Code::Unauthorized, m.to_string());
        let mut r = Reader(sig_blob);
        let alg = r.string().map_err(|_| unauth("malformed signature"))?;
        let sig = r.string().map_err(|_| unauth("malformed signature"))?;
        r.end().map_err(|_| unauth("malformed signature"))?;
        if alg != self.type_name().as_bytes() {
            return Err(unauth("signature algorithm does not match the key"));
        }
        match self.kind {
            KeyKind::Ed25519 => {
                use ed25519_dalek::Verifier;
                let vk = ed25519_dalek::VerifyingKey::from_bytes(
                    self.key.as_slice().try_into().expect("checked length"),
                )
                .map_err(|_| unauth("invalid Ed25519 key"))?;
                let s = ed25519_dalek::Signature::from_slice(sig)
                    .map_err(|_| unauth("malformed Ed25519 signature"))?;
                vk.verify(msg, &s).map_err(|_| unauth("bad signature"))
            }
            KeyKind::EcdsaP256 => {
                use p256::ecdsa::signature::Verifier;
                let mut sr = Reader(sig);
                let r_int = scalar32(sr.string().map_err(|_| unauth("malformed ECDSA r"))?)
                    .ok_or_else(|| unauth("malformed ECDSA r"))?;
                let s_int = scalar32(sr.string().map_err(|_| unauth("malformed ECDSA s"))?)
                    .ok_or_else(|| unauth("malformed ECDSA s"))?;
                sr.end().map_err(|_| unauth("malformed ECDSA signature"))?;
                let s = p256::ecdsa::Signature::from_scalars(r_int, s_int)
                    .map_err(|_| unauth("malformed ECDSA signature"))?;
                let vk = p256::ecdsa::VerifyingKey::from_sec1_bytes(&self.key)
                    .map_err(|_| unauth("invalid P-256 key"))?;
                // SSH ECDSA-P256 signs SHA-256(msg); `verify` hashes with SHA-256.
                vk.verify(msg, &s).map_err(|_| unauth("bad signature"))
            }
        }
    }
}

/// The exact bytes a client signs to get a session.
///
/// `aud` is the Worker origin the client called (`https://host`), so a
/// signature cannot be replayed against another deployment; `ts_ms` is
/// checked within ±60 s by the Worker.
pub fn session_message(fp: &str, ts_ms: u64, nonce_b64: &str, aud: &str) -> Vec<u8> {
    format!("{SESSION_DOMAIN}\nfp: {fp}\nts_ms: {ts_ms}\nnonce: {nonce_b64}\naud: {aud}\n")
        .into_bytes()
}

/// `POST /session` body.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionRequest {
    /// `authorized_keys` line of the client key.
    pub pubkey: String,
    pub ts_ms: u64,
    /// 16 random bytes, base64url.
    pub nonce: String,
    /// SSH signature blob over [`session_message`], base64url.
    pub sig: String,
}

impl SessionRequest {
    /// Check the signature and the clock window; returns the key.
    pub fn verify(&self, aud: &str, now_ms: u64) -> Result<PublicKey, Error> {
        let key = PublicKey::from_openssh(&self.pubkey)?;
        if now_ms.abs_diff(self.ts_ms) > 60_000 {
            return Err(Error::new(
                Code::Unauthorized,
                "session request timestamp is more than 60 s off — check the clock",
            ));
        }
        if b64d(&self.nonce)?.len() != 16 {
            return Err(bad("nonce must be 16 bytes"));
        }
        let msg = session_message(&key.fingerprint(), self.ts_ms, &self.nonce, aud);
        key.verify(&msg, &b64d(&self.sig)?)?;
        Ok(key)
    }
}

fn bad(m: &str) -> Error {
    Error::new(Code::BadRequest, m.to_string())
}

/// SSH `mpint` → 32-byte big-endian scalar (strip the sign byte, left-pad).
fn scalar32(mpint: &[u8]) -> Option<[u8; 32]> {
    let trimmed = match mpint {
        [0, rest @ ..] => rest,
        all => all,
    };
    if trimmed.len() > 32 {
        return None;
    }
    let mut out = [0u8; 32];
    out[32 - trimmed.len()..].copy_from_slice(trimmed);
    Some(out)
}

/// SSH wire reader: `uint32 len || bytes`.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn string(&mut self) -> Result<&'a [u8], Error> {
        if self.0.len() < 4 {
            return Err(bad("truncated SSH field"));
        }
        let n = u32::from_be_bytes([self.0[0], self.0[1], self.0[2], self.0[3]]) as usize;
        let rest = &self.0[4..];
        if rest.len() < n {
            return Err(bad("truncated SSH field"));
        }
        let (s, tail) = rest.split_at(n);
        self.0 = tail;
        Ok(s)
    }

    fn end(&self) -> Result<(), Error> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(bad("trailing bytes in SSH field"))
        }
    }
}

/// SSH wire writer, for tests and for the tengu agent client.
pub fn put_string(out: &mut Vec<u8>, s: &[u8]) {
    out.extend_from_slice(&(s.len() as u32).to_be_bytes());
    out.extend_from_slice(s);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::b64e;

    fn ed25519_pair() -> (ed25519_dalek::SigningKey, PublicKey) {
        let sk = ed25519_dalek::SigningKey::from_bytes(&[11u8; 32]);
        let mut blob = Vec::new();
        put_string(&mut blob, ED25519.as_bytes());
        put_string(&mut blob, sk.verifying_key().as_bytes());
        (sk, PublicKey::from_blob(&blob).unwrap())
    }

    fn p256_pair() -> (p256::ecdsa::SigningKey, PublicKey) {
        let sk = p256::ecdsa::SigningKey::from_bytes(&[12u8; 32].into()).unwrap();
        let q = sk.verifying_key().to_encoded_point(false);
        let mut blob = Vec::new();
        put_string(&mut blob, ECDSA_P256.as_bytes());
        put_string(&mut blob, b"nistp256");
        put_string(&mut blob, q.as_bytes());
        (sk, PublicKey::from_blob(&blob).unwrap())
    }

    fn ssh_sig_ed25519(sk: &ed25519_dalek::SigningKey, msg: &[u8]) -> Vec<u8> {
        use ed25519_dalek::Signer;
        let mut out = Vec::new();
        put_string(&mut out, ED25519.as_bytes());
        put_string(&mut out, &sk.sign(msg).to_bytes());
        out
    }

    fn ssh_sig_p256(sk: &p256::ecdsa::SigningKey, msg: &[u8]) -> Vec<u8> {
        use p256::ecdsa::signature::Signer;
        let sig: p256::ecdsa::Signature = sk.sign(msg);
        let (r, s) = (sig.r().to_bytes(), sig.s().to_bytes());
        let mpint = |b: &[u8]| {
            let t: Vec<u8> = b.iter().copied().skip_while(|x| *x == 0).collect();
            if t.first().is_some_and(|x| x & 0x80 != 0) {
                [vec![0], t].concat()
            } else {
                t
            }
        };
        let mut inner = Vec::new();
        put_string(&mut inner, &mpint(&r));
        put_string(&mut inner, &mpint(&s));
        let mut out = Vec::new();
        put_string(&mut out, ECDSA_P256.as_bytes());
        put_string(&mut out, &inner);
        out
    }

    #[test]
    fn openssh_line_round_trips_and_fingerprints() {
        let (_, pk) = p256_pair();
        let line = format!("{} my-mac", pk.to_openssh());
        let parsed = PublicKey::from_openssh(&line).unwrap();
        assert_eq!(parsed, pk);
        assert!(pk.fingerprint().starts_with("SHA256:"));
        assert_eq!(pk.fingerprint().len(), "SHA256:".len() + 43);
        assert!(PublicKey::from_openssh("ssh-rsa AAAAB3Nza").is_err());
        let ed_line = ed25519_pair().1.to_openssh();
        let mismatched = ed_line.replacen(ED25519, ECDSA_P256, 1);
        assert!(PublicKey::from_openssh(&mismatched).is_err());
    }

    #[test]
    fn verifies_both_key_kinds_and_rejects_wrong_message() {
        let (esk, epk) = ed25519_pair();
        let (psk, ppk) = p256_pair();
        let msg = b"hello";
        epk.verify(msg, &ssh_sig_ed25519(&esk, msg)).unwrap();
        ppk.verify(msg, &ssh_sig_p256(&psk, msg)).unwrap();
        assert!(epk.verify(b"other", &ssh_sig_ed25519(&esk, msg)).is_err());
        assert!(ppk.verify(b"other", &ssh_sig_p256(&psk, msg)).is_err());
        // An Ed25519 signature presented for the P-256 key.
        assert!(ppk.verify(msg, &ssh_sig_ed25519(&esk, msg)).is_err());
    }

    #[test]
    fn session_request_checks_signature_clock_and_aud() {
        let (sk, pk) = p256_pair();
        let nonce = b64e(&[3u8; 16]);
        let aud = "https://seal.example.workers.dev";
        let msg = session_message(&pk.fingerprint(), 1_000_000, &nonce, aud);
        let req = SessionRequest {
            pubkey: pk.to_openssh(),
            ts_ms: 1_000_000,
            nonce: nonce.clone(),
            sig: b64e(&ssh_sig_p256(&sk, &msg)),
        };
        assert_eq!(req.verify(aud, 1_030_000).unwrap(), pk);
        assert!(req.verify(aud, 1_061_000).is_err());
        assert!(req.verify("https://other.example", 1_000_000).is_err());
        let mut bad_nonce = req.clone();
        bad_nonce.nonce = b64e(&[3u8; 8]);
        assert!(bad_nonce.verify(aud, 1_000_000).is_err());
    }
}
