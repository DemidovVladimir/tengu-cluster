//! `sol_price` — USD oracle price of a mint (Jupiter price v3) with an
//! optional DLMM pool price as the second source. Scaffold: no tools yet.

use std::sync::Arc;

use super::SolanaShared;
use crate::ports::tool::Tool;

pub(crate) fn tools(_shared: &SolanaShared) -> Vec<Arc<dyn Tool>> {
    Vec::new()
}
