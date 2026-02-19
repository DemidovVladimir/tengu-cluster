//! TOML-backed runtime configuration schema.
//!
//! Potential use case:
//! Parse `~/.tengu/config.toml` into strongly typed runtime settings with env substitution.

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
    pub routing: Vec<RoutingBinding>,

    #[serde(default)]
    pub pipes: PipesConfig,

    #[serde(default)]
    pub skills: SkillsConfig,
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
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
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

impl Config {
    /// Load config from path and apply `${ENV_VAR}` substitution.
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let content = Self::substitute_env_vars(&content)?;
        let config: Config = toml::from_str(&content)?;
        Ok(config)
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
        }
    }
}
