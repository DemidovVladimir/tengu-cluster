// src/adapters/plugins/workspace/write_file.rs
//! `write_file` tool — write content to a file in the workspace.

use anyhow::{bail, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::adapters::outbound::tools::args::require_str;
use crate::adapters::outbound::tools::args::validate_path;
use crate::domain::message::ToolDef;
use crate::ports::tool::{Tool, ToolCtx, ToolOutput};

pub(crate) struct WriteFileTool {
    def: ToolDef,
}

impl WriteFileTool {
    pub(crate) fn new() -> Self {
        Self {
            def: ToolDef::new(
                "write_file",
                "Write content to a file.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "File path relative to the workspace root"
                        },
                        "content": {
                            "type": "string",
                            "description": "Content to write to the file"
                        }
                    },
                    "required": ["path", "content"]
                }),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::outbound::tools::workspace::test_support::TestHarness;
    use crate::domain::scope::ToolScope;
    use serde_json::json;
    use tempfile::TempDir;

    #[tokio::test]
    async fn write_file_creates_file() {
        let tmp = TempDir::new().unwrap();
        let harness = TestHarness::new(tmp.path());
        let tool = WriteFileTool::new();
        let output = tool
            .execute(
                &json!({"path": "out.txt", "content": "hello"}),
                &harness.ctx(),
            )
            .await
            .unwrap();
        assert!(output.text.contains("out.txt"));
        let written = std::fs::read_to_string(tmp.path().join("out.txt")).unwrap();
        assert_eq!(written, "hello");
    }

    #[tokio::test]
    async fn write_file_rejects_skill_directory() {
        let tmp = TempDir::new().unwrap();
        let harness = TestHarness::new(tmp.path());
        let tool = WriteFileTool::new();
        let result = tool
            .execute(
                &json!({"path": "skills/evil/SKILL.md", "content": "x"}),
                &harness.ctx(),
            )
            .await;
        assert!(
            result.is_err(),
            "expected skill-dir block, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn write_file_scope_denies_empty_roots() {
        let tmp = TempDir::new().unwrap();
        // Empty fs_roots — default-deny triggers.
        let scope = ToolScope::default();
        let harness = TestHarness::with_scope(tmp.path(), scope);
        let tool = WriteFileTool::new();
        let result = tool
            .execute(&json!({"path": "out.txt", "content": "x"}), &harness.ctx())
            .await;
        assert!(result.is_err(), "expected scope denial, got: {:?}", result);
    }
}

#[async_trait]
impl Tool for WriteFileTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        let path_str = require_str(args, "write_file", "path")?;
        let target = validate_path(ctx.workspace, path_str)?;
        ctx.scope.check_fs_write(&target)?;

        let content = require_str(args, "write_file", "content")?;

        // Block writes to skill directories to prevent LLM-crafted malicious skills.
        let normalized = path_str.replace('\\', "/");
        if normalized.starts_with("skills/")
            || normalized.starts_with(".tengu/skills/")
            || normalized.contains("/skills/")
        {
            bail!("Writing to skill directories is not allowed");
        }

        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| anyhow::anyhow!("Cannot create directory: {}", e))?;
        }

        std::fs::write(&target, content)
            .map_err(|e| anyhow::anyhow!("Cannot write file '{}': {}", path_str, e))?;

        Ok(ToolOutput::from(format!(
            "File '{}' written ({} bytes)",
            path_str,
            content.len()
        )))
    }
}
