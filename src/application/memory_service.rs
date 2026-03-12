//! Application service for persistent vector memory operations.
//!
//! Orchestrates the embedding → store → recall pipeline:
//!
//! **Remember**: text → `EmbeddingPort::embed()` → `Vec<f32>` →
//! `MemoryStorePort::store(MemoryEntry)` (disk or Qdrant).
//!
//! **Recall**: query text → `EmbeddingPort::embed()` → `Vec<f32>` →
//! `MemoryStorePort::search_by_vector()` → cosine-ranked results →
//! `budget_memories()` (greedy token-budget trimming).
//!
//! Both paths use the same `EmbeddingPort` instance, guaranteeing that queries
//! and stored entries share the same embedding space.

use crate::application::ports::{EmbeddingPort, MemoryStorePort};
use crate::domain::memory::{budget_memories, MemoryEntry, MemorySearchResult};
use anyhow::Result;

/// Application service orchestrating memory operations (embed, store, recall, forget).
///
/// This is a thin use-case layer that wires `EmbeddingPort` to
/// `MemoryStorePort`. It does not depend on any concrete adapter — both
/// ports are injected as trait references.
pub(crate) struct MemoryService<'a> {
    embedding: &'a dyn EmbeddingPort,
    store: &'a dyn MemoryStorePort,
}

impl<'a> MemoryService<'a> {
    pub(crate) fn new(embedding: &'a dyn EmbeddingPort, store: &'a dyn MemoryStorePort) -> Self {
        Self { embedding, store }
    }

    /// Embed content and store it as a new memory entry. Returns the entry ID.
    pub(crate) async fn remember(&self, content: &str, agent_id: &str) -> Result<String> {
        let embeddings = self.embedding.embed(&[content]).await?;
        let embedding = embeddings
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("embedding returned no vectors"))?;

        let id = uuid::Uuid::new_v4().to_string();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let entry = MemoryEntry {
            id: id.clone(),
            content: content.to_string(),
            embedding,
            agent_id: agent_id.to_string(),
            created_at_epoch_s: now,
        };

        self.store.store(&entry).await?;
        Ok(id)
    }

    /// Embed the query, search for similar memories, and budget-trim to fit token limit.
    pub(crate) async fn recall(
        &self,
        query: &str,
        top_k: usize,
        max_tokens: usize,
    ) -> Result<Vec<MemorySearchResult>> {
        let embeddings = self.embedding.embed(&[query]).await?;
        let embedding = embeddings
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("embedding returned no vectors"))?;

        let results = self.store.search_by_vector(&embedding, top_k).await?;
        let budgeted = budget_memories(&results, max_tokens);
        Ok(budgeted.into_iter().cloned().collect())
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::ports::{EmbeddingPort, MemoryStorePort};
    use anyhow::Result;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Mutex;

    struct MockEmbedding {
        vector: Vec<f32>,
    }

    impl EmbeddingPort for MockEmbedding {
        fn embed(
            &self,
            texts: &[&str],
        ) -> Pin<Box<dyn Future<Output = Result<Vec<Vec<f32>>>> + Send + '_>> {
            let results: Vec<Vec<f32>> = texts.iter().map(|_| self.vector.clone()).collect();
            Box::pin(async move { Ok(results) })
        }
    }

    struct MockStore {
        entries: Mutex<Vec<MemoryEntry>>,
    }

    impl MockStore {
        fn new() -> Self {
            Self {
                entries: Mutex::new(Vec::new()),
            }
        }
    }

    impl MemoryStorePort for MockStore {
        fn store(
            &self,
            entry: &MemoryEntry,
        ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
            let entry = entry.clone();
            Box::pin(async move {
                self.entries.lock().unwrap().push(entry);
                Ok(())
            })
        }

        fn search_by_vector(
            &self,
            _embedding: &[f32],
            top_k: usize,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<MemorySearchResult>>> + Send + '_>> {
            Box::pin(async move {
                let entries = self.entries.lock().unwrap();
                let results: Vec<MemorySearchResult> = entries
                    .iter()
                    .take(top_k)
                    .map(|e| MemorySearchResult {
                        entry: e.clone(),
                        score: 0.9,
                    })
                    .collect();
                Ok(results)
            })
        }

        fn delete(&self, id: &str) -> Pin<Box<dyn Future<Output = Result<bool>> + Send + '_>> {
            let id = id.to_string();
            Box::pin(async move {
                let mut entries = self.entries.lock().unwrap();
                let before = entries.len();
                entries.retain(|e| e.id != id);
                Ok(entries.len() < before)
            })
        }

        fn clear_all(&self) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
            Box::pin(async move {
                self.entries.lock().unwrap().clear();
                Ok(())
            })
        }

        fn entry_count(&self) -> Pin<Box<dyn Future<Output = usize> + Send + '_>> {
            Box::pin(async move { self.entries.lock().unwrap().len() })
        }

        fn storage_bytes(&self) -> Pin<Box<dyn Future<Output = u64> + Send + '_>> {
            Box::pin(async move { 0 })
        }
    }

    #[tokio::test]
    async fn remember_stores_and_returns_id() {
        let embedding = MockEmbedding {
            vector: vec![1.0, 0.0, 0.0],
        };
        let store = MockStore::new();
        let service = MemoryService::new(&embedding, &store);

        let id = service.remember("test memory", "agent1").await.unwrap();
        assert!(!id.is_empty());
        assert_eq!(store.entry_count().await, 1);
    }

    #[tokio::test]
    async fn recall_returns_budgeted_results() {
        let embedding = MockEmbedding {
            vector: vec![1.0, 0.0, 0.0],
        };
        let store = MockStore::new();
        let service = MemoryService::new(&embedding, &store);

        service.remember("first memory", "agent1").await.unwrap();
        service.remember("second memory", "agent1").await.unwrap();

        let results = service.recall("query", 5, 1000).await.unwrap();
        assert_eq!(results.len(), 2);
    }

    #[tokio::test]
    async fn forget_removes_entry() {
        let embedding = MockEmbedding {
            vector: vec![1.0, 0.0, 0.0],
        };
        let store = MockStore::new();
        let service = MemoryService::new(&embedding, &store);

        let id = service.remember("to forget", "agent1").await.unwrap();
        assert_eq!(store.entry_count().await, 1);

        let deleted = store.delete(&id).await.unwrap();
        assert!(deleted);
        assert_eq!(store.entry_count().await, 0);
    }
}
