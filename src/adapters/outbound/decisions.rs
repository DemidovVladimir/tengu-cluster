//! `JevClient` — `DecisionEngine` over OpenRouter's decisions endpoint
//! (`POST {OPENROUTER_BASE_URL}/alpha/decisions`, default base
//! `https://openrouter.ai/api`). Serves System One models such as
//! `~typesafe/jev-latest`; they return typed answers, never text.
//!
//! Traffic goes through `egress::llm_api_client` (proxied iff
//! `[egress] route_llm_api`), same as the OpenRouter chat engine.
//!
//! | Failure | Handling |
//! |---|---|
//! | HTTP 429 / 5xx, connect error | one retry after `domain::backoff::next_delay` (0.5–1 s jitter; `Retry-After` honoured up to 5 s — longer ⇒ no retry) |
//! | timeout, other 4xx, quota text, unparseable answer | no retry (a timed-out call may have been billed) |
//! | 3 consecutive failed `decide` calls | circuit open 30 s: calls fail fast without a request; then calls pass again (the next failure re-opens it, a success closes it) |

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::adapters::outbound::http_class::{classify_http_status, retry_after_ms};
use crate::adapters::outbound::rate_limit::{jitter01, mono_ms};
use crate::domain::backoff::{next_delay, BackoffPolicy, CircuitBreaker, Delay};
use crate::domain::decision::{Decision, Question};
use crate::domain::observation::ErrorClass;
use crate::ports::decision::DecisionEngine;

/// One retry, 0.5–1 s jitter, `Retry-After` up to 5 s.
const RETRY: BackoffPolicy = BackoffPolicy {
    base_ms: 1_000,
    cap_ms: 5_000,
    min_ms: 500,
    max_attempts: 1,
    quota_park_ms: 60_000,
};
const BREAKER_THRESHOLD: u32 = 3;
const BREAKER_COOLDOWN_MS: u64 = 30_000;

pub(crate) struct JevClient {
    client: reqwest::Client,
    url: String,
    api_key: String,
    model: String,
    retry: BackoffPolicy,
    breaker: Mutex<CircuitBreaker>,
}

/// A failed attempt: the class decides the retry, `error` is what callers see.
struct Failure {
    class: ErrorClass,
    retry_after_ms: Option<u64>,
    error: anyhow::Error,
}

impl JevClient {
    /// Build from `OPENROUTER_API_KEY` / `OPENROUTER_BASE_URL`.
    pub(crate) fn from_env(model: &str, timeout: Duration) -> Result<Self> {
        let api_key = std::env::var("OPENROUTER_API_KEY")
            .map_err(|_| anyhow!("OPENROUTER_API_KEY is required for the decision model"))?;
        let base = std::env::var("OPENROUTER_BASE_URL")
            .unwrap_or_else(|_| "https://openrouter.ai/api".to_string());
        let client = crate::adapters::outbound::egress::policy().llm_api_client(
            reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(timeout),
        )?;
        Ok(Self::with_parts(
            client,
            format!("{}/alpha/decisions", base.trim_end_matches('/')),
            api_key,
            model,
        ))
    }

    fn with_parts(client: reqwest::Client, url: String, api_key: String, model: &str) -> Self {
        Self {
            client,
            url,
            api_key,
            model: model.to_string(),
            retry: RETRY,
            breaker: Mutex::new(CircuitBreaker::new(BREAKER_THRESHOLD, BREAKER_COOLDOWN_MS)),
        }
    }

    fn breaker(&self) -> MutexGuard<'_, CircuitBreaker> {
        self.breaker.lock().unwrap_or_else(|p| p.into_inner())
    }

    async fn post_once(&self, body: &Value) -> std::result::Result<Decision, Failure> {
        let resp = self
            .client
            .post(&self.url)
            .bearer_auth(&self.api_key)
            .json(body)
            .send()
            .await
            .map_err(|e| Failure {
                class: if e.is_timeout() {
                    ErrorClass::Timeout
                } else {
                    ErrorClass::Transient
                },
                retry_after_ms: None,
                error: anyhow::Error::from(e).context("decisions request failed"),
            })?;
        let status = resp.status();
        let retry_after = retry_after_ms(resp.headers());
        // The server answered: an unreadable body is never retried.
        let text = resp.text().await.map_err(|e| Failure {
            class: if e.is_timeout() {
                ErrorClass::Timeout
            } else {
                ErrorClass::Decode
            },
            retry_after_ms: None,
            error: anyhow::Error::from(e).context("decisions response body"),
        })?;
        if !status.is_success() {
            return Err(Failure {
                class: classify_http_status(status.as_u16(), &text),
                retry_after_ms: retry_after,
                error: anyhow!("decisions endpoint HTTP {}: {}", status.as_u16(), text),
            });
        }
        serde_json::from_str(&text).map_err(|e| Failure {
            class: ErrorClass::Decode,
            retry_after_ms: None,
            error: anyhow::Error::from(e)
                .context(format!("decisions response not parseable: {text}")),
        })
    }
}

/// Request body — split out so tests can pin the wire shape.
fn request_body(model: &str, state: &Value, questions: &BTreeMap<String, Question>) -> Value {
    json!({ "model": model, "state": state, "questions": questions })
}

#[async_trait]
impl DecisionEngine for JevClient {
    fn model(&self) -> &str {
        &self.model
    }

    async fn decide(
        &self,
        state: &Value,
        questions: &BTreeMap<String, Question>,
    ) -> Result<Decision> {
        let open = self.breaker().allow(mono_ms());
        if let Err(wait_ms) = open {
            let failures = self.breaker().failures();
            return Err(anyhow!(
                "decisions endpoint circuit open after {failures} consecutive failures; next try in {wait_ms} ms"
            ));
        }
        let body = request_body(&self.model, state, questions);
        let mut attempt = 0u32;
        let outcome = loop {
            attempt += 1;
            let failure = match self.post_once(&body).await {
                Ok(d) => break Ok(d),
                Err(f) => f,
            };
            let delay = match failure.class {
                ErrorClass::RateLimited | ErrorClass::Transient => next_delay(
                    failure.class,
                    attempt,
                    failure.retry_after_ms,
                    &self.retry,
                    jitter01(),
                ),
                _ => Delay::Stop,
            };
            match delay {
                Delay::Retry(ms) => {
                    tracing::warn!(
                        model = %self.model,
                        class = failure.class.as_str(),
                        wait_ms = ms,
                        error = %format!("{:#}", failure.error),
                        "decisions call failed; retrying once"
                    );
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                }
                Delay::Park(_) | Delay::Stop => break Err(failure.error),
            }
        };
        {
            let mut b = self.breaker();
            match &outcome {
                Ok(_) => b.record_success(),
                Err(_) => b.record_failure(mono_ms()),
            }
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::outbound::http_class::test_support::{canned, serve, test_client, Canned};

    #[test]
    fn request_body_matches_probed_shape() {
        let q = BTreeMap::from([(
            "next_action".to_string(),
            Question::Choice {
                instructions: "pick".into(),
                criteria: BTreeMap::from([("hold".into(), "do nothing".into())]),
            },
        )]);
        let body = request_body("~typesafe/jev-latest", &json!({"price": 1}), &q);
        assert_eq!(
            body,
            json!({
                "model": "~typesafe/jev-latest",
                "state": {"price": 1},
                "questions": {"next_action": {"type": "choice", "instructions": "pick", "criteria": {"hold": "do nothing"}}}
            })
        );
    }

    const DECISION: &str = r#"{"id":"d1","model":"~typesafe/jev-latest","answers":{"next_action":{"type":"choice","choice":"hold","probabilities":{"hold":0.9,"act":0.1}}},"usage":{"input_tokens":12,"output_tokens":0}}"#;

    fn client(base: &str) -> JevClient {
        let mut c = JevClient::with_parts(
            test_client(),
            format!("{base}/alpha/decisions"),
            "sk-test".into(),
            "~typesafe/jev-latest",
        );
        c.retry = BackoffPolicy {
            base_ms: 1,
            cap_ms: 50,
            min_ms: 1,
            ..RETRY
        };
        c
    }

    fn questions() -> BTreeMap<String, Question> {
        BTreeMap::from([(
            "next_action".to_string(),
            Question::Choice {
                instructions: "pick".into(),
                criteria: BTreeMap::from([("hold".into(), "do nothing".into())]),
            },
        )])
    }

    #[tokio::test]
    async fn retries_once_on_429_and_5xx() {
        let (base, seen) = serve(vec![
            Canned {
                status: 429,
                headers: "Retry-After: 0\r\n",
                body: "slow down".into(),
                delay_ms: 0,
            },
            canned(200, DECISION),
            canned(503, "unavailable"),
            canned(502, "bad gateway"),
        ])
        .await;
        let c = client(&base);
        let d = c.decide(&json!({"x": 1}), &questions()).await.unwrap();
        assert_eq!(d.answers["next_action"].choice.as_deref(), Some("hold"));
        assert_eq!(seen.lock().unwrap().len(), 2, "one retry after the 429");
        let first = seen.lock().unwrap()[0].clone();
        assert!(first.starts_with("POST /alpha/decisions"), "{first}");
        assert!(
            first
                .to_ascii_lowercase()
                .contains("authorization: bearer sk-test"),
            "{first}"
        );
        // 503 then 502: one retry, then the error (message unchanged).
        let e = c.decide(&json!({}), &questions()).await.unwrap_err();
        assert_eq!(format!("{e}"), "decisions endpoint HTTP 502: bad gateway");
        assert_eq!(seen.lock().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn no_retry_on_4xx_or_a_long_retry_after() {
        let (base, seen) = serve(vec![
            canned(400, r#"{"error":"bad questions"}"#),
            Canned {
                status: 429,
                headers: "Retry-After: 30\r\n",
                body: "later".into(),
                delay_ms: 0,
            },
            canned(200, "not json"),
        ])
        .await;
        let c = client(&base);
        let e = c.decide(&json!({}), &questions()).await.unwrap_err();
        assert!(format!("{e}").contains("HTTP 400"), "{e}");
        let e = c.decide(&json!({}), &questions()).await.unwrap_err();
        assert!(format!("{e}").contains("HTTP 429"), "{e}");
        let e = c.decide(&json!({}), &questions()).await.unwrap_err();
        assert!(format!("{e:#}").contains("not parseable"), "{e:#}");
        assert_eq!(seen.lock().unwrap().len(), 3, "no retries");
    }

    #[tokio::test]
    async fn breaker_opens_after_three_failures_and_closes_on_success() {
        let (base, seen) = serve(vec![
            canned(400, "a"),
            canned(400, "b"),
            canned(400, "c"),
            canned(200, DECISION),
        ])
        .await;
        let c = client(&base);
        for _ in 0..3 {
            c.decide(&json!({}), &questions()).await.unwrap_err();
        }
        let e = c.decide(&json!({}), &questions()).await.unwrap_err();
        assert!(
            format!("{e}").contains("circuit open after 3 consecutive failures"),
            "{e}"
        );
        assert_eq!(seen.lock().unwrap().len(), 3, "open circuit sends nothing");
        // Cooldown over (simulated): the trial call succeeds and closes it.
        *c.breaker() = {
            let mut b = CircuitBreaker::new(BREAKER_THRESHOLD, 0);
            for _ in 0..3 {
                b.record_failure(mono_ms());
            }
            b
        };
        c.decide(&json!({}), &questions()).await.unwrap();
        assert_eq!(c.breaker().failures(), 0);
    }

    /// Live call — costs ~$0.00002. `cargo test --bin tengu jev_live -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn jev_live_choice() {
        let client = JevClient::from_env("~typesafe/jev-latest", Duration::from_secs(20)).unwrap();
        let q = BTreeMap::from([(
            "next_action".to_string(),
            Question::Choice {
                instructions: "Price is below the LP range and falling. Next action?".into(),
                criteria: BTreeMap::from([
                    ("hold".into(), "Do nothing".into()),
                    ("rebalance".into(), "Move range to current price".into()),
                ]),
            },
        )]);
        let d = client
            .decide(&json!({"in_range": false, "trend": "falling"}), &q)
            .await
            .unwrap();
        assert!(d.answers["next_action"].choice.is_some());
    }
}
