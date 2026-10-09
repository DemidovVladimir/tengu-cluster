//! Studio request guard (`studio/mod.rs` § Guard): loopback bind, the
//! per-process token, `Host` / `Origin` / `Sec-Fetch-Site` checks, the CSRF
//! proofs every change request (POST) carries, security headers on every
//! response. No CORS, no cookie: a page of another origin can neither read
//! an answer nor send a change request that passes.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use anyhow::{bail, Result};
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Json, Response};
use serde_json::json;

use super::AppState;

/// Header that carries the token on `/api/` requests.
pub(crate) const TOKEN_HEADER: &str = "x-studio-token";
/// Query parameter that carries it where a header cannot (`EventSource`).
pub(crate) const TOKEN_QUERY: &str = "token";

/// The address `--bind` names, if it is loopback (`127.0.0.1`, `::1`,
/// any `127.x`; `localhost` = `127.0.0.1`). Anything else is refused:
/// remote access needs authentication and TLS (plan § 2, deferred).
pub(crate) fn loopback(bind: &str) -> Result<IpAddr> {
    let ip = match bind.trim() {
        "localhost" => IpAddr::V4(Ipv4Addr::LOCALHOST),
        s => match s.trim_start_matches('[').trim_end_matches(']').parse() {
            Ok(ip) => ip,
            Err(_) => bail!("--bind {bind}: not an IP address (use 127.0.0.1 or ::1)"),
        },
    };
    if !ip.is_loopback() {
        bail!(
            "--bind {bind}: refused — tengu studio binds to loopback only (127.0.0.1 or ::1); \
             remote access needs authentication and TLS, not built"
        );
    }
    Ok(ip)
}

/// The per-process secret: 32 random bytes as hex.
#[derive(Clone)]
pub(crate) struct Token(String);

impl Token {
    pub(crate) fn generate() -> Result<Self> {
        let mut b = [0u8; 32];
        getrandom::getrandom(&mut b)
            .map_err(|e| anyhow::anyhow!("studio token: no randomness: {e}"))?;
        Ok(Self(b.iter().map(|x| format!("{x:02x}")).collect()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    /// Constant-time in the length of the token.
    pub(crate) fn matches(&self, given: &str) -> bool {
        let (a, b) = (self.0.as_bytes(), given.as_bytes());
        a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
    }
}

/// What a request must match.
pub(crate) struct Policy {
    pub token: Token,
    /// Accepted `Host` values (`127.0.0.1:<port>`, `localhost:<port>`, …),
    /// lowercase.
    pub hosts: Vec<String>,
}

impl Policy {
    /// For a server listening on `addr`.
    pub(crate) fn new(token: Token, addr: SocketAddr) -> Self {
        let port = addr.port();
        let ip = match addr.ip() {
            IpAddr::V6(v6) => format!("[{v6}]"),
            v4 => v4.to_string(),
        };
        let mut hosts = vec![format!("{ip}:{port}"), format!("localhost:{port}")];
        hosts.dedup();
        Self { token, hosts }
    }

    /// `Ok` = serve; else the status + why. Pure: method, path, query,
    /// headers (module table in `studio/mod.rs` § Guard).
    pub(crate) fn check(
        &self,
        method: &Method,
        path: &str,
        query: Option<&str>,
        headers: &HeaderMap,
    ) -> Result<(), (StatusCode, &'static str)> {
        let text = |h: header::HeaderName| headers.get(h).and_then(|v| v.to_str().ok());
        let host = text(header::HOST).map(str::to_ascii_lowercase);
        if !host.is_some_and(|h| self.hosts.contains(&h)) {
            return Err((
                StatusCode::MISDIRECTED_REQUEST,
                "Host is not this loopback server (DNS rebinding guard)",
            ));
        }
        let origin = headers.get(header::ORIGIN);
        if let Some(origin) = origin {
            let ok = origin
                .to_str()
                .ok()
                .map(str::to_ascii_lowercase)
                .and_then(|o| o.strip_prefix("http://").map(str::to_string))
                .is_some_and(|o| self.hosts.contains(&o));
            if !ok {
                return Err((StatusCode::FORBIDDEN, "cross-origin request refused"));
            }
        }
        if !path.starts_with("/api/") {
            // The page and its assets: GET / HEAD routes only (405 else).
            return Ok(());
        }
        let site = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok());
        let header_token = headers.get(TOKEN_HEADER).and_then(|v| v.to_str().ok());
        if method != Method::GET && method != Method::HEAD {
            // A change request: all three CSRF proofs, the token in the
            // header only (a query string can end up in a log).
            if site != Some("same-origin") {
                return Err((
                    StatusCode::FORBIDDEN,
                    "change request refused: Sec-Fetch-Site must be same-origin",
                ));
            }
            if origin.is_none() {
                return Err((
                    StatusCode::FORBIDDEN,
                    "change request refused: Origin must be this server",
                ));
            }
            if !header_token.is_some_and(|t| self.token.matches(t)) {
                return Err((
                    StatusCode::FORBIDDEN,
                    "change request refused: missing or wrong X-Studio-Token header",
                ));
            }
            return Ok(());
        }
        if matches!(site, Some("cross-site" | "same-site")) {
            return Err((StatusCode::FORBIDDEN, "cross-site request refused"));
        }
        let given = header_token
            .map(str::to_string)
            .or_else(|| query_token(query));
        if !given.is_some_and(|t| self.token.matches(&t)) {
            return Err((
                StatusCode::UNAUTHORIZED,
                "missing or wrong studio token (open the URL `tengu studio` printed)",
            ));
        }
        Ok(())
    }
}

/// `token=<hex>` from a query string (hex needs no decoding).
fn query_token(query: Option<&str>) -> Option<String> {
    query?
        .split('&')
        .find_map(|kv| kv.split_once('=').filter(|(k, _)| *k == TOKEN_QUERY))
        .map(|(_, v)| v.to_string())
}

/// Middleware: the policy, then the security headers on every response.
pub(crate) async fn guard(State(st): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let verdict = st.policy.check(
        req.method(),
        req.uri().path(),
        req.uri().query(),
        req.headers(),
    );
    let mut resp = match verdict {
        Ok(()) => next.run(req).await,
        Err((code, why)) => (code, Json(json!({"error": why}))).into_response(),
    };
    secure(resp.headers_mut());
    resp
}

/// No caching, no framing, no inline script, no referrer, no sniffing.
fn secure(h: &mut HeaderMap) {
    for (k, v) in [
        (header::CACHE_CONTROL, "no-store"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::REFERRER_POLICY, "no-referrer"),
        (header::X_FRAME_OPTIONS, "DENY"),
        (
            header::CONTENT_SECURITY_POLICY,
            "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
             connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
        ),
    ] {
        h.insert(k, HeaderValue::from_static(v));
    }
}
