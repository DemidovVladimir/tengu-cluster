//! Adapter bridging memory tool calls to the MemoryService.
//!
//! The tool execution port is synchronous (called from the engine's tool loop),
//! but both the embedding and memory store ports are async. This adapter owns a
//! dedicated single-threaded tokio runtime to bridge the gap via `block_on()`.

use crate::application::memory_service::MemoryService;
use crate::application::ports::{EmbeddingPort, MemoryStorePort, ToolExecutionPort};
use crate::domain::secret_registry::SecretRegistry;
use anyhow::Result;
use std::sync::Arc;
use tengu_core::types::ToolCall;

/// Shared handle owning the embedding + store ports for Arc-based sharing.
///
/// The same handle is referenced by both the `MemoryToolExecutionAdapter`
/// (for `remember` tool calls) and the TUI (for `MemoryService::recall` and
/// status bar stats).
pub(crate) struct MemoryServiceHandle {
    pub embedding: Arc<dyn EmbeddingPort>,
    pub store: Arc<dyn MemoryStorePort>,
}

/// Adapter implementing ToolExecutionPort by routing memory tool calls
/// through a dedicated tokio runtime (to bridge sync → async).
pub(crate) struct MemoryToolExecutionAdapter {
    handle: Arc<MemoryServiceHandle>,
    secret_registry: Arc<SecretRegistry>,
    runtime: tokio::runtime::Runtime,
}

impl MemoryToolExecutionAdapter {
    pub(crate) fn new(
        handle: Arc<MemoryServiceHandle>,
        secret_registry: Arc<SecretRegistry>,
    ) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        Ok(Self {
            handle,
            secret_registry,
            runtime,
        })
    }
}

impl ToolExecutionPort for MemoryToolExecutionAdapter {
    fn execute_tool(&self, call: &ToolCall) -> Result<String> {
        match call.name.as_str() {
            "remember" => {
                let raw_content = call
                    .arguments
                    .get("content")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("remember: missing 'content' argument"))?;
                // Redact secrets before persisting to memory store.
                let content = self.secret_registry.redact(raw_content);
                let content = content.as_str();

                let agent_id = call
                    .arguments
                    .get("agent_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("default");

                let service = MemoryService::new(
                    self.handle.embedding.as_ref(),
                    self.handle.store.as_ref(),
                );

                let id = self
                    .runtime
                    .block_on(service.remember(content, agent_id))?;

                Ok(format!("Stored memory with id: {}", id))
            }
            other => Err(anyhow::anyhow!("unknown memory tool: {}", other)),
        }
    }
}
