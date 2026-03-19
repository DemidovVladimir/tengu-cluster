//! Tool infrastructure: definitions, UI helpers, execution service, workspace
//! primitives, and path utilities.

use crate::adapters::ports::{ShellExecutionPort, ToolActivityPort, ToolApprovalPort, ToolExecutionPort};
use crate::adapters::types::{
    EffectClass, RegisteredTool, ToolCall, ToolPolicyCatalog,
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

/// Maximum number of characters to show in the approval preview.
const PREVIEW_MAX_CHARS: usize = 300;

/// Build title, description, and preview text for a tool approval dialog.
pub(crate) fn build_approval_text(call: &ToolCall) -> (String, String, String) {
    let title = prettify_tool_name(&call.name);

    // API-style tools: show "POST /v1/wallets/{id}/rpc" instead of raw JSON.
    let method = call.arguments.get("method").and_then(|v| v.as_str());
    let path = call.arguments.get("path").and_then(|v| v.as_str());
    if let (Some(method), Some(path)) = (method, path) {
        let description = format!("{} {}", method.to_uppercase(), path);
        let body = call
            .arguments
            .get("body")
            .and_then(|v| v.as_str())
            .unwrap_or("{}");
        let preview = if body.trim() == "{}" || body.trim().is_empty() {
            String::new()
        } else {
            truncate_detail(body, PREVIEW_MAX_CHARS)
        };
        return (title, description, preview);
    }

    let description = format!("Allow '{}' to run?", call.name);
    let preview = format_args_preview(&call.arguments);
    (title, description, preview)
}

pub(crate) fn build_tool_activity_text(
    call: &ToolCall,
    tools: &[RegisteredTool],
) -> (String, Option<String>) {
    let title = tools
        .iter()
        .find(|tool| tool.def.name == call.name)
        .and_then(|tool| tool.metadata.activity_description.clone())
        .unwrap_or_else(|| prettify_tool_name(&call.name));
    let detail = summarize_tool_args(&call.arguments);
    let detail = if detail.is_empty() {
        None
    } else {
        Some(detail)
    };
    (title, detail)
}

/// Build a short human-readable summary of tool arguments for activity lines.
pub(crate) fn summarize_tool_args(args: &serde_json::Value) -> String {
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

fn format_args_preview(args: &serde_json::Value) -> String {
    let obj = match args.as_object() {
        Some(m) if !m.is_empty() => m,
        _ => return String::new(),
    };

    let mut out = String::new();
    for (k, v) in obj {
        let s = match v.as_str() {
            Some(s) => s,
            None => continue,
        };
        if !out.is_empty() {
            out.push('\n');
        }
        let line = format!("{}: {}", k, s);
        let remaining = PREVIEW_MAX_CHARS.saturating_sub(out.len());
        if remaining == 0 {
            break;
        }
        out.push_str(&truncate_detail(&line, remaining));
    }
    out
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
#[derive(Clone)]
pub(crate) struct ToolUseService {
    policies: ToolPolicyCatalog,
    activity: Arc<dyn ToolActivityPort>,
    approval: Arc<dyn ToolApprovalPort>,
    execution: Arc<dyn ToolExecutionPort>,
}

impl ToolUseService {
    pub(crate) fn new(
        policies: ToolPolicyCatalog,
        activity: Arc<dyn ToolActivityPort>,
        approval: Arc<dyn ToolApprovalPort>,
        execution: Arc<dyn ToolExecutionPort>,
    ) -> Self {
        Self {
            policies,
            activity,
            approval,
            execution,
        }
    }

    pub(crate) fn execute(&self, call: &ToolCall) -> Result<String> {
        self.activity.publish_tool_activity(call);

        if !self.policies.is_allowed(&call.name) {
            anyhow::bail!("Tool '{}' is not available to this agent.", call.name);
        }

        let needs_approval =
            self.policies.requires_approval(&call.name) && !is_read_only_call(call);
        if needs_approval && !self.approval.request_tool_approval(call)? {
            anyhow::bail!("Tool execution denied by user.");
        }

        self.execution.execute_tool(call)
    }
}

/// A tool call is read-only if it carries a `method` argument equal to "GET".
fn is_read_only_call(call: &ToolCall) -> bool {
    call.arguments
        .get("method")
        .and_then(|v| v.as_str())
        .map(|m| m.eq_ignore_ascii_case("GET"))
        .unwrap_or(false)
}

// ── Workspace tool execution ────────────────────────────────────────────

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
pub(crate) fn build_workspace_tools() -> Vec<RegisteredTool> {
    vec![
        RegisteredTool::new(
            "read_file",
            "Read the contents of a file in the workspace. Supports text files and PDF documents — PDF text is extracted automatically.",
            path_only_schema("File path relative to the workspace root"),
            EffectClass::Read,
        )
        .with_activity_description("Reading file"),
        RegisteredTool::new(
            "list_directory",
            "List files and directories at a path in the workspace.",
            path_only_schema("Directory path relative to the workspace root. Use '.' for the root."),
            EffectClass::Read,
        )
        .with_activity_description("Listing directory"),
        RegisteredTool::new(
            "write_file",
            "Write content to a file in the workspace. Creates parent directories if needed.",
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
            EffectClass::Write,
        )
        .with_activity_description("Writing file"),
        RegisteredTool::new(
            "run_command",
            "Execute a shell command in the workspace directory and return its output. Use this to run scripts, install packages, call APIs, compile code, or perform any action the user requests. Always prefer executing commands directly over creating script files.",
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
            EffectClass::ShellExec,
        )
        .with_activity_description("Running command"),
    ]
}

// ── Platform tool definitions ───────────────────────────────────────────

/// Build the set of platform-level primitive tools.
pub(crate) fn build_platform_tools() -> Vec<RegisteredTool> {
    vec![
        RegisteredTool::new(
            "http_request",
            "Make an HTTP request to an external API. Supports JSON and multipart/form-data \
             (file upload). Use this to interact with any REST API documented in active skills.",
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
                    }
                },
                "required": ["url", "method"]
            }),
            EffectClass::ExternalApi,
        )
        .with_activity_description("HTTP request"),
        RegisteredTool::new(
            "sign_and_send_transaction",
            "Sign and send an EVM transaction using the configured Privy agentic wallet. \
             Waits for the receipt and returns tx hash + status.",
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
            EffectClass::ChainTx,
        )
        .with_activity_description("Signing transaction")
        .with_required_secrets(&["PRIVY_APP_ID", "PRIVY_APP_SECRET", "PRIVY_WALLET_ID"]),
        RegisteredTool::new(
            "sign_message",
            "Sign a message using the configured Privy agentic wallet. Returns the signature.",
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
            EffectClass::ChainTx,
        )
        .with_activity_description("Signing message")
        .with_required_secrets(&["PRIVY_APP_ID", "PRIVY_APP_SECRET", "PRIVY_WALLET_ID"]),
        RegisteredTool::new(
            "get_wallet_address",
            "Get the address of the configured Privy agentic wallet.",
            json!({
                "type": "object",
                "properties": {}
            }),
            EffectClass::Read,
        )
        .with_activity_description("Getting wallet address")
        .with_required_secrets(&["PRIVY_APP_ID", "PRIVY_APP_SECRET", "PRIVY_WALLET_ID"]),
        RegisteredTool::new(
            "abi_encode",
            "ABI-encode an EVM function call. Returns 0x-prefixed hex calldata for use \
             with sign_and_send_transaction. Handles address, uint256, string, bytes, \
             bool, and nested types.",
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
            EffectClass::Read,
        )
        .with_activity_description("ABI-encoding calldata"),
        RegisteredTool::new(
            "hex_to_uint256",
            "Convert a 0x-prefixed hex string to a decimal uint256 string. \
             Use this to derive reservation IDs from merkle roots or convert \
             any bytes32 / hex value to its decimal representation.",
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
            EffectClass::Read,
        )
        .with_activity_description("Hex to uint256"),
    ]
}
