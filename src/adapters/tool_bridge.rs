//! Adapter bridging core `Tool` trait implementations to `ToolExecutionPort`.
//!
//! This allows any `Box<dyn Tool>` to be plugged into the composite executor
//! via the standard `ToolExecutionPort` interface. Uses a dedicated tokio
//! runtime to bridge sync→async (same pattern as `MemoryToolExecutionAdapter`).

use crate::application::ports::ToolExecutionPort;
use anyhow::{bail, Result};
use std::collections::{HashMap, HashSet};
use tengu_core::types::ToolCall;
use tengu_core::{Tool, ToolContext};

pub(crate) struct ToolBridgeAdapter {
    tools: HashMap<String, Box<dyn Tool>>,
    runtime: Option<tokio::runtime::Runtime>,
    ctx: ToolContext,
}

impl ToolBridgeAdapter {
    pub(crate) fn new(
        tools: Vec<Box<dyn Tool>>,
        ctx: ToolContext,
    ) -> Result<Self> {
        let runtime = if tokio::runtime::Handle::try_current().is_ok() {
            None
        } else {
            Some(
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?,
            )
        };
        let map = tools
            .into_iter()
            .map(|t| (t.name().to_string(), t))
            .collect();
        Ok(Self {
            tools: map,
            runtime,
            ctx,
        })
    }

    pub(crate) fn tool_names(&self) -> HashSet<String> {
        self.tools.keys().cloned().collect()
    }
}

impl ToolExecutionPort for ToolBridgeAdapter {
    fn execute_tool(&self, call: &ToolCall) -> Result<String> {
        let tool = self
            .tools
            .get(&call.name)
            .ok_or_else(|| anyhow::anyhow!("Unknown tool: {}", call.name))?;
        let output = if let Ok(handle) = tokio::runtime::Handle::try_current() {
            tokio::task::block_in_place(|| {
                handle.block_on(tool.execute(call.arguments.clone(), &self.ctx))
            })?
        } else {
            self.runtime
                .as_ref()
                .expect("no tokio runtime available")
                .block_on(tool.execute(call.arguments.clone(), &self.ctx))?
        };
        if output.is_error {
            bail!("{}", output.content);
        }
        Ok(output.content)
    }
}
