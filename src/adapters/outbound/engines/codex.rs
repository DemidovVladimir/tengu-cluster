//! Codex engine — runs agents through the OpenAI Codex CLI on the operator's
//! ChatGPT subscription (`codex login`): `codex exec --json`, the prompt on
//! stdin, one JSONL event per line on stdout. Tengu tools reach the CLI
//! through `tengu mcp-bridge`, registered as the MCP server `tengu-tools`
//! with `-c mcp_servers.tengu-tools.*` overrides (the bridge env:
//! `bridge_env::bridge_env`, as for Claude Code). The CLI runs its own tool
//! loop; each finished call becomes `StreamEvent::ToolRan`.
//!
//! | `codex exec` arg (`cli_args`) | Why |
//! |---|---|
//! | `--json` | JSONL events: `item.started` / `item.completed` (`agent_message`, `reasoning`, `mcp_tool_call`, `command_execution`, `file_change`), `turn.completed` (usage), `turn.failed`, `error` |
//! | `--ignore-user-config` `--ignore-rules` | no `$CODEX_HOME/config.toml` (the operator's MCP servers, profiles, hooks, notify) and no execpolicy rules; the ChatGPT login (`$CODEX_HOME/auth.json`) still works |
//! | `--ephemeral` `--skip-git-repo-check` | no session files; runs in any workspace |
//! | `-s <sandbox>` (`[agents.<a>.codex] sandbox`, default `read-only`) `-c approval_policy="never"` | the CLI's built-in shell / patch tool: read-only or workspace-write, no network, never asks |
//! | `-c web_search="disabled"` · `-c features.<f>=false` ([`OFF_FEATURES`]) | Codex integrations that run outside tengu scopes and egress (apps, plugins, browser / computer use, image generation, sub-agents, memories, hooks); `features.*` overrides tolerate a name this CLI version lacks |
//! | `-m <model>` · `-C <workspace>` · `-c developer_instructions=…` | the agent's model; its workspace; the system prompt (once — not repeated in stdin, `cli_run::format_prompt`) |
//! | `-c mcp_servers.tengu-tools.{command,args,env.*,env_vars,default_tools_approval_mode="approve",required=true,startup_timeout_sec,tool_timeout_sec}` | the bridge; no secret value on the command line — Codex starts MCP servers with a minimal env, so `env_vars` forwards the NAMES of this run's env (vault secrets, `OPENROUTER_API_KEY`, `$VAR`s of `[[mcp_servers]]`); every bridged call is pre-approved (tengu scopes gate them in the bridge) |
//!
//! | Env of the CLI | Rule |
//! |---|---|
//! | removed | `OPENAI_API_KEY`, every `CODEX_*` but `CODEX_HOME` / `CODEX_CA_CERTIFICATE` (an API key would bill the account instead of the subscription; a parent Codex session's sandbox / thread vars) — [`stripped_env`] |
//! | added | `[egress]` `HTTPS_PROXY` / `HTTP_PROXY` / `NO_PROXY` when the LLM API is routed through the proxy (`EgressPolicy::claude_cli_env`) |
//!
//! | Run | Handling |
//! |---|---|
//! | `compress_and_store` succeeded (a `run-agent` step's bridge stored the summary) | the CLI is stopped once no other call is open, as the in-process loop stops after its round |
//! | `max_tool_rounds` | counts tool calls (MCP + built-in) of the run: past it the CLI is killed with an error |
//! | `turn.failed` · a non-zero exit with no answer | `StreamEvent::Error` with the CLI's message (last `error` event, else stderr's tail) |
//! | hardened sandbox | refused at load (`config/hardening.rs`): the built-in shell ignores tengu scopes |

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;

use anyhow::Result;
use async_trait::async_trait;
use futures::stream;
use serde_json::{Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::time::Duration;
use tracing::{debug, error, info, warn};

use super::cli_run::{format_prompt, run_workspace, Transcript};
use crate::adapters::outbound::bridge_env::{bridge_env, BridgeRun, StepBridge};
use crate::adapters::outbound::tools::skill_lifecycle::compress_and_store;
use crate::domain::message::{Message, ModelInfo, Role, StreamEvent, ToolCall, ToolDef};
use crate::domain::scope::ToolScope;
use crate::ports::engine::{Engine, EngineContext, EngineDiagnostics};

/// The MCP server name the bridge is registered under.
const SERVER: &str = "tengu-tools";
/// Codex features off for every run (module table): integrations that act
/// outside tengu scopes and egress. A name the installed CLI does not know
/// is ignored by `-c features.<f>=false` (unlike `--disable`).
pub(crate) const OFF_FEATURES: &[&str] = &[
    "apps",
    "plugins",
    "remote_plugin",
    "browser_use",
    "browser_use_external",
    "browser_use_full_cdp_access",
    "in_app_browser",
    "computer_use",
    "image_generation",
    "multi_agent",
    "goals",
    "memories",
    "hooks",
    "tool_suggest",
    "skill_mcp_dependency_install",
    "workspace_dependencies",
];
/// How long the bridge may take to start (it loads the config, may connect
/// `[[mcp_servers]]`).
const BRIDGE_STARTUP_SECS: u64 = 30;
/// One bridged call's ceiling (Codex's default is shorter than a backtest).
const TOOL_TIMEOUT_SECS: u64 = 600;
/// Bytes of stderr kept for an error message.
const STDERR_TAIL: usize = 4096;

/// `[agents.<a>.codex] sandbox` → `codex exec -s`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodexSandbox {
    ReadOnly,
    WorkspaceWrite,
}

impl CodexSandbox {
    /// Trimmed; `None` for any other value (config validation refuses it).
    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "read-only" => Some(Self::ReadOnly),
            "workspace-write" => Some(Self::WorkspaceWrite),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
        }
    }
}

pub(crate) struct CodexEngine {
    cli_path: PathBuf,
    sandbox: CodexSandbox,
    model: String,
    /// Idle timeout: no output for this long kills the CLI.
    timeout_secs: u64,
    /// The agent's scopes for the bridge's fallback (`TENGU_BRIDGE_SCOPES`).
    scopes: HashMap<String, ToolScope>,
    /// `[agents.<id>]` + its absolute config file for the bridge.
    bridge_agent: Option<(String, PathBuf)>,
    step: StepBridge,
}

impl CodexEngine {
    pub(crate) fn new(
        cli_path: PathBuf,
        sandbox: CodexSandbox,
        model: String,
        timeout_secs: u64,
    ) -> Self {
        Self {
            cli_path,
            sandbox,
            model,
            timeout_secs,
            scopes: HashMap::new(),
            bridge_agent: None,
            step: StepBridge::default(),
        }
    }

    pub(crate) fn with_scopes(mut self, scopes: HashMap<String, ToolScope>) -> Self {
        self.scopes = scopes;
        self
    }

    /// The bridge loads `[agents.<agent_id>]` of `config` (made absolute:
    /// the bridge runs in the workspace).
    pub(crate) fn with_bridge_agent(mut self, agent_id: &str, config: &Path) -> Self {
        self.bridge_agent = Some((
            agent_id.to_string(),
            crate::config::paths::absolute_path(config),
        ));
        self
    }

    pub(crate) fn with_step_bridge(mut self, step: StepBridge) -> Self {
        self.step = step;
        self
    }
}

/// The bridge as `-c` overrides ([`cli_args`]).
pub(crate) struct McpBridge<'a> {
    pub tengu_bin: &'a str,
    pub env: &'a Map<String, Value>,
    /// Env names the CLI forwards to the bridge (`env_vars`).
    pub forward: &'a [String],
}

/// A TOML value for `-c key=<value>` (the CLI parses the value as TOML).
fn toml_str(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

/// `codex exec` arguments for one run — all but the env (module table).
/// Pure, so tests pin them. The prompt is read from stdin (`-`, last).
pub(crate) fn cli_args(
    model: &str,
    sandbox: CodexSandbox,
    workspace: Option<&Path>,
    system_prompt: Option<&str>,
    bridge: Option<&McpBridge<'_>>,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "exec",
        "--json",
        "--ignore-user-config",
        "--ignore-rules",
        "--ephemeral",
        "--skip-git-repo-check",
        "-s",
        sandbox.as_str(),
        "-c",
        "approval_policy=\"never\"",
        "-c",
        "web_search=\"disabled\"",
    ]
    .into_iter()
    .map(Into::into)
    .collect();
    for f in OFF_FEATURES {
        args.extend(["-c".into(), format!("features.{f}=false").into()]);
    }
    if !model.trim().is_empty() {
        args.extend(["-m".into(), model.trim().into()]);
    }
    if let Some(ws) = workspace {
        args.extend(["-C".into(), ws.as_os_str().to_owned()]);
    }
    if let Some(sp) = system_prompt.filter(|s| !s.trim().is_empty()) {
        args.extend([
            "-c".into(),
            format!("developer_instructions={}", toml_str(sp)).into(),
        ]);
    }
    if let Some(b) = bridge {
        let key = |k: &str| format!("mcp_servers.{SERVER}.{k}");
        let mut set = |k: String, v: String| args.extend(["-c".into(), format!("{k}={v}").into()]);
        set(key("command"), toml_str(b.tengu_bin));
        set(key("args"), "[\"mcp-bridge\"]".into());
        for (name, value) in b.env {
            let v = value.as_str().map(str::to_string).unwrap_or_default();
            set(key(&format!("env.{name}")), toml_str(&v));
        }
        let forward = toml::Value::Array(
            b.forward
                .iter()
                .map(|n| toml::Value::String(n.clone()))
                .collect(),
        );
        set(key("env_vars"), forward.to_string());
        set(key("default_tools_approval_mode"), toml_str("approve"));
        set(key("required"), "true".into());
        set(key("startup_timeout_sec"), BRIDGE_STARTUP_SECS.to_string());
        set(key("tool_timeout_sec"), TOOL_TIMEOUT_SECS.to_string());
    }
    args.push("-".into());
    args
}

/// Env names removed from the CLI's env (module table).
pub(crate) fn stripped_env<'a>(names: impl IntoIterator<Item = &'a str>) -> Vec<&'a str> {
    names
        .into_iter()
        .filter(|n| {
            *n == "OPENAI_API_KEY"
                || (n.starts_with("CODEX_") && !matches!(*n, "CODEX_HOME" | "CODEX_CA_CERTIFICATE"))
        })
        .collect()
}

/// The tengu name of a tool the CLI called: bridged tools as they are,
/// another MCP server's as `{server}__{tool}`, built-ins by kind.
fn tool_name(item: &Value) -> String {
    match str_field(item, "type") {
        "mcp_tool_call" => {
            let (server, tool) = (str_field(item, "server"), str_field(item, "tool"));
            if server == SERVER || server.is_empty() {
                tool.to_string()
            } else {
                format!("{server}__{tool}")
            }
        }
        "command_execution" => "shell".to_string(),
        "file_change" => "apply_patch".to_string(),
        "web_search" => "web_search".to_string(),
        other => other.to_string(),
    }
}

/// Item kinds that are tool calls (counted against `max_tool_rounds`).
fn is_tool_item(item: &Value) -> bool {
    matches!(
        str_field(item, "type"),
        "mcp_tool_call" | "command_execution" | "file_change" | "web_search"
    )
}

/// Whether a finished tool item succeeded.
fn tool_ok(item: &Value) -> bool {
    let status = str_field(item, "status");
    let error = item.get("error").is_some_and(|e| !e.is_null());
    match str_field(item, "type") {
        "command_execution" => {
            status == "completed" && item.get("exit_code").and_then(Value::as_i64).unwrap_or(0) == 0
        }
        "web_search" => !error && status != "failed",
        _ => status == "completed" && !error,
    }
}

/// The text of an MCP result (`content[].text`), else its error message.
fn tool_result_text(item: &Value) -> String {
    if let Some(msg) = item
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
    {
        return msg.to_string();
    }
    item.get("result")
        .and_then(|r| r.get("content"))
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn str_field<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// What the stream has seen so far ([`process_line`]).
#[derive(Default)]
pub(crate) struct RunState {
    pub emitted_text: bool,
    pub tool_calls: u32,
    /// Open tool items: id → tengu name.
    pub open: HashMap<String, String>,
    /// The last top-level `error` event's message (a failed run's reason).
    pub last_error: Option<String>,
    /// A `turn.failed` was turned into `StreamEvent::Error`.
    pub failed: bool,
}

/// One JSONL line → the events to emit (module table).
pub(crate) fn process_line(line: &str, st: &mut RunState) -> Vec<StreamEvent> {
    let json: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            debug!(error = %e, "Codex: not a JSON line (skipped)");
            return vec![];
        }
    };
    let mut events = Vec::new();
    match str_field(&json, "type") {
        "thread.started" => {
            debug!(
                thread_id = str_field(&json, "thread_id"),
                "Codex thread started"
            );
        }
        "item.started" => {
            let item = json.get("item").cloned().unwrap_or(Value::Null);
            if is_tool_item(&item) {
                st.tool_calls += 1;
                let name = tool_name(&item);
                let args = item.get("arguments").cloned().unwrap_or(Value::Null);
                debug!(tool = %name, args = %args, tool_call_count = st.tool_calls, "Codex tool call");
                st.open.insert(str_field(&item, "id").to_string(), name);
            }
        }
        "item.completed" => {
            let item = json.get("item").cloned().unwrap_or(Value::Null);
            match str_field(&item, "type") {
                "agent_message" => {
                    let text = str_field(&item, "text");
                    if !text.is_empty() {
                        let text = if st.emitted_text {
                            format!("\n\n{text}")
                        } else {
                            text.to_string()
                        };
                        st.emitted_text = true;
                        events.push(StreamEvent::TextDelta { text });
                    }
                }
                "reasoning" => {
                    let text = str_field(&item, "text");
                    if !text.is_empty() {
                        events.push(StreamEvent::ThinkingDelta {
                            text: text.to_string(),
                        });
                    }
                }
                "error" => warn!(message = str_field(&item, "message"), "Codex item error"),
                _ if is_tool_item(&item) => {
                    let id = str_field(&item, "id");
                    // A call reported only when done counts here.
                    let name = match st.open.remove(id) {
                        Some(name) => name,
                        None => {
                            st.tool_calls += 1;
                            tool_name(&item)
                        }
                    };
                    let ok = tool_ok(&item);
                    if ok {
                        debug!(tool = %name, result = %tool_result_text(&item), "Codex tool result");
                    } else {
                        warn!(tool = %name, result = %tool_result_text(&item), "Codex tool ERROR");
                    }
                    events.push(StreamEvent::ToolRan { name, ok });
                }
                _ => {}
            }
        }
        "turn.completed" => {
            if let Some(usage) = json.get("usage") {
                let tokens = |k: &str| usage.get(k).and_then(Value::as_u64).unwrap_or(0) as u32;
                let (input, output) = (tokens("input_tokens"), tokens("output_tokens"));
                info!(
                    input_tokens = input,
                    cached_input_tokens = tokens("cached_input_tokens"),
                    output_tokens = output,
                    reasoning_output_tokens = tokens("reasoning_output_tokens"),
                    tool_calls = st.tool_calls,
                    "Codex turn completed"
                );
                if input > 0 || output > 0 {
                    events.push(StreamEvent::Usage {
                        input_tokens: input,
                        output_tokens: output,
                    });
                }
            }
        }
        "turn.failed" => {
            let message = json
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .or(st.last_error.as_deref())
                .unwrap_or("no reason given")
                .to_string();
            st.failed = true;
            events.push(StreamEvent::Error {
                message: format!("Codex turn failed: {message}"),
            });
        }
        // Also transient notices (a reconnect): logged, kept as the reason
        // if the run then fails.
        "error" => {
            let message = str_field(&json, "message").to_string();
            warn!(message = %message, "Codex error event");
            st.last_error = Some(message);
        }
        _ => {}
    }
    events
}

/// One JSONL line into the bridge's conversation (`cli_run::Transcript`):
/// an answer → an assistant message, a bridged call → a tool call on the
/// streamed assistant message (consecutive calls share one), its result →
/// a tool message. `true` when it changed `messages`.
pub(crate) fn absorb_event(messages: &mut Vec<Message>, streamed_from: usize, line: &str) -> bool {
    let Ok(json) = serde_json::from_str::<Value>(line) else {
        return false;
    };
    let Some(item) = json.get("item") else {
        return false;
    };
    let kind = str_field(&json, "type");
    match (kind, str_field(item, "type")) {
        ("item.completed", "agent_message") => {
            let text = str_field(item, "text");
            if text.is_empty() {
                return false;
            }
            messages.push(Message {
                role: Role::Assistant,
                content: text.to_string(),
                tool_call_id: None,
                tool_calls: None,
            });
            true
        }
        ("item.started", "mcp_tool_call") => {
            let call = ToolCall {
                id: str_field(item, "id").to_string(),
                name: tool_name(item),
                arguments: item
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({})),
            };
            let merge = messages.len() > streamed_from
                && messages
                    .last()
                    .is_some_and(|m| matches!(m.role, Role::Assistant) && m.tool_calls.is_some());
            match messages.last_mut().filter(|_| merge) {
                Some(last) => last.tool_calls.get_or_insert_with(Vec::new).push(call),
                None => messages.push(Message {
                    role: Role::Assistant,
                    content: String::new(),
                    tool_call_id: None,
                    tool_calls: Some(vec![call]),
                }),
            }
            true
        }
        ("item.completed", "mcp_tool_call") => {
            messages.push(Message {
                role: Role::Tool,
                content: tool_result_text(item),
                tool_call_id: Some(str_field(item, "id").to_string()),
                tool_calls: None,
            });
            true
        }
        _ => false,
    }
}

/// The last `max` bytes of `buf`, on a char boundary, lossy UTF-8.
fn tail(buf: &[u8], max: usize) -> String {
    let start = buf.len().saturating_sub(max);
    String::from_utf8_lossy(&buf[start..]).trim().to_string()
}

#[async_trait]
impl Engine for CodexEngine {
    fn id(&self) -> &str {
        "codex"
    }

    /// Conservative: Codex compacts its own context; the harness only
    /// budgets the history it hands over.
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
            configured_model: Some(self.model.clone()),
            endpoint: Some(self.cli_path.to_string_lossy().to_string()),
            transport: Some(format!(
                "subprocess/codex-exec-json (-s {})",
                self.sandbox.as_str()
            )),
            capabilities: self.capabilities(),
        }
    }

    fn available_models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: self.model.clone(),
            provider: "codex".to_string(),
            display_name: "OpenAI Codex (CLI, ChatGPT subscription)".to_string(),
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
        let system_prompt = context.system_prompt.as_deref();
        let prompt = format_prompt(messages, system_prompt);
        if prompt.trim().is_empty() {
            return Ok(Box::pin(stream::iter(vec![StreamEvent::Error {
                message: "Empty prompt for Codex engine".to_string(),
            }])));
        }

        let bridge_tools: &[ToolDef] = context.bridge_tools.as_deref().unwrap_or_default();
        let has_bridge = !bridge_tools.is_empty();
        let (workspace, temp_workspace) = run_workspace(
            context.workspace.as_deref(),
            has_bridge,
            "tengu-codex-",
        )
        .unwrap_or_else(|e| {
            warn!(error = %e, "Codex: no temp workspace — Tengu tools unavailable this run");
            (None, None)
        });

        let mut cmd = tokio::process::Command::new(&self.cli_path);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let inherited: Vec<String> = std::env::vars_os()
            .filter_map(|(k, _)| k.into_string().ok())
            .collect();
        let removed = stripped_env(inherited.iter().map(String::as_str));
        for name in &removed {
            cmd.env_remove(name);
        }
        let proxy_env = crate::adapters::outbound::egress::policy().claude_cli_env();
        for (k, v) in &proxy_env {
            cmd.env(k, v);
        }
        if let Some(ws) = &workspace {
            cmd.current_dir(ws);
        }

        // The bridge (only with tools and a workspace), its transcript file.
        let mut transcript: Option<Transcript> = None;
        let mut env = Map::new();
        let mut forward: Vec<String> = Vec::new();
        let tengu_bin = std::env::current_exe()
            .unwrap_or_else(|_| PathBuf::from("tengu"))
            .to_string_lossy()
            .to_string();
        if let (true, Some(ws)) = (has_bridge, &workspace) {
            transcript = Transcript::create(messages)
                .map_err(|e| warn!(error = %e, "Codex: no transcript file for the bridge"))
                .ok();
            env = bridge_env(&BridgeRun {
                workspace: ws,
                tools: bridge_tools,
                max_result_chars: context.max_mcp_result_chars.unwrap_or(50_000),
                scopes: &self.scopes,
                agent: self
                    .bridge_agent
                    .as_ref()
                    .map(|(a, c)| (a.as_str(), c.as_path())),
                step: &self.step,
                transcript: transcript.as_ref().map(Transcript::path),
                mcp_servers: &context.mcp_servers,
            });
            forward = inherited
                .iter()
                .map(String::as_str)
                .chain(proxy_env.iter().map(|(k, _)| *k))
                .filter(|n| !removed.contains(n) && !env.contains_key(*n))
                .map(str::to_string)
                .collect();
            forward.sort();
            forward.dedup();
        }
        let bridge = (has_bridge && workspace.is_some()).then(|| McpBridge {
            tengu_bin: &tengu_bin,
            env: &env,
            forward: &forward,
        });
        cmd.args(cli_args(
            &self.model,
            self.sandbox,
            workspace.as_deref(),
            system_prompt,
            bridge.as_ref(),
        ));

        debug!(
            sandbox = self.sandbox.as_str(),
            model = %self.model,
            workspace = ?workspace,
            bridge_tools = bridge_tools.len(),
            prompt_len = prompt.len(),
            timeout_secs = self.timeout_secs,
            "Spawning Codex CLI subprocess (exec --json)"
        );

        let mut child = cmd.spawn().map_err(|e| {
            anyhow::anyhow!(
                "Failed to spawn codex CLI at {:?}: {e} — install it (npm i -g @openai/codex) and run `codex login`, or set [agents.<a>.codex] cli_path",
                self.cli_path
            )
        })?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(prompt.as_bytes()).await?;
            stdin.shutdown().await?;
        }
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("Failed to capture stdout from the Codex CLI"))?;
        // Drained on its own task: a full stderr pipe would stall the CLI.
        let stderr_task = child.stderr.take().map(|mut err| {
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let _ = err.read_to_end(&mut buf).await;
                buf
            })
        });

        let timeout_secs = self.timeout_secs;
        let max_tool_rounds = context.max_tool_rounds.unwrap_or(70);
        let (tx, rx) = tokio::sync::mpsc::channel::<StreamEvent>(64);

        tokio::spawn(async move {
            // Kept alive until the CLI exits.
            let _temp_workspace = temp_workspace;
            let mut transcript = transcript;
            let mut lines = tokio::io::BufReader::new(stdout).lines();
            let mut st = RunState::default();
            let idle = Duration::from_secs(timeout_secs);
            let (mut timed_out, mut tool_limit_hit, mut step_done) = (false, false, false);
            let mut summary_stored = false;
            loop {
                match tokio::time::timeout(idle, lines.next_line()).await {
                    Ok(Ok(Some(line))) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        let events = process_line(&line, &mut st);
                        if let Some(t) = transcript.as_mut() {
                            t.update(|m, from| absorb_event(m, from, &line));
                        }
                        summary_stored |= events.iter().any(|e| {
                            matches!(e, StreamEvent::ToolRan { name, ok: true } if name == compress_and_store::NAME)
                        });
                        for event in events {
                            if tx.send(event).await.is_err() {
                                break;
                            }
                        }
                        if summary_stored && st.open.is_empty() {
                            step_done = true;
                            break;
                        }
                        if st.tool_calls > max_tool_rounds {
                            tool_limit_hit = true;
                            break;
                        }
                    }
                    Ok(Ok(None)) => break,
                    Ok(Err(e)) => {
                        warn!(error = %e, "Error reading Codex CLI stdout");
                        break;
                    }
                    Err(_) => {
                        timed_out = true;
                        break;
                    }
                }
            }

            if step_done || tool_limit_hit || timed_out {
                let _ = child.kill().await;
            }
            let status = child.wait().await;
            let stderr = match stderr_task {
                Some(task) => task.await.unwrap_or_default(),
                None => Vec::new(),
            };
            let stderr = tail(&stderr, STDERR_TAIL);
            if !stderr.is_empty() {
                debug!(stderr = %stderr, "Codex CLI stderr");
            }

            let last = if step_done {
                info!(
                    tool_calls = st.tool_calls,
                    "Codex: compress_and_store stored the step summary — ending the CLI run"
                );
                Some(StreamEvent::Done)
            } else if tool_limit_hit {
                error!(
                    tool_calls = st.tool_calls,
                    max_tool_rounds, "Codex max tool rounds exceeded — killing subprocess"
                );
                Some(StreamEvent::Error {
                    message: format!(
                        "Max tool rounds exceeded ({} calls, limit {}). The agent may be stuck in a retry loop.",
                        st.tool_calls, max_tool_rounds
                    ),
                })
            } else if timed_out {
                error!(timeout_secs, "Codex CLI idle timeout — killing subprocess");
                Some(StreamEvent::Error {
                    message: format!("Codex CLI idle timeout — no output for {timeout_secs}s"),
                })
            } else if st.failed {
                None // the `turn.failed` error is already out
            } else {
                match status {
                    Ok(s) if !s.success() && !st.emitted_text => {
                        let why = st
                            .last_error
                            .clone()
                            .filter(|m| !m.is_empty())
                            .unwrap_or_else(|| stderr.clone());
                        error!(exit_code = s.code(), reason = %why, "Codex CLI failed");
                        Some(StreamEvent::Error {
                            message: format!("Codex CLI exited with {s}: {why}"),
                        })
                    }
                    Ok(s) => {
                        if !s.success() {
                            warn!(
                                exit_code = s.code(),
                                "Codex CLI exited with non-zero status after answering"
                            );
                        }
                        Some(StreamEvent::Done)
                    }
                    Err(e) => {
                        warn!(error = %e, "Failed to wait for the Codex CLI");
                        Some(StreamEvent::Done)
                    }
                }
            };
            if let Some(event) = last {
                let _ = tx.send(event).await;
            }
        });

        Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: Vec<OsString>) -> Vec<String> {
        args.into_iter()
            .map(|a| a.into_string().expect("utf-8 arg"))
            .collect()
    }

    fn value_after<'a>(args: &'a [String], flag: &str) -> &'a str {
        let i = args.iter().position(|a| a == flag).expect(flag);
        &args[i + 1]
    }

    /// The `-c` overrides as (key, parsed TOML value).
    fn overrides(args: &[String]) -> Vec<(String, toml::Value)> {
        args.windows(2)
            .filter(|w| w[0] == "-c")
            .map(|w| {
                let (k, v) = w[1].split_once('=').expect("key=value");
                let parsed: toml::Table =
                    toml::from_str(&format!("v = {v}")).unwrap_or_else(|e| panic!("{k}: {e}"));
                (k.to_string(), parsed["v"].clone())
            })
            .collect()
    }

    fn get<'a>(o: &'a [(String, toml::Value)], key: &str) -> Option<&'a toml::Value> {
        o.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// Every run: isolated from the operator's Codex config, never asks,
    /// no web search or desktop integrations, the prompt from stdin last.
    #[test]
    fn cli_args_isolate_the_run() {
        let args = strings(cli_args(
            "gpt-5.5",
            CodexSandbox::ReadOnly,
            Some(Path::new("/srv/ws")),
            None,
            None,
        ));
        for flag in [
            "exec",
            "--json",
            "--ignore-user-config",
            "--ignore-rules",
            "--ephemeral",
            "--skip-git-repo-check",
        ] {
            assert!(args.iter().any(|a| a == flag), "{flag}: {args:?}");
        }
        assert_eq!(value_after(&args, "-s"), "read-only");
        assert_eq!(value_after(&args, "-m"), "gpt-5.5");
        assert_eq!(value_after(&args, "-C"), "/srv/ws");
        assert_eq!(args.last().map(String::as_str), Some("-"));
        let o = overrides(&args);
        assert_eq!(get(&o, "approval_policy").unwrap().as_str(), Some("never"));
        assert_eq!(get(&o, "web_search").unwrap().as_str(), Some("disabled"));
        for f in ["computer_use", "apps", "plugins", "hooks", "multi_agent"] {
            assert_eq!(
                get(&o, &format!("features.{f}")).and_then(toml::Value::as_bool),
                Some(false),
                "{f}"
            );
        }
        assert!(
            !o.iter().any(|(k, _)| k.starts_with("mcp_servers")),
            "no bridge without tools"
        );
        let write = strings(cli_args(
            "m",
            CodexSandbox::WorkspaceWrite,
            None,
            None,
            None,
        ));
        assert_eq!(value_after(&write, "-s"), "workspace-write");
        assert!(!write.iter().any(|a| a == "-C"));
    }

    /// The system prompt rides once, as developer instructions, escaped as
    /// a TOML string whatever it holds.
    #[test]
    fn system_prompt_is_developer_instructions() {
        let sp = "You are \"tengu\".\nUse '.' for the root \\ and \u{1F600}.";
        let args = strings(cli_args("m", CodexSandbox::ReadOnly, None, Some(sp), None));
        let o = overrides(&args);
        assert_eq!(
            get(&o, "developer_instructions").unwrap().as_str(),
            Some(sp)
        );
        let none = strings(cli_args(
            "m",
            CodexSandbox::ReadOnly,
            None,
            Some("  "),
            None,
        ));
        assert!(!none.iter().any(|a| a.starts_with("developer_instructions")));
    }

    /// The bridge: `tengu mcp-bridge` with the env contract (each value a
    /// TOML string, JSON inside intact), forwarded env names, every bridged
    /// call pre-approved, required to start.
    #[test]
    fn bridge_overrides_register_tengu_tools() {
        let tools = vec![ToolDef {
            name: "list_directory".into(),
            description: "List files. Use '.' for the root.".into(),
            parameters: serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        }];
        let step = StepBridge {
            grant_workspace: true,
            summary_file: Some(PathBuf::from("/tmp/summary")),
        };
        let scopes = HashMap::new();
        let env = bridge_env(&BridgeRun {
            workspace: Path::new("/srv/ws"),
            tools: &tools,
            max_result_chars: 1000,
            scopes: &scopes,
            agent: Some(("main", Path::new("/srv/sandboxes/x/config.toml"))),
            step: &step,
            transcript: Some(Path::new("/tmp/transcript")),
            mcp_servers: &[],
        });
        let forward = vec!["HOME".to_string(), "OPENROUTER_API_KEY".to_string()];
        let bridge = McpBridge {
            tengu_bin: "/opt/tengu",
            env: &env,
            forward: &forward,
        };
        let args = strings(cli_args(
            "m",
            CodexSandbox::ReadOnly,
            None,
            None,
            Some(&bridge),
        ));
        let o = overrides(&args);
        let key = |k: &str| format!("mcp_servers.tengu-tools.{k}");
        assert_eq!(
            get(&o, &key("command")).unwrap().as_str(),
            Some("/opt/tengu")
        );
        assert_eq!(
            get(&o, &key("args")).unwrap().as_array().unwrap()[0].as_str(),
            Some("mcp-bridge")
        );
        let tools_json = get(&o, &key("env.TENGU_BRIDGE_TOOLS"))
            .unwrap()
            .as_str()
            .unwrap();
        let back: Vec<ToolDef> = serde_json::from_str(tools_json).unwrap();
        assert_eq!(back[0].description, "List files. Use '.' for the root.");
        for (k, v) in [
            ("TENGU_BRIDGE_AGENT", "main"),
            ("TENGU_CONFIG", "/srv/sandboxes/x/config.toml"),
            ("TENGU_BRIDGE_GRANT_WORKSPACE", "1"),
            ("TENGU_BRIDGE_SUMMARY_FILE", "/tmp/summary"),
            ("TENGU_BRIDGE_TRANSCRIPT_FILE", "/tmp/transcript"),
            ("TENGU_BRIDGE_WORKSPACE", "/srv/ws"),
        ] {
            assert_eq!(
                get(&o, &key(&format!("env.{k}"))).and_then(toml::Value::as_str),
                Some(v),
                "{k}"
            );
        }
        let names: Vec<&str> = get(&o, &key("env_vars"))
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .filter_map(toml::Value::as_str)
            .collect();
        assert_eq!(names, ["HOME", "OPENROUTER_API_KEY"], "names only");
        assert!(
            !args.iter().any(|a| a.contains("sk-")),
            "no value is forwarded"
        );
        assert_eq!(
            get(&o, &key("default_tools_approval_mode"))
                .unwrap()
                .as_str(),
            Some("approve")
        );
        assert_eq!(get(&o, &key("required")).unwrap().as_bool(), Some(true));
        assert_eq!(
            get(&o, &key("tool_timeout_sec")).unwrap().as_integer(),
            Some(TOOL_TIMEOUT_SECS as i64)
        );
    }

    /// An API key would bill the account; a parent Codex session's vars
    /// would confuse the nested CLI. The login (`CODEX_HOME`) stays.
    #[test]
    fn stripped_env_keeps_the_login() {
        let names = [
            "OPENAI_API_KEY",
            "CODEX_API_KEY",
            "CODEX_SANDBOX",
            "CODEX_THREAD_ID",
            "CODEX_HOME",
            "CODEX_CA_CERTIFICATE",
            "HOME",
            "OPENROUTER_API_KEY",
        ];
        assert_eq!(
            stripped_env(names),
            [
                "OPENAI_API_KEY",
                "CODEX_API_KEY",
                "CODEX_SANDBOX",
                "CODEX_THREAD_ID"
            ]
        );
        assert_eq!(
            CodexSandbox::parse(" workspace-write"),
            Some(CodexSandbox::WorkspaceWrite)
        );
        assert_eq!(CodexSandbox::parse("danger-full-access"), None);
    }

    /// Lines captured from `codex exec --json` (codex-cli 0.153.4, a bridged
    /// `list_directory` call), plus a failed call and a failed turn.
    const STARTED: &str = r#"{"type":"item.started","item":{"id":"item_0","type":"mcp_tool_call","server":"tengu-tools","tool":"list_directory","arguments":{"path":"."},"result":null,"error":null,"status":"in_progress"}}"#;
    const DONE: &str = r#"{"type":"item.completed","item":{"id":"item_0","type":"mcp_tool_call","server":"tengu-tools","tool":"list_directory","arguments":{"path":"."},"result":{"content":[{"type":"text","text":"marker.txt"}],"structured_content":null},"error":null,"status":"completed"}}"#;
    const ANSWER: &str = r#"{"type":"item.completed","item":{"id":"item_1","type":"agent_message","text":"marker.txt"}}"#;
    const USAGE: &str = r#"{"type":"turn.completed","usage":{"input_tokens":44249,"cached_input_tokens":30848,"cache_write_input_tokens":0,"output_tokens":84,"reasoning_output_tokens":29}}"#;
    const DENIED: &str = r#"{"type":"item.completed","item":{"id":"item_2","type":"mcp_tool_call","server":"tengu-tools","tool":"write_file","arguments":{},"result":null,"error":{"message":"MCP tool call requires approval, but approval policy is never"},"status":"failed"}}"#;

    #[test]
    fn events_become_stream_events() {
        let mut st = RunState::default();
        let lines = [
            r#"{"type":"thread.started","thread_id":"01a12586-31f5-72d0-8137-8560b5be8b5c"}"#,
            r#"{"type":"turn.started"}"#,
            STARTED,
            DONE,
            DENIED,
            ANSWER,
            r#"{"type":"item.completed","item":{"id":"item_3","type":"reasoning","text":"hm"}}"#,
            r#"{"type":"item.completed","item":{"id":"item_4","type":"agent_message","text":"second"}}"#,
            USAGE,
            "not json",
        ];
        let events: Vec<StreamEvent> = lines
            .iter()
            .flat_map(|l| process_line(l, &mut st))
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
                ("write_file".to_string(), false)
            ]
        );
        assert_eq!(st.tool_calls, 2, "started + completed-only");
        assert!(st.open.is_empty());
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "marker.txt\n\nsecond");
        assert!(events
            .iter()
            .any(|e| matches!(e, StreamEvent::ThinkingDelta { text } if text == "hm")));
        assert!(events.iter().any(|e| matches!(
            e,
            StreamEvent::Usage {
                input_tokens: 44249,
                output_tokens: 84
            }
        )));
        let mut failed = RunState::default();
        process_line(
            r#"{"type":"error","message":"stream disconnected"}"#,
            &mut failed,
        );
        let ev = process_line(r#"{"type":"turn.failed","error":{}}"#, &mut failed);
        assert!(
            matches!(&ev[..], [StreamEvent::Error { message }] if message.contains("stream disconnected")),
            "{ev:?}"
        );
        assert!(failed.failed);
        // Another MCP server's tool keeps its server.
        let other = r#"{"type":"item.completed","item":{"id":"x","type":"mcp_tool_call","server":"matrix","tool":"token","status":"completed","error":null}}"#;
        let ev = process_line(other, &mut RunState::default());
        assert!(
            matches!(&ev[..], [StreamEvent::ToolRan { name, ok: true }] if name == "matrix__token")
        );
    }

    #[test]
    fn transcript_follows_the_events() {
        let given = vec![Message {
            role: Role::User,
            content: "goal".into(),
            tool_call_id: None,
            tool_calls: None,
        }];
        let mut messages = given.clone();
        let changed: Vec<bool> = [STARTED, DONE, ANSWER, USAGE]
            .iter()
            .map(|l| absorb_event(&mut messages, given.len(), l))
            .collect();
        assert_eq!(changed, [true, true, true, false]);
        let calls = messages[1].tool_calls.as_ref().unwrap();
        assert_eq!(
            (calls[0].id.as_str(), calls[0].name.as_str()),
            ("item_0", "list_directory")
        );
        assert_eq!(calls[0].arguments, serde_json::json!({"path": "."}));
        assert!(matches!(messages[2].role, Role::Tool));
        assert_eq!(messages[2].content, "marker.txt");
        assert_eq!(messages[3].content, "marker.txt");
    }

    /// A stand-in CLI: `script` after the prompt is read.
    fn stub_cli(dir: &Path, script: &str) -> PathBuf {
        let path = dir.join("codex");
        std::fs::write(&path, format!("#!/bin/sh\ncat > /dev/null\n{script}")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    async fn run_stub(script: &str) -> (Vec<StreamEvent>, Duration) {
        use futures::StreamExt;
        let dir = tempfile::TempDir::new().unwrap();
        let engine = CodexEngine::new(
            stub_cli(dir.path(), script),
            CodexSandbox::ReadOnly,
            "gpt-5.5".into(),
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
        let user = Message {
            role: Role::User,
            content: "go".into(),
            tool_call_id: None,
            tool_calls: None,
        };
        let stream = engine.run(&[user], &[], &context).await.unwrap();
        let events = tokio::time::timeout(Duration::from_secs(15), stream.collect::<Vec<_>>())
            .await
            .expect("the run ends");
        (events, started.elapsed())
    }

    /// A stored step summary ends the run once no call is open; a failed
    /// CLI with no answer is an error naming its reason; a clean one ends
    /// with `Done`.
    #[tokio::test]
    async fn runs_end_as_the_cli_does() {
        let call = |id: &str, tool: &str, status: &str| {
            format!(
                r#"echo '{{"type":"item.{status}","item":{{"id":"{id}","type":"mcp_tool_call","server":"tengu-tools","tool":"{tool}","arguments":{{}},"result":null,"error":null,"status":"{}"}}}}'"#,
                if status == "started" {
                    "in_progress"
                } else {
                    "completed"
                }
            )
        };
        let script = format!(
            "{}\n{}\n{}\n{}\nexec sleep 30\n",
            call("a", "read_file", "started"),
            call("c", "compress_and_store", "started"),
            call("c", "compress_and_store", "completed"),
            call("a", "read_file", "completed"),
        );
        let (events, took) = run_stub(&script).await;
        assert!(took < Duration::from_secs(10), "not stopped: {took:?}");
        assert!(
            matches!(events.last(), Some(StreamEvent::Done)),
            "{events:?}"
        );

        let (events, _) = run_stub("echo 'Not logged in. Run codex login' >&2\nexit 1\n").await;
        assert!(
            matches!(events.last(), Some(StreamEvent::Error { message }) if message.contains("codex login")),
            "{events:?}"
        );

        let ok = format!("echo '{ANSWER}'\necho '{USAGE}'\n");
        let (events, _) = run_stub(&ok).await;
        assert!(
            matches!(events.last(), Some(StreamEvent::Done)),
            "{events:?}"
        );
        assert!(events
            .iter()
            .any(|e| matches!(e, StreamEvent::TextDelta { text } if text == "marker.txt")));
    }
}
