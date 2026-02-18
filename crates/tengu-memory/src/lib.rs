//! Workspace knowledge ingestion and budget-aware retrieval.
//!
//! Potential use case:
//! Index project docs on startup and retrieve only token-bounded relevant snippets per user turn.

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::path::PathBuf;
use tengu_core::token::estimate_tokens_approx_u32;
use tengu_core::Lens;

/// Indexed knowledge entry persisted in memory for retrieval.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeEntry {
    /// Workspace-relative source path.
    pub source: PathBuf,
    /// Full file content.
    pub full_content: String,
    /// Approximate token count for full content.
    pub full_token_estimate: u32,
    /// Optional summarized version used by lower-cost lenses.
    pub summary: Option<String>,
    /// Approximate token count for summary.
    pub summary_token_estimate: Option<u32>,
    /// Content hash used for change detection.
    pub content_hash: u64,
    /// Last indexing timestamp.
    pub last_indexed: chrono::DateTime<chrono::Utc>,
}

/// Retrieval hit selected for prompt assembly.
#[derive(Debug, Clone)]
pub struct RetrievedKnowledge {
    /// Workspace-relative source path.
    pub source: PathBuf,
    /// Selected content (summary or full text).
    pub content: String,
    /// Whether `content` is a summary.
    pub is_summary: bool,
    /// Relevance score used for ranking.
    pub score: f32,
}

/// In-memory knowledge store scoped to a workspace root.
pub struct KnowledgeStore {
    entries: Vec<KnowledgeEntry>,
    workspace: PathBuf,
}

impl KnowledgeStore {
    /// Create an empty store for the given workspace.
    pub fn new(workspace: PathBuf) -> Self {
        Self {
            entries: Vec::new(),
            workspace,
        }
    }

    /// Ingest a file from workspace and upsert its indexed representation.
    pub async fn ingest(
        &mut self,
        path: &std::path::Path,
        refiner: &dyn tengu_core::Refiner,
    ) -> anyhow::Result<()> {
        let full_path = self.workspace.join(path);
        let content = tokio::fs::read_to_string(&full_path).await?;

        let hash = Self::hash_content(&content);

        if let Some(existing) = self.entries.iter().find(|e| e.source == path) {
            if existing.content_hash == hash {
                return Ok(());
            }
        }

        let (summary, summary_tokens) = if refiner.memory_footprint() > 0 || !content.is_empty() {
            let s = refiner.summarize(&content, 100).await?;
            let tokens = estimate_tokens_approx_u32(&s);
            (Some(s), Some(tokens))
        } else {
            (None, None)
        };

        let entry = KnowledgeEntry {
            source: path.to_path_buf(),
            full_content: content.clone(),
            full_token_estimate: estimate_tokens_approx_u32(&content),
            summary,
            summary_token_estimate: summary_tokens,
            content_hash: hash,
            last_indexed: chrono::Utc::now(),
        };

        // Upsert.
        self.entries.retain(|e| e.source != path);
        self.entries.push(entry);

        Ok(())
    }

    /// Query best matching entries using lightweight lexical scoring.
    pub fn query(&self, query: &str, lens: Lens, max_results: usize) -> Vec<RetrievedKnowledge> {
        if self.entries.is_empty() || max_results == 0 {
            return Vec::new();
        }

        let normalized_query = query.trim().to_lowercase();
        let tokens = Self::tokenize_query(&normalized_query);

        let mut scored: Vec<(f32, usize)> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(idx, entry)| {
                let score = Self::score_entry(entry, &normalized_query, &tokens);
                if !normalized_query.is_empty() && score <= 0.0 {
                    None
                } else {
                    Some((score, idx))
                }
            })
            .collect();

        scored.sort_by(|(score_a, idx_a), (score_b, idx_b)| {
            score_b
                .partial_cmp(score_a)
                .unwrap_or(Ordering::Equal)
                .then_with(|| {
                    self.entries[*idx_b]
                        .last_indexed
                        .cmp(&self.entries[*idx_a].last_indexed)
                })
        });

        scored
            .into_iter()
            .take(max_results)
            .map(|(score, idx)| {
                let entry = &self.entries[idx];
                let (content, is_summary) = Self::content_for_lens(entry, lens);
                RetrievedKnowledge {
                    source: entry.source.clone(),
                    content,
                    is_summary,
                    score,
                }
            })
            .collect()
    }

    /// Query and pack retrieval results under a hard token budget.
    pub fn query_with_budget(
        &self,
        query: &str,
        lens: Lens,
        max_results: usize,
        max_tokens: u32,
    ) -> Vec<RetrievedKnowledge> {
        let candidates = self.query(query, lens, max_results);
        if max_tokens == 0 {
            return Vec::new();
        }

        let mut out = Vec::new();
        let mut used: u32 = 0;

        for item in candidates {
            let est = estimate_tokens_approx_u32(&item.content);
            if est > max_tokens {
                continue;
            }

            if used.saturating_add(est) > max_tokens {
                continue;
            }

            used = used.saturating_add(est);
            out.push(item);
        }

        out
    }

    /// Ingest all files matching provided glob patterns.
    pub async fn ingest_patterns(
        &mut self,
        patterns: &[String],
        refiner: &dyn tengu_core::Refiner,
    ) -> anyhow::Result<usize> {
        let mut count = 0;

        for pattern in patterns {
            let full_pattern = self.workspace.join(pattern);
            let pattern_str = full_pattern.to_string_lossy();
            let entries = match glob::glob(&pattern_str) {
                Ok(entries) => entries,
                Err(_) => continue,
            };

            for path in entries.flatten() {
                if path.is_file() {
                    let relative = path.strip_prefix(&self.workspace).unwrap_or(&path);
                    if self.ingest(relative, refiner).await.is_ok() {
                        count += 1;
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

    fn tokenize_query(query: &str) -> Vec<&str> {
        query
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| t.len() >= 2)
            .collect()
    }

    fn content_for_lens(entry: &KnowledgeEntry, lens: Lens) -> (String, bool) {
        match lens {
            Lens::Eco | Lens::Standard => {
                if let Some(ref s) = entry.summary {
                    (s.clone(), true)
                } else {
                    (entry.full_content.clone(), false)
                }
            }
            Lens::Precise => (entry.full_content.clone(), false),
        }
    }

    fn score_entry(entry: &KnowledgeEntry, query: &str, tokens: &[&str]) -> f32 {
        if query.is_empty() {
            return 1.0;
        }

        let path = entry.source.to_string_lossy().to_lowercase();
        let summary = entry.summary.as_deref().unwrap_or("").to_lowercase();
        let full = entry.full_content.to_lowercase();

        let mut score = 0.0f32;

        if !query.is_empty() {
            if path.contains(query) {
                score += 6.0;
            }
            if summary.contains(query) {
                score += 4.0;
            }
            if full.contains(query) {
                score += 2.0;
            }
        }

        for tok in tokens {
            if path.contains(tok) {
                score += 2.5;
            }
            if summary.contains(tok) {
                score += 1.5;
            }
            if full.contains(tok) {
                score += 0.75;
            }
        }

        score
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_entry(source: &str, full: &str, summary: Option<&str>) -> KnowledgeEntry {
        KnowledgeEntry {
            source: PathBuf::from(source),
            full_content: full.to_string(),
            full_token_estimate: estimate_tokens_approx_u32(full),
            summary: summary.map(|s| s.to_string()),
            summary_token_estimate: summary.map(estimate_tokens_approx_u32),
            content_hash: 1,
            last_indexed: chrono::Utc::now(),
        }
    }

    #[test]
    fn query_filters_irrelevant_entries() {
        let store = KnowledgeStore {
            entries: vec![
                mk_entry(
                    "finance/driver_serik.md",
                    "Serik earned 450000 KZT in February",
                    Some("Serik salary February"),
                ),
                mk_entry(
                    "school/homework.md",
                    "Math homework for grade 5",
                    Some("Homework notes"),
                ),
            ],
            workspace: PathBuf::from("."),
        };

        let results = store.query("serik february salary", Lens::Standard, 10);
        assert_eq!(results.len(), 1);
        assert!(results[0].source.to_string_lossy().contains("serik"));
    }

    #[test]
    fn query_prefers_high_score_and_respects_lens() {
        let store = KnowledgeStore {
            entries: vec![
                mk_entry(
                    "drivers/serik.md",
                    "Serik profile full",
                    Some("Serik short profile"),
                ),
                mk_entry(
                    "drivers/marat.md",
                    "Marat profile full",
                    Some("Marat short profile"),
                ),
            ],
            workspace: PathBuf::from("."),
        };

        let eco = store.query("serik", Lens::Eco, 2);
        assert_eq!(eco[0].source, PathBuf::from("drivers/serik.md"));
        assert!(eco[0].is_summary);
        assert!(eco[0].content.contains("short"));

        let precise = store.query("serik", Lens::Precise, 2);
        assert_eq!(precise[0].source, PathBuf::from("drivers/serik.md"));
        assert!(!precise[0].is_summary);
        assert!(precise[0].content.contains("full"));
    }

    #[test]
    fn query_with_budget_enforces_token_cap() {
        let store = KnowledgeStore {
            entries: vec![
                mk_entry(
                    "a.md",
                    "alpha ".repeat(40).as_str(),
                    Some("alpha ".repeat(20).as_str()),
                ),
                mk_entry(
                    "b.md",
                    "beta ".repeat(40).as_str(),
                    Some("beta ".repeat(20).as_str()),
                ),
            ],
            workspace: PathBuf::from("."),
        };

        // ~120 char summary ~= 30 tokens; budget 30 should include only one.
        let results = store.query_with_budget("alpha beta", Lens::Eco, 10, 30);
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn query_with_budget_skips_too_large_and_keeps_packing() {
        let store = KnowledgeStore {
            entries: vec![
                mk_entry(
                    "large.md",
                    "serik ".repeat(120).as_str(),
                    Some("serik ".repeat(80).as_str()),
                ),
                mk_entry(
                    "small.md",
                    "serik ".repeat(30).as_str(),
                    Some("serik ".repeat(12).as_str()),
                ),
            ],
            workspace: PathBuf::from("."),
        };

        // Large summary won't fit, but the smaller one should still be selected.
        let results = store.query_with_budget("serik", Lens::Eco, 10, 20);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].source, PathBuf::from("small.md"));
    }
}
