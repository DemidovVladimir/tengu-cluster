//! `dlmm_pool` + `dlmm_positions` — Meteora DLMM pool and position state
//! from Solana RPC account reads. Scaffold: no tools yet.

use std::sync::Arc;

use super::SolanaShared;
use crate::ports::tool::Tool;

pub(crate) fn tools(_shared: &SolanaShared) -> Vec<Arc<dyn Tool>> {
    Vec::new()
}
