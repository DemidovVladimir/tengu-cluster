use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Top-level configuration.
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

fn default_bind() -> String { "127.0.0.1".to_string() }
fn default_port() -> u16 { 7070 }
fn default_auth_mode() -> String { "token".to_string() }

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

fn default_reload_mode() -> String { "hybrid".to_string() }
fn default_debounce_ms() -> u64 { 300 }

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

fn default_refiner_mode() -> String { "off".to_string() }

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

fn default_lens() -> String { "eco".to_string() }

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IdentityConfig {
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowConfig {
    #[serde(default = "default_scope")]
    pub scope: String,
    #[serde(default = "default_reset_mode")]
    pub reset_mode: String,
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout_minutes: u32,
}

impl Default for FlowConfig {
    fn default() -> Self {
        Self {
            scope: default_scope(),
            reset_mode: default_reset_mode(),
            idle_timeout_minutes: default_idle_timeout(),
        }
    }
}

fn default_scope() -> String { "per-sender".to_string() }
fn default_reset_mode() -> String { "idle".to_string() }
fn default_idle_timeout() -> u32 { 30 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitsConfig {
    #[serde(default = "default_max_tokens")]
    pub max_tokens_per_flow: u64,
    #[serde(default)]
    pub max_cost_per_flow: Option<f64>,
    #[serde(default)]
    pub warn_at_cost: Option<f64>,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_tokens_per_flow: default_max_tokens(),
            max_cost_per_flow: None,
            warn_at_cost: None,
        }
    }
}

fn default_max_tokens() -> u64 { 500_000 }

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

fn default_eco_max() -> u32 { 100 }
fn default_threshold() -> f32 { 0.7 }
fn default_budget() -> f32 { 0.5 }

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KitConfig {
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
}

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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SandboxConfig {
    #[serde(default)]
    pub mode: String,
}

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipeEntry {
    #[serde(default = "bool_true")]
    pub enabled: bool,
}

fn bool_true() -> bool { true }

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

fn default_access_policy() -> String { "approval".to_string() }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscordPipeConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebchatPipeConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_webchat_bind")]
    pub bind: String,
}

fn default_webchat_bind() -> String { "127.0.0.1:7071".to_string() }

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SkillsConfig {
    #[serde(default)]
    pub watch: bool,
    #[serde(default)]
    pub extra_dirs: Vec<String>,
}

impl Config {
    /// Load config from a TOML file, with env var substitution.
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let content = Self::substitute_env_vars(&content)?;
        let config: Config = toml::from_str(&content)?;
        Ok(config)
    }

    /// Load config or return defaults if file doesn't exist.
    pub fn load_or_default(path: &std::path::Path) -> Self {
        match Self::load(path) {
            Ok(config) => config,
            Err(_) => Self::default(),
        }
    }

    /// Substitute ${VAR_NAME} patterns with environment variable values.
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
        agents.insert("main".to_string(), AgentConfig {
            default: true,
            engine: "ollama".to_string(),
            model: "llama3.2".to_string(),
            workspace: None,
            default_lens: "eco".to_string(),
            identity: IdentityConfig { name: Some("Tengu".to_string()) },
            flow: FlowConfig::default(),
            limits: LimitsConfig::default(),
            lens: LensConfig::default(),
            kit: KitConfig::default(),
            store: StoreConfig::default(),
            allowed_engines: vec![],
            sandbox: SandboxConfig::default(),
        });

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
