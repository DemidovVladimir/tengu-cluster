//! Orchestration ports — what the orchestrator needs from the outside world:
//! a `Planner` that emits plan JSON, an `OrchestratorChatPort` /
//! `ChatServiceFactory` to run one LLM turn, and a `WorkerHandle` that runs a
//! plan step (impl: `SubprocessRunner`).

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

/// Minimal port the planner needs from the chat runtime (avoids cyclic deps).
#[async_trait]
pub trait OrchestratorChatPort: Send + Sync {
    /// Run a single LLM conversation turn and return the final message
    /// (JSON string). The port handles system-prompt assembly, tool
    /// loop (memory_search), and memory injection.
    async fn run_orchestrator_turn(
        &self,
        agent: &str,
        user_message: &str,
    ) -> anyhow::Result<String>;

    /// Phase 4c — like `run_orchestrator_turn` but with `system_prompt`
    /// replacing the agent's `identity.instructions` for this single call.
    /// Used by the RAG planner to inject `skills/orchestrator/SKILL.md`
    /// as the planner system prompt. Default impl falls back to the
    /// override-less call.
    async fn run_orchestrator_turn_with_system(
        &self,
        agent: &str,
        _system_prompt: &str,
        user_message: &str,
    ) -> anyhow::Result<String> {
        self.run_orchestrator_turn(agent, user_message).await
    }

    /// Metered variant — returns `(reply, telemetry)` so callers can build
    /// a `MetricsRecord` with prompt/completion tokens and wall-clock
    /// latency. Default impl falls back to `run_orchestrator_turn_with_system`
    /// and returns zeroed telemetry. The runtime impl
    /// (`ChatOrchestratorPortImpl`) overrides this to plumb real numbers.
    async fn run_orchestrator_turn_with_system_metered(
        &self,
        agent: &str,
        system_prompt: &str,
        user_message: &str,
    ) -> anyhow::Result<(String, crate::ports::orchestration::TurnTelemetry)> {
        let reply = self
            .run_orchestrator_turn_with_system(agent, system_prompt, user_message)
            .await?;
        Ok((reply, crate::ports::orchestration::TurnTelemetry::default()))
    }
}

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

/// Pure-data telemetry returned alongside an LLM-call response so the caller
/// can construct a [`crate::adapters::metrics::MetricsRecord`]. Defaults to
/// all-zero so trait impls that don't (yet) wire telemetry stay valid.
///
/// Token counts come from the engine's `StreamEvent::Usage` frame; latency is
/// wall-clock around the full `process_user_text` call (which includes
/// memory recall + the engine round trip). For the planner path this is the
/// only telemetry available — the planner itself attributes the prompt to
/// per-context-layer breakdown separately.
#[derive(Debug, Clone, Default)]
pub struct TurnTelemetry {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub model: String,
    pub latency_ms: u64,
    pub response_chars: u32,
}

/// Abstracts "run a conversation turn against a named agent." Implementors
/// construct the agent-scoped `ChatRuntimeService<'a>` per call with a fresh
/// `ChatLoopState::default()` and extract the final assistant text.
///
/// Implementation lives in `channel_runtime.rs` where all the borrowed
/// dependencies (engine, agent_config, tools, ...) are rooted. Trait lives
/// here so `ChatOrchestratorPortImpl` (the planner-side LLM turn) can take
/// `Arc<dyn ChatServiceFactory>` without dragging `ChatRuntimeService`'s
/// lifetime parameter through the whole orchestrator graph.
///
/// Per-step state is **ephemeral**: each call gets a fresh `ChatLoopState`.
#[async_trait]
pub trait ChatServiceFactory: Send + Sync {
    async fn run_turn(&self, agent: &str, text: &str) -> anyhow::Result<String>;

    /// Like `run_turn`, but with the supplied `system_prompt` replacing the
    /// agent's configured `identity.instructions` for this single call.
    ///
    /// Phase 4c of the redesign: the RAG planner loads
    /// `skills/orchestrator/SKILL.md` and uses it as the planner system
    /// prompt, so the orchestrator agent's "run the DeSci pipeline"-style
    /// identity does not leak into the planning turn.
    ///
    /// Default impl delegates to `run_turn`, ignoring the override — so
    /// existing implementations stay valid without changes. The runtime impl
    /// (`RuntimeChatServiceFactory`) overrides this to actually swap.
    async fn run_turn_with_system(
        &self,
        agent: &str,
        _system_prompt: &str,
        text: &str,
    ) -> anyhow::Result<String> {
        self.run_turn(agent, text).await
    }

    /// Like `run_turn_with_system` but additionally returns [`TurnTelemetry`]
    /// (token counts, latency, model slug) so the caller — typically the
    /// planner — can build a full `MetricsRecord`. Default impl falls back
    /// to `run_turn_with_system` and returns zeroed telemetry; the runtime
    /// impl (`RuntimeChatServiceFactory`) overrides this to wire real numbers.
    async fn run_turn_with_system_metered(
        &self,
        agent: &str,
        system_prompt: Option<&str>,
        text: &str,
    ) -> anyhow::Result<(String, TurnTelemetry)> {
        let reply = match system_prompt {
            Some(s) => self.run_turn_with_system(agent, s, text).await?,
            None => self.run_turn(agent, text).await?,
        };
        Ok((reply, TurnTelemetry::default()))
    }
}
