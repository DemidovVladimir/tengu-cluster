//! `MemoryManager` — holds one built-in provider plus at most one external.

use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::warn;

use crate::domain::memory::{ChunkMetadata, MemoryHit};
use crate::ports::memory::MemoryProvider;
use crate::ports::memory::{Embedding, MemoryService, VectorStore};

pub struct MemoryManager {
    providers: Arc<RwLock<Vec<Box<dyn MemoryProvider>>>>,
    has_external: Arc<RwLock<bool>>,
    /// Shared vector backend — embedder + store. Populated alongside the
    /// builtin provider so tool-callable read/write paths (`memory_ingest`,
    /// `memory_search`, `persistent_store`) can address the same backing
    /// store without going through the provider indirection (which is
    /// shaped for pre-turn fetch / post-turn sync, not user-driven
    /// ingestion).
    vector: RwLock<Option<(Arc<dyn Embedding>, Arc<dyn VectorStore>)>>,
}

impl MemoryManager {
    pub fn new() -> Self {
        Self {
            providers: Arc::new(RwLock::new(Vec::new())),
            has_external: Arc::new(RwLock::new(false)),
            vector: RwLock::new(None),
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

    /// Register the shared vector backend used by tool-callable ingest /
    /// search paths. Call once alongside `add_provider(BuiltinMemoryProvider)`
    /// so both the provider's pre/post-turn path and the tool path address
    /// the same store.
    pub async fn set_vector_backend(
        &self,
        embedder: Arc<dyn Embedding>,
        store: Arc<dyn VectorStore>,
    ) {
        *self.vector.write().await = Some((embedder, store));
    }

    /// Returns `true` if `set_vector_backend` has installed a vector
    /// backend. Tool plugins gate tool registration on this so tools aren't
    /// advertised when the backend is unavailable.
    pub async fn has_vector_backend(&self) -> bool {
        self.vector.read().await.is_some()
    }

    /// Embed `text` and write a single entry to the shared vector backend.
    /// Returns the synthetic entry id (UUID) assigned by the store — callers
    /// can later pass this id to `VectorStore::delete`.
    ///
    /// Errors when the backend isn't installed — callers must check
    /// `has_vector_backend` first.
    pub async fn ingest_one(
        &self,
        text: &str,
        agent: &str,
        metadata: ChunkMetadata,
    ) -> anyhow::Result<String> {
        let guard = self.vector.read().await;
        let (embedder, store) = guard
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("memory manager: no vector backend registered"))?;

        let embedding = embedder.embed(text).await?;
        // Ensure the agent populates the metadata so filters still work
        // when callers forget to set it.
        let mut metadata = metadata;
        if metadata.agent.is_none() {
            metadata.agent = Some(agent.to_string());
        }
        store.write(embedding, text, metadata).await
    }

    /// Delete a single entry by its id. Returns `true` if the id existed.
    /// Errors when the backend isn't installed.
    pub async fn delete_entry(&self, id: &str) -> anyhow::Result<bool> {
        let guard = self.vector.read().await;
        let (_, store) = guard
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("memory manager: no vector backend registered"))?;
        store.delete(id).await
    }

    /// Purge the entire store.
    pub async fn clear_all(&self) -> anyhow::Result<()> {
        let guard = self.vector.read().await;
        let (_, store) = guard
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("memory manager: no vector backend registered"))?;
        store.clear_all().await
    }

    /// Returns `(entry_count, storage_bytes)` for surfacing in status UIs.
    /// `None` when no backend is installed.
    pub async fn stats(&self) -> Option<(usize, u64)> {
        let guard = self.vector.read().await;
        let (_, store) = guard.as_ref()?;
        let count = store.entry_count().await.ok()?;
        let bytes = store.storage_bytes().await.ok()?;
        Some((count, bytes))
    }

    /// Batch-ingest: embed all `texts` in a single HTTP round-trip, then
    /// write each entry. All entries share the same `agent` + `metadata`
    /// template (metadata is cloned per entry). Significantly faster than
    /// calling `ingest_one` in a loop when there are many chunks — one
    /// embedding request instead of N.
    ///
    /// Returns the count of entries successfully written. Fails fast if
    /// embedding or any individual write fails.
    pub async fn ingest_batch(
        &self,
        texts: &[&str],
        agent: &str,
        metadata: ChunkMetadata,
    ) -> anyhow::Result<usize> {
        if texts.is_empty() {
            return Ok(0);
        }
        let guard = self.vector.read().await;
        let (embedder, store) = guard
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("memory manager: no vector backend registered"))?;

        let embeddings = embedder.embed_batch(texts).await?;
        if embeddings.len() != texts.len() {
            anyhow::bail!(
                "ingest_batch: embedder returned {} vectors for {} texts",
                embeddings.len(),
                texts.len()
            );
        }

        let mut template = metadata;
        if template.agent.is_none() {
            template.agent = Some(agent.to_string());
        }

        let mut written = 0usize;
        for (text, embedding) in texts.iter().zip(embeddings.into_iter()) {
            // Ingest is write-only at the batch level; individual ids are
            // not surfaced back (caller got a count, not a list of ids).
            store.write(embedding, text, template.clone()).await?;
            written += 1;
        }
        Ok(written)
    }

    /// Embed `query` and run a `top_k` vector search over the shared
    /// backend. Mirrors `VectorStore::search` directly; callers supply
    /// their own metadata filter when needed.
    pub async fn search(
        &self,
        query: &str,
        top_k: usize,
        filter: Option<&ChunkMetadata>,
    ) -> anyhow::Result<Vec<MemoryHit>> {
        let guard = self.vector.read().await;
        let (embedder, store) = guard
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("memory manager: no vector backend registered"))?;

        let embedding = embedder.embed(query).await?;
        store.search(&embedding, top_k, filter).await
    }

    /// Returns the list of provider names currently registered, in
    /// registration order (builtin first).
    #[allow(dead_code)]
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

    #[allow(dead_code)]
    pub async fn shutdown_all(&self) {
        let providers = self.providers.read().await;
        for p in providers.iter().rev() {
            p.shutdown().await;
        }
    }
}

#[async_trait::async_trait]
impl MemoryService for MemoryManager {
    async fn ingest_one(
        &self,
        text: &str,
        agent: &str,
        metadata: ChunkMetadata,
    ) -> anyhow::Result<String> {
        MemoryManager::ingest_one(self, text, agent, metadata).await
    }
    async fn ingest_batch(
        &self,
        texts: &[&str],
        agent: &str,
        metadata: ChunkMetadata,
    ) -> anyhow::Result<usize> {
        MemoryManager::ingest_batch(self, texts, agent, metadata).await
    }
    async fn search(
        &self,
        query: &str,
        top_k: usize,
        filter: Option<&ChunkMetadata>,
    ) -> anyhow::Result<Vec<MemoryHit>> {
        MemoryManager::search(self, query, top_k, filter).await
    }
    async fn delete_entry(&self, id: &str) -> anyhow::Result<bool> {
        MemoryManager::delete_entry(self, id).await
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
