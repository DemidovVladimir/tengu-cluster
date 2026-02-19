//! Runtime tool registry and built-in tool implementations.
//!
//! Potential use case:
//! Execute model-emitted tool calls (for example `read_file`) through one
//! policy-checked registry without coupling engine adapters to filesystem logic.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs;

use tengu_core::{Tool, ToolContext, ToolOutput};

/// Hard output cap for tool payloads returned to runtime prompt assembly.
const TOOL_OUTPUT_MAX_TOKENS: u32 = 2_048;

/// In-memory registry of tool implementations keyed by stable tool name.
#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// Build an empty registry.
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    /// Build registry with default built-in tools.
    pub fn with_defaults() -> Self {
        let mut registry = Self::new();
        registry.register(ReadFileTool);
        registry
    }

    /// Register one tool implementation.
    pub fn register<T>(&mut self, tool: T)
    where
        T: Tool + 'static,
    {
        self.tools.insert(tool.name().to_string(), Arc::new(tool));
    }

    /// Return whether a tool with this name is present.
    pub fn has(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    /// Execute a named tool and apply hard output token guard.
    pub async fn execute(
        &self,
        name: &str,
        arguments: Value,
        context: &ToolContext,
    ) -> Result<ToolOutput> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| anyhow!("Tool '{}' is not registered", name))?;
        let output = tool.execute(arguments, context).await?;
        Ok(output.enforce_token_limit(TOOL_OUTPUT_MAX_TOKENS).output)
    }
}

/// Built-in tool that reads one file relative to workspace root.
struct ReadFileTool;

#[derive(Debug, Deserialize)]
struct ReadFileArgs {
    path: String,
}

#[async_trait::async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read a UTF-8 text file from workspace root"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": { "type": "string" }
            }
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: &ToolContext,
    ) -> anyhow::Result<ToolOutput> {
        let args: ReadFileArgs = serde_json::from_value(params)
            .context("read_file expects JSON object: {\"path\":\"...\"}")?;
        let resolved = resolve_workspace_file(&ctx.workspace, &args.path)?;
        let content = fs::read_to_string(&resolved)
            .await
            .with_context(|| format!("Failed to read file: {}", resolved.display()))?;

        Ok(ToolOutput {
            content,
            is_error: false,
        })
    }
}

/// Resolve a user-provided file path under workspace and reject path traversal.
fn resolve_workspace_file(workspace: &Path, requested_path: &str) -> Result<PathBuf> {
    let rel = requested_path.trim();
    if rel.is_empty() {
        return Err(anyhow!("read_file path cannot be empty"));
    }

    let workspace_root = workspace
        .canonicalize()
        .with_context(|| format!("Failed to resolve workspace path: {}", workspace.display()))?;
    let candidate = workspace_root.join(rel);
    let resolved = candidate
        .canonicalize()
        .with_context(|| format!("Failed to resolve file path: {}", candidate.display()))?;

    if !resolved.starts_with(&workspace_root) {
        return Err(anyhow!(
            "Path '{}' escapes workspace root '{}'",
            rel,
            workspace_root.display()
        ));
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tengu-tool-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[tokio::test]
    async fn read_file_reads_workspace_file() {
        let workspace = tmp_dir();
        let file = workspace.join("note.txt");
        fs::write(&file, "hello tool").expect("write fixture file");

        let mut registry = ToolRegistry::new();
        registry.register(ReadFileTool);
        let ctx = ToolContext {
            workspace: workspace.clone(),
            agent_id: "main".to_string(),
        };
        let output = registry
            .execute("read_file", serde_json::json!({ "path": "note.txt" }), &ctx)
            .await
            .expect("tool output");

        assert!(!output.is_error);
        assert_eq!(output.content, "hello tool");

        let _ = fs::remove_dir_all(workspace);
    }

    #[tokio::test]
    async fn read_file_blocks_workspace_escape() {
        let root = tmp_dir();
        let workspace = root.join("workspace");
        let outside_dir = root.join("outside");
        fs::create_dir_all(&workspace).expect("create workspace");
        fs::create_dir_all(&outside_dir).expect("create outside dir");
        let outside_file = outside_dir.join("secret.txt");
        fs::write(&outside_file, "secret").expect("write outside file");

        let mut registry = ToolRegistry::new();
        registry.register(ReadFileTool);
        let ctx = ToolContext {
            workspace: workspace.clone(),
            agent_id: "main".to_string(),
        };
        let err = match registry
            .execute(
                "read_file",
                serde_json::json!({ "path": "../outside/secret.txt" }),
                &ctx,
            )
            .await
        {
            Ok(_) => panic!("expected traversal error"),
            Err(err) => err,
        };

        assert!(err.to_string().contains("escapes workspace root"));

        let _ = fs::remove_dir_all(root);
    }
}
