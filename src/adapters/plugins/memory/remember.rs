// src/adapters/plugins/memory/remember.rs
//! `remember` tool — store a fact in vector memory.
//!
//! Migrated from `memory_builder::MemoryToolExecutionAdapter` during Phase A /
//! task A5. The old executor used `tokio::task::block_in_place` to bridge
//! sync→async; this implementation is natively async and `.await`s the
//! `MemoryService` methods directly.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

use crate::adapters::memory_builder::{MemoryService, MemoryServiceHandle};
use crate::adapters::tool_plugin::{Tool, ToolCtx, ToolOutput};
use crate::adapters::types::ToolDef;

/// Tool name (kept constant for cross-module reference).
#[allow(dead_code)]
pub(crate) const REMEMBER_TOOL_NAME: &str = "remember";

pub(crate) struct RememberTool {
    def: ToolDef,
    handle: Arc<MemoryServiceHandle>,
}

impl RememberTool {
    pub(crate) fn new(handle: Arc<MemoryServiceHandle>) -> Self {
        Self {
            def: ToolDef::new(
                REMEMBER_TOOL_NAME,
                "Store a fact in long-term memory.",
                json!({
                    "type": "object",
                    "properties": {
                        "content": {
                            "type": "string",
                            "description": "The fact, insight, or information to remember"
                        },
                        "metadata": {
                            "type": "object",
                            "description": "Optional key-value tags for the memory (e.g. {\"kind\": \"fact\", \"topic\": \"auth\"})",
                            "additionalProperties": { "type": "string" }
                        }
                    },
                    "required": ["content"]
                }),
            ),
            handle,
        }
    }
}

#[async_trait]
impl Tool for RememberTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        // scope: pure-compute — the OpenRouter embedding host is an
        // implementation detail of the memory subsystem (baked into
        // `OpenRouterEmbeddingAdapter`), not a tool-argument-driven HTTP call.
        // The tool surface exposes only in-memory storage semantics.
        let raw_content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("remember: missing 'content' argument"))?;
        let content = ctx.secret_registry.redact(raw_content);
        let content = content.as_str();

        let agent_id = args
            .get("agent_id")
            .and_then(|v| v.as_str())
            .unwrap_or("default");

        let metadata: HashMap<String, String> = args
            .get("metadata")
            .and_then(|v| v.as_object())
            .map(|obj| {
                obj.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();

        let service =
            MemoryService::new(self.handle.embedding.as_ref(), self.handle.store.as_ref());
        let id = service
            .remember_with_metadata(content, agent_id, metadata)
            .await?;

        Ok(ToolOutput::from(format!("Stored memory with id: {}", id)))
    }
}
