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
//!
//! [`GatedHttp`] is the one request pipeline of the outbound API clients
//! (HL info, SEC EDGAR, GeckoTerminal, TED, Solana RPC, `fetch_json` /
//! `post_json`, the Privy signer):
//!
//! | Step | Rule |
//! |---|---|
//! | Gate | `egress::policy().check_url` + `scope.check_net_host` on the request's own URL; a denial is `Fatal` (scrubbed), nothing is sent |
//! | Budget | optional [`Budget`]: the weight is acquired before sending (`Priority::Read`, ≤ `max_wait`, else `RateLimited`), the client's extra weight charged after the reply, a 429 drains the bucket (`Limiters::penalize`); `config = None` = unlimited |
//! | Seal session | a request to the seal proxy without an `Authorization` header gets `Bearer <session>` (`keys::auth_header_for`; other hosts never see it) |
//! | Reply | status, `Retry-After`, headers, body ([`RawReply`]) go to the client's decoder; a transport / read error is [`reqwest_error`] |
//! | Audit | one egress record per request: the client's fields (`tool`, `path` …) + `host`, `ms`, `verdict` (`allowed` / `error` / `denied` before sending), `status`, `class` + `reason` on an error, `weight` when budgeted |
//!
//! [`Retry`] retries one request of a backfill / source fetch
//! (`domain::backoff::next_delay`, `Retry-After` respected); a client that
//! must not wait (HL info, RPC tools) has none here.

use std::fmt;
use std::future::Future;
use std::time::{Duration, Instant};

use anyhow::Result;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use reqwest::Url;
use serde_json::{json, Value};

use crate::adapters::outbound::rate_limit::{jitter01, Limiters, Priority};
use crate::adapters::outbound::{egress, keys};
use crate::config::rate_limits::RateLimitConfig;
use crate::domain::backoff::{next_delay, BackoffPolicy, Delay};
use crate::domain::observation::{ErrorClass, ReadError};
use crate::domain::scope::ToolScope;

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

/// One reply as read, before its client decodes it.
pub(crate) struct RawReply {
    pub status: u16,
    pub retry_after_ms: Option<u64>,
    pub headers: reqwest::header::HeaderMap,
    pub body: String,
}

impl RawReply {
    pub(crate) fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The reply when 2xx, else its [`http_status_error`].
    pub(crate) fn success(self, host: &str, scrub: &Scrubber) -> Result<Self, HttpError> {
        if self.is_success() {
            Ok(self)
        } else {
            Err(http_status_error(
                self.status,
                self.retry_after_ms,
                &self.body,
                host,
                scrub,
            ))
        }
    }

    /// The reply's `Content-Type`, when readable.
    pub(crate) fn content_type(&self) -> Option<&str> {
        self.headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok())
    }
}

/// The `[rate_limits.<name>]` budget one request is charged against.
pub(crate) struct Budget<'a> {
    pub limiters: &'static Limiters,
    pub name: &'a str,
    /// `None` = unlimited.
    pub config: Option<&'a RateLimitConfig>,
    pub weight: u32,
    /// Longest wait for budget before the request fails `RateLimited`.
    pub max_wait: Duration,
}

/// One request through the shared pipeline (module table: Step).
pub(crate) struct GatedHttp<'a> {
    scope: &'a ToolScope,
    scrub: &'a Scrubber,
    audit: Value,
    budget: Option<Budget<'a>>,
    #[cfg(test)]
    tap: Option<&'a std::sync::Mutex<Vec<Value>>>,
}

impl<'a> GatedHttp<'a> {
    /// `audit` = the client's own fields of the egress record (`tool`, …).
    pub(crate) fn new(scope: &'a ToolScope, scrub: &'a Scrubber, audit: Value) -> Self {
        Self {
            scope,
            scrub,
            audit,
            budget: None,
            #[cfg(test)]
            tap: None,
        }
    }

    pub(crate) fn budget(mut self, budget: Budget<'a>) -> Self {
        self.budget = Some(budget);
        self
    }

    /// Tests: every audit record is also pushed here.
    #[cfg(test)]
    pub(crate) fn tap(mut self, tap: Option<&'a std::sync::Mutex<Vec<Value>>>) -> Self {
        self.tap = tap;
        self
    }

    /// Send `req`; `decode` turns the reply (any status) into the result.
    pub(crate) async fn send<T>(
        self,
        req: reqwest::RequestBuilder,
        decode: impl FnOnce(RawReply) -> Result<T, HttpError>,
    ) -> Result<T, HttpError> {
        self.send_weighted(req, decode, |_| 0).await
    }

    /// [`Self::send`], charging `extra(&result)` more weight after the reply.
    pub(crate) async fn send_weighted<T>(
        mut self,
        req: reqwest::RequestBuilder,
        decode: impl FnOnce(RawReply) -> Result<T, HttpError>,
        extra: impl FnOnce(&T) -> u32,
    ) -> Result<T, HttpError> {
        let (client, req) = req.build_split();
        let mut req = match req {
            Ok(r) => r,
            Err(e) => {
                let err = HttpError::new(
                    ErrorClass::Fatal,
                    self.scrub
                        .scrub(&format!("request not built: {}", e.without_url())),
                );
                self.audit(Err(&err), None, 0);
                return Err(err);
            }
        };
        let host = req.url().host_str().unwrap_or("").to_string();
        self.audit["host"] = host.clone().into();
        let weight = self.budget.as_ref().map_or(0, |b| b.weight);

        let gate = egress::policy()
            .check_url(req.url())
            .and_then(|_| self.scope.check_net_host(&host));
        if let Err(e) = gate {
            let err = HttpError::new(ErrorClass::Fatal, self.scrub.scrub(&format!("{e:#}")));
            self.audit(Err(&err), None, weight);
            return Err(err);
        }
        if let Some(b) = &self.budget {
            if let Err(err) = b
                .limiters
                .acquire(b.name, b.config, b.weight, Priority::Read, b.max_wait)
                .await
            {
                self.audit(Err(&err), None, weight);
                return Err(err);
            }
        }
        let session = keys::auth_header_for(req.url().as_str());
        add_session(&mut req, session);

        let started = Instant::now();
        let result = match client.execute(req).await {
            Err(e) => Err(reqwest_error(e, &host, self.scrub)),
            Ok(resp) => {
                let status = resp.status().as_u16();
                let retry_after_ms = retry_after_ms(resp.headers());
                let headers = resp.headers().clone();
                match resp.text().await {
                    Err(e) => Err(reqwest_error(e, &host, self.scrub)),
                    Ok(body) => decode(RawReply {
                        status,
                        retry_after_ms,
                        headers,
                        body,
                    })
                    .map(|v| (status, v)),
                }
            }
        };
        let ms = Some(started.elapsed().as_millis() as u64);
        match result {
            Ok((status, v)) => {
                let extra = extra(&v);
                if let Some(b) = &self.budget {
                    b.limiters.charge(b.name, b.config, extra);
                }
                self.audit(Ok(status), ms, weight + extra);
                Ok(v)
            }
            Err(err) => {
                if let Some(b) = self.budget.as_ref() {
                    if err.class == ErrorClass::RateLimited {
                        b.limiters.penalize(b.name, b.config, err.retry_after_ms);
                    }
                }
                self.audit(Err(&err), ms, weight);
                Err(err)
            }
        }
    }

    fn audit(&mut self, outcome: Result<u16, &HttpError>, ms: Option<u64>, weight: u32) {
        let event = audit_record(
            std::mem::take(&mut self.audit),
            outcome,
            ms,
            self.budget.as_ref().map(|_| weight),
        );
        #[cfg(test)]
        if let Some(tap) = self.tap {
            tap.lock().unwrap().push(event.clone());
        }
        egress::policy().audit(event);
    }
}

/// The egress record of one request (module table: Audit).
pub(crate) fn audit_record(
    mut event: Value,
    outcome: Result<u16, &HttpError>,
    ms: Option<u64>,
    weight: Option<u32>,
) -> Value {
    event["ms"] = json!(ms);
    if let Some(w) = weight {
        event["weight"] = w.into();
    }
    match outcome {
        Ok(status) => {
            event["verdict"] = "allowed".into();
            event["status"] = status.into();
        }
        Err(e) => {
            event["verdict"] = if ms.is_some() { "error" } else { "denied" }.into();
            event["class"] = e.class.as_str().into();
            event["reason"] = e.message.clone().into();
            if let Some(s) = e.http_status {
                event["status"] = s.into();
            }
        }
    }
    event
}

/// `Authorization: <session>` on a seal-proxy request that has none.
fn add_session(req: &mut reqwest::Request, session: Option<String>) {
    let Some(value) = session.and_then(|s| reqwest::header::HeaderValue::from_str(&s).ok()) else {
        return;
    };
    req.headers_mut().entry(AUTHORIZATION).or_insert(value);
}

/// Retry of one backfill / source request (module doc).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Retry {
    pub policy: BackoffPolicy,
    /// A `Park` (quota, a long `Retry-After`) up to this long is waited out;
    /// longer fails the request.
    pub max_park_ms: u64,
}

impl Retry {
    /// Operator backfills: 5 retries, 1 s … 60 s full jitter, parks ≤ 5 min.
    pub(crate) const BACKFILL: Retry = Retry {
        policy: BackoffPolicy {
            base_ms: 2_000,
            cap_ms: 60_000,
            min_ms: 1_000,
            max_attempts: 5,
            quota_park_ms: 3_600_000,
        },
        max_park_ms: 300_000,
    };

    /// A tool call's backfill (`market_history` `fetch`): a model waits for
    /// it — 2 retries, 250 ms … 5 s, parks ≤ 10 s.
    pub(crate) const TOOL: Retry = Retry {
        policy: BackoffPolicy {
            base_ms: 500,
            cap_ms: 5_000,
            min_ms: 250,
            max_attempts: 2,
            quota_park_ms: 3_600_000,
        },
        max_park_ms: 10_000,
    };

    /// `call` until it succeeds or `next_delay` stops (the error is then
    /// returned). `what` labels the log line.
    pub(crate) async fn run<T, F, Fut>(&self, what: &str, mut call: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let mut attempt = 0u32;
        loop {
            let err = match call().await {
                Ok(v) => return Ok(v),
                Err(e) => e,
            };
            attempt += 1;
            let r = read_error(what, &err);
            let wait =
                match next_delay(r.class, attempt, r.retry_after_ms, &self.policy, jitter01()) {
                    Delay::Retry(ms) => ms,
                    Delay::Park(ms) if ms <= self.max_park_ms => ms,
                    Delay::Park(_) | Delay::Stop => return Err(err),
                };
            tracing::warn!(
                what,
                class = r.class.as_str(),
                attempt,
                wait_ms = wait,
                error = %r.message,
                "request failed; retrying"
            );
            tokio::time::sleep(Duration::from_millis(wait)).await;
        }
    }
}

/// Local HTTP server + client for outbound tests (no network).
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::Retry;
    use crate::domain::backoff::BackoffPolicy;
    use crate::domain::scope::ToolScope;

    /// No real waits: 1 – 10 ms between attempts.
    pub(crate) const FAST: Retry = Retry {
        policy: BackoffPolicy {
            base_ms: 1,
            cap_ms: 10,
            min_ms: 1,
            max_attempts: 3,
            quota_park_ms: 10,
        },
        max_park_ms: 10,
    };

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

    #[tokio::test]
    async fn retry_retries_transient_and_stops_on_fatal() {
        let calls = std::sync::atomic::AtomicU32::new(0);
        let got = test_support::FAST
            .run("t", || {
                let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move {
                    if n < 2 {
                        Err(HttpError::new(ErrorClass::Transient, "502").into())
                    } else {
                        Ok(n)
                    }
                }
            })
            .await
            .unwrap();
        assert_eq!(got, 2);

        let calls = std::sync::atomic::AtomicU32::new(0);
        let e = test_support::FAST
            .run("t", || {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async { Err::<(), _>(HttpError::new(ErrorClass::Fatal, "404").into()) }
            })
            .await
            .unwrap_err();
        assert!(e.to_string().contains("404"));
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "no retry"
        );

        // Retries run out: the last error is returned.
        let calls = std::sync::atomic::AtomicU32::new(0);
        assert!(test_support::FAST
            .run("t", || {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async { Err::<(), _>(HttpError::new(ErrorClass::Timeout, "slow").into()) }
            })
            .await
            .is_err());
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            4,
            "1 + 3 retries"
        );

        // A Retry-After beyond the park limit is not waited out.
        let mut slow = HttpError::new(ErrorClass::RateLimited, "429");
        slow.retry_after_ms = Some(60_000);
        let calls = std::sync::atomic::AtomicU32::new(0);
        assert!(test_support::FAST
            .run("t", || {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let e = slow.clone();
                async move { Err::<(), _>(e.into()) }
            })
            .await
            .is_err());
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// One pipeline: a scope denial sends nothing; a 2xx is decoded and
    /// charged its weight + extra; a non-2xx is classified; one audit
    /// record each.
    #[tokio::test]
    async fn gated_http_gates_budgets_and_audits() {
        use test_support::{canned, local_scope, serve, test_client};
        let (base, seen) = serve(vec![canned(200, "[1,2,3]"), canned(503, "busy")]).await;
        let url = Url::parse(&format!("{base}/x")).unwrap();
        let scrub = Scrubber::for_api(&url);
        let tap = std::sync::Mutex::new(Vec::new());
        let limiters: &'static Limiters = Box::leak(Box::new(Limiters::default()));
        let cfg = RateLimitConfig {
            per_minute: 600,
            burst: None,
            exec_reserve: 0,
        };
        let budget = || Budget {
            limiters,
            name: "t",
            config: Some(&cfg),
            weight: 2,
            max_wait: Duration::ZERO,
        };
        let fields = json!({"tool": "t", "path": "/x"});

        let other = ToolScope {
            net_hosts: vec!["other.host".into()],
            ..Default::default()
        };
        let e = GatedHttp::new(&other, &scrub, fields.clone())
            .budget(budget())
            .tap(Some(&tap))
            .send(test_client().get(url.clone()), |r| Ok(r.body))
            .await
            .unwrap_err();
        assert_eq!(e.class, ErrorClass::Fatal);

        let scope = local_scope();
        let n = GatedHttp::new(&scope, &scrub, fields.clone())
            .budget(budget())
            .tap(Some(&tap))
            .send_weighted(
                test_client().get(url.clone()),
                |r| {
                    serde_json::from_str::<Vec<u32>>(&r.body)
                        .map_err(|e| HttpError::new(ErrorClass::Decode, e.to_string()))
                },
                |v| v.len() as u32,
            )
            .await
            .unwrap();
        assert_eq!(n, vec![1, 2, 3]);

        let e = GatedHttp::new(&scope, &scrub, fields)
            .budget(budget())
            .tap(Some(&tap))
            .send(test_client().get(url.clone()), |r| {
                r.success("127.0.0.1", &scrub).map(|r| r.body)
            })
            .await
            .unwrap_err();
        assert_eq!((e.class, e.http_status), (ErrorClass::Transient, Some(503)));
        assert_eq!(seen.lock().unwrap().len(), 2, "the denial sent nothing");

        let ev = tap.lock().unwrap();
        assert_eq!(ev.len(), 3);
        for e in ev.iter() {
            assert_eq!((&e["tool"], &e["path"]), (&json!("t"), &json!("/x")));
            assert_eq!(e["host"], "127.0.0.1");
        }
        assert_eq!(
            (&ev[0]["verdict"], &ev[0]["class"]),
            (&json!("denied"), &json!("fatal"))
        );
        assert!(ev[0]["ms"].is_null());
        assert_eq!(
            (&ev[1]["verdict"], &ev[1]["status"]),
            (&json!("allowed"), &json!(200))
        );
        assert_eq!(ev[1]["weight"], 5, "2 + 3 items");
        assert_eq!(
            (&ev[2]["verdict"], &ev[2]["status"]),
            (&json!("error"), &json!(503))
        );
        assert_eq!(ev[2]["class"], "transient");
        assert!(ev[2]["ms"].is_u64());
    }

    /// The seal session goes on a request that has no `Authorization` yet.
    #[test]
    fn the_session_never_replaces_a_header() {
        let c = test_support::test_client();
        let mut r = c.get("http://127.0.0.1/x").build().unwrap();
        add_session(&mut r, None);
        assert!(r.headers().get(AUTHORIZATION).is_none());
        add_session(&mut r, Some("Bearer s1".into()));
        assert_eq!(r.headers()[AUTHORIZATION], "Bearer s1");
        let mut r = c
            .get("http://127.0.0.1/x")
            .header(AUTHORIZATION, "Bearer own")
            .build()
            .unwrap();
        add_session(&mut r, Some("Bearer s1".into()));
        assert_eq!(r.headers()[AUTHORIZATION], "Bearer own");
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
