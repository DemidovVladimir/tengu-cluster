//! Post-turn memory writes — spawned, non-blocking.

use std::sync::Arc;
use crate::adapters::memory::manager::MemoryManager;

pub fn sync_turn(mgr: Arc<MemoryManager>, agent: String, user: String, assistant: String) {
    tokio::spawn(async move {
        mgr.sync_all(&agent, &user, &assistant).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::memory::provider::MemoryProvider;
    use async_trait::async_trait;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::time::{sleep, Duration};

    struct Slow { done: Arc<AtomicBool> }
    #[async_trait]
    impl MemoryProvider for Slow {
        fn name(&self) -> &str { "builtin" }
        fn is_available(&self) -> bool { true }
        async fn initialize(&self, _: &str, _: &Path) -> anyhow::Result<()> { Ok(()) }
        async fn prefetch(&self, _: &str, _: &str) -> String { String::new() }
        async fn sync_turn(&self, _: &str, _: &str, _: &str) {
            sleep(Duration::from_millis(100)).await;
            self.done.store(true, Ordering::SeqCst);
        }
        async fn shutdown(&self) {}
    }

    #[tokio::test]
    async fn sync_turn_returns_before_write_completes() {
        let done = Arc::new(AtomicBool::new(false));
        let mgr = Arc::new(MemoryManager::new());
        mgr.add_provider(Box::new(Slow { done: done.clone() })).await;
        let t0 = std::time::Instant::now();
        sync_turn(mgr, "a".into(), "q".into(), "r".into());
        let elapsed = t0.elapsed();
        assert!(elapsed < Duration::from_millis(20), "sync_turn should return immediately");
        // give the spawned task time
        sleep(Duration::from_millis(200)).await;
        assert!(done.load(Ordering::SeqCst), "spawned task should have run");
    }
}
