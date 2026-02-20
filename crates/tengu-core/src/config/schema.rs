//! TOML-backed runtime configuration schema.
//!
//! Potential use case:
//! Parse `~/.tengu/config.toml` into strongly typed runtime settings with env substitution.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
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
    pub routing: Vec<RoutingBinding>,

    #[serde(default)]
    pub pipes: PipesConfig,

    #[serde(default)]
    pub skills: SkillsConfig,
    #[serde(default)]
    pub capability_governance: CapabilityGovernanceConfig,
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
    #[serde(default)]
    pub kit: KitConfig,
    #[serde(default)]
    pub store: StoreConfig,
    #[serde(default)]
    pub allowed_engines: Vec<String>,
    #[serde(default)]
    pub sandbox: SandboxConfig,
    #[serde(default)]
    pub skill_policy: SkillPolicyConfig,
}

fn default_lens() -> String {
    "eco".to_string()
}

/// Optional identity metadata used for prompts/UI.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IdentityConfig {
    #[serde(default)]
    pub name: Option<String>,
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
    /// Optional hard cap for recent user turns kept in active runtime history.
    ///
    /// If omitted, runtime derives a scope-aware default.
    #[serde(default)]
    pub max_history_turns: Option<u32>,
    /// Optional trigger ratio for compaction (`0.0..=1.0`) against `max_tokens_per_flow`.
    ///
    /// If omitted, runtime derives a scope-aware default.
    #[serde(default)]
    pub compaction_threshold_ratio: Option<f32>,
    /// Optional count of recent user turns to keep verbatim during compaction.
    ///
    /// If omitted, runtime derives a scope-aware default.
    #[serde(default)]
    pub compaction_keep_turns: Option<u32>,
    /// Optional max token budget used for generated compaction summaries.
    ///
    /// If omitted, runtime derives a value from effective model input budget.
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
    /// Optional provider context-window override for this agent model.
    ///
    /// When omitted, each backend uses its model-aware fallback map.
    #[serde(default)]
    pub context_window_override: Option<u32>,
    /// Optional per-turn output token cap passed to provider APIs.
    ///
    /// When omitted, runtime uses provider-specific fallback defaults.
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
    500_000
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

/// Tool allow/deny lists for agent kit policy.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KitConfig {
    /// Optional allow-list. Empty means "all tools allowed unless denied".
    #[serde(default)]
    pub allow: Vec<String>,
    /// Explicit deny-list. Deny always wins.
    #[serde(default)]
    pub deny: Vec<String>,
    /// Tools that require explicit approval before execution.
    ///
    /// This is additive to per-tool metadata defaults.
    #[serde(default)]
    pub approval_required: Vec<String>,
    /// Tools explicitly approved for execution when approval is required.
    #[serde(default)]
    pub approved: Vec<String>,
}

/// Workspace knowledge ingest configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreConfig {
    #[serde(default = "default_store_files")]
    pub files: Vec<String>,
    #[serde(default)]
    pub extra_paths: Vec<String>,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            files: default_store_files(),
            extra_paths: vec![],
        }
    }
}

fn default_store_files() -> Vec<String> {
    vec![
        "CONTEXT.md".to_string(),
        "IDENTITY.md".to_string(),
        "PROFILE.md".to_string(),
        "NOTES.md".to_string(),
        "notes/*.md".to_string(),
    ]
}

/// Sandbox runtime mode configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SandboxConfig {
    #[serde(default)]
    pub mode: String,
}

/// Sender-to-agent routing binding rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingBinding {
    pub agent: String,
    pub pipe: String,
    #[serde(default)]
    pub peer: Option<String>,
    #[serde(default)]
    pub group_id: Option<String>,
    #[serde(default)]
    pub account_id: Option<String>,
}

/// Pipe configurations keyed by channel type.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PipesConfig {
    #[serde(default)]
    pub cli: Option<PipeEntry>,
    #[serde(default)]
    pub telegram: Option<TelegramPipeConfig>,
    #[serde(default)]
    pub discord: Option<DiscordPipeConfig>,
    #[serde(default)]
    pub webchat: Option<WebchatPipeConfig>,
}

/// Generic on/off pipe entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipeEntry {
    #[serde(default = "bool_true")]
    pub enabled: bool,
}

fn bool_true() -> bool {
    true
}

/// Telegram pipe configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramPipeConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default = "default_access_policy")]
    pub access_policy: String,
    #[serde(default)]
    pub allow_from: Vec<String>,
}

fn default_access_policy() -> String {
    "approval".to_string()
}

/// Discord pipe configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscordPipeConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub token: Option<String>,
}

/// WebChat pipe configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebchatPipeConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_webchat_bind")]
    pub bind: String,
}

fn default_webchat_bind() -> String {
    "127.0.0.1:7071".to_string()
}

/// Skills discovery/watch configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SkillsConfig {
    #[serde(default)]
    pub watch: bool,
    #[serde(default)]
    pub extra_dirs: Vec<String>,
}

/// Global capability-governance mode for runtime policy decisions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityGovernanceConfig {
    /// Governance mode:
    /// - `user`: only static user-defined per-agent capability sets are allowed.
    /// - `delegated`: one orchestrator agent may request bounded capability usage.
    #[serde(default = "default_capability_governance_mode")]
    pub mode: String,
    /// Agent id allowed to request delegated capability usage in `delegated` mode.
    #[serde(default)]
    pub delegated_orchestrator_agent: Option<String>,
}

impl Default for CapabilityGovernanceConfig {
    fn default() -> Self {
        Self {
            mode: default_capability_governance_mode(),
            delegated_orchestrator_agent: None,
        }
    }
}

fn default_capability_governance_mode() -> String {
    "user".to_string()
}

/// Skill allow/deny policy for one agent.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SkillPolicyConfig {
    /// Optional allow-list. Empty means "all skills allowed unless denied".
    #[serde(default)]
    pub allow: Vec<String>,
    /// Explicit deny-list. Deny always wins.
    #[serde(default)]
    pub deny: Vec<String>,
}

/// Aggregates config validation issues with reusable guard helpers.
#[derive(Debug, Default)]
struct ValidationErrors {
    issues: Vec<String>,
}

impl ValidationErrors {
    /// Append one validation issue.
    fn push(&mut self, message: impl Into<String>) {
        self.issues.push(message.into());
    }

    /// Append `message` when `condition` is false.
    fn require(&mut self, condition: bool, message: impl Into<String>) {
        if !condition {
            self.push(message);
        }
    }

    /// Validate non-empty text field.
    fn require_nonempty(&mut self, path: &str, value: &str) {
        self.require(!value.trim().is_empty(), format!("{path} cannot be empty"));
    }

    /// Validate enum-like value against static allow-list.
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

    /// Validate optional positive integer (> 0) when set.
    fn require_positive_opt_u32(&mut self, path: &str, value: Option<u32>) {
        if matches!(value, Some(0)) {
            self.push(format!("{path} must be greater than 0 when set"));
        }
    }

    /// Validate optional positive float (> 0) when set.
    fn require_positive_opt_f64(&mut self, path: &str, value: Option<f64>) {
        if let Some(v) = value {
            if v <= 0.0 {
                self.push(format!("{path} must be greater than 0 when set"));
            }
        }
    }

    /// Consume into raw issue vector.
    fn into_vec(self) -> Vec<String> {
        self.issues
    }
}

/// Validate list items are non-empty after trim.
fn validate_nonempty_entries(path: &str, entries: &[String], errors: &mut ValidationErrors) {
    entries.iter().enumerate().for_each(|(idx, value)| {
        if value.trim().is_empty() {
            errors.push(format!("{path}[{idx}] cannot be empty"));
        }
    });
}

/// Validate list items are non-empty and unique after trim, returning normalized set.
fn validate_nonempty_unique_entries(
    path: &str,
    entries: &[String],
    errors: &mut ValidationErrors,
) -> HashSet<String> {
    let mut seen = HashSet::<String>::new();
    entries.iter().enumerate().for_each(|(idx, value)| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            errors.push(format!("{path}[{idx}] cannot be empty"));
            return;
        }
        if !seen.insert(trimmed.to_string()) {
            errors.push(format!("{path} contains duplicate '{}'", trimmed));
        }
    });
    seen
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
    ///
    /// Validation accumulates all discovered issues and returns one actionable
    /// error payload so users can fix config in a single edit cycle.
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

    /// Collect all validation errors for this config instance.
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
        errors.require_one_of(
            "capability_governance.mode",
            &self.capability_governance.mode,
            &["user", "delegated"],
        );
        if self.capability_governance.mode.trim() == "delegated" {
            match self
                .capability_governance
                .delegated_orchestrator_agent
                .as_deref()
                .map(str::trim)
            {
                Some("") | None => errors.push(
                    "capability_governance.mode=delegated requires capability_governance.delegated_orchestrator_agent"
                        .to_string(),
                ),
                Some(agent_id) => {
                    if !self.agents.contains_key(agent_id) {
                        errors.push(format!(
                            "capability_governance.delegated_orchestrator_agent '{}' does not match configured agents",
                            agent_id
                        ));
                    }
                }
            }
        }

        let default_count = self.agents.values().filter(|agent| agent.default).count();
        if default_count > 1 {
            errors.push("only one agent can have default=true");
        }

        self.agents.iter().for_each(|(agent_id, agent)| {
            Self::validate_agent(agent_id, agent, &mut errors);
        });

        errors.require(
            !self.routing.is_empty(),
            "routing must contain at least one binding",
        );
        self.routing.iter().enumerate().for_each(|(idx, binding)| {
            let route_id = idx + 1;
            if binding.agent.trim().is_empty() {
                errors.push(format!("routing[{}].agent cannot be empty", route_id));
            } else if !self.agents.contains_key(&binding.agent) {
                errors.push(format!(
                    "routing[{}] references unknown agent '{}'",
                    route_id, binding.agent
                ));
            }

            let pipe_id = binding.pipe.trim();
            if pipe_id.is_empty() {
                errors.push(format!("routing[{}].pipe cannot be empty", route_id));
                return;
            }
            match self.pipe_enabled(pipe_id) {
                Some(true) => {}
                Some(false) => errors.push(format!(
                    "routing[{}] references disabled or missing pipe '{}'",
                    route_id, pipe_id
                )),
                None => errors.push(format!(
                    "routing[{}] references unknown pipe '{}'",
                    route_id, pipe_id
                )),
            }
        });

        if let Some(telegram) = &self.pipes.telegram {
            errors.require_one_of(
                "pipes.telegram.access_policy",
                &telegram.access_policy,
                &["approval", "allowlist", "open", "disabled"],
            );

            if telegram.enabled && !has_nonempty_opt(&telegram.token) {
                errors
                    .push("pipes.telegram.enabled=true requires pipes.telegram.token".to_string());
            }
            if telegram.access_policy.trim() == "allowlist" && telegram.allow_from.is_empty() {
                errors.push(
                    "pipes.telegram.access_policy=allowlist requires non-empty allow_from"
                        .to_string(),
                );
            }
        }

        if let Some(discord) = &self.pipes.discord {
            if discord.enabled && !has_nonempty_opt(&discord.token) {
                errors.push("pipes.discord.enabled=true requires pipes.discord.token".to_string());
            }
        }

        if let Some(webchat) = &self.pipes.webchat {
            if webchat.enabled && webchat.bind.trim().is_empty() {
                errors.push(
                    "pipes.webchat.enabled=true requires non-empty pipes.webchat.bind".to_string(),
                );
            }
        }

        validate_nonempty_entries("skills.extra_dirs", &self.skills.extra_dirs, &mut errors);

        errors.into_vec()
    }

    /// Return whether a named pipe exists and is enabled in config.
    fn pipe_enabled(&self, pipe_id: &str) -> Option<bool> {
        match pipe_id {
            "cli" => Some(
                self.pipes
                    .cli
                    .as_ref()
                    .map(|entry| entry.enabled)
                    .unwrap_or(false),
            ),
            "telegram" => Some(
                self.pipes
                    .telegram
                    .as_ref()
                    .map(|entry| entry.enabled)
                    .unwrap_or(false),
            ),
            "discord" => Some(
                self.pipes
                    .discord
                    .as_ref()
                    .map(|entry| entry.enabled)
                    .unwrap_or(false),
            ),
            "webchat" => Some(
                self.pipes
                    .webchat
                    .as_ref()
                    .map(|entry| entry.enabled)
                    .unwrap_or(false),
            ),
            _ => None,
        }
    }

    /// Validate one agent section and append any issues.
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

        let allow_path = format!("agents.{agent_id}.kit.allow");
        let deny_path = format!("agents.{agent_id}.kit.deny");
        let approval_required_path = format!("agents.{agent_id}.kit.approval_required");
        let approved_path = format!("agents.{agent_id}.kit.approved");
        let seen_allow = validate_nonempty_unique_entries(&allow_path, &agent.kit.allow, errors);
        let seen_deny = validate_nonempty_unique_entries(&deny_path, &agent.kit.deny, errors);
        let seen_approval_required = validate_nonempty_unique_entries(
            &approval_required_path,
            &agent.kit.approval_required,
            errors,
        );
        let seen_approved =
            validate_nonempty_unique_entries(&approved_path, &agent.kit.approved, errors);
        seen_allow.intersection(&seen_deny).for_each(|tool_name| {
            errors.push(format!(
                "agents.{}.kit.allow and kit.deny both contain '{}'",
                agent_id, tool_name
            ))
        });
        seen_deny
            .intersection(&seen_approved)
            .for_each(|tool_name| {
                errors.push(format!(
                    "agents.{}.kit.deny and kit.approved both contain '{}'",
                    agent_id, tool_name
                ))
            });
        if !seen_allow.is_empty() {
            seen_approved.difference(&seen_allow).for_each(|tool_name| {
                errors.push(format!(
                    "agents.{}.kit.approved contains '{}' which is not present in kit.allow",
                    agent_id, tool_name
                ))
            });
            seen_approval_required
                .difference(&seen_allow)
                .for_each(|tool_name| {
                    errors.push(format!(
                        "agents.{}.kit.approval_required contains '{}' which is not present in kit.allow",
                        agent_id, tool_name
                    ))
                });
        }

        let skill_allow_path = format!("agents.{agent_id}.skill_policy.allow");
        let skill_deny_path = format!("agents.{agent_id}.skill_policy.deny");
        let seen_skill_allow =
            validate_nonempty_unique_entries(&skill_allow_path, &agent.skill_policy.allow, errors);
        let seen_skill_deny =
            validate_nonempty_unique_entries(&skill_deny_path, &agent.skill_policy.deny, errors);
        seen_skill_allow
            .intersection(&seen_skill_deny)
            .for_each(|skill_name| {
                errors.push(format!(
                    "agents.{}.skill_policy.allow and skill_policy.deny both contain '{}'",
                    agent_id, skill_name
                ))
            });

        let mut seen_allowed_engines = HashSet::<String>::new();
        let selected_engine = format!("{}/{}", agent.engine.trim(), agent.model.trim());
        agent
            .allowed_engines
            .iter()
            .enumerate()
            .for_each(|(idx, engine_id)| {
                let trimmed = engine_id.trim();
                if trimmed.is_empty() {
                    errors.push(format!(
                        "agents.{}.allowed_engines[{}] cannot be empty",
                        agent_id, idx
                    ));
                    return;
                }
                if !seen_allowed_engines.insert(trimmed.to_string()) {
                    errors.push(format!(
                        "agents.{}.allowed_engines contains duplicate '{}'",
                        agent_id, trimmed
                    ));
                }
                let mut parts = trimmed.splitn(2, '/');
                let provider = parts.next().unwrap_or_default().trim();
                let model = parts.next().unwrap_or_default().trim();
                if provider.is_empty() || model.is_empty() {
                    errors.push(format!(
                        "agents.{}.allowed_engines[{}] must use 'provider/model' format (got '{}')",
                        agent_id, idx, trimmed
                    ));
                }
            });

        if !agent.allowed_engines.is_empty() && !seen_allowed_engines.contains(&selected_engine) {
            errors.push(format!(
                "agents.{} selected engine/model '{}' is not included in allowed_engines",
                agent_id, selected_engine
            ));
        }

        if !matches!(
            agent.sandbox.mode.trim(),
            "" | "off" | "workspace" | "docker"
        ) {
            errors.push(format!(
                "agents.{}.sandbox.mode must be one of off|workspace|docker (or empty)",
                agent_id
            ));
        }

        validate_nonempty_entries(
            &format!("agents.{agent_id}.store.files"),
            &agent.store.files,
            errors,
        );
        validate_nonempty_entries(
            &format!("agents.{agent_id}.store.extra_paths"),
            &agent.store.extra_paths,
            errors,
        );
    }

    /// Load config or fallback to default when file is missing/invalid.
    pub fn load_or_default(path: &std::path::Path) -> Self {
        Self::load(path).unwrap_or_default()
    }

    /// Replace `${VAR}` placeholders with environment values when present.
    ///
    /// Missing environment variables are intentionally left unchanged so optional
    /// placeholders can remain in config templates.
    fn substitute_env_vars(content: &str) -> anyhow::Result<String> {
        let mut result = content.to_string();
        let re = regex_lite::Regex::new(r"\$\{([A-Z_][A-Z0-9_]*)\}").unwrap();

        for cap in re.captures_iter(content) {
            let full_match = cap.get(0).unwrap().as_str();
            let var_name = &cap[1];
            if let Ok(value) = std::env::var(var_name) {
                result = result.replace(full_match, &value);
            }
            // Missing vars are left as-is (they may be optional)
        }

        Ok(result)
    }
}

fn has_nonempty_opt(value: &Option<String>) -> bool {
    value
        .as_deref()
        .map(str::trim)
        .is_some_and(|v| !v.is_empty())
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
                },
                flow: FlowConfig::default(),
                limits: LimitsConfig::default(),
                lens: LensConfig::default(),
                kit: KitConfig::default(),
                store: StoreConfig::default(),
                allowed_engines: vec![],
                sandbox: SandboxConfig::default(),
                skill_policy: SkillPolicyConfig::default(),
            },
        );

        Self {
            runtime_profile: "auto".to_string(),
            hub: HubConfig::default(),
            refiner: RefinerConfig::default(),
            agents,
            routing: vec![RoutingBinding {
                agent: "main".to_string(),
                pipe: "cli".to_string(),
                peer: None,
                group_id: None,
                account_id: None,
            }],
            pipes: PipesConfig {
                cli: Some(PipeEntry { enabled: true }),
                telegram: None,
                discord: None,
                webchat: None,
            },
            skills: SkillsConfig::default(),
            capability_governance: CapabilityGovernanceConfig::default(),
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

    #[test]
    fn validate_rejects_tool_allow_deny_overlap() {
        let mut config = Config::default();
        let main = config.agents.get_mut("main").expect("main agent");
        main.kit.allow = vec!["shell".to_string()];
        main.kit.deny = vec!["shell".to_string()];

        let err = config.validate().expect_err("expected validation error");
        assert!(err.to_string().contains("kit.allow and kit.deny"));
    }

    #[test]
    fn validate_rejects_tool_deny_approved_overlap() {
        let mut config = Config::default();
        let main = config.agents.get_mut("main").expect("main agent");
        main.kit.allow = vec!["shell".to_string()];
        main.kit.deny = vec!["shell".to_string()];
        main.kit.approved = vec!["shell".to_string()];

        let err = config.validate().expect_err("expected validation error");
        assert!(err.to_string().contains("kit.deny and kit.approved"));
    }

    #[test]
    fn validate_rejects_approved_tool_outside_allowlist() {
        let mut config = Config::default();
        let main = config.agents.get_mut("main").expect("main agent");
        main.kit.allow = vec!["read_file".to_string()];
        main.kit.approved = vec!["shell".to_string()];

        let err = config.validate().expect_err("expected validation error");
        assert!(err
            .to_string()
            .contains("kit.approved contains 'shell' which is not present in kit.allow"));
    }

    #[test]
    fn validate_rejects_selected_engine_not_in_allowed_list() {
        let mut config = Config::default();
        let main = config.agents.get_mut("main").expect("main agent");
        main.allowed_engines = vec!["openai/gpt-4o-mini".to_string()];

        let err = config.validate().expect_err("expected validation error");
        assert!(err.to_string().contains("not included in allowed_engines"));
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
    fn validate_rejects_routing_unknown_agent() {
        let mut config = Config::default();
        config.routing[0].agent = "unknown".to_string();

        let err = config.validate().expect_err("expected validation error");
        assert!(err.to_string().contains("references unknown agent"));
    }

    #[test]
    fn validate_rejects_enabled_telegram_without_token() {
        let mut config = Config::default();
        config.pipes.telegram = Some(TelegramPipeConfig {
            enabled: true,
            token: None,
            access_policy: "approval".to_string(),
            allow_from: Vec::new(),
        });

        let err = config.validate().expect_err("expected validation error");
        assert!(err.to_string().contains("pipes.telegram.enabled=true"));
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
    fn validate_rejects_skill_allow_deny_overlap() {
        let mut config = Config::default();
        let main = config.agents.get_mut("main").expect("main agent");
        main.skill_policy.allow = vec!["analysis".to_string()];
        main.skill_policy.deny = vec!["analysis".to_string()];

        let err = config.validate().expect_err("expected validation error");
        assert!(err
            .to_string()
            .contains("skill_policy.allow and skill_policy.deny"));
    }

    #[test]
    fn validate_rejects_delegated_mode_without_orchestrator() {
        let mut config = Config::default();
        config.capability_governance.mode = "delegated".to_string();
        config.capability_governance.delegated_orchestrator_agent = None;

        let err = config.validate().expect_err("expected validation error");
        assert!(err.to_string().contains(
            "mode=delegated requires capability_governance.delegated_orchestrator_agent"
        ));
    }
}
