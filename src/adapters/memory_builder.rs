//! Unified memory subsystem: types, disk store, service, and tool executor.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use serde_json::json;

use crate::adapters::ports::{EmbeddingPort, MemoryStorePort, ToolExecutionPort};
use crate::adapters::secret_builder::SecretRegistry;
use crate::adapters::types::{
    MemoryEntry, MemorySearchResult, ToolCall, ToolDef,
};

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

impl MemoryStorePort for DiskVectorMemoryStore {
    fn store(&self, entry: &MemoryEntry) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
        let entry = entry.clone();
        Box::pin(async move {
            let mut entries = self
                .entries
                .write()
                .map_err(|e| anyhow::anyhow!("lock poisoned: {}", e))?;
            entries.push(entry);
            self.flush(&entries)
        })
    }

    fn search_by_vector(
        &self,
        embedding: &[f32],
        top_k: usize,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<MemorySearchResult>>> + Send + '_>> {
        let embedding = embedding.to_vec();
        Box::pin(async move {
            let entries = self
                .entries
                .read()
                .map_err(|e| anyhow::anyhow!("lock poisoned: {}", e))?;

            let mut scored: Vec<MemorySearchResult> = entries
                .iter()
                .map(|entry| {
                    let score = cosine_similarity(&entry.embedding, &embedding);
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
        })
    }

    fn delete(&self, id: &str) -> Pin<Box<dyn Future<Output = Result<bool>> + Send + '_>> {
        let id = id.to_string();
        Box::pin(async move {
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
        })
    }

    fn clear_all(&self) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
        Box::pin(async move {
            let mut entries = self
                .entries
                .write()
                .map_err(|e| anyhow::anyhow!("lock poisoned: {}", e))?;
            entries.clear();
            self.flush(&entries)
        })
    }

    fn entry_count(&self) -> Pin<Box<dyn Future<Output = usize> + Send + '_>> {
        Box::pin(async move { self.entries.read().map(|e| e.len()).unwrap_or(0) })
    }

    fn storage_bytes(&self) -> Pin<Box<dyn Future<Output = u64> + Send + '_>> {
        Box::pin(async move {
            std::fs::metadata(&self.store_path)
                .map(|m| m.len())
                .unwrap_or(0)
        })
    }
}

// ---------------------------------------------------------------------------
// Tool executor adapter
// ---------------------------------------------------------------------------

pub(crate) struct MemoryServiceHandle {
    pub embedding: Arc<dyn EmbeddingPort>,
    pub store: Arc<dyn MemoryStorePort>,
}

pub(crate) struct MemoryToolExecutionAdapter {
    handle: Arc<MemoryServiceHandle>,
    secret_registry: Arc<SecretRegistry>,
    fallback_runtime: Option<tokio::runtime::Runtime>,
}

impl MemoryToolExecutionAdapter {
    pub(crate) fn new(
        handle: Arc<MemoryServiceHandle>,
        secret_registry: Arc<SecretRegistry>,
    ) -> Result<Self> {
        let fallback_runtime = if tokio::runtime::Handle::try_current().is_ok() {
            None
        } else {
            Some(
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?,
            )
        };
        Ok(Self {
            handle,
            secret_registry,
            fallback_runtime,
        })
    }

    fn run_async<F, T>(&self, future: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            tokio::task::block_in_place(|| handle.block_on(future))
        } else {
            self.fallback_runtime
                .as_ref()
                .expect("no tokio runtime available")
                .block_on(future)
        }
    }
}

pub(crate) fn memory_tool_defs() -> Vec<ToolDef> {
    vec![ToolDef::new(
        "remember",
        "Store a fact in long-term memory.",
        json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "The fact, insight, or information to remember"
                },
                "metadata": {
                    "type": "object",
                    "description": "Optional key-value tags for the memory (e.g. {\"kind\": \"fact\", \"topic\": \"auth\"})",
                    "additionalProperties": { "type": "string" }
                }
            },
            "required": ["content"]
        }),
    )]
}

impl ToolExecutionPort for MemoryToolExecutionAdapter {
    fn execute_tool(&self, call: &ToolCall) -> Result<String> {
        match call.name.as_str() {
            "remember" => {
                let raw_content = call
                    .arguments
                    .get("content")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("remember: missing 'content' argument"))?;
                let content = self.secret_registry.redact(raw_content);
                let content = content.as_str();

                let agent_id = call
                    .arguments
                    .get("agent_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("default");

                let metadata: HashMap<String, String> = call
                    .arguments
                    .get("metadata")
                    .and_then(|v| v.as_object())
                    .map(|obj| {
                        obj.iter()
                            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                            .collect()
                    })
                    .unwrap_or_default();

                let service =
                    MemoryService::new(self.handle.embedding.as_ref(), self.handle.store.as_ref());

                let id =
                    self.run_async(service.remember_with_metadata(content, agent_id, metadata))?;

                Ok(format!("Stored memory with id: {}", id))
            }
            other => Err(anyhow::anyhow!("unknown memory tool: {}", other)),
        }
    }
}
