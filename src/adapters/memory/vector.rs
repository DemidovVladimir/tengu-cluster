//! Vector backend for memory retrieval and ingestion.
//!
//! Two swappable stores, selected by config:
//! - `disk`   — bincode file at `<workspace>/.tengu/memory.bin`
//! - `qdrant` — REST client against a Qdrant instance
//!
//! Both expose the `VectorStore` trait below.

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

    /// Phase 6.3 — delete every entry whose payload's `field` (a numeric
    /// extras key like `"extra_rag_created_at"`) is strictly less than
    /// `cutoff`. Returns the number of entries deleted (0 when nothing
    /// matched). Implementations that can't push the filter down to the
    /// store should return `Ok(0)` and log a debug line — callers must
    /// not treat `0` as failure.
    ///
    /// The default impl is the "can't" path so each backend opts in
    /// explicitly. Today Qdrant overrides; the disk store leaves the
    /// default in place (filter-based delete on a flat bincode file
    /// would mean a full rewrite — a separate cleanup story).
    async fn delete_older_than(&self, _field: &str, _cutoff: f64) -> Result<u64> {
        tracing::debug!(
            "VectorStore::delete_older_than: backend has no filter-based delete; \
             returning 0 (callers fall through to no-op)"
        );
        Ok(0)
    }
}

pub mod disk;
pub mod embedder;
#[cfg(feature = "qdrant")]
pub mod qdrant;

pub use disk::DiskVectorStore;
pub use embedder::Embedder;
#[cfg(feature = "qdrant")]
pub use qdrant::QdrantVectorStore;
