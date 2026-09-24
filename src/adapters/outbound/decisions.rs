//! `JevClient` — `DecisionEngine` over OpenRouter's decisions endpoint
//! (`POST {OPENROUTER_BASE_URL}/alpha/decisions`, default base
//! `https://openrouter.ai/api`). Serves System One models such as
//! `~typesafe/jev-latest`; they return typed answers, never text.
//!
//! Traffic goes through `egress::llm_api_client` (proxied iff
//! `[egress] route_llm_api`), same as the OpenRouter chat engine.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::domain::decision::{Decision, Question};
use crate::ports::decision::DecisionEngine;

pub(crate) struct JevClient {
    client: reqwest::Client,
    url: String,
    api_key: String,
    model: String,
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
        Ok(Self {
            client,
            url: format!("{}/alpha/decisions", base.trim_end_matches('/')),
            api_key,
            model: model.to_string(),
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
        let resp = self
            .client
            .post(&self.url)
            .bearer_auth(&self.api_key)
            .json(&request_body(&self.model, state, questions))
            .send()
            .await
            .context("decisions request failed")?;
        let status = resp.status();
        let text = resp.text().await.context("decisions response body")?;
        if !status.is_success() {
            return Err(anyhow!(
                "decisions endpoint HTTP {}: {}",
                status.as_u16(),
                text
            ));
        }
        serde_json::from_str(&text)
            .with_context(|| format!("decisions response not parseable: {text}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
