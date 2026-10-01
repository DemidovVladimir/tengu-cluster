//! xlab tool family — history-first research reads over the sandbox's
//! market-data warehouse (`outbound/market_data.rs`,
//! `<xm_state_dir>/market.db`; `docs/xlab-2026-10-01.md` § 8). Read-only:
//! these tools cannot move money. Each tool is opt-in (one catalog row per
//! name); interfaces live in [`defs`].
//!
//! | File | Tool |
//! |---|---|
//! | `history.rs` | `market_history` — `mkt_history/1:<instrument>:<interval>`: stats, a bar sample and the coverage of one instrument in a window; `fetch = true` backfills the missing part first (`outbound/backfill/`) |
//!
//! The plugin opens the market-data store once (`open_market_data`) and the
//! workspace observation store (`open_observation_store`: rows are recorded
//! when `[recorder]` takes them; ttl 0, never cached). No `[xmarket]` ⇒
//! every tool refuses `state_dir_missing`; a store that does not open ⇒
//! `market_data_unavailable`. Scope: `fs_roots` = the workspace (the
//! observation store); a fetch checks every request against `net_hosts`
//! (`api.hyperliquid.xyz`, `api.geckoterminal.com`) and reads `HL_API_URL` /
//! `GECKO_API_URL` only through `env_reads`. `market.db` sits in the state
//! dir, outside every fs root by design (like `ledger.db`): the tools' own
//! state, never an agent path.

pub(crate) mod defs;
pub(crate) mod history;

use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use tracing::warn;

use crate::adapters::outbound::market_data::open_market_data;
use crate::adapters::outbound::observations::open_observation_store;
use crate::adapters::outbound::tools::xm::STATE_DIR_MISSING;
use crate::config::sections::SandboxSections;
use crate::config::xmarket::MARKET_DB;
use crate::ports::market_data::MarketDataStore;
use crate::ports::observation::ObservationStore;
use crate::ports::tool::{PluginCtx, Tool, ToolPlugin};

pub(crate) use defs::defs_named;

/// Refusal when `market.db` could not be opened.
pub(crate) const MARKET_DATA_UNAVAILABLE: &str = "market_data_unavailable";

/// Handles every family tool shares.
#[derive(Clone)]
pub(crate) struct XlabShared {
    /// The market-data warehouse, or the refusal every tool gives.
    pub market: Result<Arc<dyn MarketDataStore>, String>,
    /// Observation store (records rows); `None` = nothing recorded.
    pub store: Option<Arc<dyn ObservationStore>>,
    /// The sandbox's sections (`[rate_limits.*]`, `[backtest]`).
    pub sandbox: Arc<SandboxSections>,
}

impl XlabShared {
    /// The warehouse, or the refusal (module doc).
    pub(crate) fn market(&self) -> Result<&dyn MarketDataStore> {
        match &self.market {
            Ok(m) => Ok(m.as_ref()),
            Err(why) => Err(anyhow!("{why}")),
        }
    }
}

/// The warehouse of `sections`, or the refusal string (module doc).
pub(crate) fn open_market(sections: &SandboxSections) -> Result<Arc<dyn MarketDataStore>, String> {
    if sections.xm_state_dir.is_none() {
        return Err(format!(
            "{STATE_DIR_MISSING}: market data unavailable: no [xmarket] section — add [xmarket] \
             state = \"<name>\" (the warehouse is <TENGU_HOME>/state/<name>/{MARKET_DB})"
        ));
    }
    open_market_data(sections).map_err(|e| format!("{MARKET_DATA_UNAVAILABLE}: {e:#}"))
}

/// Plugin grouping the xlab tools; several catalog rows share it.
pub(crate) struct XlabPlugin;

#[async_trait]
impl ToolPlugin for XlabPlugin {
    fn name(&self) -> &'static str {
        "xlab"
    }

    async fn tools(&self, ctx: &PluginCtx<'_>) -> Result<Vec<Arc<dyn Tool>>> {
        let sections = &ctx.config.sandbox;
        let store = match open_observation_store(ctx.workspace, sections) {
            Ok(s) => Some(s),
            Err(e) => {
                let error = format!("{e:#}");
                warn!(%error, "observation store unavailable; xlab rows are not recorded");
                None
            }
        };
        let shared = XlabShared {
            market: open_market(sections),
            store,
            sandbox: Arc::clone(sections),
        };
        Ok(history::tools(&shared))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_warehouse_needs_xmarket() {
        let e = open_market(&SandboxSections::default()).err().unwrap();
        assert!(
            e.starts_with("state_dir_missing: market data unavailable: no [xmarket] section"),
            "{e}"
        );
        let dir = tempfile::tempdir().unwrap();
        let sections = SandboxSections {
            xm_state_dir: Some(dir.path().join("xlab")),
            ..Default::default()
        };
        assert!(open_market(&sections).is_ok());
        assert!(dir.path().join("xlab").join(MARKET_DB).exists());
        // A state dir that cannot be created: the store's own error.
        let blocked = dir.path().join("file");
        std::fs::write(&blocked, "x").unwrap();
        let sections = SandboxSections {
            xm_state_dir: Some(blocked.join("xlab")),
            ..Default::default()
        };
        let e = open_market(&sections).err().unwrap();
        assert!(e.starts_with("market_data_unavailable: "), "{e}");
    }
}
