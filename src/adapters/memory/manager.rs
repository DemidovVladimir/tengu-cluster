//! `MemoryManager` — holds one built-in provider plus at most one external.

use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::warn;

use crate::adapters::memory::provider::MemoryProvider;

pub struct MemoryManager {
    providers: Arc<RwLock<Vec<Box<dyn MemoryProvider>>>>,
    has_external: Arc<RwLock<bool>>,
}

impl MemoryManager {
    pub fn new() -> Self {
        Self {
            providers: Arc::new(RwLock::new(Vec::new())),
            has_external: Arc::new(RwLock::new(false)),
        }
    }

    pub async fn add_provider(&self, provider: Box<dyn MemoryProvider>) {
        let is_builtin = provider.name() == "builtin";
        if !is_builtin {
            let mut flag = self.has_external.write().await;
            if *flag {
                warn!(
                    provider = provider.name(),
                    "rejected — an external memory provider is already registered"
                );
                return;
            }
            *flag = true;
        }
        self.providers.write().await.push(provider);
    }

    /// Returns the list of provider names currently registered, in
    /// registration order (builtin first).
    pub async fn providers(&self) -> Vec<String> {
        self.providers
            .read()
            .await
            .iter()
            .map(|p| p.name().to_string())
            .collect()
    }

    pub async fn prefetch_all(&self, agent: &str, query: &str) -> String {
        let providers = self.providers.read().await;
        let mut parts = Vec::new();
        for p in providers.iter() {
            let out = p.prefetch(agent, query).await;
            if !out.trim().is_empty() {
                parts.push(out);
            }
        }
        parts.join("\n\n")
    }

    pub async fn sync_all(&self, agent: &str, user: &str, assistant: &str) {
        let providers = self.providers.read().await;
        for p in providers.iter() {
            p.sync_turn(agent, user, assistant).await;
        }
    }

    pub async fn shutdown_all(&self) {
        let providers = self.providers.read().await;
        for p in providers.iter().rev() {
            p.shutdown().await;
        }
    }
}

impl Default for MemoryManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::path::Path;

    struct FakeProvider {
        name: String,
    }
    #[async_trait]
    impl MemoryProvider for FakeProvider {
        fn name(&self) -> &str {
            &self.name
        }
        fn is_available(&self) -> bool {
            true
        }
        async fn initialize(&self, _: &str, _: &Path) -> anyhow::Result<()> {
            Ok(())
        }
        async fn prefetch(&self, _: &str, q: &str) -> String {
            format!("from-{}: {}", self.name, q)
        }
        async fn sync_turn(&self, _: &str, _: &str, _: &str) {}
        async fn shutdown(&self) {}
    }

    #[tokio::test]
    async fn builtin_registers_first() {
        let mgr = MemoryManager::new();
        mgr.add_provider(Box::new(FakeProvider {
            name: "builtin".into(),
        }))
        .await;
        assert_eq!(mgr.providers().await, vec!["builtin".to_string()]);
    }

    #[tokio::test]
    async fn at_most_one_external() {
        let mgr = MemoryManager::new();
        mgr.add_provider(Box::new(FakeProvider {
            name: "builtin".into(),
        }))
        .await;
        mgr.add_provider(Box::new(FakeProvider {
            name: "letta".into(),
        }))
        .await;
        mgr.add_provider(Box::new(FakeProvider {
            name: "mem0".into(),
        }))
        .await; // rejected
        assert_eq!(
            mgr.providers().await,
            vec!["builtin".to_string(), "letta".to_string()]
        );
    }

    #[tokio::test]
    async fn prefetch_concatenates_providers() {
        let mgr = MemoryManager::new();
        mgr.add_provider(Box::new(FakeProvider {
            name: "builtin".into(),
        }))
        .await;
        mgr.add_provider(Box::new(FakeProvider {
            name: "letta".into(),
        }))
        .await;
        let out = mgr.prefetch_all("researcher", "q").await;
        assert!(out.contains("from-builtin"));
        assert!(out.contains("from-letta"));
    }
}
