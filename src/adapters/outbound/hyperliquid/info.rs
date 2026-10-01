//! `HlInfo` — Hyperliquid `POST /info` over the tool HTTP client.
//!
//! | Concern | Rule |
//! |---|---|
//! | URL | `https://api.hyperliquid.xyz/info`; `$HL_API_URL` (testnet: `https://api.hyperliquid-testnet.xyz`) when the scope's `env_reads` allows it and it is set |
//! | Gate | `egress::policy().check_url` + `scope.check_net_host(host)` on every request; a denial is `Fatal`, nothing is sent |
//! | Budget | `[rate_limits.hyperliquid]` (`outbound/rate_limit.rs`): the request weight is acquired before sending (reads leave `exec_reserve`; ≤ 15 s wait, else `RateLimited`), the per-item extra is charged after the reply, a 429 drains the bucket; no section = unlimited (one debug line) |
//! | Audit | one egress record per request: `tool = "hl_info"`, `info_type`, `host`, `path` (path segments ≥ 8 chars `<redacted>` — a keyed `HL_API_URL`, review #15), `weight`, `status`, `ms` |
//! | Secrets | a keyed `HL_API_URL` path never renders: errors scrub through `Scrubber::for_keyed_path`, the egress line carries `redacted_path`, a malformed base is refused without echoing it |
//! | Retry | none — callers cache (`observe()`) or schedule (`domain::backoff::next_delay`) |
//!
//! | Request `type` | Weight (HL docs, 1200 / min / IP) |
//! |---|---|
//! | `l2Book`, `allMids`, `clearinghouseState`, `orderStatus`, `spotClearinghouseState`, `exchangeStatus` | 2 |
//! | `userRole` | 60 |
//! | every other type | 20 |
//! | `candleSnapshot` | + 1 per 60 candles returned |
//! | `recentTrades`, `historicalOrders`, `userFills`, `userFillsByTime`, `fundingHistory`, `userFunding`, `nonUserFundingUpdates`, `twapHistory`, `userTwapSliceFills`, `userTwapSliceFillsByTime`, `delegatorHistory`, `delegatorRewards`, `validatorStats` | + 1 per 20 items returned |
//!
//! | Reply | Result |
//! |---|---|
//! | 200 JSON | `InfoReply::Json` |
//! | 200 `null` | `InfoReply::Null` — unknown coin / user: the row is `absent` |
//! | 500 with body `null` | `NotApplicable` — unknown dex / coin, not an outage |
//! | 422 | `Fatal` — the body does not deserialize (bad `type` / shape) |
//! | 429 | `RateLimited` + `Retry-After` |
//! | 403 | `AuthRequired` "blocked (geo/WAF/Tor exit?)" |
//! | other 5xx | `Transient` |
//! | timeout | `Timeout` |
//! | 200, not JSON | `Decode` |

// Consumers: `hl-ctx-tool`, `hl-book-tool` (next wave).
#![allow(dead_code)]

use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use reqwest::Url;
use serde_json::{json, Value};

use crate::adapters::outbound::egress;
use crate::adapters::outbound::http_class::{
    body_snippet, http_status_error, redacted_path, reqwest_error, retry_after_ms, HttpError,
    Scrubber,
};
use crate::adapters::outbound::rate_limit::{limiters, Limiters, Priority};
use crate::config::rate_limits::RateLimitConfig;
use crate::domain::observation::ErrorClass;
use crate::domain::scope::ToolScope;
use crate::ports::tool::ToolCtx;

/// Mainnet API base.
pub(crate) const MAINNET_URL: &str = "https://api.hyperliquid.xyz";
/// Env var overriding the API base (testnet), read only when the scope allows.
pub(crate) const API_URL_ENV: &str = "HL_API_URL";
/// The `[rate_limits.<name>]` HL requests budget against.
pub(crate) const RATE_LIMIT: &str = "hyperliquid";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_BUDGET_WAIT: Duration = Duration::from_secs(15);

/// Info types weighing 2; `userRole` weighs 60, everything else 20.
const WEIGHT_2: &[&str] = &[
    "l2Book",
    "allMids",
    "clearinghouseState",
    "orderStatus",
    "spotClearinghouseState",
    "exchangeStatus",
];

/// Info types charged + 1 per 20 items returned.
const PER_20_ITEMS: &[&str] = &[
    "recentTrades",
    "historicalOrders",
    "userFills",
    "userFillsByTime",
    "fundingHistory",
    "userFunding",
    "nonUserFundingUpdates",
    "twapHistory",
    "userTwapSliceFills",
    "userTwapSliceFillsByTime",
    "delegatorHistory",
    "delegatorRewards",
    "validatorStats",
];

/// A successful `/info` reply.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum InfoReply {
    Json(Value),
    /// `200 null`: HL does not know the coin / user.
    Null,
}

/// Base weight of an info request `type`.
pub(crate) fn request_weight(info_type: &str) -> u32 {
    if WEIGHT_2.contains(&info_type) {
        2
    } else if info_type == "userRole" {
        60
    } else {
        20
    }
}

/// Weight charged after the reply: + 1 per 60 candles, + 1 per 20 items for
/// the list types (rounded up); 0 for everything else.
pub(crate) fn reply_extra_weight(info_type: &str, reply: &InfoReply) -> u32 {
    let InfoReply::Json(Value::Array(items)) = reply else {
        return 0;
    };
    let per = if info_type == "candleSnapshot" {
        60
    } else if PER_20_ITEMS.contains(&info_type) {
        20
    } else {
        return 0;
    };
    items.len().div_ceil(per) as u32
}

/// The API base: `$HL_API_URL` when the scope may read it and it is set,
/// else mainnet.
pub(crate) fn api_url(scope: &ToolScope) -> String {
    match scope.check_env_read(API_URL_ENV) {
        Ok(()) => api_url_from(std::env::var(API_URL_ENV).ok()),
        Err(_) => MAINNET_URL.to_string(),
    }
}

fn api_url_from(env: Option<String>) -> String {
    env.map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| MAINNET_URL.to_string())
}

/// Classify one reply (module table). Pure — tested without a server.
pub(crate) fn classify_reply(
    status: u16,
    retry_after: Option<u64>,
    body: &str,
    host: &str,
    scrub: &Scrubber,
) -> std::result::Result<InfoReply, HttpError> {
    let trimmed = body.trim();
    if (200..300).contains(&status) {
        if trimmed == "null" {
            return Ok(InfoReply::Null);
        }
        return serde_json::from_str::<Value>(trimmed)
            .map(InfoReply::Json)
            .map_err(|e| {
                let mut err = HttpError::new(
                    ErrorClass::Decode,
                    scrub.scrub(&format!(
                        "HTTP {status} from {host} is not JSON ({e}): {}",
                        body_snippet(body)
                    )),
                );
                err.http_status = Some(status);
                err
            });
    }
    let mut err = match status {
        500 if trimmed == "null" => HttpError::new(
            ErrorClass::NotApplicable,
            format!("HTTP 500 null from {host}: unknown dex or coin"),
        ),
        403 => HttpError::new(
            ErrorClass::AuthRequired,
            format!("HTTP 403 from {host}: blocked (geo/WAF/Tor exit?)"),
        ),
        422 => HttpError::new(
            ErrorClass::Fatal,
            scrub.scrub(&format!("HTTP 422 from {host}: {}", body_snippet(body))),
        ),
        _ => return Err(http_status_error(status, retry_after, body, host, scrub)),
    };
    err.http_status = Some(status);
    err.retry_after_ms = retry_after;
    Err(err)
}

/// One `/info` endpoint with its scope and budget. Cheap to build per call.
pub(crate) struct HlInfo {
    http: reqwest::Client,
    url: Url,
    host: String,
    scope: ToolScope,
    budget: Option<RateLimitConfig>,
    timeout: Duration,
    max_budget_wait: Duration,
    limiters: &'static Limiters,
}

impl HlInfo {
    /// Client for a tool call: [`api_url`], `ctx.http`, `ctx.scope`, and the
    /// calling agent's `[rate_limits.hyperliquid]` (`AgentConfig::sandbox`).
    pub(crate) fn from_ctx(ctx: &ToolCtx<'_>) -> Result<Self> {
        let budget = ctx
            .agent_config
            .and_then(|a| a.sandbox.rate_limits.get(RATE_LIMIT).cloned());
        Self::new(
            ctx.http.clone(),
            &api_url(ctx.scope),
            ctx.scope.clone(),
            budget,
        )
    }

    /// `base` = API base (`…/info` appended unless present).
    pub(crate) fn new(
        http: reqwest::Client,
        base: &str,
        scope: ToolScope,
        budget: Option<RateLimitConfig>,
    ) -> Result<Self> {
        let base = base.trim().trim_end_matches('/');
        let full = if base.ends_with("/info") {
            base.to_string()
        } else {
            format!("{base}/info")
        };
        // The value is never echoed: a provider URL may carry a key.
        let url = Url::parse(&full).map_err(|e| anyhow!("{API_URL_ENV} is not a URL ({e})"))?;
        let host = url
            .host_str()
            .filter(|h| !h.is_empty())
            .ok_or_else(|| anyhow!("{API_URL_ENV} has no host"))?
            .to_string();
        Ok(Self {
            http,
            url,
            host,
            scope,
            budget,
            timeout: REQUEST_TIMEOUT,
            max_budget_wait: MAX_BUDGET_WAIT,
            limiters: limiters(),
        })
    }

    pub(crate) fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Longest wait for budget before a read fails `rate_limited`.
    pub(crate) fn with_max_budget_wait(mut self, wait: Duration) -> Self {
        self.max_budget_wait = wait;
        self
    }

    #[cfg(test)]
    fn with_limiters(mut self, limiters: &'static Limiters) -> Self {
        self.limiters = limiters;
        self
    }

    /// Host only — for headlines and audit.
    pub(crate) fn host(&self) -> &str {
        &self.host
    }

    /// POST `body` (`{"type": …, …}`) and classify the reply (module
    /// tables). Errors carry an [`HttpError`] (`http_class::read_error`).
    pub(crate) async fn post(&self, body: &Value) -> Result<InfoReply> {
        let info_type = body["type"].as_str().unwrap_or("").to_string();
        let weight = request_weight(&info_type);
        let scrub = Scrubber::for_keyed_path(&self.url);

        let gate = egress::policy()
            .check_url(&self.url)
            .and_then(|_| self.scope.check_net_host(&self.host));
        if let Err(e) = gate {
            let err = HttpError::new(ErrorClass::Fatal, scrub.scrub(&format!("{e:#}")));
            self.audit(&info_type, weight, Err(&err), None);
            return Err(err.into());
        }
        let budget = self.budget.as_ref();
        if let Err(err) = self
            .limiters
            .acquire(
                RATE_LIMIT,
                budget,
                weight,
                Priority::Read,
                self.max_budget_wait,
            )
            .await
        {
            self.audit(&info_type, weight, Err(&err), None);
            return Err(err.into());
        }

        let started = Instant::now();
        let sent = self
            .http
            .post(self.url.clone())
            .timeout(self.timeout)
            .header(reqwest::header::ACCEPT, "application/json")
            .json(body)
            .send()
            .await;
        let result = match sent {
            Err(e) => Err(reqwest_error(e, &self.host, &scrub)),
            Ok(resp) => {
                let status = resp.status().as_u16();
                let retry_after = retry_after_ms(resp.headers());
                match resp.text().await {
                    Err(e) => Err(reqwest_error(e, &self.host, &scrub)),
                    Ok(text) => classify_reply(status, retry_after, &text, &self.host, &scrub)
                        .map(|r| (status, r)),
                }
            }
        };
        let ms = Some(started.elapsed().as_millis() as u64);
        match result {
            Ok((status, reply)) => {
                let extra = reply_extra_weight(&info_type, &reply);
                self.limiters.charge(RATE_LIMIT, budget, extra);
                self.audit(&info_type, weight + extra, Ok(status), ms);
                Ok(reply)
            }
            Err(err) => {
                if err.class == ErrorClass::RateLimited {
                    self.limiters
                        .penalize(RATE_LIMIT, budget, err.retry_after_ms);
                }
                self.audit(&info_type, weight, Err(&err), ms);
                Err(err.into())
            }
        }
    }

    fn audit(
        &self,
        info_type: &str,
        weight: u32,
        outcome: std::result::Result<u16, &HttpError>,
        ms: Option<u64>,
    ) {
        egress::policy().audit(self.audit_event(info_type, weight, outcome, ms));
    }

    /// The egress record of one request (module table: Audit) — the path
    /// with a keyed segment `<redacted>` (review #15).
    fn audit_event(
        &self,
        info_type: &str,
        weight: u32,
        outcome: std::result::Result<u16, &HttpError>,
        ms: Option<u64>,
    ) -> Value {
        let mut event = json!({
            "tool": "hl_info",
            "info_type": info_type,
            "host": self.host,
            "path": redacted_path(&self.url),
            "weight": weight,
            "ms": ms,
        });
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::outbound::http_class::read_error;
    use crate::adapters::outbound::http_class::test_support::{
        canned, local_scope, serve, test_client, Canned,
    };
    use crate::adapters::outbound::tools::workspace::test_support::TestHarness;
    use crate::domain::market::{decimal_field, InstrumentId};
    use crate::domain::observation::Field;

    fn fixture(name: &str) -> String {
        let path = format!(
            "{}/tests/fixtures/hyperliquid/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    fn fixture_json(name: &str) -> Value {
        serde_json::from_str(&fixture(name)).unwrap()
    }

    /// `errors.json` case → (status, body).
    fn error_case(case: &str) -> (u16, String) {
        let e = &fixture_json("errors.json")[case];
        (
            e["status"].as_u64().unwrap() as u16,
            e["body"].as_str().unwrap().to_string(),
        )
    }

    fn own_limiters() -> &'static Limiters {
        Box::leak(Box::new(Limiters::default()))
    }

    fn client(base: &str, budget: Option<RateLimitConfig>) -> HlInfo {
        HlInfo::new(test_client(), base, local_scope(), budget)
            .unwrap()
            .with_limiters(own_limiters())
            .with_max_budget_wait(Duration::ZERO)
    }

    fn class_of(e: &anyhow::Error) -> ErrorClass {
        read_error("hl", e).class
    }

    #[tokio::test]
    async fn replays_captured_replies_and_maps_hl_errors() {
        let (_, dex_null) = error_case("unknown_dex");
        let (_, bogus) = error_case("bogus_type");
        let (_, coin_null) = error_case("unknown_coin");
        let (base, seen) = serve(vec![
            canned(200, fixture("l2Book_xyz_TSLA.json")),
            canned(200, coin_null),
            canned(500, dex_null),
            Canned {
                status: 422,
                headers: "Content-Type: text/plain; charset=utf-8\r\n",
                body: bogus,
                delay_ms: 0,
            },
            Canned {
                status: 429,
                headers: "Retry-After: 2\r\n",
                body: "rate limited".into(),
                delay_ms: 0,
            },
            canned(
                403,
                "<html>403 ERROR The request could not be satisfied.</html>",
            ),
            canned(502, "bad gateway"),
            canned(200, "<html>oops</html>"),
        ])
        .await;
        let hl = client(&base, None);

        let body = json!({"type": "l2Book", "coin": "xyz:TSLA"});
        let InfoReply::Json(book) = hl.post(&body).await.unwrap() else {
            panic!("expected JSON");
        };
        assert_eq!(book["coin"], "xyz:TSLA");
        assert_eq!(book["levels"][0].as_array().unwrap().len(), 20);
        let req = seen.lock().unwrap()[0].clone();
        assert!(req.starts_with("POST /info HTTP/1.1"), "{req}");
        assert!(
            req.to_ascii_lowercase()
                .contains("content-type: application/json"),
            "{req}"
        );
        let sent: Value = serde_json::from_str(req.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(sent, body);

        let nope = json!({"type": "l2Book", "coin": "xyz:NOPE"});
        assert_eq!(hl.post(&nope).await.unwrap(), InfoReply::Null);

        for (body, class, text) in [
            (
                json!({"type": "metaAndAssetCtxs", "dex": "nope"}),
                ErrorClass::NotApplicable,
                "HTTP 500 null from 127.0.0.1: unknown dex or coin",
            ),
            (
                json!({"type": "bogus"}),
                ErrorClass::Fatal,
                "HTTP 422 from 127.0.0.1: Failed to deserialize the JSON body into the target type",
            ),
            (
                json!({"type": "allMids"}),
                ErrorClass::RateLimited,
                "HTTP 429 from 127.0.0.1",
            ),
            (
                json!({"type": "allMids"}),
                ErrorClass::AuthRequired,
                "HTTP 403 from 127.0.0.1: blocked (geo/WAF/Tor exit?)",
            ),
            (
                json!({"type": "allMids"}),
                ErrorClass::Transient,
                "HTTP 502 from 127.0.0.1",
            ),
            (
                json!({"type": "allMids"}),
                ErrorClass::Decode,
                "is not JSON",
            ),
        ] {
            let e = hl.post(&body).await.unwrap_err();
            let r = read_error("ctx", &e);
            assert_eq!(r.class, class, "{e:#}");
            assert!(r.message.contains(text), "{}", r.message);
            if class == ErrorClass::RateLimited {
                assert_eq!(r.retry_after_ms, Some(2_000));
            }
        }
        assert_eq!(seen.lock().unwrap().len(), 8, "no retries");
    }

    #[tokio::test]
    async fn timeout_and_scope_denial() {
        let (base, seen) = serve(vec![Canned {
            status: 200,
            headers: "",
            body: "{}".into(),
            delay_ms: 1_500,
        }])
        .await;
        let hl = client(&base, None).with_timeout(Duration::from_millis(200));
        let e = hl.post(&json!({"type": "allMids"})).await.unwrap_err();
        assert_eq!(class_of(&e), ErrorClass::Timeout);

        let denied = HlInfo::new(test_client(), &base, ToolScope::default(), None).unwrap();
        let before = seen.lock().unwrap().len();
        let e = denied.post(&json!({"type": "allMids"})).await.unwrap_err();
        assert_eq!(class_of(&e), ErrorClass::Fatal);
        assert!(format!("{e}").contains("net_hosts"), "{e}");
        assert_eq!(seen.lock().unwrap().len(), before, "nothing sent");
    }

    #[tokio::test]
    async fn budget_is_acquired_before_and_charged_after() {
        let (base, seen) = serve(vec![
            canned(200, fixture("candleSnapshot_xyz_TSLA_1h.json")),
            canned(200, fixture("l2Book_xyz_TSLA.json")),
        ])
        .await;
        // 1 weight / minute: nothing refills during the test.
        let budget = RateLimitConfig {
            per_minute: 1,
            burst: Some(25),
            exec_reserve: 0,
        };
        let hl = client(&base, Some(budget));
        let candles = json!({"type": "candleSnapshot", "req": {"coin": "xyz:TSLA",
            "interval": "1h", "startTime": 1_790_366_400_000u64, "endTime": 1_790_604_000_000u64}});
        let InfoReply::Json(c) = hl.post(&candles).await.unwrap() else {
            panic!("expected candles");
        };
        assert_eq!(c.as_array().unwrap().len(), 67);
        // 25 − 20 (request) − 2 (67 candles) = 3 left.
        let left = hl.limiters.tokens(RATE_LIMIT).unwrap();
        assert!((3.0..3.1).contains(&left), "{left}");
        hl.post(&json!({"type": "l2Book", "coin": "xyz:TSLA"}))
            .await
            .unwrap();
        // 1 left: a 20-weight read is refused before sending.
        let e = hl
            .post(&json!({"type": "metaAndAssetCtxs", "dex": "xyz"}))
            .await
            .unwrap_err();
        assert_eq!(class_of(&e), ErrorClass::RateLimited, "{e:#}");
        assert!(
            format!("{e:#}").contains("[rate_limits.hyperliquid]"),
            "{e:#}"
        );
        assert_eq!(
            seen.lock().unwrap().len(),
            2,
            "the refused read was not sent"
        );
    }

    #[tokio::test]
    async fn a_429_drains_the_shared_bucket() {
        let (base, seen) = serve(vec![Canned {
            status: 429,
            headers: "Retry-After: 30\r\n",
            body: "too many".into(),
            delay_ms: 0,
        }])
        .await;
        let budget = RateLimitConfig {
            per_minute: 1_200,
            burst: None,
            exec_reserve: 0,
        };
        let hl = client(&base, Some(budget));
        let e = hl.post(&json!({"type": "allMids"})).await.unwrap_err();
        assert_eq!(read_error("x", &e).retry_after_ms, Some(30_000));
        let e = hl
            .post(&json!({"type": "l2Book", "coin": "xyz:TSLA"}))
            .await
            .unwrap_err();
        let r = read_error("x", &e);
        assert_eq!(r.class, ErrorClass::RateLimited);
        assert!(r.retry_after_ms.unwrap() > 29_000, "{r:?}");
        assert_eq!(seen.lock().unwrap().len(), 1, "held back, not sent");
    }

    #[tokio::test]
    async fn unconfigured_budget_is_unlimited() {
        let (base, _) = serve(vec![canned(200, "{}"), canned(200, "{}")]).await;
        let hl = client(&base, None);
        for _ in 0..2 {
            hl.post(
                &json!({"type": "userRole", "user": "0x0000000000000000000000000000000000000000"}),
            )
            .await
            .unwrap();
        }
        assert!(hl.limiters.tokens(RATE_LIMIT).is_none());
    }

    #[test]
    fn weights_follow_the_hl_table() {
        for (t, w) in [
            ("l2Book", 2),
            ("allMids", 2),
            ("clearinghouseState", 2),
            ("orderStatus", 2),
            ("spotClearinghouseState", 2),
            ("exchangeStatus", 2),
            ("userRole", 60),
            ("metaAndAssetCtxs", 20),
            ("allPerpMetas", 20),
            ("perpDexs", 20),
            ("candleSnapshot", 20),
            ("", 20),
        ] {
            assert_eq!(request_weight(t), w, "{t}");
        }
        let list = |n: usize| InfoReply::Json(Value::Array(vec![json!({}); n]));
        assert_eq!(reply_extra_weight("candleSnapshot", &list(67)), 2);
        assert_eq!(reply_extra_weight("candleSnapshot", &list(60)), 1);
        assert_eq!(reply_extra_weight("recentTrades", &list(20)), 1);
        assert_eq!(reply_extra_weight("fundingHistory", &list(21)), 2);
        assert_eq!(reply_extra_weight("userFills", &list(0)), 0);
        assert_eq!(reply_extra_weight("metaAndAssetCtxs", &list(2)), 0);
        assert_eq!(reply_extra_weight("candleSnapshot", &InfoReply::Null), 0);
    }

    #[test]
    fn api_url_is_scoped_and_normalised() {
        assert_eq!(api_url(&ToolScope::default()), MAINNET_URL);
        assert_eq!(
            api_url_from(Some(" https://api.hyperliquid-testnet.xyz ".into())),
            "https://api.hyperliquid-testnet.xyz"
        );
        assert_eq!(api_url_from(Some("  ".into())), MAINNET_URL);
        assert_eq!(api_url_from(None), MAINNET_URL);
        for base in [
            "https://api.hyperliquid.xyz",
            "https://api.hyperliquid.xyz/",
            "https://api.hyperliquid.xyz/info",
        ] {
            let hl = HlInfo::new(test_client(), base, ToolScope::default(), None).unwrap();
            assert_eq!(
                hl.url.as_str(),
                "https://api.hyperliquid.xyz/info",
                "{base}"
            );
            assert_eq!(hl.host(), "api.hyperliquid.xyz");
        }
        assert!(HlInfo::new(test_client(), "not a url", ToolScope::default(), None).is_err());
    }

    /// Review #15: a keyed `HL_API_URL` path never reaches the egress line
    /// or an error — path segments ≥ 8 chars are `<redacted>` (the
    /// `Scrubber::for_rpc` rule); the public mainnet URL is unchanged.
    #[tokio::test]
    async fn a_keyed_api_path_is_redacted() {
        let key = "0123456789abcdef0123456789abcdef";
        let base = format!("https://hl.example.com/{key}");
        let hl = HlInfo::new(test_client(), &base, ToolScope::default(), None).unwrap();
        assert_eq!(hl.url.path(), format!("/{key}/info"));
        let event = hl.audit_event("allMids", 2, Ok(200), Some(5));
        assert_eq!(event["path"], "/<redacted>/info");
        assert!(!event.to_string().contains(key), "{event}");
        // A scope denial (nothing sent): the error, and its egress line.
        let e = hl.post(&json!({"type": "allMids"})).await.unwrap_err();
        assert!(!format!("{e:#}").contains(key), "{e:#}");
        let err = HttpError::new(ErrorClass::Fatal, format!("refused for '{}'", hl.url));
        let scrubbed = Scrubber::for_keyed_path(&hl.url).scrub(&err.message);
        assert_eq!(
            scrubbed,
            "refused for 'https://hl.example.com/<redacted>/info'"
        );
        let alone = Scrubber::for_keyed_path(&hl.url).scrub(&format!("key {key} path /{key}/info"));
        assert!(!alone.contains(key), "{alone}");
        // The public URL renders as before.
        let main = HlInfo::new(test_client(), MAINNET_URL, ToolScope::default(), None).unwrap();
        assert_eq!(
            main.audit_event("l2Book", 2, Ok(200), None)["path"],
            "/info"
        );
        let text = "x https://api.hyperliquid.xyz/info y /info";
        assert_eq!(Scrubber::for_keyed_path(&main.url).scrub(text), text);
        // A malformed base is refused without echoing it.
        let e = HlInfo::new(
            test_client(),
            &format!("not a url {key}"),
            ToolScope::default(),
            None,
        )
        .err()
        .unwrap();
        assert!(!e.to_string().contains(key), "{e}");
    }

    #[test]
    fn from_ctx_takes_the_agent_budget() {
        let h = TestHarness::with_scope(&std::env::temp_dir(), local_scope());
        let hl = HlInfo::from_ctx(&h.ctx()).unwrap();
        assert_eq!(hl.budget, None);
        assert_eq!(hl.host(), "api.hyperliquid.xyz");
        let budget = RateLimitConfig {
            per_minute: 1_200,
            burst: None,
            exec_reserve: 200,
        };
        let mut agent = crate::config::Config::default().agents["main"].clone();
        agent.sandbox = std::sync::Arc::new(crate::config::sections::SandboxSections {
            rate_limits: [(RATE_LIMIT.to_string(), budget.clone())].into(),
            ..Default::default()
        });
        let ctx = ToolCtx {
            agent_config: Some(&agent),
            ..h.ctx()
        };
        assert_eq!(HlInfo::from_ctx(&ctx).unwrap().budget, Some(budget));
    }

    /// Shapes the `domain/hl/` decoders build on, pinned to the captured
    /// replies (`tests/fixtures/hyperliquid/meta.json`).
    #[test]
    fn captured_fixture_shapes() {
        let mac = fixture_json("metaAndAssetCtxs_xyz.json");
        let (universe, ctxs) = (
            mac[0]["universe"].as_array().unwrap(),
            mac[1].as_array().unwrap(),
        );
        assert_eq!((universe.len(), ctxs.len()), (128, 128));
        let i = universe
            .iter()
            .position(|u| u["name"] == "xyz:TSLA")
            .unwrap();
        assert_eq!(i, 1);
        let id = InstrumentId::hyperliquid(universe[i]["name"].as_str().unwrap()).unwrap();
        assert_eq!(id.to_string(), "hyperliquid:xyz:TSLA");
        assert_eq!(decimal_field(&ctxs[i], "markPx", "mark"), Field::ok(347.19));
        assert_eq!(
            decimal_field(&ctxs[i], "oraclePx", "oracle"),
            Field::ok(346.91)
        );
        assert_eq!(universe[i]["growthMode"], "enabled");
        assert_eq!(universe[i]["deployerFeeScale"], "1.0");
        // Delisted: flagged in the meta, no book in the ctx.
        let d = universe
            .iter()
            .position(|u| u["name"] == "xyz:URANIUM")
            .unwrap();
        assert_eq!(universe[d]["isDelisted"], true);
        assert_eq!(universe[d]["marginMode"], "strictIsolated");
        assert_eq!(decimal_field(&ctxs[d], "midPx", "mid"), Field::Absent);
        assert!(ctxs[d]["impactPxs"].is_null());

        let default = fixture_json("metaAndAssetCtxs_default.json");
        assert_eq!(default[0]["universe"][15]["name"], "kPEPE");
        assert_eq!(default[0]["universe"].as_array().unwrap().len(), 40);
        assert_eq!(default[1].as_array().unwrap().len(), 40);

        let dexes = fixture_json("perpDexs.json");
        assert!(dexes[0].is_null(), "index 0 = the default dex");
        assert_eq!(dexes[1]["name"], "xyz");
        let cap = dexes[1]["assetToStreamingOiCap"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p[0] == "xyz:TSLA")
            .unwrap();
        assert_eq!(cap[1], "100000000.0");

        let all = fixture_json("allPerpMetas.json");
        let metas = all.as_array().unwrap();
        let markets: usize = metas
            .iter()
            .map(|m| m["universe"].as_array().unwrap().len())
            .sum();
        let listed = metas
            .iter()
            .flat_map(|m| m["universe"].as_array().unwrap())
            .filter(|u| u["isDelisted"] != true)
            .count();
        assert_eq!((metas.len(), markets, listed), (11, 530, 329));

        assert_eq!(
            fixture_json("l2Book_flx_TSLA.json")["levels"],
            json!([[], []])
        );
        assert_eq!(fixture_json("perpsAtOpenInterestCap_xyz.json"), json!([]));
        assert!(fixture_json("perpsAtOpenInterestCap_default.json")
            .as_array()
            .unwrap()
            .contains(&json!("FTM")));
        let candles = fixture_json("candleSnapshot_xyz_TSLA_1h.json");
        let candles = candles.as_array().unwrap();
        assert_eq!(candles[0]["t"], 1_790_366_400_000u64);
        assert_eq!(candles[0]["s"], "xyz:TSLA");
        assert!(candles
            .windows(2)
            .all(|w| w[0]["t"].as_u64() < w[1]["t"].as_u64()));
        let (status, body) = error_case("unknown_dex");
        let scrub = Scrubber::for_api(&Url::parse("https://api.hyperliquid.xyz/info").unwrap());
        let e = classify_reply(status, None, &body, "api.hyperliquid.xyz", &scrub).unwrap_err();
        assert_eq!(e.class, ErrorClass::NotApplicable);
        let (status, body) = error_case("bogus_type");
        let e = classify_reply(status, None, &body, "api.hyperliquid.xyz", &scrub).unwrap_err();
        assert_eq!((e.class, e.http_status), (ErrorClass::Fatal, Some(422)));
    }
}
