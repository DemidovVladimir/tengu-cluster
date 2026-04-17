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
use async_trait::async_trait;

use crate::adapters::embedding::OpenRouterEmbeddingAdapter;
use crate::adapters::memory_builder::{DiskVectorMemoryStore, MemoryServiceHandle};
use crate::adapters::plugins::cache::{CachePlugin, SHARED_CACHE_TOOL_NAME};
use crate::adapters::plugins::crypto::CryptoPlugin;
use crate::adapters::plugins::http::HttpPlugin;
use crate::adapters::plugins::memory::{persistent_store_tool_defs, MemoryPlugin};
use crate::adapters::plugins::workspace::WorkspacePlugin;
use crate::adapters::shell_executor::LocalShellExecutor;
use crate::adapters::skill_builder::{
    self, SkillExecution, SkillRegistry, SkillStatus, SkillToolExecutionAdapter,
};
use crate::adapters::engine_builder::ToolExecutor;
use crate::adapters::ports::{ShellExecutionPort, ToolActivityPort, ToolExecutionPort, ToolScope};
use crate::adapters::tool_builder::{build_platform_tools, build_workspace_tools, ToolUseService};
use crate::adapters::tool_plugin::{LegacyToolBridge, PluginCtx, PluginToolExecutor, ToolRegistry};
use crate::adapters::secret_builder::SecretRegistry;
use crate::adapters::config::AgentConfig;
use crate::adapters::types::{
    ChatLoopState, Lens, ToolCall, ToolDef,
};

// ---------------------------------------------------------------------------
// Tool executor wrapper (legacy — kept until A9 completes the migration)
// ---------------------------------------------------------------------------

/// Thin wrapper around `ToolUseService` implementing the `ToolExecutor` trait.
///
/// Still used by non-plugin code paths (e.g. MCP bridge) during the Phase A
/// migration. Channel adapters now receive a `PluginToolExecutor` from
/// `build_tool_executor`.
#[allow(dead_code)]
pub(crate) struct ToolServiceExecutor {
    service: ToolUseService,
}

#[async_trait]
impl ToolExecutor for ToolServiceExecutor {
    async fn execute(&self, call: &ToolCall) -> Result<String> {
        self.service.execute(call)
    }
}

// ---------------------------------------------------------------------------
// Tool, executor, and prompt rebuilding
// ---------------------------------------------------------------------------

/// Merge base platform tools with active skill tools.
pub(crate) fn rebuild_tools(
    base_tools: &[ToolDef],
    skill_registry: &SkillRegistry,
) -> Vec<ToolDef> {
    let mut tools = base_tools.to_vec();
    tools.extend(skill_registry.active_tools());
    tools
}

/// Rebuild the system prompt from agent config, skill registry state, and active tools.
pub(crate) fn rebuild_system_prompt(
    agent_config: &AgentConfig,
    advertise_workspace_tools: bool,
    skill_registry: &SkillRegistry,
    tools: &[ToolDef],
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
        tools,
    )
}

/// Build the plugin-backed tool executor from workspace, skill, and memory executors.
///
/// The `activity` port is channel-specific — each channel adapter provides its own
/// implementation (TUI dialogs, Telegram inline keyboards, Slack messages, etc.).
///
/// `shared_http_client` — when `Some`, all HTTP and crypto executors share one
/// `reqwest::Client` instead of each building their own connection pool.
///
/// The returned `PluginToolExecutor` wraps a `ToolRegistry`. Workspace tools are
/// registered via `WorkspacePlugin`; every other tool is wrapped through
/// `LegacyToolBridge` (to be removed in A9 as each domain migrates to a plugin).
pub(crate) fn build_tool_executor(
    workspace: &Path,
    tools: &[ToolDef],
    skill_registry: &SkillRegistry,
    memory_handle: &Option<Arc<MemoryServiceHandle>>,
    secret_registry: &Arc<SecretRegistry>,
    activity: Arc<dyn ToolActivityPort>,
    cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    shared_http_client: Option<&reqwest::Client>,
    memory_config: Option<&crate::adapters::config::MemoryConfig>,
    agent_config: &AgentConfig,
) -> Option<PluginToolExecutor> {
    if tools.is_empty() {
        return None;
    }

    let shell: Arc<dyn ShellExecutionPort> = Arc::new(match cancel {
        Some(ref flag) => LocalShellExecutor::new().with_cancel(Arc::clone(flag)),
        None => LocalShellExecutor::new(),
    });

    let http_client = shared_http_client.cloned().unwrap_or_else(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    });

    let allowed_names: HashSet<String> = tools.iter().map(|tool| tool.name.clone()).collect();
    let allowed_list: Vec<String> = allowed_names.iter().cloned().collect();

    let mut registry = ToolRegistry::new();

    // Workspace plugin (new-style). Registers read_file, list_directory, write_file, run_command.
    let plugin_ctx = PluginCtx {
        workspace,
        config: agent_config,
        http: http_client.clone(),
        shell: Arc::clone(&shell),
        memory: memory_handle.clone(),
        secret_registry: Arc::clone(secret_registry),
    };
    // TODO(A9): make build_tool_executor async once the TUI/telegram/orchestrator chain is fully async
    if let Err(e) = futures::executor::block_on(registry.register_plugin(
        &WorkspacePlugin,
        &plugin_ctx,
        &allowed_list,
    )) {
        // Fail closed: if the workspace plugin can't register, return None so
        // callers treat this agent as having no tools rather than handing the
        // LLM a registry missing its advertised workspace primitives.
        tracing::error!(error = %e, "Failed to register workspace plugin — returning no executor");
        return None;
    }

    // Shell skills create named tools; API skills are documentation-only.
    let shell_skill_defs: Vec<_> = skill_registry
        .active_skill_definitions()
        .into_iter()
        .filter(|skill| {
            allowed_names.contains(&skill.name)
                && matches!(skill.execution, SkillExecution::Shell { .. })
        })
        .collect();

    if !shell_skill_defs.is_empty() {
        let skill_tool_defs: Vec<ToolDef> = skill_registry
            .active_tools()
            .into_iter()
            .filter(|def| shell_skill_defs.iter().any(|s| s.name == def.name))
            .collect();
        let skill_exec: Arc<dyn ToolExecutionPort> = Arc::new(SkillToolExecutionAdapter::new(
            shell_skill_defs,
            Arc::clone(&shell),
            workspace.to_path_buf(),
        ));
        for def in skill_tool_defs {
            registry.register_tool(Arc::new(LegacyToolBridge::new(def, Arc::clone(&skill_exec))));
        }
    }

    // Memory plugin (A5). Registers `remember` when memory is enabled, plus
    // `persistent_store` when listed in `workspace_tools`. The plugin itself
    // gates on `ctx.memory.is_some()`.
    // TODO(A9): make build_tool_executor async once the TUI/telegram/orchestrator chain is fully async.
    let ps_chunk_size = memory_config
        .map(|mc| mc.persistent_store_chunk_size)
        .unwrap_or(1000);
    let ps_chunk_overlap = memory_config
        .map(|mc| mc.persistent_store_chunk_overlap)
        .unwrap_or(200);
    let memory_plugin = MemoryPlugin::new(ps_chunk_size, ps_chunk_overlap);
    if let Err(e) = futures::executor::block_on(registry.register_plugin(
        &memory_plugin,
        &plugin_ctx,
        &allowed_list,
    )) {
        tracing::warn!(error = %e, "Failed to register memory plugin — remember/persistent_store unavailable");
    }

    // Cache plugin (A4). Registers `shared_cache` when `allowed_names` includes
    // it (i.e. the agent's `workspace_tools` opt-in list).
    // TODO(A9): make build_tool_executor async once the TUI/telegram/orchestrator chain is fully async.
    if allowed_names.contains(SHARED_CACHE_TOOL_NAME) {
        if let Err(e) = futures::executor::block_on(registry.register_plugin(
            &CachePlugin,
            &plugin_ctx,
            &allowed_list,
        )) {
            tracing::warn!(error = %e, "Failed to register cache plugin — shared_cache unavailable");
        }
    }

    // HTTP plugin (A2). Registers `http_request`.
    // TODO(A9): make build_tool_executor async once the TUI/telegram/orchestrator chain is fully async.
    if let Err(e) = futures::executor::block_on(registry.register_plugin(
        &HttpPlugin,
        &plugin_ctx,
        &allowed_list,
    )) {
        tracing::warn!(error = %e, "Failed to register http plugin — http_request unavailable");
    }

    // Crypto plugin (A3). Registers sign_and_send_transaction, sign_message,
    // get_wallet_address, abi_encode, hex_to_uint256.
    // TODO(A9): make build_tool_executor async once the TUI/telegram/orchestrator chain is fully async.
    let crypto_plugin = CryptoPlugin::new(cancel.clone());
    if let Err(e) = futures::executor::block_on(registry.register_plugin(
        &crypto_plugin,
        &plugin_ctx,
        &allowed_list,
    )) {
        tracing::warn!(error = %e, "Failed to register crypto plugin — crypto tools unavailable");
    }

    // Per-tool scope map: permissive by default during A1 — pre-migration
    // behaviour did not gate tools via ToolScope. Per-agent scope wiring arrives
    // alongside the remaining plugin migrations.
    let default_scope = permissive_scope(workspace);
    let mut scopes: HashMap<String, ToolScope> = HashMap::new();
    for name in registry.tool_names() {
        scopes.insert(name, default_scope.clone());
    }

    Some(PluginToolExecutor {
        registry,
        workspace: workspace.to_path_buf(),
        shell: Arc::clone(&shell),
        http: http_client,
        memory: memory_handle.clone(),
        secret_registry: Arc::clone(secret_registry),
        activity,
        scopes,
    })
}

/// Build a permissive `ToolScope` that preserves pre-migration behaviour:
/// the workspace root is writable, any host is reachable, any binary is
/// runnable, any env var is readable, and the canonical wallet label is
/// granted so the crypto plugin's `check_wallet` guard succeeds. Phase A
/// tasks tighten this once each plugin ships.
// TODO(A9): replace with per-agent scope once config-scopes land
fn permissive_scope(workspace: &Path) -> ToolScope {
    ToolScope {
        fs_roots: vec![workspace.to_path_buf()],
        net_hosts: vec!["*".to_string()],
        env_reads: vec!["*".to_string()],
        shell_bins: vec!["*".to_string()],
        wallets: vec![crate::adapters::plugins::crypto::helpers::DEFAULT_WALLET_LABEL.to_string()],
    }
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
pub(crate) fn compute_base_tools(
    uses_tools: bool,
    has_memory: bool,
    workspace_tools: &[String],
) -> Vec<ToolDef> {
    if !uses_tools {
        return vec![];
    }
    let mut tools = build_workspace_tools();
    if has_memory {
        tools.extend(crate::adapters::plugins::memory::tool_defs());
    }
    if workspace_tools.iter().any(|t| t == "shared_cache") {
        tools.extend(crate::adapters::plugins::cache::tool_defs());
    }
    if workspace_tools.iter().any(|t| t == "persistent_store") {
        tools.extend(persistent_store_tool_defs());
    }
    tools.extend(crate::adapters::plugins::http::tool_defs());
    tools.extend(crate::adapters::plugins::crypto::tool_defs());
    tools.extend(build_platform_tools());
    tools
}

/// Compute the full bridge tool set for engines that manage their own workspace.
///
/// Unlike `compute_base_tools`, this ALWAYS returns all tools (workspace + platform +
/// memory + cache) regardless of engine capabilities. Used to populate the MCP bridge
/// when a Claude Code engine needs access to Tengu-native tools.
pub(crate) fn compute_bridge_tools(
    has_memory: bool,
    workspace_tools: &[String],
) -> Vec<ToolDef> {
    let mut tools = build_workspace_tools();
    if has_memory {
        tools.extend(crate::adapters::plugins::memory::tool_defs());
    }
    if workspace_tools.iter().any(|t| t == "shared_cache") {
        tools.extend(crate::adapters::plugins::cache::tool_defs());
    }
    if workspace_tools.iter().any(|t| t == "persistent_store") {
        tools.extend(persistent_store_tool_defs());
    }
    tools.extend(crate::adapters::plugins::http::tool_defs());
    tools.extend(crate::adapters::plugins::crypto::tool_defs());
    tools.extend(build_platform_tools());
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
/// Read-only tools (read_file, list_directory, etc.) are not interesting for other agents.
pub(crate) fn format_tool_for_activity(
    call: &ToolCall,
) -> Option<String> {
    let read_only = matches!(
        call.name.as_str(),
        "read_file" | "list_directory" | "get_wallet_address" | "abi_encode" | "hex_to_uint256"
    );
    if read_only {
        return None;
    }

    let (title, detail) = crate::adapters::tool_builder::build_tool_activity_text(call);
    let detail_str = detail.unwrap_or_default();
    let truncated = truncate_summary(&detail_str, 80);
    Some(format!("{}: {}", title, truncated))
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

// ---------------------------------------------------------------------------
// Golden registry test — asserts the LLM-facing tool surface is unchanged.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod golden_tests {
    use super::*;
    use crate::adapters::config::Config;
    use crate::adapters::types::ToolCall;
    use std::collections::HashSet;
    use tempfile::TempDir;

    struct StubActivity;
    impl ToolActivityPort for StubActivity {
        fn publish_tool_activity(&self, _call: &ToolCall) {}
    }

    /// Assert that the set of registered tool names equals the pre-migration set
    /// when building a default-configured executor with every opt-in tool enabled.
    /// `persistent_store` is excluded because it requires a live embedding API.
    #[test]
    fn tool_names_match_pre_migration_surface() {
        let tmp = TempDir::new().unwrap();

        // Compute the same base-tool list the channel adapters use at startup.
        let mut tools = build_workspace_tools();
        tools.extend(crate::adapters::plugins::memory::tool_defs());
        tools.extend(crate::adapters::plugins::cache::tool_defs());
        tools.extend(crate::adapters::plugins::http::tool_defs());
        tools.extend(crate::adapters::plugins::crypto::tool_defs());
        tools.extend(build_platform_tools());

        let config = Config::default();
        let agent_config = config.agents.get("main").unwrap();
        let skill_registry = crate::adapters::skill_builder::SkillRegistry::new(Vec::new());
        let activity: Arc<dyn ToolActivityPort> = Arc::new(StubActivity);
        let secret_registry = Arc::new(SecretRegistry::new());

        let executor = build_tool_executor(
            tmp.path(),
            &tools,
            &skill_registry,
            &None, // memory_handle: omit — `remember` is only registered by MemoryPlugin when ctx.memory is Some.
            &secret_registry,
            activity,
            None,
            None,
            None,
            agent_config,
        )
        .expect("executor");

        let names: HashSet<String> = executor.registry.tool_names().into_iter().collect();

        let mut expected: HashSet<String> = [
            "read_file",
            "list_directory",
            "write_file",
            "run_command",
            "http_request",
            "sign_and_send_transaction",
            "sign_message",
            "get_wallet_address",
            "abi_encode",
            "hex_to_uint256",
            "shared_cache",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        // `remember` requires a memory handle — only present when memory is enabled.
        // `persistent_store` requires memory too and is opt-in.
        if names.contains("remember") {
            expected.insert("remember".to_string());
        }

        assert_eq!(
            names, expected,
            "tool surface drift: registry has {:?}, expected {:?}",
            names, expected
        );
    }
}
