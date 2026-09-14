//! TOML-backed runtime configuration schema and runtime profile helpers.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use sysinfo::System;
use tracing::info;

use crate::adapters::ports::ToolScope;

// ---------------------------------------------------------------------------
// Runtime profile
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeProfile {
    /// High-resource environment (cloud GPU/large RAM).
    Cloud,
    /// Typical desktop/laptop environment.
    Desktop,
    /// Constrained environment (SBC/VPS/low-RAM).
    Minimal,
}

#[derive(Debug, Clone)]
pub struct SystemCapabilities {
    pub total_ram_mb: u64,
    pub available_ram_mb: u64,
    pub cpu_cores: usize,
    pub arch: String,
    pub has_gpu: bool,
}

impl SystemCapabilities {
    pub fn detect() -> Self {
        let mut sys = System::new_all();
        sys.refresh_all();

        let total_ram_mb = sys.total_memory() / (1024 * 1024);
        let available_ram_mb = sys.available_memory() / (1024 * 1024);
        let cpu_cores = sys.cpus().len();
        let arch = std::env::consts::ARCH.to_string();

        let has_gpu = Self::detect_gpu();

        Self {
            total_ram_mb,
            available_ram_mb,
            cpu_cores,
            arch,
            has_gpu,
        }
    }

    pub fn recommended_profile(&self) -> RuntimeProfile {
        match (self.available_ram_mb, self.has_gpu) {
            (ram, true) if ram > 16_000 => RuntimeProfile::Cloud,
            (ram, _) if ram > 4_000 => RuntimeProfile::Desktop,
            _ => RuntimeProfile::Minimal,
        }
    }

    fn detect_gpu() -> bool {
        if let Ok(hint) = std::env::var("TENGU_GPU_HINT") {
            let normalized = hint.trim().to_ascii_lowercase();
            match normalized.as_str() {
                "none" | "cpu" | "off" | "false" => return false,
                "gpu" | "cuda" | "metal" | "mps" | "on" | "true" => return true,
                _ => {}
            }
        }

        if std::env::var("CUDA_VISIBLE_DEVICES").is_ok() {
            return true;
        }

        if cfg!(target_os = "macos") && std::env::consts::ARCH == "aarch64" {
            return true;
        }
        false
    }
}

impl RuntimeProfile {
    pub fn resolve(configured: Option<&str>) -> Self {
        match configured {
            Some("cloud") => RuntimeProfile::Cloud,
            Some("desktop") => RuntimeProfile::Desktop,
            Some("minimal") => RuntimeProfile::Minimal,
            _ => {
                let caps = SystemCapabilities::detect();
                let profile = caps.recommended_profile();
                info!(
                    arch = %caps.arch,
                    ram_mb = caps.total_ram_mb,
                    available_mb = caps.available_ram_mb,
                    cores = caps.cpu_cores,
                    gpu = caps.has_gpu,
                    profile = ?profile,
                    "Auto-detected runtime profile"
                );
                profile
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Configuration schema
// ---------------------------------------------------------------------------

/// Root configuration object loaded from `config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_profile")]
    pub runtime_profile: String,

    #[serde(default)]
    pub hub: HubConfig,

    #[serde(default)]
    pub agents: HashMap<String, AgentConfig>,

    #[serde(default)]
    pub orchestrator: Option<OrchestratorConfig>,

    #[serde(default)]
    pub memory: MemoryConfig,

    #[serde(default)]
    pub telegram: TelegramConfig,

    /// Inbound webhook listener configuration. Absent / `enabled = false`
    /// means `tengu webhooks` will refuse to start. Each
    /// `[webhooks.endpoints.<name>]` block binds one URL path to one agent
    /// with a shared HMAC secret. See `WebhookConfig` for field docs.
    #[serde(default)]
    pub webhooks: WebhookConfig,

    #[serde(default)]
    pub scaffold: Option<ScaffoldConfig>,

    #[serde(default)]
    pub claude_code: Option<ClaudeCodeConfig>,

    /// Fallback scopes applied when an agent has no per-tool scope entry.
    /// Per-agent scopes override default_scopes wholesale (not field-merged).
    #[serde(default)]
    pub default_scopes: HashMap<String, ToolScope>,

    /// Inbound MCP client connections — external MCP servers this install
    /// connects to. At boot the MCP plugin connects to each entry, calls
    /// `tools/list`, and exposes every remote tool as `{server_name}.{tool}`.
    /// Default is empty: MCP is opt-in per user install.
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfig>,

    /// Skill-lifecycle subsystem configuration (eval runner, distill pipeline).
    /// Absent by default — the subsystem is fully opt-in.
    #[serde(default)]
    pub skill_lifecycle: Option<crate::adapters::skill_lifecycle::config::SkillLifecycleConfig>,

    /// Phase 7.2 — name of the sandbox this `Config` was loaded from, or
    /// `None` for the default user config. Populated by `load_sandbox_or` in
    /// `main.rs`. Plumbed through `SubprocessRunner` and the IPC payload so
    /// `tengu run-agent` children resolve `sandboxes/<name>/config.toml` for
    /// scopes/secrets/MCP servers — without this the parent + child would
    /// see different scope rules and tool calls would scope-deny in the
    /// child even when the sandbox config in the parent allows them.
    ///
    /// `#[serde(skip)]` because this is a runtime-resolved field, never
    /// written to the TOML on disk. Skip on serialise too so dumping the
    /// config doesn't leak it.
    #[serde(skip)]
    pub sandbox_name: Option<String>,
}

/// A single external MCP server that tengu connects to as a client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Unique server name. Used as the prefix in `{server_name}.{tool_name}`.
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

fn default_profile() -> String {
    "auto".to_string()
}

/// Hub runtime/network configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubConfig {
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_auth_mode")]
    pub auth_mode: String,
    #[serde(default)]
    pub auth_token: Option<String>,
    #[serde(default)]
    pub reload: ReloadConfig,
}

impl Default for HubConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            port: default_port(),
            auth_mode: default_auth_mode(),
            auth_token: None,
            reload: ReloadConfig::default(),
        }
    }
}

fn default_bind() -> String {
    "127.0.0.1".to_string()
}
fn default_port() -> u16 {
    7070
}
fn default_auth_mode() -> String {
    "token".to_string()
}

/// Hot-reload behavior for runtime configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReloadConfig {
    #[serde(default = "default_reload_mode")]
    pub mode: String,
    #[serde(default = "default_debounce_ms")]
    pub debounce_ms: u64,
}

impl Default for ReloadConfig {
    fn default() -> Self {
        Self {
            mode: default_reload_mode(),
            debounce_ms: default_debounce_ms(),
        }
    }
}

fn default_reload_mode() -> String {
    "hybrid".to_string()
}
fn default_debounce_ms() -> u64 {
    300
}

/// Per-agent runtime configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    #[serde(default)]
    pub default: bool,
    pub engine: String,
    pub model: String,
    #[serde(default)]
    pub workspace: Option<PathBuf>,
    #[serde(default = "default_lens")]
    pub default_lens: String,
    #[serde(default)]
    pub identity: IdentityConfig,
    #[serde(default)]
    pub flow: FlowConfig,
    #[serde(default)]
    pub limits: LimitsConfig,
    #[serde(default)]
    pub lens: LensConfig,
    /// Fleet orchestration role (qa|backend_engineer|integration_master).
    #[serde(default)]
    pub role: Option<String>,
    /// Optional workflow/document packages loaded into the prompt and tool registry.
    #[serde(default)]
    pub skill_packages: Vec<String>,
    #[serde(default)]
    pub prompt_budget: PromptBudgetConfig,
    /// Optional first-party workspace tools this agent can use (e.g. "shared_cache").
    #[serde(default)]
    pub workspace_tools: Vec<String>,
    /// Per-tool scope restrictions (default-deny). Key = tool name.
    /// See `ToolScope` in `ports.rs` for field definitions.
    #[serde(default)]
    pub scopes: HashMap<String, ToolScope>,
    /// Per-agent Claude Code configuration (only used when engine = "claude_code").
    #[serde(default)]
    pub claude_code: Option<AgentClaudeCodeConfig>,
}

fn default_lens() -> String {
    "eco".to_string()
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

fn default_scope() -> String {
    "per-sender".to_string()
}
fn default_reset_mode() -> String {
    "idle".to_string()
}
fn default_idle_timeout() -> u32 {
    30
}

/// Hard limits applied to flow lifecycle and budgeting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitsConfig {
    #[serde(default = "default_max_tokens")]
    pub max_tokens_per_flow: u64,
    #[serde(default)]
    pub max_cost_per_flow: Option<f64>,
    #[serde(default)]
    pub warn_at_cost: Option<f64>,
    #[serde(default = "default_context_window")]
    pub context_window: u32,
    #[serde(default)]
    pub max_output_tokens_per_turn: Option<u32>,
    #[serde(default = "default_max_tool_rounds")]
    pub max_tool_rounds: u32,
    #[serde(default = "default_max_tool_result_chars")]
    pub max_tool_result_chars: u32,
    #[serde(default = "default_stream_event_timeout_secs")]
    pub stream_event_timeout_secs: u64,
    /// Max chars for compacted (old-round) tool results. Defaults to 200.
    #[serde(default = "default_compact_result_limit")]
    pub compact_result_limit: u32,
    /// Max chars per MCP bridge tool result returned to Claude Code CLI.
    /// Prevents unbounded context growth in the Claude Code engine which
    /// has no per-turn compaction. Defaults to 50 000 (~12.5K tokens).
    #[serde(default = "default_max_mcp_result_chars")]
    pub max_mcp_result_chars: u32,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_tokens_per_flow: default_max_tokens(),
            max_cost_per_flow: None,
            warn_at_cost: None,
            context_window: default_context_window(),
            max_output_tokens_per_turn: None,
            max_tool_rounds: default_max_tool_rounds(),
            max_tool_result_chars: default_max_tool_result_chars(),
            stream_event_timeout_secs: default_stream_event_timeout_secs(),
            compact_result_limit: default_compact_result_limit(),
            max_mcp_result_chars: default_max_mcp_result_chars(),
        }
    }
}

fn default_context_window() -> u32 {
    1_000_000
}
fn default_max_tool_rounds() -> u32 {
    70
}
fn default_max_tool_result_chars() -> u32 {
    300_000
}
fn default_stream_event_timeout_secs() -> u64 {
    120
}
fn default_compact_result_limit() -> u32 {
    200
}
fn default_max_mcp_result_chars() -> u32 {
    50_000
}

fn default_max_tokens() -> u64 {
    100_000
}

/// Orchestration configuration. Presence activates orchestration;
/// absence falls back to single-agent-default dispatch.
#[derive(Debug, Clone, Deserialize, Serialize)]
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

    /// Planner engine. `"rag"` (default) is the only supported value — the
    /// name is historical (Phase 4 of the redesign); the current planner is
    /// `RagPlanner` (file-backed `TENGU_PLANNER_REGISTRY.md`) + the
    /// `SubprocessRunner` worker. The legacy `"static"` roster engine was
    /// removed in Phase 7.1; any other value fails config validation.
    #[serde(default = "default_orchestrator_engine")]
    pub engine: String,
}

fn default_max_attempts_per_step() -> u32 {
    3
}
fn default_max_replans() -> u32 {
    2
}
fn default_orchestrator_engine() -> String {
    "rag".to_string()
}

/// Telegram bot adapter configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TelegramConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub allowed_users: Vec<String>,
    #[serde(default)]
    pub tool_approvals: bool,
    #[serde(default)]
    pub approve_only: Vec<String>,
}

// =====================================================================
// Webhook listener (added 2026-05-09) — `tengu webhooks --sandbox <name>`
// =====================================================================

/// Inbound HTTP webhook listener.
///
/// Bound to the orchestrator: each endpoint is a `(URL path → agent +
/// secret)` triple. Incoming POSTs are verified with HMAC-SHA256 against
/// the endpoint's `secret_env` (the value of the named env var), then
/// dispatched as a one-shot orchestrator turn with a per-request
/// `session_id` of the form `webhook-<endpoint-name>-<uuid>`.
///
/// The listener responds **202 Accepted** with `{"session_id": "..."}`
/// immediately — agents can take minutes, longer than typical webhook
/// timeouts. Final agent output goes to `tengu_outputs` (Fix C+D apply
/// on every step) and the tracing log; recall it later with
/// `tengu memory inspect --session webhook-<name>-<uuid>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookConfig {
    /// Whether the listener is enabled. `tengu webhooks --sandbox <name>`
    /// refuses to start if `enabled = false`. Default: `false` (off).
    #[serde(default)]
    pub enabled: bool,
    /// Bind address. Default `127.0.0.1` — only loopback by default;
    /// set to `0.0.0.0` to expose externally (paired with HMAC auth).
    #[serde(default = "default_webhook_bind")]
    pub bind: String,
    /// TCP port. Default `7080`.
    #[serde(default = "default_webhook_port")]
    pub port: u16,
    /// Per-endpoint configuration. Map of `<endpoint-name> →
    /// WebhookEndpointConfig`. URL surfaces as `/webhooks/<endpoint-name>`.
    #[serde(default)]
    pub endpoints: HashMap<String, WebhookEndpointConfig>,
}

impl Default for WebhookConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: default_webhook_bind(),
            port: default_webhook_port(),
            endpoints: HashMap::new(),
        }
    }
}

/// One inbound webhook endpoint binding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookEndpointConfig {
    /// Agent name to dispatch to (must match an `agents/<name>.toml`).
    pub agent: String,
    /// Name of the environment variable holding the HMAC shared secret.
    /// At verify time the listener reads `std::env::var(secret_env)`;
    /// missing env var → 500 Internal Error (the listener cannot verify
    /// the signature without the secret). Pair the env var with your
    /// secrets manager / `.env` file. Mutually exclusive with `secret`.
    #[serde(default)]
    pub secret_env: Option<String>,
    /// Inline shared secret. Less safe than `secret_env` (the secret
    /// ends up in the TOML on disk). Use only when `secret_env` is
    /// inconvenient (e.g. local dev). Mutually exclusive with `secret_env`.
    #[serde(default)]
    pub secret: Option<String>,
    /// Prefix prepended to the synthesized user message that the
    /// orchestrator receives. The full user message is
    /// `<goal_template>\n\nPayload (JSON):\n<request body>`. Defaults to
    /// `"A webhook arrived. Process the payload below."` if omitted.
    #[serde(default = "default_webhook_goal_template")]
    pub goal_template: String,
}

fn default_webhook_bind() -> String {
    "127.0.0.1".to_string()
}

fn default_webhook_port() -> u16 {
    7080
}

fn default_webhook_goal_template() -> String {
    "A webhook arrived. Process the payload below.".to_string()
}

/// Workspace scaffold — auto-creates directories and seed files on startup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScaffoldConfig {
    pub root: String,
    #[serde(default)]
    pub directories: Vec<String>,
    #[serde(default)]
    pub files: Vec<ScaffoldFile>,
    #[serde(default)]
    pub project: Option<ProjectScaffold>,
}

/// Template for per-project scaffolding (applied by `/project <name>`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectScaffold {
    #[serde(default)]
    pub directories: Vec<String>,
    #[serde(default)]
    pub files: Vec<ScaffoldFile>,
}

/// A file to seed into the workspace during scaffold.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScaffoldFile {
    pub path: String,
    pub content: String,
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

fn default_cli_path() -> String {
    "claude".to_string()
}

fn default_claude_timeout_secs() -> u64 {
    120
}

/// Per-agent Claude Code configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentClaudeCodeConfig {
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

fn default_builtin_tools_profile() -> String {
    "editor_shell".to_string()
}

/// Persistent vector memory configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_embedding_model")]
    pub embedding_model: String,
    #[serde(default = "default_max_recall_entries")]
    pub max_recall_entries: usize,
    #[serde(default = "default_max_recall_tokens")]
    pub max_recall_tokens: usize,
    #[serde(default = "default_store_path")]
    pub store_path: String,
    /// Persistent store: chunk size in characters for file vectorization.
    #[serde(default = "default_persistent_store_chunk_size")]
    pub persistent_store_chunk_size: usize,
    /// Persistent store: overlap in characters between consecutive chunks.
    #[serde(default = "default_persistent_store_chunk_overlap")]
    pub persistent_store_chunk_overlap: usize,

    /// Number of most-recent session messages the orchestrator reloads
    /// each turn (by `session_id`, ordered by timestamp) for multi-turn
    /// dialogue coherence. Deterministic, not vector-search based.
    #[serde(default = "default_session_recent_n")]
    pub session_recent_n: usize,
    /// Top-K breadth for fuzzy cross-plan recall during replan (vector
    /// search over `tengu_outputs` only; messages are excluded to keep
    /// signal/noise apart).
    #[serde(default = "default_cross_plan_top_k")]
    pub cross_plan_top_k: usize,
    /// Top-K breadth for fuzzy cross-session message recall (vector
    /// search over `tengu_messages`). Defaults to `0` = off — the planner
    /// prompt stays unchanged for users who haven't opted in. Pair with
    /// Phase 6.4 (full) durable user-message persistence: setting this
    /// to e.g. `3` injects a "Cross-session message recall" block of the
    /// top-3 semantically-similar prior user messages before the current
    /// turn, surviving across restarts and across `session_id`s. Use
    /// sparingly — values >5 inflate the planner prompt without much
    /// signal gain because conversational text tends to cluster.
    #[serde(default = "default_cross_session_msg_top_k")]
    pub cross_session_msg_top_k: usize,
    /// Top-K breadth for fuzzy WITHIN-session output recall on the planner's
    /// normal `plan()` path (vector search over `tengu_outputs` filtered by
    /// the planner's `session_id`). Defaults to `0` = off — back-compat.
    /// Pair with the unified-session_id wiring (Fix B 2026-05-09): the
    /// planner and `SubprocessRunner` share one `session_id`, so step
    /// outputs persisted by `compress_and_store` in this session are
    /// retrievable on the next user turn. Without this, `tengu_outputs`
    /// is only read on `replan()` (`cross_plan_top_k`) — which is why
    /// follow-up questions like "was the molecule project created?"
    /// previously got "I have no record of that step" answers.
    /// Recommended `3–5` once your sandbox config opts in.
    #[serde(default = "default_within_session_output_top_k")]
    pub within_session_output_top_k: usize,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            embedding_model: default_embedding_model(),
            max_recall_entries: default_max_recall_entries(),
            max_recall_tokens: default_max_recall_tokens(),
            store_path: default_store_path(),
            persistent_store_chunk_size: default_persistent_store_chunk_size(),
            persistent_store_chunk_overlap: default_persistent_store_chunk_overlap(),
            session_recent_n: default_session_recent_n(),
            cross_plan_top_k: default_cross_plan_top_k(),
            cross_session_msg_top_k: default_cross_session_msg_top_k(),
            within_session_output_top_k: default_within_session_output_top_k(),
        }
    }
}

fn default_session_recent_n() -> usize {
    10
}
fn default_cross_plan_top_k() -> usize {
    5
}
fn default_cross_session_msg_top_k() -> usize {
    0
}
fn default_within_session_output_top_k() -> usize {
    0
}

fn default_embedding_model() -> String {
    crate::adapters::memory::vector::embedder::DEFAULT_EMBEDDING_MODEL.to_string()
}
fn default_max_recall_entries() -> usize {
    5
}
fn default_max_recall_tokens() -> usize {
    600
}
fn default_store_path() -> String {
    "~/.tengu/memory/".to_string()
}

fn default_persistent_store_chunk_size() -> usize {
    1000
}

fn default_persistent_store_chunk_overlap() -> usize {
    200
}

/// System prompt token budget configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptBudgetConfig {
    #[serde(default = "default_max_file_tokens")]
    pub max_file_tokens: usize,
    #[serde(default = "default_max_skill_context_tokens")]
    pub max_skill_context_tokens: usize,
    #[serde(default = "default_max_total_tokens")]
    pub max_total_tokens: usize,
}

impl Default for PromptBudgetConfig {
    fn default() -> Self {
        Self {
            max_file_tokens: default_max_file_tokens(),
            max_skill_context_tokens: default_max_skill_context_tokens(),
            max_total_tokens: default_max_total_tokens(),
        }
    }
}

fn default_max_file_tokens() -> usize {
    2000
}
fn default_max_skill_context_tokens() -> usize {
    16000
}
fn default_max_total_tokens() -> usize {
    32000
}

/// Lens-specific retrieval and budgeting parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LensConfig {
    #[serde(default = "default_eco_max")]
    pub eco_max_tokens: u32,
    #[serde(default = "default_threshold")]
    pub standard_threshold: f32,
    #[serde(default = "default_budget")]
    pub precise_budget: f32,
}

impl Default for LensConfig {
    fn default() -> Self {
        Self {
            eco_max_tokens: default_eco_max(),
            standard_threshold: default_threshold(),
            precise_budget: default_budget(),
        }
    }
}

fn default_eco_max() -> u32 {
    100
}
fn default_threshold() -> f32 {
    0.7
}
fn default_budget() -> f32 {
    0.5
}

/// Aggregates config validation issues.
#[derive(Debug, Default)]
struct ValidationErrors {
    issues: Vec<String>,
}

impl ValidationErrors {
    fn push(&mut self, message: impl Into<String>) {
        self.issues.push(message.into());
    }

    fn require(&mut self, condition: bool, message: impl Into<String>) {
        if !condition {
            self.push(message);
        }
    }

    fn require_nonempty(&mut self, path: &str, value: &str) {
        self.require(!value.trim().is_empty(), format!("{path} cannot be empty"));
    }

    fn require_one_of(&mut self, path: &str, value: &str, allowed: &[&str]) {
        let normalized = value.trim();
        if allowed.contains(&normalized) {
            return;
        }
        self.push(format!(
            "{path} must be one of {} (got '{}')",
            allowed.join("|"),
            value
        ));
    }

    fn require_positive_opt_u32(&mut self, path: &str, value: Option<u32>) {
        if matches!(value, Some(0)) {
            self.push(format!("{path} must be greater than 0 when set"));
        }
    }

    fn require_positive_opt_f64(&mut self, path: &str, value: Option<f64>) {
        if let Some(v) = value {
            if v <= 0.0 {
                self.push(format!("{path} must be greater than 0 when set"));
            }
        }
    }

    fn into_vec(self) -> Vec<String> {
        self.issues
    }
}

impl Config {
    /// Load config from path and apply `${ENV_VAR}` substitution.
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let content = Self::substitute_env_vars(&content)?;
        let mut config: Config = toml::from_str(&content)?;
        config.validate()?;
        config.fold_default_scopes();
        Ok(config)
    }

    /// Post-load normalisation: fold `[default_scopes]` into every agent's
    /// `scopes` map (per-agent entries win wholesale, never field-merged) and
    /// expand `~` in `fs_roots`. `build_tool_executor` only holds an
    /// `AgentConfig`, so the fallback has to be materialised here — mirrors
    /// what `agent_config_from_spec` does for subprocess children.
    pub fn fold_default_scopes(&mut self) {
        for agent in self.agents.values_mut() {
            for (tool, scope) in &self.default_scopes {
                agent
                    .scopes
                    .entry(tool.clone())
                    .or_insert_with(|| scope.clone());
            }
            for scope in agent.scopes.values_mut() {
                for root in scope.fs_roots.iter_mut() {
                    *root = crate::adapters::tool_builder::expand_tilde(root);
                }
            }
        }
        for scope in self.default_scopes.values_mut() {
            for root in scope.fs_roots.iter_mut() {
                *root = crate::adapters::tool_builder::expand_tilde(root);
            }
        }
    }

    /// Validate cross-field configuration invariants.
    pub fn validate(&self) -> anyhow::Result<()> {
        let errors = self.validation_errors();
        if errors.is_empty() {
            return Ok(());
        }

        let message = format!(
            "Config validation failed ({} issue{}):\n{}",
            errors.len(),
            if errors.len() == 1 { "" } else { "s" },
            errors
                .iter()
                .map(|entry| format!("- {}", entry))
                .collect::<Vec<_>>()
                .join("\n")
        );
        Err(anyhow::anyhow!(message))
    }

    fn validation_errors(&self) -> Vec<String> {
        let mut errors = ValidationErrors::default();

        errors.require_one_of(
            "runtime_profile",
            &self.runtime_profile,
            &["auto", "cloud", "desktop", "minimal"],
        );
        errors.require_nonempty("hub.bind", &self.hub.bind);
        errors.require(self.hub.port > 0, "hub.port must be greater than 0");
        errors.require_one_of("hub.auth_mode", &self.hub.auth_mode, &["token", "open"]);
        errors.require_one_of(
            "hub.reload.mode",
            &self.hub.reload.mode,
            &["hybrid", "hot", "restart", "off"],
        );

        errors.require(
            !self.agents.is_empty(),
            "at least one agent must be configured",
        );

        let default_count = self.agents.values().filter(|agent| agent.default).count();
        if default_count > 1 {
            errors.push("only one agent can have default=true");
        }

        if let Some(orch) = &self.orchestrator {
            errors.require_one_of("orchestrator.engine", &orch.engine, &["rag"]);
        }

        self.agents.iter().for_each(|(agent_id, agent)| {
            Self::validate_agent(agent_id, agent, &mut errors);
        });

        errors.into_vec()
    }

    fn validate_agent(agent_id: &str, agent: &AgentConfig, errors: &mut ValidationErrors) {
        errors.require(!agent_id.trim().is_empty(), "agent id cannot be empty");
        errors.require_nonempty(&format!("agents.{agent_id}.engine"), &agent.engine);
        errors.require_one_of(
            &format!("agents.{agent_id}.engine"),
            &agent.engine,
            &["openrouter", "claude_code"],
        );
        errors.require_nonempty(&format!("agents.{agent_id}.model"), &agent.model);
        if agent.engine == "claude_code" {
            if let Some(ref cc) = agent.claude_code {
                errors.require_one_of(
                    &format!("agents.{agent_id}.claude_code.builtin_tools_profile"),
                    &cc.builtin_tools_profile,
                    &["none", "read_only", "editor", "editor_shell"],
                );
            }
        }
        errors.require_one_of(
            &format!("agents.{agent_id}.default_lens"),
            &agent.default_lens,
            &["eco", "standard", "precise"],
        );

        errors.require_one_of(
            &format!("agents.{agent_id}.flow.scope"),
            &agent.flow.scope,
            &["main", "per-group", "per-pipe-sender", "per-sender"],
        );
        errors.require_one_of(
            &format!("agents.{agent_id}.flow.reset_mode"),
            &agent.flow.reset_mode,
            &["idle", "manual", "time"],
        );
        if let Some(value) = agent.flow.compaction_threshold_ratio {
            if !(0.0..=1.0).contains(&value) || value == 0.0 {
                errors.push(format!(
                    "agents.{}.flow.compaction_threshold_ratio must be within (0.0, 1.0]",
                    agent_id
                ));
            }
        }
        errors.require_positive_opt_u32(
            &format!("agents.{agent_id}.flow.max_history_turns"),
            agent.flow.max_history_turns,
        );
        errors.require_positive_opt_u32(
            &format!("agents.{agent_id}.flow.compaction_keep_turns"),
            agent.flow.compaction_keep_turns,
        );
        errors.require_positive_opt_u32(
            &format!("agents.{agent_id}.flow.compaction_summary_max_tokens"),
            agent.flow.compaction_summary_max_tokens,
        );

        errors.require(
            agent.limits.max_tokens_per_flow > 0,
            format!("agents.{agent_id}.limits.max_tokens_per_flow must be greater than 0"),
        );
        errors.require_positive_opt_f64(
            &format!("agents.{agent_id}.limits.max_cost_per_flow"),
            agent.limits.max_cost_per_flow,
        );
        errors.require_positive_opt_f64(
            &format!("agents.{agent_id}.limits.warn_at_cost"),
            agent.limits.warn_at_cost,
        );
        if let (Some(max_cost), Some(warn_cost)) =
            (agent.limits.max_cost_per_flow, agent.limits.warn_at_cost)
        {
            if warn_cost > max_cost {
                errors.push(format!(
                    "agents.{}.limits.warn_at_cost cannot exceed max_cost_per_flow",
                    agent_id
                ));
            }
        }
        if let Some(output) = agent.limits.max_output_tokens_per_turn {
            let context = agent.limits.context_window;
            if output > context {
                errors.push(format!(
                    "agents.{}.limits.max_output_tokens_per_turn cannot exceed context_window",
                    agent_id
                ));
            }
        }

        if let Some(ref role) = agent.role {
            errors.require(
                !role.trim().is_empty(),
                format!("agents.{agent_id}.role cannot be empty when set"),
            );
        }

        errors.require(
            agent.lens.eco_max_tokens > 0,
            format!("agents.{agent_id}.lens.eco_max_tokens must be greater than 0"),
        );
        if !(0.0..=1.0).contains(&agent.lens.standard_threshold) {
            errors.push(format!(
                "agents.{}.lens.standard_threshold must be within [0.0, 1.0]",
                agent_id
            ));
        }
        if !(0.0..=1.0).contains(&agent.lens.precise_budget) {
            errors.push(format!(
                "agents.{}.lens.precise_budget must be within [0.0, 1.0]",
                agent_id
            ));
        }

        let pb = &agent.prompt_budget;
        let pb_prefix = format!("agents.{agent_id}.prompt_budget");
        errors.require(
            pb.max_file_tokens > 0,
            format!("{pb_prefix}.max_file_tokens must be greater than 0"),
        );
        errors.require(
            pb.max_skill_context_tokens > 0,
            format!("{pb_prefix}.max_skill_context_tokens must be greater than 0"),
        );
        errors.require(
            pb.max_total_tokens > 0,
            format!("{pb_prefix}.max_total_tokens must be greater than 0"),
        );
        errors.require(
            pb.max_file_tokens <= pb.max_total_tokens,
            format!("{pb_prefix}.max_file_tokens cannot exceed max_total_tokens"),
        );
        errors.require(
            pb.max_skill_context_tokens <= pb.max_total_tokens,
            format!("{pb_prefix}.max_skill_context_tokens cannot exceed max_total_tokens"),
        );

        let valid_workspace_tools = [
            "agentic_memory",
            "shared_cache",
            "persistent_store",
            "skill_distill",
            "apply_improver_proposal",
            "manage_skill",
        ];
        for wt in &agent.workspace_tools {
            if !valid_workspace_tools.contains(&wt.as_str()) {
                errors.push(format!(
                    "agents.{agent_id}.workspace_tools: unknown tool '{}' (valid: {})",
                    wt,
                    valid_workspace_tools.join(", ")
                ));
            }
        }
    }

    fn substitute_env_vars(content: &str) -> anyhow::Result<String> {
        let mut result = content.to_string();
        let re = regex::Regex::new(r"\$\{([A-Z_][A-Z0-9_]*)\}").unwrap();

        for cap in re.captures_iter(content) {
            let full_match = cap.get(0).unwrap().as_str();
            let var_name = &cap[1];
            if let Ok(value) = std::env::var(var_name) {
                result = result.replace(full_match, &value);
            }
        }

        Ok(result)
    }

    /// Resolve the effective ToolScope for a given agent + tool.
    /// Per-agent scopes override default_scopes wholesale (not field-merged).
    /// Returns None if neither agent nor default_scopes has an entry.
    #[allow(dead_code)] // Phase A wires this into tool registry construction
    pub fn resolve_scope(&self, agent_name: &str, tool_name: &str) -> Option<ToolScope> {
        if let Some(agent) = self.agents.get(agent_name) {
            if let Some(scope) = agent.scopes.get(tool_name) {
                return Some(scope.clone());
            }
        }
        self.default_scopes.get(tool_name).cloned()
    }
}

impl Default for Config {
    fn default() -> Self {
        let mut agents = HashMap::new();
        agents.insert(
            "main".to_string(),
            AgentConfig {
                default: true,
                engine: "openrouter".to_string(),
                model: "anthropic/claude-sonnet-4.6".to_string(),
                workspace: None,
                default_lens: "eco".to_string(),
                identity: IdentityConfig {
                    name: Some("Tengu".to_string()),
                    instructions: None,
                },
                flow: FlowConfig::default(),
                limits: LimitsConfig::default(),
                lens: LensConfig::default(),
                role: None,
                skill_packages: vec![],
                prompt_budget: PromptBudgetConfig::default(),
                workspace_tools: vec![],
                scopes: HashMap::new(),
                claude_code: None,
            },
        );

        Self {
            runtime_profile: "auto".to_string(),
            hub: HubConfig::default(),
            agents,
            orchestrator: None,
            memory: MemoryConfig::default(),
            telegram: TelegramConfig::default(),
            webhooks: WebhookConfig::default(),
            scaffold: None,
            claude_code: None,
            default_scopes: HashMap::new(),
            mcp_servers: Vec::new(),
            skill_lifecycle: None,
            sandbox_name: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_accepts_default_config() {
        let config = Config::default();
        assert!(config.validate().is_ok());
    }

    /// Webhook listener — defaults are `enabled = false`, loopback bind,
    /// port 7080, no endpoints. The `tengu webhooks` subcommand refuses
    /// to start under these defaults; users opt in by editing TOML.
    #[test]
    fn webhook_config_defaults_off_and_safe() {
        let cfg = WebhookConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.bind, "127.0.0.1");
        assert_eq!(cfg.port, 7080);
        assert!(cfg.endpoints.is_empty());
    }

    /// Webhook listener — full TOML round-trip through the standard
    /// `[webhooks]` + `[webhooks.endpoints.<name>]` shape.
    #[test]
    fn webhook_config_parses_endpoints() {
        let toml_str = r#"
[webhooks]
enabled = true
bind = "0.0.0.0"
port = 9000

[webhooks.endpoints.github]
agent = "reviewer"
secret_env = "GITHUB_WEBHOOK_SECRET"
goal_template = "GitHub PR webhook arrived."

[webhooks.endpoints.local_test]
agent = "aura"
secret = "literal-dev-secret"
"#;
        let parsed: toml::Value = toml::from_str(toml_str).unwrap();
        let webhooks: WebhookConfig = parsed.get("webhooks").unwrap().clone().try_into().unwrap();
        assert!(webhooks.enabled);
        assert_eq!(webhooks.bind, "0.0.0.0");
        assert_eq!(webhooks.port, 9000);
        assert_eq!(webhooks.endpoints.len(), 2);

        let github = webhooks.endpoints.get("github").unwrap();
        assert_eq!(github.agent, "reviewer");
        assert_eq!(github.secret_env.as_deref(), Some("GITHUB_WEBHOOK_SECRET"));
        assert!(github.secret.is_none());
        assert_eq!(github.goal_template, "GitHub PR webhook arrived.");

        let local = webhooks.endpoints.get("local_test").unwrap();
        assert_eq!(local.secret.as_deref(), Some("literal-dev-secret"));
        assert!(local.secret_env.is_none());
        // Default goal_template applied when omitted.
        assert!(local.goal_template.contains("webhook arrived"));
    }

    /// Fix A (2026-05-09) — `within_session_output_top_k` defaults to 0
    /// (off) so existing users see no behaviour change. Pair with the
    /// matching default for `cross_session_msg_top_k`.
    #[test]
    fn memory_config_within_session_output_top_k_defaults_off() {
        let cfg = MemoryConfig::default();
        assert_eq!(cfg.within_session_output_top_k, 0);
        assert_eq!(cfg.cross_session_msg_top_k, 0);
        // Replan-side cross-plan recall stays on by default — it's only
        // wired into the replan() path, never plan().
        assert_eq!(cfg.cross_plan_top_k, 5);
    }

    /// Fix A — confirm the field round-trips through TOML and keeps the
    /// default when absent in the source. This is the back-compat guard:
    /// pre-Fix-A `[memory]` blocks must still parse.
    #[test]
    fn memory_config_within_session_output_top_k_optional_in_toml() {
        let toml_str = "[memory]\nenabled = true\n";
        let parsed: toml::Value = toml::from_str(toml_str).unwrap();
        let mem_section = parsed.get("memory").unwrap().clone();
        let mem: MemoryConfig = mem_section.try_into().unwrap();
        assert_eq!(mem.within_session_output_top_k, 0);

        let toml_str_with = "[memory]\nenabled = true\nwithin_session_output_top_k = 4\n";
        let parsed: toml::Value = toml::from_str(toml_str_with).unwrap();
        let mem_section = parsed.get("memory").unwrap().clone();
        let mem: MemoryConfig = mem_section.try_into().unwrap();
        assert_eq!(mem.within_session_output_top_k, 4);
    }

    #[test]
    fn validate_rejects_output_cap_above_context_override() {
        let mut config = Config::default();
        let main = config.agents.get_mut("main").expect("main agent");
        main.limits.context_window = 4_096;
        main.limits.max_output_tokens_per_turn = Some(8_192);

        let err = config.validate().expect_err("expected validation error");
        assert!(err.to_string().contains("cannot exceed context_window"));
    }

    #[test]
    fn validate_rejects_unknown_engine() {
        let mut config = Config::default();
        let main = config.agents.get_mut("main").expect("main agent");
        main.engine = "ollama".to_string();

        let err = config.validate().expect_err("expected validation error");
        assert!(err.to_string().contains("engine must be one of"));
    }

    #[test]
    fn orchestrator_engine_defaults_to_rag() {
        let toml_str = r#"
[orchestrator]
agent = "main"
"#;
        let parsed: toml::Value = toml::from_str(toml_str).unwrap();
        let orch: OrchestratorConfig = parsed
            .get("orchestrator")
            .unwrap()
            .clone()
            .try_into()
            .unwrap();
        assert_eq!(orch.engine, "rag");
    }

    #[test]
    fn validate_rejects_unknown_orchestrator_engine() {
        let mut config = Config::default();
        config.orchestrator = Some(OrchestratorConfig {
            agent: "main".to_string(),
            max_attempts_per_step: default_max_attempts_per_step(),
            max_replans: default_max_replans(),
            route_explicit_agents: false,
            engine: "static".to_string(),
        });
        let err = config.validate().expect_err("expected validation error");
        assert!(err
            .to_string()
            .contains("orchestrator.engine must be one of"));

        config.orchestrator.as_mut().unwrap().engine = "rag".to_string();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn memory_config_ignores_removed_qdrant_keys() {
        // Sandbox configs written before Phase 6 may still carry the old
        // Qdrant-era keys; `MemoryConfig` has no `deny_unknown_fields`, so
        // they must parse cleanly (and be ignored).
        let toml_str = r#"
enabled = true
backend = "disk"
qdrant_url = "http://localhost:6334"
vector_size = 1536
ttl_days = 7
"#;
        let cfg: MemoryConfig = toml::from_str(toml_str).unwrap();
        assert!(cfg.enabled);
    }

    #[test]
    fn validate_accepts_claude_code_engine() {
        let mut config = Config::default();
        let main = config.agents.get_mut("main").expect("main agent");
        main.engine = "claude_code".to_string();
        main.claude_code = Some(AgentClaudeCodeConfig {
            builtin_tools_profile: "editor_shell".to_string(),
        });
        assert!(config.validate().is_ok());
    }

    #[test]
    fn validate_rejects_bad_claude_profile() {
        let mut config = Config::default();
        let main = config.agents.get_mut("main").expect("main agent");
        main.engine = "claude_code".to_string();
        main.claude_code = Some(AgentClaudeCodeConfig {
            builtin_tools_profile: "dangerous".to_string(),
        });

        let err = config.validate().expect_err("expected validation error");
        assert!(err
            .to_string()
            .contains("builtin_tools_profile must be one of"));
    }

    #[test]
    fn existing_config_no_scopes_parses() {
        let toml_str = r#"
            runtime_profile = "auto"
            [hub]
            bind = "127.0.0.1"
            port = 7070
            auth_mode = "token"

            [agents.main]
            default = true
            engine = "openrouter"
            model = "anthropic/claude-sonnet-4.6"
        "#;
        let config: Config = toml::from_str(toml_str).expect("should parse");
        assert!(config.validate().is_ok());
        assert!(config.agents["main"].scopes.is_empty());
        assert!(config.default_scopes.is_empty());
    }

    #[test]
    fn agent_scopes_roundtrip() {
        let toml_str = r#"
            runtime_profile = "auto"

            [agents.main]
            default = true
            engine = "openrouter"
            model = "anthropic/claude-sonnet-4.6"

            [agents.main.scopes.write_file]
            fs_roots = ["./research", "./drafts"]

            [agents.main.scopes.http_request]
            net_hosts = ["api.linear.app", "*.anthropic.com"]
            env_reads = ["LINEAR_API_KEY"]
        "#;
        let config: Config = toml::from_str(toml_str).expect("should parse");
        let scopes = &config.agents["main"].scopes;
        assert_eq!(scopes["write_file"].fs_roots.len(), 2);
        assert_eq!(scopes["http_request"].net_hosts.len(), 2);
        assert_eq!(scopes["http_request"].env_reads.len(), 1);
    }

    #[test]
    fn default_scopes_roundtrip() {
        let toml_str = r#"
            runtime_profile = "auto"

            [agents.main]
            default = true
            engine = "openrouter"
            model = "anthropic/claude-sonnet-4.6"

            [default_scopes.read_file]
            fs_roots = ["./"]

            [default_scopes.run_command]
            shell_bins = ["git", "rg", "cargo"]
            fs_roots = ["./"]
        "#;
        let config: Config = toml::from_str(toml_str).expect("should parse");
        assert_eq!(config.default_scopes["read_file"].fs_roots.len(), 1);
        assert_eq!(config.default_scopes["run_command"].shell_bins.len(), 3);
    }

    #[test]
    fn resolve_scope_agent_overrides_default() {
        let toml_str = r#"
            runtime_profile = "auto"

            [default_scopes.write_file]
            fs_roots = ["./global"]

            [agents.main]
            default = true
            engine = "openrouter"
            model = "anthropic/claude-sonnet-4.6"

            [agents.main.scopes.write_file]
            fs_roots = ["./agent-specific"]
        "#;
        let config: Config = toml::from_str(toml_str).expect("should parse");
        let scope = config.resolve_scope("main", "write_file").unwrap();
        assert_eq!(scope.fs_roots.len(), 1);
        assert_eq!(scope.fs_roots[0].to_str().unwrap(), "./agent-specific");
    }

    #[test]
    fn resolve_scope_falls_back_to_default() {
        let toml_str = r#"
            runtime_profile = "auto"

            [default_scopes.read_file]
            fs_roots = ["./"]

            [agents.main]
            default = true
            engine = "openrouter"
            model = "anthropic/claude-sonnet-4.6"
        "#;
        let config: Config = toml::from_str(toml_str).expect("should parse");
        let scope = config.resolve_scope("main", "read_file").unwrap();
        assert_eq!(scope.fs_roots.len(), 1);
    }

    #[test]
    fn fold_default_scopes_materialises_fallback_per_agent() {
        let toml_str = r#"
            runtime_profile = "auto"

            [default_scopes.read_file]
            fs_roots = ["~/ws"]

            [default_scopes.write_file]
            fs_roots = ["./global"]

            [agents.main]
            default = true
            engine = "openrouter"
            model = "anthropic/claude-sonnet-4-6"

            [agents.main.scopes.write_file]
            fs_roots = ["./agent-specific"]
        "#;
        let mut config: Config = toml::from_str(toml_str).expect("should parse");
        config.fold_default_scopes();
        let scopes = &config.agents["main"].scopes;
        // Unconfigured tool inherits the default entry.
        assert_eq!(scopes["read_file"].fs_roots.len(), 1);
        // `~` is expanded so `ToolScope::check_fs` matches canonical paths.
        assert!(!scopes["read_file"].fs_roots[0].starts_with("~"));
        // Per-agent entry wins wholesale over the default.
        assert_eq!(scopes["write_file"].fs_roots.len(), 1);
        assert_eq!(
            scopes["write_file"].fs_roots[0].to_str().unwrap(),
            "./agent-specific"
        );
    }

    #[test]
    fn resolve_scope_none_when_absent() {
        let config = Config::default();
        assert!(config.resolve_scope("main", "write_file").is_none());
    }

    #[test]
    fn validate_passes_with_scopes() {
        let toml_str = r#"
            runtime_profile = "auto"

            [default_scopes.read_file]
            fs_roots = ["./"]

            [agents.main]
            default = true
            engine = "openrouter"
            model = "anthropic/claude-sonnet-4.6"

            [agents.main.scopes.write_file]
            fs_roots = ["./research"]
        "#;
        let config: Config = toml::from_str(toml_str).expect("should parse");
        assert!(config.validate().is_ok());
    }
}
