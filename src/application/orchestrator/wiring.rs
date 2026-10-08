//! Wiring: `OrchestratorChatPort` backed by a pluggable `ChatServiceFactory`
//! (per-agent, per-call `ChatRuntimeService` construction owned by the impl,
//! which lives in `bootstrap/`).
//!
//! Phase 7.1 (full) — `ChatWorker` is gone. `SubprocessRunner` is the only
//! `WorkerHandle` impl now that `engine = "rag"` is the only orchestrator
//! path. The `ChatServiceFactory` trait remains because the planner side
//! still needs an LLM-call abstraction (used by `ChatOrchestratorPortImpl`
//! below for the planner LLM turn).

use async_trait::async_trait;
use std::sync::Arc;

use crate::application::memory::injector;
use crate::application::memory::manager::MemoryManager;
use crate::application::memory::writer;
use crate::ports::orchestration::{ChatServiceFactory, OrchestratorChatPort, TurnTelemetry};

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
        // A planner turn is not written to workspace memory: its "user
        // message" is the whole assembled planner prompt (roster, recall,
        // history), which chat recall then surfaced as a memory, and only
        // Telegram had a store for it. Cross-plan recall reads Postgres
        // `agentic_memory`, not this store.
        self.chat
            .run_turn_with_system(agent, system_prompt, user_message)
            .await
    }

    async fn run_orchestrator_turn_with_system_metered(
        &self,
        agent: &str,
        system_prompt: &str,
        user_message: &str,
    ) -> anyhow::Result<(String, TurnTelemetry)> {
        // Not written to workspace memory (see `run_orchestrator_turn_with_system`).
        self.chat
            .run_turn_with_system_metered(agent, Some(system_prompt), user_message)
            .await
    }
}

#[cfg(test)]
mod threading_tests {
    //! Phase 7.1 — most of this module's tests covered `ChatWorker::run_step`,
    //! which has been deleted along with the static-mode worker. Only the
    //! `snapshots_inputs_fn` test survives because it exercises a helper
    //! `RagPlanner` still relies on.

    use super::*;

    struct Echo;
    #[async_trait]
    impl ChatServiceFactory for Echo {
        async fn run_turn(&self, _agent: &str, _text: &str) -> anyhow::Result<String> {
            Ok(r#"{"kind":"direct","response":"hi"}"#.into())
        }
    }

    struct Recording(Arc<std::sync::atomic::AtomicUsize>);
    #[async_trait]
    impl crate::ports::memory::MemoryProvider for Recording {
        fn name(&self) -> &str {
            "builtin"
        }
        fn is_available(&self) -> bool {
            true
        }
        async fn initialize(&self, _: &str, _: &std::path::Path) -> anyhow::Result<()> {
            Ok(())
        }
        async fn prefetch(&self, _: &str, _: &str) -> String {
            String::new()
        }
        async fn sync_turn(&self, _: &str, _: &str, _: &str) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        async fn shutdown(&self) {}
    }

    /// A planner turn writes nothing to workspace memory (its message is the
    /// whole assembled planner prompt).
    #[tokio::test]
    async fn planner_turns_are_not_written_to_workspace_memory() {
        let writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let memory = Arc::new(MemoryManager::new());
        memory
            .add_provider(Box::new(Recording(Arc::clone(&writes))))
            .await;
        let port = ChatOrchestratorPortImpl::new(Arc::new(Echo), memory);
        port.run_orchestrator_turn_with_system_metered("planner", "sys", "## Planner registry …")
            .await
            .unwrap();
        port.run_orchestrator_turn_with_system("planner", "sys", "x")
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(writes.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    /// The closure returned by `snapshots_inputs_fn` must look up agents by
    /// name and return cloned `ChatTurnInputs`. Missing agents produce a
    /// hard error that surfaces as a step failure (replan trigger).
    #[tokio::test]
    async fn snapshots_inputs_fn_missing_agent_errors() {
        use crate::bootstrap::orchestrator::{snapshots_inputs_fn, OrchestratorSnapshots};
        use std::collections::HashMap;
        let map: OrchestratorSnapshots = Arc::new(std::sync::RwLock::new(HashMap::new()));
        let inputs_fn = snapshots_inputs_fn(map);
        match inputs_fn("missing-agent") {
            Ok(_) => panic!("expected missing-agent error"),
            Err(e) => assert!(e.to_string().contains("missing-agent")),
        }
    }
}
