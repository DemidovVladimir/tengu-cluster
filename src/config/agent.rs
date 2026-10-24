//! `[agents.<a>]` — `AgentConfig` and its blocks (`[limits]`, `[flow]`,
//! `[identity]`, `[claude_code]` / `[local]` / `[codex]`), the engine list,
//! `[orchestrator]`, `[[mcp_servers]]` and the global `[claude_code]`.

use super::*;

/// A single external MCP server that tengu connects to as a client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Unique server name. Used as the prefix in `{server_name}__{tool_name}`.
    pub name: String,
    /// Transport: "stdio" or "http".
    pub transport: String,
    /// For stdio: `[program, arg1, arg2, ...]`.
    #[serde(default)]
    pub command: Vec<String>,
    /// For http: the JSON-RPC endpoint URL.
    #[serde(default)]
    pub url: Option<String>,
    /// Environment variables passed to the stdio subprocess. Values of the
    /// form `$VAR` are resolved from the parent process's environment.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Optional authentication applied to http transport.
    #[serde(default)]
    pub auth: Option<McpAuthConfig>,
}

/// Authentication for an MCP HTTP transport.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpAuthConfig {
    /// Auth scheme: currently only "bearer" is supported.
    #[serde(rename = "type")]
    pub auth_type: String,
    /// Token value. `$VAR` references are resolved from the process env.
    pub token: String,
}

/// Every `[agents.<a>] engine` value (`engines::build_engine` builds each;
/// the Studio builder palette describes each, `config/builder.rs`).
pub(crate) const ENGINES: &[&str] = &["openrouter", "claude_code", "local", "codex"];

/// Per-agent runtime configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
/// One block = one agent: in-process (planner, `@role:` chat) and, when it
/// carries a `description`, a planner-routable subagent (`tengu run-agent`).
/// Unknown keys are a parse error so a typo never silently disables a knob.
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    #[serde(default)]
    pub default: bool,
    pub engine: String,
    pub model: String,
    /// Planner-facing description. **Present ⇒ the agent is a routable
    /// subagent**: it is rendered into `TENGU_PLANNER_REGISTRY.md` and the
    /// planner may dispatch plan steps to it (`tengu run-agent`). Write it
    /// for the planner LLM: what the agent handles, what it is NOT for.
    /// Absent ⇒ in-process only (planner role, direct `@role:` chat).
    #[serde(default)]
    pub description: Option<String>,
    /// Example user questions rendered under the registry entry so the
    /// planner can match short casual messages. Only used with `description`.
    #[serde(default)]
    pub example_queries: Vec<String>,
    /// Tool allow-list on every surface: in-process chat (TUI, Telegram,
    /// webhooks, eval, `tengu tool`), `run-agent` steps, decision loops and
    /// the Claude Code bridge (`bootstrap::tools::agent_base_tools`). Empty =
    /// every base tool. Opt-in names (`domain::tools::WORKSPACE_TOOLS`)
    /// listed here are opted in exactly like `workspace_tools`.
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub workspace: Option<PathBuf>,
    #[serde(default)]
    pub identity: IdentityConfig,
    #[serde(default)]
    pub flow: FlowConfig,
    #[serde(default)]
    pub limits: LimitsConfig,
    /// Fleet orchestration role (qa|backend_engineer|integration_master).
    #[serde(default)]
    pub role: Option<String>,
    /// Skills loaded into the prompt (in-process) / system prompt (subagent).
    /// `skills = [...]` is accepted as an alias.
    #[serde(default, alias = "skills")]
    pub skill_packages: Vec<String>,
    #[serde(default)]
    pub prompt_budget: PromptBudgetConfig,
    /// Opt-in tools this agent can use — only names in
    /// `domain::tools::WORKSPACE_TOOLS` (anything else fails validation).
    #[serde(default)]
    pub workspace_tools: Vec<String>,
    /// Per-tool scope restrictions (default-deny). Key = tool name.
    /// See `ToolScope` in `domain/scope.rs` for field definitions.
    #[serde(default)]
    pub scopes: HashMap<String, ToolScope>,
    /// Per-agent Claude Code configuration (only used when engine = "claude_code").
    #[serde(default)]
    pub claude_code: Option<AgentClaudeCodeConfig>,
    /// Local OpenAI-compatible server (only used when engine = "local").
    /// Absent = defaults (Unsloth on `http://127.0.0.1:8888`).
    #[serde(default)]
    pub local: Option<AgentLocalConfig>,
    /// OpenAI Codex CLI on the ChatGPT subscription (only used when engine =
    /// "codex", `engines/codex.rs`). Absent = defaults (`sandbox =
    /// "read-only"`, `cli_path = "codex"`).
    #[serde(default)]
    pub codex: Option<AgentCodexConfig>,
    /// Runtime (never in TOML): set by `Config::fold_default_scopes` on every
    /// agent of a hardened sandbox (`config/hardening.rs`: a `[solana]`
    /// signer, a `[risk]` or an `[soe]` section) — tools without a configured scope then
    /// get a fallback that runs no shell (`bootstrap::tools::resolve_tool_scopes`,
    /// in-process and in the bridge), and no shell skill loads
    /// (`bootstrap::tools::agent_skill_registry`).
    /// Read as [`AgentConfig::hardened`].
    #[serde(skip)]
    pub no_shell_fallback: bool,
    /// Runtime (never in TOML): sandbox-level sections tools read at call
    /// time (`config/sections.rs`) — one `Arc` shared by every agent, set by
    /// `Config::fold_default_scopes`.
    #[serde(skip)]
    pub sandbox: std::sync::Arc<sections::SandboxSections>,
}

impl AgentConfig {
    /// A hardened sandbox (`config/hardening.rs`: `[risk]`, `[soe]` or a
    /// `[solana]` signer) — `fold_default_scopes` marks every agent of one
    /// (`no_shell_fallback`), the MCP bridge its fallback agent too. Its
    /// writers also refuse the workspace's prompt files
    /// (`domain::scope::protected_write_in`).
    pub fn hardened(&self) -> bool {
        self.no_shell_fallback
    }

    /// The most chars of one tool result this agent's model gets — and of
    /// one upstream output pasted into its plan step (`WorkerHandle::input_cap`):
    /// `limits.max_mcp_result_chars` on a CLI engine (the bridge's cap), else
    /// `limits.max_tool_result_chars`, and on `local` at most 1/8 of
    /// `limits.context_window` (`domain::token::tool_result_char_budget`, as
    /// `LocalEngine::tool_result_char_cap`).
    pub fn tool_result_cap(&self) -> usize {
        let limits = &self.limits;
        match self.engine.as_str() {
            "claude_code" | "codex" => limits.max_mcp_result_chars as usize,
            "local" => (limits.max_tool_result_chars as usize).min(
                crate::domain::token::tool_result_char_budget(limits.context_window as usize),
            ),
            _ => limits.max_tool_result_chars as usize,
        }
    }
}

/// Optional identity metadata used for prompts/UI.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IdentityConfig {
    #[serde(default)]
    pub name: Option<String>,
    /// Free-form instructions injected into the system prompt.
    #[serde(default)]
    pub instructions: Option<String>,
}

/// Flow/session behavior (scope and reset strategy).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowConfig {
    #[serde(default = "default_scope")]
    pub scope: String,
    #[serde(default = "default_reset_mode")]
    pub reset_mode: String,
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout_minutes: u32,
    #[serde(default)]
    pub max_history_turns: Option<u32>,
    #[serde(default)]
    pub compaction_threshold_ratio: Option<f32>,
    #[serde(default)]
    pub compaction_keep_turns: Option<u32>,
    #[serde(default)]
    pub compaction_summary_max_tokens: Option<u32>,
}

impl Default for FlowConfig {
    fn default() -> Self {
        Self {
            scope: default_scope(),
            reset_mode: default_reset_mode(),
            idle_timeout_minutes: default_idle_timeout(),
            max_history_turns: None,
            compaction_threshold_ratio: None,
            compaction_keep_turns: None,
            compaction_summary_max_tokens: None,
        }
    }
}

pub(super) fn default_scope() -> String {
    "per-sender".to_string()
}
pub(super) fn default_reset_mode() -> String {
    "idle".to_string()
}
pub(super) fn default_idle_timeout() -> u32 {
    30
}

/// Hard limits applied to flow lifecycle and budgeting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitsConfig {
    #[serde(default = "default_max_tokens")]
    pub max_tokens_per_flow: u64,
    /// Model window in tokens (default 1_000_000). `engine = "local"`: set
    /// the server's real window (load warns on the default); one tool
    /// result is capped at 1/8 of it (`domain::token::tool_result_char_budget`).
    #[serde(default = "default_context_window")]
    pub context_window: u32,
    #[serde(default)]
    pub max_output_tokens_per_turn: Option<u32>,
    /// Tool cap per turn (chat) or plan step (`run-agent`; default 70).
    ///
    /// | Engine | Counts |
    /// |---|---|
    /// | `openrouter`, `local` | engine turns (rounds; one may call several tools) — chat then forces one answer without tools, a `run-agent` step just ends |
    /// | `claude_code` | tool calls (`tool_use` blocks) of the CLI run — bridged and built-in; past the cap the run is killed with an error |
    #[serde(default = "default_max_tool_rounds")]
    pub max_tool_rounds: u32,
    #[serde(default = "default_max_tool_result_chars")]
    pub max_tool_result_chars: u32,
    #[serde(default = "default_stream_event_timeout_secs")]
    pub stream_event_timeout_secs: u64,
    /// Total wall-clock budget for a single non-streaming engine HTTP request,
    /// body read included. OpenRouter answers `stream: false` with HTTP 200 up
    /// front and holds the body open until generation ends, so slow reasoning
    /// models need headroom here or the read aborts with "error decoding
    /// response body". Defaults to 600s.
    #[serde(default = "default_request_timeout_secs")]
    pub request_timeout_secs: u64,
    /// Max chars for compacted (old-round) tool results. Defaults to 200.
    #[serde(default = "default_compact_result_limit")]
    pub compact_result_limit: u32,
    /// Max chars per MCP bridge tool result returned to Claude Code CLI.
    /// Prevents unbounded context growth in the Claude Code engine which
    /// has no per-turn compaction. Defaults to 50 000 (~12.5K tokens).
    #[serde(default = "default_max_mcp_result_chars")]
    pub max_mcp_result_chars: u32,
    /// Wall-clock cap for one plan step when this agent runs as a
    /// `tengu run-agent` subprocess (the parent kills the child after it).
    /// `max_tool_rounds` caps the child's LLM turns. Defaults to 600s.
    #[serde(default = "default_step_timeout_secs")]
    pub step_timeout_secs: u64,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_tokens_per_flow: default_max_tokens(),
            context_window: default_context_window(),
            max_output_tokens_per_turn: None,
            max_tool_rounds: default_max_tool_rounds(),
            max_tool_result_chars: default_max_tool_result_chars(),
            stream_event_timeout_secs: default_stream_event_timeout_secs(),
            request_timeout_secs: default_request_timeout_secs(),
            compact_result_limit: default_compact_result_limit(),
            max_mcp_result_chars: default_max_mcp_result_chars(),
            step_timeout_secs: default_step_timeout_secs(),
        }
    }
}

pub(super) fn default_context_window() -> u32 {
    1_000_000
}

/// Tools whose output lives in the agent's workspace (files, skills): a load
/// warning names them for a routable agent without `workspace`, whose plan
/// steps run in a temp dir removed after each (`validation_warnings`).
pub(super) const STEP_WORKSPACE_WRITERS: &[&str] = &[
    "write_file",
    "manage_skill",
    "skill_distill",
    "apply_improver_proposal",
];
pub(super) fn default_max_tool_rounds() -> u32 {
    70
}
pub(super) fn default_max_tool_result_chars() -> u32 {
    300_000
}
pub(super) fn default_stream_event_timeout_secs() -> u64 {
    120
}
pub(crate) fn default_request_timeout_secs() -> u64 {
    600
}
pub(super) fn default_compact_result_limit() -> u32 {
    200
}
pub(super) fn default_max_mcp_result_chars() -> u32 {
    50_000
}
pub(crate) fn default_step_timeout_secs() -> u64 {
    600
}

pub(super) fn default_max_tokens() -> u64 {
    100_000
}

/// Orchestration configuration. Presence activates orchestration;
/// absence falls back to single-agent-default dispatch. Unknown keys fail
/// the load (the old `engine = "rag"` too: the planner runs on `agent`'s
/// engine).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OrchestratorConfig {
    /// Name of the agent (in `Config.agents`) that acts as the
    /// orchestrator.
    pub agent: String,

    /// Tier 1: how many times a single step is retried before
    /// escalation.
    #[serde(default = "default_max_attempts_per_step")]
    pub max_attempts_per_step: u32,

    /// Tier 2: how many times the orchestrator is re-invoked to replan
    /// after exhaustion before bailing out.
    #[serde(default = "default_max_replans")]
    pub max_replans: u32,

    /// When `true`, `@role:`-prefixed messages are also routed through the
    /// orchestrator (the planner decides whether to honor or override the
    /// user's explicit target). When `false` (default), `@role:` bypasses
    /// the orchestrator and dispatches directly to the named agent —
    /// preserves the "talk to this agent specifically" escape hatch.
    #[serde(default)]
    pub route_explicit_agents: bool,
}

pub(super) fn default_max_attempts_per_step() -> u32 {
    3
}
pub(super) fn default_max_replans() -> u32 {
    2
}

/// Global Claude Code backend configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeCodeConfig {
    #[serde(default = "default_cli_path")]
    pub cli_path: String,
    #[serde(default = "default_claude_timeout_secs")]
    pub timeout_secs: u64,
}

impl Default for ClaudeCodeConfig {
    fn default() -> Self {
        Self {
            cli_path: default_cli_path(),
            timeout_secs: default_claude_timeout_secs(),
        }
    }
}

pub(super) fn default_cli_path() -> String {
    "claude".to_string()
}

pub(super) fn default_claude_timeout_secs() -> u64 {
    120
}

/// `[agents.<a>.claude_code] builtin_tools_profile` values — the Claude
/// Code CLI's own tools (`engines/claude_code.rs::BuiltinToolsProfile`; a
/// test keeps the two equal).
///
/// | Profile | Built-ins |
/// |---|---|
/// | `none` | none: tengu tools only (default; the only value a hardened sandbox allows) |
/// | `read_only` | Read, Glob, Grep |
/// | `research` | WebSearch — the search runs at Anthropic on the subscription; no Read / Glob / Grep (not workspace-bound); files and pages through bridged tools (`read_file`, `http_request`: scopes + egress), never WebFetch |
/// | `editor` | + Edit, Write, MultiEdit |
/// | `editor_shell` | + Bash |
pub const CLAUDE_CODE_PROFILES: &[&str] =
    &["none", "read_only", "research", "editor", "editor_shell"];

/// Per-agent Claude Code configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentClaudeCodeConfig {
    /// One of [`CLAUDE_CODE_PROFILES`], read trimmed; default `none` (no
    /// built-ins: every file, page and command through bridged, scoped
    /// tools).
    #[serde(default = "default_builtin_tools_profile")]
    pub builtin_tools_profile: String,
}

impl Default for AgentClaudeCodeConfig {
    fn default() -> Self {
        Self {
            builtin_tools_profile: default_builtin_tools_profile(),
        }
    }
}

/// Per-agent local inference server (`engine = "local"`). Any server with
/// `POST /v1/chat/completions` + OpenAI `tools`: Unsloth (`unsloth run`,
/// port 8888), llama.cpp `llama-server` (8080), Ollama (11434), vLLM (8000),
/// LM Studio (1234). `model` is sent verbatim (Unsloth: `GET /v1/models`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentLocalConfig {
    /// Server root; a trailing `/v1` (Ollama's documented form) is dropped.
    /// Loopback bypasses the Tor proxy.
    #[serde(default = "default_local_base_url")]
    pub base_url: String,
    /// Env var holding the bearer key (Unsloth: `sk-unsloth-…`). Unset or
    /// empty env = no `Authorization` header (llama.cpp, Ollama).
    #[serde(default = "default_local_api_key_env")]
    pub api_key_env: String,
}

impl Default for AgentLocalConfig {
    fn default() -> Self {
        Self {
            base_url: default_local_base_url(),
            api_key_env: default_local_api_key_env(),
        }
    }
}

/// `[agents.<a>.codex] sandbox` values: the Codex CLI's `-s` policy for
/// its own built-in shell and file edits (`engines/codex.rs`).
pub(crate) const CODEX_SANDBOXES: &[&str] = &["read-only", "workspace-write"];

/// `[agents.<a>.codex] web_search` values (`engines/codex.rs::CodexWebSearch`;
/// a test keeps the two equal).
pub(crate) const CODEX_WEB_SEARCH: &[&str] = &["disabled", "cached", "live"];

/// Per-agent OpenAI Codex CLI (`engine = "codex"`, `engines/codex.rs`): the
/// operator's ChatGPT subscription (`codex login`), never an API key — the
/// engine removes `OPENAI_API_KEY` / `CODEX_API_KEY` from the CLI's env.
/// `model` = a bare OpenAI model id the CLI accepts (`gpt-5.5`, `gpt-6-sol`;
/// `codex` lists them in `~/.codex/models_cache.json`). Tengu tools reach
/// the CLI through `tengu mcp-bridge`, as for Claude Code. Refused in a
/// hardened sandbox (`config/hardening.rs`): the CLI's built-in shell
/// ignores tengu scopes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentCodexConfig {
    /// Codex's own sandbox for its built-in shell and patch tool (`codex
    /// exec -s`): `read-only` (default: it may read files, write nothing,
    /// no network) or `workspace-write` (writes under the agent workspace,
    /// still no network). Tengu tools are not affected — scopes gate them.
    #[serde(default = "default_codex_sandbox")]
    pub sandbox: String,
    /// The `codex` executable (`PATH` lookup unless absolute).
    #[serde(default = "default_codex_cli_path")]
    pub cli_path: String,
    /// Env names the CLI gets (and forwards to `tengu mcp-bridge`) besides
    /// the harmless base set (`engines/codex.rs::BASE_ENV`). The CLI starts
    /// from an empty env, so vault secrets, `.env` keys and the seal-proxy
    /// session never reach it unless named here — and a name here is
    /// readable by the CLI's built-in shell, which ignores tengu scopes.
    /// List only what this agent's bridged tools must read. Refused at load
    /// ([`codex_forward_env_error`]): `TENGU_MASTER_PASSWORD`, `TENGU_KEYS_*`,
    /// every `[keys.env]` name, `OPENAI_API_KEY`, `CODEX_*`.
    #[serde(default)]
    pub forward_env: Vec<String>,
    /// OpenAI's hosted web search for the CLI (`codex exec -c web_search`):
    /// `disabled` (default), `cached` (OpenAI's index) or `live`. It runs at
    /// OpenAI on the subscription, not from this machine; pages the agent
    /// must read still come through tengu's `http_request` (scopes, egress).
    #[serde(default = "default_codex_web_search")]
    pub web_search: String,
}

impl Default for AgentCodexConfig {
    fn default() -> Self {
        Self {
            sandbox: default_codex_sandbox(),
            cli_path: default_codex_cli_path(),
            forward_env: Vec::new(),
            web_search: default_codex_web_search(),
        }
    }
}

pub(super) fn default_codex_web_search() -> String {
    "disabled".to_string()
}

/// Why `name` may not be in `[agents.<a>.codex] forward_env` (`None`: it
/// may). `keys_env` = the sandbox's `[keys.env]` names (route URLs and
/// `@session` values: the seal-proxy capability).
pub(crate) fn codex_forward_env_error(name: &str, keys_env: &[&str]) -> Option<String> {
    let valid = !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !valid {
        return Some(format!("'{name}' is not an env name"));
    }
    if name == "TENGU_MASTER_PASSWORD" {
        return Some("the vault's master password never reaches the Codex CLI".into());
    }
    if name.starts_with("TENGU_KEYS_") || keys_env.contains(&name) {
        return Some(format!(
            "'{name}' is the seal-proxy session or one of its [keys.env] exports — it would let the CLI's shell use every route"
        ));
    }
    if name == "OPENAI_API_KEY" || name.starts_with("CODEX_") {
        return Some(format!(
            "'{name}': the CLI runs on the ChatGPT login (an API key would bill the account; CODEX_HOME / CODEX_CA_CERTIFICATE are passed already)"
        ));
    }
    None
}

pub(super) fn default_codex_sandbox() -> String {
    "read-only".to_string()
}

pub(super) fn default_codex_cli_path() -> String {
    "codex".to_string()
}

pub(super) fn default_local_base_url() -> String {
    "http://127.0.0.1:8888".to_string()
}

pub(super) fn default_local_api_key_env() -> String {
    "UNSLOTH_API_KEY".to_string()
}

pub(super) fn default_builtin_tools_profile() -> String {
    "none".to_string()
}
