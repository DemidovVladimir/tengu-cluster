//! `fetch_json` — scoped, egress-checked JSON GET for the HTTP APIs of the
//! Solana tool family (Jupiter lite price, Pyth Hermes, Meteora datapi).
//!
//! | Concern | Rule |
//! |---|---|
//! | Gate | `egress::policy().check_url` + `ctx.scope.check_net_host(host)` before the request (denial = `Fatal`, nothing sent) |
//! | Client | `ctx.http` (egress tool client, redirects off), 20 s timeout, `Accept: application/json` |
//! | 2xx | `Ok((status, json))`; a non-JSON body is `Decode` |
//! | non-2xx | `Err(RpcError)` classified like RPC: 429 `RateLimited` (+`Retry-After`), 401/403 `AuthRequired`, 5xx `Transient`, quota text `QuotaExhausted`, else `Fatal` |
//! | Errors | the URL renders as `rpc::display_url`: values of credential-like query params (`key`, `token`, `secret`, `auth`, `sig`, `password`) are `<redacted>` (`rpc::Scrubber::for_api`); ids in the query stay intact |
//! | Audit | one egress record per request (host + path, no query) |
//! | Retry | none (callers cache; `rpc::read_error` maps the error to a `ReadError`) |

// Called by the Solana tool family (`tools/solana/*`), wired in the next stage.
#![allow(dead_code)]

use std::time::Instant;

use anyhow::Result;
use reqwest::Url;
use serde_json::{json, Value};

use super::rpc::{
    display_url, http_status_error, reqwest_error, retry_after_ms, RpcError, Scrubber,
    REQUEST_TIMEOUT,
};
use crate::adapters::outbound::egress;
use crate::domain::observation::ErrorClass;
use crate::ports::tool::ToolCtx;

/// GET `url` and parse the JSON body (see the module table).
pub(crate) async fn fetch_json(ctx: &ToolCtx<'_>, url: &str) -> Result<(u16, Value)> {
    let url = Url::parse(url)
        .map_err(|e| RpcError::new(ErrorClass::Fatal, format!("invalid URL: {e}")))?;
    let host = url.host_str().unwrap_or("").to_string();
    let scrub = Scrubber::for_api(&url);
    let shown = display_url(&url);
    let audit = |outcome: std::result::Result<u16, &RpcError>, ms: Option<u64>| {
        let mut event = json!({
            "tool": "fetch_json",
            "host": host,
            "path": url.path(),
            "ms": ms,
        });
        match outcome {
            Ok(status) => {
                event["verdict"] = "allowed".into();
                event["status"] = status.into();
            }
            Err(e) => {
                event["verdict"] = if ms.is_some() { "error" } else { "denied" }.into();
                event["reason"] = e.message.clone().into();
                if let Some(s) = e.http_status {
                    event["status"] = s.into();
                }
            }
        }
        egress::policy().audit(event);
    };

    let gate = egress::policy()
        .check_url(&url)
        .and_then(|_| ctx.scope.check_net_host(&host));
    if let Err(e) = gate {
        let err = RpcError::new(ErrorClass::Fatal, scrub.scrub(&format!("{e:#}")));
        audit(Err(&err), None);
        return Err(err.into());
    }

    let started = Instant::now();
    let sent = ctx
        .http
        .get(url.clone())
        .timeout(REQUEST_TIMEOUT)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await;
    let result: std::result::Result<(u16, Value), RpcError> = match sent {
        Err(e) => Err(reqwest_error(e, &host, &scrub)),
        Ok(resp) => {
            let status = resp.status().as_u16();
            let retry_after = retry_after_ms(resp.headers());
            match resp.text().await {
                Err(e) => Err(reqwest_error(e, &host, &scrub)),
                Ok(body) if !(200..300).contains(&status) => {
                    Err(http_status_error(status, retry_after, &body, &host, &scrub))
                }
                Ok(body) => serde_json::from_str::<Value>(&body)
                    .map(|v| (status, v))
                    .map_err(|e| {
                        let mut err = RpcError::new(
                            ErrorClass::Decode,
                            scrub.scrub(&format!("{shown} returned a non-JSON body: {e}")),
                        );
                        err.http_status = Some(status);
                        err
                    }),
            }
        }
    };
    let ms = Some(started.elapsed().as_millis() as u64);
    match result {
        Ok((status, v)) => {
            audit(Ok(status), ms);
            Ok((status, v))
        }
        Err(e) => {
            audit(Err(&e), ms);
            Err(e.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::outbound::solana::rpc::read_error;
    use crate::adapters::outbound::solana::rpc::tests::{
        canned, local_scope, serve, test_client, Canned,
    };
    use crate::adapters::outbound::tools::workspace::test_support::TestHarness;
    use crate::domain::scope::ToolScope;

    /// Public-API errors may show the URL, never a credential value.
    fn assert_no_secret(e: &anyhow::Error) {
        let text = format!("{e:#} {e:?}");
        assert!(!text.contains("SECRET"), "secret leaked: {text}");
    }

    struct Harness(TestHarness);

    impl Harness {
        fn new(scope: ToolScope) -> Self {
            let mut h = TestHarness::with_scope(&std::env::temp_dir(), scope);
            h.http = test_client();
            Self(h)
        }
        fn ctx(&self) -> ToolCtx<'_> {
            self.0.ctx()
        }
    }

    #[test]
    fn display_url_redacts_credential_params() {
        let u = Url::parse(
            "https://hermes.pyth.network/v2/updates/price/latest?ids[]=0xef&api-key=SECRETKEY1&Token=abc",
        )
        .unwrap();
        let s = display_url(&u);
        assert!(!s.contains("SECRETKEY1") && !s.contains("abc"), "{s}");
        assert!(s.contains("ids[]=0xef"), "{s}");
        assert!(s.starts_with("https://hermes.pyth.network/v2/updates/price/latest?"));
    }

    #[tokio::test]
    async fn fetch_json_success_and_error_classes() {
        let (base, seen) = serve(vec![
            canned(
                200,
                r#"{"So11111111111111111111111111111111111111112":{"usdPrice":150.5}}"#,
            ),
            canned(401, r#"{"message":"Unauthorized SECRETKEY9876543"}"#),
            Canned {
                status: 429,
                headers: "Retry-After: 3\r\n",
                body: "slow down".into(),
                delay_ms: 0,
            },
            canned(502, "bad gateway"),
            canned(200, "<html>oops</html>"),
            canned(404, "not found"),
        ])
        .await;
        let h = Harness::new(local_scope());
        let url = format!("{base}/price/v3?ids=So11111111111111111111111111111111111111112&api-key=SECRETKEY9876543");
        let (status, v) = fetch_json(&h.ctx(), &url).await.unwrap();
        assert_eq!(status, 200);
        assert_eq!(
            v["So11111111111111111111111111111111111111112"]["usdPrice"],
            150.5
        );
        let req = seen.lock().unwrap()[0].clone();
        assert!(req.starts_with("GET /price/v3?ids="), "{req}");
        assert!(
            req.to_ascii_lowercase()
                .contains("accept: application/json"),
            "{req}"
        );

        let classes = [
            (ErrorClass::AuthRequired, None),
            (ErrorClass::RateLimited, Some(3_000)),
            (ErrorClass::Transient, None),
            (ErrorClass::Decode, None),
            (ErrorClass::Fatal, None),
        ];
        for (class, retry_after) in classes {
            let e = fetch_json(&h.ctx(), &url).await.unwrap_err();
            let r = read_error("price", &e);
            assert_eq!((r.class, r.retry_after_ms), (class, retry_after), "{e}");
            assert_no_secret(&e);
            if class == ErrorClass::Decode {
                // Ids in the query are rendered in full.
                assert!(
                    r.message.contains(
                        "ids=So11111111111111111111111111111111111111112&api-key=<redacted>"
                    ),
                    "{}",
                    r.message
                );
            }
        }
        assert_eq!(seen.lock().unwrap().len(), 6, "no retries");
    }

    #[tokio::test]
    async fn fetch_json_gate_and_timeout() {
        // Scope without the host: refused before any request.
        let h = Harness::new(ToolScope::default());
        let e = fetch_json(&h.ctx(), "http://127.0.0.1:9/x?api-key=SECRETKEY9876543")
            .await
            .unwrap_err();
        assert_eq!(read_error("x", &e).class, ErrorClass::Fatal);
        assert!(format!("{e}").contains("net_hosts"), "{e}");
        assert_no_secret(&e);
        // Invalid URL.
        let e = fetch_json(&h.ctx(), "not a url").await.unwrap_err();
        assert_eq!(read_error("x", &e).class, ErrorClass::Fatal);
        // Unreachable port → Transient (connect error), no URL in the text.
        let h = Harness::new(local_scope());
        let e = fetch_json(&h.ctx(), "http://127.0.0.1:9/x?api-key=SECRETKEY9876543")
            .await
            .unwrap_err();
        assert_eq!(read_error("x", &e).class, ErrorClass::Transient);
        assert_no_secret(&e);
    }

    #[tokio::test]
    #[ignore]
    async fn live_solcore_fetch_json_jupiter() {
        let scope = ToolScope {
            net_hosts: vec!["lite-api.jup.ag".into()],
            ..Default::default()
        };
        let mut h = Harness::new(scope);
        h.0.http = egress::policy().tool_client(REQUEST_TIMEOUT).unwrap();
        let (status, v) = fetch_json(
            &h.ctx(),
            "https://lite-api.jup.ag/price/v3?ids=So11111111111111111111111111111111111111112",
        )
        .await
        .unwrap();
        assert_eq!(status, 200);
        let usd = v["So11111111111111111111111111111111111111112"]["usdPrice"]
            .as_f64()
            .unwrap();
        assert!(usd > 1.0, "{v}");
    }
}
