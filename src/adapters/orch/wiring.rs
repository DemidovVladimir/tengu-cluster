//! Wiring: real `WorkerHandle` + `OrchestratorChatPort` backed by
//! a pluggable `ChatServiceFactory` (per-agent, per-call `ChatRuntimeService`
//! construction owned by the impl, which lives in `channel_runtime.rs`).

use async_trait::async_trait;
use std::sync::Arc;

use crate::adapters::memory::injector;
use crate::adapters::memory::manager::MemoryManager;
use crate::adapters::memory::writer;
use crate::adapters::orch::executor::WorkerHandle;
use crate::adapters::orch::plan::Step;
use crate::adapters::orch::planner::OrchestratorChatPort;

/// Abstracts "run a conversation turn against a named agent." Implementors
/// construct the agent-scoped `ChatRuntimeService<'a>` per call with a fresh
/// `ChatLoopState::default()` and extract the final assistant text.
///
/// Implementation lives in `channel_runtime.rs` where all the borrowed
/// dependencies (engine, agent_config, tools, ...) are rooted. Trait lives
/// here so `ChatWorker` + `ChatOrchestratorPortImpl` can take
/// `Arc<dyn ChatServiceFactory>` without dragging `ChatRuntimeService`'s
/// lifetime parameter through the whole orchestrator graph.
///
/// Per-step state is **ephemeral**: each call gets a fresh `ChatLoopState`.
/// Retries within a step re-issue the same prompt against a fresh
/// conversation — the orchestrator is responsible for any cross-step
/// state carry-over, not the worker.
#[async_trait]
pub trait ChatServiceFactory: Send + Sync {
    async fn run_turn(&self, agent: &str, text: &str) -> anyhow::Result<String>;
}

pub struct ChatWorker {
    chat: Arc<dyn ChatServiceFactory>,
    memory: Arc<MemoryManager>,
}

impl ChatWorker {
    pub fn new(chat: Arc<dyn ChatServiceFactory>, memory: Arc<MemoryManager>) -> Self {
        Self { chat, memory }
    }
}

#[async_trait]
impl WorkerHandle for ChatWorker {
    async fn run_step(&self, step: &Step, step_inputs: &str) -> anyhow::Result<String> {
        // Memory block keyed on step goal (not raw user prompt).
        let mem = injector::for_turn(&self.memory, &step.agent, &step.goal).await;
        let user_content = if step_inputs.is_empty() {
            format!("{}\n\n{}", mem.body, step.goal).trim().to_string()
        } else {
            format!("{}\n\n{}\n\nYour task:\n{}", mem.body, step_inputs, step.goal).trim().to_string()
        };

        let reply = self.chat.run_turn(&step.agent, &user_content).await?;
        writer::sync_turn(Arc::clone(&self.memory), step.agent.clone(), step.goal.clone(), reply.clone());
        Ok(reply)
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
    async fn run_orchestrator_turn(&self, agent: &str, user_message: &str) -> anyhow::Result<String> {
        let mem = injector::for_turn(&self.memory, agent, user_message).await;
        let user_content = format!("{}\n\n{}", mem.body, user_message).trim().to_string();
        let reply = self.chat.run_turn(agent, &user_content).await?;
        writer::sync_turn(Arc::clone(&self.memory), agent.to_string(), user_message.to_string(), reply.clone());
        Ok(reply)
    }
}
