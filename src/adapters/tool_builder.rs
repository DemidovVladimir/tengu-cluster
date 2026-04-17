//! Tool infrastructure: definitions, UI helpers, execution service, workspace
//! primitives, and path utilities.

use crate::adapters::ports::{ShellExecutionPort, ToolActivityPort, ToolExecutionPort};
use crate::adapters::types::{
    ToolAllowList, ToolCall, ToolDef,
};
use anyhow::{bail, Result};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;

// ── Path utilities ──────────────────────────────────────────────────────

pub fn expand_tilde(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if s.starts_with("~/") {
        if let Some(home) = dirs_next::home_dir() {
            return home.join(&s[2..]);
        }
    }
    path.to_path_buf()
}

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

// ── UI helpers ──────────────────────────────────────────────────────────

pub(crate) fn build_tool_activity_text(
    call: &ToolCall,
) -> (String, Option<String>) {
    let title = prettify_tool_name(&call.name);
    let detail = summarize_tool_args_for(&call.name, &call.arguments);
    let detail = if detail.is_empty() {
        None
    } else {
        Some(detail)
    };
    (title, detail)
}

/// Build a short human-readable summary of tool arguments for activity lines.
///
/// Tool-aware: uses the tool name to pick the most informative arguments
/// (e.g. URL for http_request, destination for sign_and_send_transaction).
fn summarize_tool_args_for(tool_name: &str, args: &serde_json::Value) -> String {
    let obj = match args.as_object() {
        Some(m) if !m.is_empty() => m,
        _ => return String::new(),
    };

    match tool_name {
        "http_request" => {
            let method = obj.get("method").and_then(|v| v.as_str()).unwrap_or("?");
            let url = obj.get("url").and_then(|v| v.as_str()).unwrap_or("?");
            format!("{} {}", method, truncate_detail(url, 100))
        }
        "sign_and_send_transaction" => {
            let to = obj.get("to").and_then(|v| v.as_str()).unwrap_or("?");
            let chain = obj.get("chain_id").and_then(|v| v.as_u64());
            match chain {
                Some(id) => format!("to={} chain={}", to, id),
                None => format!("to={}", to),
            }
        }
        _ => summarize_tool_args(args),
    }
}

/// Generic argument summary fallback for tools without special handling.
fn summarize_tool_args(args: &serde_json::Value) -> String {
    let obj = match args.as_object() {
        Some(m) if !m.is_empty() => m,
        _ => return String::new(),
    };

    if obj.len() == 1 {
        if let Some(val) = obj.values().next().and_then(|v| v.as_str()) {
            return truncate_detail(val, 120);
        }
    }

    let mut short_args: Vec<(&str, &str)> = obj
        .iter()
        .filter_map(|(k, v)| v.as_str().map(|s| (k.as_str(), s)))
        .collect();
    short_args.sort_by_key(|(_, v)| v.len());

    if let Some(&(_, val)) = short_args.first() {
        if val.len() <= 120 {
            return truncate_detail(val, 120);
        }
    }

    let parts: Vec<String> = obj
        .iter()
        .filter_map(|(k, v)| {
            v.as_str()
                .map(|s| format!("{}={}", k, truncate_detail(s, 60)))
        })
        .collect();
    parts.join(" ")
}

/// Convert "run_command" → "Run Command".
fn prettify_tool_name(name: &str) -> String {
    name.split('_')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(c) => c.to_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Truncate a display string, appending "…" if it exceeds the limit.
fn truncate_detail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

// ── Execution service ───────────────────────────────────────────────────

/// Service for the "execute tool call" use case.
///
/// Retained during the Phase A migration for the legacy bridge path; the new
/// channel adapters now build `PluginToolExecutor` directly.
// TODO(A9): remove alongside CompositeToolExecutionAdapter and ToolExecutionPort
#[allow(dead_code)]
#[derive(Clone)]
pub(crate) struct ToolUseService {
    allow_list: ToolAllowList,
    activity: Arc<dyn ToolActivityPort>,
    execution: Arc<dyn ToolExecutionPort>,
}

#[allow(dead_code)]
impl ToolUseService {
    pub(crate) fn new(
        allow_list: ToolAllowList,
        activity: Arc<dyn ToolActivityPort>,
        execution: Arc<dyn ToolExecutionPort>,
    ) -> Self {
        Self {
            allow_list,
            activity,
            execution,
        }
    }

    pub(crate) fn execute(&self, call: &ToolCall) -> Result<String> {
        self.activity.publish_tool_activity(call);

        if !self.allow_list.is_allowed(&call.name) {
            anyhow::bail!("Tool '{}' is not available to this agent.", call.name);
        }

        self.execution.execute_tool(call)
    }
}

// ── Workspace tool execution ────────────────────────────────────────────

// TODO(A9): delete once mcp_bridge uses ToolRegistry
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

// TODO(A9): delete once mcp_bridge uses ToolRegistry
fn execute_workspace_tool(workspace: &Path, call: &ToolCall) -> Result<String> {
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
                let text = pdf_extract::extract_text(&target).map_err(|e| {
                    anyhow::anyhow!("Cannot extract text from PDF '{}': {}", path_str, e)
                })?;
                if text.trim().is_empty() {
                    bail!(
                        "PDF '{}' contains no extractable text (may be image-only)",
                        path_str
                    );
                } else {
                    Ok(text)
                }
            } else {
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

impl ToolExecutionPort for WorkspaceToolExecutionAdapter {
    fn execute_tool(&self, call: &ToolCall) -> Result<String> {
        if call.name == "run_command" {
            return self.execute_run_command(call);
        }
        execute_workspace_tool(&self.workspace, call)
    }
}

// ── Workspace tool definitions ──────────────────────────────────────────

fn path_only_schema(path_description: &str) -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": path_description
            }
        },
        "required": ["path"]
    })
}

/// Build the set of workspace tool definitions to pass to engine.run().
// TODO(A9): delete once mcp_bridge uses ToolRegistry
pub(crate) fn build_workspace_tools() -> Vec<ToolDef> {
    vec![
        ToolDef::new(
            "read_file",
            "Read a file.",
            path_only_schema("File path relative to the workspace root"),
        ),
        ToolDef::new(
            "list_directory",
            "List directory contents.",
            path_only_schema("Directory path relative to the workspace root. Use '.' for the root."),
        ),
        ToolDef::new(
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
        ToolDef::new(
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
    ]
}

// ── Platform tool definitions ───────────────────────────────────────────

/// Build the set of platform-level primitive tools.
pub(crate) fn build_platform_tools() -> Vec<ToolDef> {
    vec![
        ToolDef::new(
            "http_request",
            "Make an HTTP request.",
            json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "Full URL. Supports $ENV_VAR (e.g. $MOLECULE_LABS_URL or https://api.example.com/v1/resource)"
                    },
                    "method": {
                        "type": "string",
                        "description": "HTTP method",
                        "enum": ["GET", "POST", "PUT", "DELETE", "PATCH"]
                    },
                    "headers": {
                        "type": "string",
                        "description": "JSON object of request headers. Use $ENV_VAR for secrets, e.g. {\"Authorization\": \"Bearer $BEACH_API_KEY\"}"
                    },
                    "body": {
                        "type": "string",
                        "description": "Request body — JSON string for application/json, or raw text"
                    },
                    "file_path": {
                        "type": "string",
                        "description": "Workspace-relative path for multipart/form-data file upload"
                    },
                    "file_field_name": {
                        "type": "string",
                        "description": "Form field name for the uploaded file (default: \"file\")"
                    },
                    "auth_bearer_env": {
                        "type": "string",
                        "description": "Env var name for Bearer token auth (e.g. \"BEACH_API_KEY\")"
                    },
                    "auth_basic_user_env": {
                        "type": "string",
                        "description": "Env var name for Basic auth username (e.g. \"PRIVY_APP_ID\")"
                    },
                    "auth_basic_pass_env": {
                        "type": "string",
                        "description": "Env var name for Basic auth password (e.g. \"PRIVY_APP_SECRET\")"
                    },
                    "return_body": {
                        "type": "boolean",
                        "description": "If true, include the response body in the result. Default false — only status is returned on success. Set to true when you need data from the response (e.g. upload URLs, created resource IDs). Errors always include the body."
                    }
                },
                "required": ["url", "method"]
            }),
        ),
        ToolDef::new(
            "sign_and_send_transaction",
            "Sign and send an EVM transaction via Privy wallet.",
            json!({
                "type": "object",
                "properties": {
                    "to": {
                        "type": "string",
                        "description": "Destination address (0x-prefixed)"
                    },
                    "data": {
                        "type": "string",
                        "description": "Transaction calldata (0x-prefixed hex)"
                    },
                    "value": {
                        "type": "string",
                        "description": "Value in wei (decimal string, default: \"0\")"
                    },
                    "chain_id": {
                        "type": "integer",
                        "description": "Chain ID (default: 11155111 = Sepolia)"
                    },
                    "wait_for_receipt": {
                        "type": "boolean",
                        "description": "Wait for confirmation (default: true)"
                    }
                },
                "required": ["to"]
            }),
        ),
        ToolDef::new(
            "sign_message",
            "Sign a message via Privy wallet.",
            json!({
                "type": "object",
                "properties": {
                    "message": {
                        "type": "string",
                        "description": "The message to sign"
                    }
                },
                "required": ["message"]
            }),
        ),
        ToolDef::new(
            "get_wallet_address",
            "Get Privy wallet address.",
            json!({
                "type": "object",
                "properties": {}
            }),
        ),
        ToolDef::new(
            "abi_encode",
            "ABI-encode an EVM function call.",
            json!({
                "type": "object",
                "properties": {
                    "function_signature": {
                        "type": "string",
                        "description": "Solidity function signature, e.g. 'mintReservation(address,uint256,string,string,bytes)'"
                    },
                    "args": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Arguments as strings: address='0x...', uint256=decimal or '0x' hex, bytes='0x...' hex, string=plain text, bool='true'/'false'"
                    }
                },
                "required": ["function_signature", "args"]
            }),
        ),
        ToolDef::new(
            "hex_to_uint256",
            "Convert hex to decimal uint256.",
            json!({
                "type": "object",
                "properties": {
                    "hex": {
                        "type": "string",
                        "description": "0x-prefixed hex string (e.g. '0xe6f7...728c')"
                    }
                },
                "required": ["hex"]
            }),
        ),
    ]
}
