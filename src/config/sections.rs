//! Sandbox-level sections a tool reads at call time. `Config::fold_default_scopes`
//! resolves them once and gives every agent the same `Arc`
//! (`AgentConfig::sandbox`), so each surface — in-process executor,
//! `run-agent` child, decision-loop executor, `tengu run`, and the MCP bridge
//! once it loads the sandbox file — hands a tool identical values (tracker
//! convention 20). A new section a tool needs goes here, not into another
//! `#[serde(skip)]` field on `AgentConfig`.

use std::path::PathBuf;

/// Resolved sandbox sections (runtime only, never in TOML).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SandboxSections {
    /// `[xmarket]` present: `<TENGU_HOME>/state/<xmarket.state>` (absolute).
    /// The install-wide xmarket stores live here (tracker convention 3).
    pub xm_state_dir: Option<PathBuf>,
}
