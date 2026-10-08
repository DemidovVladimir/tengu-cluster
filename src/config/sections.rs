//! Sandbox-level sections a tool reads at call time. `Config::fold_default_scopes`
//! resolves them once and gives every agent the same `Arc`
//! (`AgentConfig::sandbox`), so each surface — in-process executor,
//! `run-agent` child, decision-loop executor, `tengu run`, and the MCP bridge
//! once it loads the sandbox file — hands a tool identical values (tracker
//! convention 20). A new section a tool needs goes here, not into another
//! `#[serde(skip)]` field on `AgentConfig`.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

use super::backtest::BacktestConfig;
use super::rate_limits::RateLimitConfig;
use super::risk::{PaperConfig, RiskConfig};
use super::strategy_ranking::RankingSection;
use super::xmarket::WeekendFadeConfig;
use crate::config::recorder::RecorderConfig;
use crate::domain::calendar::Calendar;
use crate::domain::lineage::generation::GenerationScope;

/// Owner name of a config that is no `sandboxes/<name>/config.toml` file —
/// as `tengu run` names its runner then.
pub const DEFAULT_SANDBOX: &str = "default";

/// Resolved sandbox sections (runtime only, never in TOML).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SandboxSections {
    /// The sandbox of the config file (`paths::sandbox_of_config_file`:
    /// `<name>` for `sandboxes/<name>/config.toml`, however it was loaded);
    /// `None` = [`DEFAULT_SANDBOX`]. The paper ledger records it as the
    /// owner of each account its tools write ([`SandboxSections::owner`],
    /// `outbound/paper_store.rs`).
    pub sandbox: Option<String>,
    /// `[xmarket]` present: `<TENGU_HOME>/state/<xmarket.state>` (absolute).
    /// The install-wide xmarket stores live here (tracker convention 3).
    pub xm_state_dir: Option<PathBuf>,
    /// `[risk]` with `kill_switch_file` expanded (`config/risk.rs`). `None` ⇒
    /// every exec tool refuses (`risk_config_missing`, convention 9).
    pub risk: Option<RiskConfig>,
    /// `[paper]` — the paper fill engine's knobs (`config/risk.rs`).
    pub paper: Option<PaperConfig>,
    /// `[xmarket.calendars.<id>]`, built (`domain/calendar.rs`); an invalid
    /// row fails `Config::load`, so every configured id is here.
    pub calendars: BTreeMap<String, Calendar>,
    /// `[rate_limits.<name>]` as loaded; a tool hands the entry it budgets
    /// against to `outbound/rate_limit.rs` (absent = unlimited).
    pub rate_limits: HashMap<String, RateLimitConfig>,
    /// `[recorder]` (`config/recorder.rs`): what `RecordingObservationStore`
    /// records.
    pub recorder: RecorderConfig,
    /// `<xm_state_dir>/history` when `[recorder] enabled`: the day files
    /// `open_observation_store` records into; `None` = no recording.
    pub history_dir: Option<PathBuf>,
    /// `[xmarket.weekend_fade]` (`xm_weekend_fade`); its `calendar` is one
    /// of `calendars`.
    pub weekend_fade: Option<WeekendFadeConfig>,
    /// `[backtest]` (`config/backtest.rs`): what `tengu backtest` and the
    /// `backtest` / `market_history` tools read (xlab).
    pub backtest: Option<BacktestConfig>,
    /// `[generation]` resolved (`config/lineage.rs`): the tools and strategy
    /// kinds the bound generation's capabilities make available; `None` =
    /// unbound. The backtest use case refuses a kind outside it
    /// (`capability_unavailable`), the executor build a tool.
    pub generation: Option<Arc<GenerationScope>>,
    /// `[strategy_ranking]` resolved (`config/strategy_ranking.rs`): the
    /// contracts, their registry and the run dirs it cites — kept by
    /// run-dir retention bound or not (`application/backtest/mod.rs`);
    /// `None` = no ranking.
    pub ranking: Option<Arc<RankingSection>>,
}

impl SandboxSections {
    /// The ledger owner these tools write as: [`SandboxSections::sandbox`],
    /// else [`DEFAULT_SANDBOX`].
    pub fn owner(&self) -> &str {
        self.sandbox.as_deref().unwrap_or(DEFAULT_SANDBOX)
    }
}
