//! Orchestration ports — what the orchestrator needs from the outside world:
//! a `Planner` that emits plan JSON, a `WorkerHandle` that runs a plan step
//! (impl: `SubprocessRunner`), and a `ChatServiceFactory` that runs one agent
//! turn for a CLI command (the skill improver). The planner's own LLM call is
//! `application::orchestrator::planner::PlannerLlm` (an engine, no port).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::domain::plan::{Plan, Step};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlannerVerdict {
    Direct {
        response: String,
    },
    Plan {
        #[serde(flatten)]
        plan: Plan,
    },
}

#[async_trait]
pub trait Planner: Send + Sync {
    /// First-plan call: user message + empty context.
    async fn plan(&self, user_message: &str) -> anyhow::Result<PlannerVerdict>;

    /// Replan call: user message + failure context to avoid repeat mistakes.
    async fn replan(
        &self,
        user_message: &str,
        prior_plan: &Plan,
        failed_step_id: &str,
        error: &str,
    ) -> anyhow::Result<PlannerVerdict>;

    /// Orchestrator session id this planner stamps on memory writes, when it
    /// has one. `replan::drive` keys the per-session active plan
    /// (`shared_files::set_active_plan`) on it so `SubprocessRunner` — which
    /// shares the same id — can hand each child its own session's plan over
    /// IPC. `None` (default) skips the per-session registration.
    fn session_id(&self) -> Option<String> {
        None
    }
}

/// Runs one plan step. The real impl (`SubprocessRunner`) spawns `tengu
/// run-agent` for it; tests inject fakes.
#[async_trait]
pub trait WorkerHandle: Send + Sync {
    /// Run `step.agent` with `step.goal + step_inputs` as the user turn.
    /// `step_inputs` is the completed upstream steps' outputs, each in a
    /// `domain::untrusted` fence (origin `step:<id>`).
    async fn run_step(&self, step: &Step, step_inputs: &str) -> anyhow::Result<String>;

    /// The agent names a step may use, sorted — `replan::drive` checks each
    /// plan against them before any step runs (`Plan::validate`). `None`
    /// (default): any name.
    fn known_agents(&self) -> Option<Vec<String>> {
        None
    }

    /// The most chars of one upstream output `step`'s agent gets as a step
    /// input: its tool-result cap (`AgentConfig::tool_result_cap`). `None`
    /// (default): whole.
    fn input_cap(&self, _step: &Step) -> Option<usize> {
        None
    }
}

/// What one planner LLM call cost (`PlannerLlm::call`), for its
/// [`crate::domain::metrics::MetricsRecord`]. Token counts come from the
/// engine's `StreamEvent::Usage` frame; latency is wall-clock around the
/// engine call. The planner attributes the prompt to its context layers
/// separately.
#[derive(Debug, Clone, Default)]
pub struct TurnTelemetry {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub model: String,
    pub latency_ms: u64,
    pub response_chars: u32,
}

/// Runs one turn of a named agent: its engine, system prompt and tools.
/// Used by CLI commands that need an agent's answer (`tengu skill evolve`
/// asks the skill-improver agent); the impl (`bootstrap::orchestrator::
/// RuntimeChatServiceFactory`) builds a `ChatRuntimeService` per call with a
/// fresh `ChatLoopState`.
#[async_trait]
pub trait ChatServiceFactory: Send + Sync {
    async fn run_turn(&self, agent: &str, text: &str) -> anyhow::Result<String>;
}
