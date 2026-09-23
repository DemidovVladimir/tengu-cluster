// src/adapters/outbound/tools/workspace/list_directory.rs
//! `list_directory` tool — list the entries of a directory in the workspace.

use anyhow::{bail, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::adapters::outbound::tools::args::validate_path;
use crate::domain::message::ToolDef;
use crate::ports::tool::{Tool, ToolCtx, ToolOutput};

pub(crate) struct ListDirectoryTool {
    def: ToolDef,
}

impl ListDirectoryTool {
    pub(crate) fn new() -> Self {
        Self {
            def: ToolDef::new(
                "list_directory",
                "List directory contents.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Directory path relative to the workspace root. Use '.' for the root."
                        }
                    },
                    "required": ["path"]
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
    async fn list_directory_shows_entries() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "").unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        let harness = TestHarness::new(tmp.path());
        let tool = ListDirectoryTool::new();
        let output = tool
            .execute(&json!({"path": "."}), &harness.ctx())
            .await
            .unwrap();
        assert!(output.text.contains("a.txt"));
        assert!(output.text.contains("sub/"));
    }

    #[tokio::test]
    async fn list_directory_scope_denies_outside_root() {
        let tmp = TempDir::new().unwrap();
        let other = TempDir::new().unwrap();
        std::fs::create_dir_all(other.path().join("payload")).unwrap();
        let scope = ToolScope {
            fs_roots: vec![tmp.path().to_path_buf()],
            ..Default::default()
        };
        let harness = TestHarness::with_scope(tmp.path(), scope);
        let tool = ListDirectoryTool::new();
        let outside = other.path().join("payload");
        let result = tool
            .execute(&json!({"path": outside.to_str().unwrap()}), &harness.ctx())
            .await;
        assert!(result.is_err(), "expected scope denial, got: {:?}", result);
    }
}

#[async_trait]
impl Tool for ListDirectoryTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        let path_str = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");

        let target = validate_path(ctx.workspace, path_str)?;
        ctx.scope.check_fs_read(&target)?;

        if !target.is_dir() {
            bail!("'{}' is not a directory", path_str);
        }

        let mut entries: Vec<String> = Vec::new();
        for entry in std::fs::read_dir(&target)
            .map_err(|e| anyhow::anyhow!("Cannot list '{}': {}", path_str, e))?
        {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            let file_type = entry.file_type()?;
            let suffix = if file_type.is_dir() {
                "/"
            } else if file_type.is_symlink() {
                "@"
            } else {
                ""
            };
            entries.push(format!("{}{}", name, suffix));
        }
        entries.sort();

        if entries.is_empty() {
            Ok(ToolOutput::from("(empty directory)".to_string()))
        } else {
            Ok(ToolOutput::from(entries.join("\n")))
        }
    }
}
