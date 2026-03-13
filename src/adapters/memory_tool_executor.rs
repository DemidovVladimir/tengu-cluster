//! Adapter bridging memory tool calls to the MemoryService.
//!
//! Owns the memory subsystem's `ToolDef` definitions via `memory_tool_defs()`.
//! The tool execution port is synchronous (called from the engine's tool loop),
//! but both the embedding and memory store ports are async. Uses `block_in_place`
//! when inside a multi-thread runtime (Telegram), or a fallback runtime (TUI).

use crate::application::memory_service::MemoryService;
use crate::application::ports::{EmbeddingPort, MemoryStorePort, ToolExecutionPort};
use crate::domain::capability::{CapabilityId, EffectClass, RegisteredTool};
use crate::domain::secret_registry::SecretRegistry;
use anyhow::Result;
use serde_json::json;
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
/// via `block_in_place` (multi-thread runtime) or a fallback runtime (TUI).
///
/// The fallback runtime is only created when no tokio runtime is active at
/// construction time (TUI case). When running inside an existing runtime
/// (Telegram/Orchestrator), it is `None` — avoiding the "Cannot drop a
/// runtime in a context where blocking is not allowed" panic.
pub(crate) struct MemoryToolExecutionAdapter {
    handle: Arc<MemoryServiceHandle>,
    secret_registry: Arc<SecretRegistry>,
    fallback_runtime: Option<tokio::runtime::Runtime>,
}

impl MemoryToolExecutionAdapter {
    pub(crate) fn new(
        handle: Arc<MemoryServiceHandle>,
        secret_registry: Arc<SecretRegistry>,
    ) -> Result<Self> {
        let fallback_runtime = if tokio::runtime::Handle::try_current().is_ok() {
            // Already inside a runtime — use block_in_place at call time.
            None
        } else {
            Some(
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?,
            )
        };
        Ok(Self {
            handle,
            secret_registry,
            fallback_runtime,
        })
    }

    fn run_async<F, T>(&self, future: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            tokio::task::block_in_place(|| handle.block_on(future))
        } else {
            self.fallback_runtime
                .as_ref()
                .expect("no tokio runtime available")
                .block_on(future)
        }
    }
}

/// Return tool definitions owned by the memory subsystem.
pub(crate) fn memory_tool_defs() -> Vec<RegisteredTool> {
    vec![RegisteredTool::new(
        "remember",
        "Store a fact or insight in long-term memory for future retrieval across sessions.",
        json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "The fact, insight, or information to remember"
                }
            },
            "required": ["content"]
        }),
        CapabilityId::new("memory.remember").expect("static capability is valid"),
        EffectClass::Read,
    )]
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

                let service =
                    MemoryService::new(self.handle.embedding.as_ref(), self.handle.store.as_ref());

                let id = self.run_async(service.remember(content, agent_id))?;

                Ok(format!("Stored memory with id: {}", id))
            }
            other => Err(anyhow::anyhow!("unknown memory tool: {}", other)),
        }
    }
}
