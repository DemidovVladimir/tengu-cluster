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
//! - **Agent routing** — `parse_agent_routing`, `infer_agent_from_keywords`
//! - **Message chunking** — `chunk_message` (for channels with length limits)
//! - **State factories** — `create_chat_loop_state`
//! - **Skill list formatting** — `format_skill_list`

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;

use crate::adapters::composite_tool_executor::CompositeToolExecutionAdapter;
use crate::adapters::embedding::OpenRouterEmbeddingAdapter;
use crate::adapters::memory_store::DiskVectorMemoryStore;
use crate::adapters::memory_tool_executor::{
    memory_tool_defs, MemoryServiceHandle, MemoryToolExecutionAdapter,
};
use crate::adapters::shell_executor::LocalShellExecutor;
use crate::adapters::skill_tool_executor::SkillToolExecutionAdapter;
use crate::adapters::system_prompt;
use crate::adapters::workspace_tools;
use crate::application::engine_runtime::ToolExecutor;
use crate::application::ports::{ShellExecutionPort, ToolActivityPort, ToolApprovalPort};
use crate::application::skill_registry::SkillRegistry;
use crate::application::tool_use_service::ToolUseService;
use crate::application::workspace_tools_catalog::{build_workspace_tools, filter_tools_by_allowlist};
use crate::domain::chat::ChatLoopState;
use crate::domain::secret_registry::SecretRegistry;
use crate::domain::skill::SkillStatus;
use crate::domain::tool_policy::ToolPolicyCatalog;
use tengu_core::config::AgentConfig;
use tengu_core::types::{ToolCall, ToolDef};
use tengu_core::Lens;

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

/// Merge base workspace tools with active skill tools.
pub(crate) fn rebuild_tools(base_tools: &[ToolDef], skill_registry: &SkillRegistry) -> Vec<ToolDef> {
    let mut tools = base_tools.to_vec();
    tools.extend(skill_registry.active_tool_defs());
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
    system_prompt::build_system_prompt_with_tools(
        agent_config,
        advertise_workspace_tools,
        &skill_context_strings,
        tools,
    )
}

/// Build the composite tool executor from workspace, skill, and memory executors.
///
/// The `approval` and `activity` ports are channel-specific — each channel adapter
/// provides its own implementations (e.g., TUI dialogs, Telegram inline keyboards,
/// Slack interactive messages).
pub(crate) fn build_tool_executor(
    workspace: &Path,
    tools: &[ToolDef],
    skill_registry: &SkillRegistry,
    memory_handle: &Option<Arc<MemoryServiceHandle>>,
    secret_registry: &Arc<SecretRegistry>,
    approval: Arc<dyn ToolApprovalPort>,
    activity: Arc<dyn ToolActivityPort>,
) -> Option<ToolServiceExecutor> {
    if tools.is_empty() {
        return None;
    }

    let shell: Arc<dyn ShellExecutionPort> = Arc::new(LocalShellExecutor);

    let workspace_exec = Arc::new(
        workspace_tools::WorkspaceToolExecutionAdapter::new(workspace.to_path_buf())
            .with_shell(Arc::clone(&shell)),
    );

    let skill_defs = skill_registry.active_skill_definitions();
    let skill_names: HashSet<String> = skill_defs.iter().map(|s| s.name.clone()).collect();

    let mut composite = CompositeToolExecutionAdapter::new(workspace_exec);

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
            let mem_names: HashSet<String> = memory_tool_defs()
                .iter()
                .map(|t| t.name.clone())
                .collect();
            composite = composite.with_executor(Arc::new(mem_exec), mem_names);
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
pub(crate) fn compute_base_tools(
    uses_tools: bool,
    has_memory: bool,
    allowed_tools: Option<&[String]>,
) -> Vec<ToolDef> {
    if !uses_tools {
        return vec![];
    }
    let mut tools = filter_tools_by_allowlist(build_workspace_tools(), allowed_tools);
    if has_memory {
        tools.extend(memory_tool_defs());
    }
    tools
}

// ---------------------------------------------------------------------------
// Memory subsystem initialization
// ---------------------------------------------------------------------------

/// Build the shared memory subsystem handle from config.
///
/// Returns `None` if memory is disabled, the API key is not set, or store init fails.
/// The tokio runtime is needed for async Qdrant initialization.
pub(crate) fn build_memory_handle(
    memory_config: &tengu_core::config::MemoryConfig,
    #[allow(unused_variables)] rt: &tokio::runtime::Runtime,
) -> Option<Arc<MemoryServiceHandle>> {
    if !memory_config.enabled {
        return None;
    }

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
                                tracing::warn!(error = %e, "Failed to init Qdrant memory store, memory disabled");
                                None
                            }
                        }
                    }
                    #[cfg(not(feature = "qdrant"))]
                    "qdrant" => {
                        tracing::warn!("Qdrant backend requested but 'qdrant' feature not enabled, falling back to disk");
                        let store_path_str = memory_config.store_path.replace(
                            "~",
                            &dirs_next::home_dir().unwrap_or_default().to_string_lossy(),
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
                            &dirs_next::home_dir().unwrap_or_default().to_string_lossy(),
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
        active_lens: agent_config
            .default_lens
            .parse()
            .unwrap_or(Lens::Eco),
        total_input_tokens: 0,
        total_output_tokens: 0,
        tokens_saved: 0,
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

/// Infer the target agent by matching whole words against known role keys / agent IDs.
///
/// Returns `Some(agent_id)` when exactly one agent is matched. Returns `None` when
/// zero or multiple different agents match (ambiguous).
pub(crate) fn infer_agent_from_keywords(
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
        assert_eq!(
            infer_agent_from_keywords("add backend", &roles),
            Some("backend".into())
        );
    }
}
