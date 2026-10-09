//! Seal-proxy formats shared by tengu (signs, calls) and the `tengu-seal`
//! Cloudflare Worker (checks, injects, forwards, masks).
//!
//! The harness never holds a provider key: keys are Worker secrets, set by
//! the operator in Cloudflare. A request names a route; the Worker adds that
//! route's key and forwards.
//!
//! | Piece | Lives | Module |
//! |---|---|---|
//! | Route table `name → {upstream, secret, header?}` | Worker `ROUTES` var (`wrangler.toml`, no secrets) | [`route`] |
//! | Client list `SHA256:<fp> → {label, session_hours, routes}` | Worker `CLIENTS` var (fingerprints are public) | [`route`] |
//! | Client identity: an SSH key held by an agent (Secure Enclave via Secretive, Touch ID) | the operator's Mac | [`ssh`] |
//! | Session token `tss1.<claims>.<hmac>`, key = Worker secret `SESSION_KEY` | tengu RAM only, 24 h default | [`session`] |
//! | Upstream request (target URL + injected header) and reply masking | built in the Worker | [`target`] |
//!
//! Everything here is pure Rust (no I/O, no clock, no RNG), so it builds
//! natively and for `wasm32-unknown-unknown`.

pub mod route;
pub mod session;
pub mod ssh;
pub mod target;

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
    /// The Worker itself is misconfigured (500): a route names a missing
    /// secret, `ROUTES` / `CLIENTS` do not parse, `SESSION_KEY` is unset.
    Misconfigured,
}

impl Code {
    pub fn http_status(self) -> u16 {
        match self {
            Code::BadRequest => 400,
            Code::Unauthorized => 401,
            Code::Forbidden => 403,
            Code::BadGateway => 502,
            Code::Misconfigured => 500,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Code::BadRequest => "bad_request",
            Code::Unauthorized => "unauthorized",
            Code::Forbidden => "forbidden",
            Code::BadGateway => "bad_gateway",
            Code::Misconfigured => "misconfigured",
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
