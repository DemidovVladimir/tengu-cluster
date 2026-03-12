//! Filesystem adapter for workspace tool execution.

use crate::application::ports::{ShellExecutionPort, ToolExecutionPort};
use anyhow::{bail, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tengu_core::types::ToolCall;

const MAX_READ_SIZE: u64 = 100 * 1024;
/// PDFs are binary and larger, but extracted text is bounded by MAX_READ_SIZE.
const MAX_PDF_SIZE: u64 = 10 * 1024 * 1024;

pub fn validate_path(workspace: &Path, requested: &str) -> Result<PathBuf> {
    let workspace_canonical = workspace
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("Workspace directory not found: {}", e))?;

    let target = if Path::new(requested).is_absolute() {
        PathBuf::from(requested)
    } else {
        workspace.join(requested)
    };

    if target.exists() {
        let canonical = target
            .canonicalize()
            .map_err(|e| anyhow::anyhow!("Cannot resolve path: {}", e))?;
        if !canonical.starts_with(&workspace_canonical) {
            bail!("Path escapes workspace: {}", requested);
        }
        return Ok(canonical);
    }

    let parent = target
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Invalid path: no parent directory"))?;
    if !parent.exists() {
        let mut ancestor = parent.to_path_buf();
        while !ancestor.exists() {
            ancestor = match ancestor.parent() {
                Some(p) => p.to_path_buf(),
                None => bail!("No valid ancestor directory for path: {}", requested),
            };
        }
        let ancestor_canonical = ancestor.canonicalize()?;
        if !ancestor_canonical.starts_with(&workspace_canonical) {
            bail!("Path escapes workspace: {}", requested);
        }
    } else {
        let parent_canonical = parent.canonicalize()?;
        if !parent_canonical.starts_with(&workspace_canonical) {
            bail!("Path escapes workspace: {}", requested);
        }
    }

    Ok(target)
}

pub fn execute_tool(workspace: &Path, call: &ToolCall) -> Result<String> {
    match call.name.as_str() {
        "read_file" => {
            let path_str = call
                .arguments
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("read_file: missing 'path' argument"))?;

            let target = validate_path(workspace, path_str)?;

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
                if metadata.len() > MAX_PDF_SIZE {
                    bail!(
                        "PDF '{}' is too large ({} bytes, max {} bytes)",
                        path_str,
                        metadata.len(),
                        MAX_PDF_SIZE
                    );
                }
                let text = pdf_extract::extract_text(&target).map_err(|e| {
                    anyhow::anyhow!("Cannot extract text from PDF '{}': {}", path_str, e)
                })?;
                if text.trim().is_empty() {
                    bail!(
                        "PDF '{}' contains no extractable text (may be image-only)",
                        path_str
                    );
                }
                // Truncate extracted text to the standard read limit.
                let max_chars = MAX_READ_SIZE as usize;
                if text.len() > max_chars {
                    let truncated = truncate_utf8_safe(&text, max_chars);
                    Ok(format!(
                        "{}\n\n[truncated — {} of {} bytes shown]",
                        truncated,
                        max_chars,
                        text.len()
                    ))
                } else {
                    Ok(text)
                }
            } else {
                if metadata.len() > MAX_READ_SIZE {
                    bail!(
                        "File '{}' is too large ({} bytes, max {} bytes)",
                        path_str,
                        metadata.len(),
                        MAX_READ_SIZE
                    );
                }
                let content = std::fs::read_to_string(&target)
                    .map_err(|e| anyhow::anyhow!("Cannot read file '{}': {}", path_str, e))?;
                Ok(content)
            }
        }

        "list_directory" => {
            let path_str = call
                .arguments
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or(".");

            let target = validate_path(workspace, path_str)?;

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
                Ok("(empty directory)".into())
            } else {
                Ok(entries.join("\n"))
            }
        }

        "write_file" => {
            let path_str = call
                .arguments
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("write_file: missing 'path' argument"))?;
            let content = call
                .arguments
                .get("content")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("write_file: missing 'content' argument"))?;

            // Block writes to skill directories to prevent LLM-crafted malicious skills.
            let normalized = path_str.replace('\\', "/");
            if normalized.starts_with("skills/")
                || normalized.starts_with(".tengu/skills/")
                || normalized.contains("/skills/")
            {
                bail!("Writing to skill directories is not allowed");
            }

            let target = validate_path(workspace, path_str)?;

            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| anyhow::anyhow!("Cannot create directory: {}", e))?;
            }

            std::fs::write(&target, content)
                .map_err(|e| anyhow::anyhow!("Cannot write file '{}': {}", path_str, e))?;

            Ok(format!(
                "File '{}' written ({} bytes)",
                path_str,
                content.len()
            ))
        }

        other => bail!("Unknown tool: {}", other),
    }
}

pub(crate) struct WorkspaceToolExecutionAdapter {
    workspace: PathBuf,
    shell: Option<Arc<dyn ShellExecutionPort>>,
}

impl WorkspaceToolExecutionAdapter {
    pub(crate) fn new(workspace: PathBuf) -> Self {
        Self {
            workspace,
            shell: None,
        }
    }

    pub(crate) fn with_shell(mut self, shell: Arc<dyn ShellExecutionPort>) -> Self {
        self.shell = Some(shell);
        self
    }

    fn execute_run_command(&self, call: &ToolCall) -> Result<String> {
        let command = call
            .arguments
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("run_command: missing 'command' argument"))?;

        let shell = self
            .shell
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Shell execution is not available"))?;

        shell.execute_shell(command, &self.workspace)
    }
}

impl ToolExecutionPort for WorkspaceToolExecutionAdapter {
    fn execute_tool(&self, call: &ToolCall) -> Result<String> {
        if call.name == "run_command" {
            return self.execute_run_command(call);
        }
        execute_tool(&self.workspace, call)
    }
}

/// Truncate a string to at most `max_bytes` without splitting a UTF-8 codepoint.
fn truncate_utf8_safe(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

pub fn expand_tilde(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if s.starts_with("~/") {
        if let Some(home) = dirs_next::home_dir() {
            return home.join(&s[2..]);
        }
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use tempfile::TempDir;

    fn make_workspace() -> TempDir {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("hello.txt"), "Hello, world!").unwrap();
        fs::create_dir_all(dir.path().join("subdir")).unwrap();
        fs::write(dir.path().join("subdir/nested.txt"), "nested content").unwrap();
        dir
    }

    fn tool_call(name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "test-id".into(),
            name: name.into(),
            arguments: args,
        }
    }

    #[test]
    fn read_file_returns_content() {
        let ws = make_workspace();
        let call = tool_call("read_file", json!({"path": "hello.txt"}));
        let result = execute_tool(ws.path(), &call).unwrap();
        assert_eq!(result, "Hello, world!");
    }

    #[test]
    fn read_file_nested() {
        let ws = make_workspace();
        let call = tool_call("read_file", json!({"path": "subdir/nested.txt"}));
        let result = execute_tool(ws.path(), &call).unwrap();
        assert_eq!(result, "nested content");
    }

    #[test]
    fn list_directory_root() {
        let ws = make_workspace();
        let call = tool_call("list_directory", json!({"path": "."}));
        let result = execute_tool(ws.path(), &call).unwrap();
        assert!(result.contains("hello.txt"));
        assert!(result.contains("subdir/"));
    }

    #[test]
    fn write_file_creates_file() {
        let ws = make_workspace();
        let call = tool_call(
            "write_file",
            json!({"path": "new.txt", "content": "new content"}),
        );
        let result = execute_tool(ws.path(), &call).unwrap();
        assert!(result.contains("new.txt"));
        assert_eq!(
            fs::read_to_string(ws.path().join("new.txt")).unwrap(),
            "new content"
        );
    }

    #[test]
    fn write_file_creates_parent_dirs() {
        let ws = make_workspace();
        let call = tool_call(
            "write_file",
            json!({"path": "deep/dir/file.txt", "content": "deep"}),
        );
        execute_tool(ws.path(), &call).unwrap();
        assert_eq!(
            fs::read_to_string(ws.path().join("deep/dir/file.txt")).unwrap(),
            "deep"
        );
    }

    #[test]
    fn path_traversal_rejected() {
        let ws = make_workspace();
        let call = tool_call("read_file", json!({"path": "../../etc/passwd"}));
        let result = execute_tool(ws.path(), &call);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("escapes workspace") || err.contains("Cannot read"));
    }

    #[test]
    fn read_directory_as_file_rejected() {
        let ws = make_workspace();
        let call = tool_call("read_file", json!({"path": "subdir"}));
        let result = execute_tool(ws.path(), &call);
        assert!(result.is_err());
    }

    #[test]
    fn write_to_skills_dir_rejected() {
        let ws = make_workspace();
        for path in &[
            "skills/evil/SKILL.md",
            ".tengu/skills/evil/SKILL.md",
            "subdir/skills/evil.md",
        ] {
            let call = tool_call("write_file", json!({"path": path, "content": "malicious"}));
            let result = execute_tool(ws.path(), &call);
            assert!(result.is_err(), "write to '{}' should be rejected", path);
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("skill directories"),
                "error for '{}' should mention skill directories",
                path,
            );
        }
    }
}
