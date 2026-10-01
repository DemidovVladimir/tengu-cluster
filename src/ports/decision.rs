//! Ports for the decision loop (`application::decision_loop`).
//!
//! - `DecisionEngine` — a System One decision model (Jev). Implemented by
//!   `adapters::outbound::decisions::JevClient`, and for replay by
//!   `adapters::outbound::decision_cache::CachedDecisionEngine` (stored
//!   answers; [`CacheStats`] for the backtest report's spend estimate).
//! - `Escalator` — hands a low-confidence decision to the LLM side (planner →
//!   subagents). Implemented by the inbound surface that owns an orchestrator
//!   (the webhook listener); `None` means "log only".

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;

use crate::domain::decision::{Decision, Question};

#[async_trait]
pub(crate) trait DecisionEngine: Send + Sync {
    /// Model slug, for metrics / audit.
    fn model(&self) -> &str;
    /// Ask every question in one call against `state`.
    async fn decide(
        &self,
        state: &Value,
        questions: &BTreeMap<String, Question>,
    ) -> anyhow::Result<Decision>;
    /// Counters of a caching engine; `None` (default) = every call is live.
    fn cache_stats(&self) -> Option<CacheStats> {
        None
    }
}

/// What a caching `DecisionEngine` did so far — the backtest report's spend
/// estimate (live calls × the per-decision price).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub(crate) struct CacheStats {
    /// Answered from the cache: no call.
    pub hits: u64,
    /// Not in the cache: one live call each when online; an error offline.
    pub misses: u64,
    /// Calls that failed: offline misses, failed live calls, store errors.
    pub errors: u64,
}

#[async_trait]
pub(crate) trait Escalator: Send + Sync {
    /// Fire-and-forget: run an orchestrator turn for `message` under
    /// `session_id`. Must not block the loop for the LLM turn's duration.
    async fn escalate(&self, session_id: String, message: String);
}
