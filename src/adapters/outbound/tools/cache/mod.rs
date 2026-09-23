// src/adapters/outbound/tools/cache/mod.rs
//! Cache plugin — SQLite-backed shared workspace cache for agent coordination.
//!
//! Provides: `shared_cache`. Opt-in per agent via `AgentConfig.workspace_tools`
//! (the tool is advertised only when `"shared_cache"` is listed). Every
//! operation is gated by `ctx.scope.check_fs_write(ctx.workspace)` — the cache
//! DB lives under `<workspace>/.tengu/cache.db`.

use anyhow::Result;
use async_trait::async_trait;
use rusqlite::Connection;
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::domain::message::ToolDef;
use crate::ports::tool::{PluginCtx, Tool, ToolPlugin};

pub(crate) mod shared_cache;

pub(crate) use shared_cache::SharedCacheTool;

/// SQL that creates the cache table if missing.
const CREATE_TABLE: &str = "
CREATE TABLE IF NOT EXISTS cache_entries (
    namespace TEXT NOT NULL,
    key       TEXT NOT NULL,
    value_json TEXT NOT NULL,
    PRIMARY KEY (namespace, key)
)";

/// Open the workspace cache database, creating `.tengu/cache.db` if needed.
///
/// Shared with tests and `CachePlugin::tools`.
pub(crate) fn open_cache_db(workspace: &Path) -> Result<Connection> {
    let db_dir = workspace.join(".tengu");
    std::fs::create_dir_all(&db_dir)?;
    let db_path = db_dir.join("cache.db");

    let conn = Connection::open(&db_path)?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")?;
    conn.execute_batch(CREATE_TABLE)?;
    Ok(conn)
}

/// Tool definitions advertised by the cache plugin.
///
/// Referenced by the tool catalog (`tools/mod.rs`) to
/// advertise cache tools to the engine before the plugin is instantiated. The
/// caller gates inclusion on `AgentConfig.workspace_tools`.
pub(crate) fn tool_defs() -> Vec<ToolDef> {
    // Construct a throwaway tool to reuse the single source of truth for the
    // schema. The DB handle is never touched because we only need `definition()`.
    //
    // We open an in-memory DB so this function stays infallible (matches the
    // pre-migration `build_shared_cache_tools()` contract).
    let conn = Connection::open_in_memory().expect("in-memory sqlite cannot fail");
    let tool = SharedCacheTool::new(Arc::new(Mutex::new(conn)));
    vec![tool.definition().clone()]
}

/// Plugin grouping the shared-cache tool.
pub(crate) struct CachePlugin;

#[async_trait]
impl ToolPlugin for CachePlugin {
    fn name(&self) -> &'static str {
        "cache"
    }

    async fn tools(&self, ctx: &PluginCtx<'_>) -> Result<Vec<Arc<dyn Tool>>> {
        match open_cache_db(ctx.workspace) {
            Ok(conn) => Ok(vec![Arc::new(SharedCacheTool::new(Arc::new(Mutex::new(
                conn,
            ))))]),
            Err(e) => {
                tracing::warn!(error = %e, "Failed to open shared cache, tool disabled");
                Ok(vec![])
            }
        }
    }
}
