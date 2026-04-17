// src/adapters/plugins/skill/shell_tool.rs
//! Reusable `SkillShellTool` — executes a shell skill template.
//!
//! One struct, one `Tool` impl, one shared implementation for every active
//! shell skill: the tool definition and the command template are captured at
//! construction time by the [`SkillPlugin`](super::SkillPlugin). Every
//! `execute()` gates the resolved binary through
//! `ctx.scope.check_shell_bin(...)` before dispatching to
//! `ctx.shell.execute_shell`.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::adapters::tool_plugin::{Tool, ToolCtx, ToolOutput};
use crate::adapters::types::ToolDef;

/// Thin wrapper that renders a shell skill's command template and runs it.
pub(crate) struct SkillShellTool {
    def: ToolDef,
    template: String,
}

impl SkillShellTool {
    pub(crate) fn new(def: ToolDef, template: String) -> Self {
        Self { def, template }
    }
}

#[async_trait]
impl Tool for SkillShellTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        let command = render_command(&self.template, args)?;
        let bin = extract_binary(&command);
        if bin.is_empty() {
            anyhow::bail!("skill_shell: empty command");
        }
        ctx.scope.check_shell_bin(bin)?;

        tracing::info!(skill = %self.def.name, command = %command, "Executing skill tool");
        let result = ctx.shell.execute_shell(&command, ctx.workspace);
        match &result {
            Ok(output) => tracing::info!(
                skill = %self.def.name,
                output_len = output.len(),
                "Skill tool executed"
            ),
            Err(e) => tracing::warn!(skill = %self.def.name, error = %e, "Skill tool failed"),
        }
        Ok(ToolOutput::from(result?))
    }
}

// ---------------------------------------------------------------------------
// Template rendering — moved verbatim from `skill_builder::render_command`.
// ---------------------------------------------------------------------------

/// Substitute `{{param}}` placeholders with shell-escaped argument values.
pub(crate) fn render_command(template: &str, arguments: &Value) -> Result<String> {
    let mut result = template.to_string();
    let mut pos = 0;

    while let Some(start) = result[pos..].find("{{") {
        let abs_start = pos + start;
        if let Some(end) = result[abs_start + 2..].find("}}") {
            let abs_end = abs_start + 2 + end;
            let placeholder = result[abs_start + 2..abs_end].trim();

            let value = arguments.get(placeholder);
            let rendered = match value {
                Some(Value::String(s)) => shell_escape(s),
                Some(Value::Number(n)) => n.to_string(),
                Some(Value::Bool(b)) => b.to_string(),
                Some(Value::Null) | None => String::new(),
                Some(other) => shell_escape(&other.to_string()),
            };

            result.replace_range(abs_start..abs_end + 2, &rendered);
            pos = abs_start + rendered.len();
        } else {
            break;
        }
    }

    Ok(result)
}

fn shell_escape(s: &str) -> String {
    let escaped = s.replace('\'', "'\\''");
    format!("'{}'", escaped)
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

    fn tool_def(name: &str) -> ToolDef {
        ToolDef::new(
            name,
            "test skill",
            json!({
                "type": "object",
                "properties": {
                    "expression": { "type": "string" }
                },
                "required": []
            }),
        )
    }

    #[tokio::test]
    async fn skill_shell_renders_template() {
        let tmp = TempDir::new().unwrap();
        let harness = TestHarness::new(tmp.path());
        let tool = SkillShellTool::new(
            tool_def("echo_arg"),
            "echo {{expression}}".to_string(),
        );
        let output = tool
            .execute(&json!({ "expression": "hello-skills" }), &harness.ctx())
            .await
            .unwrap();
        assert!(
            output.text.contains("hello-skills"),
            "expected template substitution, got: {:?}",
            output.text
        );
    }

    #[tokio::test]
    async fn skill_shell_rejects_empty_command() {
        let tmp = TempDir::new().unwrap();
        let harness = TestHarness::new(tmp.path());
        let tool = SkillShellTool::new(tool_def("blank"), "   ".to_string());
        let result = tool.execute(&json!({}), &harness.ctx()).await;
        assert!(result.is_err(), "expected empty-command rejection, got: {:?}", result);
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("empty command"), "unexpected error: {}", msg);
    }

    #[tokio::test]
    async fn skill_shell_scope_denies_unallowed_bin() {
        let tmp = TempDir::new().unwrap();
        let scope = ToolScope {
            fs_roots: vec![tmp.path().to_path_buf()],
            shell_bins: vec!["allowed".to_string()],
            ..Default::default()
        };
        let harness = TestHarness::with_scope(tmp.path(), scope);
        let tool = SkillShellTool::new(
            tool_def("forbidden"),
            "echo should-not-run".to_string(),
        );
        let result = tool.execute(&json!({}), &harness.ctx()).await;
        assert!(result.is_err(), "expected scope denial, got: {:?}", result);
    }
}
