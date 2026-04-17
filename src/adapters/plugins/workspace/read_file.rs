// src/adapters/plugins/workspace/read_file.rs
//! `read_file` tool — read a file from the workspace, with PDF text extraction.

use anyhow::{bail, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::adapters::tool_builder::validate_path;
use crate::adapters::tool_plugin::{Tool, ToolCtx, ToolOutput};
use crate::adapters::types::ToolDef;

pub(crate) struct ReadFileTool {
    def: ToolDef,
}

impl ReadFileTool {
    pub(crate) fn new() -> Self {
        Self {
            def: ToolDef::new(
                "read_file",
                "Read a file.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "File path relative to the workspace root"
                        }
                    },
                    "required": ["path"]
                }),
            ),
        }
    }
}

#[async_trait]
impl Tool for ReadFileTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        let path_str = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("read_file: missing 'path' argument"))?;

        let target = validate_path(ctx.workspace, path_str)?;
        ctx.scope.check_fs_read(&target)?;

        let metadata = std::fs::metadata(&target)
            .map_err(|e| anyhow::anyhow!("Cannot read file '{}': {}", path_str, e))?;

        if metadata.is_dir() {
            bail!(
                "'{}' is a directory, not a file. Use list_directory instead.",
                path_str
            );
        }

        let is_pdf = target
            .extension()
            .map(|e| e.eq_ignore_ascii_case("pdf"))
            .unwrap_or(false);

        if is_pdf {
            let text = pdf_extract::extract_text(&target).map_err(|e| {
                anyhow::anyhow!("Cannot extract text from PDF '{}': {}", path_str, e)
            })?;
            if text.trim().is_empty() {
                bail!(
                    "PDF '{}' contains no extractable text (may be image-only)",
                    path_str
                );
            }
            Ok(ToolOutput::from(text))
        } else {
            let content = std::fs::read_to_string(&target)
                .map_err(|e| anyhow::anyhow!("Cannot read file '{}': {}", path_str, e))?;
            Ok(ToolOutput::from(content))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::plugins::workspace::test_support::TestHarness;
    use serde_json::json;
    use tempfile::TempDir;

    #[tokio::test]
    async fn read_file_returns_content() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("hello.txt"), "world").unwrap();
        let harness = TestHarness::new(tmp.path());
        let tool = ReadFileTool::new();
        let output = tool
            .execute(&json!({"path": "hello.txt"}), &harness.ctx())
            .await
            .unwrap();
        assert_eq!(output.text, "world");
    }

    #[tokio::test]
    async fn read_file_scope_denies_outside_root() {
        let tmp = TempDir::new().unwrap();
        let other = TempDir::new().unwrap();
        std::fs::write(other.path().join("secret.txt"), "password").unwrap();
        let harness = TestHarness::new(tmp.path());
        let tool = ReadFileTool::new();
        let outside = other.path().join("secret.txt");
        let result = tool
            .execute(&json!({"path": outside.to_str().unwrap()}), &harness.ctx())
            .await;
        assert!(result.is_err(), "expected scope denial, got: {:?}", result);
    }
}
