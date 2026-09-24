//! `dlmm_pools` — Meteora DLMM pool search (dlmm.datapi.meteora.ag).
//! Scaffold: no tools yet.

use std::sync::Arc;

use super::SolanaShared;
use crate::ports::tool::Tool;

pub(crate) fn tools(_shared: &SolanaShared) -> Vec<Arc<dyn Tool>> {
    Vec::new()
}
