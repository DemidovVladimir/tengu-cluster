// src/adapters/plugins/memory/mod.rs
//! Memory plugin — vector-memory-backed tools.
//!
//! Provides:
//! - `memory_ingest` — embed + store a document or fact in the shared memory
//!   backend (renamed from `remember` in harness-orchestration task 2.1).
//!   Always registered when `ctx.memory` is `Some`.
//! - `persistent_store` — chunked file storage with semantic search. Opt-in
//!   via `AgentConfig.workspace_tools` (like `shared_cache`).
//!
//! Both tools are natively async — no `block_in_place` bridge. See task A5 in
//! `docs/superpowers/plans/2026-04-16-phase-a-tool-plugin-architecture.md`.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

use crate::adapters::tool_plugin::{PluginCtx, Tool, ToolPlugin};
use crate::adapters::types::ToolDef;

pub(crate) mod ingest;
pub(crate) mod persistent_store;

pub(crate) use ingest::{MemoryIngestTool, MEMORY_INGEST_TOOL_NAME};
pub(crate) use persistent_store::{PersistentStoreTool, PERSISTENT_STORE_TOOL_NAME};

/// ToolDef for `memory_ingest` — kept in sync with the schema in
/// `ingest::MemoryIngestTool::new`.
pub(crate) fn memory_ingest_def() -> ToolDef {
    ToolDef::new(
        MEMORY_INGEST_TOOL_NAME,
        "Ingest a document or fact into long-term vector memory. \
         Accepts either a single `text`/`content` string or a list of \
         pre-chunked `chunks`, plus optional free-form `metadata` \
         (e.g. source, topic, kind). Embeddings are computed by the \
         memory backend.",
        json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "The fact, insight, or document body to ingest. \
                                    Alias of `text`; one of content/text/chunks is required."
                },
                "text": {
                    "type": "string",
                    "description": "Alias of `content` — the text to ingest."
                },
                "chunks": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional pre-chunked content. If supplied, each chunk \
                                    is ingested as a separate memory entry sharing the \
                                    same metadata."
                },
                "metadata": {
                    "type": "object",
                    "description": "Optional free-form tags attached to every stored entry \
                                    (e.g. {\"kind\": \"fact\", \"source\": \"url\", \"topic\": \"auth\"}). \
                                    Non-string values are coerced to strings.",
                    "additionalProperties": true
                }
            }
        }),
    )
}

/// ToolDef for `persistent_store` — kept in sync with the schema in
/// `persistent_store::PersistentStoreTool::new`.
pub(crate) fn persistent_store_def() -> ToolDef {
    ToolDef::new(
        PERSISTENT_STORE_TOOL_NAME,
        "Persistent file store with vector search. Store files, search by semantic query, list or delete stored files.",
        json!({
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "enum": ["store", "search", "list", "delete"],
                    "description": "Operation to perform"
                },
                "file_path": {
                    "type": "string",
                    "description": "Path to the file to store (required for store). Relative to workspace or absolute."
                },
                "description": {
                    "type": "string",
                    "description": "Human description of the file content (optional for store, improves search quality)"
                },
                "query": {
                    "type": "string",
                    "description": "Semantic search query (required for search)"
                },
                "file_id": {
                    "type": "string",
                    "description": "File ID to delete (required for delete)"
                },
                "top_k": {
                    "type": "integer",
                    "description": "Max results to return for search (default: 5)"
                }
            },
            "required": ["operation"]
        }),
    )
}

/// Tool definitions advertised by the memory plugin's always-on tools.
///
/// Used by `channel_runtime::compute_base_tools` and `compute_bridge_tools`
/// to advertise memory tools before plugin instantiation. The caller gates
/// inclusion by whether memory is enabled. `persistent_store` is opt-in per
/// agent via `workspace_tools` and has its own separate `tool_defs()` below.
pub(crate) fn tool_defs() -> Vec<ToolDef> {
    vec![memory_ingest_def()]
}

/// Tool definitions for the opt-in `persistent_store` tool.
pub(crate) fn persistent_store_tool_defs() -> Vec<ToolDef> {
    vec![persistent_store_def()]
}

/// Plugin grouping memory tools.
///
/// Construction is driven by `PluginCtx`:
/// - `memory_ingest` is included whenever `ctx.memory` is `Some`.
/// - `persistent_store` is included when `ctx.memory` is `Some` AND
///   `ctx.config.workspace_tools` contains `"persistent_store"`.
///   Chunk size / overlap are read from the agent config's `MemoryConfig` if
///   present on that config path; otherwise defaults (1000 / 200).
pub(crate) struct MemoryPlugin {
    pub(crate) chunk_size: usize,
    pub(crate) chunk_overlap: usize,
}

impl MemoryPlugin {
    pub(crate) fn new(chunk_size: usize, chunk_overlap: usize) -> Self {
        Self {
            chunk_size,
            chunk_overlap,
        }
    }
}

#[async_trait]
impl ToolPlugin for MemoryPlugin {
    fn name(&self) -> &'static str {
        "memory"
    }

    async fn tools(&self, ctx: &PluginCtx<'_>) -> Result<Vec<Arc<dyn Tool>>> {
        let Some(handle) = ctx.memory.as_ref().cloned() else {
            // Memory disabled — no tools.
            return Ok(vec![]);
        };

        let mut tools: Vec<Arc<dyn Tool>> =
            vec![Arc::new(MemoryIngestTool::new(Arc::clone(&handle)))];

        if ctx
            .config
            .workspace_tools
            .iter()
            .any(|t| t == PERSISTENT_STORE_TOOL_NAME)
        {
            tools.push(Arc::new(PersistentStoreTool::new(
                ctx.workspace.to_path_buf(),
                Arc::clone(&handle),
                self.chunk_size,
                self.chunk_overlap,
            )));
        }

        Ok(tools)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::config::Config;
    use crate::adapters::memory_builder::MemoryServiceHandle;
    use crate::adapters::ports::{EmbeddingPort, MemoryStorePort};
    use crate::adapters::types::{MemoryEntry, MemorySearchResult};
    use tempfile::TempDir;

    /// Test-only no-op memory handle used to exercise plugin gating without a
    /// real embedding backend or vector store.
    fn dummy_handle() -> Arc<MemoryServiceHandle> {
        struct NoopEmbed;

        #[async_trait]
        impl EmbeddingPort for NoopEmbed {
            async fn embed(&self, _texts: &[&str]) -> Result<Vec<Vec<f32>>> {
                Ok(vec![])
            }
        }

        struct NoopStore;

        #[async_trait]
        impl MemoryStorePort for NoopStore {
            async fn store(&self, _entry: &MemoryEntry) -> Result<()> {
                Ok(())
            }
            async fn search_by_vector(
                &self,
                _embedding: &[f32],
                _top_k: usize,
            ) -> Result<Vec<MemorySearchResult>> {
                Ok(vec![])
            }
            async fn delete(&self, _id: &str) -> Result<bool> {
                Ok(false)
            }
            async fn clear_all(&self) -> Result<()> {
                Ok(())
            }
            async fn entry_count(&self) -> usize {
                0
            }
            async fn storage_bytes(&self) -> u64 {
                0
            }
        }

        Arc::new(MemoryServiceHandle {
            embedding: Arc::new(NoopEmbed),
            store: Arc::new(NoopStore),
        })
    }

    #[tokio::test]
    async fn memory_plugin_returns_empty_when_memory_disabled() {
        let tmp = TempDir::new().unwrap();
        let config = Config::default();
        let agent_config = config.agents.get("main").unwrap().clone();
        let ctx = PluginCtx {
            workspace: tmp.path(),
            config: &agent_config,
            http: reqwest::Client::new(),
            shell: Arc::new(crate::adapters::shell_executor::LocalShellExecutor::new()),
            memory: None,
            secret_registry: Arc::new(crate::adapters::secret_builder::SecretRegistry::new()),
            subagents: None,
        };
        let plugin = MemoryPlugin::new(1000, 200);
        let tools = plugin.tools(&ctx).await.unwrap();
        assert!(tools.is_empty(), "expected no tools when memory disabled");
    }

    #[tokio::test]
    async fn memory_plugin_includes_memory_ingest_when_memory_enabled() {
        let tmp = TempDir::new().unwrap();
        let config = Config::default();
        let agent_config = config.agents.get("main").unwrap().clone();
        let ctx = PluginCtx {
            workspace: tmp.path(),
            config: &agent_config,
            http: reqwest::Client::new(),
            shell: Arc::new(crate::adapters::shell_executor::LocalShellExecutor::new()),
            memory: Some(dummy_handle()),
            secret_registry: Arc::new(crate::adapters::secret_builder::SecretRegistry::new()),
            subagents: None,
        };
        let plugin = MemoryPlugin::new(1000, 200);
        let tools = plugin.tools(&ctx).await.unwrap();
        let names: Vec<String> = tools.iter().map(|t| t.definition().name.clone()).collect();
        assert!(names.contains(&"memory_ingest".to_string()));
        assert!(
            !names.contains(&"persistent_store".to_string()),
            "persistent_store should be opt-in"
        );
    }

    #[tokio::test]
    async fn memory_plugin_includes_persistent_store_when_opted_in() {
        let tmp = TempDir::new().unwrap();
        let config = Config::default();
        let mut agent_config = config.agents.get("main").unwrap().clone();
        agent_config.workspace_tools = vec!["persistent_store".to_string()];
        let ctx = PluginCtx {
            workspace: tmp.path(),
            config: &agent_config,
            http: reqwest::Client::new(),
            shell: Arc::new(crate::adapters::shell_executor::LocalShellExecutor::new()),
            memory: Some(dummy_handle()),
            secret_registry: Arc::new(crate::adapters::secret_builder::SecretRegistry::new()),
            subagents: None,
        };
        let plugin = MemoryPlugin::new(1000, 200);
        let tools = plugin.tools(&ctx).await.unwrap();
        let names: Vec<String> = tools.iter().map(|t| t.definition().name.clone()).collect();
        assert!(names.contains(&"memory_ingest".to_string()));
        assert!(names.contains(&"persistent_store".to_string()));
    }
}
