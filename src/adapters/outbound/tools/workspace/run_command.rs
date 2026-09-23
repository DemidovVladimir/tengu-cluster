// src/adapters/outbound/tools/workspace/run_command.rs
//! `run_command` tool — execute a shell command in the workspace.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::adapters::outbound::tools::args::require_str;
use crate::domain::message::ToolDef;
use crate::ports::tool::{Tool, ToolCtx, ToolOutput};

pub(crate) struct RunCommandTool {
    def: ToolDef,
}

impl RunCommandTool {
    pub(crate) fn new() -> Self {
        Self {
            def: ToolDef::new(
                "run_command",
                "Run a shell command.",
                json!({
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "The shell command to execute (runs via sh -c)"
                        }
                    },
                    "required": ["command"]
                }),
            ),
        }
    }
}

/// Extract the first whitespace-delimited token (the binary name) from a shell command.
fn extract_binary(command: &str) -> &str {
    command.trim().split_whitespace().next().unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::outbound::tools::workspace::test_support::TestHarness;
    use crate::domain::scope::ToolScope;
    use serde_json::json;
    use tempfile::TempDir;

    #[tokio::test]
    async fn run_command_echoes_output() {
        let tmp = TempDir::new().unwrap();
        let harness = TestHarness::new(tmp.path());
        let tool = RunCommandTool::new();
        let output = tool
            .execute(&json!({"command": "echo hello"}), &harness.ctx())
            .await
            .unwrap();
        assert!(output.text.contains("hello"));
    }

    #[tokio::test]
    async fn run_command_rejects_empty_command() {
        let tmp = TempDir::new().unwrap();
        let harness = TestHarness::new(tmp.path());
        let tool = RunCommandTool::new();
        let result = tool
            .execute(&json!({"command": "   "}), &harness.ctx())
            .await;
        assert!(
            result.is_err(),
            "expected empty-command rejection, got: {:?}",
            result
        );
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("empty command"), "unexpected error: {}", msg);
    }

    #[tokio::test]
    async fn run_command_scope_denies_unlisted_binary() {
        let tmp = TempDir::new().unwrap();
        let scope = ToolScope {
            fs_roots: vec![tmp.path().to_path_buf()],
            shell_bins: vec!["git".to_string()],
            ..Default::default()
        };
        let harness = TestHarness::with_scope(tmp.path(), scope);
        let tool = RunCommandTool::new();
        let result = tool
            .execute(&json!({"command": "echo should-not-run"}), &harness.ctx())
            .await;
        assert!(result.is_err(), "expected scope denial, got: {:?}", result);
    }
}

#[async_trait]
impl Tool for RunCommandTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        let command = require_str(args, "run_command", "command")?;

        let bin = extract_binary(command);
        if bin.is_empty() {
            anyhow::bail!("run_command: empty command");
        }
        ctx.scope.check_shell_bin(bin)?;
        let audit =
            crate::adapters::outbound::egress::policy().guard_shell("run_command", command)?;

        let output = ctx.shell.execute_shell(command, ctx.workspace);
        audit.finish(&output);
        Ok(ToolOutput::from(output?))
    }
}
