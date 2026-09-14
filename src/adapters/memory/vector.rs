//! Vector backend for the in-process memory tools (`remember`,
//! `persistent_store`, `memory_ingest` / `memory_search`).
//!
//! | Piece | Status |
//! |---|---|
//! | `VectorStore` trait (below) | port; one impl |
//! | `disk::DiskVectorStore` | the only impl — bincode file at `<workspace>/.tengu/memory.bin` |
//! | `embedder::Embedder` | OpenRouter embeddings client; model = `embedder::DEFAULT_EMBEDDING_MODEL` |
//! | Qdrant impl | removed Phase 6 (2026-05-14); `memory_config.backend` is no longer branched on |
//!
//! Durable runtime memory (planner recall, step summaries, wiki) is NOT here —
//! it is the Postgres `agentic_memory` plugin (`plugins/agentic_memory/`,
//! feature `postgres_memory`).

use anyhow::Result;
use async_trait::async_trait;

use crate::adapters::memory::context_block::{ChunkMetadata, MemoryHit};

#[async_trait]
pub trait VectorStore: Send + Sync {
    /// Append a new entry. The returned identifier is stable for the lifetime
    /// of the store — callers may later pass it to `delete`. For stores that
    /// don't naturally have row IDs (e.g. bincode with no primary key),
    /// implementations must synthesize a UUID/index-based ID.
    async fn write(
        &self,
        embedding: Vec<f32>,
        text: &str,
        metadata: ChunkMetadata,
    ) -> Result<String>;

    async fn search(
        &self,
        embedding: &[f32],
        top_k: usize,
        filter: Option<&ChunkMetadata>,
    ) -> Result<Vec<MemoryHit>>;

    /// Remove a single entry by its `id`. Returns `true` if the id existed
    /// and was removed, `false` if no entry matched. Implementations that
    /// don't support deletion (e.g. append-only adapters) can return
    /// `Ok(false)` uniformly — callers must not treat `false` as an error.
    async fn delete(&self, id: &str) -> Result<bool>;

    /// Purge every entry in the store. Used by channel `/purge` commands.
    async fn clear_all(&self) -> Result<()>;

    /// Count of entries currently in the store. Used by TUI status lines
    /// and ops dashboards.
    async fn entry_count(&self) -> Result<usize>;

    /// Approximate on-disk (or over-the-wire) size in bytes. Implementations
    /// that can't cheaply compute this may return `0`.
    async fn storage_bytes(&self) -> Result<u64>;
}

pub mod disk;
pub mod embedder;

pub use disk::DiskVectorStore;
pub use embedder::Embedder;
