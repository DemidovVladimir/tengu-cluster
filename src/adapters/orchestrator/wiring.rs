//! Wiring: `OrchestratorChatPort` backed by a pluggable `ChatServiceFactory`
//! (per-agent, per-call `ChatRuntimeService` construction owned by the impl,
//! which lives in `channel_runtime.rs`).
//!
//! Phase 7.1 (full) — `ChatWorker` is gone. `SubprocessRunner` is the only
//! `WorkerHandle` impl now that `engine = "rag"` is the only orchestrator
//! path. The `ChatServiceFactory` trait remains because the planner side
//! still needs an LLM-call abstraction (used by `ChatOrchestratorPortImpl`
//! below for the planner LLM turn).

use async_trait::async_trait;
use std::sync::Arc;

use crate::adapters::memory::injector;
use crate::adapters::memory::manager::MemoryManager;
use crate::adapters::memory::writer;
use crate::adapters::orchestrator::planner::OrchestratorChatPort;

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
}

pub struct ChatOrchestratorPortImpl {
    chat: Arc<dyn ChatServiceFactory>,
    memory: Arc<MemoryManager>,
}

impl ChatOrchestratorPortImpl {
    pub fn new(chat: Arc<dyn ChatServiceFactory>, memory: Arc<MemoryManager>) -> Self {
        Self { chat, memory }
    }
}

#[async_trait]
impl OrchestratorChatPort for ChatOrchestratorPortImpl {
    async fn run_orchestrator_turn(
        &self,
        agent: &str,
        user_message: &str,
    ) -> anyhow::Result<String> {
        let mem = injector::for_turn(&self.memory, agent, user_message).await;
        let user_content = format!("{}\n\n{}", mem.body, user_message)
            .trim()
            .to_string();
        let reply = self.chat.run_turn(agent, &user_content).await?;
        writer::sync_turn(
            Arc::clone(&self.memory),
            agent.to_string(),
            user_message.to_string(),
            reply.clone(),
        );
        Ok(reply)
    }

    /// Phase 4c — pass through to `ChatServiceFactory::run_turn_with_system`,
    /// also skipping memory injection (the planner does its own context
    /// assembly via the ranked roster + dialogue, so we don't want the
    /// pre-turn memory block stuffed in too).
    async fn run_orchestrator_turn_with_system(
        &self,
        agent: &str,
        system_prompt: &str,
        user_message: &str,
    ) -> anyhow::Result<String> {
        let reply = self
            .chat
            .run_turn_with_system(agent, system_prompt, user_message)
            .await?;
        // Still record the turn into memory so cross-plan recall has it.
        writer::sync_turn(
            Arc::clone(&self.memory),
            agent.to_string(),
            user_message.to_string(),
            reply.clone(),
        );
        Ok(reply)
    }
}

#[cfg(test)]
mod threading_tests {
    //! Phase 7.1 — most of this module's tests covered `ChatWorker::run_step`,
    //! which has been deleted along with the static-mode worker. Only the
    //! `snapshots_inputs_fn` test survives because it exercises a helper
    //! `RagPlanner` still relies on.

    use super::*;

    /// The closure returned by `snapshots_inputs_fn` must look up agents by
    /// name and return cloned `ChatTurnInputs`. Missing agents produce a
    /// hard error that surfaces as a step failure (replan trigger).
    #[tokio::test]
    async fn snapshots_inputs_fn_missing_agent_errors() {
        use crate::adapters::channel_runtime::{snapshots_inputs_fn, OrchestratorSnapshots};
        use std::collections::HashMap;
        let map: OrchestratorSnapshots = Arc::new(std::sync::RwLock::new(HashMap::new()));
        let inputs_fn = snapshots_inputs_fn(map);
        match inputs_fn("missing-agent") {
            Ok(_) => panic!("expected missing-agent error"),
            Err(e) => assert!(e.to_string().contains("missing-agent")),
        }
    }
}
