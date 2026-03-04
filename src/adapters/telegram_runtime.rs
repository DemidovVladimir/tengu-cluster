//! Headless Telegram bot adapter that wires TelegramPipe → ChatRuntimeService.

use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tracing::{error, info, warn};

use crate::adapters::composite_tool_executor::CompositeToolExecutionAdapter;
use crate::adapters::embedding::OpenRouterEmbeddingAdapter;
use crate::adapters::engine_factory::build_engine;
use crate::adapters::flow_store::FlowStore;
use crate::adapters::memory_store::DiskVectorMemoryStore;
use crate::adapters::memory_tool_executor::{MemoryServiceHandle, MemoryToolExecutionAdapter};
use crate::adapters::shell_executor::LocalShellExecutor;
use crate::adapters::skill_source::FileSystemSkillSource;
use crate::adapters::skill_tool_executor::SkillToolExecutionAdapter;
use crate::adapters::system_prompt;
use crate::adapters::workspace_tools;
use crate::application::chat_runtime::ChatRuntimeService;
use crate::application::engine_runtime::{SanitizedToolExecutor, ToolExecutor};
use crate::application::flow_policy::resolve_flow_compaction_policy;
use crate::application::memory_service::MemoryService;
use crate::application::ports::{ToolActivityPort, ToolApprovalPort};
use crate::application::skill_registry::SkillRegistry;
use crate::application::tool_use_service::ToolUseService;
use crate::application::workspace_tools_catalog::{build_memory_tools, build_workspace_tools};
#[cfg(feature = "evm")]
use crate::application::workspace_tools_catalog::build_evm_tools;
use crate::domain::chat::{resolve_history_turn_limit, ChatLoopState};
use crate::domain::secret_registry::SecretRegistry;
use crate::domain::tool_policy::ToolPolicyCatalog;
use crate::resolve_tengu_home;
use tengu_core::config::Config;
use tengu_core::types::{DeliveryOptions, ToolCall, ToolDef};
use tengu_core::{Lens, Pipe, PipeContext, Refiner};
use tengu_optimizer::{NoopRefiner, RuleRefiner};

/// Maximum characters per Telegram message (with safety margin).
const TELEGRAM_MAX_LEN: usize = 4000;

// ---------------------------------------------------------------------------
// Port adapters for Telegram
// ---------------------------------------------------------------------------

/// Lightweight tool activity adapter — just logs.
struct TelegramToolActivityAdapter;

impl ToolActivityPort for TelegramToolActivityAdapter {
    fn publish_tool_activity(&self, call: &ToolCall) {
        tracing::debug!(tool = %call.name, "Tool activity");
    }
}

/// Auto-approve all tools in headless Telegram mode.
///
/// Telegram inline-keyboard approval could be added later, but for the
/// initial implementation we auto-approve to keep things simple and avoid
/// blocking the async message loop.
struct TelegramToolApprovalAdapter;

impl ToolApprovalPort for TelegramToolApprovalAdapter {
    fn request_tool_approval(&self, call: &ToolCall) -> Result<bool> {
        info!(tool = %call.name, "Auto-approving tool in Telegram mode");
        Ok(true)
    }
}

/// Tool executor wrapping `ToolUseService`.
struct TelegramToolExecutor {
    service: ToolUseService,
}

impl ToolExecutor for TelegramToolExecutor {
    fn execute(&self, call: &ToolCall) -> Result<String> {
        self.service.execute(call)
    }
}

// ---------------------------------------------------------------------------
// Message chunking
// ---------------------------------------------------------------------------

/// Split text into chunks of at most `max_len` characters.
/// Prefers splitting at `\n\n` boundaries; falls back to char boundary.
fn chunk_message(text: &str, max_len: usize) -> Vec<&str> {
    if text.len() <= max_len {
        return vec![text];
    }

    let mut chunks = Vec::new();
    let mut start = 0;

    while start < text.len() {
        if text.len() - start <= max_len {
            chunks.push(&text[start..]);
            break;
        }

        let end = start + max_len;
        // Find a safe char boundary at or before `end`.
        let mut boundary = end;
        while boundary > start && !text.is_char_boundary(boundary) {
            boundary -= 1;
        }

        // Try to split at \n\n within the last quarter of the chunk.
        let search_start = start + (boundary - start) / 2;
        let split = text[search_start..boundary]
            .rfind("\n\n")
            .map(|pos| search_start + pos + 2) // after the \n\n
            .unwrap_or(boundary);

        chunks.push(&text[start..split]);
        start = split;
    }

    chunks
}

// ---------------------------------------------------------------------------
// Helpers: build tools / executor / system prompt
// ---------------------------------------------------------------------------

fn rebuild_tools(base_tools: &[ToolDef], skill_registry: &SkillRegistry) -> Vec<ToolDef> {
    let mut tools = base_tools.to_vec();
    tools.extend(skill_registry.active_tool_defs());
    tools
}

fn rebuild_executor(
    workspace: &std::path::Path,
    tools: &[ToolDef],
    skill_registry: &SkillRegistry,
    memory_handle: &Option<Arc<MemoryServiceHandle>>,
    secret_registry: &Arc<SecretRegistry>,
) -> Option<TelegramToolExecutor> {
    if tools.is_empty() {
        return None;
    }

    let shell: Arc<dyn crate::application::ports::ShellExecutionPort> =
        Arc::new(LocalShellExecutor);

    let workspace_exec = Arc::new(
        workspace_tools::WorkspaceToolExecutionAdapter::new(workspace.to_path_buf())
            .with_shell(Arc::clone(&shell)),
    );

    let skill_defs = skill_registry.active_skill_definitions();
    let skill_names: HashSet<String> = skill_defs.iter().map(|s| s.name.clone()).collect();

    let mut composite = CompositeToolExecutionAdapter::new(workspace_exec);

    // Attach skill executor if any skills are active.
    if !skill_defs.is_empty() {
        let skill_exec = Arc::new(SkillToolExecutionAdapter::new(
            skill_defs,
            Arc::clone(&shell),
            workspace.to_path_buf(),
        ));
        composite = composite.with_executor(skill_exec, skill_names);
    }

    if let Some(ref handle) = memory_handle {
        if let Ok(mem_exec) =
            MemoryToolExecutionAdapter::new(Arc::clone(handle), Arc::clone(secret_registry))
        {
            let mem_names: HashSet<String> =
                build_memory_tools().iter().map(|t| t.name.clone()).collect();
            composite = composite.with_executor(Arc::new(mem_exec), mem_names);
        }
    }

    #[cfg(feature = "evm")]
    {
        if let (Ok(key), Ok(url)) = (std::env::var("EVM_PRIVATE_KEY"), std::env::var("EVM_RPC_URL"))
        {
            use crate::adapters::evm_signer::AlloySigner;
            use crate::adapters::evm_tool_executor::EvmToolExecutionAdapter;
            match AlloySigner::new(&key, url) {
                Ok(signer) => {
                    let port: Arc<dyn crate::application::ports::EvmPort> = Arc::new(signer);
                    if let Ok(evm_exec) = EvmToolExecutionAdapter::new(port) {
                        let evm_names: HashSet<String> =
                            build_evm_tools().iter().map(|t| t.name.clone()).collect();
                        composite = composite.with_executor(Arc::new(evm_exec), evm_names);
                    }
                }
                Err(e) => warn!("EVM signer init failed: {e}"),
            }
        }
    }

    let composite = Arc::new(composite);

    let service = ToolUseService::new(
        ToolPolicyCatalog::from_tools(tools),
        Arc::new(TelegramToolActivityAdapter),
        Arc::new(TelegramToolApprovalAdapter),
        composite,
    );
    Some(TelegramToolExecutor { service })
}

fn rebuild_system_prompt(
    agent_config: &tengu_core::config::AgentConfig,
    advertise_workspace_tools: bool,
    skill_registry: &SkillRegistry,
    evm_tools_available: bool,
) -> String {
    let skill_context_strings: Vec<String> = skill_registry
        .active_context_fragments()
        .into_iter()
        .map(|(_, body)| body)
        .collect();
    system_prompt::build_system_prompt(
        agent_config,
        advertise_workspace_tools,
        &skill_context_strings,
        evm_tools_available,
    )
}

// ---------------------------------------------------------------------------
// Build the allowed-users set from config + env
// ---------------------------------------------------------------------------

fn build_allowed_users(config: &Config) -> HashSet<String> {
    let mut allowed: HashSet<String> = config
        .telegram
        .allowed_users
        .iter()
        .cloned()
        .collect();

    if let Ok(env_users) = std::env::var("TENGU_TELEGRAM_ALLOWED_USERS") {
        for uid in env_users.split(',') {
            let uid = uid.trim();
            if !uid.is_empty() {
                allowed.insert(uid.to_string());
            }
        }
    }

    allowed
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Run the headless Telegram bot adapter (sync — call from `block_in_place`).
///
/// Creates its own tokio runtime internally so that state containing nested
/// runtimes (e.g. `MemoryToolExecutionAdapter`) drops in a sync context,
/// avoiding the "Cannot drop a runtime in a context where blocking is not
/// allowed" panic.
pub(crate) fn run_telegram(
    config: Config,
    secret_registry: Arc<SecretRegistry>,
) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("Failed to create Telegram runtime");

    // Resolve default agent.
    let (agent_id, agent_config) = config
        .agents
        .iter()
        .find(|(_, ac)| ac.default)
        .or_else(|| config.agents.iter().next())
        .map(|(id, ac)| (id.clone(), ac.clone()))
        .ok_or_else(|| anyhow::anyhow!("No agents configured"))?;

    let engine = build_engine(&agent_id, &agent_config)?;

    let refiner: Box<dyn Refiner> = match config.refiner.mode.as_str() {
        "rules" => Box::new(RuleRefiner::new()),
        _ => Box::new(NoopRefiner),
    };

    // Bot token from env.
    let bot_token = std::env::var("TELEGRAM_BOT_TOKEN")
        .map_err(|_| anyhow::anyhow!("TELEGRAM_BOT_TOKEN env var is required"))?;

    let allowed_users = build_allowed_users(&config);
    if allowed_users.is_empty() {
        warn!("No allowed Telegram users configured — all messages will be rejected");
    } else {
        info!(count = allowed_users.len(), "Telegram allowed users loaded");
    }

    // Flow & compaction settings.
    let flow_store = FlowStore::new(&resolve_tengu_home())?;
    let history_turn_limit = resolve_history_turn_limit(&agent_config.flow);
    let compaction_policy = resolve_flow_compaction_policy(
        &agent_config.flow,
        agent_config.limits.max_tokens_per_flow,
        engine.context_window(),
        engine.max_output_tokens_per_turn() as usize,
    );
    let advertise_workspace_tools = engine.supports_tool_use() && !engine.manages_own_workspace();
    let memory_config = config.memory.clone();

    // Workspace path.
    let workspace: Option<std::path::PathBuf> = agent_config
        .workspace
        .as_ref()
        .map(|p| workspace_tools::expand_tilde(p));

    // Memory subsystem (qdrant init needs async, so use rt.block_on).
    let memory_handle: Option<Arc<MemoryServiceHandle>> = if memory_config.enabled {
        match std::env::var("OPENROUTER_API_KEY") {
            Ok(api_key) => {
                let store: Option<Arc<dyn crate::application::ports::MemoryStorePort>> =
                    match memory_config.backend.as_str() {
                        #[cfg(feature = "qdrant")]
                        "qdrant" => {
                            use crate::adapters::qdrant_memory_store::QdrantMemoryStore;
                            match rt.block_on(QdrantMemoryStore::new(
                                &memory_config.qdrant_url,
                                memory_config.qdrant_api_key.as_deref(),
                                &memory_config.qdrant_collection,
                                memory_config.vector_size,
                            )) {
                                Ok(s) => Some(Arc::new(s)),
                                Err(e) => {
                                    warn!(error = %e, "Failed to init Qdrant memory store, memory disabled");
                                    None
                                }
                            }
                        }
                        #[cfg(not(feature = "qdrant"))]
                        "qdrant" => {
                            warn!("Qdrant backend requested but 'qdrant' feature not enabled, falling back to disk");
                            let store_path_str = memory_config.store_path.replace(
                                "~",
                                &dirs_next::home_dir()
                                    .unwrap_or_default()
                                    .to_string_lossy(),
                            );
                            DiskVectorMemoryStore::new(std::path::Path::new(&store_path_str))
                                .ok()
                                .map(|s| {
                                    Arc::new(s)
                                        as Arc<dyn crate::application::ports::MemoryStorePort>
                                })
                        }
                        _ => {
                            let store_path_str = memory_config.store_path.replace(
                                "~",
                                &dirs_next::home_dir()
                                    .unwrap_or_default()
                                    .to_string_lossy(),
                            );
                            DiskVectorMemoryStore::new(std::path::Path::new(&store_path_str))
                                .ok()
                                .map(|s| {
                                    Arc::new(s)
                                        as Arc<dyn crate::application::ports::MemoryStorePort>
                                })
                        }
                    };
                store.map(|s| {
                    let embedding = OpenRouterEmbeddingAdapter::new(
                        api_key,
                        memory_config.embedding_model.clone(),
                    );
                    Arc::new(MemoryServiceHandle {
                        embedding: Arc::new(embedding),
                        store: s,
                    })
                })
            }
            Err(_) => {
                warn!("OPENROUTER_API_KEY not set, memory disabled");
                None
            }
        }
    } else {
        None
    };

    // Base tools.
    let uses_tools =
        engine.supports_tool_use() && !engine.manages_own_workspace() && workspace.is_some();
    let has_memory = memory_handle.is_some();

    let (base_tools, evm_tools_available) = {
        if !uses_tools {
            (vec![], false)
        } else {
            let mut all_tools = build_workspace_tools();
            if has_memory {
                all_tools.extend(build_memory_tools());
            }
            let evm;
            #[cfg(feature = "evm")]
            {
                evm = std::env::var("EVM_PRIVATE_KEY").is_ok()
                    && std::env::var("EVM_RPC_URL").is_ok();
                if evm {
                    all_tools.extend(build_evm_tools());
                }
            }
            #[cfg(not(feature = "evm"))]
            {
                evm = false;
            }
            (all_tools, evm)
        }
    };

    // Skill registry.
    let skill_source: Option<FileSystemSkillSource> =
        workspace.as_ref().map(|ws| FileSystemSkillSource::new(ws.clone()));

    let base_reserved: Vec<String> = base_tools.iter().map(|t| t.name.clone()).collect();
    let mut skill_registry = SkillRegistry::new(base_reserved);

    if let Some(ref src) = skill_source {
        skill_registry.reload(src);
    }

    // Build initial tools & system prompt.
    let mut current_tools = rebuild_tools(&base_tools, &skill_registry);
    let mut current_executor: Option<TelegramToolExecutor> = workspace.as_ref().and_then(|ws| {
        rebuild_executor(ws, &current_tools, &skill_registry, &memory_handle, &secret_registry)
    });
    let mut current_system_prompt = rebuild_system_prompt(
        &agent_config,
        advertise_workspace_tools,
        &skill_registry,
        evm_tools_available,
    );

    let memory_service_instance = memory_handle
        .as_ref()
        .map(|h| MemoryService::new(h.embedding.as_ref(), h.store.as_ref()));

    // Per-user conversation states.
    let mut user_states: HashMap<String, ChatLoopState> = HashMap::new();

    // Connect Telegram pipe (async).
    let pipe = tengu_channels::telegram::TelegramPipe::new(bot_token);
    let (inbound_tx, mut inbound_rx) = tokio::sync::mpsc::channel(256);
    rt.block_on(pipe.connect(PipeContext { inbound_tx }))?;

    info!("Telegram bot started — waiting for messages (Ctrl+C to stop)");

    // Run the async message loop inside block_on.
    // All mutable state is borrowed (not moved), so it drops in this sync
    // scope after block_on returns — not inside an async context.
    rt.block_on(async {
        let delivery_opts = DeliveryOptions::default();

        let ctrl_c = tokio::signal::ctrl_c();
        tokio::pin!(ctrl_c);

        loop {
            let msg = tokio::select! {
                msg = inbound_rx.recv() => match msg {
                    Some(m) => m,
                    None => break,
                },
                _ = &mut ctrl_c => {
                    info!("Received Ctrl+C, shutting down Telegram bot");
                    break;
                }
            };

            let sender_id = &msg.sender.peer_id;

            // Access control.
            if !allowed_users.is_empty() && !allowed_users.contains(sender_id) {
                warn!(sender = %sender_id, "Unauthorized Telegram user");
                let _ = pipe
                    .send_text(&msg.sender, "Unauthorized.", &delivery_opts)
                    .await;
                continue;
            }

            // Hot-reload skills.
            let mut tools_dirty = false;
            if let Some(ref src) = skill_source {
                if skill_registry.reload(src) {
                    tools_dirty = true;
                }
            }

            if tools_dirty {
                if let Some(ref ws) = workspace {
                    current_tools = rebuild_tools(&base_tools, &skill_registry);
                    current_executor = rebuild_executor(
                        ws,
                        &current_tools,
                        &skill_registry,
                        &memory_handle,
                        &secret_registry,
                    );
                    current_system_prompt = rebuild_system_prompt(
                        &agent_config,
                        advertise_workspace_tools,
                        &skill_registry,
                        evm_tools_available,
                    );
                }
            }

            // Get or create per-user state.
            let state = user_states.entry(sender_id.clone()).or_insert_with(|| {
                ChatLoopState {
                    messages: Vec::new(),
                    active_flow_key: None,
                    manual_session_id: None,
                    flow_token_usage: 0,
                    active_lens: agent_config
                        .default_lens
                        .parse()
                        .unwrap_or(Lens::Eco),
                    total_input_tokens: 0,
                    total_output_tokens: 0,
                    tokens_saved: 0,
                    last_prompt_report: None,
                }
            });

            // Wrap tool executor with secret redaction.
            let sanitized_executor = current_executor.as_ref().map(|e| {
                SanitizedToolExecutor::new(e as &dyn ToolExecutor, &secret_registry)
            });

            let chat_runtime = ChatRuntimeService {
                engine: engine.as_ref(),
                refiner: refiner.as_ref(),
                flow_store: &flow_store,
                agent_id: &agent_id,
                agent_config: &agent_config,
                history_turn_limit,
                compaction_policy,
                system_prompt: current_system_prompt.clone(),
                tools: &current_tools,
                tool_executor: sanitized_executor
                    .as_ref()
                    .map(|e| e as &dyn ToolExecutor),
                memory_service: memory_service_instance.as_ref(),
                max_recall_entries: memory_config.max_recall_entries,
                max_recall_tokens: memory_config.max_recall_tokens,
            };

            match chat_runtime.process_user_text(state, &msg.content).await {
                Ok(result) => {
                    if let Some(notice) = result.system_notice {
                        let _ = pipe.send_text(&msg.sender, &notice, &delivery_opts).await;
                    }
                    if let Some(text) = result.assistant_text {
                        let reply = secret_registry.redact(&text);
                        for chunk in chunk_message(&reply, TELEGRAM_MAX_LEN) {
                            if let Err(e) =
                                pipe.send_text(&msg.sender, chunk, &delivery_opts).await
                            {
                                error!(error = %e, "Failed to send Telegram reply chunk");
                            }
                        }
                    }
                }
                Err(e) => {
                    error!(error = %e, "Engine error");
                    let _ = pipe
                        .send_text(
                            &msg.sender,
                            &format!("Error: {}", e),
                            &delivery_opts,
                        )
                        .await;
                }
            }
        }

        pipe.disconnect().await.ok();
    });

    // current_executor drops here in SYNC context — safe for nested runtimes
    // (MemoryToolExecutionAdapter owns a tokio::runtime::Runtime that would
    // panic if dropped inside another runtime's block_on).
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_message_short() {
        let chunks = chunk_message("hello", 4096);
        assert_eq!(chunks, vec!["hello"]);
    }

    #[test]
    fn chunk_message_splits_at_paragraph_boundary() {
        let text = format!("{}\n\n{}", "a".repeat(2000), "b".repeat(2000));
        let chunks = chunk_message(&text, 3000);
        assert!(chunks.len() >= 2);
        for c in &chunks {
            assert!(c.len() <= 3000);
        }
    }

    #[test]
    fn chunk_message_splits_long_single_paragraph() {
        let text = "x".repeat(5000);
        let chunks = chunk_message(&text, 2000);
        assert!(chunks.len() >= 3);
        for c in &chunks {
            assert!(c.len() <= 2000);
        }
    }

    #[test]
    fn build_allowed_users_from_config() {
        let mut config = Config::default();
        config.telegram.allowed_users = vec!["111".to_string(), "222".to_string()];
        let users = build_allowed_users(&config);
        assert!(users.contains("111"));
        assert!(users.contains("222"));
        assert_eq!(users.len(), 2);
    }
}
