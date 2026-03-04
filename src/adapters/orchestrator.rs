//! Adapter wiring for fleet orchestrator bootstrap.

use crate::adapters::embedding::OpenRouterEmbeddingAdapter;
use crate::adapters::memory_store::DiskVectorMemoryStore;
use crate::adapters::memory_tool_executor::MemoryServiceHandle;
use crate::adapters::skill_source::FileSystemSkillSource;
use crate::adapters::system_prompt;
use crate::adapters::task_store::InMemoryTaskStore;
use crate::adapters::workspace_tools;
use crate::application::fleet_runtime::FleetRuntimeService;
use crate::application::heartbeat::run_heartbeat_loop;
use crate::application::skill_catalog;
use crate::application::workspace_tools_catalog::{build_memory_tools, build_workspace_tools};
use anyhow::Result;
use std::sync::Arc;
use tengu_core::config::Config;
use tengu_core::events::EventBus;
use tracing::info;

/// Boot the orchestrator: register fleet agents, start heartbeat loop.
///
/// This function spawns background tasks and never returns (blocks on heartbeat).
pub(crate) async fn boot_orchestrator(
    config: &Config,
    event_bus: &dyn EventBus,
) -> Result<()> {
    let orch_config = config
        .orchestrator
        .clone()
        .unwrap_or_default();

    if !orch_config.enabled {
        info!("Orchestrator disabled in config, skipping boot");
        return Ok(());
    }

    // Build shared memory handle for all fleet agents.
    let _memory_handle: Option<Arc<MemoryServiceHandle>> = if config.memory.enabled {
        match std::env::var("OPENROUTER_API_KEY") {
            Ok(api_key) => {
                let store_path_str = config.memory.store_path.replace(
                    "~",
                    &dirs_next::home_dir()
                        .unwrap_or_default()
                        .to_string_lossy(),
                );
                match DiskVectorMemoryStore::new(std::path::Path::new(&store_path_str)) {
                    Ok(store) => {
                        let embedding = OpenRouterEmbeddingAdapter::new(
                            api_key,
                            config.memory.embedding_model.clone(),
                        );
                        info!("Shared memory store initialized for fleet");
                        Some(Arc::new(MemoryServiceHandle {
                            embedding: Arc::new(embedding),
                            store: Arc::new(store),
                        }))
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Failed to init shared memory store");
                        None
                    }
                }
            }
            Err(_) => {
                tracing::warn!("OPENROUTER_API_KEY not set, fleet memory disabled");
                None
            }
        }
    } else {
        None
    };

    let task_store = InMemoryTaskStore::new();
    let mut fleet = FleetRuntimeService::new();

    // Register agents with roles, skills, and system prompts.
    for (agent_id, agent_config) in &config.agents {
        if let Some(ref role_str) = agent_config.role {
            match role_str.parse::<crate::domain::agent_role::AgentRole>() {
                Ok(role) => {
                    // Load skills and build system prompt if agent has a workspace.
                    let (system_prompt_str, tools) =
                        if let Some(ref ws_raw) = agent_config.workspace {
                            let ws = workspace_tools::expand_tilde(ws_raw);
                            let skill_source = FileSystemSkillSource::new(ws);
                            let mut all_tools = build_workspace_tools();
                            if _memory_handle.is_some() {
                                all_tools.extend(build_memory_tools());
                            }
                            let reserved: Vec<&str> =
                                all_tools.iter().map(|t| t.name.as_str()).collect();
                            let agent_skill_allowlist = agent_config.skills.as_deref();
                            let loaded = skill_catalog::load_skills_for_agent(
                                &skill_source,
                                &reserved,
                                agent_skill_allowlist,
                            );
                            all_tools.extend(loaded.tool_defs);

                            let skill_contexts: Vec<String> =
                                loaded.context_fragments.iter().map(|f| f.body.clone()).collect();
                            let prompt = system_prompt::build_system_prompt(
                                agent_config,
                                true,
                                &skill_contexts,
                                false,
                            );
                            (prompt, all_tools)
                        } else {
                            let prompt =
                                system_prompt::build_system_prompt(agent_config, false, &[], false);
                            (prompt, vec![])
                        };

                    let tool_count = tools.len();
                    fleet.register_agent(
                        agent_id.clone(),
                        role,
                        agent_config.engine.clone(),
                        system_prompt_str,
                        tools,
                    );
                    info!(
                        agent_id = %agent_id,
                        role = %role,
                        engine = %agent_config.engine,
                        tools = tool_count,
                        "Registered fleet agent"
                    );
                }
                Err(e) => {
                    tracing::warn!(agent_id = %agent_id, error = %e, "Skipping agent with invalid role");
                }
            }
        }
    }

    let agent_count = fleet.agents().len();
    info!(agents = agent_count, "Fleet initialized");

    if agent_count == 0 {
        anyhow::bail!("No agents with roles configured for orchestration");
    }

    let interval = std::time::Duration::from_secs(orch_config.heartbeat_interval_s);
    info!(
        interval_s = orch_config.heartbeat_interval_s,
        max_retries = orch_config.max_retries,
        "Starting heartbeat loop"
    );

    // Run heartbeat loop (blocks forever)
    run_heartbeat_loop(interval, &task_store, event_bus, orch_config.max_retries).await;

    Ok(())
}
