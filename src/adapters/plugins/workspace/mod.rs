// src/adapters/plugins/workspace/mod.rs
//! Workspace plugin — filesystem and shell primitives scoped to a workspace.
//!
//! Provides: `read_file`, `list_directory`, `write_file`, `run_command`.
//! Every tool gates its operation via `ctx.scope.check_*()` for fine-grained access control.

use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

use crate::domain::message::ToolDef;
use crate::ports::tool::{PluginCtx, Tool, ToolPlugin};

pub(crate) mod list_directory;
pub(crate) mod read_file;
pub(crate) mod run_command;
pub(crate) mod write_file;

#[cfg(test)]
pub(crate) mod test_support;

pub(crate) use list_directory::ListDirectoryTool;
pub(crate) use read_file::ReadFileTool;
pub(crate) use run_command::RunCommandTool;
pub(crate) use write_file::WriteFileTool;

/// Tool definitions advertised by the workspace plugin — used by
/// `channel_runtime::compute_base_tools`/`compute_bridge_tools` to populate
/// the agent-facing tool list before instantiating the registry.
pub(crate) fn tool_defs() -> Vec<ToolDef> {
    vec![
        ReadFileTool::new().definition().clone(),
        ListDirectoryTool::new().definition().clone(),
        WriteFileTool::new().definition().clone(),
        RunCommandTool::new().definition().clone(),
    ]
}

/// Plugin grouping the four workspace primitive tools.
pub(crate) struct WorkspacePlugin;

#[async_trait]
impl ToolPlugin for WorkspacePlugin {
    fn name(&self) -> &'static str {
        "workspace"
    }

    async fn tools(&self, _ctx: &PluginCtx<'_>) -> Result<Vec<Arc<dyn Tool>>> {
        Ok(vec![
            Arc::new(ReadFileTool::new()),
            Arc::new(ListDirectoryTool::new()),
            Arc::new(WriteFileTool::new()),
            Arc::new(RunCommandTool::new()),
        ])
    }
}
