//! Shared types for memory retrieval results.

use serde::{Deserialize, Serialize};

/// Pinned block produced by `MemoryInjector::for_turn`, appended to the
/// user-turn message at API-call time. Never persisted in message
/// history.
#[derive(Debug, Clone, Default)]
pub struct PinnedMemoryBlock {
    pub body: String,
}

impl PinnedMemoryBlock {
    pub fn is_empty(&self) -> bool {
        self.body.trim().is_empty()
    }
}

/// Metadata attached to ingested chunks. Stored alongside the embedding
/// for retrieval-time filtering.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChunkMetadata {
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub timestamp_utc: Option<String>,
    #[serde(default, flatten)]
    pub extra: std::collections::HashMap<String, serde_json::Value>,
}

/// Single search hit returned by `MemoryProvider::prefetch` underlying
/// backends. Rendered into the `PinnedMemoryBlock.body` by the provider.
#[derive(Debug, Clone)]
pub struct MemoryHit {
    pub text: String,
    pub score: f32,
    pub metadata: ChunkMetadata,
}
