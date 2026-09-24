//! `lp_snapshot` + `hedge_decide` + `lp_decide` — the composed wallet x pool
//! snapshot and the pure hedge / LP decisions over it. Scaffold: no tools yet.

use std::sync::Arc;

use super::SolanaShared;
use crate::ports::tool::Tool;

pub(crate) fn tools(_shared: &SolanaShared) -> Vec<Arc<dyn Tool>> {
    Vec::new()
}
