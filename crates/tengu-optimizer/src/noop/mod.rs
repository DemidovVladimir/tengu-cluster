//! No-op refiner implementation.
//!
//! Potential use case:
//! Disable optimization to preserve exact user wording during debugging or evaluations.

use async_trait::async_trait;
use tengu_core::Refiner;

/// Refiner that returns input unchanged.
pub struct NoopRefiner;

#[async_trait]
impl Refiner for NoopRefiner {
    async fn compress(&self, input: &str) -> anyhow::Result<String> {
        Ok(input.to_string())
    }

    async fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
        Ok(vec![])
    }

    async fn summarize(&self, content: &str, _max_tokens: u32) -> anyhow::Result<String> {
        Ok(content.to_string())
    }

    fn memory_footprint(&self) -> usize {
        0
    }
}
