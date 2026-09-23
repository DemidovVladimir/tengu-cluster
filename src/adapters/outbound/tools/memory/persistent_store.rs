// src/adapters/plugins/memory/persistent_store.rs
//! `persistent_store` tool — chunked file storage with vector semantic search.
//!
//! Migrated from `persistent_store_executor.rs` during Phase A / task A5. The
//! old executor used `tokio::task::block_in_place` to bridge sync→async; this
//! implementation is natively async and `.await`s `MemoryService` methods
//! directly.
//!
//! Files are saved raw at `<workspace>/.tengu/storage/<file_id>/` and each
//! file is chunked, embedded, and stored in the vector memory backend (disk or
//! Qdrant).

use std::collections::HashMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Result};
use async_trait::async_trait;
use calamine::{open_workbook_auto, Reader};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::adapters::memory::manager::MemoryManager;
use crate::adapters::outbound::tools::args::require_str;
use crate::domain::memory::ChunkMetadata;
use crate::domain::message::ToolDef;
use crate::ports::tool::{Tool, ToolCtx, ToolOutput};

pub(crate) const PERSISTENT_STORE_TOOL_NAME: &str = "persistent_store";

const MANIFEST_FILE: &str = "manifest.json";

// ---------------------------------------------------------------------------
// File manifest persisted alongside the raw file
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileManifest {
    file_id: String,
    original_name: String,
    stored_at_epoch_s: u64,
    size_bytes: u64,
    chunk_count: usize,
    chunk_ids: Vec<String>,
    description: Option<String>,
}

// ---------------------------------------------------------------------------
// Chunking
// ---------------------------------------------------------------------------

fn chunk_text(text: &str, chunk_size: usize, overlap: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![];
    }
    let chunk_size = chunk_size.max(1);
    let overlap = overlap.min(chunk_size.saturating_sub(1));
    let step = chunk_size.saturating_sub(overlap).max(1);

    let chars: Vec<char> = text.chars().collect();
    let mut chunks = Vec::new();
    let mut start = 0;

    while start < chars.len() {
        let end = (start + chunk_size).min(chars.len());
        let chunk: String = chars[start..end].iter().collect();
        if !chunk.trim().is_empty() {
            chunks.push(chunk);
        }
        start += step;
        if end == chars.len() {
            break;
        }
    }

    chunks
}

// ---------------------------------------------------------------------------
// Document text extraction
// ---------------------------------------------------------------------------

/// Extract readable text from a file, dispatching by extension.
/// Returns `Ok(Some(text))` for recognized formats, `Ok(None)` if the file
/// should fall through to the generic UTF-8 path.
fn extract_text_from_file(path: &Path, raw_bytes: &[u8]) -> Result<Option<String>> {
    let ext = path
        .extension()
        .map(|e| e.to_ascii_lowercase().to_string_lossy().to_string())
        .unwrap_or_default();

    match ext.as_str() {
        "pdf" => {
            let text = pdf_extract::extract_text(path)
                .map_err(|e| anyhow::anyhow!("PDF text extraction failed: {e}"))?;
            if text.trim().is_empty() {
                bail!("PDF contains no extractable text (may be image-only)");
            }
            Ok(Some(text))
        }
        "docx" => extract_docx_text(raw_bytes).map(Some),
        "xlsx" | "xls" | "xlsm" => extract_excel_text(path).map(Some),
        _ => Ok(None), // fall through to UTF-8 lossy
    }
}

/// Extract text from a .docx file (ZIP of XML).
fn extract_docx_text(raw_bytes: &[u8]) -> Result<String> {
    let cursor = std::io::Cursor::new(raw_bytes);
    let mut archive =
        zip::ZipArchive::new(cursor).map_err(|e| anyhow::anyhow!("Not a valid docx: {e}"))?;

    let mut full_text = String::new();

    if let Ok(mut entry) = archive.by_name("word/document.xml") {
        let mut xml = String::new();
        entry.read_to_string(&mut xml)?;
        full_text.push_str(&strip_xml_tags(&xml));
    }

    if full_text.trim().is_empty() {
        bail!("docx contains no extractable text");
    }
    Ok(full_text)
}

/// Extract text from Excel workbooks via calamine.
fn extract_excel_text(path: &Path) -> Result<String> {
    let mut workbook =
        open_workbook_auto(path).map_err(|e| anyhow::anyhow!("Cannot open spreadsheet: {e}"))?;

    let mut full_text = String::new();
    let sheet_names: Vec<String> = workbook.sheet_names().to_vec();

    for name in &sheet_names {
        if let Ok(range) = workbook.worksheet_range(name) {
            full_text.push_str(&format!("--- Sheet: {} ---\n", name));
            for row in range.rows() {
                let cells: Vec<String> = row.iter().map(|cell| format!("{}", cell)).collect();
                full_text.push_str(&cells.join("\t"));
                full_text.push('\n');
            }
            full_text.push('\n');
        }
    }

    if full_text.trim().is_empty() {
        bail!("Spreadsheet contains no data");
    }
    Ok(full_text)
}

/// Minimal XML tag stripper — extracts text content between tags.
fn strip_xml_tags(xml: &str) -> String {
    let mut out = String::with_capacity(xml.len() / 2);
    let mut in_tag = false;
    let mut tag_buf = String::new();

    for ch in xml.chars() {
        match ch {
            '<' => {
                in_tag = true;
                tag_buf.clear();
            }
            '>' if in_tag => {
                in_tag = false;
                let tag = tag_buf.as_str();
                if tag.starts_with("/w:p") || tag.starts_with("/w:tr") {
                    out.push('\n');
                }
            }
            _ if in_tag => {
                tag_buf.push(ch);
            }
            _ => {
                out.push(ch);
            }
        }
    }
    out
}

fn current_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ---------------------------------------------------------------------------
// Tool
// ---------------------------------------------------------------------------

pub(crate) struct PersistentStoreTool {
    def: ToolDef,
    workspace: PathBuf,
    memory_manager: Arc<MemoryManager>,
    chunk_size: usize,
    chunk_overlap: usize,
}

impl PersistentStoreTool {
    pub(crate) fn new(
        workspace: PathBuf,
        memory_manager: Arc<MemoryManager>,
        chunk_size: usize,
        chunk_overlap: usize,
    ) -> Self {
        Self {
            def: super::persistent_store_def(),
            workspace,
            memory_manager,
            chunk_size,
            chunk_overlap,
        }
    }

    /// Build a structured `ChunkMetadata` from the legacy `HashMap<String, String>`
    /// the executor tracks per-chunk. Known keys (`file_id`, `file_name`, etc.)
    /// are preserved under `extra` as JSON string values so `memory_search`'s
    /// agent/source/kind filters don't match persistent-store chunks
    /// accidentally.
    fn chunk_metadata(kv: &HashMap<String, String>) -> ChunkMetadata {
        let mut md = ChunkMetadata {
            agent: Some("persistent_store".to_string()),
            kind: Some("persistent_store".to_string()),
            ..Default::default()
        };
        for (k, v) in kv {
            md.extra
                .insert(k.clone(), serde_json::Value::String(v.clone()));
        }
        md
    }

    fn storage_root(&self) -> PathBuf {
        self.workspace.join(".tengu").join("storage")
    }

    fn resolve_source_path(&self, raw: &str) -> Result<PathBuf> {
        let path = if Path::new(raw).is_absolute() {
            PathBuf::from(raw)
        } else {
            self.workspace.join(raw)
        };
        if !path.exists() {
            bail!("file not found: {}", path.display());
        }
        Ok(path)
    }

    // -- store ---------------------------------------------------------------

    async fn execute_store(&self, file_path: &str, description: Option<&str>) -> Result<String> {
        let source = self.resolve_source_path(file_path)?;
        let raw_bytes = std::fs::read(&source)?;
        let file_name = source
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "unnamed".to_string());

        let file_id = uuid::Uuid::new_v4().to_string();
        let file_dir = self.storage_root().join(&file_id);
        std::fs::create_dir_all(&file_dir)?;

        // Save raw file.
        std::fs::write(file_dir.join(&file_name), &raw_bytes)?;

        // Extract text — dispatch by file format (PDF, docx, xlsx) or UTF-8 fallback.
        let text_content = match extract_text_from_file(&source, &raw_bytes) {
            Ok(Some(extracted)) => std::borrow::Cow::Owned(extracted),
            Ok(None) => String::from_utf8_lossy(&raw_bytes),
            Err(e) => {
                warn!(error = %e, file_name = %file_name, "Text extraction failed, storing as binary");
                std::borrow::Cow::Borrowed("")
            }
        };

        if text_content.trim().is_empty() {
            // Binary file with no text — store manifest only, no chunks.
            let manifest = FileManifest {
                file_id: file_id.clone(),
                original_name: file_name.clone(),
                stored_at_epoch_s: current_epoch(),
                size_bytes: raw_bytes.len() as u64,
                chunk_count: 0,
                chunk_ids: vec![],
                description: description.map(|s| s.to_string()),
            };
            let manifest_json = serde_json::to_string_pretty(&manifest)?;
            std::fs::write(file_dir.join(MANIFEST_FILE), &manifest_json)?;

            // If a description was provided, embed at least that.
            if let Some(desc) = description {
                let desc_content = format!("[file:{}] {}", file_name, desc);
                let mut meta = HashMap::new();
                meta.insert("persistent_store".to_string(), "true".to_string());
                meta.insert("file_id".to_string(), file_id.clone());
                meta.insert("file_name".to_string(), file_name.clone());
                meta.insert("chunk_index".to_string(), "desc".to_string());
                let md = Self::chunk_metadata(&meta);
                let _ = self
                    .memory_manager
                    .ingest_one(&desc_content, "persistent_store", md)
                    .await;
            }

            info!(file_id = %file_id, file_name = %file_name, "Stored binary file (no chunks)");
            return Ok(json!({
                "file_id": file_id,
                "file_name": file_name,
                "size_bytes": raw_bytes.len(),
                "chunks": 0,
                "status": "stored (binary, no text chunks)"
            })
            .to_string());
        }

        // Chunk the text content.
        let chunks = chunk_text(&text_content, self.chunk_size, self.chunk_overlap);

        // Track the chunk ids returned by the vector store so the manifest
        // can record them — enables deterministic per-chunk delete on file
        // removal (addresses limitation #2).
        let mut chunk_ids: Vec<String> = Vec::new();
        for (i, chunk) in chunks.iter().enumerate() {
            let chunk_content = if i == 0 {
                if let Some(desc) = description {
                    format!("[file:{} | {}] {}", file_name, desc, chunk)
                } else {
                    format!("[file:{}] {}", file_name, chunk)
                }
            } else {
                format!(
                    "[file:{} chunk {}/{}] {}",
                    file_name,
                    i + 1,
                    chunks.len(),
                    chunk
                )
            };

            let mut meta = HashMap::new();
            meta.insert("persistent_store".to_string(), "true".to_string());
            meta.insert("file_id".to_string(), file_id.clone());
            meta.insert("file_name".to_string(), file_name.clone());
            meta.insert("chunk_index".to_string(), i.to_string());
            meta.insert("chunk_total".to_string(), chunks.len().to_string());
            let md = Self::chunk_metadata(&meta);

            match self
                .memory_manager
                .ingest_one(&chunk_content, "persistent_store", md)
                .await
            {
                Ok(id) => chunk_ids.push(id),
                Err(e) => {
                    warn!(error = %e, chunk = i, "Failed to embed chunk, skipping");
                }
            }
        }

        let embedded_count = chunk_ids.len();
        let manifest = FileManifest {
            file_id: file_id.clone(),
            original_name: file_name.clone(),
            stored_at_epoch_s: current_epoch(),
            size_bytes: raw_bytes.len() as u64,
            chunk_count: embedded_count,
            chunk_ids: chunk_ids.clone(),
            description: description.map(|s| s.to_string()),
        };
        let manifest_json = serde_json::to_string_pretty(&manifest)?;
        std::fs::write(file_dir.join(MANIFEST_FILE), &manifest_json)?;

        info!(
            file_id = %file_id,
            file_name = %file_name,
            chunks = embedded_count,
            "Stored file with vector chunks"
        );

        Ok(json!({
            "file_id": file_id,
            "file_name": file_name,
            "size_bytes": raw_bytes.len(),
            "chunks": embedded_count,
            "status": "stored"
        })
        .to_string())
    }

    // -- search --------------------------------------------------------------

    async fn execute_search(&self, query: &str, top_k: usize) -> Result<String> {
        // Filter by `kind = "persistent_store"` so we don't retrieve
        // agent-visible turn summaries or `memory_ingest` documents.
        let filter = ChunkMetadata {
            kind: Some("persistent_store".to_string()),
            ..Default::default()
        };

        let results = self
            .memory_manager
            .search(query, top_k * 3, Some(&filter))
            .await?;

        // Deduplicate by file_id, keep highest-scoring chunk per file.
        let mut seen_files: HashMap<String, serde_json::Value> = HashMap::new();
        for r in &results {
            let file_id = r
                .metadata
                .extra
                .get("file_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_default();
            if file_id.is_empty() {
                continue;
            }
            if seen_files.len() >= top_k && !seen_files.contains_key(&file_id) {
                continue;
            }
            seen_files.entry(file_id.clone()).or_insert_with(|| {
                let file_name = r
                    .metadata
                    .extra
                    .get("file_name")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                json!({
                    "file_id": file_id,
                    "file_name": file_name,
                    "score": r.score,
                    "matched_chunk": r.text,
                })
            });
        }

        let results_vec: Vec<serde_json::Value> = seen_files.into_values().collect();
        Ok(json!({
            "results": results_vec,
            "count": results_vec.len(),
        })
        .to_string())
    }

    // -- list ----------------------------------------------------------------

    fn execute_list(&self) -> Result<String> {
        let storage_root = self.storage_root();
        let mut files = Vec::new();

        if storage_root.is_dir() {
            for entry in std::fs::read_dir(&storage_root)? {
                let entry = entry?;
                if !entry.path().is_dir() {
                    continue;
                }
                let manifest_path = entry.path().join(MANIFEST_FILE);
                if let Ok(data) = std::fs::read_to_string(&manifest_path) {
                    if let Ok(manifest) = serde_json::from_str::<FileManifest>(&data) {
                        files.push(json!({
                            "file_id": manifest.file_id,
                            "file_name": manifest.original_name,
                            "size_bytes": manifest.size_bytes,
                            "chunks": manifest.chunk_count,
                            "stored_at": manifest.stored_at_epoch_s,
                            "description": manifest.description,
                        }));
                    }
                }
            }
        }

        Ok(json!({
            "files": files,
            "count": files.len(),
        })
        .to_string())
    }

    // -- delete --------------------------------------------------------------

    async fn execute_delete(&self, file_id: &str) -> Result<String> {
        let file_dir = self.storage_root().join(file_id);
        let manifest_path = file_dir.join(MANIFEST_FILE);

        if !manifest_path.exists() {
            bail!("persistent_store: file_id '{}' not found", file_id);
        }

        let manifest_data = std::fs::read_to_string(&manifest_path)?;
        let manifest: FileManifest = serde_json::from_str(&manifest_data)?;

        // Delete vector entries by id (addresses limitation #2). For older
        // manifests written before ids were tracked, chunk_ids is empty —
        // log that we can't clean the vectors and move on (the file-level
        // manifest is still removed so listings stay consistent).
        let mut vectors_deleted = 0usize;
        let mut vectors_missing = 0usize;
        for id in &manifest.chunk_ids {
            match self.memory_manager.delete_entry(id).await {
                Ok(true) => vectors_deleted += 1,
                Ok(false) => vectors_missing += 1,
                Err(e) => warn!(error = %e, chunk_id = %id, "failed to delete chunk vector"),
            }
        }
        if manifest.chunk_ids.is_empty() && manifest.chunk_count > 0 {
            warn!(
                file_id = %file_id,
                chunk_count = manifest.chunk_count,
                "Legacy manifest without chunk_ids — vectors cannot be deleted individually. \
                 Run `tengu memory purge` to clear the full vector store if stale entries \
                 cause problems."
            );
        }

        // Delete the file directory.
        std::fs::remove_dir_all(&file_dir)?;

        info!(
            file_id = %file_id,
            file_name = %manifest.original_name,
            vectors_deleted,
            vectors_missing,
            "Deleted stored file + vector entries"
        );

        Ok(json!({
            "file_id": file_id,
            "file_name": manifest.original_name,
            "chunks_deleted": vectors_deleted,
            "chunks_orphaned": vectors_missing,
            "status": "deleted"
        })
        .to_string())
    }
}

#[async_trait]
impl Tool for PersistentStoreTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        // Every operation reads or writes under `<workspace>/.tengu/storage/`.
        // Gate on workspace fs_write — matches `shared_cache`'s approach.
        ctx.scope.check_fs_write(ctx.workspace)?;

        // Ensure the storage root exists — pre-migration the executor created
        // it in `new()`; we do it lazily per-call now.
        let storage_root = self.storage_root();
        std::fs::create_dir_all(&storage_root)?;

        let operation = require_str(args, "persistent_store", "operation")?;

        let result = match operation {
            "store" => {
                let file_path = require_str(args, "persistent_store store", "file_path")?;
                let description = args.get("description").and_then(|v| v.as_str());
                self.execute_store(file_path, description).await?
            }
            "search" => {
                let query = require_str(args, "persistent_store search", "query")?;
                let top_k = args.get("top_k").and_then(|v| v.as_u64()).unwrap_or(5) as usize;
                self.execute_search(query, top_k).await?
            }
            "list" => self.execute_list()?,
            "delete" => {
                let file_id = require_str(args, "persistent_store delete", "file_id")?;
                self.execute_delete(file_id).await?
            }
            other => bail!("persistent_store: unknown operation '{other}'"),
        };

        Ok(ToolOutput::from(result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chunk_text_basic() {
        let text = "abcdefghij";
        let chunks = chunk_text(text, 4, 1);
        assert_eq!(chunks, vec!["abcd", "defg", "ghij"]);
    }

    #[test]
    fn test_chunk_text_no_overlap() {
        let text = "abcdefgh";
        let chunks = chunk_text(text, 4, 0);
        assert_eq!(chunks, vec!["abcd", "efgh"]);
    }

    #[test]
    fn test_chunk_text_small() {
        let text = "abc";
        let chunks = chunk_text(text, 10, 2);
        assert_eq!(chunks, vec!["abc"]);
    }

    #[test]
    fn test_chunk_text_empty() {
        let chunks = chunk_text("", 4, 1);
        assert!(chunks.is_empty());
    }

    #[test]
    fn test_chunk_text_exact() {
        let text = "abcd";
        let chunks = chunk_text(text, 4, 0);
        assert_eq!(chunks, vec!["abcd"]);
    }

    // -- integration-ish tests using plugin context ------------------------

    use crate::adapters::outbound::memory::disk_vector::DiskVectorStore;
    use crate::adapters::outbound::memory::embedder::Embedder;
    use crate::adapters::outbound::tools::workspace::test_support::TestHarness;
    use crate::domain::scope::ToolScope;
    use crate::ports::memory::VectorStore;
    use std::sync::Arc;
    use tempfile::TempDir;

    async fn make_manager() -> Arc<MemoryManager> {
        let manager = Arc::new(MemoryManager::new());
        let store: Arc<dyn VectorStore> = Arc::new(DiskVectorStore::in_memory());
        let embedder = Arc::new(Embedder::null());
        manager.set_vector_backend(embedder, store).await;
        manager
    }

    #[tokio::test]
    async fn persistent_store_scope_denies_outside_workspace() {
        let tmp_ws = TempDir::new().unwrap();
        let tmp_root = TempDir::new().unwrap();
        // fs_roots points at a different tempdir — check_fs_write must deny.
        let scope = ToolScope {
            fs_roots: vec![tmp_root.path().to_path_buf()],
            ..Default::default()
        };
        let harness = TestHarness::with_scope(tmp_ws.path(), scope);
        let manager = make_manager().await;
        let tool = PersistentStoreTool::new(tmp_ws.path().to_path_buf(), manager, 1000, 200);

        let result = tool
            .execute(&json!({ "operation": "list" }), &harness.ctx())
            .await;
        assert!(result.is_err(), "expected scope denial, got: {:?}", result);
    }

    #[tokio::test]
    async fn persistent_store_list_empty_returns_zero() {
        let tmp = TempDir::new().unwrap();
        let harness = TestHarness::new(tmp.path());
        let manager = make_manager().await;
        let tool = PersistentStoreTool::new(tmp.path().to_path_buf(), manager, 1000, 200);

        let out = tool
            .execute(&json!({ "operation": "list" }), &harness.ctx())
            .await
            .expect("list should succeed on empty workspace");
        let parsed: Value = serde_json::from_str(&out.text).expect("valid json");
        assert_eq!(parsed["count"], 0);
        assert_eq!(parsed["files"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn persistent_store_store_then_list_then_delete() {
        let tmp = TempDir::new().unwrap();
        let harness = TestHarness::new(tmp.path());
        let manager = make_manager().await;
        let tool = PersistentStoreTool::new(tmp.path().to_path_buf(), manager, 100, 20);

        // Create a text file in the workspace.
        let text_file = tmp.path().join("note.txt");
        std::fs::write(&text_file, "the quick brown fox jumps over the lazy dog").unwrap();

        // Store it.
        let stored = tool
            .execute(
                &json!({
                    "operation": "store",
                    "file_path": "note.txt",
                    "description": "test note"
                }),
                &harness.ctx(),
            )
            .await
            .expect("store");
        let parsed: Value = serde_json::from_str(&stored.text).unwrap();
        let file_id = parsed["file_id"].as_str().unwrap().to_string();
        assert_eq!(parsed["file_name"], "note.txt");

        // List — should find exactly one file.
        let listed = tool
            .execute(&json!({ "operation": "list" }), &harness.ctx())
            .await
            .unwrap();
        let listed: Value = serde_json::from_str(&listed.text).unwrap();
        assert_eq!(listed["count"], 1);

        // Delete it.
        let deleted = tool
            .execute(
                &json!({ "operation": "delete", "file_id": file_id }),
                &harness.ctx(),
            )
            .await
            .expect("delete");
        let deleted: Value = serde_json::from_str(&deleted.text).unwrap();
        assert_eq!(deleted["status"], "deleted");

        // List — should be empty again.
        let listed = tool
            .execute(&json!({ "operation": "list" }), &harness.ctx())
            .await
            .unwrap();
        let listed: Value = serde_json::from_str(&listed.text).unwrap();
        assert_eq!(listed["count"], 0);
    }

    #[tokio::test]
    async fn persistent_store_unknown_operation_errors() {
        let tmp = TempDir::new().unwrap();
        let harness = TestHarness::new(tmp.path());
        let manager = make_manager().await;
        let tool = PersistentStoreTool::new(tmp.path().to_path_buf(), manager, 1000, 200);

        let result = tool
            .execute(&json!({ "operation": "reboot" }), &harness.ctx())
            .await;
        assert!(
            result.is_err(),
            "expected error for unknown op, got: {:?}",
            result
        );
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("unknown operation"), "unexpected: {}", msg);
    }

    #[tokio::test]
    async fn persistent_store_delete_missing_errors() {
        let tmp = TempDir::new().unwrap();
        let harness = TestHarness::new(tmp.path());
        let manager = make_manager().await;
        let tool = PersistentStoreTool::new(tmp.path().to_path_buf(), manager, 1000, 200);

        let result = tool
            .execute(
                &json!({ "operation": "delete", "file_id": "no-such-id" }),
                &harness.ctx(),
            )
            .await;
        assert!(result.is_err(), "expected error for missing file_id");
    }
}
