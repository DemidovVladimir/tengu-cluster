//! Config layer — the TOML schema (`sandboxes/<name>/config.toml`), its
//! validation, and path resolution. Imports only `domain` (see
//! `tests/layering_lint.rs`).
//!
//! | File | Holds |
//! |---|---|
//! | this file | `Config` (the root of `config.toml`) + its built-in default |
//! | `agent.rs` | `[agents.<a>]` (`AgentConfig`, limits, flow, engine blocks), `[orchestrator]`, `[[mcp_servers]]`, `[claude_code]` |
//! | `channels.rs` | `[telegram]`, `[webhooks]`, `[scaffold]` |
//! | `memory.rs` | `[memory]`, `[prompt_budget]` |
//! | `load.rs` | `Config::load` (`${VAR}` substitution, fold, validate), validation errors / warnings, scope lookup |
//! | the other files | one section each |

pub(crate) mod a2a;
mod agent;
pub(crate) mod backtest;
pub(crate) mod builder;
mod channels;
pub(crate) mod decision_loop;
pub(crate) mod egress;
pub(crate) mod execution_map;
pub(crate) mod feeds;
pub(crate) mod hardening;
pub(crate) mod keys;
pub(crate) mod lineage;
mod load;
mod memory;
pub(crate) mod paths;
pub(crate) mod rate_limits;
pub(crate) mod readers;
pub(crate) mod recorder;
pub(crate) mod risk;
pub(crate) mod runtime;
pub(crate) mod sections;
pub(crate) mod skill_lifecycle;
pub(crate) mod soe;
pub(crate) mod solana;
pub(crate) mod sources;
pub(crate) mod strategy_ranking;
pub(crate) mod studio;
pub(crate) mod xmarket;

pub(crate) use agent::*;
pub(crate) use channels::*;
pub(crate) use memory::*;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

use crate::domain::scope::ToolScope;
use egress::EgressConfig;

// ---------------------------------------------------------------------------
// Configuration schema
// ---------------------------------------------------------------------------

/// Root configuration object loaded from `config.toml`. Unknown top-level
/// keys are a parse error (tracker convention 6: a misspelled `[risk]` must
/// not load as "no limits"); tests in `config/risk.rs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
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

    /// `[decision_loops.<name>]` — System One (Jev) control loops over
    /// existing tools; triggered by webhook endpoints with `loop = "<name>"`
    /// or `tengu decide`. See `config/decision_loop.rs`.
    #[serde(default)]
    pub decision_loops: HashMap<String, decision_loop::DecisionLoopConfig>,

    #[serde(default)]
    pub scaffold: Option<ScaffoldConfig>,

    #[serde(default)]
    pub claude_code: Option<ClaudeCodeConfig>,

    /// Fallback scopes applied when an agent has no per-tool scope entry.
    /// Per-agent scopes override default_scopes wholesale (not field-merged).
    #[serde(default)]
    pub default_scopes: HashMap<String, ToolScope>,

    /// Network egress policy for LLM-initiated traffic: `network = "tor"`
    /// (default — everything through the Tor proxy) or `"open"`,
    /// sandbox-wide host ceiling, shell isolation, audit log. Installed
    /// process-wide by `egress::install`; see `adapters/egress.rs`.
    #[serde(default)]
    pub egress: EgressConfig,

    /// `[keys]` — sealed-secret proxy: provider keys live only inside blobs
    /// the `tengu-seal` Cloudflare Worker opens (`config/keys.rs`). Absent =
    /// keys come from the env / vault as before.
    #[serde(default)]
    pub keys: keys::KeysConfig,

    /// Inbound MCP client connections — external MCP servers this install
    /// connects to. At boot the MCP plugin connects to each entry, calls
    /// `tools/list`, and exposes every remote tool as `{server_name}__{tool}`.
    /// Default is empty: MCP is opt-in per user install.
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfig>,

    /// `[solana]` — signing key of the Solana write tools (`mode = "send"`)
    /// and the sandbox rules that keep it private (`config/solana.rs`).
    /// Absent = the write tools only simulate.
    #[serde(default)]
    pub solana: solana::SolanaConfig,

    /// `[xmarket]` — xmarket runtime settings (`config/xmarket.rs`): the
    /// install-wide state directory `<TENGU_HOME>/state/<state>`. Absent =
    /// not an xmarket sandbox.
    #[serde(default)]
    pub xmarket: Option<xmarket::XmarketConfig>,

    /// `[risk]` — every limit the gate inside each exec tool checks
    /// (`config/risk.rs`; every field required). Absent = exec tools refuse.
    #[serde(default)]
    pub risk: Option<risk::RiskConfig>,

    /// `[paper]` — paper fill engine knobs (`config/risk.rs`); required with
    /// `[risk]`.
    #[serde(default)]
    pub paper: Option<risk::PaperConfig>,
    /// `[rate_limits.<name>]` — request budgets (one token bucket per name,
    /// per-request weights) shared process-wide by feeds and tools
    /// (`config/rate_limits.rs`, `outbound/rate_limit.rs`). A name without a
    /// section is unlimited.
    #[serde(default)]
    pub rate_limits: HashMap<String, rate_limits::RateLimitConfig>,
    /// `[runtime]` — `tengu run` knobs (`config/runtime.rs`): shutdown grace,
    /// loop events in flight. Defaults apply when absent.
    #[serde(default)]
    pub runtime: runtime::RuntimeConfig,
    /// `[recorder]` — observation history (`config/recorder.rs`): which
    /// schemas `RecordingObservationStore` appends to
    /// `<TENGU_HOME>/state/<xmarket.state>/history/`. Default: off.
    #[serde(default)]
    pub recorder: recorder::RecorderConfig,
    /// `[backtest]` — history-first research (`config/backtest.rs`): costs per
    /// venue prefix, universes, strategy specs, the Jev gate loop. Needs
    /// `[xmarket]`. Absent = no backtests.
    #[serde(default)]
    pub backtest: Option<backtest::BacktestConfig>,
    /// `[feeds.<name>]` — scheduled work of `tengu run` (`config/feeds.rs`):
    /// `kind = "tool"` calls a tool of an agent, `kind = "tick"` sends a
    /// decision-loop event, on `every_secs` / `windows` / `at` clock ticks.
    /// Absent = no feeds.
    #[serde(default)]
    pub feeds: std::collections::BTreeMap<String, feeds::FeedConfig>,

    /// `[studio]` — Tengu Studio knobs (`config/studio.rs`): `control` lets
    /// `tengu studio` Play / Stop this sandbox's runtime. Default: read-only.
    #[serde(default)]
    pub studio: studio::StudioConfig,

    /// `[sources]` — the source registry (O2, `config/sources.rs`): one row per
    /// approved external source (SEC EDGAR, EU TED) and the state dir of
    /// `sources.db`. Absent = no source layer.
    #[serde(default)]
    pub sources: Option<sources::SourcesConfig>,

    /// `[soe]` — the Software Opportunity Engine's weekly cycle (O3,
    /// `config/soe.rs`): the Architect and Critic agents, the cycle limits,
    /// token prices. Present ⇒ a closed-world, hardened sandbox whose state
    /// root is the `[sources]` state dir. Absent = no SOE cycle.
    #[serde(default)]
    pub soe: Option<soe::SoeConfig>,

    /// `[a2a]` — A2A (Agent2Agent, `config/a2a.rs`): remote agents the opt-in
    /// `a2a` tool may call (`[a2a.remotes.<name>]`) and `[a2a.server]`
    /// (`tengu a2a serve`) exposing this sandbox's agents to other harnesses.
    /// Absent = no A2A.
    #[serde(default)]
    pub a2a: Option<a2a::A2aConfig>,

    /// Skill-lifecycle subsystem configuration (eval runner, distill pipeline).
    /// Absent by default — the subsystem is fully opt-in.
    #[serde(default)]
    pub skill_lifecycle: Option<crate::config::skill_lifecycle::SkillLifecycleConfig>,

    /// `[generation]` — the lineage generation this sandbox is bound to
    /// (`config/lineage.rs`): `Config::load` refuses any tool or strategy
    /// kind outside it. Absent = unbound.
    #[serde(default)]
    pub generation: Option<lineage::GenerationBinding>,

    /// Runtime (never in TOML): the bound generation's resolved scope, set by
    /// `Config::load` (`lineage::binding_errors`) → `SandboxSections::generation`.
    #[serde(skip)]
    pub generation_scope:
        Option<std::sync::Arc<crate::domain::lineage::generation::GenerationScope>>,

    /// `[strategy_ranking]` — the strategy-ranking contracts this sandbox runs
    /// and their lineage registry (`config/strategy_ranking.rs`); run-dir
    /// retention keeps every run that registry cites. Absent = none.
    #[serde(default)]
    pub strategy_ranking: Option<strategy_ranking::StrategyRankingConfig>,

    /// Runtime (never in TOML): `[strategy_ranking]` resolved by
    /// `Config::load` (`strategy_ranking::section_errors`) →
    /// `SandboxSections::ranking`.
    #[serde(skip)]
    pub ranking_section: Option<std::sync::Arc<strategy_ranking::RankingSection>>,

    /// Name of the sandbox this `Config` was loaded from, or
    /// `None` for the default user config. Populated by `load_sandbox_or` in
    /// `main.rs`. Plumbed through `SubprocessRunner` and the IPC payload so
    /// `tengu run-agent` children resolve `sandboxes/<name>/config.toml` for
    /// the `[agents.<name>]` block they run plus scopes/secrets/MCP servers —
    /// without this the parent + child would see different agents and scope
    /// rules.
    ///
    /// `#[serde(skip)]` because this is a runtime-resolved field, never
    /// written to the TOML on disk. Skip on serialise too so dumping the
    /// config doesn't leak it.
    #[serde(skip)]
    pub sandbox_name: Option<String>,

    /// Runtime (never in TOML): the file `Config::load` read, as given.
    /// `fold_default_scopes` names the sandbox from it
    /// (`paths::sandbox_of_config_file` → `SandboxSections::sandbox`, the
    /// paper ledger's account owner) — the same in every process that loads
    /// the file, the MCP bridge included. `None` for a config built in code.
    #[serde(skip)]
    pub loaded_from: Option<PathBuf>,

    /// Runtime (never in TOML): sha256 of the file `Config::load` read —
    /// `domain::lineage::pins::toml_digest` of its raw text (canonical JSON
    /// of the parsed TOML: comments and layout do not count, `${VAR}` not
    /// substituted), 64 hex, the hash family of the W1 pins. The
    /// `config_hash` of the Studio graph (`domain/workflow.rs`) and of every
    /// trace event (`domain/trace.rs`). `None` for a config built in code,
    /// or when the raw text is not TOML before substitution.
    #[serde(skip)]
    pub source_sha256: Option<String>,
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
                description: None,
                example_queries: vec![],
                tools: vec![],
                workspace: None,
                identity: IdentityConfig {
                    name: Some("Tengu".to_string()),
                    instructions: None,
                },
                flow: FlowConfig::default(),
                limits: LimitsConfig::default(),
                role: None,
                skill_packages: vec![],
                prompt_budget: PromptBudgetConfig::default(),
                workspace_tools: vec![],
                scopes: HashMap::new(),
                claude_code: None,
                local: None,
                codex: None,
                no_shell_fallback: false,
                sandbox: Default::default(),
            },
        );

        Self {
            agents,
            orchestrator: None,
            memory: MemoryConfig::default(),
            telegram: TelegramConfig::default(),
            webhooks: WebhookConfig::default(),
            decision_loops: HashMap::new(),
            scaffold: None,
            claude_code: None,
            default_scopes: HashMap::new(),
            egress: EgressConfig::default(),
            keys: keys::KeysConfig::default(),
            mcp_servers: Vec::new(),
            solana: solana::SolanaConfig::default(),
            xmarket: None,
            risk: None,
            paper: None,
            rate_limits: HashMap::new(),
            runtime: Default::default(),
            recorder: recorder::RecorderConfig::default(),
            backtest: None,
            feeds: Default::default(),
            studio: Default::default(),
            sources: None,
            soe: None,
            a2a: None,
            skill_lifecycle: None,
            generation: None,
            generation_scope: None,
            strategy_ranking: None,
            ranking_section: None,
            sandbox_name: None,
            loaded_from: None,
            source_sha256: None,
        }
    }
}

#[cfg(test)]
mod tests;
