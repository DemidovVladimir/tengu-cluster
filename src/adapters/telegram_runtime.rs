//! Headless Telegram bot adapter that wires TelegramPipe → ChatRuntimeService.
//!
//! This adapter provides the full Telegram integration:
//!
//! - **Inline keyboard approval** — dangerous tools (write_file, run_command)
//!   prompt the user with Approve/Deny
//!   buttons via `TelegramInlineApprovalAdapter`. 60-second timeout auto-denies.
//! - **Typing indicator** — runs as an independent `tokio::spawn` task so it
//!   stays alive during synchronous tool execution.
//! - **File attachments** — documents and photos are downloaded and saved to
//!   `{workspace}/.tengu-attachments/`, paths prepended to message content.
//! - **Message chunking** — long responses split at `\n\n` boundaries, max 4000
//!   chars per chunk (Telegram's 4096 limit with safety margin).
//! - **Per-user state** — each Telegram user gets their own `ChatLoopState`.
//! - **Secret redaction** — all outbound text passes through `SecretRegistry::redact`.
//! - **Hot-reload** — skills are re-scanned on each message if files changed.

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
use crate::application::chat_commands::{self, CommandResult, EngineInfo};
use crate::application::chat_runtime::ChatRuntimeService;
use crate::application::engine_runtime::{SanitizedToolExecutor, ToolExecutor};
use crate::application::flow_policy::resolve_flow_compaction_policy;
use crate::application::memory_service::MemoryService;
use crate::application::ports::{ToolActivityPort, ToolApprovalPort};
use crate::application::skill_registry::SkillRegistry;
use crate::application::tool_use_service::ToolUseService;
use crate::application::workspace_tools_catalog::{
    build_memory_tools, build_workspace_tools, filter_tools_by_allowlist,
};
use crate::domain::chat::{resolve_history_turn_limit, ChatLoopState};
use crate::domain::secret_registry::SecretRegistry;
use crate::domain::tool_policy::ToolPolicyCatalog;
use crate::resolve_tengu_home;
use tengu_core::config::Config;
use tengu_core::types::{DeliveryOptions, ToolCall, ToolDef};
use tengu_core::{Engine, Lens, Pipe, PipeContext, Refiner};
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

/// Shared state for the current recipient — updated before each engine turn.
type CurrentRecipient = Arc<std::sync::Mutex<Option<tengu_core::types::Recipient>>>;

/// Telegram inline keyboard approval adapter.
///
/// Sends an inline keyboard with Approve/Deny buttons and blocks the
/// current thread (via `block_in_place`) until the user responds or
/// the timeout expires (default: 60 seconds).
struct TelegramInlineApprovalAdapter {
    pipe: Arc<tengu_channels::telegram::TelegramPipe>,
    current_recipient: CurrentRecipient,
}

/// Timeout for waiting on user approval via inline keyboard.
const APPROVAL_TIMEOUT_SECS: u64 = 60;

impl ToolApprovalPort for TelegramInlineApprovalAdapter {
    fn request_tool_approval(&self, call: &ToolCall) -> Result<bool> {
        let recipient = self
            .current_recipient
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow::anyhow!("No recipient set for approval"))?;

        let (title, description, preview) = build_approval_text(call);
        let approval_id = format!("tool_{}", uuid::Uuid::new_v4().simple());
        let mut text = format!("🔐 *{}*\n{}", title, description);
        if !preview.is_empty() {
            // Truncate preview for Telegram message limits.
            let truncated = if preview.len() > 500 {
                let mut end = 500;
                while end > 0 && !preview.is_char_boundary(end) {
                    end -= 1;
                }
                format!("{}…", &preview[..end])
            } else {
                preview
            };
            text.push_str(&format!("\n```\n{}\n```", truncated));
        }

        let pipe = Arc::clone(&self.pipe);
        let aid = approval_id.clone();

        // Bridge async → sync: block_in_place lets us await inside a sync fn
        // on a multi-thread tokio runtime.
        tokio::task::block_in_place(move || {
            let handle = tokio::runtime::Handle::current();
            handle.block_on(async {
                match pipe.send_inline_approval(&recipient, &aid, &text).await {
                    Ok(rx) => {
                        match tokio::time::timeout(
                            std::time::Duration::from_secs(APPROVAL_TIMEOUT_SECS),
                            rx,
                        )
                        .await
                        {
                            Ok(Ok(approved)) => {
                                info!(tool = %call.name, approved, "Inline keyboard approval response");
                                Ok(approved)
                            }
                            Ok(Err(_)) => {
                                warn!(tool = %call.name, "Approval channel closed — denying");
                                Ok(false)
                            }
                            Err(_) => {
                                warn!(tool = %call.name, "Approval timed out — denying");
                                Ok(false)
                            }
                        }
                    }
                    Err(e) => {
                        error!(tool = %call.name, error = %e, "Failed to send approval request — denying");
                        Ok(false)
                    }
                }
            })
        })
    }
}

/// Build title, description, and preview text for an approval prompt.
fn build_approval_text(call: &ToolCall) -> (String, String, String) {
    match call.name.as_str() {
        "run_command" => {
            let cmd = call
                .arguments
                .get("command")
                .or_else(|| call.arguments.get("cmd"))
                .and_then(|v| v.as_str())
                .unwrap_or("<unknown>");
            (
                "Run Command".to_string(),
                "Allow this command to execute?".to_string(),
                cmd.to_string(),
            )
        }
        "write_file" => {
            let path = call
                .arguments
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("<unknown>");
            let content = call
                .arguments
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            (
                "Write File".to_string(),
                format!("Allow write to '{}'?", path),
                content.to_string(),
            )
        }
        _ => {
            let args_str = serde_json::to_string_pretty(&call.arguments)
                .unwrap_or_else(|_| format!("{:?}", call.arguments));
            (
                "Tool Confirmation".to_string(),
                format!("Allow '{}' to run?", call.name),
                args_str,
            )
        }
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
// Cross-agent activity log
// ---------------------------------------------------------------------------

/// Maximum activity entries retained across all agents.
const MAX_ACTIVITY_ENTRIES: usize = 10;

/// Maximum characters of an agent's response kept in the activity summary.
const MAX_ACTIVITY_SUMMARY_CHARS: usize = 400;

/// A record of what one agent did in a single turn.
struct ActivityEntry {
    agent_label: String,
    agent_id: String,
    tools_used: Vec<String>,
    response_summary: String,
}

/// Format a tool call for the activity log.
/// Read-only tools are skipped — only mutations are interesting for other agents.
fn format_tool_for_activity(call: &ToolCall) -> Option<String> {
    match call.name.as_str() {
        "write_file" => {
            let path = call
                .arguments
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            Some(format!("wrote `{}`", path))
        }
        "run_command" => {
            let cmd = call
                .arguments
                .get("command")
                .or_else(|| call.arguments.get("cmd"))
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let truncated = if cmd.len() > 80 {
                let mut end = 80;
                while end > 0 && !cmd.is_char_boundary(end) {
                    end -= 1;
                }
                format!("{}…", &cmd[..end])
            } else {
                cmd.to_string()
            };
            Some(format!("ran `{}`", truncated))
        }
        _ => None,
    }
}

/// Build a context block summarising what OTHER agents have done recently.
fn build_activity_context(activity_log: &[ActivityEntry], current_agent_id: &str) -> String {
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
         IMPORTANT: Use read_file / list_directory to check files they created before making changes.\n\n",
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
        ctx.push('\n');
    }

    ctx
}

/// Truncate text to at most `max` chars on a char boundary, appending "…" if cut.
fn truncate_summary(text: &str, max: usize) -> String {
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
// Multi-agent orchestration (plan-and-execute for Telegram)
// ---------------------------------------------------------------------------

/// Directory inside workspace where task outcomes are stored.
const TASK_OUTCOMES_DIR: &str = ".tengu-tasks";

/// Build the prompt for a task, including outcomes of dependency tasks.
fn build_task_prompt(goal: &str, task_description: &str, dep_outcomes: &[(String, String)]) -> String {
    let mut prompt = format!("## Goal\n{}\n\n## Your Task\n{}\n", goal, task_description);

    if !dep_outcomes.is_empty() {
        prompt.push_str("\n## Completed dependency tasks — READ BEFORE STARTING\n");
        prompt.push_str("The following tasks were completed before yours. Read the outcome files for full details.\n\n");
        for (task_id, outcome_path) in dep_outcomes {
            prompt.push_str(&format!("- **{}** → outcome at `{}`\n", task_id, outcome_path));
        }
        prompt.push_str("\nUse read_file on these outcome files to see what was done and build on it.\n");
    }

    prompt.push_str("\n## IMPORTANT\n\
        When you finish, write a concise outcome summary to the file path provided below.\n\
        Include: what you did, key results, file paths you created, URLs, IDs, and any info the next agent needs.\n");

    prompt
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
    approval: Arc<dyn ToolApprovalPort>,
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

    let composite = Arc::new(composite);

    let service = ToolUseService::new(
        ToolPolicyCatalog::from_tools(tools),
        Arc::new(TelegramToolActivityAdapter),
        approval,
        composite,
    );
    Some(TelegramToolExecutor { service })
}

fn rebuild_system_prompt(
    agent_config: &tengu_core::config::AgentConfig,
    advertise_workspace_tools: bool,
    skill_registry: &SkillRegistry,
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

/// Per-agent runtime state for multi-agent Telegram support.
struct TelegramAgentState {
    agent_id: String,
    agent_config: tengu_core::config::AgentConfig,
    engine: Box<dyn Engine>,
    engine_info: EngineInfo,
    workspace: Option<std::path::PathBuf>,
    base_tools: Vec<ToolDef>,
    skill_source: Option<FileSystemSkillSource>,
    skill_registry: SkillRegistry,
    current_tools: Vec<ToolDef>,
    current_system_prompt: String,
    advertise_workspace_tools: bool,
    history_turn_limit: usize,
    compaction_policy: crate::domain::chat::FlowCompactionPolicy,
    role: Option<String>,
}

/// Parse agent routing from message text.
///
/// Supports two formats:
/// 1. `@role: message` — explicit routing (always accepted, even unknown roles)
/// 2. `role: message`  — implicit routing (only when `role` matches a known agent)
///
/// Returns `(Some(role_key), message)` if found, `(None, original)` otherwise.
fn parse_agent_routing(
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

/// Infer the target agent by matching whole words against known role keys / agent IDs.
///
/// Returns `Some(agent_id)` when exactly one agent is matched. Returns `None` when
/// zero or multiple different agents match (ambiguous).
fn infer_agent_from_keywords(
    text: &str,
    role_to_agent: &HashMap<String, String>,
) -> Option<String> {
    let mut matched: Option<String> = None;
    for word in text.split_whitespace() {
        let normalized = word
            .trim_matches(|c: char| !c.is_alphanumeric() && c != '_')
            .to_lowercase()
            .replace('-', "_");
        if let Some(agent_id) = role_to_agent.get(&normalized) {
            match matched {
                None => matched = Some(agent_id.clone()),
                Some(ref prev) if prev == agent_id => {} // same agent, ok
                Some(_) => return None,                  // ambiguous
            }
        }
    }
    matched
}

/// Run the headless Telegram bot adapter (sync — call from `block_in_place`).
///
/// Creates its own tokio runtime internally so that state containing nested
/// runtimes (e.g. `MemoryToolExecutionAdapter`) drops in a sync context,
/// avoiding the "Cannot drop a runtime in a context where blocking is not
/// allowed" panic.
///
/// Supports multi-agent routing: prefix messages with `@role: message` to
/// target a specific agent. Unrouted messages go to the default agent.
/// Use `/agents` to list available agents and their roles.
pub(crate) fn run_telegram(
    config: Config,
    secret_registry: Arc<SecretRegistry>,
) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Failed to create Telegram runtime");

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

    let flow_store = FlowStore::new(&resolve_tengu_home())?;
    let memory_config = config.memory.clone();

    // Build shared memory handle.
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

    let has_memory = memory_handle.is_some();

    // Apply workspace scaffold if configured.
    crate::adapters::scaffold::maybe_apply_scaffold(&config);

    // -----------------------------------------------------------------------
    // Build per-agent runtime state for ALL configured agents.
    // -----------------------------------------------------------------------
    let mut agent_states: HashMap<String, TelegramAgentState> = HashMap::new();
    // Map from role_key -> agent_id for routing.
    let mut role_to_agent: HashMap<String, String> = HashMap::new();
    let mut default_agent_id: Option<String> = None;

    for (agent_id, agent_config) in &config.agents {
        let engine = match build_engine(agent_id, agent_config) {
            Ok(e) => e,
            Err(e) => {
                warn!(agent_id = %agent_id, error = %e, "Failed to build engine, skipping");
                continue;
            }
        };

        let engine_info = EngineInfo {
            context_window: engine.context_window(),
            diagnostics: engine.diagnostics(),
        };

        let workspace: Option<std::path::PathBuf> = agent_config
            .workspace
            .as_ref()
            .map(|p| workspace_tools::expand_tilde(p));

        let advertise_workspace_tools =
            engine.supports_tool_use() && !engine.manages_own_workspace();
        let uses_tools = advertise_workspace_tools && workspace.is_some();

        let base_tools = if !uses_tools {
            vec![]
        } else {
            let mut all_tools = filter_tools_by_allowlist(
                build_workspace_tools(),
                agent_config.allowed_tools.as_deref(),
            );
            if has_memory {
                all_tools.extend(build_memory_tools());
            }
            all_tools
        };

        let skill_source: Option<FileSystemSkillSource> =
            workspace.as_ref().map(|ws| FileSystemSkillSource::new(ws.clone()));

        let base_reserved: Vec<String> = base_tools.iter().map(|t| t.name.clone()).collect();
        let mut skill_registry = SkillRegistry::new(base_reserved);
        if let Some(ref src) = skill_source {
            skill_registry.reload(src);
        }

        let current_tools = rebuild_tools(&base_tools, &skill_registry);
        let current_system_prompt = rebuild_system_prompt(
            agent_config,
            advertise_workspace_tools,
            &skill_registry,
        );

        let history_turn_limit = resolve_history_turn_limit(&agent_config.flow);
        let compaction_policy = resolve_flow_compaction_policy(
            &agent_config.flow,
            agent_config.limits.max_tokens_per_flow,
            engine.context_window(),
            engine.max_output_tokens_per_turn() as usize,
        );

        // Track role -> agent mapping for routing.
        let role = agent_config.role.clone();
        if let Some(ref role_str) = role {
            let role_key = role_str.trim().to_lowercase().replace('-', "_");
            if !role_key.is_empty() {
                role_to_agent.insert(role_key, agent_id.clone());
            }
        }
        // Also allow routing by agent_id directly.
        role_to_agent.insert(agent_id.clone(), agent_id.clone());

        if agent_config.default || default_agent_id.is_none() {
            if agent_config.default {
                default_agent_id = Some(agent_id.clone());
            } else if default_agent_id.is_none() {
                default_agent_id = Some(agent_id.clone());
            }
        }

        info!(
            agent_id = %agent_id,
            role = ?role,
            tools = current_tools.len(),
            "Registered Telegram agent"
        );

        agent_states.insert(
            agent_id.clone(),
            TelegramAgentState {
                agent_id: agent_id.clone(),
                agent_config: agent_config.clone(),
                engine,
                engine_info,
                workspace,
                base_tools,
                skill_source,
                skill_registry,
                current_tools,
                current_system_prompt,
                advertise_workspace_tools,
                history_turn_limit,
                compaction_policy,
                role,
            },
        );
    }

    let default_agent_id = default_agent_id
        .ok_or_else(|| anyhow::anyhow!("No agents configured"))?;

    // Remember each agent's base workspace so /project can create subdirs.
    let base_workspaces: HashMap<String, std::path::PathBuf> = agent_states
        .iter()
        .filter_map(|(id, a)| a.workspace.as_ref().map(|w| (id.clone(), w.clone())))
        .collect();

    // Inject team awareness into each agent's system prompt so agents know
    // about each other and can suggest routing for out-of-scope requests.
    if agent_states.len() > 1 {
        let mut team_block = String::from("\n\n## Team Members\n");
        team_block.push_str(
            "If a request is outside your expertise, suggest the user route to the right agent.\n",
        );
        team_block.push_str("Format: @role: message\n\n");
        for state in agent_states.values() {
            let role_key = state.role.as_deref().unwrap_or(&state.agent_id);
            let name = state
                .agent_config
                .identity
                .name
                .as_deref()
                .unwrap_or(&state.agent_id);
            team_block.push_str(&format!("- @{}: {}\n", role_key, name));
        }

        for state in agent_states.values_mut() {
            state.current_system_prompt.push_str(&team_block);
        }
    }

    // Build agent descriptions for the planner prompt (multi-agent orchestration).
    let agent_descriptions: HashMap<String, String> = agent_states
        .iter()
        .map(|(aid, astate)| {
            let role_key = astate.role.as_deref().unwrap_or(aid).to_string();
            let name = astate
                .agent_config
                .identity
                .name
                .as_deref()
                .unwrap_or(aid);
            let brief = astate
                .agent_config
                .identity
                .instructions
                .as_deref()
                .and_then(|s| s.lines().find(|l| !l.trim().is_empty()))
                .unwrap_or("AI assistant");
            (role_key, format!("{} — {}", name, brief))
        })
        .collect();

    info!(
        agents = agent_states.len(),
        default = %default_agent_id,
        "Telegram multi-agent setup complete"
    );

    // Pending approvals map shared between the pipe's callback handler and the approval adapter.
    let pending_approvals: tengu_channels::telegram::PendingApprovals =
        Arc::new(std::sync::Mutex::new(HashMap::new()));

    // Turn-cancellation flag.
    let turn_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let pipe = Arc::new(tengu_channels::telegram::TelegramPipe::with_approvals(
        bot_token,
        Arc::clone(&pending_approvals),
        Arc::clone(&turn_cancel),
    ));
    let (inbound_tx, mut inbound_rx) = tokio::sync::mpsc::channel(256);
    rt.block_on(pipe.connect(PipeContext { inbound_tx }))?;

    let current_recipient: CurrentRecipient = Arc::new(std::sync::Mutex::new(None));
    let approval_adapter: Arc<dyn ToolApprovalPort> = Arc::new(TelegramInlineApprovalAdapter {
        pipe: Arc::clone(&pipe),
        current_recipient: Arc::clone(&current_recipient),
    });

    let memory_service_instance = memory_handle
        .as_ref()
        .map(|h| MemoryService::new(h.embedding.as_ref(), h.store.as_ref()));

    // Per-user-per-agent conversation states: keyed by "sender_id:agent_id".
    let mut user_states: HashMap<String, ChatLoopState> = HashMap::new();

    // Track which agent each user last talked to (for showing in prompts).
    let mut user_active_agent: HashMap<String, String> = HashMap::new();

    // Cross-agent activity log — lets each agent see what others have done.
    let mut activity_log: Vec<ActivityEntry> = Vec::new();

    let is_multi_agent = agent_states.len() > 1;

    info!("Telegram bot started — waiting for messages (Ctrl+C to stop)");

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

            // Handle slash commands.
            if msg.content.starts_with('/') {
                if msg.content == "/stop" || msg.content.starts_with("/stop@") {
                    let _ = pipe
                        .send_text(&msg.sender, "No active operation to stop.", &delivery_opts)
                        .await;
                    continue;
                }

                // /team <goal> — orchestrate a goal across multiple agents.
                if msg.content.starts_with("/team") {
                    let goal = msg.content.trim_start_matches("/team").trim();
                    if goal.is_empty() || !is_multi_agent {
                        let hint = if !is_multi_agent {
                            "Only one agent configured — /team requires multiple agents."
                        } else {
                            "Usage: /team <goal>\nExample: /team build a full stack Rust app"
                        };
                        let _ = pipe.send_text(&msg.sender, hint, &delivery_opts).await;
                        continue;
                    }

                    // Set recipient for inline approvals.
                    *current_recipient.lock().unwrap() = Some(msg.sender.clone());

                    // Save attached files (PDF, images) so agents can access them.
                    let mut attachment_notes: Vec<String> = Vec::new();
                    if let Some(ref media) = msg.media {
                        // Use default agent's workspace for attachments.
                        let ws = agent_states
                            .get(&default_agent_id)
                            .and_then(|a| a.workspace.as_ref());
                        if let Some(ws) = ws {
                            let attachments_dir = ws.join(".tengu-attachments");
                            std::fs::create_dir_all(&attachments_dir).ok();
                            for m in media {
                                let fname =
                                    m.filename.as_deref().unwrap_or("attachment");
                                let path = attachments_dir.join(fname);
                                match std::fs::write(&path, &m.data) {
                                    Ok(()) => {
                                        info!(path = %path.display(), size = m.data.len(), "Saved Telegram attachment for /team");
                                        attachment_notes.push(format!(
                                            "[Attached file: {} ({}, {} bytes)]",
                                            path.display(),
                                            m.mime_type,
                                            m.data.len()
                                        ));
                                    }
                                    Err(e) => {
                                        warn!(error = %e, "Failed to save Telegram attachment for /team");
                                    }
                                }
                            }
                        }
                    }

                    // Build goal text including attachment paths so agents know where files are.
                    let goal = if attachment_notes.is_empty() {
                        goal.to_string()
                    } else {
                        format!("{}\n{}", attachment_notes.join("\n"), goal)
                    };

                    let _ = pipe
                        .send_text(&msg.sender, "Planning…", &delivery_opts)
                        .await;

                    // Generate plan using the default agent's engine.
                    let planner_engine = match agent_states.get(&default_agent_id) {
                        Some(a) => a.engine.as_ref(),
                        None => {
                            let _ = pipe
                                .send_text(&msg.sender, "Planner engine unavailable.", &delivery_opts)
                                .await;
                            continue;
                        }
                    };

                    let tasks = match crate::application::task_planner::generate_plan(
                        planner_engine,
                        &goal,
                        &agent_descriptions,
                    )
                    .await
                    {
                        Ok(s) => s,
                        Err(e) => {
                            let _ = pipe
                                .send_text(
                                    &msg.sender,
                                    &format!("Failed to generate plan: {}", e),
                                    &delivery_opts,
                                )
                                .await;
                            continue;
                        }
                    };

                    // Resolve execution order (parallel batches).
                    let batches = match crate::application::task_planner::resolve_execution_order(&tasks) {
                        Ok(b) => b,
                        Err(e) => {
                            let _ = pipe
                                .send_text(&msg.sender, &format!("Bad plan: {}", e), &delivery_opts)
                                .await;
                            continue;
                        }
                    };

                    // Show plan to user.
                    let mut plan_text = format!("Plan ({} tasks, {} batches):\n", tasks.len(), batches.len());
                    for (bi, batch) in batches.iter().enumerate() {
                        let parallel_note = if batch.len() > 1 { " [parallel]" } else { "" };
                        plan_text.push_str(&format!("Batch {}{}:\n", bi + 1, parallel_note));
                        for &idx in batch {
                            let t = &tasks[idx];
                            let deps = if t.depends_on.is_empty() {
                                String::new()
                            } else {
                                format!(" (after: {})", t.depends_on.join(", "))
                            };
                            plan_text.push_str(&format!("  - [{}] {}{}\n", t.role, t.task, deps));
                        }
                    }
                    let _ = pipe
                        .send_text(&msg.sender, &plan_text, &delivery_opts)
                        .await;

                    // Prepare outcomes dir in workspace.
                    let outcomes_dir = agent_states
                        .get(&default_agent_id)
                        .and_then(|a| a.workspace.as_ref())
                        .map(|ws| ws.join(TASK_OUTCOMES_DIR));
                    if let Some(ref dir) = outcomes_dir {
                        std::fs::create_dir_all(dir).ok();
                    }

                    // Track outcome file paths by task id.
                    let mut outcome_paths: HashMap<String, String> = HashMap::new();

                    turn_cancel.store(false, std::sync::atomic::Ordering::Relaxed);
                    let mut completed_count = 0usize;
                    let mut stopped = false;

                    'batch_loop: for (bi, batch) in batches.iter().enumerate() {
                        if turn_cancel.load(std::sync::atomic::Ordering::Relaxed) {
                            let _ = pipe
                                .send_text(&msg.sender, "Stopped by /stop.", &delivery_opts)
                                .await;
                            stopped = true;
                            break;
                        }

                        if batch.len() > 1 {
                            let labels: Vec<String> = batch
                                .iter()
                                .map(|&idx| tasks[idx].role.clone())
                                .collect();
                            let _ = pipe
                                .send_text(
                                    &msg.sender,
                                    &format!("Batch {}/{} — running in parallel: {}", bi + 1, batches.len(), labels.join(", ")),
                                    &delivery_opts,
                                )
                                .await;
                        }

                        // Run all tasks in this batch. For parallel batches we
                        // unfortunately cannot run truly concurrently because
                        // agent_states requires &mut and ChatRuntimeService
                        // borrows from it. So we run them sequentially within a
                        // batch for now — but the key design improvement is that
                        // agents communicate via outcome files, not in-memory
                        // prompt chains.
                        for &task_idx in batch {
                            if turn_cancel.load(std::sync::atomic::Ordering::Relaxed) {
                                let _ = pipe
                                    .send_text(&msg.sender, "Stopped by /stop.", &delivery_opts)
                                    .await;
                                stopped = true;
                                break 'batch_loop;
                            }

                            let task = &tasks[task_idx];
                            let task_agent_id = match role_to_agent.get(&task.role) {
                                Some(id) => id.clone(),
                                None => {
                                    let _ = pipe
                                        .send_text(
                                            &msg.sender,
                                            &format!("Unknown role '{}', skipping task '{}'.", task.role, task.id),
                                            &delivery_opts,
                                        )
                                        .await;
                                    continue;
                                }
                            };

                            let agent = match agent_states.get_mut(&task_agent_id) {
                                Some(a) => a,
                                None => continue,
                            };

                            let agent_label = agent
                                .agent_config
                                .identity
                                .name
                                .clone()
                                .unwrap_or_else(|| agent.agent_id.clone());

                            let _ = pipe
                                .send_text(
                                    &msg.sender,
                                    &format!("[{}] {}", agent_label, task.task),
                                    &delivery_opts,
                                )
                                .await;

                            // Hot-reload skills.
                            if let Some(ref src) = agent.skill_source {
                                if agent.skill_registry.reload(src) {
                                    agent.current_tools =
                                        rebuild_tools(&agent.base_tools, &agent.skill_registry);
                                    agent.current_system_prompt = rebuild_system_prompt(
                                        &agent.agent_config,
                                        agent.advertise_workspace_tools,
                                        &agent.skill_registry,
                                    );
                                }
                            }

                            // Build executor.
                            let task_executor: Option<TelegramToolExecutor> =
                                agent.workspace.as_ref().and_then(|ws| {
                                    rebuild_executor(
                                        ws,
                                        &agent.current_tools,
                                        &agent.skill_registry,
                                        &memory_handle,
                                        &secret_registry,
                                        Arc::clone(&approval_adapter),
                                    )
                                });
                            let task_sanitized = task_executor.as_ref().map(|e| {
                                SanitizedToolExecutor::new(e as &dyn ToolExecutor, &secret_registry)
                            });

                            // Tool observer — notify user of tool calls.
                            let obs_pipe = Arc::clone(&pipe);
                            let obs_sender = msg.sender.clone();
                            let obs_secrets = Arc::clone(&secret_registry);
                            let obs_label = agent_label.clone();
                            let task_observer = move |call: &ToolCall, result: &str| {
                                let _ = obs_secrets.redact(result);
                                let text = format!("[{}] 🔧 `{}`", obs_label, call.name);
                                let p = Arc::clone(&obs_pipe);
                                let r = obs_sender.clone();
                                tokio::task::block_in_place(move || {
                                    let handle = tokio::runtime::Handle::current();
                                    handle.block_on(async {
                                        let _ = p.send_text(&r, &text, &DeliveryOptions::default()).await;
                                    });
                                });
                            };

                            // Collect dependency outcome paths for this task.
                            let dep_outcomes: Vec<(String, String)> = task
                                .depends_on
                                .iter()
                                .filter_map(|dep_id| {
                                    outcome_paths.get(dep_id).map(|p| (dep_id.clone(), p.clone()))
                                })
                                .collect();

                            // Build prompt: goal + task + where to find dep outcomes + where to write own outcome.
                            let outcome_rel_path = format!("{}/{}.md", TASK_OUTCOMES_DIR, task.id);
                            let mut prompt = build_task_prompt(&goal, &task.task, &dep_outcomes);
                            prompt.push_str(&format!(
                                "\nWrite your outcome summary to: `{}`\n",
                                outcome_rel_path
                            ));

                            let state_key = format!("{}:{}", sender_id, task_agent_id);
                            let state = user_states
                                .entry(state_key)
                                .or_insert_with(|| ChatLoopState {
                                    messages: Vec::new(),
                                    active_flow_key: None,
                                    manual_session_id: None,
                                    flow_token_usage: 0,
                                    active_lens: agent
                                        .agent_config
                                        .default_lens
                                        .parse()
                                        .unwrap_or(Lens::Eco),
                                    total_input_tokens: 0,
                                    total_output_tokens: 0,
                                    tokens_saved: 0,
                                    last_prompt_report: None,
                                });

                            // Typing indicator.
                            let _ = pipe.send_chat_action(&msg.sender).await;
                            let t_pipe = Arc::clone(&pipe);
                            let t_sender = msg.sender.clone();
                            let t_handle = tokio::spawn(async move {
                                loop {
                                    tokio::time::sleep(std::time::Duration::from_secs(4)).await;
                                    let _ = t_pipe.send_chat_action(&t_sender).await;
                                }
                            });

                            let task_runtime = ChatRuntimeService {
                                engine: agent.engine.as_ref(),
                                refiner: refiner.as_ref(),
                                flow_store: &flow_store,
                                agent_id: &agent.agent_id,
                                agent_config: &agent.agent_config,
                                history_turn_limit: agent.history_turn_limit,
                                compaction_policy: agent.compaction_policy,
                                system_prompt: agent.current_system_prompt.clone(),
                                tools: &agent.current_tools,
                                tool_executor: task_sanitized
                                    .as_ref()
                                    .map(|e| e as &dyn ToolExecutor),
                                memory_service: memory_service_instance.as_ref(),
                                max_recall_entries: memory_config.max_recall_entries,
                                max_recall_tokens: memory_config.max_recall_tokens,
                                tool_observer: Some(&task_observer),
                                cancel: Some(&turn_cancel),
                            };

                            let result = task_runtime.process_user_text(state, &prompt).await;
                            t_handle.abort();

                            match result {
                                Ok(res) => {
                                    let output = res.assistant_text.unwrap_or_default();
                                    if !output.is_empty() {
                                        let reply = secret_registry.redact(&output);
                                        for chunk in chunk_message(&reply, TELEGRAM_MAX_LEN) {
                                            let _ = pipe
                                                .send_text(&msg.sender, chunk, &delivery_opts)
                                                .await;
                                        }
                                    }
                                    // Record outcome path (agent should have written the file,
                                    // but even if not, we track the expected path).
                                    outcome_paths.insert(task.id.clone(), outcome_rel_path);
                                    completed_count += 1;
                                }
                                Err(e) => {
                                    let _ = pipe
                                        .send_text(
                                            &msg.sender,
                                            &format!("[{}] Failed: {}", agent_label, e),
                                            &delivery_opts,
                                        )
                                        .await;
                                    // Write a failure outcome so dependents can see it.
                                    if let Some(ref dir) = outcomes_dir {
                                        let path = dir.join(format!("{}.md", task.id));
                                        let _ = std::fs::write(
                                            &path,
                                            format!("# FAILED\n\nTask: {}\nError: {}\n", task.task, e),
                                        );
                                    }
                                    outcome_paths.insert(task.id.clone(), outcome_rel_path);
                                }
                            }
                        }
                    }

                    let status = if stopped { "stopped" } else { "completed" };
                    let _ = pipe
                        .send_text(
                            &msg.sender,
                            &format!("Team {} — {}/{} tasks done.", status, completed_count, tasks.len()),
                            &delivery_opts,
                        )
                        .await;
                    continue;
                }

                // /project <name> — create a new project subdirectory and switch all agents to it.
                if msg.content.starts_with("/project") {
                    let name = msg.content.trim_start_matches("/project").trim();
                    if name.is_empty() {
                        // Show current project.
                        let current = agent_states
                            .get(&default_agent_id)
                            .and_then(|a| a.workspace.as_ref())
                            .map(|w| w.display().to_string())
                            .unwrap_or_else(|| "(no workspace)".into());
                        let _ = pipe
                            .send_text(
                                &msg.sender,
                                &format!("Current workspace: {}\n\nUsage: /project <name>", current),
                                &delivery_opts,
                            )
                            .await;
                        continue;
                    }

                    // Sanitize name: only allow alphanumeric, hyphens, underscores.
                    let sanitized: String = name
                        .chars()
                        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
                        .collect();

                    // Create project subfolder and apply project scaffold inside it.
                    let mut created = false;

                    if let Some(base) = base_workspaces.values().next() {
                        let project_dir = base.join(&sanitized);

                        // Build a ScaffoldConfig for the project folder.
                        let project_template = config
                            .scaffold
                            .as_ref()
                            .and_then(|s| s.project.as_ref());

                        let project_scaffold = tengu_core::config::ScaffoldConfig {
                            root: project_dir.to_string_lossy().to_string(),
                            directories: project_template
                                .map(|p| p.directories.clone())
                                .unwrap_or_default(),
                            files: project_template
                                .map(|p| p.files.clone())
                                .unwrap_or_default(),
                            project: None,
                        };

                        if let Err(e) = crate::adapters::scaffold::apply_scaffold(&project_scaffold) {
                            let _ = pipe
                                .send_text(
                                    &msg.sender,
                                    &format!("Scaffold failed: {}", e),
                                    &delivery_opts,
                                )
                                .await;
                            continue;
                        }
                    }

                    // Switch all agents to the project subfolder.
                    for (aid, agent) in agent_states.iter_mut() {
                        if let Some(base) = base_workspaces.get(aid) {
                            let project_dir = base.join(&sanitized);
                            agent.workspace = Some(project_dir);

                            // Rebuild skill source for new workspace.
                            agent.skill_source = agent
                                .workspace
                                .as_ref()
                                .map(|ws| FileSystemSkillSource::new(ws.clone()));
                            if let Some(ref src) = agent.skill_source {
                                agent.skill_registry.reload(src);
                            }
                            agent.current_tools = rebuild_tools(&agent.base_tools, &agent.skill_registry);

                            created = true;
                        }
                    }

                    if created {
                        // Reset all conversation states so agents start fresh.
                        let prefix = format!("{}:", sender_id);
                        for (key, state) in user_states.iter_mut() {
                            if key.starts_with(&prefix) {
                                state.reset_for_new_session();
                            }
                        }

                        let ws_display = agent_states
                            .get(&default_agent_id)
                            .and_then(|a| a.workspace.as_ref())
                            .map(|w| w.display().to_string())
                            .unwrap_or_default();

                        let _ = pipe
                            .send_text(
                                &msg.sender,
                                &format!("Project '{}' created.\nWorkspace: {}\nConversations reset.", sanitized, ws_display),
                                &delivery_opts,
                            )
                            .await;
                    } else {
                        let _ = pipe
                            .send_text(&msg.sender, "No workspaces configured.", &delivery_opts)
                            .await;
                    }
                    continue;
                }

                // /agents — list available agents and their roles.
                if msg.content == "/agents" || msg.content.starts_with("/agents@") {
                    let mut lines = vec!["Available agents:".to_string()];
                    for (aid, astate) in &agent_states {
                        let role_label = astate.role.as_deref().unwrap_or("-");
                        let is_default = if *aid == default_agent_id { " [default]" } else { "" };
                        let tool_count = astate.current_tools.len();
                        lines.push(format!(
                            "  {} (role: {}, {} tools){}",
                            aid, role_label, tool_count, is_default
                        ));
                    }
                    lines.push(String::new());
                    lines.push("Direct: @role: message  or  role: message".to_string());
                    lines.push("Team:    /team <goal>     (plan & execute across agents)".to_string());
                    lines.push("Project: /project <name>  (new project subfolder)".to_string());
                    lines.push("Example: @backend_engineer: add rate limiting".to_string());
                    lines.push("Example: /team build a full stack Rust app".to_string());
                    let _ = pipe
                        .send_text(&msg.sender, &lines.join("\n"), &delivery_opts)
                        .await;
                    continue;
                }

                // For other slash commands, route to the user's active agent (or default).
                let active_aid = user_active_agent
                    .get(sender_id)
                    .cloned()
                    .unwrap_or_else(|| default_agent_id.clone());
                let agent = match agent_states.get_mut(&active_aid) {
                    Some(a) => a,
                    None => continue,
                };

                // /reset — clear conversation state for ALL agents (this user).
                if msg.content == "/reset" || msg.content.starts_with("/reset@") {
                    let prefix = format!("{}:", sender_id);
                    let mut reset_count = 0usize;
                    for (key, state) in user_states.iter_mut() {
                        if key.starts_with(&prefix) {
                            state.reset_for_new_session();
                            reset_count += 1;
                        }
                    }
                    let _ = pipe
                        .send_text(
                            &msg.sender,
                            &format!(
                                "Session reset — {} agent conversation(s) cleared.",
                                reset_count
                            ),
                            &delivery_opts,
                        )
                        .await;
                    continue;
                }

                // /purge — clear conversation state + wipe persistent memory.
                if msg.content == "/purge" || msg.content.starts_with("/purge@") {
                    let prefix = format!("{}:", sender_id);
                    for (key, state) in user_states.iter_mut() {
                        if key.starts_with(&prefix) {
                            state.reset_for_new_session();
                        }
                    }
                    let mut lines = vec!["All conversations cleared.".to_string()];
                    if let Some(ref handle) = memory_handle {
                        match handle.store.clear_all().await {
                            Ok(()) => lines.push("Persistent memory purged.".to_string()),
                            Err(e) => lines.push(format!("Memory clear failed: {}", e)),
                        }
                    } else {
                        lines.push("No persistent memory active.".to_string());
                    }
                    let _ = pipe
                        .send_text(&msg.sender, &lines.join("\n"), &delivery_opts)
                        .await;
                    continue;
                }

                // /reload — re-scan skills for active agent.
                if msg.content == "/reload" {
                    let mut lines = Vec::new();
                    if let Some(ref src) = agent.skill_source {
                        if agent.skill_registry.reload(src) {
                            lines.push("Skills reloaded (changes detected).".to_string());
                        } else {
                            lines.push("Skills reloaded (no changes).".to_string());
                        }
                    } else {
                        lines.push("No workspace — skills unavailable.".to_string());
                    }
                    agent.current_tools = rebuild_tools(&agent.base_tools, &agent.skill_registry);
                    agent.current_system_prompt = rebuild_system_prompt(
                        &agent.agent_config,
                        agent.advertise_workspace_tools,
                        &agent.skill_registry,
                    );
                    lines.push(format!("{} tool(s) active.", agent.current_tools.len()));
                    let _ = pipe
                        .send_text(&msg.sender, &lines.join("\n"), &delivery_opts)
                        .await;
                    continue;
                }

                let state_key = format!("{}:{}", sender_id, active_aid);
                let state = user_states.entry(state_key).or_insert_with(|| {
                    ChatLoopState {
                        messages: Vec::new(),
                        active_flow_key: None,
                        manual_session_id: None,
                        flow_token_usage: 0,
                        active_lens: agent.agent_config
                            .default_lens
                            .parse()
                            .unwrap_or(Lens::Eco),
                        total_input_tokens: 0,
                        total_output_tokens: 0,
                        tokens_saved: 0,
                        last_prompt_report: None,
                    }
                });

                let cmd_result = chat_commands::handle_chat_command(
                    &msg.content,
                    state,
                    &agent.engine_info,
                    &agent.agent_config,
                    agent.history_turn_limit,
                    agent.compaction_policy,
                );
                match cmd_result {
                    CommandResult::Handled(output) => {
                        let reply = output.lines.join("\n");
                        let _ = pipe
                            .send_text(&msg.sender, &reply, &delivery_opts)
                            .await;
                        continue;
                    }
                    CommandResult::NotHandled => {
                        let _ = pipe
                            .send_text(
                                &msg.sender,
                                "Unknown command. Type /help or /agents for available commands.",
                                &delivery_opts,
                            )
                            .await;
                        continue;
                    }
                }
            }

            // ---------------------------------------------------------------
            // Route message to an agent.
            // ---------------------------------------------------------------
            let (routed_role, user_text) =
                parse_agent_routing(&msg.content, Some(&role_to_agent));

            let target_agent_id = if let Some(ref role_key) = routed_role {
                match role_to_agent.get(role_key) {
                    Some(aid) => aid.clone(),
                    None => {
                        let available: Vec<&str> = agent_states.values()
                            .filter_map(|a| a.role.as_deref())
                            .collect();
                        let _ = pipe
                            .send_text(
                                &msg.sender,
                                &format!(
                                    "Unknown agent role: {}\nAvailable: {}",
                                    role_key,
                                    available.join(", ")
                                ),
                                &delivery_opts,
                            )
                            .await;
                        continue;
                    }
                }
            } else if is_multi_agent {
                // Try keyword-based auto-routing before falling back to sticky/default.
                infer_agent_from_keywords(&user_text, &role_to_agent)
                    .unwrap_or_else(|| {
                        user_active_agent
                            .get(sender_id)
                            .cloned()
                            .unwrap_or_else(|| default_agent_id.clone())
                    })
            } else {
                default_agent_id.clone()
            };

            // Update user's active agent.
            user_active_agent.insert(sender_id.clone(), target_agent_id.clone());

            let agent = match agent_states.get_mut(&target_agent_id) {
                Some(a) => a,
                None => continue,
            };

            // Hot-reload skills for this agent.
            if let Some(ref src) = agent.skill_source {
                if agent.skill_registry.reload(src) {
                    agent.current_tools = rebuild_tools(&agent.base_tools, &agent.skill_registry);
                    agent.current_system_prompt = rebuild_system_prompt(
                        &agent.agent_config,
                        agent.advertise_workspace_tools,
                        &agent.skill_registry,
                    );
                }
            }

            // Save attached files to workspace and build augmented content.
            let user_content = if let (Some(ref ws), Some(ref media)) =
                (&agent.workspace, &msg.media)
            {
                let attachments_dir = ws.join(".tengu-attachments");
                std::fs::create_dir_all(&attachments_dir).ok();

                let mut file_notes = Vec::new();
                for m in media {
                    let fname = m
                        .filename
                        .as_deref()
                        .unwrap_or("attachment");
                    let path = attachments_dir.join(fname);
                    match std::fs::write(&path, &m.data) {
                        Ok(()) => {
                            info!(path = %path.display(), size = m.data.len(), "Saved Telegram attachment");
                            file_notes.push(format!(
                                "[Attached file: {} ({}, {} bytes)]",
                                path.display(),
                                m.mime_type,
                                m.data.len()
                            ));
                        }
                        Err(e) => {
                            warn!(error = %e, "Failed to save Telegram attachment");
                        }
                    }
                }

                if file_notes.is_empty() {
                    user_text.clone()
                } else {
                    format!("{}\n{}", file_notes.join("\n"), user_text)
                }
            } else {
                user_text.clone()
            };

            // Set the current recipient so inline approval messages go to the right chat.
            *current_recipient.lock().unwrap() = Some(msg.sender.clone());

            // Build executor for this agent.
            let current_executor: Option<TelegramToolExecutor> =
                agent.workspace.as_ref().and_then(|ws| {
                    rebuild_executor(
                        ws,
                        &agent.current_tools,
                        &agent.skill_registry,
                        &memory_handle,
                        &secret_registry,
                        Arc::clone(&approval_adapter),
                    )
                });

            // Wrap tool executor with secret redaction.
            let sanitized_executor = current_executor.as_ref().map(|e| {
                SanitizedToolExecutor::new(e as &dyn ToolExecutor, &secret_registry)
            });

            // Track tool calls for the cross-agent activity log.
            let turn_tool_log: Arc<std::sync::Mutex<Vec<String>>> =
                Arc::new(std::sync::Mutex::new(Vec::new()));

            // Debug tool observer — sends tool results to Telegram + records for activity log.
            let observer_pipe = Arc::clone(&pipe);
            let observer_sender = msg.sender.clone();
            let observer_secrets = Arc::clone(&secret_registry);
            let observer_agent_label = agent
                .agent_config
                .identity
                .name
                .clone()
                .unwrap_or_else(|| agent.agent_id.clone());
            let turn_tool_log_ref = Arc::clone(&turn_tool_log);
            let tool_result_observer = move |call: &ToolCall, result: &str| {
                // Record mutation-type tool calls for activity log.
                if let Some(entry) = format_tool_for_activity(call) {
                    turn_tool_log_ref.lock().unwrap().push(entry);
                }

                let redacted = observer_secrets.redact(result);
                let mut end = redacted.len().min(1500);
                while end < redacted.len() && !redacted.is_char_boundary(end) {
                    end -= 1;
                }
                let truncated = if redacted.len() > 1500 {
                    format!("{}…", &redacted[..end])
                } else {
                    redacted
                };
                let text = format!(
                    "[{}] 🔧 `{}` →\n```\n{}\n```",
                    observer_agent_label, call.name, truncated
                );
                let p = Arc::clone(&observer_pipe);
                let r = observer_sender.clone();
                tokio::task::block_in_place(move || {
                    let handle = tokio::runtime::Handle::current();
                    handle.block_on(async {
                        let _ = p.send_text(&r, &text, &DeliveryOptions::default()).await;
                    });
                });
            };

            // Reset the cancel flag before each turn.
            turn_cancel.store(false, std::sync::atomic::Ordering::Relaxed);

            // Get or create per-user-per-agent state.
            let state_key = format!("{}:{}", sender_id, target_agent_id);
            let state = user_states.entry(state_key).or_insert_with(|| {
                ChatLoopState {
                    messages: Vec::new(),
                    active_flow_key: None,
                    manual_session_id: None,
                    flow_token_usage: 0,
                    active_lens: agent.agent_config
                        .default_lens
                        .parse()
                        .unwrap_or(Lens::Eco),
                    total_input_tokens: 0,
                    total_output_tokens: 0,
                    tokens_saved: 0,
                    last_prompt_report: None,
                }
            });

            // Show which agent is responding (always in multi-agent setups).
            if is_multi_agent {
                let agent_label = agent.agent_config.identity.name.as_deref()
                    .unwrap_or(&agent.agent_id);
                let _ = pipe
                    .send_text(
                        &msg.sender,
                        &format!("[{}]", agent_label),
                        &delivery_opts,
                    )
                    .await;
            }

            // Inject cross-agent activity so this agent knows what others have done.
            let mut turn_system_prompt = agent.current_system_prompt.clone();
            if is_multi_agent {
                let activity_ctx =
                    build_activity_context(&activity_log, &target_agent_id);
                if !activity_ctx.is_empty() {
                    turn_system_prompt.push_str(&activity_ctx);
                }
            }

            let chat_runtime = ChatRuntimeService {
                engine: agent.engine.as_ref(),
                refiner: refiner.as_ref(),
                flow_store: &flow_store,
                agent_id: &agent.agent_id,
                agent_config: &agent.agent_config,
                history_turn_limit: agent.history_turn_limit,
                compaction_policy: agent.compaction_policy,
                system_prompt: turn_system_prompt,
                tools: &agent.current_tools,
                tool_executor: sanitized_executor
                    .as_ref()
                    .map(|e| e as &dyn ToolExecutor),
                memory_service: memory_service_instance.as_ref(),
                max_recall_entries: memory_config.max_recall_entries,
                max_recall_tokens: memory_config.max_recall_tokens,
                tool_observer: Some(&tool_result_observer),
                cancel: Some(&turn_cancel),
            };

            // Spawn typing indicator.
            let _ = pipe.send_chat_action(&msg.sender).await;
            let typing_pipe = Arc::clone(&pipe);
            let typing_sender = msg.sender.clone();
            let typing_handle = tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(4)).await;
                    let _ = typing_pipe.send_chat_action(&typing_sender).await;
                }
            });

            let result = chat_runtime.process_user_text(state, &user_content).await;
            typing_handle.abort();

            match result {
                Ok(result) => {
                    if let Some(notice) = result.system_notice {
                        let _ = pipe.send_text(&msg.sender, &notice, &delivery_opts).await;
                    }
                    if let Some(ref text) = result.assistant_text {
                        let reply = secret_registry.redact(text);
                        for chunk in chunk_message(&reply, TELEGRAM_MAX_LEN) {
                            if let Err(e) =
                                pipe.send_text(&msg.sender, chunk, &delivery_opts).await
                            {
                                error!(error = %e, "Failed to send Telegram reply chunk");
                            }
                        }
                    }

                    // Record activity for cross-agent context.
                    if is_multi_agent {
                        let tools_used = match Arc::try_unwrap(turn_tool_log) {
                            Ok(mutex) => mutex.into_inner().unwrap_or_default(),
                            Err(arc) => arc.lock().unwrap().clone(),
                        };
                        let response_summary = result
                            .assistant_text
                            .as_deref()
                            .map(|t| truncate_summary(t, MAX_ACTIVITY_SUMMARY_CHARS))
                            .unwrap_or_default();
                        let label = agent
                            .agent_config
                            .identity
                            .name
                            .clone()
                            .unwrap_or_else(|| agent.agent_id.clone());
                        if !tools_used.is_empty() || !response_summary.is_empty() {
                            activity_log.push(ActivityEntry {
                                agent_label: label,
                                agent_id: target_agent_id.clone(),
                                tools_used,
                                response_summary,
                            });
                            if activity_log.len() > MAX_ACTIVITY_ENTRIES {
                                activity_log
                                    .drain(..activity_log.len() - MAX_ACTIVITY_ENTRIES);
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

    // All agent state drops here in SYNC context — safe for nested runtimes.
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

    #[test]
    fn parse_agent_routing_with_role() {
        let (role, msg) = parse_agent_routing("@backend_engineer: add rate limiting", None);
        assert_eq!(role.as_deref(), Some("backend_engineer"));
        assert_eq!(msg, "add rate limiting");
    }

    #[test]
    fn parse_agent_routing_hyphenated() {
        let (role, msg) = parse_agent_routing("@cms-guide: review content models", None);
        assert_eq!(role.as_deref(), Some("cms_guide"));
        assert_eq!(msg, "review content models");
    }

    #[test]
    fn parse_agent_routing_no_prefix() {
        let (role, msg) = parse_agent_routing("just a normal message", None);
        assert!(role.is_none());
        assert_eq!(msg, "just a normal message");
    }

    #[test]
    fn parse_agent_routing_empty_message() {
        let (role, _) = parse_agent_routing("@qa:", None);
        assert!(role.is_none());
    }

    #[test]
    fn parse_agent_routing_empty_role() {
        let (role, _) = parse_agent_routing("@: something", None);
        assert!(role.is_none());
    }

    #[test]
    fn parse_agent_routing_without_at_known_role() {
        let mut roles = HashMap::new();
        roles.insert("backend".into(), "backend".into());
        roles.insert("frontend".into(), "frontend".into());
        let (role, msg) = parse_agent_routing("backend: add API endpoint", Some(&roles));
        assert_eq!(role.as_deref(), Some("backend"));
        assert_eq!(msg, "add API endpoint");
    }

    #[test]
    fn parse_agent_routing_without_at_unknown_role_ignored() {
        let mut roles = HashMap::new();
        roles.insert("backend".into(), "backend".into());
        // "hello" is not a known role, so colon is treated as regular text.
        let (role, msg) = parse_agent_routing("hello: world", Some(&roles));
        assert!(role.is_none());
        assert_eq!(msg, "hello: world");
    }

    #[test]
    fn infer_agent_from_keywords_single_match() {
        let mut roles = HashMap::new();
        roles.insert("backend".into(), "backend".into());
        roles.insert("frontend".into(), "frontend".into());
        assert_eq!(
            infer_agent_from_keywords("add backend feature", &roles),
            Some("backend".into())
        );
    }

    #[test]
    fn infer_agent_from_keywords_no_match() {
        let mut roles = HashMap::new();
        roles.insert("backend".into(), "backend".into());
        assert_eq!(
            infer_agent_from_keywords("fix the button style", &roles),
            None
        );
    }

    #[test]
    fn infer_agent_from_keywords_ambiguous() {
        let mut roles = HashMap::new();
        roles.insert("backend".into(), "backend".into());
        roles.insert("frontend".into(), "frontend".into());
        // Both agents mentioned — ambiguous, returns None.
        assert_eq!(
            infer_agent_from_keywords("connect frontend to backend", &roles),
            None
        );
    }

    #[test]
    fn infer_agent_from_keywords_role_key_match() {
        let mut roles = HashMap::new();
        roles.insert("backend_engineer".into(), "backend".into());
        roles.insert("backend".into(), "backend".into());
        // "backend_engineer" is not a single word in the message, but "backend" is.
        assert_eq!(
            infer_agent_from_keywords("add backend", &roles),
            Some("backend".into())
        );
    }

    #[test]
    fn activity_context_empty_when_no_other_agents() {
        let log = vec![ActivityEntry {
            agent_label: "Frontend".into(),
            agent_id: "frontend".into(),
            tools_used: vec!["wrote `src/app.js`".into()],
            response_summary: "Created app component.".into(),
        }];
        // Same agent — should produce empty context.
        assert!(build_activity_context(&log, "frontend").is_empty());
    }

    #[test]
    fn activity_context_includes_other_agent_work() {
        let log = vec![
            ActivityEntry {
                agent_label: "Frontend Engineer".into(),
                agent_id: "frontend".into(),
                tools_used: vec!["wrote `src/app.js`".into()],
                response_summary: "Created the main app component.".into(),
            },
            ActivityEntry {
                agent_label: "Backend Engineer".into(),
                agent_id: "backend".into(),
                tools_used: vec!["wrote `src/api/server.js`".into()],
                response_summary: "Set up Express server.".into(),
            },
        ];
        let ctx = build_activity_context(&log, "marketing");
        assert!(ctx.contains("Frontend Engineer"));
        assert!(ctx.contains("wrote `src/app.js`"));
        assert!(ctx.contains("Backend Engineer"));
        assert!(ctx.contains("wrote `src/api/server.js`"));
        assert!(ctx.contains("read_file"));
    }

    #[test]
    fn format_tool_for_activity_write_file() {
        let call = ToolCall {
            id: "1".into(),
            name: "write_file".into(),
            arguments: serde_json::json!({"path": "src/main.js", "content": "..."}),
        };
        assert_eq!(
            format_tool_for_activity(&call),
            Some("wrote `src/main.js`".into())
        );
    }

    #[test]
    fn format_tool_for_activity_read_file_skipped() {
        let call = ToolCall {
            id: "1".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({"path": "src/main.js"}),
        };
        assert!(format_tool_for_activity(&call).is_none());
    }
}
