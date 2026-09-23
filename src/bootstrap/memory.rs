//! Memory wiring — builds the `MemoryManager` (builtin provider + disk vector
//! store + embedder) for an agent workspace.

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

/// Resolve the memory store path: workspace-local if a workspace is provided,
/// otherwise fall back to the global path from config (with tilde expansion).
pub(crate) fn resolve_memory_store_path(
    memory_config: &crate::config::MemoryConfig,
    workspace: Option<&Path>,
) -> std::path::PathBuf {
    match workspace {
        Some(ws) => ws.join("memory"),
        None => {
            let store_path_str = memory_config.store_path.replace(
                "~",
                &dirs_next::home_dir().unwrap_or_default().to_string_lossy(),
            );
            std::path::PathBuf::from(store_path_str)
        }
    }
}

// `resolve_qdrant_collection` was removed in Phase 6 along with the Qdrant
// backend — the disk-backed `VectorStore` does not use collection names.

/// Build a `MemoryManager` populated with a `BuiltinMemoryProvider` that
/// uses the new `Embedder` + `VectorStore` pair.
///
/// Returns a freshly-constructed manager (possibly empty) when memory is
/// disabled or the vector stack fails to initialize, so channels can
/// always hand a valid `Arc<MemoryManager>` to the orchestrator.
/// Workspace defaults to the current directory when `None` so the
/// `BuiltinMemoryProvider` has a real path to read AGENTS.md / MEMORY.md
/// / daily logs from.
/// Phase 7.6 — async sibling of `build_memory_manager` for callers that
/// already live inside a tokio context (notably `run_agent_subprocess`,
/// which can't take a `&Runtime` arg without re-entrant block-on hazards).
/// Returns the same fully-wired `MemoryManager` — backed by the bincode
/// `DiskVectorStore` (the only `VectorStore` impl since Phase 6).
pub(crate) async fn build_memory_manager_async(
    memory_config: &crate::config::MemoryConfig,
    workspace: Option<&Path>,
) -> Arc<crate::application::memory::manager::MemoryManager> {
    use crate::adapters::outbound::memory::builtin::BuiltinMemoryProvider;
    use crate::application::memory::manager::MemoryManager;

    let manager = Arc::new(MemoryManager::new());

    let Some((embedder, store)) = build_vector_stack_async(memory_config, workspace).await else {
        return manager;
    };

    let ws = workspace
        .map(Path::to_path_buf)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let provider = Box::new(BuiltinMemoryProvider::new(
        ws,
        Arc::clone(&store),
        Arc::clone(&embedder),
    ));

    manager.add_provider(provider).await;
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

    // The legacy `qdrant` backend was removed in Phase 6. The disk-backed
    // bincode store is the only `VectorStore` impl now — `memory_config.backend`
    // is no longer branched on.
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

/// Phase 7.7 refactor #4 — sync wrapper around `build_memory_manager_async`.
/// Pre-7.7 this duplicated the BuiltinMemoryProvider construction + the
/// `add_provider` / `set_vector_backend` call sequence. Now those live in
/// the async function exclusively; this wrapper just block_on's it.
pub(crate) fn build_memory_manager(
    memory_config: &crate::config::MemoryConfig,
    rt: &tokio::runtime::Runtime,
    workspace: Option<&Path>,
) -> Arc<crate::application::memory::manager::MemoryManager> {
    rt.block_on(build_memory_manager_async(memory_config, workspace))
}
