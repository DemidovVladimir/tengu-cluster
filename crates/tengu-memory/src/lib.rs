use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tengu_core::Lens;

/// A single entry in the knowledge store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeEntry {
    pub source: PathBuf,
    pub full_content: String,
    pub full_token_estimate: u32,
    pub summary: Option<String>,
    pub summary_token_estimate: Option<u32>,
    pub content_hash: u64,
    pub last_indexed: chrono::DateTime<chrono::Utc>,
}

/// Result of a knowledge query.
#[derive(Debug, Clone)]
pub struct RetrievedKnowledge {
    pub source: PathBuf,
    pub content: String,
    pub is_summary: bool,
    pub score: f32,
}

/// The knowledge store — indexes workspace files for retrieval.
pub struct KnowledgeStore {
    entries: Vec<KnowledgeEntry>,
    workspace: PathBuf,
}

impl KnowledgeStore {
    pub fn new(workspace: PathBuf) -> Self {
        Self {
            entries: Vec::new(),
            workspace,
        }
    }

    /// Ingest a file into the store.
    pub async fn ingest(
        &mut self,
        path: &std::path::Path,
        refiner: &dyn tengu_core::Refiner,
    ) -> anyhow::Result<()> {
        let full_path = self.workspace.join(path);
        let content = tokio::fs::read_to_string(&full_path).await?;

        let hash = Self::hash_content(&content);

        // Check if already indexed and unchanged
        if let Some(existing) = self.entries.iter().find(|e| e.source == path) {
            if existing.content_hash == hash {
                return Ok(());
            }
        }

        // Generate summary if refiner supports it
        let (summary, summary_tokens) = if refiner.memory_footprint() > 0 || !content.is_empty() {
            let s = refiner.summarize(&content, 100).await?;
            let tokens = (s.len() / 4) as u32;
            (Some(s), Some(tokens))
        } else {
            (None, None)
        };

        let entry = KnowledgeEntry {
            source: path.to_path_buf(),
            full_content: content.clone(),
            full_token_estimate: (content.len() / 4) as u32,
            summary,
            summary_token_estimate: summary_tokens,
            content_hash: hash,
            last_indexed: chrono::Utc::now(),
        };

        // Upsert
        self.entries.retain(|e| e.source != path);
        self.entries.push(entry);

        Ok(())
    }

    /// Query the store respecting the user's lens setting.
    pub fn query(&self, _query: &str, lens: Lens, max_results: usize) -> Vec<RetrievedKnowledge> {
        // Phase 1: simple keyword matching.
        // Phase 5 will add vector search.
        let mut results: Vec<RetrievedKnowledge> = self
            .entries
            .iter()
            .map(|entry| {
                let (content, is_summary) = match lens {
                    Lens::Eco => {
                        if let Some(ref s) = entry.summary {
                            (s.clone(), true)
                        } else {
                            (entry.full_content.clone(), false)
                        }
                    }
                    Lens::Standard => {
                        // For now, return summary. Phase 5 adds confidence-based expansion.
                        if let Some(ref s) = entry.summary {
                            (s.clone(), true)
                        } else {
                            (entry.full_content.clone(), false)
                        }
                    }
                    Lens::Precise => (entry.full_content.clone(), false),
                };

                RetrievedKnowledge {
                    source: entry.source.clone(),
                    content,
                    is_summary,
                    score: 1.0, // Placeholder until vector search
                }
            })
            .collect();

        results.truncate(max_results);
        results
    }

    /// Load workspace files matching glob patterns.
    pub async fn ingest_patterns(
        &mut self,
        patterns: &[String],
        refiner: &dyn tengu_core::Refiner,
    ) -> anyhow::Result<usize> {
        let mut count = 0;

        for pattern in patterns {
            let full_pattern = self.workspace.join(pattern);
            let pattern_str = full_pattern.to_string_lossy().to_string();

            for entry in glob::glob(&pattern_str).unwrap_or_else(|_| glob::glob("").unwrap()) {
                if let Ok(path) = entry {
                    if path.is_file() {
                        let relative = path.strip_prefix(&self.workspace).unwrap_or(&path);
                        if self.ingest(relative, refiner).await.is_ok() {
                            count += 1;
                        }
                    }
                }
            }
        }

        Ok(count)
    }

    fn hash_content(content: &str) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        content.hash(&mut hasher);
        hasher.finish()
    }
}
