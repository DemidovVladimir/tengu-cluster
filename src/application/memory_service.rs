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

use std::collections::HashMap;

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
    #[allow(dead_code)]
    pub(crate) async fn remember(&self, content: &str, agent_id: &str) -> Result<String> {
        self.remember_with_metadata(content, agent_id, HashMap::new())
            .await
    }

    /// Embed content and store it with metadata tags. Returns the entry ID.
    pub(crate) async fn remember_with_metadata(
        &self,
        content: &str,
        agent_id: &str,
        metadata: HashMap<String, String>,
    ) -> Result<String> {
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
            metadata,
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
        self.recall_filtered(query, top_k, max_tokens, &HashMap::new())
            .await
    }

    /// Like `recall`, but only returns entries whose metadata contains all
    /// key-value pairs in `required_metadata`.
    ///
    /// Fetches extra candidates (3× top_k) to compensate for post-filter loss.
    pub(crate) async fn recall_filtered(
        &self,
        query: &str,
        top_k: usize,
        max_tokens: usize,
        required_metadata: &HashMap<String, String>,
    ) -> Result<Vec<MemorySearchResult>> {
        let embeddings = self.embedding.embed(&[query]).await?;
        let embedding = embeddings
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("embedding returned no vectors"))?;

        let fetch_k = if required_metadata.is_empty() {
            top_k
        } else {
            top_k * 3
        };
        let results = self.store.search_by_vector(&embedding, fetch_k).await?;

        let filtered: Vec<MemorySearchResult> = if required_metadata.is_empty() {
            results
        } else {
            results
                .into_iter()
                .filter(|r| {
                    required_metadata
                        .iter()
                        .all(|(k, v)| r.entry.metadata.get(k).map(|mv| mv == v).unwrap_or(false))
                })
                .collect()
        };

        let budgeted = budget_memories(&filtered, max_tokens);
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
    async fn remember_with_metadata_stores_metadata() {
        let embedding = MockEmbedding {
            vector: vec![1.0, 0.0, 0.0],
        };
        let store = MockStore::new();
        let service = MemoryService::new(&embedding, &store);

        let mut meta = HashMap::new();
        meta.insert("kind".into(), "topic_overview".into());
        meta.insert("source".into(), "orchestrator".into());

        let id = service
            .remember_with_metadata("test with metadata", "agent1", meta)
            .await
            .unwrap();
        assert!(!id.is_empty());

        let entries = store.entries.lock().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].metadata.get("kind").map(|s| s.as_str()),
            Some("topic_overview")
        );
        assert_eq!(
            entries[0].metadata.get("source").map(|s| s.as_str()),
            Some("orchestrator")
        );
    }

    #[tokio::test]
    async fn recall_filtered_only_topic_overviews() {
        let embedding = MockEmbedding {
            vector: vec![1.0, 0.0, 0.0],
        };
        let store = MockStore::new();
        let service = MemoryService::new(&embedding, &store);

        // Store a topic_overview from orchestrator
        let mut meta_overview = HashMap::new();
        meta_overview.insert("kind".into(), "topic_overview".into());
        meta_overview.insert("source".into(), "orchestrator".into());
        service
            .remember_with_metadata("DeSci pipeline completed", "orchestrator", meta_overview)
            .await
            .unwrap();

        // Store a plain user memory (no kind/source)
        service
            .remember("user preference: dark mode", "agent1")
            .await
            .unwrap();

        // Store a fact from an agent (wrong kind)
        let mut meta_fact = HashMap::new();
        meta_fact.insert("kind".into(), "fact".into());
        meta_fact.insert("source".into(), "agent".into());
        service
            .remember_with_metadata("API key rotated", "agent2", meta_fact)
            .await
            .unwrap();

        // Unfiltered recall returns all 3
        let all = service.recall("query", 10, 10000).await.unwrap();
        assert_eq!(all.len(), 3);

        // Filtered: only kind=topic_overview + source=orchestrator
        let mut filter = HashMap::new();
        filter.insert("kind".into(), "topic_overview".into());
        filter.insert("source".into(), "orchestrator".into());
        let filtered = service
            .recall_filtered("query", 10, 10000, &filter)
            .await
            .unwrap();
        assert_eq!(filtered.len(), 1);
        assert!(filtered[0].entry.content.contains("DeSci pipeline"));
    }

    #[tokio::test]
    async fn recall_filtered_excludes_wrong_workspace() {
        let embedding = MockEmbedding {
            vector: vec![1.0, 0.0, 0.0],
        };
        let store = MockStore::new();
        let service = MemoryService::new(&embedding, &store);

        // Memory from desci workspace
        let mut meta_desci = HashMap::new();
        meta_desci.insert("kind".into(), "topic_overview".into());
        meta_desci.insert("source".into(), "orchestrator".into());
        meta_desci.insert("workspace_id".into(), "desci-sandbox".into());
        service
            .remember_with_metadata("DeSci result", "orchestrator", meta_desci)
            .await
            .unwrap();

        // Memory from webstudio workspace
        let mut meta_web = HashMap::new();
        meta_web.insert("kind".into(), "topic_overview".into());
        meta_web.insert("source".into(), "orchestrator".into());
        meta_web.insert("workspace_id".into(), "webstudio".into());
        service
            .remember_with_metadata("WebStudio result", "orchestrator", meta_web)
            .await
            .unwrap();

        // Filter for desci-sandbox only
        let mut filter = HashMap::new();
        filter.insert("kind".into(), "topic_overview".into());
        filter.insert("workspace_id".into(), "desci-sandbox".into());
        let results = service
            .recall_filtered("query", 10, 10000, &filter)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].entry.content.contains("DeSci"));
    }

    #[tokio::test]
    async fn recall_filtered_excludes_non_orchestrator() {
        let embedding = MockEmbedding {
            vector: vec![1.0, 0.0, 0.0],
        };
        let store = MockStore::new();
        let service = MemoryService::new(&embedding, &store);

        // topic_overview from orchestrator
        let mut meta_orch = HashMap::new();
        meta_orch.insert("kind".into(), "topic_overview".into());
        meta_orch.insert("source".into(), "orchestrator".into());
        service
            .remember_with_metadata("orchestrator summary", "orchestrator", meta_orch)
            .await
            .unwrap();

        // topic_overview from agent (shouldn't match planner filter)
        let mut meta_agent = HashMap::new();
        meta_agent.insert("kind".into(), "topic_overview".into());
        meta_agent.insert("source".into(), "agent".into());
        service
            .remember_with_metadata("agent summary", "agent1", meta_agent)
            .await
            .unwrap();

        let mut filter = HashMap::new();
        filter.insert("kind".into(), "topic_overview".into());
        filter.insert("source".into(), "orchestrator".into());
        let results = service
            .recall_filtered("query", 10, 10000, &filter)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].entry.content.contains("orchestrator summary"));
    }

    #[tokio::test]
    async fn recall_filtered_empty_filter_returns_all() {
        let embedding = MockEmbedding {
            vector: vec![1.0, 0.0, 0.0],
        };
        let store = MockStore::new();
        let service = MemoryService::new(&embedding, &store);

        service.remember("mem1", "a").await.unwrap();
        service.remember("mem2", "a").await.unwrap();

        let results = service
            .recall_filtered("query", 10, 10000, &HashMap::new())
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
    }

    /// Integration-style test: simulates the orchestrator auto-summarize + planner
    /// recall flow. Creates mixed memories, verifies only topic overviews appear.
    #[tokio::test]
    async fn orchestrator_memory_round_trip() {
        let embedding = MockEmbedding {
            vector: vec![1.0, 0.0, 0.0],
        };
        let store = MockStore::new();
        let service = MemoryService::new(&embedding, &store);

        // Simulate: user remembers something via remember tool (no special metadata)
        service.remember("user fact", "default").await.unwrap();

        // Simulate: agent remembers a fact with metadata
        let mut agent_meta = HashMap::new();
        agent_meta.insert("kind".into(), "fact".into());
        agent_meta.insert("source".into(), "agent".into());
        service
            .remember_with_metadata("agent discovered X", "researcher", agent_meta)
            .await
            .unwrap();

        // Simulate: orchestrator auto-summarize after task completion
        let mut orch_meta = HashMap::new();
        orch_meta.insert("kind".into(), "topic_overview".into());
        orch_meta.insert("source".into(), "orchestrator".into());
        orch_meta.insert("goal".into(), "mint IP-NFT".into());
        orch_meta.insert("workspace_id".into(), "desci-sandbox".into());
        service
            .remember_with_metadata(
                "Goal: mint IP-NFT\n\nResults:\n- researcher (ok): found paper\n- minter (ok): minted NFT #42",
                "orchestrator",
                orch_meta,
            )
            .await
            .unwrap();

        // Planner recall with orchestrator filter
        let mut planner_filter = HashMap::new();
        planner_filter.insert("kind".into(), "topic_overview".into());
        planner_filter.insert("source".into(), "orchestrator".into());
        let planner_context = service
            .recall_filtered("mint an IP-NFT", 3, 600, &planner_filter)
            .await
            .unwrap();

        // Only the orchestrator topic_overview should appear
        assert_eq!(planner_context.len(), 1);
        assert!(planner_context[0].entry.content.contains("mint IP-NFT"));
        assert_eq!(
            planner_context[0]
                .entry
                .metadata
                .get("kind")
                .map(|s| s.as_str()),
            Some("topic_overview")
        );
        assert_eq!(
            planner_context[0]
                .entry
                .metadata
                .get("workspace_id")
                .map(|s| s.as_str()),
            Some("desci-sandbox")
        );
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
