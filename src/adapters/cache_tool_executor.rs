//! Shared workspace cache backed by SQLite.
//!
//! Provides a `shared_cache` tool with operations: get, put, delete, list.
//! Data is stored at `<workspace>/.tengu/cache.db`.

use crate::adapters::ports::ToolExecutionPort;
use crate::adapters::types::{ToolCall, ToolDef};
use anyhow::{bail, Result};
use rusqlite::Connection;
use serde_json::json;
use std::path::Path;
use std::sync::Mutex;

/// Reserved tool name — skills cannot shadow this.
pub(crate) const SHARED_CACHE_TOOL_NAME: &str = "shared_cache";

pub(crate) fn build_shared_cache_tools() -> Vec<ToolDef> {
    vec![ToolDef::new(
        SHARED_CACHE_TOOL_NAME,
        "Shared workspace cache for data exchange between agents.",
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
                    "description": "Namespace for key isolation"
                },
                "key": {
                    "type": "string",
                    "description": "Cache key (required for get, put, delete)"
                },
                "value": {
                    "type": "string",
                    "description": "JSON string value to store (required for put)"
                }
            },
            "required": ["operation", "namespace"]
        }),
    )]
}

const CREATE_TABLE: &str = "
CREATE TABLE IF NOT EXISTS cache_entries (
    namespace TEXT NOT NULL,
    key       TEXT NOT NULL,
    value_json TEXT NOT NULL,
    PRIMARY KEY (namespace, key)
)";

pub(crate) struct CacheToolExecutionAdapter {
    db: Mutex<Connection>,
}

impl CacheToolExecutionAdapter {
    pub(crate) fn open(workspace: &Path) -> Result<Self> {
        let db_dir = workspace.join(".tengu");
        std::fs::create_dir_all(&db_dir)?;
        let db_path = db_dir.join("cache.db");

        let conn = Connection::open(&db_path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")?;
        conn.execute_batch(CREATE_TABLE)?;
        Ok(Self {
            db: Mutex::new(conn),
        })
    }

    fn execute_get(&self, namespace: &str, key: &str) -> Result<String> {
        let db = self.db.lock().map_err(|e| anyhow::anyhow!("lock: {e}"))?;
        let result = db.query_row(
            "SELECT value_json FROM cache_entries WHERE namespace = ?1 AND key = ?2",
            rusqlite::params![namespace, key],
            |row| row.get::<_, String>(0),
        );

        match result {
            Ok(val) => Ok(val),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(format!("miss: {namespace}/{key}")),
            Err(e) => bail!("cache get failed: {e}"),
        }
    }

    fn execute_put(&self, namespace: &str, key: &str, value_json: &str) -> Result<String> {
        // Auto-wrap non-JSON values as JSON strings.
        let value_owned;
        let value_json = match serde_json::from_str::<serde_json::Value>(value_json) {
            Ok(_) => value_json,
            Err(_) => {
                value_owned = serde_json::Value::String(value_json.to_string()).to_string();
                &value_owned
            }
        };

        let db = self.db.lock().map_err(|e| anyhow::anyhow!("lock: {e}"))?;
        db.execute(
            "INSERT INTO cache_entries (namespace, key, value_json)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(namespace, key) DO UPDATE SET value_json = excluded.value_json",
            rusqlite::params![namespace, key, value_json],
        )?;

        Ok(format!("ok: {namespace}/{key} ({} bytes)", value_json.len()))
    }

    fn execute_delete(&self, namespace: &str, key: &str) -> Result<String> {
        let db = self.db.lock().map_err(|e| anyhow::anyhow!("lock: {e}"))?;
        let affected = db.execute(
            "DELETE FROM cache_entries WHERE namespace = ?1 AND key = ?2",
            rusqlite::params![namespace, key],
        )?;

        if affected > 0 {
            Ok(format!("deleted: {namespace}/{key}"))
        } else {
            Ok(format!("not_found: {namespace}/{key}"))
        }
    }

    fn execute_list(&self, namespace: &str) -> Result<String> {
        let db = self.db.lock().map_err(|e| anyhow::anyhow!("lock: {e}"))?;
        let mut stmt = db.prepare(
            "SELECT key FROM cache_entries WHERE namespace = ?1 ORDER BY key",
        )?;
        let keys: Vec<String> = stmt
            .query_map(rusqlite::params![namespace], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();

        if keys.is_empty() {
            Ok(format!("{namespace}: (empty)"))
        } else {
            Ok(format!("{namespace}: {}", keys.join(", ")))
        }
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
                let key = call.arguments.get("key").and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("shared_cache get: missing 'key'"))?;
                self.execute_get(namespace, key)
            }
            "put" => {
                let key = call.arguments.get("key").and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("shared_cache put: missing 'key'"))?;
                let value = call.arguments.get("value").and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("shared_cache put: missing 'value'"))?;
                self.execute_put(namespace, key, value)
            }
            "delete" => {
                let key = call.arguments.get("key").and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("shared_cache delete: missing 'key'"))?;
                self.execute_delete(namespace, key)
            }
            "list" => self.execute_list(namespace),
            other => bail!("shared_cache: unknown operation '{other}'"),
        }
    }
}

