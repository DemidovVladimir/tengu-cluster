//! Memory ports — `MemoryProvider` (harness-level memory backends driven by
//! the injector/writer) and `VectorStore` (embedding store behind the
//! workspace memory tools; impl: `DiskVectorStore`).

use anyhow::Result;
use async_trait::async_trait;
use std::path::Path;

use crate::domain::memory::{ChunkMetadata, MemoryHit};

// Several lifecycle methods on this trait (`is_available`, `initialize`,
// `system_prompt_block`, `shutdown`) are unused on the v2 RagPlanner /
// SubprocessRunner path — subagents persist step summaries to Postgres `agentic_memory` themselves (`main.rs::try_persist_agentic_step_summary`)
// rather than going through `MemoryProvider`. The trait is kept intact so
// the static-mode path keeps compiling; Phase 7.1 will collapse this whole
// hierarchy once the static path is deleted.
#[allow(dead_code)]
#[async_trait]
pub trait MemoryProvider: Send + Sync {
    /// Short identifier (`"builtin"`, `"letta"`, …).
    fn name(&self) -> &str;

    /// Return `true` if configuration/credentials are present and the
    /// provider should activate. Synchronous, no network calls.
    fn is_available(&self) -> bool;

    /// One-time init at session start. `workspace` is the agent's
    /// workspace root. Implementors may open databases, spawn background
    /// tasks, etc.
    async fn initialize(&self, session_id: &str, workspace: &Path) -> anyhow::Result<()>;

    /// Static system-prompt contribution (AGENTS.md, identity files,
    /// daily logs for the builtin; empty by default for external
    /// providers).
    fn system_prompt_block(&self) -> String {
        String::new()
    }

    /// Called pre-turn. Implementors do vector search (or equivalent)
    /// against `query` and return formatted text to inject. Empty
    /// string means "nothing relevant."
    async fn prefetch(&self, agent: &str, query: &str) -> String;

    /// Called post-turn. Implementors persist the (user, assistant)
    /// summary. MUST be non-blocking or cheap — the user-facing reply
    /// never waits on this.
    async fn sync_turn(&self, agent: &str, user: &str, assistant: &str);

    /// Clean teardown — flush queues, close connections.
    async fn shutdown(&self);
}

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

/// Text → vector. Impl: `adapters::outbound::memory::embedder::Embedder`.
#[async_trait]
pub trait Embedding: Send + Sync {
    async fn embed(&self, text: &str) -> Result<Vec<f32>>;
    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;
}

/// What memory tools (`memory_ingest`, `memory_search`, `persistent_store`)
/// need from the memory subsystem. Impl: `application::memory::manager::MemoryManager`.
#[async_trait]
pub trait MemoryService: Send + Sync {
    /// Embed + store one entry; returns its id.
    async fn ingest_one(&self, text: &str, agent: &str, metadata: ChunkMetadata) -> Result<String>;
    /// Embed + store several entries with one embedding call; returns the count.
    async fn ingest_batch(
        &self,
        texts: &[&str],
        agent: &str,
        metadata: ChunkMetadata,
    ) -> Result<usize>;
    /// Embed `query` and return the `top_k` nearest entries.
    async fn search(
        &self,
        query: &str,
        top_k: usize,
        filter: Option<&ChunkMetadata>,
    ) -> Result<Vec<MemoryHit>>;
    /// Remove one entry; `true` if it existed.
    async fn delete_entry(&self, id: &str) -> Result<bool>;
}
