// src/adapters/outbound/tools/workspace/read_file.rs
//! `read_file` tool — read a file from the workspace, with PDF text extraction.

use anyhow::{bail, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::adapters::outbound::tools::args::require_str;
use crate::adapters::outbound::tools::args::validate_path;
use crate::domain::message::ToolDef;
use crate::ports::tool::{Tool, ToolCtx, ToolOutput};

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
        let path_str = require_str(args, "read_file", "path")?;

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

        let ext_lower = target
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();

        if ext_lower == "pdf" {
            let text = pdf_extract::extract_text(&target).map_err(|e| {
                anyhow::anyhow!("Cannot extract text from PDF '{}': {}", path_str, e)
            })?;
            if text.trim().is_empty() {
                bail!(
                    "PDF '{}' contains no extractable text (may be image-only)",
                    path_str
                );
            }
            return Ok(ToolOutput::from(text));
        }

        // Reject well-known binary formats up front with actionable guidance.
        // The LLM commonly tries to `read_file` images before upload; that's
        // unnecessary — the upload path only needs `file_path`.
        const BINARY_EXTS: &[&str] = &[
            "png", "jpg", "jpeg", "gif", "webp", "bmp", "tiff", "ico", "svg", "mp3", "mp4", "mov",
            "avi", "wav", "ogg", "flac", "zip", "tar", "gz", "bz2", "xz", "7z", "rar", "exe",
            "dll", "so", "dylib", "bin", "wasm", "parquet", "db", "sqlite",
        ];
        if BINARY_EXTS.contains(&ext_lower.as_str()) {
            bail!(
                "Refusing to read binary file '{}' (.{}). Do NOT read binary assets (images, archives, etc.) — \
                 pass the path directly to the upload tool via `file_path`. \
                 For images in particular, `http_request` with `file_path: {}` is sufficient for S3/PUT uploads.",
                path_str, ext_lower, path_str
            );
        }

        match std::fs::read_to_string(&target) {
            Ok(content) => Ok(ToolOutput::from(content)),
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                bail!(
                    "File '{}' is not UTF-8 text. If it's a binary asset (image, archive, etc.), \
                     do not read it — pass the path to the upload tool via `file_path`.",
                    path_str
                );
            }
            Err(e) => bail!("Cannot read file '{}': {}", path_str, e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::outbound::tools::workspace::test_support::TestHarness;
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
