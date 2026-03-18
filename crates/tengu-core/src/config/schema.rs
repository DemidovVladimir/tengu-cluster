//! TOML-backed runtime configuration schema.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Root configuration object loaded from `config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_profile")]
    pub runtime_profile: String,

    #[serde(default)]
    pub hub: HubConfig,

    #[serde(default)]
    pub refiner: RefinerConfig,

    #[serde(default)]
    pub agents: HashMap<String, AgentConfig>,

    #[serde(default)]
    pub orchestrator: Option<OrchestratorConfig>,

    #[serde(default)]
    pub memory: MemoryConfig,

    #[serde(default)]
    pub telegram: TelegramConfig,

    #[serde(default)]
    pub scaffold: Option<ScaffoldConfig>,
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

/// Prompt refiner configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefinerConfig {
    #[serde(default = "default_refiner_mode")]
    pub mode: String,
    pub model: Option<String>,
    pub url: Option<String>,
}

impl Default for RefinerConfig {
    fn default() -> Self {
        Self {
            mode: default_refiner_mode(),
            model: None,
            url: None,
        }
    }
}

fn default_refiner_mode() -> String {
    "off".to_string()
}

/// Per-agent runtime configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
    /// Explicit runtime capabilities enforced by the runtime.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Optional workflow/document packages loaded into the prompt and tool registry.
    #[serde(default)]
    pub skill_packages: Vec<String>,
    #[serde(default)]
    pub prompt_budget: PromptBudgetConfig,
    /// Roles this agent depends on — tasks for this agent must follow tasks from these roles.
    /// Used by the planner to enforce correct dependency ordering.
    #[serde(default)]
    pub requires: Vec<String>,
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
    #[serde(default)]
    pub context_window_override: Option<u32>,
    #[serde(default)]
    pub max_output_tokens_per_turn: Option<u32>,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_tokens_per_flow: default_max_tokens(),
            max_cost_per_flow: None,
            warn_at_cost: None,
            context_window_override: None,
            max_output_tokens_per_turn: None,
        }
    }
}

fn default_max_tokens() -> u64 {
    100_000
}

/// Orchestrator configuration for fleet management.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchestratorConfig {
    #[serde(default = "default_orchestrator_enabled")]
    pub enabled: bool,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    /// Optional dedicated engine for the planner/classifier (e.g. "openrouter").
    /// When set together with `planner_model`, a separate engine is created for
    /// plan generation and request classification instead of reusing the default
    /// agent's engine. This lets you use a cheaper/faster model for planning.
    pub planner_engine: Option<String>,
    /// Optional dedicated model for the planner/classifier (e.g. "google/gemini-2.5-flash").
    pub planner_model: Option<String>,
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        Self {
            enabled: default_orchestrator_enabled(),
            max_retries: default_max_retries(),
            planner_engine: None,
            planner_model: None,
        }
    }
}

fn default_orchestrator_enabled() -> bool {
    false
}
fn default_max_retries() -> u32 {
    3
}

/// Telegram bot adapter configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TelegramConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Telegram user IDs allowed to interact with the bot.
    #[serde(default)]
    pub allowed_users: Vec<String>,
    /// Require explicit approve/deny for tools whose policies demand approval.
    /// Defaults to false so Telegram flows can run end-to-end without extra taps.
    #[serde(default)]
    pub tool_approvals: bool,
    /// When non-empty, only these tool names require user approval.
    /// All other tools are auto-approved even if tool_approvals is true.
    /// Example: `approve_only = ["sign_and_send_transaction"]`
    #[serde(default)]
    pub approve_only: Vec<String>,
}

/// Workspace scaffold — auto-creates directories and seed files on startup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScaffoldConfig {
    /// Root workspace directory to create (supports ~ expansion).
    pub root: String,
    /// Subdirectories to create under root at startup.
    #[serde(default)]
    pub directories: Vec<String>,
    /// Seed files to create at startup (only if they don't already exist).
    #[serde(default)]
    pub files: Vec<ScaffoldFile>,
    /// Per-project template applied by `/project <name>`.
    /// Directories and files are created inside `{root}/{project_name}/`.
    #[serde(default)]
    pub project: Option<ProjectScaffold>,
}

/// Template for per-project scaffolding (applied by `/project <name>`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectScaffold {
    /// Subdirectories to create inside the project folder.
    #[serde(default)]
    pub directories: Vec<String>,
    /// Seed files to create inside the project folder.
    #[serde(default)]
    pub files: Vec<ScaffoldFile>,
}

/// A file to seed into the workspace during scaffold.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScaffoldFile {
    /// Path relative to scaffold root.
    pub path: String,
    /// File content.
    pub content: String,
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
    #[serde(default = "default_embedding_provider")]
    pub embedding_provider: String,
    /// Storage backend: "disk" (default) or "qdrant".
    #[serde(default = "default_memory_backend")]
    pub backend: String,
    /// Qdrant gRPC endpoint URL.
    #[serde(default = "default_qdrant_url")]
    pub qdrant_url: String,
    /// Optional API key for Qdrant Cloud.
    #[serde(default)]
    pub qdrant_api_key: Option<String>,
    /// Qdrant collection name.
    #[serde(default = "default_qdrant_collection")]
    pub qdrant_collection: String,
    /// Embedding vector dimensionality (must match embedding model output).
    #[serde(default = "default_vector_size")]
    pub vector_size: u64,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            embedding_model: default_embedding_model(),
            max_recall_entries: default_max_recall_entries(),
            max_recall_tokens: default_max_recall_tokens(),
            store_path: default_store_path(),
            embedding_provider: default_embedding_provider(),
            backend: default_memory_backend(),
            qdrant_url: default_qdrant_url(),
            qdrant_api_key: None,
            qdrant_collection: default_qdrant_collection(),
            vector_size: default_vector_size(),
        }
    }
}

fn default_embedding_model() -> String {
    "text-embedding-3-small".to_string()
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
fn default_embedding_provider() -> String {
    "openrouter".to_string()
}

fn default_memory_backend() -> String {
    "disk".to_string()
}

fn default_qdrant_url() -> String {
    "http://localhost:6334".to_string()
}

fn default_qdrant_collection() -> String {
    "tengu-memory".to_string()
}

fn default_vector_size() -> u64 {
    1536
}

/// System prompt token budget configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptBudgetConfig {
    /// Max tokens for individual workspace files (IDENTITY.md, PROFILE.md, etc.) and instructions.
    #[serde(default = "default_max_file_tokens")]
    pub max_file_tokens: usize,
    /// Max tokens for each skill context fragment (API docs from frontmatter skills).
    #[serde(default = "default_max_skill_context_tokens")]
    pub max_skill_context_tokens: usize,
    /// Max total tokens for the entire assembled system prompt.
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
    4000
}
fn default_max_total_tokens() -> usize {
    8000
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

fn has_nonempty_opt(value: &Option<String>) -> bool {
    value
        .as_deref()
        .map(str::trim)
        .is_some_and(|v| !v.is_empty())
}

impl Config {
    /// Load config from path and apply `${ENV_VAR}` substitution.
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let content = Self::substitute_env_vars(&content)?;
        let config: Config = toml::from_str(&content)?;
        config.validate()?;
        Ok(config)
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

        match self.refiner.mode.trim() {
            "off" | "rules" => {}
            "local" => {
                if !has_nonempty_opt(&self.refiner.model) {
                    errors.push("refiner.mode=local requires refiner.model".to_string());
                }
            }
            "remote" => {
                if !has_nonempty_opt(&self.refiner.url) {
                    errors.push("refiner.mode=remote requires refiner.url".to_string());
                }
            }
            other => errors.push(format!(
                "refiner.mode must be one of off|rules|local|remote (got '{}')",
                other
            )),
        }

        errors.require(
            !self.agents.is_empty(),
            "at least one agent must be configured",
        );

        let default_count = self.agents.values().filter(|agent| agent.default).count();
        if default_count > 1 {
            errors.push("only one agent can have default=true");
        }

        self.agents.iter().for_each(|(agent_id, agent)| {
            Self::validate_agent(agent_id, agent, &mut errors);
        });

        errors.into_vec()
    }

    fn validate_agent(agent_id: &str, agent: &AgentConfig, errors: &mut ValidationErrors) {
        errors.require(!agent_id.trim().is_empty(), "agent id cannot be empty");
        errors.require_nonempty(&format!("agents.{agent_id}.engine"), &agent.engine);
        errors.require_nonempty(&format!("agents.{agent_id}.model"), &agent.model);
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
        if let (Some(context), Some(output)) = (
            agent.limits.context_window_override,
            agent.limits.max_output_tokens_per_turn,
        ) {
            if output > context {
                errors.push(format!(
                    "agents.{}.limits.max_output_tokens_per_turn cannot exceed context_window_override",
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

        for capability in &agent.capabilities {
            if let Err(e) = validate_capability_id(capability) {
                errors.push(format!(
                    "agents.{agent_id}.capabilities entry '{}' is invalid: {}",
                    capability, e
                ));
            }
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
    }

    pub fn load_or_default(path: &std::path::Path) -> Self {
        Self::load(path).unwrap_or_default()
    }

    fn substitute_env_vars(content: &str) -> anyhow::Result<String> {
        let mut result = content.to_string();
        let re = regex_lite::Regex::new(r"\$\{([A-Z_][A-Z0-9_]*)\}").unwrap();

        for cap in re.captures_iter(content) {
            let full_match = cap.get(0).unwrap().as_str();
            let var_name = &cap[1];
            if let Ok(value) = std::env::var(var_name) {
                result = result.replace(full_match, &value);
            }
        }

        Ok(result)
    }
}

impl Default for Config {
    fn default() -> Self {
        let mut agents = HashMap::new();
        agents.insert(
            "main".to_string(),
            AgentConfig {
                default: true,
                engine: "ollama".to_string(),
                model: "llama3.2".to_string(),
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
                capabilities: vec![],
                skill_packages: vec![],
                prompt_budget: PromptBudgetConfig::default(),
                requires: vec![],
            },
        );

        Self {
            runtime_profile: "auto".to_string(),
            hub: HubConfig::default(),
            refiner: RefinerConfig::default(),
            agents,
            orchestrator: None,
            memory: MemoryConfig::default(),
            telegram: TelegramConfig::default(),
            scaffold: None,
        }
    }
}

fn validate_capability_id(value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err("cannot be empty".to_string());
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
    {
        return Err("must use lowercase letters, digits, dot, underscore, or hyphen".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_accepts_default_config() {
        let config = Config::default();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn validate_rejects_remote_refiner_without_url() {
        let mut config = Config::default();
        config.refiner.mode = "remote".to_string();
        config.refiner.url = None;

        let err = config.validate().expect_err("expected validation error");
        assert!(err
            .to_string()
            .contains("refiner.mode=remote requires refiner.url"));
    }

    #[test]
    fn validate_rejects_output_cap_above_context_override() {
        let mut config = Config::default();
        let main = config.agents.get_mut("main").expect("main agent");
        main.limits.context_window_override = Some(4_096);
        main.limits.max_output_tokens_per_turn = Some(8_192);

        let err = config.validate().expect_err("expected validation error");
        assert!(err
            .to_string()
            .contains("cannot exceed context_window_override"));
    }

    #[test]
    fn validate_rejects_invalid_capability_id() {
        let mut config = Config::default();
        let main = config.agents.get_mut("main").expect("main agent");
        main.capabilities = vec!["Bad Capability".to_string()];

        let err = config.validate().expect_err("expected validation error");
        assert!(err.to_string().contains("capabilities entry"));
    }

    #[test]
    fn parse_rejects_legacy_agent_fields() {
        let raw = r#"
[agents.main]
default = true
engine = "ollama"
model = "llama3.2"
skills = ["search"]
allowed_tools = ["read_file"]
"#;

        let err = toml::from_str::<Config>(raw).expect_err("legacy agent fields should fail");
        assert!(err.to_string().contains("unknown field"));
    }
}
