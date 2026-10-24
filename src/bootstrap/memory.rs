//! Memory wiring — builds the `MemoryManager` (disk vector store + embedder)
//! for an agent workspace.

use std::path::Path;
use std::sync::Arc;

use crate::adapters::outbound::memory::disk_vector::DiskVectorStore;
use crate::adapters::outbound::memory::embedder::Embedder;
use crate::ports::memory::VectorStore;
// Plugin set: `outbound::tools::catalog`. McpPlugin + SkillPlugin are
// registered here, outside the catalog — they need the config's
// `[[mcp_servers]]` / a skill registry.

// ---------------------------------------------------------------------------
// Memory subsystem initialization
// ---------------------------------------------------------------------------

/// Resolve the memory store path: `<workspace>/memory` when a workspace is
/// provided, else `[memory] store_path` (`~` expanded), else
/// `<TENGU_HOME>/memory`.
pub(crate) fn resolve_memory_store_path(
    memory_config: &crate::config::MemoryConfig,
    workspace: Option<&Path>,
) -> std::path::PathBuf {
    use crate::config::paths::{expand_tilde, resolve_tengu_home};
    match (workspace, memory_config.store_path.as_deref()) {
        (Some(ws), _) => ws.join("memory"),
        (None, Some(path)) => expand_tilde(Path::new(path)),
        (None, None) => resolve_tengu_home().join("memory"),
    }
}

/// Build a `MemoryManager` holding the `Embedder` + `VectorStore` pair.
///
/// Returns a manager without a backend when memory is disabled or the
/// vector stack fails to initialize, so channels can always hand a valid
/// `Arc<MemoryManager>` to the tool executor.
/// Async sibling of `build_memory_manager` for callers that already live
/// inside a tokio context (notably `run_agent_subprocess`, which can't take
/// a `&Runtime` arg without re-entrant block-on hazards). Backed by the
/// bincode `DiskVectorStore`, the only `VectorStore` impl.
pub(crate) async fn build_memory_manager_async(
    memory_config: &crate::config::MemoryConfig,
    workspace: Option<&Path>,
) -> Arc<crate::application::memory::manager::MemoryManager> {
    use crate::application::memory::manager::MemoryManager;

    let manager = Arc::new(MemoryManager::new());

    let Some((embedder, store)) = build_vector_stack_async(memory_config, workspace).await else {
        return manager;
    };

    manager.set_vector_backend(embedder, store).await;
    manager
}

/// Build the `Embedder` + `DiskVectorStore` pair from config. Returns `None`
/// if memory is disabled, `OPENROUTER_API_KEY` is missing, or the store fails
/// to open. Consumed by `build_memory_manager_async` only.
async fn build_vector_stack_async(
    memory_config: &crate::config::MemoryConfig,
    workspace: Option<&Path>,
) -> Option<(Arc<Embedder>, Arc<dyn VectorStore>)> {
    if !memory_config.enabled {
        return None;
    }
    let api_key = match std::env::var("OPENROUTER_API_KEY") {
        Ok(k) => k,
        Err(_) => {
            tracing::warn!("OPENROUTER_API_KEY not set, memory disabled");
            return None;
        }
    };
    let resolved_store_path = resolve_memory_store_path(memory_config, workspace);

    // The disk-backed bincode store is the only `VectorStore` impl:
    // `memory_config.backend` is not branched on.
    let store: Option<Arc<dyn VectorStore>> = DiskVectorStore::new(&resolved_store_path)
        .ok()
        .map(|s| Arc::new(s) as Arc<dyn VectorStore>);
    let store = store?;
    let embedder = Arc::new(Embedder::new(
        api_key,
        memory_config.embedding_model.clone(),
    ));
    Some((embedder, store))
}

/// Sync wrapper around `build_memory_manager_async`.
/// The backend wiring lives in the async function; this wrapper just
/// block_on's it.
pub(crate) fn build_memory_manager(
    memory_config: &crate::config::MemoryConfig,
    rt: &tokio::runtime::Runtime,
    workspace: Option<&Path>,
) -> Arc<crate::application::memory::manager::MemoryManager> {
    rt.block_on(build_memory_manager_async(memory_config, workspace))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An agent without a workspace keeps its vectors under `TENGU_HOME`
    /// unless `[memory] store_path` names another dir; a workspace wins.
    #[test]
    fn memory_store_defaults_under_tengu_home() {
        let mut memory = crate::config::MemoryConfig::default();
        let home = crate::config::paths::resolve_tengu_home();
        assert_eq!(
            resolve_memory_store_path(&memory, None),
            home.join("memory")
        );
        let ws = Path::new("/ws");
        assert_eq!(
            resolve_memory_store_path(&memory, Some(ws)),
            ws.join("memory")
        );
        memory.store_path = Some("~/elsewhere/mem".into());
        assert_eq!(
            resolve_memory_store_path(&memory, None),
            crate::config::paths::expand_tilde(Path::new("~/elsewhere/mem"))
        );
        assert!(!resolve_memory_store_path(&memory, None).starts_with("~"));
    }
}
