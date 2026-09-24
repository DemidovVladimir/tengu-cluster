//! Ports for the decision loop (`application::decision_loop`).
//!
//! - `DecisionEngine` — a System One decision model (Jev). Implemented by
//!   `adapters::outbound::decisions::JevClient`.
//! - `Escalator` — hands a low-confidence decision to the LLM side (planner →
//!   subagents). Implemented by the inbound surface that owns an orchestrator
//!   (the webhook listener); `None` means "log only".

use std::collections::BTreeMap;

use async_trait::async_trait;
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
}

#[async_trait]
pub(crate) trait Escalator: Send + Sync {
    /// Fire-and-forget: run an orchestrator turn for `message` under
    /// `session_id`. Must not block the loop for the LLM turn's duration.
    async fn escalate(&self, session_id: String, message: String);
}
