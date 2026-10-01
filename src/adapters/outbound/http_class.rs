//! Shared HTTP error model: one classified error ([`HttpError`]) and the
//! status / transport → `ErrorClass` mapping every outbound HTTP client uses
//! (Solana RPC + JSON fetch, Hyperliquid info, Jev decisions, feeds). Lifted
//! from `solana/rpc.rs`, which re-exports it under its old names (`RpcError`).
//!
//! | Failure | `ErrorClass` |
//! |---|---|
//! | quota text in the body (`max usage reached`, `-32429`) | `QuotaExhausted` |
//! | HTTP 429 | `RateLimited` (`Retry-After` seconds → `retry_after_ms`) |
//! | HTTP 401 / 403 | `AuthRequired` |
//! | HTTP 408, request timeout | `Timeout` |
//! | HTTP 5xx, connect / transport errors | `Transient` |
//! | undecodable body | `Decode` |
//! | other 4xx, egress / scope denial | `Fatal` |
//!
//! Messages never carry a secret: URLs render through [`display_url`] or a
//! [`Scrubber`]; bodies are quoted as ≤ 160-char snippets. Venue overrides
//! (HL `500 null` ⇒ `NotApplicable`) live with the venue client.

use std::fmt;

use reqwest::Url;
use serde_json::Value;

use crate::domain::observation::{ErrorClass, ReadError};

/// Max chars of a response body quoted in an error message.
const BODY_SNIPPET: usize = 160;

/// A classified HTTP / API failure. `message` never contains a
/// secret-bearing URL.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HttpError {
    pub class: ErrorClass,
    pub message: String,
    pub retry_after_ms: Option<u64>,
    /// API / JSON-RPC error code, when the server answered with one.
    pub code: Option<i64>,
    /// HTTP status, when the failure was a non-2xx response.
    pub http_status: Option<u16>,
    /// API error payload (a failed Solana `sendTransaction` preflight puts
    /// its `err` + `logs` here).
    pub data: Option<Value>,
}

impl HttpError {
    pub(crate) fn new(class: ErrorClass, message: impl Into<String>) -> Self {
        Self {
            class,
            message: message.into(),
            retry_after_ms: None,
            code: None,
            http_status: None,
            data: None,
        }
    }

    /// `Transient` and `RateLimited` are worth one more try; nothing else.
    pub(crate) fn retryable(&self) -> bool {
        matches!(self.class, ErrorClass::Transient | ErrorClass::RateLimited)
    }

    pub(crate) fn to_read_error(&self, field: &str) -> ReadError {
        ReadError {
            field: field.to_string(),
            class: self.class,
            message: self.message.clone(),
            retry_after_ms: self.retry_after_ms,
        }
    }

    pub(crate) fn prefixed(mut self, prefix: &str) -> Self {
        self.message = format!("{prefix}: {}", self.message);
        self
    }
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.message, self.class.as_str())
    }
}

impl std::error::Error for HttpError {}

/// `ReadError` for any error of an outbound HTTP client: the classified
/// [`HttpError`] when one is in the chain, else `Fatal`.
pub(crate) fn read_error(field: &str, e: &anyhow::Error) -> ReadError {
    match e.chain().find_map(|c| c.downcast_ref::<HttpError>()) {
        Some(h) => h.to_read_error(field),
        None => ReadError::new(field, ErrorClass::Fatal, format!("{e:#}")),
    }
}

/// Quota-exhaustion wording (Helius / Solana `-32429`, "max usage reached").
pub(crate) fn is_quota_text(s: &str) -> bool {
    let s = s.to_ascii_lowercase();
    s.contains("max usage reached") || s.contains("-32429")
}

/// Class of a non-2xx HTTP response. The body decides only between
/// `QuotaExhausted` and the status class.
pub(crate) fn classify_http_status(status: u16, body: &str) -> ErrorClass {
    if is_quota_text(body) {
        return ErrorClass::QuotaExhausted;
    }
    match status {
        429 => ErrorClass::RateLimited,
        401 | 403 => ErrorClass::AuthRequired,
        408 => ErrorClass::Timeout,
        500..=599 => ErrorClass::Transient,
        _ => ErrorClass::Fatal,
    }
}

/// `Retry-After` in seconds → ms (the HTTP-date form is ignored).
pub(crate) fn retry_after_ms(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(|s| s.saturating_mul(1000))
}

/// Query parameter names whose values are credentials (never rendered).
const SECRET_PARAM_HINTS: &[&str] = &["key", "token", "secret", "auth", "sig", "password"];

fn is_secret_param(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    SECRET_PARAM_HINTS.iter().any(|h| n.contains(h))
}

/// `url` for messages: `scheme://host/path` plus the query with
/// credential-like parameter values replaced by `<redacted>`. For public
/// APIs (Jupiter, datapi) — never for a URL that may embed a key in its path
/// (RPC endpoints render the host only; an operator-set API base renders
/// through [`Scrubber::for_keyed_path`]).
pub(crate) fn display_url(url: &Url) -> String {
    display_url_with_path(url, url.path())
}

/// `url.path()` with every segment of ≥ 8 chars `<redacted>` — the
/// [`Scrubber::for_rpc`] rule for where a provider puts a key
/// (`/<api-key>/info`); `/info` stays.
pub(crate) fn redacted_path(url: &Url) -> String {
    url.path()
        .split('/')
        .map(|seg| if seg.len() >= 8 { "<redacted>" } else { seg })
        .collect::<Vec<_>>()
        .join("/")
}

/// [`display_url`] showing `path` instead of the URL's own.
fn display_url_with_path(url: &Url, path: &str) -> String {
    let mut shown = format!(
        "{}://{}{}",
        url.scheme(),
        url.host_str().unwrap_or(""),
        path
    );
    let pairs: Vec<String> = url
        .query_pairs()
        .map(|(k, v)| {
            if is_secret_param(&k) {
                format!("{k}=<redacted>")
            } else {
                format!("{k}={v}")
            }
        })
        .collect();
    if !pairs.is_empty() {
        shown.push('?');
        shown.push_str(&pairs.join("&"));
    }
    shown
}

/// Rewrites every rendering of a secret-bearing URL in error text
/// (longest needle first, so a full URL goes before its parts).
pub(crate) struct Scrubber {
    rules: Vec<(String, String)>,
}

impl Scrubber {
    /// RPC endpoint: everything but the host may carry a key — the full
    /// URL, its path (`/<api-key>/` tokens), path segments ≥ 8 chars, the
    /// query and every query value ≥ 8 chars become the host.
    pub(crate) fn for_rpc(url: &Url) -> Self {
        let host = url.host_str().unwrap_or("").to_string();
        let mut needles = vec![url.as_str().to_string()];
        let path = url.path();
        if path.len() > 1 {
            needles.push(path.to_string());
            needles.extend(
                path.split('/')
                    .filter(|seg| seg.len() >= 8)
                    .map(str::to_string),
            );
        }
        if let Some(q) = url.query().filter(|q| !q.is_empty()) {
            needles.push(q.to_string());
            needles.extend(
                url.query_pairs()
                    .map(|(_, v)| v.into_owned())
                    .filter(|v| v.len() >= 8),
            );
        }
        Self::from_rules(needles.into_iter().map(|n| (n, host.clone())).collect())
    }

    /// Public API: the full URL renders as [`display_url`]; only values of
    /// credential-like query params are secret (→ `<redacted>`). Ids in the
    /// query (mints, pool pairs) are left intact.
    pub(crate) fn for_api(url: &Url) -> Self {
        let mut rules = vec![(url.as_str().to_string(), display_url(url))];
        rules.extend(Self::query_rules(url));
        Self::from_rules(rules)
    }

    /// An API base an operator sets (`HL_API_URL`: a node provider may key
    /// its path): as [`Scrubber::for_api`], with every path segment of ≥ 8
    /// chars `<redacted>` ([`redacted_path`], the [`Scrubber::for_rpc`]
    /// rule) — in the rendered URL and wherever the path or a segment
    /// appears alone. The public `https://api.hyperliquid.xyz/info` renders
    /// unchanged.
    pub(crate) fn for_keyed_path(url: &Url) -> Self {
        let (path, shown) = (url.path(), redacted_path(url));
        let mut rules = vec![(url.as_str().to_string(), display_url_with_path(url, &shown))];
        if shown != path {
            rules.push((path.to_string(), shown));
            rules.extend(
                path.split('/')
                    .filter(|seg| seg.len() >= 8)
                    .map(|seg| (seg.to_string(), "<redacted>".to_string())),
            );
        }
        rules.extend(Self::query_rules(url));
        Self::from_rules(rules)
    }

    /// Credential-like query values (and the whole query holding one).
    fn query_rules(url: &Url) -> Vec<(String, String)> {
        let mut rules = Vec::new();
        if let Some(q) = url.query().filter(|q| !q.is_empty()) {
            if url.query_pairs().any(|(k, _)| is_secret_param(&k)) {
                rules.push((q.to_string(), "<query redacted>".to_string()));
            }
        }
        rules.extend(
            url.query_pairs()
                .filter(|(k, v)| is_secret_param(k) && !v.is_empty())
                .map(|(_, v)| (v.into_owned(), "<redacted>".to_string())),
        );
        rules
    }

    fn from_rules(mut rules: Vec<(String, String)>) -> Self {
        rules.retain(|(n, _)| !n.is_empty());
        rules.sort_by_key(|(n, _)| std::cmp::Reverse(n.len()));
        rules.dedup_by(|a, b| a.0 == b.0);
        Self { rules }
    }

    pub(crate) fn scrub(&self, text: &str) -> String {
        let mut out = text.to_string();
        for (needle, with) in &self.rules {
            out = out.replace(needle.as_str(), with);
        }
        out
    }
}

/// Classified error for a failed `send()` / body read (no URL in the
/// message): timeout ⇒ `Timeout`, body decode ⇒ `Decode`, else `Transient`.
pub(crate) fn reqwest_error(e: reqwest::Error, host: &str, scrub: &Scrubber) -> HttpError {
    let class = if e.is_timeout() {
        ErrorClass::Timeout
    } else if e.is_decode() {
        ErrorClass::Decode
    } else {
        ErrorClass::Transient
    };
    let chain = format!("{:#}", anyhow::Error::from(e.without_url()));
    HttpError::new(
        class,
        scrub.scrub(&format!("request to {host} failed: {chain}")),
    )
}

/// `body` as a message snippet: trimmed, ≤ 160 chars.
pub(crate) fn body_snippet(body: &str) -> String {
    body.trim().chars().take(BODY_SNIPPET).collect()
}

/// Classified error for a non-2xx response; quotes a short body snippet.
pub(crate) fn http_status_error(
    status: u16,
    retry_after: Option<u64>,
    body: &str,
    host: &str,
    scrub: &Scrubber,
) -> HttpError {
    let mut e = HttpError::new(
        classify_http_status(status, body),
        scrub.scrub(&format!(
            "HTTP {status} from {host}: {}",
            body_snippet(body)
        )),
    );
    e.http_status = Some(status);
    e.retry_after_ms = retry_after;
    e
}

/// Local HTTP server + client for outbound tests (no network).
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use crate::domain::scope::ToolScope;

    pub(crate) struct Canned {
        pub status: u16,
        pub headers: &'static str,
        pub body: String,
        pub delay_ms: u64,
    }

    pub(crate) fn canned(status: u16, body: impl Into<String>) -> Canned {
        Canned {
            status,
            headers: "",
            body: body.into(),
            delay_ms: 0,
        }
    }

    /// Serves `replies` in order, one connection each (`Connection: close`).
    /// Returns the base URL and the raw requests received.
    pub(crate) async fn serve(replies: Vec<Canned>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            for c in replies {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = Vec::new();
                let mut tmp = [0u8; 8192];
                loop {
                    let n = sock.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some(pos) = text.find("\r\n\r\n") {
                        let len = text[..pos]
                            .lines()
                            .find_map(|l| {
                                let (k, v) = l.split_once(':')?;
                                k.eq_ignore_ascii_case("content-length")
                                    .then(|| v.trim().parse::<usize>().ok())?
                            })
                            .unwrap_or(0);
                        if buf.len() >= pos + 4 + len {
                            break;
                        }
                    }
                }
                log.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf).to_string());
                tokio::time::sleep(Duration::from_millis(c.delay_ms)).await;
                let content_type = if c.headers.to_ascii_lowercase().contains("content-type:") {
                    String::new()
                } else {
                    "Content-Type: application/json\r\n".to_string()
                };
                let resp = format!(
                    "HTTP/1.1 {} X\r\n{content_type}Content-Length: {}\r\nConnection: close\r\n{}\r\n{}",
                    c.status,
                    c.body.len(),
                    c.headers,
                    c.body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        (format!("http://{addr}"), seen)
    }

    pub(crate) fn local_scope() -> ToolScope {
        ToolScope {
            net_hosts: vec!["127.0.0.1".into()],
            ..Default::default()
        }
    }

    pub(crate) fn test_client() -> reqwest::Client {
        reqwest::Client::builder().no_proxy().build().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};

    #[test]
    fn classifies_http_statuses() {
        use ErrorClass::*;
        for (status, body, want) in [
            (429, "", RateLimited),
            (
                429,
                r#"{"error":{"code":-32429,"message":"max usage reached"}}"#,
                QuotaExhausted,
            ),
            (401, "", AuthRequired),
            (403, "forbidden", AuthRequired),
            (408, "", Timeout),
            (500, "null", Transient),
            (502, "bad gateway", Transient),
            (422, "Failed to deserialize the JSON body", Fatal),
            (404, "", Fatal),
        ] {
            assert_eq!(classify_http_status(status, body), want, "{status} {body}");
        }
    }

    #[test]
    fn retry_after_seconds_only() {
        let mut h = HeaderMap::new();
        assert_eq!(retry_after_ms(&h), None);
        h.insert(RETRY_AFTER, HeaderValue::from_static(" 3 "));
        assert_eq!(retry_after_ms(&h), Some(3_000));
        h.insert(
            RETRY_AFTER,
            HeaderValue::from_static("Wed, 30 Sep 2026 13:39:41 GMT"),
        );
        assert_eq!(retry_after_ms(&h), None);
    }

    #[test]
    fn status_error_quotes_a_scrubbed_snippet() {
        let url = Url::parse("https://api.example.com/v1?api-key=SECRETKEY9876543").unwrap();
        let scrub = Scrubber::for_api(&url);
        let body = format!("slow down {} {}", "SECRETKEY9876543", "x".repeat(400));
        let e = http_status_error(429, Some(2_000), &body, "api.example.com", &scrub);
        assert_eq!(e.class, ErrorClass::RateLimited);
        assert_eq!((e.http_status, e.retry_after_ms), (Some(429), Some(2_000)));
        assert!(!e.message.contains("SECRETKEY"), "{}", e.message);
        assert!(e
            .message
            .starts_with("HTTP 429 from api.example.com: slow down"));
        assert!(e.message.chars().count() < 220, "{}", e.message);
        let any = anyhow::Error::new(e).context("reading");
        let r = read_error("book", &any);
        assert_eq!(
            (r.class, r.retry_after_ms, r.field.as_str()),
            (ErrorClass::RateLimited, Some(2_000), "book")
        );
        assert_eq!(
            read_error("book", &anyhow::anyhow!("boom")).class,
            ErrorClass::Fatal
        );
    }
}
