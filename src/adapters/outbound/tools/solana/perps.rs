//! `jup_perps` — a wallet's Jupiter perps long / short positions and the
//! SOL / USDC custody rates. Scaffold: no tools yet.

use std::sync::Arc;

use super::SolanaShared;
use crate::ports::tool::Tool;

pub(crate) fn tools(_shared: &SolanaShared) -> Vec<Arc<dyn Tool>> {
    Vec::new()
}
