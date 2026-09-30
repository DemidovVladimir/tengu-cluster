//! Claude Code engine — runs agents through the local Claude CLI subprocess.
//!
//! Uses `claude -p --output-format stream-json` for NDJSON streaming. Claude CLI
//! spawns as a subprocess, connects to the `tengu mcp-bridge` for Tengu-native
//! tools, and uses its own native workspace tools (Read, Write, Bash, etc.) per
//! `builtin_tools_profile`. `--strict-mcp-config` on every run: the bridge is
//! its only MCP server (`cli_args`).
//!
//! The NDJSON stream yields per-turn events: system init (session_id), assistant
//! messages (text + tool activity), and a final result with cost/usage metrics.
//! Each `tool_use` → `tool_result` pair becomes `StreamEvent::ToolRan` (name
//! without the `mcp__tengu-tools__` prefix, `ok = !is_error`): the harness
//! never runs these calls, so this is its only record of them.

use anyhow::Result;
use async_trait::async_trait;
use futures::stream;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::time::Duration;
use tracing::{debug, error, warn};

use crate::domain::message::{Message, ModelInfo, Role, StreamEvent, ToolDef};
use crate::ports::engine::EngineContext;
use crate::ports::engine::{Engine, EngineDiagnostics};

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
    /// The bridge prefers the agent's scopes from `bridge_agent`'s config;
    /// this map is its fallback when it cannot resolve the agent.
    scopes: std::collections::HashMap<String, crate::domain::scope::ToolScope>,
    /// `[agents.<id>]` + the absolute config file it came from, exported as
    /// `TENGU_BRIDGE_AGENT` / `TENGU_CONFIG`: the bridge loads that block, so
    /// bridged tools see the same config as in-process ones. `None` (planner
    /// engine) = the bridge's standalone fallback.
    bridge_agent: Option<(String, PathBuf)>,
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
            bridge_agent: None,
        }
    }

    /// Attach the agent's per-tool scope map (see `scopes` field). Call from
    /// `engines::build_engine` with `agent_config.scopes.clone()`.
    pub fn with_scopes(
        mut self,
        scopes: std::collections::HashMap<String, crate::domain::scope::ToolScope>,
    ) -> Self {
        self.scopes = scopes;
        self
    }

    /// Name the agent and its config file for the bridge (see
    /// `bridge_agent`). `config` is made absolute here — the bridge runs in
    /// the agent workspace. `engines::build_engine` passes the agent id and
    /// `config::paths::default_config_path()`.
    pub fn with_bridge_agent(mut self, agent_id: &str, config: &std::path::Path) -> Self {
        self.bridge_agent = Some((
            agent_id.to_string(),
            crate::config::paths::absolute_path(config),
        ));
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
        mcp_servers: &[crate::config::McpServerConfig],
    ) -> serde_json::Value {
        let tools_json = serde_json::to_string(bridge_tools).unwrap_or_else(|_| "[]".into());
        // Per-tool scopes cross the process boundary as JSON — the bridge's
        // fallback when it cannot resolve `bridge_agent` from the config.
        let scopes_json = serde_json::to_string(&self.scopes).unwrap_or_else(|_| "{}".into());
        let mut env = serde_json::json!({
            "TENGU_BRIDGE_WORKSPACE": workspace.to_string_lossy(),
            "TENGU_BRIDGE_TOOLS": tools_json,
            "TENGU_BRIDGE_MAX_RESULT_CHARS": max_mcp_result_chars.to_string()
        });
        env[crate::adapters::outbound::bridge_env::TENGU_BRIDGE_SCOPES_ENV] =
            serde_json::Value::String(scopes_json);
        // Agent + config file: the bridge builds its tools from that
        // `[agents.<id>]` block (sandbox sections, scopes, `no_shell`).
        if let Some((agent, config)) = &self.bridge_agent {
            env[crate::adapters::outbound::bridge_env::TENGU_BRIDGE_AGENT_ENV] =
                serde_json::Value::String(agent.clone());
            env[crate::config::paths::TENGU_CONFIG_ENV] =
                serde_json::Value::String(config.to_string_lossy().into_owned());
        }
        // The CLI merges this `env` over its own inherited env (verified with
        // CLI 2.1.285, `docs/mcp-bridge.md` § Env), so the bridge also gets
        // this process's env — vault secrets included. The keys below are
        // forwarded explicitly anyway, as part of the documented contract.
        //
        // External `[[mcp_servers]]` with a `{server}__{tool}` entry in
        // `bridge_tools`: the bridge reconnects to them and proxies the calls
        // under its egress policy, resolving their `$VAR` references.
        use crate::adapters::outbound::mcp_client::{is_server_tool, referenced_env_vars};
        let servers: Vec<&crate::config::McpServerConfig> = mcp_servers
            .iter()
            .filter(|s| {
                bridge_tools
                    .iter()
                    .any(|t| is_server_tool(&s.name, &t.name))
            })
            .collect();
        if !servers.is_empty() {
            env[crate::adapters::outbound::bridge_env::TENGU_BRIDGE_MCP_SERVERS_ENV] =
                serde_json::Value::String(
                    serde_json::to_string(&servers).unwrap_or_else(|_| "[]".into()),
                );
            for var in servers.iter().flat_map(|s| referenced_env_vars(s)) {
                if let Ok(v) = std::env::var(&var) {
                    env[var] = serde_json::Value::String(v);
                }
            }
        }
        // The bridge runs the tools — it must apply the parent's egress policy.
        env[crate::adapters::outbound::egress::EGRESS_ENV] =
            serde_json::Value::String(crate::adapters::outbound::egress::policy().child_env());
        // Forward persistent store chunk config if set in the parent process.
        if let Ok(v) = std::env::var("TENGU_PERSISTENT_STORE_CHUNK_SIZE") {
            env["TENGU_PERSISTENT_STORE_CHUNK_SIZE"] = serde_json::Value::String(v);
        }
        if let Ok(v) = std::env::var("TENGU_PERSISTENT_STORE_CHUNK_OVERLAP") {
            env["TENGU_PERSISTENT_STORE_CHUNK_OVERLAP"] = serde_json::Value::String(v);
        }
        // Phase 7.6 — session id, so `agentic_memory` `capture` in the
        // bridge stamps `session_id` on Open Brain writes when the LLM omits it.
        if let Ok(v) = std::env::var("TENGU_SESSION_ID") {
            env["TENGU_SESSION_ID"] = serde_json::Value::String(v);
        }
        // The names of the vault vars: the bridge registers their values for
        // redaction, and a `tengu` run by a bridge tool (`run_command`) does
        // not re-prompt for the vault password on the terminal the TUI owns.
        let loaded = crate::adapters::outbound::secrets::SECRETS_LOADED_ENV;
        if let Ok(v) = std::env::var(loaded) {
            env[loaded] = serde_json::Value::String(v);
        }
        // OPENROUTER_API_KEY — the bridge's memory backend (DiskVectorStore
        // + Embedder) needs it.
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

/// `claude` arguments for one run — all but the prompt (stdin), the working
/// directory and the env. Pure, so tests pin the hardening flags.
///
/// | Arg | Why |
/// |---|---|
/// | `--strict-mcp-config` | always: only the `--mcp-config` servers (the tengu bridge), never the operator's user / project / plugin MCP servers — they run outside tengu scopes and egress. No bridge = no MCP server |
/// | `--tools <profile>` | built-in tools; `""` = none (`builtin_tools_profile = "none"`) |
/// | `--mcp-config <file>` `--allowedTools mcp__tengu-tools__<name>…` | the bridge; `--tools` covers built-ins only, so each bridged tool is allowed by name |
fn cli_args(
    profile: BuiltinToolsProfile,
    model: Option<&str>,
    system_prompt: Option<&str>,
    bridge: Option<(&std::path::Path, &[ToolDef])>,
) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = [
        "-p",
        "--output-format",
        "stream-json",
        "--verbose",
        "--dangerously-skip-permissions",
        "--no-session-persistence",
        "--strict-mcp-config",
    ]
    .into_iter()
    .map(Into::into)
    .collect();
    if let Some(model) = model {
        args.extend(["--model".into(), model.into()]);
    }
    if let Some(sp) = system_prompt {
        args.extend(["--system-prompt".into(), sp.into()]);
    }
    args.extend(["--tools".into(), profile.cli_tools_arg().into()]);
    if let Some((config, tools)) = bridge {
        args.extend(["--mcp-config".into(), config.as_os_str().to_owned()]);
        // Variadic flag: last, and never bare (the CLI rejects a value-less one).
        if !tools.is_empty() {
            args.push("--allowedTools".into());
            args.extend(
                tools
                    .iter()
                    .map(|t| format!("mcp__tengu-tools__{}", t.name).into()),
            );
        }
    }
    args
}

/// The tengu name of a tool the CLI called: bridged tools arrive as
/// `mcp__tengu-tools__<name>`, built-ins (`Read`, `Bash`) as they are.
fn tengu_tool_name(cli_name: &str) -> &str {
    cli_name
        .strip_prefix("mcp__tengu-tools__")
        .unwrap_or(cli_name)
}

/// Process a single NDJSON line from the Claude CLI stream.
///
/// Returns events to emit. Mutates `emitted_text` to track whether any
/// assistant text has been sent (used to decide whether result.result
/// needs to be emitted as a fallback). `tool_names` maps each `tool_use` id
/// to its tool until the matching `tool_result` turns into
/// `StreamEvent::ToolRan` (the CLI runs the tools; this is the activity).
fn process_ndjson_line(
    line: &str,
    emitted_text: &mut bool,
    tool_call_count: &mut u32,
    tool_names: &mut std::collections::HashMap<String, String>,
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
                                if let Some(id) = block.get("id").and_then(|v| v.as_str()) {
                                    tool_names
                                        .insert(id.to_string(), tengu_tool_name(name).to_string());
                                }
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
                        events.push(StreamEvent::ToolRan {
                            name: tool_names
                                .remove(tool_id)
                                .unwrap_or_else(|| "unknown".to_string()),
                            ok: !is_error,
                        });
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
            .map(|p| crate::config::paths::expand_tilde(p));

        // Build subprocess command (arguments: `cli_args`, after the bridge
        // config is written)
        let mut cmd = tokio::process::Command::new(&self.cli_path);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .env_remove("ANTHROPIC_API_KEY");
        // `[egress]`: with `route_llm_api` the CLI's own Anthropic traffic
        // goes through the proxy's HTTP CONNECT port (Arti serves it on 9050).
        for (k, v) in crate::adapters::outbound::egress::policy().claude_cli_env() {
            cmd.env(k, v);
        }

        // Working directory
        if let Some(ref ws) = workspace {
            cmd.current_dir(ws);
        }

        // MCP config for Tengu bridge tools — write to temp file, keep handle alive
        // The temp file is moved into the spawned task so it stays alive until the
        // subprocess exits.
        let mcp_temp =
            if let (Some(ref bridge_tools), Some(ref ws)) = (&context.bridge_tools, &workspace) {
                if !bridge_tools.is_empty() {
                    let tengu_bin = std::env::current_exe()
                        .unwrap_or_else(|_| PathBuf::from("tengu"))
                        .to_string_lossy()
                        .to_string();
                    let mcp_limit = context.max_mcp_result_chars.unwrap_or(50_000);
                    let config = self.build_mcp_config_json(
                        &tengu_bin,
                        ws,
                        bridge_tools,
                        mcp_limit,
                        &context.mcp_servers,
                    );
                    let mut tmp = tempfile::NamedTempFile::new()?;
                    serde_json::to_writer(&mut tmp, &config)?;
                    Some(tmp)
                } else {
                    None
                }
            } else {
                None
            };

        // `--tools` only allowlists BUILT-IN tools; without `--allowedTools`
        // the bridge's tools are silently denied at call time.
        let bridge = mcp_temp.as_ref().map(|tmp| {
            let tools: &[ToolDef] = context.bridge_tools.as_deref().unwrap_or_default();
            (tmp.path(), tools)
        });
        cmd.args(cli_args(
            self.profile,
            self.model.as_deref(),
            context.system_prompt.as_deref(),
            bridge,
        ));

        debug!(
            profile = ?self.profile,
            workspace = ?workspace,
            bridge_tools = context.bridge_tools.as_ref().map(|t| t.len()).unwrap_or(0),
            prompt_len = prompt.len(),
            timeout_secs = self.timeout_secs,
            "Spawning Claude Code CLI subprocess (stream-json, --strict-mcp-config)"
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
            let mut tool_names = std::collections::HashMap::new();
            let idle_timeout = Duration::from_secs(timeout_secs);

            let mut timed_out = false;
            let mut tool_limit_hit = false;
            loop {
                match tokio::time::timeout(idle_timeout, lines.next_line()).await {
                    Ok(Ok(Some(line))) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        let events = process_ndjson_line(
                            &line,
                            &mut emitted_text,
                            &mut tool_call_count,
                            &mut tool_names,
                        );
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

#[cfg(test)]
mod tests {
    use super::*;

    fn server(name: &str, env: &[(&str, &str)]) -> crate::config::McpServerConfig {
        crate::config::McpServerConfig {
            name: name.to_string(),
            transport: "stdio".to_string(),
            command: vec!["true".to_string()],
            url: None,
            env: env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            auth: None,
        }
    }

    fn strings(args: Vec<std::ffi::OsString>) -> Vec<String> {
        args.into_iter()
            .map(|a| a.into_string().expect("utf-8 arg"))
            .collect()
    }

    fn value_after<'a>(args: &'a [String], flag: &str) -> &'a str {
        let i = args.iter().position(|a| a == flag).expect(flag);
        &args[i + 1]
    }

    /// `--strict-mcp-config` rides on every run — with the bridge (only its
    /// servers) and without one (no MCP server at all).
    #[test]
    fn cli_args_always_pass_strict_mcp_config() {
        let bare = strings(cli_args(BuiltinToolsProfile::None, None, None, None));
        assert!(bare.iter().any(|a| a == "--strict-mcp-config"), "{bare:?}");
        assert_eq!(
            value_after(&bare, "--tools"),
            "",
            "profile none = no built-ins"
        );
        assert!(!bare
            .iter()
            .any(|a| a == "--mcp-config" || a == "--allowedTools" || a == "--model"));

        let tools = vec![
            ToolDef::new("read_file", "d", serde_json::json!({})),
            ToolDef::new("fake__echo", "d", serde_json::json!({})),
        ];
        let cfg = std::path::Path::new("/tmp/tengu-mcp.json");
        let full = strings(cli_args(
            BuiltinToolsProfile::ReadOnly,
            Some("claude-haiku-4-5"),
            Some("be brief"),
            Some((cfg, &tools)),
        ));
        assert!(full.iter().any(|a| a == "--strict-mcp-config"), "{full:?}");
        assert_eq!(value_after(&full, "--model"), "claude-haiku-4-5");
        assert_eq!(value_after(&full, "--system-prompt"), "be brief");
        assert_eq!(value_after(&full, "--tools"), "Read,Glob,Grep");
        let i = full.iter().position(|a| a == "--mcp-config").unwrap();
        assert_eq!(
            full[i..],
            [
                "--mcp-config",
                "/tmp/tengu-mcp.json",
                "--allowedTools",
                "mcp__tengu-tools__read_file",
                "mcp__tengu-tools__fake__echo",
            ],
            "variadic --allowedTools comes last"
        );

        let no_tools = strings(cli_args(
            BuiltinToolsProfile::None,
            None,
            None,
            Some((cfg, &[])),
        ));
        assert!(
            !no_tools.iter().any(|a| a == "--allowedTools"),
            "never bare"
        );
    }

    /// `tool_use` → `tool_result` pairs become `ToolRan` (bridge prefix
    /// stripped, `ok = !is_error`, built-ins keep their name); a result with
    /// no known `tool_use` is `unknown`; text still streams.
    #[test]
    fn tool_results_become_tool_ran_events() {
        let lines = [
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"listing"},{"type":"tool_use","id":"toolu_1","name":"mcp__tengu-tools__list_directory","input":{"path":"."}},{"type":"tool_use","id":"toolu_2","name":"Read","input":{}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","content":[{"type":"text","text":"a.txt"}]},{"type":"tool_result","tool_use_id":"toolu_2","is_error":true,"content":"denied"}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_9","is_error":false,"content":"x"}]}}"#,
        ];
        let (mut text, mut count, mut names) = (false, 0u32, std::collections::HashMap::new());
        let events: Vec<StreamEvent> = lines
            .iter()
            .flat_map(|l| process_ndjson_line(l, &mut text, &mut count, &mut names))
            .collect();
        let ran: Vec<(String, bool)> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolRan { name, ok } => Some((name.clone(), *ok)),
                _ => None,
            })
            .collect();
        assert_eq!(
            ran,
            [
                ("list_directory".to_string(), true),
                ("Read".to_string(), false),
                ("unknown".to_string(), true),
            ]
        );
        assert!(matches!(&events[0], StreamEvent::TextDelta { text } if text == "listing"));
        assert_eq!(count, 2);
        assert!(names.is_empty(), "every tool_use was resolved: {names:?}");
    }

    /// The bridge env names the agent and its config file (absolute — the
    /// bridge runs in the agent workspace); a planner engine names neither.
    #[test]
    fn bridge_env_names_the_agent_and_its_config_file() {
        let engine =
            ClaudeCodeEngine::new(PathBuf::from("claude"), BuiltinToolsProfile::None, None, 60);
        let tools = [ToolDef::new("read_file", "d", serde_json::json!({}))];
        let ws = std::path::Path::new("/tmp");
        let plain = engine.build_mcp_config_json("tengu", ws, &tools, 1000, &[]);
        let env = &plain["mcpServers"]["tengu-tools"]["env"];
        assert!(env.get("TENGU_BRIDGE_AGENT").is_none());
        assert!(env.get("TENGU_CONFIG").is_none());

        let engine = engine.with_bridge_agent(
            "xm_architect",
            std::path::Path::new("sandboxes/xmarket/config.toml"),
        );
        let cfg = engine.build_mcp_config_json("tengu", ws, &tools, 1000, &[]);
        let env = &cfg["mcpServers"]["tengu-tools"]["env"];
        assert_eq!(env["TENGU_BRIDGE_AGENT"], "xm_architect");
        let config = std::path::Path::new(env["TENGU_CONFIG"].as_str().unwrap());
        assert!(config.is_absolute(), "{}", config.display());
        assert!(config.ends_with("sandboxes/xmarket/config.toml"));
    }

    #[test]
    fn bridge_config_carries_only_requested_mcp_servers_and_their_vars() {
        let engine = ClaudeCodeEngine::new(
            PathBuf::from("claude"),
            BuiltinToolsProfile::ReadOnly,
            None,
            60,
        );
        let tools = vec![ToolDef::new("fake__echo", "d", serde_json::json!({}))];
        // `$HOME` is always set, so the forwarded value is observable.
        let servers = vec![
            server("fake", &[("TOKEN", "$HOME")]),
            server("unused", &[("OTHER", "$PATH")]),
        ];
        let cfg = engine.build_mcp_config_json(
            "tengu",
            std::path::Path::new("/tmp"),
            &tools,
            1000,
            &servers,
        );
        let env = &cfg["mcpServers"]["tengu-tools"]["env"];
        let passed: Vec<crate::config::McpServerConfig> = serde_json::from_str(
            env[crate::adapters::outbound::bridge_env::TENGU_BRIDGE_MCP_SERVERS_ENV]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(passed.len(), 1);
        assert_eq!(passed[0].name, "fake");
        assert_eq!(env["HOME"], std::env::var("HOME").unwrap());
        assert!(
            env.get("PATH").is_none(),
            "unused server's vars must not leak"
        );

        let none = engine.build_mcp_config_json(
            "tengu",
            std::path::Path::new("/tmp"),
            &[ToolDef::new("read_file", "d", serde_json::json!({}))],
            1000,
            &servers,
        );
        assert!(none["mcpServers"]["tengu-tools"]["env"]
            .get(crate::adapters::outbound::bridge_env::TENGU_BRIDGE_MCP_SERVERS_ENV)
            .is_none());
    }
}
