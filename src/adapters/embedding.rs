//! OpenRouter embedding adapter implementing EmbeddingPort.

use crate::application::ports::EmbeddingPort;
use anyhow::{Context, Result};
use std::future::Future;
use std::pin::Pin;

/// Adapter that calls the OpenRouter embeddings API.
pub(crate) struct OpenRouterEmbeddingAdapter {
    client: reqwest::Client,
    api_key: String,
    model: String,
}

impl OpenRouterEmbeddingAdapter {
    pub(crate) fn new(api_key: String, model: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            model,
        }
    }
}

impl EmbeddingPort for OpenRouterEmbeddingAdapter {
    fn embed(
        &self,
        texts: &[&str],
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Vec<f32>>>> + Send + '_>> {
        let input: Vec<String> = texts.iter().map(|s| s.to_string()).collect();
        Box::pin(async move {
            let body = serde_json::json!({
                "model": self.model,
                "input": input,
            });

            let resp = self
                .client
                .post("https://openrouter.ai/api/v1/embeddings")
                .header("Authorization", format!("Bearer {}", self.api_key))
                .header("Content-Type", "application/json")
                .json(&body)
                .send()
                .await
                .context("embedding request failed")?;

            let status = resp.status();
            if !status.is_success() {
                let error_body = resp.text().await.unwrap_or_default();
                anyhow::bail!(
                    "embedding API returned {}: {}",
                    status,
                    error_body
                );
            }

            let json: serde_json::Value = resp.json().await.context("failed to parse embedding response")?;
            let data = json["data"]
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("missing 'data' array in embedding response"))?;

            let mut embeddings = Vec::with_capacity(data.len());
            for item in data {
                let embedding = item["embedding"]
                    .as_array()
                    .ok_or_else(|| anyhow::anyhow!("missing 'embedding' array in response item"))?
                    .iter()
                    .map(|v| v.as_f64().unwrap_or(0.0) as f32)
                    .collect::<Vec<f32>>();
                embeddings.push(embedding);
            }

            Ok(embeddings)
        })
    }
}
