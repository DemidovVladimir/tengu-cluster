//! Unified memory subsystem: types, disk store, and service.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use async_trait::async_trait;

use crate::adapters::ports::{EmbeddingPort, MemoryStorePort};
use crate::adapters::types::{MemoryEntry, MemorySearchResult};

// ---------------------------------------------------------------------------
// Pure functions
// ---------------------------------------------------------------------------

pub(crate) fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }

    let mut dot = 0.0_f64;
    let mut norm_a = 0.0_f64;
    let mut norm_b = 0.0_f64;

    for (x, y) in a.iter().zip(b.iter()) {
        let x = *x as f64;
        let y = *y as f64;
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }

    let denom = (norm_a.sqrt() * norm_b.sqrt()) as f64;
    if denom < 1e-12 {
        return 0.0;
    }

    (dot / denom) as f32
}

pub(crate) fn budget_memories(
    results: &[MemorySearchResult],
    max_tokens: usize,
) -> Vec<&MemorySearchResult> {
    let mut selected = Vec::new();
    let mut used_tokens = 0usize;

    for result in results {
        let entry_tokens = (result.entry.content.len() + 3) / 4;
        if used_tokens + entry_tokens > max_tokens {
            break;
        }
        used_tokens += entry_tokens;
        selected.push(result);
    }

    selected
}

// ---------------------------------------------------------------------------
// MemoryService
// ---------------------------------------------------------------------------

pub(crate) struct MemoryService<'a> {
    embedding: &'a dyn EmbeddingPort,
    store: &'a dyn MemoryStorePort,
}

impl<'a> MemoryService<'a> {
    pub(crate) fn new(embedding: &'a dyn EmbeddingPort, store: &'a dyn MemoryStorePort) -> Self {
        Self { embedding, store }
    }

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

    pub(crate) async fn recall(
        &self,
        query: &str,
        top_k: usize,
        max_tokens: usize,
    ) -> Result<Vec<MemorySearchResult>> {
        self.recall_filtered(query, top_k, max_tokens, &HashMap::new())
            .await
    }

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

// ---------------------------------------------------------------------------
// DiskVectorMemoryStore
// ---------------------------------------------------------------------------

pub(crate) struct DiskVectorMemoryStore {
    entries: RwLock<Vec<MemoryEntry>>,
    store_path: PathBuf,
}

impl DiskVectorMemoryStore {
    pub(crate) fn new(store_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(store_dir).with_context(|| {
            format!("failed to create memory store dir: {}", store_dir.display())
        })?;

        let store_path = store_dir.join("vectors.bin");
        let entries = if store_path.exists() {
            let data = std::fs::read(&store_path)
                .with_context(|| format!("failed to read {}", store_path.display()))?;
            bincode::deserialize(&data).unwrap_or_else(|e| {
                tracing::warn!(error = %e, "Corrupted memory store, starting fresh");
                Vec::new()
            })
        } else {
            Vec::new()
        };

        tracing::info!(
            entries = entries.len(),
            path = %store_path.display(),
            "Memory store loaded"
        );

        Ok(Self {
            entries: RwLock::new(entries),
            store_path,
        })
    }

    fn flush(&self, entries: &[MemoryEntry]) -> Result<()> {
        let data = bincode::serialize(entries).context("failed to serialize memory store")?;
        let tmp_path = self.store_path.with_extension("bin.tmp");
        std::fs::write(&tmp_path, &data)
            .with_context(|| format!("failed to write {}", tmp_path.display()))?;
        std::fs::rename(&tmp_path, &self.store_path)
            .with_context(|| format!("failed to rename to {}", self.store_path.display()))?;
        Ok(())
    }
}

#[async_trait]
impl MemoryStorePort for DiskVectorMemoryStore {
    async fn store(&self, entry: &MemoryEntry) -> Result<()> {
        let mut entries = self
            .entries
            .write()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {}", e))?;
        entries.push(entry.clone());
        self.flush(&entries)
    }

    async fn search_by_vector(
        &self,
        embedding: &[f32],
        top_k: usize,
    ) -> Result<Vec<MemorySearchResult>> {
        let entries = self
            .entries
            .read()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {}", e))?;

        let mut scored: Vec<MemorySearchResult> = entries
            .iter()
            .map(|entry| {
                let score = cosine_similarity(&entry.embedding, embedding);
                MemorySearchResult {
                    entry: entry.clone(),
                    score,
                }
            })
            .collect();

        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(top_k);
        Ok(scored)
    }

    async fn delete(&self, id: &str) -> Result<bool> {
        let mut entries = self
            .entries
            .write()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {}", e))?;
        let before = entries.len();
        entries.retain(|e| e.id != id);
        let deleted = entries.len() < before;
        if deleted {
            self.flush(&entries)?;
        }
        Ok(deleted)
    }

    async fn clear_all(&self) -> Result<()> {
        let mut entries = self
            .entries
            .write()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {}", e))?;
        entries.clear();
        self.flush(&entries)
    }

    async fn entry_count(&self) -> usize {
        self.entries.read().map(|e| e.len()).unwrap_or(0)
    }

    async fn storage_bytes(&self) -> u64 {
        std::fs::metadata(&self.store_path)
            .map(|m| m.len())
            .unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// MemoryServiceHandle — shared handle used by the memory plugin and any caller
// that needs direct access to the embedding + store ports.
// ---------------------------------------------------------------------------

pub(crate) struct MemoryServiceHandle {
    pub embedding: Arc<dyn EmbeddingPort>,
    pub store: Arc<dyn MemoryStorePort>,
}
