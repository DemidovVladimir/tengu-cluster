//! DAG executor: parallel step dispatch with retry escalation.

use async_trait::async_trait;

use crate::adapters::orch::plan::Step;

/// Abstracts "how to run a worker step." The real impl calls
/// `ChatRuntimeService::process_user_text` under the hood. Tests
/// inject fake impls.
#[async_trait]
pub trait WorkerHandle: Send + Sync {
    /// Run `step.agent` with `step.goal + step_inputs` as the user turn.
    /// `step_inputs` is the rendered `<step-input>` blocks from upstream
    /// completed steps.
    async fn run_step(&self, step: &Step, step_inputs: &str) -> anyhow::Result<String>;
}
