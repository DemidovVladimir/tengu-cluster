//! Shared workspace cache backed by SQLite.
//!
//! Provides a `shared_cache` tool with operations: get, put, delete, list.
//! Data is stored at `<workspace>/.tengu/cache.db` — inspectable with sqlite3.

use crate::adapters::ports::ToolExecutionPort;
use crate::adapters::types::{EffectClass, RegisteredTool, ToolCall};
use anyhow::{bail, Result};
use rusqlite::Connection;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Reserved tool name — skills cannot shadow this.
pub(crate) const SHARED_CACHE_TOOL_NAME: &str = "shared_cache";

// ---------------------------------------------------------------------------
// Tool definition
// ---------------------------------------------------------------------------

pub(crate) fn build_shared_cache_tools() -> Vec<RegisteredTool> {
    vec![RegisteredTool::new(
        SHARED_CACHE_TOOL_NAME,
        "Shared workspace cache for structured data exchange between agents. \
         Persisted across runs in a workspace-local SQLite database.\n\n\
         Operations:\n\
         - \"get\": retrieve a value by namespace + key\n\
         - \"put\": store a JSON value by namespace + key (overwrites if exists)\n\
         - \"delete\": remove an entry by namespace + key\n\
         - \"list\": list keys in a namespace",
        json!({
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "enum": ["get", "put", "delete", "list"],
                    "description": "Cache operation to perform"
                },
                "namespace": {
                    "type": "string",
                    "description": "Namespace for key isolation (e.g. agent name or domain)"
                },
                "key": {
                    "type": "string",
                    "description": "Cache key (required for get, put, delete)"
                },
                "value": {
                    "type": "string",
                    "description": "JSON string value to store (required for put)"
                },
                "metadata": {
                    "type": "string",
                    "description": "Optional JSON metadata to attach (put only)"
                }
            },
            "required": ["operation", "namespace"]
        }),
        EffectClass::Write,
    )
    .with_activity_description("Shared cache")]
}

// ---------------------------------------------------------------------------
// SQLite schema
// ---------------------------------------------------------------------------

const CREATE_TABLE: &str = "
CREATE TABLE IF NOT EXISTS cache_entries (
    namespace       TEXT NOT NULL,
    key             TEXT NOT NULL,
    value_json      TEXT NOT NULL,
    metadata_json   TEXT NOT NULL DEFAULT '{}',
    created_at      TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at      TEXT NOT NULL DEFAULT (datetime('now')),
    producer_agent  TEXT NOT NULL DEFAULT '',
    task_id         TEXT NOT NULL DEFAULT '',
    version         INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (namespace, key)
)";

// ---------------------------------------------------------------------------
// Executor
// ---------------------------------------------------------------------------

pub(crate) struct CacheToolExecutionAdapter {
    db: Mutex<Connection>,
}

impl CacheToolExecutionAdapter {
    /// Open (or create) the cache database at `<workspace>/.tengu/cache.db`.
    pub(crate) fn open(workspace: &Path) -> Result<Self> {
        let db_dir = workspace.join(".tengu");
        std::fs::create_dir_all(&db_dir)?;
        let db_path = db_dir.join("cache.db");

        tracing::info!(path = %db_path.display(), "Opening shared cache database");
        let conn = Connection::open(&db_path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")?;
        conn.execute_batch(CREATE_TABLE)?;
        Ok(Self {
            db: Mutex::new(conn),
        })
    }

    fn execute_get(&self, namespace: &str, key: &str) -> Result<String> {
        let db = self.db.lock().map_err(|e| anyhow::anyhow!("lock: {e}"))?;
        let mut stmt = db.prepare(
            "SELECT value_json, metadata_json, updated_at, producer_agent, version \
             FROM cache_entries WHERE namespace = ?1 AND key = ?2",
        )?;
        let result = stmt.query_row(rusqlite::params![namespace, key], |row| {
            Ok(json!({
                "namespace": namespace,
                "key": key,
                "value": serde_json::from_str::<serde_json::Value>(
                    &row.get::<_, String>(0)?
                ).unwrap_or(serde_json::Value::Null),
                "metadata": serde_json::from_str::<serde_json::Value>(
                    &row.get::<_, String>(1)?
                ).unwrap_or(serde_json::Value::Null),
                "updated_at": row.get::<_, String>(2)?,
                "producer_agent": row.get::<_, String>(3)?,
                "version": row.get::<_, i64>(4)?,
            }))
        });

        match result {
            Ok(val) => {
                tracing::debug!(namespace, key, "cache hit");
                Ok(serde_json::to_string_pretty(&val)?)
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                tracing::debug!(namespace, key, "cache miss");
                Ok(json!({ "status": "miss", "namespace": namespace, "key": key }).to_string())
            }
            Err(e) => bail!("cache get failed: {e}"),
        }
    }

    fn execute_put(
        &self,
        namespace: &str,
        key: &str,
        value_json: &str,
        metadata_json: &str,
        producer_agent: &str,
        task_id: &str,
    ) -> Result<String> {
        // If value is not valid JSON, auto-wrap it as a JSON string.
        // LLMs often pass raw values (hex strings, plain text) without JSON encoding.
        let value_owned;
        let value_json = match serde_json::from_str::<serde_json::Value>(value_json) {
            Ok(_) => value_json,
            Err(_) => {
                value_owned = serde_json::Value::String(value_json.to_string()).to_string();
                &value_owned
            }
        };
        if !metadata_json.is_empty() {
            serde_json::from_str::<serde_json::Value>(metadata_json)
                .map_err(|e| anyhow::anyhow!("metadata must be valid JSON: {e}"))?;
        }

        let meta = if metadata_json.is_empty() {
            "{}"
        } else {
            metadata_json
        };

        let db = self.db.lock().map_err(|e| anyhow::anyhow!("lock: {e}"))?;
        db.execute(
            "INSERT INTO cache_entries (namespace, key, value_json, metadata_json, producer_agent, task_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(namespace, key) DO UPDATE SET
                 value_json = excluded.value_json,
                 metadata_json = excluded.metadata_json,
                 updated_at = datetime('now'),
                 producer_agent = excluded.producer_agent,
                 task_id = excluded.task_id,
                 version = version + 1",
            rusqlite::params![namespace, key, value_json, meta, producer_agent, task_id],
        )?;

        let size = value_json.len();
        tracing::debug!(namespace, key, producer_agent, task_id, size, "cache put");
        Ok(json!({
            "status": "ok",
            "namespace": namespace,
            "key": key,
            "size_bytes": size
        })
        .to_string())
    }

    fn execute_delete(&self, namespace: &str, key: &str) -> Result<String> {
        let db = self.db.lock().map_err(|e| anyhow::anyhow!("lock: {e}"))?;
        let affected = db.execute(
            "DELETE FROM cache_entries WHERE namespace = ?1 AND key = ?2",
            rusqlite::params![namespace, key],
        )?;

        tracing::debug!(namespace, key, affected, "cache delete");
        Ok(json!({
            "status": if affected > 0 { "deleted" } else { "not_found" },
            "namespace": namespace,
            "key": key
        })
        .to_string())
    }

    fn execute_list(&self, namespace: &str) -> Result<String> {
        let db = self.db.lock().map_err(|e| anyhow::anyhow!("lock: {e}"))?;
        let mut stmt = db.prepare(
            "SELECT key, length(value_json), updated_at, producer_agent, version \
             FROM cache_entries WHERE namespace = ?1 ORDER BY key",
        )?;
        let entries: Vec<serde_json::Value> = stmt
            .query_map(rusqlite::params![namespace], |row| {
                Ok(json!({
                    "key": row.get::<_, String>(0)?,
                    "size_bytes": row.get::<_, i64>(1)?,
                    "updated_at": row.get::<_, String>(2)?,
                    "producer_agent": row.get::<_, String>(3)?,
                    "version": row.get::<_, i64>(4)?,
                }))
            })?
            .filter_map(|r| r.ok())
            .collect();

        Ok(json!({
            "namespace": namespace,
            "count": entries.len(),
            "entries": entries
        })
        .to_string())
    }
}

impl ToolExecutionPort for CacheToolExecutionAdapter {
    fn execute_tool(&self, call: &ToolCall) -> Result<String> {
        let operation = call
            .arguments
            .get("operation")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("shared_cache: missing 'operation'"))?;

        let namespace = call
            .arguments
            .get("namespace")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("shared_cache: missing 'namespace'"))?;

        match operation {
            "get" => {
                let key = call
                    .arguments
                    .get("key")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("shared_cache get: missing 'key'"))?;
                self.execute_get(namespace, key)
            }
            "put" => {
                let key = call
                    .arguments
                    .get("key")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("shared_cache put: missing 'key'"))?;
                let value = call
                    .arguments
                    .get("value")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("shared_cache put: missing 'value'"))?;
                let metadata = call
                    .arguments
                    .get("metadata")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                self.execute_put(namespace, key, value, metadata, "", "")
            }
            "delete" => {
                let key = call
                    .arguments
                    .get("key")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("shared_cache delete: missing 'key'"))?;
                self.execute_delete(namespace, key)
            }
            "list" => self.execute_list(namespace),
            other => bail!("shared_cache: unknown operation '{other}'"),
        }
    }
}

// ---------------------------------------------------------------------------
// CLI debug commands
// ---------------------------------------------------------------------------

/// Find the cache database path for a workspace.
pub(crate) fn cache_db_path(workspace: &Path) -> PathBuf {
    workspace.join(".tengu").join("cache.db")
}

/// List cache entries for a namespace (or all namespaces if None).
pub fn cli_cache_list(workspace: &Path, namespace: Option<&str>) -> Result<()> {
    let db_path = cache_db_path(workspace);
    if !db_path.exists() {
        println!("  No cache database found at {}", db_path.display());
        return Ok(());
    }

    let conn = Connection::open(&db_path)?;
    let (sql, params): (&str, Vec<Box<dyn rusqlite::types::ToSql>>) = match namespace {
        Some(ns) => (
            "SELECT namespace, key, length(value_json), updated_at, producer_agent, version \
             FROM cache_entries WHERE namespace = ?1 ORDER BY namespace, key",
            vec![Box::new(ns.to_string())],
        ),
        None => (
            "SELECT namespace, key, length(value_json), updated_at, producer_agent, version \
             FROM cache_entries ORDER BY namespace, key",
            vec![],
        ),
    };

    let mut stmt = conn.prepare(sql)?;
    let params_ref: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let rows = stmt.query_map(params_ref.as_slice(), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, i64>(5)?,
        ))
    })?;

    let entries: Vec<_> = rows.filter_map(|r| r.ok()).collect();
    if entries.is_empty() {
        println!("  (no entries)");
        return Ok(());
    }

    println!(
        "  {:<20} {:<30} {:>8} {:<20} {:<15} {:>4}",
        "NAMESPACE", "KEY", "SIZE", "UPDATED", "PRODUCER", "VER"
    );
    println!("  {}", "-".repeat(101));
    for (ns, key, size, updated, producer, ver) in &entries {
        let producer_display = if producer.is_empty() {
            "-"
        } else {
            producer
        };
        println!(
            "  {:<20} {:<30} {:>6} B {:<20} {:<15} {:>4}",
            ns, key, size, updated, producer_display, ver
        );
    }
    println!("\n  {} entries total", entries.len());
    Ok(())
}

/// Get a single cache entry.
pub fn cli_cache_get(workspace: &Path, namespace: &str, key: &str) -> Result<()> {
    let db_path = cache_db_path(workspace);
    if !db_path.exists() {
        println!("  No cache database found at {}", db_path.display());
        return Ok(());
    }

    let conn = Connection::open(&db_path)?;
    let result = conn.query_row(
        "SELECT value_json, metadata_json, created_at, updated_at, producer_agent, task_id, version \
         FROM cache_entries WHERE namespace = ?1 AND key = ?2",
        rusqlite::params![namespace, key],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, i64>(6)?,
            ))
        },
    );

    match result {
        Ok((value, metadata, created, updated, producer, task_id, version)) => {
            println!("  namespace:  {namespace}");
            println!("  key:        {key}");
            println!("  version:    {version}");
            println!("  created:    {created}");
            println!("  updated:    {updated}");
            if !producer.is_empty() {
                println!("  producer:   {producer}");
            }
            if !task_id.is_empty() {
                println!("  task_id:    {task_id}");
            }
            if metadata != "{}" {
                println!("  metadata:   {metadata}");
            }
            println!("  value:");
            // Pretty-print JSON value
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&value) {
                println!("{}", serde_json::to_string_pretty(&parsed)?);
            } else {
                println!("{value}");
            }
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            println!("  Not found: {namespace}/{key}");
        }
        Err(e) => bail!("cache get failed: {e}"),
    }
    Ok(())
}

/// Delete a single cache entry.
pub fn cli_cache_delete(workspace: &Path, namespace: &str, key: &str) -> Result<()> {
    let db_path = cache_db_path(workspace);
    if !db_path.exists() {
        println!("  No cache database found at {}", db_path.display());
        return Ok(());
    }

    let conn = Connection::open(&db_path)?;
    let affected = conn.execute(
        "DELETE FROM cache_entries WHERE namespace = ?1 AND key = ?2",
        rusqlite::params![namespace, key],
    )?;
    if affected > 0 {
        println!("  Deleted {namespace}/{key}");
    } else {
        println!("  Not found: {namespace}/{key}");
    }
    Ok(())
}

/// Print cache statistics.
pub fn cli_cache_stats(workspace: &Path) -> Result<()> {
    let db_path = cache_db_path(workspace);
    if !db_path.exists() {
        println!("  No cache database found at {}", db_path.display());
        return Ok(());
    }

    let conn = Connection::open(&db_path)?;

    let total: i64 = conn.query_row("SELECT COUNT(*) FROM cache_entries", [], |r| r.get(0))?;
    let total_size: i64 = conn.query_row(
        "SELECT COALESCE(SUM(length(value_json)), 0) FROM cache_entries",
        [],
        |r| r.get(0),
    )?;

    println!("  Cache: {}", db_path.display());
    println!("  Entries:    {total}");
    println!("  Value size: {total_size} bytes");

    // Per-namespace breakdown
    let mut stmt = conn.prepare(
        "SELECT namespace, COUNT(*), SUM(length(value_json)) FROM cache_entries GROUP BY namespace ORDER BY namespace",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;

    let namespaces: Vec<_> = rows.filter_map(|r| r.ok()).collect();
    if !namespaces.is_empty() {
        println!("\n  {:<30} {:>8} {:>10}", "NAMESPACE", "ENTRIES", "SIZE");
        println!("  {}", "-".repeat(50));
        for (ns, count, size) in &namespaces {
            println!("  {:<30} {:>8} {:>8} B", ns, count, size);
        }
    }

    // DB file size
    if let Ok(meta) = std::fs::metadata(&db_path) {
        println!("\n  DB file: {} bytes", meta.len());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_call(operation: &str, namespace: &str, key: Option<&str>, value: Option<&str>) -> ToolCall {
        let mut args = json!({ "operation": operation, "namespace": namespace });
        if let Some(k) = key {
            args["key"] = json!(k);
        }
        if let Some(v) = value {
            args["value"] = json!(v);
        }
        ToolCall {
            id: "test".into(),
            name: SHARED_CACHE_TOOL_NAME.into(),
            arguments: args,
        }
    }

    #[test]
    fn put_and_get() {
        let dir = TempDir::new().unwrap();
        let exec = CacheToolExecutionAdapter::open(dir.path()).unwrap();

        let put = exec.execute_tool(&make_call("put", "test_ns", Some("k1"), Some(r#"{"a":1}"#))).unwrap();
        assert!(put.contains("\"status\":\"ok\""));

        let get = exec.execute_tool(&make_call("get", "test_ns", Some("k1"), None)).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&get).unwrap();
        assert_eq!(parsed["value"]["a"], 1);
    }

    #[test]
    fn get_miss() {
        let dir = TempDir::new().unwrap();
        let exec = CacheToolExecutionAdapter::open(dir.path()).unwrap();

        let get = exec.execute_tool(&make_call("get", "ns", Some("missing"), None)).unwrap();
        assert!(get.contains("\"status\":\"miss\""));
    }

    #[test]
    fn put_overwrite_increments_version() {
        let dir = TempDir::new().unwrap();
        let exec = CacheToolExecutionAdapter::open(dir.path()).unwrap();

        exec.execute_tool(&make_call("put", "ns", Some("k"), Some(r#""v1""#))).unwrap();
        exec.execute_tool(&make_call("put", "ns", Some("k"), Some(r#""v2""#))).unwrap();

        let get = exec.execute_tool(&make_call("get", "ns", Some("k"), None)).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&get).unwrap();
        assert_eq!(parsed["value"], "v2");
        assert_eq!(parsed["version"], 2);
    }

    #[test]
    fn delete_existing_and_missing() {
        let dir = TempDir::new().unwrap();
        let exec = CacheToolExecutionAdapter::open(dir.path()).unwrap();

        exec.execute_tool(&make_call("put", "ns", Some("k"), Some(r#""val""#))).unwrap();
        let del = exec.execute_tool(&make_call("delete", "ns", Some("k"), None)).unwrap();
        assert!(del.contains("\"status\":\"deleted\""));

        let del2 = exec.execute_tool(&make_call("delete", "ns", Some("k"), None)).unwrap();
        assert!(del2.contains("\"status\":\"not_found\""));
    }

    #[test]
    fn list_entries() {
        let dir = TempDir::new().unwrap();
        let exec = CacheToolExecutionAdapter::open(dir.path()).unwrap();

        exec.execute_tool(&make_call("put", "ns", Some("a"), Some(r#""1""#))).unwrap();
        exec.execute_tool(&make_call("put", "ns", Some("b"), Some(r#""2""#))).unwrap();
        exec.execute_tool(&make_call("put", "other", Some("c"), Some(r#""3""#))).unwrap();

        let list = exec.execute_tool(&make_call("list", "ns", None, None)).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&list).unwrap();
        assert_eq!(parsed["count"], 2);
    }

    #[test]
    fn rejects_invalid_json_value() {
        let dir = TempDir::new().unwrap();
        let exec = CacheToolExecutionAdapter::open(dir.path()).unwrap();

        let result = exec.execute_tool(&make_call("put", "ns", Some("k"), Some("not valid json {")));
        assert!(result.is_err());
    }

    #[test]
    fn namespace_isolation() {
        let dir = TempDir::new().unwrap();
        let exec = CacheToolExecutionAdapter::open(dir.path()).unwrap();

        exec.execute_tool(&make_call("put", "ns_a", Some("key"), Some(r#""a""#))).unwrap();
        exec.execute_tool(&make_call("put", "ns_b", Some("key"), Some(r#""b""#))).unwrap();

        let get_a = exec.execute_tool(&make_call("get", "ns_a", Some("key"), None)).unwrap();
        let parsed_a: serde_json::Value = serde_json::from_str(&get_a).unwrap();
        assert_eq!(parsed_a["value"], "a");

        let get_b = exec.execute_tool(&make_call("get", "ns_b", Some("key"), None)).unwrap();
        let parsed_b: serde_json::Value = serde_json::from_str(&get_b).unwrap();
        assert_eq!(parsed_b["value"], "b");
    }
}
