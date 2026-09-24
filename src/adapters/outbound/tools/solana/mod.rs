//! Solana LP tool family — typed, cached observations over Solana RPC,
//! Jupiter and the Meteora datapi, plus pure LP / hedge decisions.
//!
//! Provides (each opt-in via `workspace_tools`, one catalog row per name):
//! `sol_price`, `dlmm_pools`, `dlmm_pool`, `dlmm_positions`, `jup_perps`,
//! `solana_wallet`, `solana_tx`, `lp_snapshot`, `hedge_decide`, `lp_decide`.
//! Interfaces (names, descriptions, input schemas) live in [`defs`]; each
//! family file implements its tools over the shared observation store:
//!
//! | File | Tools |
//! |---|---|
//! | `price.rs` | `sol_price` |
//! | `pools.rs` | `dlmm_pools` |
//! | `dlmm.rs` | `dlmm_pool`, `dlmm_positions` |
//! | `perps.rs` | `jup_perps` |
//! | `wallet.rs` | `solana_wallet`, `solana_tx` |
//! | `lp.rs` | `lp_snapshot`, `hedge_decide`, `lp_decide` |
//!
//! The plugin opens `<workspace>/.tengu/observations.db` once
//! (`SqliteObservationStore`); when that fails the tools read live without
//! caching (fail-soft).

pub(crate) mod defs;
pub(crate) mod dlmm;
pub(crate) mod lp;
pub(crate) mod perps;
pub(crate) mod pools;
pub(crate) mod price;
pub(crate) mod wallet;

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use crate::adapters::outbound::observations::SqliteObservationStore;
use crate::ports::observation::ObservationStore;
use crate::ports::tool::{PluginCtx, Tool, ToolPlugin};

pub(crate) use defs::defs_named;

/// Handles every family tool shares.
#[derive(Clone, Default)]
pub(crate) struct SolanaShared {
    /// Observation cache; `None` = read live, never cache.
    pub store: Option<Arc<dyn ObservationStore>>,
}

/// Plugin grouping the Solana LP tools. Several catalog rows share it; it
/// is registered once and `register_plugin` keeps the allowed names.
pub(crate) struct SolanaPlugin;

#[async_trait]
impl ToolPlugin for SolanaPlugin {
    fn name(&self) -> &'static str {
        "solana"
    }

    async fn tools(&self, ctx: &PluginCtx<'_>) -> Result<Vec<Arc<dyn Tool>>> {
        let store = match SqliteObservationStore::open(ctx.workspace) {
            Ok(s) => Some(Arc::new(s) as Arc<dyn ObservationStore>),
            Err(e) => {
                let error = format!("{e:#}");
                tracing::warn!(%error, "observation store unavailable; Solana tools read live");
                None
            }
        };
        let shared = SolanaShared { store };
        let mut tools = Vec::new();
        tools.extend(price::tools(&shared));
        tools.extend(pools::tools(&shared));
        tools.extend(dlmm::tools(&shared));
        tools.extend(perps::tools(&shared));
        tools.extend(wallet::tools(&shared));
        tools.extend(lp::tools(&shared));
        Ok(tools)
    }
}
