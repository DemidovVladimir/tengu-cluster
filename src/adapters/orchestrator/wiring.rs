//! Wiring: real `WorkerHandle` + `OrchestratorChatPort` backed by
//! a pluggable `ChatServiceFactory` (per-agent, per-call `ChatRuntimeService`
//! construction owned by the impl, which lives in `channel_runtime.rs`).

use async_trait::async_trait;
use std::sync::Arc;

use crate::adapters::memory::injector;
use crate::adapters::memory::manager::MemoryManager;
use crate::adapters::memory::writer;
use crate::adapters::orchestrator::executor::WorkerHandle;
use crate::adapters::orchestrator::plan::Step;
use crate::adapters::orchestrator::planner::OrchestratorChatPort;

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
            format!(
                "{}\n\n{}\n\nYour task:\n{}",
                mem.body, step_inputs, step.goal
            )
            .trim()
            .to_string()
        };

        let reply = self.chat.run_turn(&step.agent, &user_content).await?;
        writer::sync_turn(
            Arc::clone(&self.memory),
            step.agent.clone(),
            step.goal.clone(),
            reply.clone(),
        );
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
}

#[cfg(test)]
mod threading_tests {
    //! Contract: `ChatWorker::run_step` correctly threads `step.goal` +
    //! `step_inputs` + the memory block into the factory's `run_turn` call
    //! and posts `sync_turn` to memory after the factory returns.

    use super::*;
    use crate::adapters::memory::provider::MemoryProvider;
    use crate::adapters::orchestrator::plan::{Step, StepId};
    use async_trait::async_trait;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;
    use tokio::time::{sleep, Duration};

    /// Provider that injects a fixed `MEM_BLOCK` body into prefetch and
    /// records sync_turn calls so the test can verify memory plumbing.
    struct RecordingProvider {
        synced: Arc<Mutex<Vec<(String, String, String)>>>,
        sync_flag: Arc<AtomicBool>,
    }
    #[async_trait]
    impl MemoryProvider for RecordingProvider {
        fn name(&self) -> &str {
            "builtin"
        }
        fn is_available(&self) -> bool {
            true
        }
        async fn initialize(&self, _: &str, _: &Path) -> anyhow::Result<()> {
            Ok(())
        }
        async fn prefetch(&self, _agent: &str, _query: &str) -> String {
            "MEM_BLOCK_FACT".to_string()
        }
        async fn sync_turn(&self, agent: &str, user: &str, assistant: &str) {
            self.synced.lock().unwrap().push((
                agent.to_string(),
                user.to_string(),
                assistant.to_string(),
            ));
            self.sync_flag.store(true, Ordering::SeqCst);
        }
        async fn shutdown(&self) {}
    }

    /// Factory that records the `run_turn` arguments it receives and
    /// returns a canned reply.
    struct RecordingFactory {
        calls: Arc<Mutex<Vec<(String, String)>>>,
        reply: String,
    }
    #[async_trait]
    impl ChatServiceFactory for RecordingFactory {
        async fn run_turn(&self, agent: &str, text: &str) -> anyhow::Result<String> {
            self.calls
                .lock()
                .unwrap()
                .push((agent.to_string(), text.to_string()));
            Ok(self.reply.clone())
        }
    }

    fn make_step(goal: &str) -> Step {
        Step {
            id: StepId::new("s1"),
            agent: "researcher".to_string(),
            goal: goal.to_string(),
            depends_on: vec![],
        }
    }

    async fn wait_for_flag(flag: Arc<AtomicBool>) {
        for _ in 0..100 {
            if flag.load(Ordering::SeqCst) {
                return;
            }
            sleep(Duration::from_millis(10)).await;
        }
        panic!("sync_turn never fired");
    }

    #[tokio::test]
    async fn run_step_threads_goal_and_memory_block_into_factory() {
        let synced: Arc<Mutex<Vec<(String, String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let sync_flag = Arc::new(AtomicBool::new(false));
        let mgr = Arc::new(crate::adapters::memory::manager::MemoryManager::new());
        mgr.add_provider(Box::new(RecordingProvider {
            synced: Arc::clone(&synced),
            sync_flag: Arc::clone(&sync_flag),
        }))
        .await;

        let calls: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let factory: Arc<dyn ChatServiceFactory> = Arc::new(RecordingFactory {
            calls: Arc::clone(&calls),
            reply: "FACTORY_REPLY".to_string(),
        });

        let worker = ChatWorker::new(factory, Arc::clone(&mgr));

        let step = make_step("refine the draft");
        let reply = worker.run_step(&step, "").await.expect("run_step succeeds");
        assert_eq!(reply, "FACTORY_REPLY");

        // Factory saw the right agent + body.
        let calls = calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1);
        let (got_agent, got_text) = &calls[0];
        assert_eq!(got_agent, "researcher");
        // Memory block is injected.
        assert!(
            got_text.contains("MEM_BLOCK_FACT"),
            "expected memory body in text, got: {got_text}"
        );
        // Step goal is injected.
        assert!(
            got_text.contains("refine the draft"),
            "expected step goal in text, got: {got_text}"
        );

        // sync_turn fires after the factory returns (spawned, so wait).
        wait_for_flag(sync_flag).await;
        let synced = synced.lock().unwrap().clone();
        assert_eq!(synced.len(), 1);
        let (sync_agent, sync_user, sync_assistant) = &synced[0];
        assert_eq!(sync_agent, "researcher");
        assert_eq!(sync_user, "refine the draft");
        assert_eq!(sync_assistant, "FACTORY_REPLY");
    }

    #[tokio::test]
    async fn run_step_embeds_step_inputs_when_nonempty() {
        let mgr = Arc::new(crate::adapters::memory::manager::MemoryManager::new());
        let calls: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let factory: Arc<dyn ChatServiceFactory> = Arc::new(RecordingFactory {
            calls: Arc::clone(&calls),
            reply: "ok".to_string(),
        });
        let worker = ChatWorker::new(factory, Arc::clone(&mgr));
        let step = make_step("synthesize");
        let inputs = "<step-input from=\"s0\">\nupstream result\n</step-input>";
        let _ = worker.run_step(&step, inputs).await.expect("run_step");
        let calls = calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1);
        let (_, got_text) = &calls[0];
        assert!(got_text.contains("upstream result"));
        assert!(got_text.contains("Your task:\nsynthesize"));
    }

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

    /// Sanity: the worker propagates factory errors so the retry driver can
    /// escalate them.
    #[tokio::test]
    async fn run_step_propagates_factory_error() {
        struct FailingFactory;
        #[async_trait]
        impl ChatServiceFactory for FailingFactory {
            async fn run_turn(&self, _: &str, _: &str) -> anyhow::Result<String> {
                anyhow::bail!("factory boom")
            }
        }
        let mgr = Arc::new(crate::adapters::memory::manager::MemoryManager::new());
        let worker = ChatWorker::new(Arc::new(FailingFactory), mgr);
        let step = make_step("anything");
        let err = worker.run_step(&step, "").await.expect_err("should fail");
        assert!(err.to_string().contains("factory boom"));
    }
}
