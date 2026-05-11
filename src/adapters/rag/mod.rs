//! RAG facade — the **v2 high-level layer** that the planner uses.
//!
//! ## Where this sits in the stack
//!
//! ```text
//!  src/adapters/rag/      ← THIS MODULE. v2 facade. Owns three Qdrant
//!         │                   collections (registry / messages / outputs).
//!         │                   Indexer, query, cleanup, RagStore handle.
//!         ▼
//!  src/adapters/memory/   ← Low-level vector store + provider abstraction.
//!                             `memory/vector/qdrant.rs` is what we wrap.
//! ```
//!
//! `memory/` and `rag/` look like duplicates but they're layered. The
//! distinction: `memory/vector` exposes a generic `VectorStore` trait
//! (Qdrant or disk-backed bincode), while `rag/` is opinionated — it
//! knows there are exactly THREE collections, what each one stores, and
//! the auto-reindex / fingerprint / TTL / per-example-vector idioms that
//! make the planner work.
//!
//! ## API surface (the four things callers reach for)
//!
//! * [`RagStore::from_config`]      — constructs the facade against Qdrant.
//! * [`RagStore::store_memory`]     — write into the ephemeral collections.
//! * [`RagStore::search_registry`]  — semantic search over agents/skills/tools.
//! * [`RagStore::search_messages`] / [`RagStore::search_memory`] — read prior turns / step outputs.
//!
//! ## Three Qdrant collections
//!
//! | Collection         | Holds                                | Written by                                  | Read by                                   |
//! |--------------------|--------------------------------------|---------------------------------------------|-------------------------------------------|
//! | `tengu_registry`   | agent specs, skills, tool defs       | `indexer::reindex_all_workspace` (auto + CLI) | `RagPlanner::plan` for the ranked roster |
//! | `tengu_messages`   | user messages keyed by `session_id`  | `RagPlanner::persist_user_message`           | (forward-compat — `search_messages`)     |
//! | `tengu_outputs`    | step output summaries                | `compress_and_store` tool                    | `RagPlanner::replan` for cross-plan recall |
//!
//! The module is gated behind the `qdrant` cargo feature so builds without
//! Qdrant still succeed. Consumers reach this via the `tengu registry`
//! CLI subcommand or via `RagPlanner` (the only `Planner` impl since
//! Phase 7.1).
//!
//! ## File map
//!
//! - `mod.rs`     — `RagStore` facade, the three `MemoryEntry`/`MemoryKind` types.
//! - `indexer.rs` — `enumerate_builtin_tools`, `enumerate_mcp_tools`, `index_*`,
//!                  `reindex_all_workspace`, fingerprint dedup (Phase 6.2).
//! - `query.rs`   — `search_registry` (with dedup), `search_memory`, `search_messages`.
//! - `cleanup.rs` — `ttl_cleanup` (Phase 6.3 — Qdrant filter-based delete).

#![cfg(feature = "qdrant")]
// Many items in this module are "staging APIs" for Phase 4 of the redesign —
// they exist (RagStore::messages/outputs/cfg, MemoryKind variants, store_memory)
// so Phase 4 wiring is a pure-compile addition, not a new-code addition. Silence
// dead_code at the module level until Phase 4 lights them up. Removing this
// allow-list in Phase 4 is a quick signal that the wire-up was complete.
#![allow(dead_code)]

pub mod cleanup;
pub mod indexer;
pub mod query;

use anyhow::{Context, Result};
use std::sync::Arc;

use crate::adapters::config::MemoryConfig;
use crate::adapters::memory::context_block::ChunkMetadata;
use crate::adapters::memory::vector::embedder::Embedder;
use crate::adapters::memory::vector::{QdrantVectorStore, VectorStore};

/// Canonical collection names used by the v2 RAG layer.
pub const REGISTRY_COLLECTION: &str = "tengu_registry";
pub const MESSAGES_COLLECTION: &str = "tengu_messages";
pub const OUTPUTS_COLLECTION: &str = "tengu_outputs";

/// Categorises entries stored in `tengu_registry`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RagKind {
    Skill,
    Agent,
    Tool,
}

impl RagKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            RagKind::Skill => "skill",
            RagKind::Agent => "agent",
            RagKind::Tool => "tool",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "skill" => Some(Self::Skill),
            "agent" => Some(Self::Agent),
            "tool" => Some(Self::Tool),
            _ => None,
        }
    }
}

/// Kinds of entries stored in the ephemeral memory collections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryKind {
    Message,
    StepOutput,
    FileChunk,
}

impl MemoryKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            MemoryKind::Message => "message",
            MemoryKind::StepOutput => "step_output",
            MemoryKind::FileChunk => "file_chunk",
        }
    }

    /// Which Qdrant collection this kind is written to.
    pub fn collection(&self) -> &'static str {
        match self {
            MemoryKind::Message => MESSAGES_COLLECTION,
            MemoryKind::StepOutput | MemoryKind::FileChunk => OUTPUTS_COLLECTION,
        }
    }
}

/// One hit from a RAG registry search.
#[derive(Debug, Clone)]
pub struct RagResult {
    pub kind: RagKind,
    pub name: String,
    pub description: String,
    pub score: f32,
    pub source_path: Option<String>,
}

/// One skill discovered by the Phase 2 scanner. Carries just the fields the
/// registry needs: `name`, `description`, and the `source_path` used for
/// shadowing across the three tiers (managed / workspace dotdir / workspace root).
#[derive(Debug, Clone)]
pub struct SkillEntry {
    pub name: String,
    pub description: String,
    pub source_path: std::path::PathBuf,
}

/// One entry to store into an ephemeral memory collection.
#[derive(Debug, Clone)]
pub struct MemoryEntry {
    pub kind: MemoryKind,
    pub session_id: String,
    pub step_id: Option<String>,
    pub content: String,
    pub created_at: u64,
}

/// Central RAG facade. Holds one [`Embedder`] and three [`VectorStore`]
/// instances (one per collection). Each method is a thin translator over
/// the existing `memory/` types.
pub struct RagStore {
    embedder: Arc<Embedder>,
    registry: Arc<dyn VectorStore>,
    messages: Arc<dyn VectorStore>,
    outputs: Arc<dyn VectorStore>,
    cfg: MemoryConfig,
}

impl RagStore {
    /// Construct a `RagStore` from the existing [`MemoryConfig`]. Fails if
    /// `OPENROUTER_API_KEY` is missing or any of the three collections
    /// cannot be created.
    pub async fn from_config(cfg: MemoryConfig) -> Result<Self> {
        let api_key = std::env::var("OPENROUTER_API_KEY")
            .context("OPENROUTER_API_KEY must be set for RAG indexer (embedding provider)")?;
        let embedder = Arc::new(Embedder::new(api_key, cfg.embedding_model.clone()));

        let registry: Arc<dyn VectorStore> = Arc::new(
            QdrantVectorStore::new(
                &cfg.qdrant_url,
                cfg.qdrant_api_key.as_deref(),
                REGISTRY_COLLECTION,
                cfg.vector_size,
            )
            .await
            .with_context(|| format!("create collection {}", REGISTRY_COLLECTION))?,
        );
        let messages: Arc<dyn VectorStore> = Arc::new(
            QdrantVectorStore::new(
                &cfg.qdrant_url,
                cfg.qdrant_api_key.as_deref(),
                MESSAGES_COLLECTION,
                cfg.vector_size,
            )
            .await
            .with_context(|| format!("create collection {}", MESSAGES_COLLECTION))?,
        );
        let outputs: Arc<dyn VectorStore> = Arc::new(
            QdrantVectorStore::new(
                &cfg.qdrant_url,
                cfg.qdrant_api_key.as_deref(),
                OUTPUTS_COLLECTION,
                cfg.vector_size,
            )
            .await
            .with_context(|| format!("create collection {}", OUTPUTS_COLLECTION))?,
        );

        Ok(Self {
            embedder,
            registry,
            messages,
            outputs,
            cfg,
        })
    }

    // Accessors used by submodules (indexer, cleanup, query).
    pub fn embedder(&self) -> Arc<Embedder> {
        self.embedder.clone()
    }
    pub fn registry(&self) -> Arc<dyn VectorStore> {
        self.registry.clone()
    }
    pub fn messages(&self) -> Arc<dyn VectorStore> {
        self.messages.clone()
    }
    pub fn outputs(&self) -> Arc<dyn VectorStore> {
        self.outputs.clone()
    }
    pub fn cfg(&self) -> &MemoryConfig {
        &self.cfg
    }

    /// Write a [`MemoryEntry`] into the appropriate ephemeral collection.
    /// Returns the store-synthesised entry id.
    pub async fn store_memory(&self, entry: MemoryEntry) -> Result<String> {
        let vec = self.embedder.embed(&entry.content).await?;
        self.store_memory_with_vec(entry, vec).await
    }

    /// Fix E (2026-05-09) — vector-input variant. Lets `RagPlanner::plan`
    /// embed the user message ONCE per turn and reuse the same vector for
    /// the registry / outputs / messages searches AND the `tengu_messages`
    /// persist write — collapsing 3 embedder calls per turn into 1.
    pub async fn store_memory_with_vec(
        &self,
        entry: MemoryEntry,
        vec: Vec<f32>,
    ) -> Result<String> {
        let store: &Arc<dyn VectorStore> = match entry.kind {
            MemoryKind::Message => &self.messages,
            MemoryKind::StepOutput | MemoryKind::FileChunk => &self.outputs,
        };
        let meta = memory_entry_metadata(&entry);
        store.write(vec, &entry.content, meta).await
    }

    /// Search `tengu_registry` for the top-`k` best matches to `query`.
    pub async fn search_registry(&self, query: &str, top_k: usize) -> Result<Vec<RagResult>> {
        query::search_registry(self, query, top_k).await
    }

    /// Fix E (2026-05-09) — vector-input variant of [`Self::search_registry`].
    /// See [`query::search_registry_with_vec`].
    pub async fn search_registry_with_vec(
        &self,
        vec: &[f32],
        top_k: usize,
    ) -> Result<Vec<RagResult>> {
        query::search_registry_with_vec(self, vec, top_k).await
    }

    /// Search `tengu_outputs` (the high-signal bucket) for cross-plan recall.
    /// `tengu_messages` is deliberately excluded — conversational noise
    /// pollutes fuzzy recall quality. Use [`Self::search_messages`] for the
    /// messages collection explicitly.
    pub async fn search_memory(&self, query: &str, top_k: usize) -> Result<Vec<RagResult>> {
        query::search_memory(self, query, top_k).await
    }

    /// Phase 6.4 (full) — semantic search over `tengu_messages` for
    /// cross-session conversational recall. No caller wires this into a
    /// prompt yet; the immediate purpose is to give the next commit a
    /// one-liner hook now that durable writes have landed.
    pub async fn search_messages(&self, query: &str, top_k: usize) -> Result<Vec<RagResult>> {
        query::search_messages(self, query, top_k).await
    }

    /// Fix E (2026-05-09) — vector-input variant of [`Self::search_messages`].
    pub async fn search_messages_with_vec(
        &self,
        vec: &[f32],
        top_k: usize,
    ) -> Result<Vec<RagResult>> {
        query::search_messages_with_vec(self, vec, top_k).await
    }

    /// Fix A (2026-05-09) — within-session output recall over
    /// `tengu_outputs` filtered to entries whose `rag_session_id` equals
    /// `session_id`. See [`query::search_outputs_for_session`] for filter
    /// semantics. Used by `RagPlanner::plan` when
    /// `MemoryConfig.within_session_output_top_k > 0`.
    pub async fn search_outputs_for_session(
        &self,
        query: &str,
        session_id: &str,
        top_k: usize,
    ) -> Result<Vec<RagResult>> {
        query::search_outputs_for_session(self, query, session_id, top_k).await
    }

    /// Fix E (2026-05-09) — vector-input variant of
    /// [`Self::search_outputs_for_session`].
    pub async fn search_outputs_for_session_with_vec(
        &self,
        vec: &[f32],
        session_id: &str,
        top_k: usize,
    ) -> Result<Vec<RagResult>> {
        query::search_outputs_for_session_with_vec(self, vec, session_id, top_k).await
    }

    /// Clear every entry from `tengu_registry`. Used before a full reindex.
    pub async fn clear_registry(&self) -> Result<()> {
        self.registry.clear_all().await
    }

    /// Write a vector of tool descriptions into `tengu_registry`.
    /// Does NOT clear first — call [`RagStore::clear_registry`] if you want
    /// a clean slate. (Phase 2 callers typically clear once then write all
    /// three of: tools, agents, skills.)
    pub async fn index_tools(
        &self,
        tools: Vec<crate::adapters::types::ToolDef>,
    ) -> Result<usize> {
        indexer::index_tools(self, tools).await
    }

    /// Write a vector of agent specs into `tengu_registry`.
    pub async fn index_agents(
        &self,
        agents: Vec<crate::adapters::agents::AgentSpec>,
    ) -> Result<usize> {
        indexer::index_agents(self, agents).await
    }

    /// Write a vector of discovered skills into `tengu_registry`.
    pub async fn index_skills(&self, skills: Vec<SkillEntry>) -> Result<usize> {
        indexer::index_skills(self, skills).await
    }

    /// Phase 1-compat wrapper: clear registry then index only the tools.
    /// Retained so `tengu registry reindex-tools` keeps working.
    pub async fn startup_index(
        &self,
        tools: Vec<crate::adapters::types::ToolDef>,
    ) -> Result<usize> {
        self.clear_registry().await?;
        self.index_tools(tools).await
    }

    /// TTL cleanup. No-op when `memory.ttl_days == 0`.
    pub async fn ttl_cleanup(&self) -> Result<u64> {
        cleanup::ttl_cleanup(self).await
    }
}

/// Build a registry `ChunkMetadata` payload that carries the rag-specific
/// fields (`rag_type`, `rag_name`, `rag_source_path`) inside the `extra`
/// map so the existing `VectorStore` trait can be used unchanged.
pub(crate) fn registry_metadata(
    kind: RagKind,
    name: &str,
    source_path: Option<&str>,
) -> ChunkMetadata {
    let mut extra = std::collections::HashMap::new();
    extra.insert(
        "rag_type".to_string(),
        serde_json::Value::String(kind.as_str().to_string()),
    );
    extra.insert(
        "rag_name".to_string(),
        serde_json::Value::String(name.to_string()),
    );
    if let Some(p) = source_path {
        extra.insert(
            "rag_source_path".to_string(),
            serde_json::Value::String(p.to_string()),
        );
    }
    ChunkMetadata {
        agent: None,
        source: source_path.map(|s| s.to_string()),
        kind: Some(format!("rag.{}", kind.as_str())),
        tags: vec!["rag".to_string(), kind.as_str().to_string()],
        timestamp_utc: Some(chrono::Utc::now().to_rfc3339()),
        extra,
    }
}

/// Build a memory-entry `ChunkMetadata` payload. All fields are cloned so
/// the caller can continue to own `entry`.
pub(crate) fn memory_entry_metadata(entry: &MemoryEntry) -> ChunkMetadata {
    let mut extra = std::collections::HashMap::new();
    extra.insert(
        "rag_memory_kind".to_string(),
        serde_json::Value::String(entry.kind.as_str().to_string()),
    );
    extra.insert(
        "rag_session_id".to_string(),
        serde_json::Value::String(entry.session_id.clone()),
    );
    if let Some(ref sid) = entry.step_id {
        extra.insert(
            "rag_step_id".to_string(),
            serde_json::Value::String(sid.clone()),
        );
    }
    extra.insert(
        "rag_created_at".to_string(),
        serde_json::Value::Number(serde_json::Number::from(entry.created_at)),
    );
    ChunkMetadata {
        agent: None,
        source: None,
        kind: Some(format!("rag.memory.{}", entry.kind.as_str())),
        tags: vec![
            "rag".to_string(),
            "memory".to_string(),
            entry.kind.as_str().to_string(),
        ],
        timestamp_utc: Some(chrono::Utc::now().to_rfc3339()),
        extra,
    }
}
