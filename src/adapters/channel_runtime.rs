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
//! - **Memory subsystem initialization** — `build_memory_manager`
//! - **Base tool computation** — `compute_base_tools` (workspace primitives +
//!   subsystem tools)
//! - **Agent routing** — `parse_agent_routing`
//! - **Message chunking** — `chunk_message` (for channels with length limits)
//! - **State factories** — `create_chat_loop_state`
//! - **Skill list formatting** — `format_skill_list`

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use crate::adapters::outbound::memory::disk_vector::DiskVectorStore;
use crate::adapters::outbound::memory::embedder::Embedder;
use crate::config::{AgentConfig, McpServerConfig};
use crate::ports::memory::VectorStore;
// Plugin set: `outbound::tools::catalog`. McpPlugin + SkillPlugin are
// registered here, outside the catalog — they need the config's
// `[[mcp_servers]]` / a skill registry.
use crate::adapters::outbound::mcp_client::McpPlugin;
use crate::adapters::outbound::shell::LocalShellExecutor;
use crate::adapters::outbound::tools::skill::SkillPlugin;
use crate::application::skills::registry::{SkillRegistry, SkillStatus};
use crate::application::tools::registry::{PluginToolExecutor, ToolRegistry};
use crate::domain::message::{Lens, ToolCall, ToolDef};
use crate::domain::scope::ToolScope;
use crate::domain::secrets::SecretRegistry;
use crate::domain::session::ChatLoopState;
use crate::ports::shell::ShellExecutionPort;
use crate::ports::tool::PluginCtx;
use crate::ports::tool_activity::ToolActivityPort;

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
    crate::application::skills::registry::build_system_prompt_with_tools(
        agent_config,
        advertise_workspace_tools,
        &skill_context_strings,
        tools,
    )
}

/// Register a plugin, logging failures as warnings. Returns `true` on
/// success, `false` on failure.
///
/// Used by `build_tool_executor` — wraps the `futures::executor::block_on`
/// bridge until the channel chain is fully async.
// TODO(Phase B): drop the block_on once build_tool_executor is async.
fn register_plugin_safe(
    registry: &mut ToolRegistry,
    plugin: &dyn crate::ports::tool::ToolPlugin,
    plugin_ctx: &PluginCtx<'_>,
    allow_list: &[String],
    failure_message: &str,
) -> bool {
    match futures::executor::block_on(registry.register_plugin(plugin, plugin_ctx, allow_list)) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(error = %e, "{}", failure_message);
            false
        }
    }
}

/// Build the plugin-backed tool executor from workspace, skill, and memory executors.
///
/// The `activity` port is channel-specific — each channel adapter provides its own
/// implementation (TUI dialogs, Telegram inline keyboards, Slack messages, etc.).
///
/// The HTTP client comes from `egress::policy().tool_client` — proxied per
/// `[egress]`, redirects off. There is deliberately no way to pass in a
/// client: an unproxied one would bypass the egress policy. If the client
/// can't be built the executor is not built (no tools, fail-closed).
///
/// The returned `PluginToolExecutor` wraps a `ToolRegistry`. Every tool is
/// backed by a domain plugin (workspace, http, crypto, cache, memory, skill,
/// mcp).
pub(crate) fn build_tool_executor(
    workspace: &Path,
    tools: &[ToolDef],
    skill_registry: &SkillRegistry,
    memory_manager: &Option<Arc<crate::application::memory::manager::MemoryManager>>,
    secret_registry: &Arc<SecretRegistry>,
    activity: Arc<dyn ToolActivityPort>,
    cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    memory_config: Option<&crate::config::MemoryConfig>,
    agent_config: &AgentConfig,
    mcp_servers: &[McpServerConfig],
) -> Option<PluginToolExecutor> {
    if tools.is_empty() {
        return None;
    }

    let shell: Arc<dyn ShellExecutionPort> = Arc::new(match cancel {
        Some(ref flag) => LocalShellExecutor::new().with_cancel(Arc::clone(flag)),
        None => LocalShellExecutor::new(),
    });

    let http_client = match crate::adapters::outbound::egress::policy()
        .tool_client(std::time::Duration::from_secs(60))
    {
        Ok(client) => client,
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "egress: tool http client unavailable — tools disabled");
            return None;
        }
    };

    let allowed_names: HashSet<String> = tools.iter().map(|tool| tool.name.clone()).collect();
    let allowed_list: Vec<String> = allowed_names.iter().cloned().collect();

    let mut registry = ToolRegistry::new();

    // Workspace plugin (new-style). Registers read_file, list_directory, write_file, run_command.
    let plugin_ctx = PluginCtx {
        workspace,
        config: agent_config,
        http: http_client.clone(),
        shell: Arc::clone(&shell),
        memory_manager: memory_manager
            .clone()
            .map(|m| m as Arc<dyn crate::ports::memory::MemoryService>),
        secret_registry: Arc::clone(secret_registry),
    };

    // Register the tool catalog (`outbound/tools/mod.rs`) — the same call
    // the MCP bridge makes, so a new catalog row reaches both.
    futures::executor::block_on(crate::adapters::outbound::tools::register_catalog(
        &mut registry,
        &plugin_ctx,
        &allowed_names,
        &allowed_list,
        crate::adapters::outbound::tools::CatalogOpts {
            cancel: cancel.clone(),
            memory_config,
        },
    ));

    // SkillPlugin — in-process only (the bridge has no skill registry).
    // Registers one `SkillShellTool` per active shell skill. Doc/API skills
    // stay in the system-prompt path and do not create tools.
    let skill_plugin = SkillPlugin::from_registry(skill_registry);
    register_plugin_safe(
        &mut registry,
        &skill_plugin,
        &plugin_ctx,
        &allowed_list,
        "Failed to register skill plugin — shell skills unavailable",
    );

    // MCP plugin — connects to each configured external server and registers
    // its tools as `{server_name}__{tool_name}`. The Claude Code bridge does
    // the same for the servers its engine passes (`TENGU_BRIDGE_MCP_SERVERS`).
    if !mcp_servers.is_empty() {
        let mcp_plugin = McpPlugin::new(mcp_servers.to_vec());
        register_plugin_safe(
            &mut registry,
            &mcp_plugin,
            &plugin_ctx,
            &[],
            "Failed to register mcp plugin — external MCP tools unavailable",
        );
    }

    // Per-tool scope map — ENFORCED. `agent_config.scopes` already has the
    // parent's `[default_scopes]` folded in (`Config::fold_default_scopes`
    // at `Config::load`; `run-agent` children load the same config and go
    // through `subagent_config`). A tool with no configured entry falls
    // back to `permissive_scope`.
    let scopes = resolve_tool_scopes(workspace, &agent_config.scopes, registry.tool_names());

    Some(PluginToolExecutor {
        registry,
        workspace: workspace.to_path_buf(),
        shell: Arc::clone(&shell),
        http: http_client,
        memory_manager: memory_manager
            .clone()
            .map(|m| m as Arc<dyn crate::ports::memory::MemoryService>),
        secret_registry: Arc::clone(secret_registry),
        activity,
        scopes,
        // Stream M — clone into the executor so tools (e.g. skill_distill)
        // can read engine + model when seeding generated artefacts. The
        // borrow into ToolCtx happens in PluginToolExecutor::execute.
        agent_config: Some(agent_config.clone()),
    })
}

/// Resolve the per-tool scope map for an executor: a configured entry in
/// `configured` (per-agent `[agents.*.scopes.<tool>]`, with `[default_scopes]`
/// already folded in) wins; any tool without one gets `permissive_scope`.
/// Shared by `build_tool_executor` and `mcp_bridge::build_bridge_executor`.
pub(crate) fn resolve_tool_scopes(
    workspace: &Path,
    configured: &HashMap<String, ToolScope>,
    tool_names: impl IntoIterator<Item = String>,
) -> HashMap<String, ToolScope> {
    let fallback = permissive_scope(workspace);
    tool_names
        .into_iter()
        .map(|name| {
            let scope = configured
                .get(&name)
                .cloned()
                .unwrap_or_else(|| fallback.clone());
            (name, scope)
        })
        .collect()
}

/// Inherited `[default_scopes]` were authored for the parent's workspace. A
/// subprocess child runs in its own workspace (`agent.workspace`, or a temp /
/// cwd when unset), so grant that root on every inherited scope — otherwise
/// `read_file` / multipart `http_request` under the child's own workspace is
/// scope-denied. Tools without a configured scope already get the workspace
/// via `permissive_scope`, so this only equalises the configured ones.
pub(crate) fn grant_workspace_root(scopes: &mut HashMap<String, ToolScope>, workspace: &Path) {
    for scope in scopes.values_mut() {
        if !scope.fs_roots.iter().any(|r| r == workspace) {
            scope.fs_roots.push(workspace.to_path_buf());
        }
    }
}

/// Build the fallback `ToolScope` for tools with no configured entry:
/// the workspace root is writable, any host is reachable, any binary is
/// runnable, any env var is readable, and the canonical wallet label is
/// granted so the crypto plugin's `check_wallet` guard succeeds. Narrow it
/// per tool via `[default_scopes.<tool>]` / `[agents.*.scopes.<tool>]`.
///
/// Phase 7.7 — `pub(crate)` so the MCP bridge calls this instead of
/// inlining its own copy. Both paths share the same scope shape.
pub(crate) fn permissive_scope(workspace: &Path) -> ToolScope {
    ToolScope {
        fs_roots: vec![workspace.to_path_buf()],
        net_hosts: vec!["*".to_string()],
        env_reads: vec!["*".to_string()],
        shell_bins: vec!["*".to_string()],
        wallets: vec![
            crate::adapters::outbound::tools::crypto::helpers::DEFAULT_WALLET_LABEL.to_string(),
        ],
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
    crate::adapters::outbound::tools::advertised_defs(has_memory, workspace_tools)
}

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
    crate::domain::token::truncate_with_suffix(text, max_chars, "...(truncated)")
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
pub(crate) fn format_tool_for_activity(call: &ToolCall) -> Option<String> {
    let read_only = matches!(
        call.name.as_str(),
        "read_file" | "list_directory" | "get_wallet_address" | "abi_encode" | "hex_to_uint256"
    );
    if read_only {
        return None;
    }

    let (title, detail) = crate::adapters::inbound::activity::build_tool_activity_text(call);
    let detail_str = detail.unwrap_or_default();
    let truncated = truncate_summary(&detail_str, 80);
    Some(format!("{}: {}", title, truncated))
}

/// Build a context block summarising what OTHER agents have done recently.
pub(crate) fn build_activity_context(
    activity_log: &[ActivityEntry],
    current_agent_id: &str,
) -> String {
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
    crate::domain::token::truncate_with_suffix(text, max, "…")
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
// ChatServiceFactory — per-agent, per-call ChatRuntimeService construction
// ---------------------------------------------------------------------------
//
// `ChatRuntimeService<'a>` borrows most of its fields (`engine`, `agent_config`,
// `tools`, ...) so it cannot live behind `Arc<dyn ChatServiceFactory>` on its
// own. The factory here owns `Arc`-held snapshots of everything a service
// needs and rebuilds one service per `run_turn` call. Channels construct the
// factory once at startup by supplying an `inputs_fn` closure that knows how
// to produce per-agent inputs (the same logic they currently run inline when
// building a `ChatRuntimeService`).
//
// This is the escape hatch called out in Task 5.2: per-channel fidelity
// (`tool_observer`, `cancel`, channel-specific tool wrapping, multi-agent
// activity context) is preserved because each channel supplies its own
// `inputs_fn`. The generic helper `build_orchestrator` below only needs the
// factory trait, not the channel-specific shape.

use async_trait::async_trait;

use crate::application::chat::service::ChatRuntimeService;
use crate::application::chat::tool_loop::ToolResultObserver;
use crate::application::memory::manager::MemoryManager;
use crate::application::orchestrator::planner::RagPlanner;
use crate::application::orchestrator::retry::RetryPolicy;
use crate::config::Config;
use crate::ports::engine::ToolExecutor;
use crate::ports::orchestration::Planner;
// Phase 7.1 (full) — `OrchestratorAgentPlanner`, `ChatWorker`, and the
// `render_roster` helper were deleted along with the static-mode path.
// `build_orchestrator` now constructs only `RagPlanner` + `SubprocessRunner`.
use crate::application::orchestrator::wiring::ChatOrchestratorPortImpl;
use crate::application::orchestrator::Orchestrator;
use crate::domain::session::FlowCompactionPolicy;
use crate::ports::engine::Engine;
use crate::ports::orchestration::ChatServiceFactory;

/// Owned snapshot of the inputs a `ChatRuntimeService<'a>` needs for a single
/// turn. `inputs_fn` closures produce one of these per call; the factory then
/// borrows into it to build the service for `process_user_text`.
///
/// Keeping this owned (`Arc`, `String`, `Vec`, `Box<dyn Fn ...>`) sidesteps
/// the lifetime problem discovered in Task 5.1: `ChatRuntimeService<'a>` has
/// an `'a` lifetime, so it must be constructed *inside* `run_turn` where the
/// borrow can be rooted in a stack-local `ChatTurnInputs`.
pub(crate) struct ChatTurnInputs {
    pub engine: Arc<dyn Engine>,
    pub agent_id: String,
    pub agent_config: Arc<AgentConfig>,
    pub history_turn_limit: usize,
    pub compaction_policy: FlowCompactionPolicy,
    pub system_prompt: String,
    pub tools: Vec<ToolDef>,
    pub tool_executor: Option<Arc<dyn ToolExecutor>>,
    pub memory_manager: Option<Arc<MemoryManager>>,
    pub max_recall_entries: usize,
    pub max_recall_tokens: usize,
    pub bridge_tools: Option<Vec<ToolDef>>,
    /// Optional per-turn callback invoked after each tool executes.
    /// Boxed so channels can close over their own event channels.
    pub tool_observer: Option<Arc<dyn Fn(&ToolCall, &str) + Send + Sync>>,
    /// Optional cancellation flag (shared). Cloned into each turn by the
    /// channel; the factory just borrows from it.
    pub cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
}

/// Closure that produces the per-turn inputs for a named agent.
///
/// Each channel (TUI, Telegram, CLI orchestrator) supplies its own closure so
/// channel-specific details (secret-redacting tool executor wrapper,
/// tool-activity observer, typing-indicator cancel flag, multi-agent
/// activity context appended to the system prompt) are preserved. A closure
/// is used instead of a second trait to keep the factory construction
/// site-local and avoid leaking channel internals into the factory API.
pub(crate) type ChatInputsFn = Arc<dyn Fn(&str) -> anyhow::Result<ChatTurnInputs> + Send + Sync>;

/// Concrete `ChatServiceFactory` used by the orchestrator wiring.
///
/// Cheap to clone behind an `Arc`. All state is immutable after construction;
/// per-call variability lives in `inputs_fn`.
pub(crate) struct RuntimeChatServiceFactory {
    inputs_fn: ChatInputsFn,
}

impl RuntimeChatServiceFactory {
    pub(crate) fn new(inputs_fn: ChatInputsFn) -> Self {
        Self { inputs_fn }
    }
}

// ---------------------------------------------------------------------------
// OrchestratorSnapshots — shared state bridge for channel factory closures.
// ---------------------------------------------------------------------------
//
// Channels (Telegram, TUI) hold per-agent runtime state that mutates between
// messages (skill hot-reload + per-turn system-prompt tweaks). The orchestrator
// holds an `Arc<dyn ChatServiceFactory>` whose `run_turn` method is spawned
// into a tokio task, so the factory cannot borrow channel-local state.
//
// `OrchestratorSnapshots` is an `Arc<RwLock<HashMap<String, ChatTurnInputs>>>`
// shared between the channel and the factory. Before each `orchestrator.handle`
// call the channel writes a fresh snapshot for each agent; the factory closure
// reads back from the same map. This keeps the factory `'static` while letting
// per-message state (hot-reloaded tools, current system prompt, per-turn
// tool_observer / cancel) flow in through owned clones.
pub(crate) type OrchestratorSnapshots = Arc<std::sync::RwLock<HashMap<String, ChatTurnInputs>>>;

/// Build a `ChatInputsFn` closure that resolves agent inputs from an
/// `OrchestratorSnapshots` table. Returns a cheap error when the agent is not
/// present — the orchestrator surfaces this as a step failure which triggers a
/// replan.
pub(crate) fn snapshots_inputs_fn(state: OrchestratorSnapshots) -> ChatInputsFn {
    Arc::new(move |agent: &str| {
        let guard = state
            .read()
            .map_err(|e| anyhow::anyhow!("orchestrator snapshot lock poisoned: {}", e))?;
        let snap = guard.get(agent).ok_or_else(|| {
            anyhow::anyhow!(
                "orchestrator snapshot missing for agent '{}' (populate snapshots before handle())",
                agent
            )
        })?;
        Ok(clone_chat_turn_inputs(snap))
    })
}

/// Clone a `ChatTurnInputs` by cloning Arcs and owned fields.
/// `Arc<dyn Fn>` / `Arc<dyn ToolExecutor>` / `Arc<dyn Engine>` all clone cheaply.
fn clone_chat_turn_inputs(src: &ChatTurnInputs) -> ChatTurnInputs {
    ChatTurnInputs {
        engine: Arc::clone(&src.engine),
        agent_id: src.agent_id.clone(),
        agent_config: Arc::clone(&src.agent_config),
        history_turn_limit: src.history_turn_limit,
        compaction_policy: src.compaction_policy,
        system_prompt: src.system_prompt.clone(),
        tools: src.tools.clone(),
        tool_executor: src.tool_executor.clone(),
        memory_manager: src.memory_manager.clone(),
        max_recall_entries: src.max_recall_entries,
        max_recall_tokens: src.max_recall_tokens,
        bridge_tools: src.bridge_tools.clone(),
        tool_observer: src.tool_observer.clone(),
        cancel: src.cancel.clone(),
    }
}

#[async_trait]
impl ChatServiceFactory for RuntimeChatServiceFactory {
    async fn run_turn(&self, agent: &str, text: &str) -> anyhow::Result<String> {
        let (reply, _) = self.run_turn_inner(agent, None, text).await?;
        Ok(reply)
    }

    async fn run_turn_with_system(
        &self,
        agent: &str,
        system_prompt: &str,
        text: &str,
    ) -> anyhow::Result<String> {
        let (reply, _) = self
            .run_turn_inner(agent, Some(system_prompt), text)
            .await?;
        Ok(reply)
    }

    async fn run_turn_with_system_metered(
        &self,
        agent: &str,
        system_prompt: Option<&str>,
        text: &str,
    ) -> anyhow::Result<(String, crate::ports::orchestration::TurnTelemetry)> {
        self.run_turn_inner(agent, system_prompt, text).await
    }
}

impl RuntimeChatServiceFactory {
    /// Shared body for `run_turn` and `run_turn_with_system`. When
    /// `system_override` is `Some`, it replaces `inputs.system_prompt` for
    /// this single call (Phase 4c — used by the RAG planner to inject
    /// `skills/orchestrator/SKILL.md` instead of the agent's identity).
    async fn run_turn_inner(
        &self,
        agent: &str,
        system_override: Option<&str>,
        text: &str,
    ) -> anyhow::Result<(String, crate::ports::orchestration::TurnTelemetry)> {
        let inputs = (self.inputs_fn)(agent)?;
        let model_slug = inputs.agent_config.model.clone();
        let started = std::time::Instant::now();

        // Wrap tool_observer Arc into the `&dyn Fn` form the service expects.
        let observer_arc = inputs.tool_observer.clone();
        let observer_ref: Option<ToolResultObserver<'_>> =
            observer_arc.as_deref().map(|f| f as ToolResultObserver<'_>);

        let (system_prompt, tools_slice) = match system_override {
            Some(s) => {
                // Phase 4c: a system-prompt override means this is a planner call.
                // Strip tools entirely — we don't want the LLM dispatching
                // http_request etc. when its job is to emit plan JSON. Memory
                // injection is also disabled by passing memory_manager: None.
                (s.to_string(), &[][..])
            }
            None => (inputs.system_prompt.clone(), inputs.tools.as_slice()),
        };

        let service = ChatRuntimeService {
            engine: inputs.engine.as_ref(),
            agent_id: &inputs.agent_id,
            agent_config: inputs.agent_config.as_ref(),
            history_turn_limit: inputs.history_turn_limit,
            compaction_policy: inputs.compaction_policy,
            system_prompt,
            tools: tools_slice,
            tool_executor: if system_override.is_some() {
                None
            } else {
                inputs
                    .tool_executor
                    .as_deref()
                    .map(|e| e as &dyn ToolExecutor)
            },
            memory_manager: if system_override.is_some() {
                None
            } else {
                inputs.memory_manager.as_deref()
            },
            max_recall_entries: inputs.max_recall_entries,
            max_recall_tokens: inputs.max_recall_tokens,
            tool_observer: observer_ref,
            cancel: inputs.cancel.as_deref(),
            bridge_tools: inputs.bridge_tools.as_deref(),
            suppress_grounding_nudge: system_override.is_some(),
        };

        let mut state = create_chat_loop_state(inputs.agent_config.as_ref());
        let result = service.process_user_text(&mut state, text).await?;
        // Per §5.1 note: an empty assistant response (tool-only turn, budget
        // exhaustion notice) degrades to an empty string; the caller can
        // inspect `system_notice` via a dedicated path when that matters.
        // The orchestrator always wants *some* string to feed back into the
        // next step.
        let reply = result.assistant_text.unwrap_or_default();
        let telemetry = crate::ports::orchestration::TurnTelemetry {
            // `process_user_text` writes per-turn deltas onto state.total_*;
            // we read them here so the value reflects only this turn (the
            // caller mints a fresh ChatLoopState per call).
            prompt_tokens: state.total_input_tokens,
            completion_tokens: state.total_output_tokens,
            model: model_slug,
            latency_ms: started.elapsed().as_millis() as u64,
            response_chars: reply.chars().count() as u32,
        };
        Ok((reply, telemetry))
    }
}

/// Build the harness-owned `Orchestrator` from config + factory + memory.
///
/// Phase 7.1 (full) — only one path remains: `RagPlanner` + `SubprocessRunner`.
/// Returns `None` when:
///   - `config.orchestrator` is absent (orchestration disabled — channels
///     fall back to direct default-agent dispatch).
///
/// `cfg.engine` is left in the config schema for forward-compatibility (a
/// future engine variant could land here without a config break) but the
/// only currently-supported value is `"rag"`. Anything else logs a warning
/// and disables orchestration for that channel.
/// Resolve the planner / runner `session_id` from env-or-fresh.
///
/// Reused across surfaces — `tengu chat` and `tengu telegram` resolve
/// once per process at startup; `tengu webhooks` resolves per request
/// (so each inbound POST has its own recall key). Empty / whitespace-only
/// `TENGU_SESSION_ID` falls through to a fresh UUID so callers don't
/// need to defend against malformed env input.
pub fn resolve_session_id() -> String {
    std::env::var("TENGU_SESSION_ID")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
}

/// The planner's durable-memory lanes: Postgres `agentic_memory` when built
/// with `postgres_memory`, otherwise none (those prompt blocks stay empty).
fn planner_recall_store() -> Option<Arc<dyn crate::ports::memory::RecallStore>> {
    #[cfg(feature = "postgres_memory")]
    {
        Some(Arc::new(
            crate::adapters::outbound::tools::agentic_memory::AgenticRecallStore,
        ))
    }
    #[cfg(not(feature = "postgres_memory"))]
    {
        None
    }
}

/// Embedder for the planner's recall lanes — only when they exist and
/// `OPENROUTER_API_KEY` is set.
fn planner_embedder(
    memory: &crate::config::MemoryConfig,
) -> Option<Arc<dyn crate::ports::memory::Embedding>> {
    if !cfg!(feature = "postgres_memory") {
        return None;
    }
    let api_key = std::env::var("OPENROUTER_API_KEY").ok()?;
    Some(Arc::new(Embedder::new(
        api_key,
        memory.embedding_model.clone(),
    )))
}

pub(crate) fn build_orchestrator(
    config: &Config,
    chat_factory: Arc<dyn ChatServiceFactory>,
    memory: Arc<MemoryManager>,
    session_id: String,
) -> Option<Orchestrator> {
    let cfg = config.orchestrator.as_ref()?;

    if cfg.engine != "rag" {
        tracing::warn!(
            engine = %cfg.engine,
            "[orchestrator] engine != \"rag\" — only the rag engine remains since Phase 7.1; \
             orchestration disabled for this channel"
        );
        return None;
    }

    let chat_port = Arc::new(ChatOrchestratorPortImpl::new(
        Arc::clone(&chat_factory),
        Arc::clone(&memory),
    ));

    // Phase 6.1 (full) — mint the orchestrator event bus *before* the
    // planner so we can hand the same bus to both. RagPlanner emits
    // `OrchestratorEvent::RagQueried` on it; Orchestrator emits
    // `PlanCreated`/`StepStarted`/etc. on the same channel; subscribers
    // (TUI, Telegram adapter) see one unified event stream.
    let bus = crate::application::orchestrator::events::new_bus();

    // Metrics — install the process-global metrics sink and bridge it
    // onto the orchestrator event bus so a single subscriber can render
    // PlanCreated / RagQueried / MetricsRecorded uniformly. Idempotent;
    // subsequent `build_orchestrator` calls reuse the already-installed
    // sink. Bridge task lives as long as the metrics sink exists.
    let metrics_tx = crate::application::metrics::install_global_sink();
    {
        let mut metrics_rx = metrics_tx.subscribe();
        let bus_tx = bus.clone();
        tokio::spawn(async move {
            use tokio::sync::broadcast::error::RecvError;
            loop {
                match metrics_rx.recv().await {
                    Ok(record) => {
                        // `bus.send` returns Err only when zero
                        // subscribers — fine, drop and keep listening.
                        let _ = bus_tx.send(
                            crate::application::orchestrator::events::OrchestratorEvent::MetricsRecorded {
                                record,
                            },
                        );
                    }
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!(dropped = n, "metrics sink subscriber lagged");
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        });
    }

    // Fix B (2026-05-09) — session_id is resolved by the caller and
    // passed in. Single value shared between RagPlanner (recall reads)
    // and SubprocessRunner (compress_and_store writes via IPC).
    // Logged at info-level so users can stitch tracing output back
    // to a specific persisted thread.
    tracing::info!(
        session_id = %session_id,
        sandbox = ?config.sandbox_name,
        "orchestrator: engine=rag, planner=RagPlanner(file-registry), worker=SubprocessRunner"
    );
    let worker: Arc<dyn crate::ports::orchestration::WorkerHandle> = Arc::new(
        crate::adapters::outbound::subprocess_runner::SubprocessRunner::new(
            config.sandbox_name.clone(),
            session_id.clone(),
            config.agents.clone(),
        ),
    );
    let planner: Arc<dyn Planner> = Arc::new(RagPlanner::new(
        cfg.agent.clone(),
        chat_port,
        config.memory.clone(),
        crate::application::orchestrator::shared_files::routable_agents(&config.agents),
        Arc::new(crate::adapters::outbound::tools::CatalogDirectory {
            mcp_servers: config.mcp_servers.clone(),
        }),
        planner_recall_store(),
        planner_embedder(&config.memory),
        Some(bus.clone()),
        session_id,
    ));

    let policy = RetryPolicy::new(cfg.max_attempts_per_step);
    Some(Orchestrator::new(
        planner,
        worker,
        policy,
        cfg.max_replans,
        memory,
        bus,
    ))
}

/// The `[agents.<name>]` block as the `run-agent` child uses it: identical
/// to the in-process config (scopes already folded with `[default_scopes]`
/// by `Config::load`), plus the workspace-tool opt-ins the agent listed in
/// `tools` merged into `workspace_tools`. Single shared allow-list
/// (`crate::domain::tools::WORKSPACE_TOOLS`) — the MCP bridge filters the same way.
pub(crate) fn subagent_config(agent: &AgentConfig) -> AgentConfig {
    let mut cfg = agent.clone();
    for t in &agent.tools {
        if crate::domain::tools::WORKSPACE_TOOLS.contains(&t.as_str())
            && !cfg.workspace_tools.contains(t)
        {
            cfg.workspace_tools.push(t.clone());
        }
    }
    cfg
}

/// Phase 5b — build the per-subprocess tool stack for `tengu run-agent`.
///
/// Resolves `effective_tools = (compute_base_tools ∩ agent.tools) ∪ {compress_and_store}`,
/// then constructs a `PluginToolExecutor` over those tools. Returns the
/// resolved ToolDef list (so `run-agent` can pass it to `engine.run`) plus
/// the executor.
///
/// `compress_and_store` is dispatched out-of-band by the run-agent loop
/// (the summary is captured there and persisted to Postgres
/// `agentic_memory` with `postgres_memory`) so its `ToolDef` is appended
/// to the advertised list but its execution path bypasses the
/// `PluginToolExecutor`.
pub(crate) fn build_subprocess_tool_executor(
    agent: &AgentConfig,
    config: &Config,
    workspace: &Path,
    secret_registry: &Arc<SecretRegistry>,
    activity: Arc<dyn ToolActivityPort>,
    // Phase 7.6 (Bug A) — caller provides a real MemoryManager so the
    // MemoryPlugin can register `persistent_store` / `memory_ingest`.
    // Pre-7.6 this was hardcoded `None`, which meant MCP-routed Claude Code
    // tool calls returned "Tool not available" even when the tool def was
    // advertised. Pass `None` only when the agent definitively has no memory
    // tools in its allow-list.
    memory_manager: Option<Arc<crate::application::memory::manager::MemoryManager>>,
) -> (Vec<ToolDef>, Option<PluginToolExecutor>) {
    let mut agent_cfg = subagent_config(agent);
    grant_workspace_root(&mut agent_cfg.scopes, workspace);

    // Full base tool list (workspace + http + crypto + memory if enabled).
    let base_tools = compute_base_tools(
        true,                  // uses_tools
        config.memory.enabled, // has_memory
        &agent_cfg.workspace_tools,
    );

    // Filter to `tools` when the agent declares an allow-list. Empty
    // `tools` means "no allow-list" — keep all base tools available.
    // compress_and_store is appended unconditionally regardless of `tools`.
    //
    // Workspace-tools opt-ins are always-on for this agent regardless of
    // whether they appear in `tools` — they're separately gated by the
    // workspace_tools allowlist + per-agent declaration. Pre-fix bug: an
    // agent with `tools = ["read_file", ...]` and `workspace_tools = ["foo"]`
    // would NOT get `foo` because `tools` filtered it out.
    let mut effective: Vec<ToolDef> = if agent.tools.is_empty() {
        base_tools
    } else {
        let mut allow: std::collections::HashSet<&str> =
            agent.tools.iter().map(|s| s.as_str()).collect();
        for wt in &agent_cfg.workspace_tools {
            allow.insert(wt.as_str());
        }
        base_tools
            .into_iter()
            .filter(|t| allow.contains(t.name.as_str()))
            .collect()
    };

    // Always-on protocol tool. Phase 5b dispatches it out-of-band, so we
    // only need its description here for the LLM to see + call.
    effective
        .push(crate::adapters::outbound::tools::skill_lifecycle::compress_and_store::definition());

    // Skill registry is empty for the subprocess (skill bodies are loaded
    // separately and merged into the system prompt; no shell-skills exposed
    // as tools yet).
    let skill_registry = crate::application::skills::registry::SkillRegistry::new(Vec::new());

    let executor = build_tool_executor(
        workspace,
        &effective,
        &skill_registry,
        &memory_manager, // Phase 7.6 — passed in by caller
        secret_registry,
        activity,
        None, // cancel
        Some(&config.memory),
        &agent_cfg,
        &config.mcp_servers,
    );

    // `[[mcp_servers]]` tools are only known once the executor has connected
    // to the servers. Advertise them too, through the same `tools` allow-list
    // (empty = all). Claude Code subagents get them via the bridge, which
    // receives this list as `bridge_tools`.
    if let Some(exec) = &executor {
        let mcp_defs: Vec<ToolDef> = exec
            .additional_tool_defs(&effective)
            .into_iter()
            .filter(|d| agent.tools.is_empty() || agent.tools.contains(&d.name))
            .collect();
        effective.extend(mcp_defs);
    }

    (effective, executor)
}

/// Build a minimal `ChatServiceFactory` suitable for CLI commands that need to
/// dispatch a turn against a named agent (e.g. `tengu skill evolve` targeting
/// the skill-improver agent).
///
/// Pre-populates an `OrchestratorSnapshots` table with one `ChatTurnInputs` per
/// agent in `config.agents`, then wraps it in a `RuntimeChatServiceFactory`.
/// Tool execution is intentionally omitted — the skill-improver only needs the
/// engine + memory for text generation; workspace tools can be added later.
pub(crate) async fn build_cli_chat_factory(
    config: &Config,
    workspace: &std::path::Path,
) -> anyhow::Result<Arc<dyn ChatServiceFactory>> {
    use crate::adapters::outbound::engines::build_engine;
    use crate::application::memory::manager::MemoryManager;

    // Build a shared memory manager (no vector backend for CLI — acceptable
    // degradation; the improver only needs text generation context).
    let memory: Arc<MemoryManager> = Arc::new(MemoryManager::new());

    let snapshots: OrchestratorSnapshots =
        Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));

    for (name, agent_cfg) in &config.agents {
        let engine_box = build_engine(name, agent_cfg, config.claude_code.as_ref())?;
        let compaction_policy = crate::application::chat::flow::resolve_flow_compaction_policy(
            &agent_cfg.flow,
            agent_cfg.limits.max_tokens_per_flow,
            engine_box.context_window(),
            engine_box.max_output_tokens_per_turn() as usize,
        );
        let engine: Arc<dyn Engine> = Arc::from(engine_box);
        let system_prompt =
            crate::application::skills::registry::build_system_prompt(agent_cfg, false, &[]);
        let history_turn_limit =
            crate::application::chat::flow::resolve_history_turn_limit(&agent_cfg.flow);
        let inputs = ChatTurnInputs {
            engine,
            agent_id: name.clone(),
            agent_config: Arc::new(agent_cfg.clone()),
            history_turn_limit,
            compaction_policy,
            system_prompt,
            tools: Vec::new(),
            tool_executor: None,
            memory_manager: Some(Arc::clone(&memory)),
            max_recall_entries: 10,
            max_recall_tokens: 2000,
            bridge_tools: None,
            tool_observer: None,
            cancel: None,
        };
        snapshots
            .write()
            .map_err(|e| anyhow::anyhow!("snapshots lock poisoned: {e}"))?
            .insert(name.clone(), inputs);
    }

    let _ = workspace; // workspace available for future tool wiring
    let inputs_fn = snapshots_inputs_fn(snapshots);
    Ok(Arc::new(RuntimeChatServiceFactory::new(inputs_fn)))
}

// ---------------------------------------------------------------------------
// Golden registry test — asserts the LLM-facing tool surface is unchanged.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod golden_tests {
    use super::*;
    use crate::config::Config;
    use crate::domain::message::ToolCall;
    use std::collections::HashSet;
    use tempfile::TempDir;

    struct StubActivity;
    impl ToolActivityPort for StubActivity {
        fn publish_tool_activity(&self, _call: &ToolCall) {}
    }

    /// A configured `[agents.*.scopes.<tool>]` entry is used verbatim; an
    /// unconfigured tool falls back to `permissive_scope`.
    #[test]
    fn configured_scope_is_used_and_unconfigured_falls_back_to_permissive() {
        let tmp = TempDir::new().unwrap();
        let mut configured: HashMap<String, ToolScope> = HashMap::new();
        configured.insert(
            "http_request".to_string(),
            ToolScope {
                net_hosts: vec!["api.example.com".to_string()],
                env_reads: vec!["EXAMPLE_API_KEY".to_string()],
                ..Default::default()
            },
        );

        let scopes = resolve_tool_scopes(
            tmp.path(),
            &configured,
            vec!["http_request".to_string(), "read_file".to_string()],
        );

        // Configured scope: exact allow-list, no wildcard.
        let http = &scopes["http_request"];
        assert!(http.check_net_host("api.example.com").is_ok());
        assert!(http.check_net_host("evil.example.org").is_err());
        assert!(http.check_env_read("EXAMPLE_API_KEY").is_ok());
        assert!(http.check_env_read("OTHER").is_err());
        assert!(http.check_fs_read(tmp.path()).is_err());

        // Unconfigured tool: permissive fallback.
        let read = &scopes["read_file"];
        assert!(read.check_net_host("anything.example").is_ok());
        assert!(read.check_env_read("ANY").is_ok());
        assert!(read.check_shell_bin("rm").is_ok());
        assert!(read.check_fs_read(tmp.path()).is_ok());
    }

    /// End-to-end through `build_tool_executor`: the executor's scope map
    /// carries the agent's configured entry, not the permissive default.
    #[test]
    fn subagent_config_merges_workspace_tool_optins_from_tools() {
        let mut agent = crate::config::Config::default()
            .agents
            .remove("main")
            .unwrap();
        agent.tools = vec![
            "http_request".into(),
            "shared_cache".into(),
            "persistent_store".into(),
        ];
        agent.workspace_tools = vec!["persistent_store".into()];
        let cfg = subagent_config(&agent);
        assert_eq!(
            cfg.workspace_tools,
            vec!["persistent_store", "shared_cache"]
        );
        assert_eq!(cfg.tools, agent.tools);
        assert_eq!(cfg.engine, agent.engine);
    }

    #[test]
    fn build_tool_executor_honours_agent_scopes() {
        let tmp = TempDir::new().unwrap();
        let tools = crate::adapters::outbound::tools::http::tool_defs();

        let mut config = Config::default();
        let agent_config = config.agents.get_mut("main").unwrap();
        agent_config.scopes.insert(
            "http_request".to_string(),
            ToolScope {
                net_hosts: vec!["api.example.com".to_string()],
                ..Default::default()
            },
        );
        let agent_config = config.agents.get("main").unwrap();
        let skill_registry = crate::application::skills::registry::SkillRegistry::new(Vec::new());
        let activity: Arc<dyn ToolActivityPort> = Arc::new(StubActivity);
        let secret_registry = Arc::new(SecretRegistry::new());

        let executor = build_tool_executor(
            tmp.path(),
            &tools,
            &skill_registry,
            &None,
            &secret_registry,
            activity,
            None,
            None,
            agent_config,
            &[],
        )
        .expect("executor");

        let scope = &executor.scopes["http_request"];
        assert_eq!(scope.net_hosts, vec!["api.example.com".to_string()]);
        assert!(scope.check_net_host("openrouter.ai").is_err());
    }

    /// Assert that the set of registered tool names equals the pre-migration set
    /// when building a default-configured executor with every opt-in tool enabled.
    /// `persistent_store` is excluded because it requires a live embedding API.
    #[test]
    fn tool_names_match_pre_migration_surface() {
        let tmp = TempDir::new().unwrap();

        // Compute the same base-tool list the channel adapters use at startup.
        let mut tools = crate::adapters::outbound::tools::workspace::tool_defs();
        tools.extend(crate::adapters::outbound::tools::memory::tool_defs());
        tools.extend(crate::adapters::outbound::tools::cache::tool_defs());
        tools.extend(crate::adapters::outbound::tools::http::tool_defs());
        tools.extend(crate::adapters::outbound::tools::crypto::tool_defs());

        let config = Config::default();
        let agent_config = config.agents.get("main").unwrap();
        let skill_registry = crate::application::skills::registry::SkillRegistry::new(Vec::new());
        let activity: Arc<dyn ToolActivityPort> = Arc::new(StubActivity);
        let secret_registry = Arc::new(SecretRegistry::new());

        let executor = build_tool_executor(
            tmp.path(),
            &tools,
            &skill_registry,
            &None, // memory_manager: omit — memory plugin gates on ctx.memory_manager.
            &secret_registry,
            activity,
            None,
            None,
            agent_config,
            &[], // mcp_servers: default install has no MCP servers configured.
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
        // `memory_ingest` and `memory_search` require a memory handle — only
        // present when memory is enabled. `persistent_store` requires memory
        // too and is opt-in.
        if names.contains("memory_ingest") {
            expected.insert("memory_ingest".to_string());
        }
        if names.contains("memory_search") {
            expected.insert("memory_search".to_string());
        }

        assert_eq!(
            names, expected,
            "tool surface drift: registry has {:?}, expected {:?}",
            names, expected
        );
    }

    // A plan-step subagent must see `[[mcp_servers]]` tools, filtered by its
    // `tools` allow-list like every other tool. Multi-thread runtime: the
    // executor build `block_on`s plugin registration (as in `run-agent`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn subprocess_executor_advertises_mcp_server_tools() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut config = Config::default();
        config.mcp_servers = vec![crate::adapters::outbound::mcp_client::tests::fake_server(
            "fake",
        )];
        let base = config.agents.get("main").unwrap().clone();
        let secrets = Arc::new(SecretRegistry::new());
        let advertised = |tools: &[&str]| {
            let mut agent = base.clone();
            agent.tools = tools.iter().map(|s| s.to_string()).collect();
            let (defs, _) = build_subprocess_tool_executor(
                &agent,
                &config,
                tmp.path(),
                &secrets,
                Arc::new(crate::adapters::outbound::noop::NoopActivity),
                None,
            );
            defs.into_iter().map(|d| d.name).collect::<Vec<_>>()
        };

        assert!(advertised(&[]).contains(&"fake__echo".to_string()));
        assert!(!advertised(&["read_file"]).contains(&"fake__echo".to_string()));
        let listed = advertised(&["read_file", "fake__echo"]);
        assert!(listed.contains(&"fake__echo".to_string()));
        assert!(listed.contains(&"read_file".to_string()));
    }
}
