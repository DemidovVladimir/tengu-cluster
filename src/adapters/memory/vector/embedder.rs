//! Text embedding client.
//!
//! Ported from `src/adapters/embedding.rs::OpenRouterEmbeddingAdapter` with a
//! simpler single-string `embed()` API. Calls
//! `POST https://openrouter.ai/api/v1/embeddings` (OpenAI-compatible) with the
//! configured model (default: `text-embedding-3-small`, 1536 dims).
//!
//! A `null()` test helper returns a zero-vector without making network calls.

use anyhow::{Context, Result};

/// Production default dimensionality (`text-embedding-3-small`). Used by
/// `Embedder::null()` so test vectors match what the real backend returns.
#[allow(dead_code)]
pub const DEFAULT_DIM: usize = 1536;

enum Mode {
    Real {
        client: reqwest::Client,
        api_key: String,
        model: String,
    },
    /// Test mode — `embed()` returns `Ok(vec![0.0; DEFAULT_DIM])` without
    /// making any network calls.
    #[cfg(test)]
    Null,
}

/// OpenRouter embeddings client. Single-text `embed()` entry point; the
/// internal `Mode` enum distinguishes real-HTTP from test-null mode.
pub struct Embedder {
    mode: Mode,
}

impl Embedder {
    /// Construct a real HTTP-backed embedder. Requires a valid OpenRouter
    /// API key; the `model` string is passed through verbatim (e.g.
    /// `"text-embedding-3-small"`).
    pub fn new(api_key: String, model: String) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(30))
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .unwrap_or_default();
        Self {
            mode: Mode::Real {
                client,
                api_key,
                model,
            },
        }
    }

    /// Embed a single text. Returns a `Vec<f32>` of model-dependent length
    /// (1536 for `text-embedding-3-small`).
    ///
    /// Internally a thin wrapper over `embed_batch(&[text])` — prefer
    /// `embed_batch` when you have >1 text to embed (single HTTP round-trip).
    pub async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let mut out = self.embed_batch(&[text]).await?;
        out.pop()
            .ok_or_else(|| anyhow::anyhow!("embedder returned no vectors for single-text call"))
    }

    /// Embed multiple texts in a single HTTP request. Returns a vector of
    /// embeddings in the same order as `texts`.
    ///
    /// Empty input short-circuits to `Ok(vec![])`.
    pub async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        match &self.mode {
            Mode::Real {
                client,
                api_key,
                model,
            } => embed_batch_openrouter(client, api_key, model, texts).await,
            #[cfg(test)]
            Mode::Null => Ok(texts.iter().map(|_| vec![0.0; DEFAULT_DIM]).collect()),
        }
    }

    /// Test helper: returns an `Embedder` whose `embed()` always returns
    /// `Ok(vec![0.0; DEFAULT_DIM])`. No network calls.
    #[cfg(test)]
    pub fn null() -> Self {
        Self { mode: Mode::Null }
    }
}

async fn embed_batch_openrouter(
    client: &reqwest::Client,
    api_key: &str,
    model: &str,
    texts: &[&str],
) -> Result<Vec<Vec<f32>>> {
    let body = serde_json::json!({
        "model": model,
        "input": texts,
    });

    let resp = client
        .post("https://openrouter.ai/api/v1/embeddings")
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .context("embedding request failed")?;

    let status = resp.status();
    if !status.is_success() {
        let error_body = resp.text().await.unwrap_or_default();
        anyhow::bail!("embedding API returned {}: {}", status, error_body);
    }

    let json: serde_json::Value = resp
        .json()
        .await
        .context("failed to parse embedding response")?;
    let data = json["data"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("missing 'data' array in embedding response"))?;

    if data.len() != texts.len() {
        anyhow::bail!(
            "embedding API returned {} vectors for {} inputs",
            data.len(),
            texts.len()
        );
    }

    let mut out = Vec::with_capacity(texts.len());
    for item in data {
        let embedding = item["embedding"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("missing 'embedding' array in response item"))?
            .iter()
            .map(|v| v.as_f64().unwrap_or(0.0) as f32)
            .collect::<Vec<f32>>();
        out.push(embedding);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn null_returns_expected_dim() {
        let e = Embedder::null();
        let v = e.embed("any text").await.unwrap();
        assert_eq!(v.len(), DEFAULT_DIM);
        assert!(v.iter().all(|x| *x == 0.0));
    }

    #[tokio::test]
    async fn null_batch_returns_one_vector_per_input() {
        let e = Embedder::null();
        let v = e.embed_batch(&["a", "b", "c"]).await.unwrap();
        assert_eq!(v.len(), 3);
        for emb in &v {
            assert_eq!(emb.len(), DEFAULT_DIM);
        }
    }

    #[tokio::test]
    async fn empty_batch_short_circuits() {
        let e = Embedder::null();
        let v = e.embed_batch(&[]).await.unwrap();
        assert!(v.is_empty());
    }
}
