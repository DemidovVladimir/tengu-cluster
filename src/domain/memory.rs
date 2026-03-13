//! Domain types and pure functions for persistent vector memory.
//!
//! This module contains the core data structures and similarity math shared by
//! all memory store backends (disk and Qdrant). It has no infrastructure
//! dependencies — pure business logic only.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// A single memory entry stored in the vector store.
///
/// The `embedding` field holds the pre-computed vector representation of
/// `content`, produced by the configured embedding model (e.g.
/// `text-embedding-3-small` → 1536-dimensional `Vec<f32>`). Both the original
/// text and its embedding are persisted so that the disk store can perform
/// brute-force cosine search locally. The Qdrant store sends the embedding as
/// the point vector and the remaining fields as payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MemoryEntry {
    /// Unique UUID v4 identifier.
    pub id: String,
    /// Original human-readable text stored as a memory.
    pub content: String,
    /// Embedding vector produced by the embedding model.
    pub embedding: Vec<f32>,
    /// The agent that created this memory.
    pub agent_id: String,
    /// Unix epoch seconds when the memory was created.
    pub created_at_epoch_s: u64,
    /// Optional metadata for tagging (e.g. kind, source, goal, run_id).
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

/// A search result pairing a memory entry with its cosine similarity score.
///
/// Score ranges from -1.0 (opposite) through 0.0 (orthogonal) to 1.0
/// (identical). Results are returned sorted by score descending.
#[derive(Debug, Clone)]
pub(crate) struct MemorySearchResult {
    pub entry: MemoryEntry,
    /// Cosine similarity between the query embedding and this entry's embedding.
    pub score: f32,
}

/// Compute cosine similarity between two vectors.
/// Uses f64 accumulator for numerical precision.
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

/// Greedily select memories that fit within the token budget.
/// Assumes ~4 chars per token approximation.
pub(crate) fn budget_memories(
    results: &[MemorySearchResult],
    max_tokens: usize,
) -> Vec<&MemorySearchResult> {
    let mut selected = Vec::new();
    let mut used_tokens = 0usize;

    for result in results {
        let entry_tokens = (result.entry.content.len() + 3) / 4; // ceil(len/4)
        if used_tokens + entry_tokens > max_tokens {
            break;
        }
        used_tokens += entry_tokens;
        selected.push(result);
    }

    selected
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_identical_vectors() {
        let v = vec![1.0, 2.0, 3.0];
        let score = cosine_similarity(&v, &v);
        assert!(
            (score - 1.0).abs() < 1e-6,
            "identical vectors should have similarity 1.0"
        );
    }

    #[test]
    fn cosine_orthogonal_vectors() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        let score = cosine_similarity(&a, &b);
        assert!(
            score.abs() < 1e-6,
            "orthogonal vectors should have similarity ~0.0"
        );
    }

    #[test]
    fn cosine_opposite_vectors() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![-1.0, -2.0, -3.0];
        let score = cosine_similarity(&a, &b);
        assert!(
            (score + 1.0).abs() < 1e-6,
            "opposite vectors should have similarity -1.0"
        );
    }

    #[test]
    fn cosine_empty_vectors() {
        let score = cosine_similarity(&[], &[]);
        assert_eq!(score, 0.0);
    }

    #[test]
    fn cosine_mismatched_lengths() {
        let a = vec![1.0, 2.0];
        let b = vec![1.0];
        let score = cosine_similarity(&a, &b);
        assert_eq!(score, 0.0);
    }

    #[test]
    fn cosine_zero_vector() {
        let a = vec![0.0, 0.0];
        let b = vec![1.0, 2.0];
        let score = cosine_similarity(&a, &b);
        assert_eq!(score, 0.0);
    }

    #[test]
    fn budget_memories_respects_limit() {
        let results = vec![
            MemorySearchResult {
                entry: MemoryEntry {
                    id: "1".into(),
                    content: "short".into(), // 5 chars -> ~2 tokens
                    embedding: vec![],
                    agent_id: "a".into(),
                    created_at_epoch_s: 0,
                    metadata: HashMap::new(),
                },
                score: 0.9,
            },
            MemorySearchResult {
                entry: MemoryEntry {
                    id: "2".into(),
                    content: "a".repeat(400), // 400 chars -> 100 tokens
                    embedding: vec![],
                    agent_id: "a".into(),
                    created_at_epoch_s: 0,
                    metadata: HashMap::new(),
                },
                score: 0.8,
            },
        ];

        // Budget of 10 tokens: only first entry fits
        let selected = budget_memories(&results, 10);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].entry.id, "1");
    }

    #[test]
    fn budget_memories_empty_input() {
        let selected = budget_memories(&[], 100);
        assert!(selected.is_empty());
    }

    #[test]
    fn budget_memories_zero_budget() {
        let results = vec![MemorySearchResult {
            entry: MemoryEntry {
                id: "1".into(),
                content: "hello".into(),
                embedding: vec![],
                agent_id: "a".into(),
                created_at_epoch_s: 0,
                metadata: HashMap::new(),
            },
            score: 0.9,
        }];
        let selected = budget_memories(&results, 0);
        assert!(selected.is_empty());
    }
}
