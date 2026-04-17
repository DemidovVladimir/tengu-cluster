// src/adapters/plugins/workspace/run_command.rs
//! `run_command` tool — execute a shell command in the workspace.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::adapters::tool_plugin::{Tool, ToolCtx, ToolOutput};
use crate::adapters::types::ToolDef;

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
    use crate::adapters::plugins::workspace::test_support::TestHarness;
    use crate::adapters::ports::ToolScope;
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
        assert!(result.is_err(), "expected empty-command rejection, got: {:?}", result);
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
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("run_command: missing 'command' argument"))?;

        let bin = extract_binary(command);
        if bin.is_empty() { anyhow::bail!("run_command: empty command"); }
        ctx.scope.check_shell_bin(bin)?;

        let output = ctx.shell.execute_shell(command, ctx.workspace)?;
        Ok(ToolOutput::from(output))
    }
}
