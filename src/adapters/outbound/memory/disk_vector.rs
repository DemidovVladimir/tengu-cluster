//! Disk-backed `VectorStore` — bincode `<store dir>/vectors.bin` (`<workspace>/memory`).
//! Without a workspace the store dir is `[memory] store_path` (default
//! `~/.tengu/memory/`), see `bootstrap::memory::resolve_memory_store_path`.
//!
//! Ported from `src/adapters/memory_builder.rs::DiskVectorMemoryStore` with the
//! signatures adapted to the new `VectorStore` trait. Uses an in-memory
//! `RwLock<Vec<Entry>>` as the working set, atomic-rename flush to disk.

use std::path::{Path, PathBuf};
use std::sync::RwLock;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::domain::memory::{ChunkMetadata, MemoryHit};
use crate::ports::memory::VectorStore;

/// Internal on-disk record. Distinct from the legacy `MemoryEntry` in
/// `types.rs` so the new module has its own wire-format surface.
///
/// `metadata` is stored as a JSON string because `ChunkMetadata::extra`
/// contains `serde_json::Value` (which uses untagged enums that bincode
/// refuses to serialize — "sequences must have a knowable size"). We
/// serialize the whole struct through `serde_json` then bincode the
/// resulting string. Kept private to this module.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    /// Synthetic UUID assigned at write-time so callers can later `delete(id)`.
    /// Defaulted for backwards compatibility with stores written before
    /// delete-by-id landed — old entries get `""` and can't be deleted
    /// individually until re-written.
    #[serde(default)]
    id: String,
    text: String,
    embedding: Vec<f32>,
    metadata_json: String,
}

impl Entry {
    fn new(
        id: String,
        text: String,
        embedding: Vec<f32>,
        metadata: &ChunkMetadata,
    ) -> Result<Self> {
        let metadata_json =
            serde_json::to_string(metadata).context("failed to serialize chunk metadata")?;
        Ok(Self {
            id,
            text,
            embedding,
            metadata_json,
        })
    }

    fn metadata(&self) -> ChunkMetadata {
        serde_json::from_str(&self.metadata_json).unwrap_or_default()
    }
}

/// Brute-force cosine-similarity disk store. Appropriate for local /
/// single-workspace use; the only built-in `VectorStore` (durable memory is Postgres `agentic_memory`).
pub struct DiskVectorStore {
    entries: RwLock<Vec<Entry>>,
    store_path: PathBuf,
    /// Holds a tempdir alive for `in_memory()` test helpers — `None` in
    /// production mode.
    _tempdir: Option<tempfile::TempDir>,
}

impl DiskVectorStore {
    /// Open (or create) a disk-backed store under `store_dir`. The bincode
    /// file lives at `<store_dir>/vectors.bin`.
    pub fn new(store_dir: &Path) -> Result<Self> {
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
            "DiskVectorStore loaded"
        );

        Ok(Self {
            entries: RwLock::new(entries),
            store_path,
            _tempdir: None,
        })
    }

    /// Apply one change to the store under the cross-process lock: re-read
    /// `vectors.bin` first (another process may have written it since this one
    /// loaded), apply `op`, write it back. Each process used to rewrite the
    /// whole file from its own copy, so the last writer dropped the others' rows.
    fn mutate<T>(&self, op: impl FnOnce(&mut Vec<Entry>) -> T) -> Result<T> {
        let _lock = StoreLock::acquire(self.store_path.with_extension("bin.lock"))?;
        let mut entries = self
            .entries
            .write()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {}", e))?;
        if let Some(on_disk) = self.read_disk() {
            *entries = on_disk;
        }
        let out = op(&mut entries);
        self.flush(&entries)?;
        Ok(out)
    }

    /// The rows in `vectors.bin`; `None` when there is no readable file (the
    /// in-memory rows stay).
    fn read_disk(&self) -> Option<Vec<Entry>> {
        let data = std::fs::read(&self.store_path).ok()?;
        bincode::deserialize(&data)
            .map_err(|e| tracing::warn!(error = %e, "memory store unreadable; keeping loaded rows"))
            .ok()
    }

    fn flush(&self, entries: &[Entry]) -> Result<()> {
        let data = bincode::serialize(entries).context("failed to serialize memory store")?;
        let tmp_path = self.store_path.with_extension("bin.tmp");
        std::fs::write(&tmp_path, &data)
            .with_context(|| format!("failed to write {}", tmp_path.display()))?;
        std::fs::rename(&tmp_path, &self.store_path)
            .with_context(|| format!("failed to rename to {}", self.store_path.display()))?;
        Ok(())
    }
}

/// Cross-process write lock: `<store>/vectors.bin.lock`, created exclusively
/// and removed on drop. A lock older than `STALE_LOCK` is a crashed writer's
/// and is taken over. (`File::lock` needs rustc 1.89; the crate supports 1.78.)
struct StoreLock {
    path: PathBuf,
}

const STALE_LOCK: std::time::Duration = std::time::Duration::from_secs(30);
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

impl StoreLock {
    fn acquire(path: PathBuf) -> Result<Self> {
        let deadline = std::time::Instant::now() + LOCK_WAIT;
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => return Ok(Self { path }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = std::fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.elapsed().ok())
                        .is_some_and(|age| age > STALE_LOCK);
                    if stale {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    if std::time::Instant::now() > deadline {
                        anyhow::bail!(
                            "memory store busy: {} is held by another writer",
                            path.display()
                        );
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(e) => return Err(e).with_context(|| format!("create {}", path.display())),
            }
        }
    }
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
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

    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom < 1e-12 {
        return 0.0;
    }

    (dot / denom) as f32
}

/// Return `true` iff every non-empty field in `want` matches the corresponding
/// field in `have`. `None` / empty-vec fields in `want` are ignored (don't
/// constrain). `tags` in `want` must all be present in `have` (subset match).
fn metadata_matches(have: &ChunkMetadata, want: &ChunkMetadata) -> bool {
    if let Some(ref a) = want.agent {
        if have.agent.as_deref() != Some(a.as_str()) {
            return false;
        }
    }
    if let Some(ref s) = want.source {
        if have.source.as_deref() != Some(s.as_str()) {
            return false;
        }
    }
    if let Some(ref k) = want.kind {
        if have.kind.as_deref() != Some(k.as_str()) {
            return false;
        }
    }
    if let Some(ref t) = want.timestamp_utc {
        if have.timestamp_utc.as_deref() != Some(t.as_str()) {
            return false;
        }
    }
    for tag in &want.tags {
        if !have.tags.iter().any(|h| h == tag) {
            return false;
        }
    }
    for (k, v) in &want.extra {
        if have.extra.get(k) != Some(v) {
            return false;
        }
    }
    true
}

#[async_trait]
impl VectorStore for DiskVectorStore {
    async fn write(
        &self,
        embedding: Vec<f32>,
        text: &str,
        metadata: ChunkMetadata,
    ) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        let entry = Entry::new(id.clone(), text.to_string(), embedding, &metadata)?;
        self.mutate(|entries| entries.push(entry))?;
        Ok(id)
    }

    async fn search(
        &self,
        embedding: &[f32],
        top_k: usize,
        filter: Option<&ChunkMetadata>,
    ) -> Result<Vec<MemoryHit>> {
        let entries = self
            .entries
            .read()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {}", e))?;

        let mut scored: Vec<MemoryHit> = entries
            .iter()
            .filter_map(|e| {
                let metadata = e.metadata();
                if let Some(f) = filter {
                    if !metadata_matches(&metadata, f) {
                        return None;
                    }
                }
                Some(MemoryHit {
                    text: e.text.clone(),
                    score: cosine_similarity(&e.embedding, embedding),
                    metadata,
                })
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
        self.mutate(|entries| {
            let before = entries.len();
            entries.retain(|e| e.id != id);
            entries.len() != before
        })
    }

    async fn clear_all(&self) -> Result<()> {
        self.mutate(Vec::clear)
    }

    async fn entry_count(&self) -> Result<usize> {
        let entries = self
            .entries
            .read()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {}", e))?;
        Ok(entries.len())
    }

    async fn storage_bytes(&self) -> Result<u64> {
        match std::fs::metadata(&self.store_path) {
            Ok(m) => Ok(m.len()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(anyhow::anyhow!(
                "failed to stat {}: {}",
                self.store_path.display(),
                e
            )),
        }
    }
}

#[cfg(test)]
impl DiskVectorStore {
    /// Returns a `DiskVectorStore` backed by a fresh `TempDir`. The tempdir
    /// is held by the returned store — it drops (and deletes the files)
    /// when the store drops. Callers needing a longer lifetime should
    /// construct their own via `DiskVectorStore::new`.
    pub fn in_memory() -> Self {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let store_dir = tmp.path().to_path_buf();
        let store_path = store_dir.join("vectors.bin");
        Self {
            entries: RwLock::new(Vec::new()),
            store_path,
            _tempdir: Some(tmp),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two processes on one `vectors.bin` (two stores here) keep each other's
    /// rows: each change re-reads the file under the lock before writing.
    #[tokio::test]
    async fn two_writers_on_one_file_keep_each_others_rows() {
        let dir = tempfile::TempDir::new().unwrap();
        let a = DiskVectorStore::new(dir.path()).unwrap();
        let b = DiskVectorStore::new(dir.path()).unwrap();
        let md = ChunkMetadata::default();
        let id_a = a.write(vec![1.0, 0.0], "from a", md.clone()).await.unwrap();
        b.write(vec![0.0, 1.0], "from b", md.clone()).await.unwrap();
        let fresh = DiskVectorStore::new(dir.path()).unwrap();
        assert_eq!(fresh.entry_count().await.unwrap(), 2);
        // A delete through b sees a's row too.
        assert!(b.delete(&id_a).await.unwrap());
        assert_eq!(
            DiskVectorStore::new(dir.path())
                .unwrap()
                .entry_count()
                .await
                .unwrap(),
            1
        );
        assert!(
            !dir.path().join("vectors.bin.lock").exists(),
            "lock released"
        );
    }

    #[tokio::test]
    async fn write_then_search_returns_hit() {
        let store = DiskVectorStore::in_memory();
        let emb = vec![1.0_f32, 0.0, 0.0];
        store
            .write(emb.clone(), "hello world", ChunkMetadata::default())
            .await
            .unwrap();

        let hits = store.search(&emb, 5, None).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].text, "hello world");
        assert!(hits[0].score > 0.99);
    }

    #[tokio::test]
    async fn search_orders_by_similarity() {
        let store = DiskVectorStore::in_memory();
        store
            .write(vec![1.0, 0.0, 0.0], "match", ChunkMetadata::default())
            .await
            .unwrap();
        store
            .write(vec![0.0, 1.0, 0.0], "orthogonal", ChunkMetadata::default())
            .await
            .unwrap();

        let hits = store.search(&[1.0, 0.0, 0.0], 2, None).await.unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].text, "match");
        assert!(hits[0].score > hits[1].score);
    }

    #[tokio::test]
    async fn filter_by_agent_metadata() {
        let store = DiskVectorStore::in_memory();
        let md_a = ChunkMetadata {
            agent: Some("alice".into()),
            ..Default::default()
        };
        let md_b = ChunkMetadata {
            agent: Some("bob".into()),
            ..Default::default()
        };
        store
            .write(vec![1.0, 0.0], "alice-note", md_a)
            .await
            .unwrap();
        store.write(vec![1.0, 0.0], "bob-note", md_b).await.unwrap();

        let want = ChunkMetadata {
            agent: Some("alice".into()),
            ..Default::default()
        };
        let hits = store.search(&[1.0, 0.0], 5, Some(&want)).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].text, "alice-note");
    }

    #[tokio::test]
    async fn persists_across_reopen() {
        let tmp = tempfile::TempDir::new().unwrap();
        {
            let store = DiskVectorStore::new(tmp.path()).unwrap();
            store
                .write(vec![1.0, 0.0], "persisted", ChunkMetadata::default())
                .await
                .unwrap();
        }
        let store = DiskVectorStore::new(tmp.path()).unwrap();
        let hits = store.search(&[1.0, 0.0], 5, None).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].text, "persisted");
    }
}
