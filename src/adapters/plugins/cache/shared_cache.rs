// src/adapters/plugins/cache/shared_cache.rs
//! `shared_cache` tool — SQLite-backed workspace cache for agent data exchange.
//!
//! Operations: `get`, `put`, `delete`, `list`. All operations are namespaced to
//! keep per-agent / per-skill data isolated inside a shared workspace DB.
//!
//! Backed by `<workspace>/.tengu/cache.db`. Opened once by `CachePlugin::tools`
//! so every call reuses the same connection behind a `Mutex`.

use anyhow::{bail, Result};
use async_trait::async_trait;
use rusqlite::Connection;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

use crate::adapters::tool_plugin::{Tool, ToolCtx, ToolOutput};
use crate::adapters::types::ToolDef;

/// Reserved tool name — skills cannot shadow this.
pub(crate) const SHARED_CACHE_TOOL_NAME: &str = "shared_cache";

pub(crate) struct SharedCacheTool {
    def: ToolDef,
    db: Arc<Mutex<Connection>>,
}

impl SharedCacheTool {
    pub(crate) fn new(db: Arc<Mutex<Connection>>) -> Self {
        Self {
            def: ToolDef::new(
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
            ),
            db,
        }
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

        Ok(format!(
            "ok: {namespace}/{key} ({} bytes)",
            value_json.len()
        ))
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
        let mut stmt =
            db.prepare("SELECT key FROM cache_entries WHERE namespace = ?1 ORDER BY key")?;
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

#[async_trait]
impl Tool for SharedCacheTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        // The cache DB lives under `<workspace>/.tengu/cache.db`; gate on the
        // workspace write permission since every op touches that file.
        ctx.scope.check_fs_write(ctx.workspace)?;

        let operation = args
            .get("operation")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("shared_cache: missing 'operation'"))?;

        let namespace = args
            .get("namespace")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("shared_cache: missing 'namespace'"))?;

        let result = match operation {
            "get" => {
                let key = args
                    .get("key")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("shared_cache get: missing 'key'"))?;
                self.execute_get(namespace, key)?
            }
            "put" => {
                let key = args
                    .get("key")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("shared_cache put: missing 'key'"))?;
                let value = args
                    .get("value")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("shared_cache put: missing 'value'"))?;
                self.execute_put(namespace, key, value)?
            }
            "delete" => {
                let key = args
                    .get("key")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("shared_cache delete: missing 'key'"))?;
                self.execute_delete(namespace, key)?
            }
            "list" => self.execute_list(namespace)?,
            other => bail!("shared_cache: unknown operation '{other}'"),
        };

        Ok(ToolOutput::from(result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::plugins::cache::open_cache_db;
    use crate::adapters::plugins::workspace::test_support::TestHarness;
    use crate::adapters::ports::ToolScope;
    use tempfile::TempDir;

    fn make_tool(workspace: &std::path::Path) -> SharedCacheTool {
        let db = open_cache_db(workspace).expect("open cache db");
        SharedCacheTool::new(Arc::new(Mutex::new(db)))
    }

    #[tokio::test]
    async fn shared_cache_put_then_get_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let harness = TestHarness::new(tmp.path());
        let tool = make_tool(tmp.path());

        let put = tool
            .execute(
                &json!({
                    "operation": "put",
                    "namespace": "ns",
                    "key": "k",
                    "value": "\"hello\""
                }),
                &harness.ctx(),
            )
            .await
            .unwrap();
        assert!(
            put.text.starts_with("ok: ns/k"),
            "unexpected put: {}",
            put.text
        );

        let get = tool
            .execute(
                &json!({
                    "operation": "get",
                    "namespace": "ns",
                    "key": "k"
                }),
                &harness.ctx(),
            )
            .await
            .unwrap();
        assert_eq!(get.text, "\"hello\"");
    }

    #[tokio::test]
    async fn shared_cache_scope_denies_outside_workspace() {
        // fs_roots points at a different tempdir, so the workspace path is not
        // under any allowed root — check_fs_write must deny.
        let tmp_ws = TempDir::new().unwrap();
        let tmp_root = TempDir::new().unwrap();
        let scope = ToolScope {
            fs_roots: vec![tmp_root.path().to_path_buf()],
            ..Default::default()
        };
        let harness = TestHarness::with_scope(tmp_ws.path(), scope);
        let tool = make_tool(tmp_ws.path());

        let result = tool
            .execute(
                &json!({
                    "operation": "list",
                    "namespace": "ns"
                }),
                &harness.ctx(),
            )
            .await;
        assert!(result.is_err(), "expected scope denial, got: {:?}", result);
    }

    #[tokio::test]
    async fn shared_cache_unknown_operation_errors() {
        let tmp = TempDir::new().unwrap();
        let harness = TestHarness::new(tmp.path());
        let tool = make_tool(tmp.path());

        let result = tool
            .execute(
                &json!({
                    "operation": "reboot",
                    "namespace": "ns"
                }),
                &harness.ctx(),
            )
            .await;
        assert!(
            result.is_err(),
            "expected error for unknown op, got: {:?}",
            result
        );
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("unknown operation"),
            "unexpected error message: {}",
            msg
        );
    }
}
