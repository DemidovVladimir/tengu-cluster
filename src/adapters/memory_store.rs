//! Disk-backed vector memory store implementing MemoryStorePort.

use crate::application::ports::MemoryStorePort;
use crate::domain::memory::{cosine_similarity, MemoryEntry, MemorySearchResult};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

/// In-memory vector store with bincode persistence to disk.
pub(crate) struct DiskVectorMemoryStore {
    entries: RwLock<Vec<MemoryEntry>>,
    store_path: PathBuf,
}

impl DiskVectorMemoryStore {
    /// Load existing store from disk, or create empty if no file exists.
    pub(crate) fn new(store_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(store_dir)
            .with_context(|| format!("failed to create memory store dir: {}", store_dir.display()))?;

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

    /// Persist current entries to disk (atomic write via temp + rename).
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
    fn store(&self, entry: &MemoryEntry) -> Result<()> {
        let mut entries = self.entries.write().map_err(|e| anyhow::anyhow!("lock poisoned: {}", e))?;
        entries.push(entry.clone());
        self.flush(&entries)
    }

    fn search_by_vector(
        &self,
        embedding: &[f32],
        top_k: usize,
    ) -> Result<Vec<MemorySearchResult>> {
        let entries = self.entries.read().map_err(|e| anyhow::anyhow!("lock poisoned: {}", e))?;

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

        scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(top_k);
        Ok(scored)
    }

    fn delete(&self, id: &str) -> Result<bool> {
        let mut entries = self.entries.write().map_err(|e| anyhow::anyhow!("lock poisoned: {}", e))?;
        let before = entries.len();
        entries.retain(|e| e.id != id);
        let deleted = entries.len() < before;
        if deleted {
            self.flush(&entries)?;
        }
        Ok(deleted)
    }

    fn entry_count(&self) -> usize {
        self.entries.read().map(|e| e.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_entry(id: &str, embedding: Vec<f32>) -> MemoryEntry {
        MemoryEntry {
            id: id.to_string(),
            content: format!("content for {}", id),
            embedding,
            agent_id: "test".to_string(),
            created_at_epoch_s: 0,
        }
    }

    #[test]
    fn round_trip_persistence() {
        let tmp = TempDir::new().unwrap();
        let entry = make_entry("1", vec![1.0, 0.0, 0.0]);

        // Store and drop
        {
            let store = DiskVectorMemoryStore::new(tmp.path()).unwrap();
            store.store(&entry).unwrap();
            assert_eq!(store.entry_count(), 1);
        }

        // Reload and verify
        {
            let store = DiskVectorMemoryStore::new(tmp.path()).unwrap();
            assert_eq!(store.entry_count(), 1);
        }
    }

    #[test]
    fn search_ordering() {
        let tmp = TempDir::new().unwrap();
        let store = DiskVectorMemoryStore::new(tmp.path()).unwrap();

        // Store entries with different embeddings
        store.store(&make_entry("close", vec![0.9, 0.1, 0.0])).unwrap();
        store.store(&make_entry("far", vec![0.0, 0.0, 1.0])).unwrap();
        store.store(&make_entry("mid", vec![0.5, 0.5, 0.0])).unwrap();

        // Search with query similar to "close"
        let results = store.search_by_vector(&[1.0, 0.0, 0.0], 3).unwrap();
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].entry.id, "close");
        assert!(results[0].score > results[1].score);
    }

    #[test]
    fn delete_removes_and_persists() {
        let tmp = TempDir::new().unwrap();
        let store = DiskVectorMemoryStore::new(tmp.path()).unwrap();

        store.store(&make_entry("a", vec![1.0])).unwrap();
        store.store(&make_entry("b", vec![0.0])).unwrap();
        assert_eq!(store.entry_count(), 2);

        let deleted = store.delete("a").unwrap();
        assert!(deleted);
        assert_eq!(store.entry_count(), 1);

        // Verify persistence after delete
        let store2 = DiskVectorMemoryStore::new(tmp.path()).unwrap();
        assert_eq!(store2.entry_count(), 1);
    }

    #[test]
    fn delete_nonexistent_returns_false() {
        let tmp = TempDir::new().unwrap();
        let store = DiskVectorMemoryStore::new(tmp.path()).unwrap();
        let deleted = store.delete("nonexistent").unwrap();
        assert!(!deleted);
    }

    #[test]
    fn empty_store_search() {
        let tmp = TempDir::new().unwrap();
        let store = DiskVectorMemoryStore::new(tmp.path()).unwrap();
        let results = store.search_by_vector(&[1.0, 0.0], 5).unwrap();
        assert!(results.is_empty());
    }
}
