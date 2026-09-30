//! xmarket tool family — the risk / paper tools over the install's paper
//! ledger (`outbound/paper_store.rs`, `<xm_state_dir>/ledger.db`) and the
//! workspace observation store. Each tool is opt-in (one catalog row per
//! name); interfaces live in [`defs`].
//!
//! | File | Tool |
//! |---|---|
//! | `risk_status.rs` | `risk_status` — `risk_state/1:<account>`: halt + kill switch, equity / P&L / loss headroom at fresh `mkt_ctx/1` marks, exposure, order rate |
//!
//! The plugin opens the observation store (`open_observation_store`) and —
//! only with `[risk]` — the ledger (`open_paper_ledger`) once. No store ⇒
//! rows are not cached (fail-soft). No `[risk]` ⇒ every tool refuses
//! `risk_config_missing` (tracker convention 9); no ledger (no `[xmarket]`)
//! ⇒ refused with that reason. Scope: `fs_roots` = the workspace (store).
//! The ledger and the kill-switch file sit outside every fs root by design
//! (convention 12): they are the tools' own state, never agent paths.

pub(crate) mod defs;
pub(crate) mod risk_status;

use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use tracing::warn;

use crate::adapters::outbound::observations::open_observation_store;
use crate::adapters::outbound::paper_store::open_paper_ledger;
use crate::config::risk::{PaperConfig, RiskConfig};
use crate::ports::observation::ObservationStore;
use crate::ports::paper::PaperLedger;
use crate::ports::tool::{PluginCtx, Tool, ToolPlugin};

pub(crate) use defs::defs_named;

/// Refusal when the sandbox has no `[risk]` (tracker convention 9).
pub(crate) const RISK_CONFIG_MISSING: &str = "risk_config_missing";

/// Handles every family tool shares.
#[derive(Clone)]
pub(crate) struct XmShared {
    /// Observation cache; `None` = rows are not cached.
    pub store: Option<Arc<dyn ObservationStore>>,
    /// The paper ledger, or why there is none.
    pub ledger: Result<Arc<dyn PaperLedger>, String>,
    /// `[risk]`, `kill_switch_file` expanded.
    pub risk: Option<RiskConfig>,
    pub paper: Option<PaperConfig>,
}

impl XmShared {
    /// `[risk]`, `[paper]` and the ledger — or the refusal every tool gives.
    pub(crate) fn parts(&self) -> Result<(&RiskConfig, &PaperConfig, &Arc<dyn PaperLedger>)> {
        let (Some(risk), Some(paper)) = (&self.risk, &self.paper) else {
            return Err(anyhow!(
                "{RISK_CONFIG_MISSING}: this sandbox has no [risk] + [paper] section"
            ));
        };
        let ledger = self
            .ledger
            .as_ref()
            .map_err(|why| anyhow!("paper ledger unavailable: {why}"))?;
        Ok((risk, paper, ledger))
    }
}

/// Plugin grouping the xmarket risk / paper tools; several catalog rows
/// share it.
pub(crate) struct XmPlugin;

#[async_trait]
impl ToolPlugin for XmPlugin {
    fn name(&self) -> &'static str {
        "xm"
    }

    async fn tools(&self, ctx: &PluginCtx<'_>) -> Result<Vec<Arc<dyn Tool>>> {
        let sections = &ctx.config.sandbox;
        let store = match open_observation_store(ctx.workspace, sections) {
            Ok(s) => Some(s),
            Err(e) => {
                let error = format!("{e:#}");
                warn!(%error, "observation store unavailable; xmarket rows are not cached");
                None
            }
        };
        let ledger = match &sections.risk {
            None => Err(RISK_CONFIG_MISSING.to_string()),
            Some(_) => open_paper_ledger(sections).map_err(|e| format!("{e:#}")),
        };
        let shared = XmShared {
            store,
            ledger,
            risk: sections.risk.clone(),
            paper: sections.paper.clone(),
        };
        Ok(risk_status::tools(&shared))
    }
}
