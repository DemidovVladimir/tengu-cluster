//! Pre-turn memory injection.
//!
//! **Phase 7.1 note:** this module had two callers historically — the
//! deleted `ChatWorker::run_step` AND `ChatOrchestratorPortImpl::run_orchestrator_turn`.
//! After Phase 7.1 only the latter remains, so this is now planner-LLM-turn
//! plumbing only. Subagent turns (running in `tengu run-agent` subprocesses)
//! handle their own context assembly via `[agents.<name>].skill_packages` and do
//! NOT call this. Don't add new callers without thinking about whether
//! per-turn memory injection is actually what you want.

use crate::adapters::memory::context_block::PinnedMemoryBlock;
use crate::adapters::memory::fencing::build_memory_context_block;
use crate::adapters::memory::manager::MemoryManager;

/// Build a `PinnedMemoryBlock` for an agent's upcoming turn.
/// Caller appends this to the user-turn message of the API call.
pub async fn for_turn(mgr: &MemoryManager, agent: &str, query: &str) -> PinnedMemoryBlock {
    let raw = mgr.prefetch_all(agent, query).await;
    let body = build_memory_context_block(&raw);
    PinnedMemoryBlock { body }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::memory::provider::MemoryProvider;
    use async_trait::async_trait;
    use std::path::Path;

    struct Stub {
        body: String,
    }
    #[async_trait]
    impl MemoryProvider for Stub {
        fn name(&self) -> &str {
            "builtin"
        }
        fn is_available(&self) -> bool {
            true
        }
        async fn initialize(&self, _: &str, _: &Path) -> anyhow::Result<()> {
            Ok(())
        }
        async fn prefetch(&self, _: &str, _: &str) -> String {
            self.body.clone()
        }
        async fn sync_turn(&self, _: &str, _: &str, _: &str) {}
        async fn shutdown(&self) {}
    }

    #[tokio::test]
    async fn empty_provider_gives_empty_block() {
        let mgr = MemoryManager::new();
        mgr.add_provider(Box::new(Stub { body: "".into() })).await;
        let block = for_turn(&mgr, "a", "q").await;
        assert!(block.is_empty());
    }

    #[tokio::test]
    async fn nonempty_provider_fences_output() {
        let mgr = MemoryManager::new();
        mgr.add_provider(Box::new(Stub {
            body: "relevant fact".into(),
        }))
        .await;
        let block = for_turn(&mgr, "a", "q").await;
        assert!(block.body.starts_with("<memory-context>"));
        assert!(block.body.contains("relevant fact"));
    }
}
