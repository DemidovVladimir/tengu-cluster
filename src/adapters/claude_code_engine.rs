//! Claude Code engine — runs agents through the local Claude CLI subprocess.
//!
//! Uses `claude -p --output-format stream-json` for NDJSON streaming. Claude CLI
//! spawns as a subprocess, connects to the `tengu mcp-bridge` for Tengu-native
//! tools, and uses its own native workspace tools (Read, Write, Bash, etc.).
//!
//! The NDJSON stream yields per-turn events: system init (session_id), assistant
//! messages (text + tool activity), and a final result with cost/usage metrics.

use anyhow::Result;
use async_trait::async_trait;
use futures::stream;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::time::Duration;
use tracing::{debug, error, warn};

use crate::adapters::types::{EngineContext, Message, ModelInfo, Role, StreamEvent, ToolDef};
use crate::adapters::{Engine, EngineDiagnostics};

// ---------------------------------------------------------------------------
// Builtin tools profile
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BuiltinToolsProfile {
    None,
    ReadOnly,
    Editor,
    EditorShell,
}

impl BuiltinToolsProfile {
    pub fn from_str(s: &str) -> Self {
        match s {
            "none" => Self::None,
            "read_only" => Self::ReadOnly,
            "editor" => Self::Editor,
            "editor_shell" => Self::EditorShell,
            _ => Self::EditorShell,
        }
    }

    fn cli_tools_arg(&self) -> String {
        match self {
            Self::None => String::new(),
            Self::ReadOnly => "Read,Glob,Grep".into(),
            Self::Editor => "Read,Glob,Grep,Edit,Write,MultiEdit".into(),
            Self::EditorShell => "Read,Glob,Grep,Edit,Write,MultiEdit,Bash".into(),
        }
    }
}

// ---------------------------------------------------------------------------
// NDJSON message types (stream-json protocol)
// ---------------------------------------------------------------------------

// NDJSON messages are parsed via serde_json::Value for maximum flexibility
// against protocol evolution. Typed structs can be added later if needed.

// ---------------------------------------------------------------------------
// Claude Code Engine
// ---------------------------------------------------------------------------

pub(crate) struct ClaudeCodeEngine {
    cli_path: PathBuf,
    profile: BuiltinToolsProfile,
    model: Option<String>,
    timeout_secs: u64,
    /// Calling agent's per-tool scope map (`AgentConfig.scopes`, with
    /// `[default_scopes]` already folded in). Exported to the bridge
    /// subprocess as `TENGU_BRIDGE_SCOPES` so MCP-routed tool calls are
    /// gated the same way in-process calls are. Empty = every tool permissive.
    scopes: std::collections::HashMap<String, crate::adapters::ports::ToolScope>,
}

impl ClaudeCodeEngine {
    pub fn new(
        cli_path: PathBuf,
        profile: BuiltinToolsProfile,
        model: Option<String>,
        timeout_secs: u64,
    ) -> Self {
        Self {
            cli_path,
            profile,
            model,
            timeout_secs,
            scopes: std::collections::HashMap::new(),
        }
    }

    /// Attach the agent's per-tool scope map (see `scopes` field). Call from
    /// `engine_builder::build_engine` with `agent_config.scopes.clone()`.
    pub fn with_scopes(
        mut self,
        scopes: std::collections::HashMap<String, crate::adapters::ports::ToolScope>,
    ) -> Self {
        self.scopes = scopes;
        self
    }

    /// Format conversation history into a prompt string for one-shot queries.
    fn format_prompt(messages: &[Message]) -> String {
        let mut parts = Vec::new();

        for msg in messages {
            match msg.role {
                Role::User => {
                    parts.push(format!("User: {}", msg.content));
                }
                Role::Assistant => {
                    parts.push(format!("Assistant: {}", msg.content));
                }
                Role::Tool => {
                    if let Some(ref id) = msg.tool_call_id {
                        parts.push(format!("[Tool result for {}]: {}", id, msg.content));
                    }
                }
                Role::System => {
                    parts.push(msg.content.clone());
                }
            }
        }

        parts.join("\n\n")
    }

    /// Build MCP config JSON for the tengu-tools bridge server.
    fn build_mcp_config_json(
        &self,
        tengu_bin: &str,
        workspace: &std::path::Path,
        bridge_tools: &[ToolDef],
        max_mcp_result_chars: u32,
    ) -> serde_json::Value {
        let tools_json = serde_json::to_string(bridge_tools).unwrap_or_else(|_| "[]".into());
        // Per-tool scopes cross the process boundary as JSON; the bridge's
        // `build_bridge_executor` reads them back and falls back to
        // `permissive_scope` for any tool without an entry.
        let scopes_json = serde_json::to_string(&self.scopes).unwrap_or_else(|_| "{}".into());
        let mut env = serde_json::json!({
            "TENGU_BRIDGE_WORKSPACE": workspace.to_string_lossy(),
            "TENGU_BRIDGE_TOOLS": tools_json,
            "TENGU_BRIDGE_MAX_RESULT_CHARS": max_mcp_result_chars.to_string()
        });
        env[crate::adapters::mcp_bridge::TENGU_BRIDGE_SCOPES_ENV] =
            serde_json::Value::String(scopes_json);
        // The bridge runs the tools — it must apply the parent's egress policy.
        env[crate::adapters::egress::EGRESS_ENV] =
            serde_json::Value::String(crate::adapters::egress::policy().child_env());
        // Forward persistent store chunk config if set in the parent process.
        if let Ok(v) = std::env::var("TENGU_PERSISTENT_STORE_CHUNK_SIZE") {
            env["TENGU_PERSISTENT_STORE_CHUNK_SIZE"] = serde_json::Value::String(v);
        }
        if let Ok(v) = std::env::var("TENGU_PERSISTENT_STORE_CHUNK_OVERLAP") {
            env["TENGU_PERSISTENT_STORE_CHUNK_OVERLAP"] = serde_json::Value::String(v);
        }
        // Phase 7.6 — forward session id so `agentic_memory` `capture` in the
        // bridge stamps `session_id` on Open Brain writes when the LLM omits
        // it (the MCP config replaces the inherited env).
        if let Ok(v) = std::env::var("TENGU_SESSION_ID") {
            env["TENGU_SESSION_ID"] = serde_json::Value::String(v);
        }
        // Forward OPENROUTER_API_KEY — the bridge's memory backend (DiskVectorStore
        // + Embedder) needs it.
        // The MCP config replaces inherited env, so without explicit forwarding
        // the bridge process boots without API access and memory tools silently
        // fail to register.
        if let Ok(v) = std::env::var("OPENROUTER_API_KEY") {
            env["OPENROUTER_API_KEY"] = serde_json::Value::String(v);
        }
        serde_json::json!({
            "mcpServers": {
                "tengu-tools": {
                    "command": tengu_bin,
                    "args": ["mcp-bridge"],
                    "env": env
                }
            }
        })
    }
}

/// Process a single NDJSON line from the Claude CLI stream.
///
/// Returns events to emit. Mutates `emitted_text` to track whether any
/// assistant text has been sent (used to decide whether result.result
/// needs to be emitted as a fallback).
fn process_ndjson_line(
    line: &str,
    emitted_text: &mut bool,
    tool_call_count: &mut u32,
) -> Vec<StreamEvent> {
    let json: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            debug!(error = %e, "Failed to parse NDJSON line (skipping)");
            return vec![];
        }
    };

    let msg_type = json.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let mut events = Vec::new();

    match msg_type {
        "system" => {
            let subtype = json.get("subtype").and_then(|v| v.as_str()).unwrap_or("");
            match subtype {
                "init" => {
                    let session_id = json.get("session_id").and_then(|v| v.as_str());
                    let model = json.get("model").and_then(|v| v.as_str());
                    let tools: Vec<&str> = json
                        .get("tools")
                        .and_then(|v| v.as_array())
                        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
                        .unwrap_or_default();
                    debug!(
                        session_id = session_id,
                        model = model,
                        tools = ?tools,
                        "Claude Code session initialized"
                    );
                }
                _ => {
                    debug!(subtype = subtype, "Claude Code system message");
                }
            }
        }

        "assistant" => {
            let message = json.get("message");
            if let Some(msg) = message {
                // Parse content blocks
                if let Some(content) = msg.get("content").and_then(|c| c.as_array()) {
                    for block in content {
                        let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        match block_type {
                            "text" => {
                                if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                                    if !text.is_empty() {
                                        *emitted_text = true;
                                        events.push(StreamEvent::TextDelta {
                                            text: text.to_string(),
                                        });
                                    }
                                }
                            }
                            "tool_use" => {
                                *tool_call_count += 1;
                                let name = block
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("unknown");
                                let input_summary = block
                                    .get("input")
                                    .map(|v| {
                                        let s = v.to_string();
                                        if s.len() > 300 {
                                            let mut end = 300;
                                            while end > 0 && !s.is_char_boundary(end) {
                                                end -= 1;
                                            }
                                            format!("{}…", &s[..end])
                                        } else {
                                            s
                                        }
                                    })
                                    .unwrap_or_default();
                                debug!(
                                    tool = %name,
                                    input = %input_summary,
                                    tool_call_count = *tool_call_count,
                                    "Claude Code tool call"
                                );
                            }
                            "thinking" => {
                                if let Some(text) = block.get("thinking").and_then(|v| v.as_str()) {
                                    if !text.is_empty() {
                                        events.push(StreamEvent::ThinkingDelta {
                                            text: text.to_string(),
                                        });
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }

                // Usage from this turn
                if let Some(usage) = msg.get("usage") {
                    let inp = usage
                        .get("input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as u32;
                    let out = usage
                        .get("output_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as u32;
                    if inp > 0 || out > 0 {
                        events.push(StreamEvent::Usage {
                            input_tokens: inp,
                            output_tokens: out,
                        });
                    }
                }
            }
        }

        "result" => {
            let subtype = json
                .get("subtype")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let cost = json.get("total_cost_usd").and_then(|v| v.as_f64());
            let turns = json.get("num_turns").and_then(|v| v.as_u64());
            let duration = json.get("duration_ms").and_then(|v| v.as_u64());
            let is_error = json
                .get("is_error")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            tracing::info!(
                subtype = subtype,
                cost_usd = cost,
                num_turns = turns,
                duration_ms = duration,
                is_error = is_error,
                "Claude Code query completed"
            );

            // Emit result text as fallback if no assistant text was streamed
            if !*emitted_text {
                if let Some(text) = json.get("result").and_then(|v| v.as_str()) {
                    if !text.is_empty() {
                        events.push(StreamEvent::TextDelta {
                            text: text.to_string(),
                        });
                    }
                }
            }

            // Emit final usage from result (cumulative)
            if let Some(usage) = json.get("usage") {
                let inp = usage
                    .get("input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32;
                let out = usage
                    .get("output_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32;
                if inp > 0 || out > 0 {
                    events.push(StreamEvent::Usage {
                        input_tokens: inp,
                        output_tokens: out,
                    });
                }
            }

            if is_error {
                let errors: Vec<&str> = json
                    .get("errors")
                    .and_then(|v| v.as_array())
                    .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
                    .unwrap_or_default();
                if !errors.is_empty() {
                    events.push(StreamEvent::Error {
                        message: format!("Claude Code error ({}): {}", subtype, errors.join("; ")),
                    });
                }
            }
        }

        "user" => {
            // Tool results come back as user messages with tool_result content blocks.
            if let Some(content) = json
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
            {
                for block in content {
                    let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if block_type == "tool_result" {
                        let tool_id = block
                            .get("tool_use_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("?");
                        let is_error = block
                            .get("is_error")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        let result_text = block
                            .get("content")
                            .map(|v| match v {
                                serde_json::Value::String(s) => s.clone(),
                                _ => v.to_string(),
                            })
                            .unwrap_or_default();
                        let truncated = if result_text.len() > 500 {
                            let mut end = 500;
                            while end > 0 && !result_text.is_char_boundary(end) {
                                end -= 1;
                            }
                            format!("{}…", &result_text[..end])
                        } else {
                            result_text
                        };
                        if is_error {
                            warn!(
                                tool_use_id = %tool_id,
                                result = %truncated,
                                "Claude Code tool ERROR"
                            );
                        } else {
                            debug!(
                                tool_use_id = %tool_id,
                                result = %truncated,
                                "Claude Code tool result"
                            );
                        }
                    }
                }
            }
        }

        _ => {}
    }

    events
}

#[async_trait]
impl Engine for ClaudeCodeEngine {
    fn id(&self) -> &str {
        "claude_code"
    }

    fn context_window(&self) -> usize {
        200_000
    }

    fn supports_tool_use(&self) -> bool {
        true
    }

    fn manages_own_workspace(&self) -> bool {
        true
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    fn diagnostics(&self) -> EngineDiagnostics {
        EngineDiagnostics {
            engine_id: self.id().to_string(),
            configured_model: Some(
                self.model
                    .clone()
                    .unwrap_or_else(|| "claude-code-cli".to_string()),
            ),
            endpoint: Some(self.cli_path.to_string_lossy().to_string()),
            transport: Some("subprocess/stream-json".to_string()),
            capabilities: self.capabilities(),
        }
    }

    fn available_models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "claude-code-cli".to_string(),
            provider: "claude_code".to_string(),
            display_name: "Claude Code (CLI)".to_string(),
            context_window: self.context_window(),
            supports_tools: true,
            supports_streaming: true,
        }]
    }

    async fn run(
        &self,
        messages: &[Message],
        _tools: &[ToolDef],
        context: &EngineContext,
    ) -> Result<Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send>>> {
        let prompt = Self::format_prompt(messages);

        if prompt.trim().is_empty() {
            return Ok(Box::pin(stream::iter(vec![StreamEvent::Error {
                message: "Empty prompt for Claude Code engine".to_string(),
            }])));
        }

        let workspace = context
            .workspace
            .as_ref()
            .map(|p| crate::adapters::tool_builder::expand_tilde(p));

        // Build subprocess command
        let mut cmd = tokio::process::Command::new(&self.cli_path);
        cmd.arg("-p")
            .arg("--output-format")
            .arg("stream-json")
            .arg("--verbose")
            .arg("--dangerously-skip-permissions")
            .arg("--no-session-persistence")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .env_remove("ANTHROPIC_API_KEY");
        // `[egress]`: with `route_llm_api` the CLI's own Anthropic traffic
        // goes through the proxy's HTTP CONNECT port (Arti serves it on 9050).
        for (k, v) in crate::adapters::egress::policy().claude_cli_env() {
            cmd.env(k, v);
        }

        // Model
        if let Some(ref model) = self.model {
            cmd.arg("--model").arg(model);
        }

        // System prompt
        if let Some(ref sp) = context.system_prompt {
            cmd.arg("--system-prompt").arg(sp);
        }

        // Built-in tools based on profile
        let tools_arg = self.profile.cli_tools_arg();
        cmd.arg("--tools").arg(&tools_arg);

        // Working directory
        if let Some(ref ws) = workspace {
            cmd.current_dir(ws);
        }

        // MCP config for Tengu bridge tools — write to temp file, keep handle alive
        // The temp file is moved into the spawned task so it stays alive until the
        // subprocess exits.
        let mcp_temp = if let (Some(ref bridge_tools), Some(ref ws)) =
            (&context.bridge_tools, &workspace)
        {
            if !bridge_tools.is_empty() {
                let tengu_bin = std::env::current_exe()
                    .unwrap_or_else(|_| PathBuf::from("tengu"))
                    .to_string_lossy()
                    .to_string();
                let mcp_limit = context.max_mcp_result_chars.unwrap_or(50_000);
                let config = self.build_mcp_config_json(&tengu_bin, ws, bridge_tools, mcp_limit);
                let mut tmp = tempfile::NamedTempFile::new()?;
                serde_json::to_writer(&mut tmp, &config)?;
                cmd.arg("--mcp-config").arg(tmp.path());

                // --tools only allowlists BUILT-IN tools; MCP tools need --allowedTools.
                // Without this, the tengu-tools server spawns but its tools are silently
                // denied at call time and never appear in the session init manifest.
                let mcp_tool_args: Vec<String> = bridge_tools
                    .iter()
                    .map(|t| format!("mcp__tengu-tools__{}", t.name))
                    .collect();
                cmd.arg("--allowedTools").args(&mcp_tool_args);

                Some(tmp)
            } else {
                None
            }
        } else {
            None
        };

        debug!(
            profile = ?self.profile,
            workspace = ?workspace,
            bridge_tools = context.bridge_tools.as_ref().map(|t| t.len()).unwrap_or(0),
            prompt_len = prompt.len(),
            timeout_secs = self.timeout_secs,
            tools = %tools_arg,
            "Spawning Claude Code CLI subprocess (stream-json)"
        );

        // Spawn subprocess
        let mut child = cmd.spawn().map_err(|e| {
            anyhow::anyhow!("Failed to spawn claude CLI at {:?}: {}", self.cli_path, e)
        })?;

        // Write prompt to stdin, then close it to signal EOF
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(prompt.as_bytes()).await?;
            stdin.shutdown().await?;
        }

        // Take stdout for NDJSON streaming
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("Failed to capture stdout from Claude CLI"))?;

        let timeout_secs = self.timeout_secs;
        let max_tool_rounds = context.max_tool_rounds.unwrap_or(70);
        let (tx, rx) = tokio::sync::mpsc::channel::<StreamEvent>(64);

        // Spawn async reader task that processes NDJSON lines and emits StreamEvents
        tokio::spawn(async move {
            // Keep temp file alive until subprocess exits
            let _mcp_temp = mcp_temp;

            let reader = tokio::io::BufReader::new(stdout);
            let mut lines = reader.lines();
            let mut emitted_text = false;
            let mut tool_call_count: u32 = 0;
            let idle_timeout = Duration::from_secs(timeout_secs);

            let mut timed_out = false;
            let mut tool_limit_hit = false;
            loop {
                match tokio::time::timeout(idle_timeout, lines.next_line()).await {
                    Ok(Ok(Some(line))) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        let events =
                            process_ndjson_line(&line, &mut emitted_text, &mut tool_call_count);
                        for event in events {
                            if tx.send(event).await.is_err() {
                                break;
                            }
                        }
                        // Enforce max tool rounds — kill subprocess if exceeded
                        if tool_call_count > max_tool_rounds {
                            tool_limit_hit = true;
                            break;
                        }
                    }
                    Ok(Ok(None)) => break, // EOF
                    Ok(Err(e)) => {
                        warn!(error = %e, "Error reading Claude CLI stdout");
                        break;
                    }
                    Err(_) => {
                        timed_out = true;
                        break;
                    }
                }
            }

            if tool_limit_hit {
                error!(
                    tool_call_count,
                    max_tool_rounds, "Claude Code max tool rounds exceeded — killing subprocess"
                );
                let _ = child.kill().await;
                let _ = tx
                    .send(StreamEvent::Error {
                        message: format!(
                            "Max tool rounds exceeded ({} calls, limit {}). \
                             The agent may be stuck in a retry loop.",
                            tool_call_count, max_tool_rounds
                        ),
                    })
                    .await;
            } else if timed_out {
                error!(timeout_secs, "Claude CLI idle timeout — killing subprocess");
                let _ = child.kill().await;
                let _ = tx
                    .send(StreamEvent::Error {
                        message: format!(
                            "Claude CLI idle timeout — no output for {}s",
                            timeout_secs
                        ),
                    })
                    .await;
            } else {
                let _ = tx.send(StreamEvent::Done).await;
            }

            // Wait for subprocess to exit and log any stderr
            match child.wait_with_output().await {
                Ok(output) => {
                    let stderr_str = String::from_utf8_lossy(&output.stderr);
                    if !stderr_str.is_empty() {
                        warn!(stderr = %stderr_str, "Claude CLI stderr");
                    }
                    if !output.status.success() {
                        warn!(
                            exit_code = output.status.code(),
                            "Claude CLI exited with non-zero status"
                        );
                    }
                }
                Err(e) => {
                    warn!(error = %e, "Failed to wait for Claude CLI process");
                }
            }
        });

        // Return the channel receiver as an async stream
        Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }
}
