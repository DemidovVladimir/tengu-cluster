// src/adapters/plugins/memory/mod.rs
//! Memory plugin — vector-memory-backed tools.
//!
//! Provides:
//! - `remember` — embed + store a fact in the shared memory backend. Always
//!   registered when `ctx.memory` is `Some`.
//! - `persistent_store` — chunked file storage with semantic search. Opt-in
//!   via `AgentConfig.workspace_tools` (like `shared_cache`).
//!
//! Both tools are natively async — no `block_in_place` bridge. See task A5 in
//! `docs/superpowers/plans/2026-04-16-phase-a-tool-plugin-architecture.md`.

use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

use crate::adapters::memory_builder::MemoryServiceHandle;
use crate::adapters::tool_plugin::{PluginCtx, Tool, ToolPlugin};
use crate::adapters::types::ToolDef;

pub(crate) mod persistent_store;
pub(crate) mod remember;

pub(crate) use persistent_store::{PersistentStoreTool, PERSISTENT_STORE_TOOL_NAME};
pub(crate) use remember::RememberTool;

/// Tool definitions advertised by the memory plugin's always-on tools.
///
/// Used by `channel_runtime::compute_base_tools` and `compute_bridge_tools`
/// to advertise memory tools before plugin instantiation. The caller gates
/// inclusion by whether memory is enabled. `persistent_store` is opt-in per
/// agent via `workspace_tools` and has its own separate `tool_defs()` below.
pub(crate) fn tool_defs() -> Vec<ToolDef> {
    // Building a throwaway tool reuses the schema source of truth. We never
    // execute against the handle — `definition()` does not touch it.
    //
    // We need a valid `MemoryServiceHandle` to construct the tool; use a
    // dummy one since `definition()` never dereferences it.
    let dummy = dummy_handle();
    vec![RememberTool::new(dummy).definition().clone()]
}

/// Tool definitions for the opt-in `persistent_store` tool.
pub(crate) fn persistent_store_tool_defs() -> Vec<ToolDef> {
    let dummy = dummy_handle();
    vec![
        PersistentStoreTool::new(std::path::PathBuf::from("/"), dummy, 1000, 200)
            .definition()
            .clone(),
    ]
}

/// Construct a no-op `MemoryServiceHandle` for schema inspection paths that
/// never touch the embedding / store ports. The `ToolDef` stored on each tool
/// is independent of the handle, so this keeps `tool_defs()` infallible.
fn dummy_handle() -> Arc<MemoryServiceHandle> {
    use crate::adapters::ports::{EmbeddingPort, MemoryStorePort};
    use crate::adapters::types::{MemoryEntry, MemorySearchResult};
    use std::future::Future;
    use std::pin::Pin;

    struct NoopEmbed;
    impl EmbeddingPort for NoopEmbed {
        fn embed(
            &self,
            _texts: &[&str],
        ) -> Pin<Box<dyn Future<Output = Result<Vec<Vec<f32>>>> + Send + '_>> {
            Box::pin(async { Ok(vec![]) })
        }
    }

    struct NoopStore;
    impl MemoryStorePort for NoopStore {
        fn store(
            &self,
            _entry: &MemoryEntry,
        ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
            Box::pin(async { Ok(()) })
        }
        fn search_by_vector(
            &self,
            _embedding: &[f32],
            _top_k: usize,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<MemorySearchResult>>> + Send + '_>> {
            Box::pin(async { Ok(vec![]) })
        }
        fn delete(
            &self,
            _id: &str,
        ) -> Pin<Box<dyn Future<Output = Result<bool>> + Send + '_>> {
            Box::pin(async { Ok(false) })
        }
        fn clear_all(&self) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
            Box::pin(async { Ok(()) })
        }
        fn entry_count(&self) -> Pin<Box<dyn Future<Output = usize> + Send + '_>> {
            Box::pin(async { 0 })
        }
        fn storage_bytes(&self) -> Pin<Box<dyn Future<Output = u64> + Send + '_>> {
            Box::pin(async { 0 })
        }
    }

    Arc::new(MemoryServiceHandle {
        embedding: Arc::new(NoopEmbed),
        store: Arc::new(NoopStore),
    })
}

/// Plugin grouping memory tools.
///
/// Construction is driven by `PluginCtx`:
/// - `remember` is included whenever `ctx.memory` is `Some`.
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
            vec![Arc::new(RememberTool::new(Arc::clone(&handle)))];

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
    use tempfile::TempDir;

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
    async fn memory_plugin_includes_remember_when_memory_enabled() {
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
        assert!(names.contains(&"remember".to_string()));
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
        assert!(names.contains(&"remember".to_string()));
        assert!(names.contains(&"persistent_store".to_string()));
    }
}
