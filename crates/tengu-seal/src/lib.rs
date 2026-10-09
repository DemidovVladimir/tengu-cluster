//! Sealed-secret formats shared by tengu (seals, signs, calls) and the
//! `tengu-seal` Cloudflare Worker (opens, checks, injects, forwards).
//!
//! The harness never holds a provider key in plaintext:
//!
//! | Piece | Lives | Module |
//! |---|---|---|
//! | Vault seed (32 bytes) → X25519 key pair + session key | Worker's `KeyVault` Durable Object, never exported | [`vault`] |
//! | Sealed blob `tsb1.<meta>.<enc>.<ct>`: HPKE ciphertext of one secret, metadata as AAD | tengu (`<TENGU_HOME>/sealed/*.sealed`, safe in git) | [`blob`] |
//! | Session token `tss1.<nonce>.<ct>`: claims sealed with the session key | tengu RAM only, 24 h | [`session`] |
//! | Client identity: an SSH key held by an agent (Secure Enclave via Secretive, Touch ID) | the operator's Mac | [`ssh`] |
//! | Upstream request: target URL + injected header, bound to the blob's upstream | built in the Worker | [`target`] |
//!
//! Everything here is pure Rust (no I/O, no clock, no RNG except the
//! caller-supplied one), so it builds natively and for
//! `wasm32-unknown-unknown`.

pub mod blob;
pub mod session;
pub mod ssh;
pub mod target;
pub mod vault;

use std::fmt;

/// Base64url without padding — every wire field in this crate.
pub(crate) fn b64e(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub(crate) fn b64d(s: &str) -> Result<Vec<u8>, Error> {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s.trim())
        .map_err(|_| Error::new(Code::BadRequest, "invalid base64url"))
}

/// Error class; the Worker maps it to an HTTP status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    /// Malformed input (400).
    BadRequest,
    /// Missing, forged or expired credential (401).
    Unauthorized,
    /// Valid credential, action not allowed (403).
    Forbidden,
    /// The upstream could not be reached (502); never carries its URL,
    /// which may hold the secret.
    BadGateway,
}

impl Code {
    pub fn http_status(self) -> u16 {
        match self {
            Code::BadRequest => 400,
            Code::Unauthorized => 401,
            Code::Forbidden => 403,
            Code::BadGateway => 502,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Code::BadRequest => "bad_request",
            Code::Unauthorized => "unauthorized",
            Code::Forbidden => "forbidden",
            Code::BadGateway => "bad_gateway",
        }
    }
}

/// An error whose message never contains secret material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub code: Code,
    pub message: String,
}

impl Error {
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for Error {}

/// The OS RNG (`getrandom`), for [`blob::seal`] on the tengu side.
pub use rand_core::OsRng;
