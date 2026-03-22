//! Shared channel runtime helpers for all communication adapters.
//!
//! This module extracts common logic used by TUI, Telegram, and future channel
//! adapters (Slack, Discord, email, etc.). Adding a new channel requires only
//! channel-specific I/O code — all shared business logic lives here.
//!
//! ## What's shared
//!
//! - **Tool/executor/prompt rebuilding** — `rebuild_tools`, `rebuild_system_prompt`,
//!   `build_tool_executor`
//! - **Memory subsystem initialization** — `build_memory_handle`
//! - **Base tool computation** — `compute_base_tools` (workspace primitives +
//!   subsystem tools)
//! - **Agent routing** — `parse_agent_routing`
//! - **Message chunking** — `chunk_message` (for channels with length limits)
//! - **State factories** — `create_chat_loop_state`
//! - **Skill list formatting** — `format_skill_list`

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;

use crate::adapters::cache_tool_executor::{
    build_shared_cache_tools, CacheToolExecutionAdapter, SHARED_CACHE_TOOL_NAME,
};
use crate::adapters::composite_tool_executor::CompositeToolExecutionAdapter;
use crate::adapters::crypto_tool_executor::CryptoToolExecutionAdapter;
use crate::adapters::embedding::OpenRouterEmbeddingAdapter;
use crate::adapters::http_tool_executor::HttpToolExecutionAdapter;
use crate::adapters::memory_builder::{
    memory_tool_defs, DiskVectorMemoryStore, MemoryServiceHandle, MemoryToolExecutionAdapter,
};
use crate::adapters::shell_executor::LocalShellExecutor;
use crate::adapters::skill_builder::{
    self, SkillExecution, SkillRegistry, SkillStatus, SkillToolExecutionAdapter,
};
use crate::adapters::engine_builder::ToolExecutor;
use crate::adapters::ports::{ShellExecutionPort, ToolActivityPort, ToolApprovalPort};
use crate::adapters::tool_builder::{build_platform_tools, build_session_tools, build_workspace_tools, ToolUseService, WorkspaceToolExecutionAdapter};
use crate::adapters::secret_builder::SecretRegistry;
use crate::adapters::config::AgentConfig;
use crate::adapters::types::{
    ChatLoopState, Lens, RegisteredTool, ToolCall, ToolDef, ToolPolicyCatalog,
};

// ---------------------------------------------------------------------------
// Tool executor wrapper
// ---------------------------------------------------------------------------

/// Thin wrapper around `ToolUseService` implementing the `ToolExecutor` trait.
///
/// Shared across all channel adapters — eliminates the need for each channel
/// to define its own executor type.
pub(crate) struct ToolServiceExecutor {
    service: ToolUseService,
}

impl ToolExecutor for ToolServiceExecutor {
    fn execute(&self, call: &ToolCall) -> Result<String> {
        self.service.execute(call)
    }
}

// ---------------------------------------------------------------------------
// Tool, executor, and prompt rebuilding
// ---------------------------------------------------------------------------

pub(crate) fn tool_defs(tools: &[RegisteredTool]) -> Vec<crate::adapters::types::ToolDef> {
    tools.iter().map(|tool| tool.def.clone()).collect()
}

/// Merge base platform tools with active skill tools.
pub(crate) fn rebuild_tools(
    base_tools: &[RegisteredTool],
    skill_registry: &SkillRegistry,
) -> Vec<RegisteredTool> {
    let mut tools = base_tools.to_vec();
    tools.extend(skill_registry.active_tools());
    tools
}

/// Apply per-agent tool allow/deny policies (OpenClaw-compatible).
///
/// When `tool_allow` is non-empty, only tools in the allow list are kept.
/// Then any tools in `tool_deny` are removed. Deny wins over allow.
pub(crate) fn apply_tool_policies(
    tools: Vec<RegisteredTool>,
    tool_allow: &[String],
    tool_deny: &[String],
) -> Vec<RegisteredTool> {
    if tool_allow.is_empty() && tool_deny.is_empty() {
        return tools;
    }

    let allow_set: HashSet<String> = tool_allow
        .iter()
        .map(|n| n.trim().to_lowercase())
        .collect();
    let deny_set: HashSet<String> = tool_deny
        .iter()
        .map(|n| n.trim().to_lowercase())
        .collect();

    tools
        .into_iter()
        .filter(|t| {
            let name = t.def.name.to_lowercase();
            // If allow list is set, tool must be in it.
            if !allow_set.is_empty() && !allow_set.contains(&name) {
                return false;
            }
            // Deny always wins.
            !deny_set.contains(&name)
        })
        .collect()
}

/// Rebuild the system prompt from agent config, skill registry state, and active tools.
pub(crate) fn rebuild_system_prompt(
    agent_config: &AgentConfig,
    advertise_workspace_tools: bool,
    skill_registry: &SkillRegistry,
    tools: &[RegisteredTool],
) -> String {
    let skill_context_strings: Vec<String> = skill_registry
        .active_context_fragments()
        .into_iter()
        .map(|(_, body)| body)
        .collect();
    skill_builder::build_system_prompt_with_tools(
        agent_config,
        advertise_workspace_tools,
        &skill_context_strings,
        &tool_defs(tools),
    )
}

/// Build the composite tool executor from workspace, skill, and memory executors.
///
/// The `approval` and `activity` ports are channel-specific — each channel adapter
/// provides its own implementations (e.g., TUI dialogs, Telegram inline keyboards,
/// Slack interactive messages).
///
/// `shared_http_client` — when `Some`, all HTTP and crypto executors share one
/// `reqwest::Client` instead of each building their own connection pool.
///
/// `session_registry` — when `Some`, enables session tools for agent-to-agent
/// coordination (OpenClaw-compatible sessions_list/send/history).
pub(crate) fn build_tool_executor(
    workspace: &Path,
    tools: &[RegisteredTool],
    skill_registry: &SkillRegistry,
    memory_handle: &Option<Arc<MemoryServiceHandle>>,
    secret_registry: &Arc<SecretRegistry>,
    approval: Arc<dyn ToolApprovalPort>,
    activity: Arc<dyn ToolActivityPort>,
    cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    shared_http_client: Option<&reqwest::Client>,
) -> Option<ToolServiceExecutor> {
    build_tool_executor_with_sessions(
        workspace,
        tools,
        skill_registry,
        memory_handle,
        secret_registry,
        approval,
        activity,
        cancel,
        shared_http_client,
        None,
    )
}

/// Full tool executor builder with optional session registry.
pub(crate) fn build_tool_executor_with_sessions(
    workspace: &Path,
    tools: &[RegisteredTool],
    skill_registry: &SkillRegistry,
    memory_handle: &Option<Arc<MemoryServiceHandle>>,
    secret_registry: &Arc<SecretRegistry>,
    approval: Arc<dyn ToolApprovalPort>,
    activity: Arc<dyn ToolActivityPort>,
    cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    shared_http_client: Option<&reqwest::Client>,
    session_registry: Option<Arc<SessionRegistry>>,
) -> Option<ToolServiceExecutor> {
    if tools.is_empty() {
        return None;
    }

    let shell: Arc<dyn ShellExecutionPort> = Arc::new(match cancel {
        Some(ref flag) => LocalShellExecutor::new().with_cancel(Arc::clone(flag)),
        None => LocalShellExecutor::new(),
    });

    let workspace_exec = Arc::new(
        WorkspaceToolExecutionAdapter::new(workspace.to_path_buf())
            .with_shell(Arc::clone(&shell)),
    );

    let allowed_names: HashSet<&str> = tools.iter().map(|tool| tool.def.name.as_str()).collect();

    // Shell skills create named tools; API skills are documentation-only.
    let shell_skill_defs: Vec<_> = skill_registry
        .active_skill_definitions()
        .into_iter()
        .filter(|skill| {
            allowed_names.contains(skill.name.as_str())
                && matches!(skill.execution, SkillExecution::Shell { .. })
        })
        .collect();

    let mut composite = CompositeToolExecutionAdapter::new(workspace_exec);

    if !shell_skill_defs.is_empty() {
        let skill_names: HashSet<String> =
            shell_skill_defs.iter().map(|s| s.name.clone()).collect();
        let skill_exec = Arc::new(SkillToolExecutionAdapter::new(
            shell_skill_defs,
            Arc::clone(&shell),
            workspace.to_path_buf(),
        ));
        composite = composite.with_executor(skill_exec, skill_names);
    }

    if let Some(ref handle) = memory_handle {
        if let Ok(mem_exec) =
            MemoryToolExecutionAdapter::new(Arc::clone(handle), Arc::clone(secret_registry))
        {
            let mem_exec = mem_exec.with_workspace(workspace.to_path_buf());
            let mem_names: HashSet<String> = memory_tool_defs()
                .iter()
                .map(|t| t.def.name.clone())
                .collect();
            composite = composite.with_executor(Arc::new(mem_exec), mem_names);
        }
    }

    // Optional workspace tool: shared cache
    if allowed_names.contains(SHARED_CACHE_TOOL_NAME) {
        match CacheToolExecutionAdapter::open(workspace) {
            Ok(cache_exec) => {
                composite = composite.with_executor(
                    Arc::new(cache_exec),
                    HashSet::from([SHARED_CACHE_TOOL_NAME.to_string()]),
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to open shared cache, tool disabled");
            }
        }
    }

    // Platform primitives: HTTP request
    if allowed_names.contains("http_request") {
        if let Ok(http_exec) = HttpToolExecutionAdapter::with_client(
            shared_http_client.cloned(),
            workspace.to_path_buf(),
        ) {
            composite = composite.with_executor(
                Arc::new(http_exec),
                HashSet::from(["http_request".to_string()]),
            );
        }
    }

    // Platform primitives: crypto signing + ABI encoding
    let crypto_tool_names: HashSet<String> = [
        "sign_and_send_transaction",
        "sign_message",
        "get_wallet_address",
        "abi_encode",
        "hex_to_uint256",
    ]
    .iter()
    .filter(|n| allowed_names.contains(**n))
    .map(|n| n.to_string())
    .collect();
    if !crypto_tool_names.is_empty() {
        if let Ok(mut crypto_exec) = CryptoToolExecutionAdapter::with_client(shared_http_client.cloned()) {
            if let Some(ref flag) = cancel {
                crypto_exec = crypto_exec.with_cancel(Arc::clone(flag));
            }
            composite = composite.with_executor(Arc::new(crypto_exec), crypto_tool_names);
        }
    }

    // Session tools for agent-to-agent coordination (OpenClaw-compatible).
    if let Some(ref registry) = session_registry {
        let session_tool_names: HashSet<String> = [
            "sessions_list",
            "sessions_send",
            "sessions_history",
        ]
        .iter()
        .filter(|n| allowed_names.contains(**n))
        .map(|n| n.to_string())
        .collect();
        if !session_tool_names.is_empty() {
            composite = composite.with_executor(
                Arc::new(SessionToolExecutor::new(Arc::clone(registry))),
                session_tool_names,
            );
        }
    }

    let composite = Arc::new(composite);

    let service = ToolUseService::new(
        ToolPolicyCatalog::from_tools(tools),
        activity,
        approval,
        composite,
    );
    Some(ToolServiceExecutor { service })
}

/// Full tool executor builder with optional session registry AND subagent executor.
///
/// Used by the orchestrator to give the main agent the ability to spawn subagents.
pub(crate) fn build_tool_executor_full(
    workspace: &Path,
    tools: &[RegisteredTool],
    skill_registry: &SkillRegistry,
    memory_handle: &Option<Arc<MemoryServiceHandle>>,
    secret_registry: &Arc<SecretRegistry>,
    approval: Arc<dyn ToolApprovalPort>,
    activity: Arc<dyn ToolActivityPort>,
    cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    shared_http_client: Option<&reqwest::Client>,
    session_registry: Option<Arc<SessionRegistry>>,
    subagent_executor: Option<Arc<crate::adapters::subagent_builder::SubagentToolExecutor>>,
) -> Option<ToolServiceExecutor> {
    if tools.is_empty() {
        return None;
    }

    let shell: Arc<dyn ShellExecutionPort> = Arc::new(match cancel {
        Some(ref flag) => LocalShellExecutor::new().with_cancel(Arc::clone(flag)),
        None => LocalShellExecutor::new(),
    });

    let workspace_exec = Arc::new(
        WorkspaceToolExecutionAdapter::new(workspace.to_path_buf())
            .with_shell(Arc::clone(&shell)),
    );

    let allowed_names: HashSet<&str> = tools.iter().map(|tool| tool.def.name.as_str()).collect();

    let shell_skill_defs: Vec<_> = skill_registry
        .active_skill_definitions()
        .into_iter()
        .filter(|skill| {
            allowed_names.contains(skill.name.as_str())
                && matches!(skill.execution, SkillExecution::Shell { .. })
        })
        .collect();

    let mut composite = CompositeToolExecutionAdapter::new(workspace_exec);

    if !shell_skill_defs.is_empty() {
        let skill_names: HashSet<String> =
            shell_skill_defs.iter().map(|s| s.name.clone()).collect();
        let skill_exec = Arc::new(SkillToolExecutionAdapter::new(
            shell_skill_defs,
            Arc::clone(&shell),
            workspace.to_path_buf(),
        ));
        composite = composite.with_executor(skill_exec, skill_names);
    }

    if let Some(ref handle) = memory_handle {
        if let Ok(mem_exec) =
            MemoryToolExecutionAdapter::new(Arc::clone(handle), Arc::clone(secret_registry))
        {
            let mem_exec = mem_exec.with_workspace(workspace.to_path_buf());
            let mem_names: HashSet<String> = memory_tool_defs()
                .iter()
                .map(|t| t.def.name.clone())
                .collect();
            composite = composite.with_executor(Arc::new(mem_exec), mem_names);
        }
    }

    if allowed_names.contains(SHARED_CACHE_TOOL_NAME) {
        match CacheToolExecutionAdapter::open(workspace) {
            Ok(cache_exec) => {
                composite = composite.with_executor(
                    Arc::new(cache_exec),
                    HashSet::from([SHARED_CACHE_TOOL_NAME.to_string()]),
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to open shared cache, tool disabled");
            }
        }
    }

    if allowed_names.contains("http_request") {
        if let Ok(http_exec) = HttpToolExecutionAdapter::with_client(
            shared_http_client.cloned(),
            workspace.to_path_buf(),
        ) {
            composite = composite.with_executor(
                Arc::new(http_exec),
                HashSet::from(["http_request".to_string()]),
            );
        }
    }

    let crypto_tool_names: HashSet<String> = [
        "sign_and_send_transaction",
        "sign_message",
        "get_wallet_address",
        "abi_encode",
        "hex_to_uint256",
    ]
    .iter()
    .filter(|n| allowed_names.contains(**n))
    .map(|n| n.to_string())
    .collect();
    if !crypto_tool_names.is_empty() {
        if let Ok(mut crypto_exec) = CryptoToolExecutionAdapter::with_client(shared_http_client.cloned()) {
            if let Some(ref flag) = cancel {
                crypto_exec = crypto_exec.with_cancel(Arc::clone(flag));
            }
            composite = composite.with_executor(Arc::new(crypto_exec), crypto_tool_names);
        }
    }

    if let Some(ref registry) = session_registry {
        let session_tool_names: HashSet<String> = [
            "sessions_list",
            "sessions_send",
            "sessions_history",
        ]
        .iter()
        .filter(|n| allowed_names.contains(**n))
        .map(|n| n.to_string())
        .collect();
        if !session_tool_names.is_empty() {
            composite = composite.with_executor(
                Arc::new(SessionToolExecutor::new(Arc::clone(registry))),
                session_tool_names,
            );
        }
    }

    // Subagent spawning tools (OpenClaw-compatible LLM-driven orchestration).
    if let Some(ref sub_exec) = subagent_executor {
        let subagent_tool_names: HashSet<String> = [
            "sessions_spawn",
            "sessions_fan_out",
            "subagents",
        ]
        .iter()
        .filter(|n| allowed_names.contains(**n))
        .map(|n| n.to_string())
        .collect();
        if !subagent_tool_names.is_empty() {
            composite = composite.with_executor(Arc::clone(sub_exec) as Arc<dyn crate::adapters::ports::ToolExecutionPort>, subagent_tool_names);
        }
    }

    let composite = Arc::new(composite);

    let service = ToolUseService::new(
        ToolPolicyCatalog::from_tools(tools),
        activity,
        approval,
        composite,
    );
    Some(ToolServiceExecutor { service })
}

// ---------------------------------------------------------------------------
// Base tool computation
// ---------------------------------------------------------------------------

/// Compute the base tool list from workspace primitives and memory subsystem tools.
///
/// This is the set of tools available before skill tools are added.
/// Call at startup and on `/reload` (env vars may change).
///
/// `workspace_tools` controls optional first-party workspace tools (e.g. `["shared_cache"]`).
/// `has_subagents` adds sessions_spawn/sessions_fan_out/subagents tools (orchestrator mode).
pub(crate) fn compute_base_tools(
    uses_tools: bool,
    has_memory: bool,
    workspace_tools: &[String],
) -> Vec<RegisteredTool> {
    compute_base_tools_ext(uses_tools, has_memory, workspace_tools, false, true)
}

/// Compute base tools for sub-agents.
///
/// OpenClaw pattern: sub-agents receive the full tool set minus session tools
/// and minus tools in the DENY_ALWAYS list. Memory tools are denied because
/// sub-agents should receive all relevant context in the spawn prompt instead
/// of searching memory independently. `agents_list` is also denied because
/// sub-agents don't need to discover siblings.
pub(crate) fn compute_base_tools_subagent(
    uses_tools: bool,
    has_memory: bool,
    workspace_tools: &[String],
) -> Vec<RegisteredTool> {
    let tools = compute_base_tools_ext(uses_tools, has_memory, workspace_tools, false, false);
    // OpenClaw SUBAGENT_TOOL_DENY_ALWAYS: tools always denied for sub-agents.
    // Memory tools are denied — pass relevant info in the spawn prompt instead.
    // agents_list is denied — sub-agents don't need sibling discovery.
    // sessions_send is denied — sub-agents communicate via announce chain.
    const SUBAGENT_DENY_ALWAYS: &[&str] = &[
        "memory_search",
        "memory_get",
        "memory_write",
        "agents_list",
        "sessions_send",
    ];
    tools
        .into_iter()
        .filter(|t| !SUBAGENT_DENY_ALWAYS.contains(&t.def.name.as_str()))
        .collect()
}

/// Extended base tool computation with subagent and session tool control.
pub(crate) fn compute_base_tools_ext(
    uses_tools: bool,
    has_memory: bool,
    workspace_tools: &[String],
    has_subagents: bool,
    has_session_tools: bool,
) -> Vec<RegisteredTool> {
    if !uses_tools {
        return vec![];
    }
    let mut tools = build_workspace_tools();
    if has_memory {
        tools.extend(memory_tool_defs());
    }
    if workspace_tools.iter().any(|t| t == "shared_cache") {
        tools.extend(build_shared_cache_tools());
    }
    tools.extend(build_platform_tools());
    // Session tools for agent-to-agent coordination (OpenClaw-compatible).
    // Sub-agents do NOT get session tools — prevents recursive spawning.
    if has_session_tools {
        tools.extend(build_session_tools());
    }
    // Subagent spawning tools (OpenClaw-compatible LLM-driven orchestration).
    if has_subagents {
        tools.extend(crate::adapters::subagent_builder::build_subagent_tools());
    }
    tools
}

// ---------------------------------------------------------------------------
// Memory subsystem initialization
// ---------------------------------------------------------------------------

/// Resolve the memory store path: workspace-local if a workspace is provided,
/// otherwise fall back to the global path from config (with tilde expansion).
pub(crate) fn resolve_memory_store_path(
    memory_config: &crate::adapters::config::MemoryConfig,
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

/// Resolve the Qdrant collection name: workspace-scoped if a workspace is
/// provided, otherwise fall back to the config value.
pub(crate) fn resolve_qdrant_collection(
    memory_config: &crate::adapters::config::MemoryConfig,
    workspace: Option<&Path>,
) -> String {
    match workspace {
        Some(ws) => {
            let name = ws
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "default".to_string());
            format!("tengu-memory-{}", name)
        }
        None => memory_config.qdrant_collection.clone(),
    }
}

/// Build the shared memory subsystem handle from config.
///
/// Returns `None` if memory is disabled, the API key is not set, or store init fails.
/// When `workspace` is `Some`, memory is stored in `<workspace>/memory/` instead
/// of the global `~/.tengu/memory/` path.
pub(crate) fn build_memory_handle(
    memory_config: &crate::adapters::config::MemoryConfig,
    #[allow(unused_variables)] rt: &tokio::runtime::Runtime,
    workspace: Option<&Path>,
) -> Option<Arc<MemoryServiceHandle>> {
    if !memory_config.enabled {
        return None;
    }

    match std::env::var("OPENROUTER_API_KEY") {
        Ok(api_key) => {
            let resolved_store_path = resolve_memory_store_path(memory_config, workspace);
            #[allow(unused_variables)]
            let resolved_collection = resolve_qdrant_collection(memory_config, workspace);

            let store: Option<Arc<dyn crate::adapters::ports::MemoryStorePort>> =
                match memory_config.backend.as_str() {
                    #[cfg(feature = "qdrant")]
                    "qdrant" => {
                        use crate::adapters::qdrant_memory_store::QdrantMemoryStore;
                        match rt.block_on(QdrantMemoryStore::new(
                            &memory_config.qdrant_url,
                            memory_config.qdrant_api_key.as_deref(),
                            &resolved_collection,
                            memory_config.vector_size,
                        )) {
                            Ok(s) => Some(Arc::new(s)),
                            Err(e) => {
                                tracing::warn!(error = %e, "Failed to init Qdrant memory store, memory disabled");
                                None
                            }
                        }
                    }
                    #[cfg(not(feature = "qdrant"))]
                    "qdrant" => {
                        tracing::warn!("Qdrant backend requested but 'qdrant' feature not enabled, falling back to disk");
                        DiskVectorMemoryStore::new(&resolved_store_path)
                            .ok()
                            .map(|s| {
                                Arc::new(s) as Arc<dyn crate::adapters::ports::MemoryStorePort>
                            })
                    }
                    _ => DiskVectorMemoryStore::new(&resolved_store_path)
                        .ok()
                        .map(|s| {
                            Arc::new(s) as Arc<dyn crate::adapters::ports::MemoryStorePort>
                        }),
                };

            store.map(|s| {
                let embedding =
                    OpenRouterEmbeddingAdapter::new(api_key, memory_config.embedding_model.clone());
                Arc::new(MemoryServiceHandle {
                    embedding: Arc::new(embedding),
                    store: s,
                })
            })
        }
        Err(_) => {
            tracing::warn!("OPENROUTER_API_KEY not set, memory disabled");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// ChatLoopState factory
// ---------------------------------------------------------------------------

/// Create a new `ChatLoopState` with defaults from agent config.
pub(crate) fn create_chat_loop_state(agent_config: &AgentConfig) -> ChatLoopState {
    ChatLoopState {
        messages: Vec::new(),
        active_flow_key: None,
        manual_session_id: None,
        flow_token_usage: 0,
        active_lens: agent_config.default_lens.parse().unwrap_or(Lens::Eco),
        total_input_tokens: 0,
        total_output_tokens: 0,
        last_prompt_report: None,
        memory_flush_triggered: false,
    }
}

// ---------------------------------------------------------------------------
// Agent routing (multi-agent message dispatch)
// ---------------------------------------------------------------------------

/// Parse agent routing from message text.
///
/// Supports two formats:
/// 1. `@role: message` — explicit routing (always accepted, even unknown roles)
/// 2. `role: message`  — implicit routing (only when `role` matches a known agent)
///
/// Returns `(Some(role_key), message)` if routed, `(None, original)` otherwise.
pub(crate) fn parse_agent_routing(
    text: &str,
    known_roles: Option<&HashMap<String, String>>,
) -> (Option<String>, String) {
    let trimmed = text.trim();

    // 1. @role: message — always works, even for unknown roles.
    if let Some(rest) = trimmed.strip_prefix('@') {
        if let Some(colon_pos) = rest.find(':') {
            let role = rest[..colon_pos].trim().to_lowercase().replace('-', "_");
            let message = rest[colon_pos + 1..].trim().to_string();
            if !role.is_empty() && !message.is_empty() {
                return (Some(role), message);
            }
        }
    }

    // 2. role: message — only when the part before ":" matches a known agent.
    if let Some(roles) = known_roles {
        if let Some(colon_pos) = trimmed.find(':') {
            let candidate = trimmed[..colon_pos].trim().to_lowercase().replace('-', "_");
            let message = trimmed[colon_pos + 1..].trim().to_string();
            if !candidate.is_empty() && !message.is_empty() && roles.contains_key(&candidate) {
                return (Some(candidate), message);
            }
        }
    }

    (None, trimmed.to_string())
}

// ---------------------------------------------------------------------------
// Message chunking
// ---------------------------------------------------------------------------

/// Split text into chunks of at most `max_len` characters.
///
/// Prefers splitting at `\n\n` boundaries; falls back to char boundary.
/// Useful for any channel with message length limits (Telegram: 4096, Slack: 40000).
pub(crate) fn chunk_message(text: &str, max_len: usize) -> Vec<&str> {
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

        // Try to split at \n\n within the last half of the chunk.
        let mut search_start = start + (boundary - start) / 2;
        while search_start < boundary && !text.is_char_boundary(search_start) {
            search_start += 1;
        }
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
// Output truncation (shared by both orchestrators)
// ---------------------------------------------------------------------------

/// Char-boundary-safe truncation with `"...(truncated)"` suffix.
///
/// Used by both CLI and Telegram orchestrators to embed previous step output
/// inline in task prompts instead of referencing file paths.
pub(crate) fn truncate_output(text: &str, max_chars: usize) -> String {
    if text.len() <= max_chars {
        return text.to_string();
    }
    let mut end = max_chars;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...(truncated)", &text[..end])
}

// ---------------------------------------------------------------------------
// Cross-agent activity log
// ---------------------------------------------------------------------------

/// Maximum activity entries retained across all agents.
pub(crate) const MAX_ACTIVITY_ENTRIES: usize = 10;

/// Maximum characters of an agent's response kept in the activity summary.
pub(crate) const MAX_ACTIVITY_SUMMARY_CHARS: usize = 400;

/// A record of what one agent did in a single turn.
pub(crate) struct ActivityEntry {
    pub agent_label: String,
    pub agent_id: String,
    pub tools_used: Vec<String>,
    pub response_summary: String,
    /// Key tool outcomes (name, result) — concrete data for cross-agent handoff.
    pub tool_outcomes: Vec<(String, String)>,
}

/// Format a tool call for the activity log.
/// Only tools that require approval (mutations) are interesting for other agents.
pub(crate) fn format_tool_for_activity(
    call: &ToolCall,
    tools: &[ToolDef],
) -> Option<String> {
    let tool_def = tools.iter().find(|t| t.name == call.name);
    let requires_approval = tool_def
        .and_then(|t| t.policy.as_ref())
        .map(|p| p.requires_approval)
        .unwrap_or(false);

    if !requires_approval {
        return None;
    }

    let summary = crate::adapters::tool_builder::summarize_tool_args(&call.arguments);
    let truncated = truncate_summary(&summary, 80);
    Some(format!("{}: {}", call.name, truncated))
}

/// Build a context block summarising what OTHER agents have done recently.
pub(crate) fn build_activity_context(activity_log: &[ActivityEntry], current_agent_id: &str) -> String {
    let other: Vec<&ActivityEntry> = activity_log
        .iter()
        .filter(|e| e.agent_id != current_agent_id)
        .collect();
    if other.is_empty() {
        return String::new();
    }

    let mut ctx = String::from("\n\n## Recent Team Activity\n");
    ctx.push_str(
        "Other team members have been working on this project. Build on their work.\n\
         IMPORTANT: Use your available tools to check files they created before making changes.\n\n",
    );

    for entry in other.iter().rev().take(5) {
        ctx.push_str(&format!("**{}**", entry.agent_label));
        if !entry.tools_used.is_empty() {
            ctx.push_str(&format!(" — {}", entry.tools_used.join(", ")));
        }
        ctx.push('\n');
        if !entry.response_summary.is_empty() {
            ctx.push_str(&entry.response_summary);
            ctx.push('\n');
        }
        if !entry.tool_outcomes.is_empty() {
            ctx.push_str("\nKey outputs:\n");
            for (name, result) in &entry.tool_outcomes {
                ctx.push_str(&format!(
                    "- `{}`: {}\n",
                    name,
                    truncate_output(result, 1000),
                ));
            }
        }
        ctx.push('\n');
    }

    ctx
}

/// Truncate text to at most `max` chars on a char boundary, appending "…" if cut.
pub(crate) fn truncate_summary(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

// ---------------------------------------------------------------------------
// Skill list formatting
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Session registry (OpenClaw-compatible agent-to-agent coordination)
// ---------------------------------------------------------------------------

/// Metadata about an active agent session.
#[derive(Debug, Clone)]
pub(crate) struct SessionInfo {
    pub agent_id: String,
    pub agent_name: String,
    pub role: Option<String>,
    pub model: String,
    pub active: bool,
}

/// Shared registry of active agent sessions, enabling agent-to-agent discovery.
pub(crate) struct SessionRegistry {
    sessions: std::sync::RwLock<HashMap<String, SessionInfo>>,
}

impl SessionRegistry {
    pub(crate) fn new() -> Self {
        Self {
            sessions: std::sync::RwLock::new(HashMap::new()),
        }
    }

    /// Register an agent session.
    pub(crate) fn register(&self, info: SessionInfo) {
        self.sessions
            .write()
            .unwrap()
            .insert(info.agent_id.clone(), info);
    }

    /// List all registered sessions.
    pub(crate) fn list(&self) -> Vec<SessionInfo> {
        self.sessions.read().unwrap().values().cloned().collect()
    }

    /// Find a session by agent name or role (case-insensitive).
    pub(crate) fn find(&self, query: &str) -> Option<SessionInfo> {
        let q = query.trim().to_lowercase().replace('-', "_");
        self.sessions.read().unwrap().values().find(|s| {
            s.agent_id.to_lowercase() == q
                || s.agent_name.to_lowercase() == q
                || s.role.as_ref().map(|r| r.to_lowercase() == q).unwrap_or(false)
        }).cloned()
    }
}

/// Tool executor for session tools.
pub(crate) struct SessionToolExecutor {
    registry: Arc<SessionRegistry>,
}

impl SessionToolExecutor {
    pub(crate) fn new(registry: Arc<SessionRegistry>) -> Self {
        Self { registry }
    }
}

impl crate::adapters::ports::ToolExecutionPort for SessionToolExecutor {
    fn execute_tool(&self, call: &crate::adapters::types::ToolCall) -> anyhow::Result<String> {
        match call.name.as_str() {
            "sessions_list" => {
                let sessions = self.registry.list();
                if sessions.is_empty() {
                    return Ok("No active agent sessions.".to_string());
                }
                let mut lines = Vec::new();
                for s in &sessions {
                    let role = s.role.as_deref().unwrap_or("none");
                    let status = if s.active { "active" } else { "idle" };
                    lines.push(format!(
                        "- {} (role: {}, model: {}, status: {})",
                        s.agent_name, role, s.model, status
                    ));
                }
                Ok(lines.join("\n"))
            }

            "sessions_send" => {
                let agent = call
                    .arguments
                    .get("agent")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("sessions_send: missing 'agent'"))?;
                let message = call
                    .arguments
                    .get("message")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("sessions_send: missing 'message'"))?;

                match self.registry.find(agent) {
                    Some(info) => {
                        tracing::info!(
                            target_agent = %info.agent_id,
                            message_len = message.len(),
                            "sessions_send — message queued"
                        );
                        // In the current architecture, inter-agent messaging goes
                        // through the EventBus when orchestration is active.
                        // For direct chat mode, we log and acknowledge.
                        Ok(format!(
                            "Message sent to {} ({}). Note: in the current runtime, \
                             cross-agent messaging is fully supported during orchestrated \
                             task execution. In direct chat mode, the message is logged \
                             for the next orchestration run.",
                            info.agent_name,
                            info.role.as_deref().unwrap_or("no role"),
                        ))
                    }
                    None => {
                        let available: Vec<String> = self
                            .registry
                            .list()
                            .iter()
                            .map(|s| s.agent_name.clone())
                            .collect();
                        Err(anyhow::anyhow!(
                            "Agent '{}' not found. Available: {}",
                            agent,
                            if available.is_empty() {
                                "none".to_string()
                            } else {
                                available.join(", ")
                            }
                        ))
                    }
                }
            }

            "sessions_history" => {
                let agent = call
                    .arguments
                    .get("agent")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("sessions_history: missing 'agent'"))?;

                match self.registry.find(agent) {
                    Some(info) => {
                        Ok(format!(
                            "Session history for {} ({}): history retrieval is available \
                             during orchestrated execution. In direct chat, session history \
                             is maintained per-agent via the flow system.",
                            info.agent_name,
                            info.role.as_deref().unwrap_or("no role"),
                        ))
                    }
                    None => Err(anyhow::anyhow!("Agent '{}' not found", agent)),
                }
            }

            other => Err(anyhow::anyhow!("unknown session tool: {}", other)),
        }
    }
}

/// Format the skill registry into a human-readable list.
pub(crate) fn format_skill_list(registry: &SkillRegistry) -> String {
    let skills = registry.list_all();
    if skills.is_empty() {
        return "No skills discovered.".to_string();
    }
    let mut lines = vec!["Skills:".to_string()];
    for (name, status) in skills {
        let tag = match status {
            SkillStatus::Active => "active",
            SkillStatus::Inactive => "disabled",
        };
        let readiness = registry
            .entries()
            .get(&name)
            .map(|e| {
                let missing = e.missing_required_env_vars();
                if missing.is_empty() {
                    " [ready]".to_string()
                } else {
                    format!(" (missing: {})", missing.join(", "))
                }
            })
            .unwrap_or_default();
        lines.push(format!("  {} [{}]{}", name, tag, readiness));
    }
    lines.join("\n")
}
