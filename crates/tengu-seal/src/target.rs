//! Build the upstream request for `/fwd/<rest>`: target URL + injected
//! header, and refuse anything that would leave the blob's upstream.
//!
//! The upstream comes from the opened blob (authenticated), never from the
//! caller. The caller only supplies `rest` and the query, and both are
//! checked twice: lexically here, then by comparing the parsed target's
//! scheme / host / port / path prefix with the upstream's.

use base64::Engine;
use url::Url;

use crate::blob::{Inject, SealedMeta};
use crate::{Code, Error};

/// What the Worker sends upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub url: String,
    /// Header to set (lowercase name, value) — `None` for URL-borne secrets.
    pub header: Option<(String, String)>,
}

fn bad(m: &str) -> Error {
    Error::new(Code::BadRequest, m.to_string())
}

fn forbidden(m: &str) -> Error {
    Error::new(Code::Forbidden, m.to_string())
}

/// `https://host[:port][/base]`, no userinfo, query or fragment.
pub fn parse_upstream(upstream: &str) -> Result<Url, Error> {
    let u = Url::parse(upstream).map_err(|_| bad("upstream is not a URL"))?;
    if u.scheme() != "https" {
        return Err(bad("upstream must be https"));
    }
    if u.host_str().is_none() || !u.username().is_empty() || u.password().is_some() {
        return Err(bad("upstream needs a host and no userinfo"));
    }
    if u.query().is_some() || u.fragment().is_some() || upstream.contains('?') {
        return Err(bad("upstream must not carry a query or fragment"));
    }
    Ok(u)
}

/// Headers the caller may never set through `inject = header`.
const RESERVED_HEADERS: &[&str] = &[
    "host",
    "cookie",
    "connection",
    "keep-alive",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "content-length",
    "proxy-authorization",
    "proxy-connection",
];

pub fn check_header_name(name: &str) -> Result<(), Error> {
    let lower = name.to_ascii_lowercase();
    let token_ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !token_ok
        || RESERVED_HEADERS.contains(&lower.as_str())
        || lower.starts_with("cf-")
        || lower.starts_with("tengu-")
    {
        return Err(bad("inject header name is not allowed"));
    }
    Ok(())
}

/// A URL secret must be https and point at the upstream's host and port.
pub fn check_url_secret(upstream: &str, secret: &str) -> Result<Url, Error> {
    let up = parse_upstream(upstream)?;
    let u = Url::parse(secret).map_err(|_| bad("url secret is not a URL"))?;
    if u.scheme() != "https"
        || u.host_str() != up.host_str()
        || u.port_or_known_default() != up.port_or_known_default()
        || u.fragment().is_some()
    {
        return Err(forbidden("url secret must be https on the upstream's host"));
    }
    Ok(u)
}

/// Lexical checks on the caller's path remainder and query.
fn check_rest(rest: &str, query: Option<&str>) -> Result<(), Error> {
    let lower = rest.to_ascii_lowercase();
    if rest.starts_with('/')
        || rest.contains('\\')
        || rest.contains("//")
        || rest.contains("://")
        || rest.chars().any(|c| c.is_control() || c == '#' || c == ' ')
        || lower.contains("%2f")
        || lower.contains("%5c")
        || lower.contains("%00")
    {
        return Err(forbidden("path is not allowed"));
    }
    for seg in rest.split('/') {
        let s = seg.to_ascii_lowercase().replace("%2e", ".");
        if s == "." || s == ".." {
            return Err(forbidden("path is not allowed"));
        }
    }
    if let Some(q) = query {
        if q.chars().any(|c| c.is_control() || c == '#') {
            return Err(forbidden("query is not allowed"));
        }
    }
    Ok(())
}

/// Same scheme, host, port, and a path inside the upstream's base path.
fn assert_inside(up: &Url, target: &str) -> Result<(), Error> {
    let t = Url::parse(target).map_err(|_| forbidden("target is not a URL"))?;
    let base = up.path().trim_end_matches('/');
    let inside = t.path() == base || t.path().starts_with(&format!("{base}/"));
    if t.scheme() != "https"
        || t.host_str() != up.host_str()
        || t.port_or_known_default() != up.port_or_known_default()
        || !t.username().is_empty()
        || t.password().is_some()
        || !inside
    {
        return Err(forbidden("target leaves the sealed upstream"));
    }
    Ok(())
}

/// Build the upstream request for `meta` + `secret` + the caller's `rest`.
pub fn build(
    meta: &SealedMeta,
    secret: &str,
    rest: &str,
    query: Option<&str>,
) -> Result<Target, Error> {
    let up = parse_upstream(&meta.upstream)?;
    check_rest(rest, query)?;
    if secret.chars().any(|c| c.is_control()) {
        return Err(forbidden("secret contains control characters"));
    }
    let joined = || {
        let base = meta.upstream.trim_end_matches('/');
        let mut s = if rest.is_empty() {
            base.to_string()
        } else {
            format!("{base}/{rest}")
        };
        if let Some(q) = query.filter(|q| !q.is_empty()) {
            s.push('?');
            s.push_str(q);
        }
        s
    };
    let target = match &meta.inject {
        Inject::Url => {
            if !rest.is_empty() || query.is_some_and(|q| !q.is_empty()) {
                return Err(forbidden(
                    "a url secret is the whole target — send no path or query",
                ));
            }
            let u = check_url_secret(&meta.upstream, secret)?;
            Target {
                url: u.to_string(),
                header: None,
            }
        }
        Inject::Placeholder { token } => {
            if secret
                .chars()
                .any(|c| matches!(c, '/' | '?' | '#' | '@' | '\\' | ' ' | '%'))
            {
                return Err(forbidden(
                    "placeholder secret has URL-structural characters",
                ));
            }
            let raw = joined();
            if !raw.contains(token.as_str()) {
                return Err(bad(
                    "placeholder token not found in the request path or query",
                ));
            }
            let url = raw.replace(token.as_str(), secret);
            assert_inside(&up, &url)?;
            Target { url, header: None }
        }
        Inject::Bearer => Target {
            url: joined(),
            header: Some(("authorization".into(), format!("Bearer {secret}"))),
        },
        Inject::Header { name } => {
            check_header_name(name)?;
            Target {
                url: joined(),
                header: Some((name.to_ascii_lowercase(), secret.to_string())),
            }
        }
        Inject::Basic => {
            if !secret.contains(':') {
                return Err(bad("basic secret must be 'user:pass'"));
            }
            Target {
                url: joined(),
                header: Some((
                    "authorization".into(),
                    format!(
                        "Basic {}",
                        base64::engine::general_purpose::STANDARD.encode(secret)
                    ),
                )),
            }
        }
    };
    if !matches!(meta.inject, Inject::Url) {
        assert_inside(&up, &target.url)?;
    }
    Ok(target)
}

/// The Worker's own endpoints; never forwarded.
pub const RESERVED_PATHS: &[&str] = &["/healthz", "/pubkey", "/session", "/whoami"];

/// Split a forward path into (blob carried in the path, rest).
///
/// | Path | Blob from | Rest |
/// |---|---|---|
/// | `/fwd/<rest>` | `Tengu-Sealed` header | `<rest>` |
/// | `/s/<blob>/<rest>` | the path (so env-configured base URLs need no header) | `<rest>` |
/// | any other path, `header_blob` set | `Tengu-Sealed` header (clients that build absolute paths, e.g. teloxide's `/bot<token>/…`) | the path without its `/` |
pub fn split_forward_path(path: &str, header_blob: bool) -> Option<(Option<&str>, &str)> {
    if path == "/fwd" {
        return Some((None, ""));
    }
    if let Some(rest) = path.strip_prefix("/fwd/") {
        return Some((None, rest));
    }
    if let Some(after) = path.strip_prefix("/s/") {
        let (blob, rest) = after.split_once('/').unwrap_or((after, ""));
        return (!blob.is_empty()).then_some((Some(blob), rest));
    }
    if header_blob && !RESERVED_PATHS.contains(&path) && path.starts_with('/') {
        return Some((None, &path[1..]));
    }
    None
}

/// Request headers the Worker drops before forwarding (lowercase names).
pub fn drop_request_header(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    RESERVED_HEADERS.contains(&n.as_str())
        || n == "authorization"
        || n.starts_with("cf-")
        || n.starts_with("tengu-")
        || n.starts_with("x-forwarded-")
        || n == "x-real-ip"
        || n == "cdn-loop"
        || n == "forwarded"
        || n == "accept-encoding"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(upstream: &str, inject: Inject) -> SealedMeta {
        SealedMeta {
            v: 1,
            name: "t".into(),
            upstream: upstream.into(),
            inject,
            clients: vec![],
            kid: "k".into(),
            created_ms: 0,
        }
    }

    #[test]
    fn bearer_joins_path_and_query() {
        let m = meta("https://openrouter.ai/api", Inject::Bearer);
        let t = build(&m, "sk-1", "v1/chat/completions", None).unwrap();
        assert_eq!(t.url, "https://openrouter.ai/api/v1/chat/completions");
        assert_eq!(
            t.header,
            Some(("authorization".into(), "Bearer sk-1".into()))
        );
        let t = build(&m, "sk-1", "v1/models", Some("a=1")).unwrap();
        assert_eq!(t.url, "https://openrouter.ai/api/v1/models?a=1");
    }

    #[test]
    fn path_escapes_are_refused() {
        let m = meta("https://openrouter.ai/api", Inject::Bearer);
        for rest in [
            "../x",
            "v1/../../x",
            "%2e%2e/x",
            "v1/%2E%2e/x",
            "/etc",
            "a//b",
            "https://evil.example/x",
            "a\\b",
            "a%2fb",
            "a%5cb",
            "a#b",
            "a\nb",
        ] {
            let e = build(&m, "k", rest, None).unwrap_err();
            assert_eq!(e.code, Code::Forbidden, "{rest}");
        }
    }

    #[test]
    fn telegram_placeholder() {
        let m = meta(
            "https://api.telegram.org",
            Inject::Placeholder {
                token: "TENGU_SECRET".into(),
            },
        );
        let t = build(&m, "123:abc", "botTENGU_SECRET/getMe", None).unwrap();
        assert_eq!(t.url, "https://api.telegram.org/bot123:abc/getMe");
        assert_eq!(t.header, None);
        assert!(build(&m, "123:abc", "bot1/getMe", None).is_err());
        assert!(build(&m, "12/3", "botTENGU_SECRET/getMe", None).is_err());
        assert!(build(&m, "a@b", "botTENGU_SECRET/getMe", None).is_err());
    }

    #[test]
    fn placeholder_in_query() {
        let m = meta(
            "https://mainnet.helius-rpc.com",
            Inject::Placeholder {
                token: "TENGU_SECRET".into(),
            },
        );
        let t = build(&m, "abc-123", "", Some("api-key=TENGU_SECRET")).unwrap();
        assert_eq!(t.url, "https://mainnet.helius-rpc.com?api-key=abc-123");
    }

    #[test]
    fn url_secret_is_the_target() {
        let m = meta("https://rpc.example", Inject::Url);
        let t = build(&m, "https://rpc.example/?api-key=z", "", None).unwrap();
        assert_eq!(t.url, "https://rpc.example/?api-key=z");
        assert!(build(&m, "https://evil.example/?k", "", None).is_err());
        assert!(build(&m, "https://rpc.example/?k", "x", None).is_err());
        assert!(build(&m, "https://rpc.example/?k", "", Some("a=1")).is_err());
        assert!(build(&m, "http://rpc.example/", "", None).is_err());
    }

    #[test]
    fn header_and_basic() {
        let m = meta(
            "https://api.example.com/v2",
            Inject::Header {
                name: "X-Api-Key".into(),
            },
        );
        let t = build(&m, "abc", "items", None).unwrap();
        assert_eq!(t.url, "https://api.example.com/v2/items");
        assert_eq!(t.header, Some(("x-api-key".into(), "abc".into())));
        for name in [
            "Host",
            "Cookie",
            "cf-connecting-ip",
            "Tengu-Sealed",
            "a b",
            "",
        ] {
            assert!(check_header_name(name).is_err(), "{name}");
        }
        let b = meta("https://api.example.com", Inject::Basic);
        let t = build(&b, "user:pass", "", None).unwrap();
        assert_eq!(
            t.header,
            Some(("authorization".into(), "Basic dXNlcjpwYXNz".into()))
        );
        assert!(build(&b, "nocolon", "", None).is_err());
    }

    #[test]
    fn base_path_boundary() {
        let m = meta("https://api.example.com/v2", Inject::Bearer);
        assert!(build(&m, "k", "", None).is_ok());
        // `/v2x` is not inside `/v2`.
        let up = parse_upstream("https://api.example.com/v2").unwrap();
        assert!(assert_inside(&up, "https://api.example.com/v2x").is_err());
        assert!(assert_inside(&up, "https://api.example.com:8443/v2/a").is_err());
        assert!(assert_inside(&up, "https://u@api.example.com/v2/a").is_err());
    }

    #[test]
    fn forward_path_forms() {
        assert_eq!(split_forward_path("/fwd", false), Some((None, "")));
        assert_eq!(split_forward_path("/fwd/v1/x", false), Some((None, "v1/x")));
        assert_eq!(
            split_forward_path("/s/tsb1.a.b.c/v1/x", false),
            Some((Some("tsb1.a.b.c"), "v1/x"))
        );
        assert_eq!(
            split_forward_path("/s/tsb1.a.b.c", false),
            Some((Some("tsb1.a.b.c"), ""))
        );
        assert_eq!(split_forward_path("/s/", false), None);
        assert_eq!(split_forward_path("/pubkey", false), None);
        assert_eq!(split_forward_path("/fwdx", false), None);
        // teloxide: absolute `/bot<token>/<method>` with the blob in a header.
        assert_eq!(
            split_forward_path("/botTENGU_SECRET/getMe", true),
            Some((None, "botTENGU_SECRET/getMe"))
        );
        assert_eq!(split_forward_path("/botX/getMe", false), None);
        assert_eq!(split_forward_path("/pubkey", true), None);
        assert_eq!(split_forward_path("/session", true), None);
    }

    #[test]
    fn request_header_filter() {
        for h in [
            "Authorization",
            "Cookie",
            "Host",
            "CF-Connecting-IP",
            "Tengu-Sealed",
            "X-Forwarded-For",
        ] {
            assert!(drop_request_header(h), "{h}");
        }
        for h in ["content-type", "accept", "http-referer", "x-title"] {
            assert!(!drop_request_header(h), "{h}");
        }
    }
}
