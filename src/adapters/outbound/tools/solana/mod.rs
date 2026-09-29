//! Solana LP tool family — typed, cached observations over Solana RPC,
//! Jupiter and the Meteora datapi, plus pure LP / hedge decisions.
//!
//! Provides (each opt-in via `workspace_tools`, one catalog row per name):
//! `sol_price`, `dlmm_pools`, `dlmm_pool`, `dlmm_positions`, `jup_perps`,
//! `solana_wallet`, `solana_tx`, `lp_snapshot`, `hedge_decide`, `lp_decide`,
//! and the write tools (`mode = simulate | send`, runner `write_common`):
//! `solana_close_token_accounts`, `jupiter_swap`, `dlmm_close_position`,
//! `dlmm_open_position`, `jup_perps_order`.
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
//! | `write_tokens.rs` | `solana_close_token_accounts` |
//! | `write_swap.rs` | `jupiter_swap` (Jupiter Ultra) |
//! | `write_dlmm.rs` | `dlmm_close_position`, `dlmm_open_position` |
//! | `write_perps.rs` | `jup_perps_order` |
//!
//! The plugin opens `<workspace>/.tengu/observations.db` once
//! (`SqliteObservationStore`); when that fails the tools read live without
//! caching (fail-soft). It also opens the install-wide write store
//! (`<TENGU_HOME>/state/solana-writes.db`: lease, in-flight sends, write
//! fence); when that fails there is no fence and `mode = "send"` is
//! refused.

pub(crate) mod defs;
pub(crate) mod dlmm;
pub(crate) mod lp;
pub(crate) mod perps;
pub(crate) mod pools;
pub(crate) mod price;
pub(crate) mod wallet;
pub(crate) mod write_common;
pub(crate) mod write_dlmm;
pub(crate) mod write_perps;
pub(crate) mod write_swap;
pub(crate) mod write_tokens;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use crate::adapters::outbound::observations::SqliteObservationStore;
use crate::adapters::outbound::solana::writes_store::SqliteWriteStore;
use crate::ports::observation::ObservationStore;
use crate::ports::solana_writes::SolanaWriteStore;
use crate::ports::tool::{PluginCtx, Tool, ToolPlugin};

pub(crate) use defs::defs_named;

/// Handles every family tool shares.
#[derive(Clone, Default)]
pub(crate) struct SolanaShared {
    /// Observation cache; `None` = read live, never cache.
    pub store: Option<Arc<dyn ObservationStore>>,
    /// Install-wide write store (lease / pending / fence); `None` ⇒ the
    /// write tools refuse `mode = "send"`.
    pub writes: Option<Arc<dyn SolanaWriteStore>>,
    /// `[solana] signer_key_file` (loaded only at send time).
    pub signer_key_file: Option<PathBuf>,
}

impl SolanaShared {
    /// The wallet's write fence (slot of its last landed write), if known.
    pub(crate) async fn fence(&self, wallet: &crate::domain::solana::Pubkey) -> Option<u64> {
        self.writes
            .as_ref()?
            .fence(&wallet.to_string())
            .await
            .ok()
            .flatten()
    }
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
        // Every Solana agent reads the write fence (a write by another
        // process makes older snapshots stale); only write tools send.
        let writes = match SqliteWriteStore::open(&SqliteWriteStore::default_dir()) {
            Ok(s) => Some(Arc::new(s) as Arc<dyn SolanaWriteStore>),
            Err(e) => {
                let error = format!("{e:#}");
                tracing::warn!(%error, "solana write store unavailable; no write fence, mode = send is refused");
                None
            }
        };
        let shared = SolanaShared {
            store,
            writes,
            signer_key_file: ctx.config.signer_key_file.clone(),
        };
        let mut tools = Vec::new();
        tools.extend(price::tools(&shared));
        tools.extend(pools::tools(&shared));
        tools.extend(dlmm::tools(&shared));
        tools.extend(perps::tools(&shared));
        tools.extend(wallet::tools(&shared));
        tools.extend(lp::tools(&shared));
        tools.extend(write_tokens::tools(&shared));
        tools.extend(write_swap::tools(&shared));
        tools.extend(write_dlmm::tools(&shared));
        tools.extend(write_perps::tools(&shared));
        Ok(tools)
    }
}
