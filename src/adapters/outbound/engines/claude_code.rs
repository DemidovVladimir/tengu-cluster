//! Claude Code engine — runs agents through the local Claude CLI subprocess.
//!
//! Uses `claude -p --output-format stream-json` for NDJSON streaming. Claude CLI
//! spawns as a subprocess, connects to the `tengu mcp-bridge` for Tengu-native
//! tools, and uses its own native workspace tools (Read, Write, Bash, etc.) per
//! `builtin_tools_profile`. `--strict-mcp-config` on every run: the bridge is
//! its only MCP server; profile `none` (the hardened one) also drops settings
//! files, hooks, plugins, skills, CLAUDE.md discovery and auto-memory
//! (`cli_args`).
//!
//! The NDJSON stream yields per-turn events: system init (session_id), assistant
//! messages (text + tool activity), and a final result with cost/usage metrics.
//! Each `tool_use` → `tool_result` pair becomes `StreamEvent::ToolRan` (name
//! without the `mcp__tengu-tools__` prefix, `ok = !is_error`): the harness
//! never runs these calls, so this is its only record of them.
//!
//! | Run with a bridge | Handling |
//! |---|---|
//! | the system prompt | `--system-prompt` only — a system message equal to it is not repeated in the stdin prompt (`format_prompt`) |
//! | a bridged tool's conversation (`ToolCtx.conversation`, what `skill_distill` reads) | [`Transcript`]: the run's messages + every streamed assistant message and tool result, in a 0600 temp file the bridge reads per call (`TENGU_BRIDGE_TRANSCRIPT_FILE`) |
//! | `[[mcp_servers]]` behind `{server}__{tool}` bridge tools | their names only (`TENGU_BRIDGE_MCP_SERVERS`): the bridge takes each from the config it loads — no value of the config (a `${VAR}`-expanded secret) is written to the temp `--mcp-config` |
//! | `compress_and_store` succeeded (a `run-agent` step) | the run ends once the round's other calls are answered — the CLI is stopped, as the in-process loop stops after its round; the run's usage so far is reported as the `result` line would (`RunUsage`) |
//! | `max_tool_rounds` | counts tool calls (`tool_use` blocks) of the run, not rounds: past it the CLI is killed with an error |

use anyhow::Result;
use async_trait::async_trait;
use futures::stream;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::time::Duration;
use tracing::{debug, error, info, warn};

use crate::adapters::outbound::tools::skill_lifecycle::compress_and_store;
use crate::domain::message::{Message, ModelInfo, Role, StreamEvent, ToolCall, ToolDef};
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
    /// `[agents.<a>.claude_code] builtin_tools_profile`, surrounding
    /// whitespace ignored (`" none"` is `none`, as `Config::validate` and the
    /// hardening rule read it). `None` for any other value — never a silent
    /// `editor_shell`; config validation refuses it at load.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "none" => Some(Self::None),
            "read_only" => Some(Self::ReadOnly),
            "editor" => Some(Self::Editor),
            "editor_shell" => Some(Self::EditorShell),
            _ => None,
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
    /// What a `run-agent` step (or the doctor's smoke turn, run like one)
    /// asks of its bridge (`with_step_bridge`); default = nothing.
    step: StepBridge,
}

/// A `run-agent` step's bridge options, written into the `--mcp-config` env
/// (`adapters/outbound/bridge_env.rs`). Explicit, never inherited from the
/// process env.
#[derive(Debug, Clone, Default)]
pub(crate) struct StepBridge {
    /// `TENGU_BRIDGE_GRANT_WORKSPACE=1`: every configured scope also gets
    /// the workspace as an fs root — what the step's own executor does
    /// (`bootstrap::tools::grant_workspace_root`).
    pub grant_workspace: bool,
    /// `TENGU_BRIDGE_SUMMARY_FILE`: the bridge serves `compress_and_store`
    /// by writing the summary here; the step reads it back as its IPC
    /// summary. `None` = the bridge refuses the call with the reason.
    pub summary_file: Option<PathBuf>,
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
            step: StepBridge::default(),
        }
    }

    /// Run as a `run-agent` step (see [`StepBridge`]);
    /// `engines::build_step_engine` calls it.
    pub fn with_step_bridge(mut self, step: StepBridge) -> Self {
        self.step = step;
        self
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
    /// A system message equal to `system_prompt` is left out: the CLI gets
    /// that one as `--system-prompt` (`run-agent`, `tengu tool turn`, the
    /// doctor and webhooks send it both ways).
    fn format_prompt(messages: &[Message], system_prompt: Option<&str>) -> String {
        let mut parts = Vec::new();
        let system_prompt = system_prompt.filter(|s| !s.trim().is_empty());

        for msg in messages {
            if matches!(msg.role, Role::System) && Some(msg.content.as_str()) == system_prompt {
                continue;
            }
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

    /// Build MCP config JSON for the tengu-tools bridge server. `transcript`:
    /// the run's [`Transcript`] file (`TENGU_BRIDGE_TRANSCRIPT_FILE`).
    fn build_mcp_config_json(
        &self,
        tengu_bin: &str,
        workspace: &std::path::Path,
        bridge_tools: &[ToolDef],
        max_mcp_result_chars: u32,
        mcp_servers: &[crate::config::McpServerConfig],
        transcript: Option<&Path>,
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
        if self.step.grant_workspace {
            env[crate::adapters::outbound::bridge_env::TENGU_BRIDGE_GRANT_WORKSPACE_ENV] =
                serde_json::Value::String("1".into());
        }
        if let Some(file) = &self.step.summary_file {
            env[crate::adapters::outbound::bridge_env::TENGU_BRIDGE_SUMMARY_FILE_ENV] =
                serde_json::Value::String(file.to_string_lossy().into_owned());
        }
        if let Some(file) = transcript {
            env[crate::adapters::outbound::bridge_env::TENGU_BRIDGE_TRANSCRIPT_FILE_ENV] =
                serde_json::Value::String(file.to_string_lossy().into_owned());
        }
        // The CLI merges this `env` over its own inherited env (verified with
        // CLI 2.1.285, `docs/mcp-bridge.md` § Env), so the bridge inherits
        // this process's env — vault secrets, `OPENROUTER_API_KEY` and the
        // vars `[[mcp_servers]]` `$VAR` references name. No secret value is
        // written into this file (it sits on disk for the whole run).
        //
        // External `[[mcp_servers]]` with a `{server}__{tool}` entry in
        // `bridge_tools`: their NAMES only — the bridge takes each server
        // from the config it loads (`TENGU_CONFIG`, `Config::load`: `${VAR}`
        // expanded there, from the same inherited env), reconnects and
        // proxies the calls under its egress policy, resolving `$VAR`
        // references from that env. A `${VAR}` value `Config::load` already
        // expanded here never reaches the file.
        use crate::adapters::outbound::mcp_client::is_server_tool;
        let servers: Vec<&str> = mcp_servers
            .iter()
            .filter(|s| {
                bridge_tools
                    .iter()
                    .any(|t| is_server_tool(&s.name, &t.name))
            })
            .map(|s| s.name.as_str())
            .collect();
        if !servers.is_empty() {
            env[crate::adapters::outbound::bridge_env::TENGU_BRIDGE_MCP_SERVERS_ENV] =
                serde_json::Value::String(
                    serde_json::to_string(&servers).unwrap_or_else(|_| "[]".into()),
                );
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
        // The names of the vault vars (names only): the bridge registers
        // their values for redaction, and a `tengu` run by a bridge tool
        // (`run_command`) does not re-prompt for the vault password on the
        // terminal the TUI owns.
        let loaded = crate::adapters::outbound::secrets::SECRETS_LOADED_ENV;
        if let Ok(v) = std::env::var(loaded) {
            env[loaded] = serde_json::Value::String(v);
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

/// Operator configuration under the `CLAUDE_CODE_` prefix that a nested
/// `claude` keeps: auth (`claude setup-token`), the provider switch and
/// client certificates. Every other `CLAUDE_CODE_*` var is a parent session's.
const KEPT_CLAUDE_CODE_ENV: &[&str] = &[
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_API_KEY_HELPER_TTL_MS",
    "CLAUDE_CODE_CLIENT_CERT",
    "CLAUDE_CODE_CLIENT_KEY",
    "CLAUDE_CODE_CLIENT_KEY_PASSPHRASE",
];

/// Env a parent Claude Code session sets (tengu run from inside one): a
/// nested `claude` must start like one from the operator's terminal, not as
/// that session's child (its messaging socket + token, session id, effort).
///
/// | Removed | Kept |
/// |---|---|
/// | `CLAUDECODE`, `CLAUDE_PID`, `CLAUDE_EFFORT`, every `CLAUDE_CODE_*` | [`KEPT_CLAUDE_CODE_ENV`], `CLAUDE_CODE_USE_*` / `CLAUDE_CODE_SKIP_*_AUTH` (provider), `PATH`, `HOME`, `CLAUDE_CONFIG_DIR`, the egress proxy env; `ANTHROPIC_API_KEY` is removed separately (subscription) |
fn parent_session_env<'a>(names: impl IntoIterator<Item = &'a str>) -> Vec<&'a str> {
    names
        .into_iter()
        .filter(|n| {
            matches!(*n, "CLAUDECODE" | "CLAUDE_PID" | "CLAUDE_EFFORT")
                || n.strip_prefix("CLAUDE_CODE_").is_some_and(|rest| {
                    !KEPT_CLAUDE_CODE_ENV.contains(n)
                        && !rest.starts_with("USE_")
                        && !(rest.starts_with("SKIP_") && rest.ends_with("_AUTH"))
                })
        })
        .collect()
}

/// The settings a `builtin_tools_profile = "none"` run adds through
/// `--settings` (which still applies under `--setting-sources ""`):
/// no auto-memory, no hooks from any source.
const ISOLATED_SETTINGS: &str = r#"{"autoMemoryEnabled":false,"disableAllHooks":true}"#;

/// `claude` arguments for one run — all but the prompt (stdin), the working
/// directory and the env. Pure, so tests pin the hardening flags.
///
/// | Arg | Why |
/// |---|---|
/// | `--strict-mcp-config` | always: only the `--mcp-config` servers (the tengu bridge), never the operator's user / project / plugin MCP servers — they run outside tengu scopes and egress. No bridge = no MCP server |
/// | `--tools <profile>` | built-in tools; `""` = none (`builtin_tools_profile = "none"`) |
/// | profile `none` (the hardened one, `config/hardening.rs`): `--setting-sources ""` `--disable-slash-commands` `--settings` [`ISOLATED_SETTINGS`] | no user / project / local settings files (their hooks and installed plugins run shell commands outside tengu scopes; their `env`), no project `CLAUDE.md` / `AGENTS.md` auto-discovery, no skills or custom commands, no auto-memory, no hooks at all. Verified on CLI 2.1.286: subscription OAuth and the `--mcp-config` bridge still work. Not `--safe-mode` (drops the `--mcp-config` bridge too) nor `--bare` (no OAuth / keychain) |
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
    if profile == BuiltinToolsProfile::None {
        args.extend(
            [
                "--setting-sources",
                "",
                "--disable-slash-commands",
                "--settings",
                ISOLATED_SETTINGS,
            ]
            .map(Into::into),
        );
    }
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

/// The conversation a bridged tool sees (`ToolCtx.conversation` — what
/// `skill_distill` seeds fixtures from), kept for the bridge in a temp file
/// (`TENGU_BRIDGE_TRANSCRIPT_FILE`; mode 0600, removed when the run ends):
/// the messages this run was given — what an in-process engine's tools see —
/// then each streamed assistant message (text, `tool_use` → `tool_calls`
/// under the tengu name) and `tool_result` (→ a tool message). Rewritten
/// after every line that changes it, atomically (a sibling file renamed
/// over it): the bridge reads it per call and never sees half a write.
struct Transcript {
    path: tempfile::TempPath,
    messages: Vec<Message>,
    /// Index of the first streamed message: a streamed assistant line
    /// merges into a streamed assistant message only.
    streamed_from: usize,
}

impl Transcript {
    fn create(messages: &[Message]) -> Result<Self> {
        let path = tempfile::Builder::new()
            .prefix("tengu-transcript-")
            .tempfile()?
            .into_temp_path();
        let transcript = Self {
            path,
            messages: messages.to_vec(),
            streamed_from: messages.len(),
        };
        transcript.write()?;
        Ok(transcript)
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// One NDJSON line of the run; rewrites the file when it changed the
    /// conversation. A failed write leaves the last one (warned).
    fn absorb(&mut self, line: &str) {
        if absorb_ndjson(&mut self.messages, self.streamed_from, line) {
            if let Err(e) = self.write() {
                warn!(error = %e, file = %self.path.display(), "Claude Code: transcript for the bridge not updated");
            }
        }
    }

    fn write(&self) -> Result<()> {
        let dir = self.path.parent().unwrap_or_else(|| Path::new("."));
        let mut next = tempfile::NamedTempFile::new_in(dir)?;
        serde_json::to_writer(&mut next, &self.messages)?;
        next.persist(&*self.path)?;
        Ok(())
    }
}

/// One stream line into `messages` (see [`Transcript`]); `true` when it
/// changed them. The CLI emits a line per content block, so consecutive
/// streamed assistant lines — one API response — form one message, as the
/// in-process loop's one assistant message per round.
fn absorb_ndjson(messages: &mut Vec<Message>, streamed_from: usize, line: &str) -> bool {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(line) else {
        return false;
    };
    let Some(blocks) = json
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
    else {
        return false;
    };
    match str_field(&json, "type") {
        "assistant" => {
            let mut text = String::new();
            let mut calls = Vec::new();
            for block in blocks {
                match str_field(block, "type") {
                    "text" => text.push_str(str_field(block, "text")),
                    "tool_use" => calls.push(ToolCall {
                        id: str_field(block, "id").to_string(),
                        name: tengu_tool_name(str_field(block, "name")).to_string(),
                        arguments: block
                            .get("input")
                            .cloned()
                            .unwrap_or_else(|| serde_json::json!({})),
                    }),
                    _ => {}
                }
            }
            if text.is_empty() && calls.is_empty() {
                return false;
            }
            let merge = messages.len() > streamed_from
                && matches!(messages.last().map(|m| &m.role), Some(Role::Assistant));
            match messages.last_mut().filter(|_| merge) {
                Some(last) => {
                    if !text.is_empty() && !last.content.is_empty() {
                        last.content.push('\n');
                    }
                    last.content.push_str(&text);
                    if !calls.is_empty() {
                        last.tool_calls.get_or_insert_with(Vec::new).extend(calls);
                    }
                }
                None => messages.push(Message {
                    role: Role::Assistant,
                    content: text,
                    tool_call_id: None,
                    tool_calls: (!calls.is_empty()).then_some(calls),
                }),
            }
            true
        }
        "user" => {
            let before = messages.len();
            for block in blocks {
                if str_field(block, "type") != "tool_result" {
                    continue;
                }
                let content = match block.get("content") {
                    Some(serde_json::Value::String(s)) => s.clone(),
                    Some(serde_json::Value::Array(parts)) => parts
                        .iter()
                        .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    Some(other) => other.to_string(),
                    None => String::new(),
                };
                messages.push(Message {
                    role: Role::Tool,
                    content,
                    tool_call_id: Some(str_field(block, "tool_use_id").to_string()),
                    tool_calls: None,
                });
            }
            messages.len() > before
        }
        _ => false,
    }
}

/// `value[key]` as a string, `""` when absent or not a string.
fn str_field<'a>(value: &'a serde_json::Value, key: &str) -> &'a str {
    value.get(key).and_then(|v| v.as_str()).unwrap_or_default()
}

/// The usage of one CLI run so far: each API response's
/// `input_tokens` / `output_tokens` (`message.usage`, the last one seen per
/// `message.id` — the CLI repeats a response's usage on each of its lines),
/// summed — what the `result` line reports when the run ends on its own.
#[derive(Default)]
struct RunUsage {
    by_message: std::collections::HashMap<String, (u32, u32)>,
}

impl RunUsage {
    fn absorb(&mut self, line: &str) {
        let Ok(json) = serde_json::from_str::<serde_json::Value>(line) else {
            return;
        };
        if str_field(&json, "type") != "assistant" {
            return;
        }
        let Some(message) = json.get("message") else {
            return;
        };
        let Some(usage) = message.get("usage") else {
            return;
        };
        let tokens = |k: &str| usage.get(k).and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let id = str_field(message, "id");
        let key = if id.is_empty() {
            format!("#{}", self.by_message.len())
        } else {
            id.to_string()
        };
        self.by_message
            .insert(key, (tokens("input_tokens"), tokens("output_tokens")));
    }

    fn total(&self) -> Option<(u32, u32)> {
        let (input, output) = self
            .by_message
            .values()
            .fold((0u32, 0u32), |(i, o), (a, b)| {
                (i.saturating_add(*a), o.saturating_add(*b))
            });
        (input > 0 || output > 0).then_some((input, output))
    }
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
        let prompt = Self::format_prompt(messages, context.system_prompt.as_deref());

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
        let inherited: Vec<String> = std::env::vars_os()
            .filter_map(|(k, _)| k.into_string().ok())
            .collect();
        for name in parent_session_env(inherited.iter().map(String::as_str)) {
            cmd.env_remove(name);
        }
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
        // subprocess exits. The run's transcript (the bridge's conversation
        // for each call) likewise.
        let mut transcript: Option<Transcript> = None;
        let mcp_temp =
            if let (Some(ref bridge_tools), Some(ref ws)) = (&context.bridge_tools, &workspace) {
                if !bridge_tools.is_empty() {
                    let tengu_bin = std::env::current_exe()
                        .unwrap_or_else(|_| PathBuf::from("tengu"))
                        .to_string_lossy()
                        .to_string();
                    let mcp_limit = context.max_mcp_result_chars.unwrap_or(50_000);
                    // Fail-soft: without it a bridged tool that reads the
                    // conversation refuses (`skill_distill`), nothing else.
                    transcript = Transcript::create(messages)
                        .map_err(
                            |e| warn!(error = %e, "Claude Code: no transcript file for the bridge"),
                        )
                        .ok();
                    let config = self.build_mcp_config_json(
                        &tengu_bin,
                        ws,
                        bridge_tools,
                        mcp_limit,
                        &context.mcp_servers,
                        transcript.as_ref().map(Transcript::path),
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
            // Keep temp files alive until subprocess exits
            let _mcp_temp = mcp_temp;
            let mut transcript = transcript;

            let reader = tokio::io::BufReader::new(stdout);
            let mut lines = reader.lines();
            let mut emitted_text = false;
            let mut tool_call_count: u32 = 0;
            let mut tool_names = std::collections::HashMap::new();
            let idle_timeout = Duration::from_secs(timeout_secs);

            let mut timed_out = false;
            let mut tool_limit_hit = false;
            // `compress_and_store` succeeded (a run-agent step's bridge stored
            // the summary): the run ends once the round's calls are answered.
            let mut summary_stored = false;
            let mut step_done = false;
            // Usage per API response: a run stopped early has no `result`
            // line, whose cumulative usage the turn's metric would take.
            let mut usage = RunUsage::default();
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
                        if let Some(t) = transcript.as_mut() {
                            t.absorb(&line);
                        }
                        usage.absorb(&line);
                        summary_stored |= events.iter().any(|e| {
                            matches!(e, StreamEvent::ToolRan { name, ok: true } if name == compress_and_store::NAME)
                        });
                        for event in events {
                            if tx.send(event).await.is_err() {
                                break;
                            }
                        }
                        if summary_stored && tool_names.is_empty() {
                            step_done = true;
                            break;
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

            if step_done {
                info!(
                    tool_call_count,
                    "Claude Code: compress_and_store stored the step summary — ending the CLI run"
                );
                let _ = child.kill().await;
                // What the `result` line would have carried: the run's usage
                // so far (the turn takes the last `Usage` frame).
                if let Some((input_tokens, output_tokens)) = usage.total() {
                    let _ = tx
                        .send(StreamEvent::Usage {
                            input_tokens,
                            output_tokens,
                        })
                        .await;
                }
                let _ = tx.send(StreamEvent::Done).await;
            } else if tool_limit_hit {
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
                    // A step ended after `compress_and_store` was killed on purpose.
                    if !output.status.success() && !step_done {
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

    /// Profile `none` (the hardened one) runs the CLI without settings
    /// files, CLAUDE.md / AGENTS.md discovery, skills, auto-memory and
    /// hooks — with the bridge still last; every other profile keeps the
    /// operator's settings.
    #[test]
    fn cli_args_isolate_the_none_profile() {
        let tools = vec![ToolDef::new("read_file", "d", serde_json::json!({}))];
        let cfg = std::path::Path::new("/tmp/tengu-mcp.json");
        let none = strings(cli_args(
            BuiltinToolsProfile::None,
            Some("claude-haiku-4-5"),
            None,
            Some((cfg, &tools)),
        ));
        assert_eq!(value_after(&none, "--setting-sources"), "");
        assert!(
            none.iter().any(|a| a == "--disable-slash-commands"),
            "{none:?}"
        );
        let settings: serde_json::Value =
            serde_json::from_str(value_after(&none, "--settings")).unwrap();
        assert_eq!(
            settings,
            serde_json::json!({"autoMemoryEnabled": false, "disableAllHooks": true})
        );
        for flag in ["--bare", "--safe-mode"] {
            assert!(
                !none.iter().any(|a| a == flag),
                "{flag} breaks OAuth or the bridge"
            );
        }
        assert_eq!(
            none[none.len() - 2..],
            ["--allowedTools", "mcp__tengu-tools__read_file"],
            "variadic --allowedTools stays last"
        );
        assert!(none.iter().any(|a| a == "--strict-mcp-config"));

        for profile in [
            BuiltinToolsProfile::ReadOnly,
            BuiltinToolsProfile::Editor,
            BuiltinToolsProfile::EditorShell,
        ] {
            let args = strings(cli_args(profile, None, None, Some((cfg, &tools))));
            assert!(
                !args.iter().any(|a| a == "--setting-sources"
                    || a == "--settings"
                    || a == "--disable-slash-commands"),
                "{profile:?}: {args:?}"
            );
        }
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
        let plain = engine.build_mcp_config_json("tengu", ws, &tools, 1000, &[], None);
        let env = &plain["mcpServers"]["tengu-tools"]["env"];
        assert!(env.get("TENGU_BRIDGE_AGENT").is_none());
        assert!(env.get("TENGU_CONFIG").is_none());
        assert!(env.get("TENGU_BRIDGE_TRANSCRIPT_FILE").is_none());

        let engine = engine.with_bridge_agent(
            "xm_architect",
            std::path::Path::new("sandboxes/xmarket/config.toml"),
        );
        let cfg = engine.build_mcp_config_json(
            "tengu",
            ws,
            &tools,
            1000,
            &[],
            Some(std::path::Path::new("/tmp/tengu-transcript-x")),
        );
        let env = &cfg["mcpServers"]["tengu-tools"]["env"];
        assert_eq!(env["TENGU_BRIDGE_AGENT"], "xm_architect");
        let config = std::path::Path::new(env["TENGU_CONFIG"].as_str().unwrap());
        assert!(config.is_absolute(), "{}", config.display());
        assert!(config.ends_with("sandboxes/xmarket/config.toml"));
        assert_eq!(
            env["TENGU_BRIDGE_TRANSCRIPT_FILE"],
            "/tmp/tengu-transcript-x"
        );
    }

    /// Every key the engine may write into the temp `--mcp-config` env
    /// (`bridge_env.rs`): names and non-secret settings only.
    const BRIDGE_ENV_KEYS: &[&str] = &[
        "TENGU_BRIDGE_WORKSPACE",
        "TENGU_BRIDGE_TOOLS",
        "TENGU_BRIDGE_MAX_RESULT_CHARS",
        "TENGU_BRIDGE_SCOPES",
        "TENGU_BRIDGE_AGENT",
        "TENGU_CONFIG",
        "TENGU_BRIDGE_GRANT_WORKSPACE",
        "TENGU_BRIDGE_SUMMARY_FILE",
        "TENGU_BRIDGE_TRANSCRIPT_FILE",
        "TENGU_BRIDGE_MCP_SERVERS",
        "TENGU_EGRESS",
        "TENGU_PERSISTENT_STORE_CHUNK_SIZE",
        "TENGU_PERSISTENT_STORE_CHUNK_OVERLAP",
        "TENGU_SESSION_ID",
        "TENGU_SECRETS_LOADED",
    ];

    /// The temp file names the requested servers but holds no secret value:
    /// no `$VAR` value of a server, no value `Config::load` expanded from a
    /// `${VAR}` (in `env`, `auth.token`, `url` or `command`), no
    /// `OPENROUTER_API_KEY` — the bridge takes the servers from the config
    /// it loads and inherits the env (the CLI merges it, `docs/mcp-bridge.md`).
    #[test]
    fn bridge_config_carries_requested_mcp_servers_and_no_secret_values() {
        let engine = ClaudeCodeEngine::new(
            PathBuf::from("claude"),
            BuiltinToolsProfile::ReadOnly,
            None,
            60,
        );
        let tools = vec![
            ToolDef::new("fake__echo", "d", serde_json::json!({})),
            ToolDef::new("web__fetch", "d", serde_json::json!({})),
            ToolDef::new("compress_and_store", "d", serde_json::json!({})),
        ];
        // `${VAR}` is expanded by `Config::load` (the whole file, before
        // parsing): the loaded servers hold the secret itself.
        const SECRET: &str = "sk-dollar-brace-0123456789abcdef";
        let var = format!("TENGU_TEST_MCP_SECRET_{}", uuid::Uuid::new_v4().simple()).to_uppercase();
        std::env::set_var(&var, SECRET);
        let dir = tempfile::TempDir::new().unwrap();
        let file = dir.path().join("config.toml");
        std::fs::write(
            &file,
            format!(
                "[agents.main]\ndefault = true\nengine = \"openrouter\"\nmodel = \"m\"\n\n\
                 [[mcp_servers]]\nname = \"fake\"\ntransport = \"stdio\"\n\
                 command = [\"sh\", \"server.sh\", \"--key=${{{var}}}\"]\n\
                 env = {{ TOKEN = \"${{{var}}}\", HOME_REF = \"$HOME\" }}\n\n\
                 [[mcp_servers]]\nname = \"web\"\ntransport = \"http\"\n\
                 url = \"https://mcp.example/${{{var}}}\"\n\
                 auth = {{ type = \"bearer\", token = \"${{{var}}}\" }}\n\n\
                 [[mcp_servers]]\nname = \"unused\"\ntransport = \"stdio\"\ncommand = [\"true\"]\n"
            ),
        )
        .unwrap();
        let loaded = crate::config::Config::load(&file).unwrap();
        std::env::remove_var(&var);
        assert_eq!(
            loaded.mcp_servers[0].env["TOKEN"], SECRET,
            "expanded at load"
        );
        assert_eq!(loaded.mcp_servers[1].auth.as_ref().unwrap().token, SECRET);

        let cfg = engine.build_mcp_config_json(
            "tengu",
            std::path::Path::new("/tmp"),
            &tools,
            1000,
            &loaded.mcp_servers,
            None,
        );
        let text = cfg.to_string();
        assert!(
            !text.contains(SECRET),
            "a `${{VAR}}` value reached the file: {text}"
        );
        let env = &cfg["mcpServers"]["tengu-tools"]["env"];
        let passed: Vec<String> = serde_json::from_str(
            env[crate::adapters::outbound::bridge_env::TENGU_BRIDGE_MCP_SERVERS_ENV]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            passed,
            ["fake", "web"],
            "names of the requested servers only"
        );
        let env = env.as_object().unwrap();
        let unknown: Vec<&String> = env
            .keys()
            .filter(|k| !BRIDGE_ENV_KEYS.contains(&k.as_str()))
            .collect();
        assert!(unknown.is_empty(), "keys outside the contract: {unknown:?}");
        let home = std::env::var("HOME").unwrap();
        assert!(
            env.values().all(|v| v.as_str() != Some(home.as_str())),
            "a `$VAR` value was written into the file"
        );
        assert!(!env.contains_key("OPENROUTER_API_KEY"));
        assert!(
            !env.contains_key("TENGU_BRIDGE_GRANT_WORKSPACE")
                && !env.contains_key("TENGU_BRIDGE_SUMMARY_FILE"),
            "a plain engine is no run-agent step"
        );

        let none = engine.build_mcp_config_json(
            "tengu",
            std::path::Path::new("/tmp"),
            &[ToolDef::new("read_file", "d", serde_json::json!({}))],
            1000,
            &[server("fake", &[("TOKEN", "$HOME")])],
            None,
        );
        assert!(none["mcpServers"]["tengu-tools"]["env"]
            .get(crate::adapters::outbound::bridge_env::TENGU_BRIDGE_MCP_SERVERS_ENV)
            .is_none());
    }

    /// A `run-agent` step names its workspace grant and summary file
    /// explicitly in the bridge env.
    #[test]
    fn step_bridge_options_reach_the_bridge_env() {
        let engine =
            ClaudeCodeEngine::new(PathBuf::from("claude"), BuiltinToolsProfile::None, None, 60)
                .with_step_bridge(StepBridge {
                    grant_workspace: true,
                    summary_file: Some(PathBuf::from("/tmp/tengu-summary-x")),
                });
        let tools = [ToolDef::new("read_file", "d", serde_json::json!({}))];
        let cfg = engine.build_mcp_config_json(
            "tengu",
            std::path::Path::new("/tmp"),
            &tools,
            1000,
            &[],
            None,
        );
        let env = &cfg["mcpServers"]["tengu-tools"]["env"];
        assert_eq!(env["TENGU_BRIDGE_GRANT_WORKSPACE"], "1");
        assert_eq!(env["TENGU_BRIDGE_SUMMARY_FILE"], "/tmp/tengu-summary-x");
    }

    /// A parent Claude Code session's env is dropped; the operator's auth,
    /// provider and other env stay.
    #[test]
    fn parent_session_env_strips_the_session_keeps_operator_config() {
        let names = [
            "CLAUDECODE",
            "CLAUDE_PID",
            "CLAUDE_EFFORT",
            "CLAUDE_CODE_ENTRYPOINT",
            "CLAUDE_CODE_SSE_PORT",
            "CLAUDE_CODE_SESSION_ID",
            "CLAUDE_CODE_CHILD_SESSION",
            "CLAUDE_CODE_SESSION_ATTENDED",
            "CLAUDE_CODE_EXECPATH",
            "CLAUDE_CODE_MESSAGING_SOCKET",
            "CLAUDE_CODE_MESSAGING_TOKEN",
            "CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS",
            "CLAUDE_CODE_SOME_FUTURE_SESSION_VAR",
            // kept
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_SKIP_BEDROCK_AUTH",
            "CLAUDE_CODE_CLIENT_CERT",
            "CLAUDE_CONFIG_DIR",
            "PATH",
            "HOME",
            "HTTPS_PROXY",
            "TENGU_SECRETS_LOADED",
        ];
        let removed = parent_session_env(names);
        assert_eq!(removed, &names[..13]);
    }

    /// Whitespace around a profile is ignored; anything else is no profile
    /// (never a silent `editor_shell`).
    #[test]
    fn builtin_tools_profile_parses_trimmed_and_refuses_unknown() {
        assert_eq!(
            BuiltinToolsProfile::parse(" none"),
            Some(BuiltinToolsProfile::None)
        );
        assert_eq!(
            BuiltinToolsProfile::parse("read_only\n"),
            Some(BuiltinToolsProfile::ReadOnly)
        );
        assert_eq!(
            BuiltinToolsProfile::parse("editor_shell"),
            Some(BuiltinToolsProfile::EditorShell)
        );
        for bad in ["", "None", "shell", "editor-shell"] {
            assert_eq!(BuiltinToolsProfile::parse(bad), None, "{bad:?}");
        }
    }

    fn msg(role: Role, content: &str) -> Message {
        Message {
            role,
            content: content.to_string(),
            tool_call_id: None,
            tool_calls: None,
        }
    }

    /// The system prompt rides on `--system-prompt` only: a system message
    /// equal to it is not repeated in the stdin prompt; other system
    /// messages (a memory block) stay.
    #[test]
    fn format_prompt_sends_the_system_prompt_once() {
        let messages = [
            msg(Role::System, "SYS PROMPT"),
            msg(Role::User, "goal"),
            msg(Role::System, "memory block"),
        ];
        let once = ClaudeCodeEngine::format_prompt(&messages, Some("SYS PROMPT"));
        assert!(!once.contains("SYS PROMPT"), "{once}");
        assert_eq!(once, "User: goal\n\nmemory block");
        let args = strings(cli_args(
            BuiltinToolsProfile::None,
            None,
            Some("SYS PROMPT"),
            None,
        ));
        assert_eq!(value_after(&args, "--system-prompt"), "SYS PROMPT");
        // No context prompt: every system message is in the prompt.
        let all = ClaudeCodeEngine::format_prompt(&messages, None);
        assert!(all.starts_with("SYS PROMPT\n\nUser: goal"), "{all}");
    }

    /// The stream as the bridge's conversation: one assistant message per
    /// API response (text + `tool_use` blocks of consecutive lines, tengu
    /// names), a tool message per `tool_result`; lines that carry neither
    /// change nothing, and a streamed line never merges into the given
    /// history.
    #[test]
    fn transcript_follows_the_stream() {
        let given = vec![
            msg(Role::System, "sys"),
            msg(Role::User, "earlier"),
            msg(Role::Assistant, "earlier answer"),
            msg(Role::User, "goal"),
        ];
        let lines = [
            r#"{"type":"system","subtype":"init","session_id":"s"}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"listing"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_1","name":"mcp__tengu-tools__list_directory","input":{"path":"."}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","content":[{"type":"text","text":"a.txt"},{"type":"text","text":"b.txt"}]}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"hm"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_2","name":"Read","input":{}},{"type":"tool_use","id":"toolu_3","name":"mcp__tengu-tools__skill_distill","input":{"from_message_index":1}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_2","is_error":true,"content":"denied"}]}}"#,
            r#"{"type":"result","subtype":"success","result":"done"}"#,
            "not json",
        ];
        let mut messages = given.clone();
        let changed: Vec<bool> = lines
            .iter()
            .map(|l| absorb_ndjson(&mut messages, given.len(), l))
            .collect();
        assert_eq!(
            changed,
            [false, true, true, true, false, true, true, false, false]
        );
        assert_eq!(messages.len(), given.len() + 4);
        let first = &messages[4];
        assert!(matches!(first.role, Role::Assistant));
        assert_eq!(
            first.content, "listing",
            "a new message, not merged into history"
        );
        let calls = first.tool_calls.as_ref().unwrap();
        assert_eq!(
            (calls[0].id.as_str(), calls[0].name.as_str()),
            ("toolu_1", "list_directory")
        );
        assert_eq!(calls[0].arguments, serde_json::json!({"path": "."}));
        let result = &messages[5];
        assert!(matches!(result.role, Role::Tool));
        assert_eq!(
            (result.tool_call_id.as_deref(), result.content.as_str()),
            (Some("toolu_1"), "a.txt\nb.txt")
        );
        let names: Vec<&str> = messages[6]
            .tool_calls
            .iter()
            .flatten()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(names, ["Read", "skill_distill"]);
        assert_eq!(messages[7].content, "denied");
    }

    /// The transcript file: mode 0600, the given messages first, rewritten
    /// whole on every changing line, removed with the run.
    #[test]
    fn transcript_file_is_private_and_kept_current() {
        let given = [msg(Role::System, "sys"), msg(Role::User, "goal")];
        let mut transcript = Transcript::create(&given).unwrap();
        let path = transcript.path().to_path_buf();
        let read = |p: &Path| -> Vec<Message> {
            serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap()
        };
        assert_eq!(read(&path).len(), 2);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{mode:o}");
        }
        transcript.absorb(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_1","name":"mcp__tengu-tools__read_file","input":{"path":"a"}}]}}"#,
        );
        let now = read(&path);
        assert_eq!(now.len(), 3);
        assert_eq!(now[2].tool_calls.as_ref().unwrap()[0].name, "read_file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "rewritten private: {mode:o}");
        }
        drop(transcript);
        assert!(!path.exists(), "removed with the run");
    }

    /// A stand-in CLI: `script` after the prompt is read.
    fn stub_cli(dir: &Path, script: &str) -> PathBuf {
        let path = dir.join("claude");
        std::fs::write(&path, format!("#!/bin/sh\ncat > /dev/null\n{script}")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    /// Events of one run of the stub, and how long it took.
    async fn run_stub(script: &str) -> (Vec<StreamEvent>, Duration) {
        use futures::StreamExt;
        let dir = tempfile::TempDir::new().unwrap();
        let engine = ClaudeCodeEngine::new(
            stub_cli(dir.path(), script),
            BuiltinToolsProfile::None,
            None,
            60,
        );
        let context = EngineContext {
            workspace: Some(dir.path().to_path_buf()),
            system_prompt: None,
            bridge_tools: None,
            max_tool_rounds: Some(10),
            max_mcp_result_chars: None,
            mcp_servers: Vec::new(),
        };
        let started = std::time::Instant::now();
        let stream = engine
            .run(&[msg(Role::User, "go")], &[], &context)
            .await
            .unwrap();
        let events = tokio::time::timeout(Duration::from_secs(15), stream.collect::<Vec<_>>())
            .await
            .expect("the run ends");
        (events, started.elapsed())
    }

    fn ran(events: &[StreamEvent]) -> Vec<(String, bool)> {
        events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolRan { name, ok } => Some((name.clone(), *ok)),
                _ => None,
            })
            .collect()
    }

    /// A successful `compress_and_store` ends the run (the CLI is stopped,
    /// the run's summed usage, `Done`), once the round's other calls are
    /// answered — the stub would otherwise keep going for 30 s; a refused
    /// one does not.
    #[tokio::test]
    async fn compress_and_store_ends_the_run_after_its_round() {
        const TEXT: &str = r#"echo '{"type":"assistant","message":{"id":"msg_0","content":[{"type":"text","text":"working"}],"usage":{"input_tokens":7,"output_tokens":3}}}'"#;
        const CALLS: &str = r#"echo '{"type":"assistant","message":{"id":"msg_1","content":[{"type":"tool_use","id":"toolu_a","name":"mcp__tengu-tools__read_file","input":{"path":"a"}},{"type":"tool_use","id":"toolu_c","name":"mcp__tengu-tools__compress_and_store","input":{"summary":"done"}}],"usage":{"input_tokens":10,"output_tokens":5}}}'"#;
        const STORED: &str = r#"echo '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_c","content":"stored — stop now"}]}}'"#;
        const READ: &str = r#"echo '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_a","content":"alpha"}]}}'"#;
        const LATE: &str =
            r#"echo '{"type":"assistant","message":{"content":[{"type":"text","text":"late"}]}}'"#;

        // `exec`: the stopped stub leaves no child holding the pipes.
        let (events, took) = run_stub(&format!(
            "{TEXT}\n{CALLS}\n{STORED}\nsleep 0.3\n{READ}\nexec sleep 30\n"
        ))
        .await;
        assert!(took < Duration::from_secs(10), "not stopped: {took:?}");
        assert_eq!(
            ran(&events),
            [
                ("compress_and_store".to_string(), true),
                ("read_file".to_string(), true)
            ],
            "the round's other call is answered first"
        );
        assert!(
            matches!(
                &events[events.len() - 2..],
                [
                    StreamEvent::Usage {
                        input_tokens: 17,
                        output_tokens: 8
                    },
                    StreamEvent::Done
                ]
            ),
            "the run's summed usage, then Done: {events:?}"
        );

        // Refused (no step bridge): the run goes on to its own end.
        let refused = CALLS.replace("toolu_a", "toolu_x");
        let error = STORED.replace(
            r#""content":"stored — stop now""#,
            r#""is_error":true,"content":"refused""#,
        );
        let (events, _) = run_stub(&format!("{refused}\n{error}\n{LATE}\n")).await;
        assert_eq!(ran(&events), [("compress_and_store".to_string(), false)]);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, StreamEvent::TextDelta { text } if text == "late")),
            "{events:?}"
        );
    }
}
