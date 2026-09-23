//! Tool wiring — builds the `PluginToolExecutor` an agent runs with: the tool
//! catalog (`adapters/outbound/tools`), shell skills, `[[mcp_servers]]`, and
//! the per-tool scope map. Also the `run-agent` subagent variant and the
//! Claude Code bridge tool list.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use crate::config::{AgentConfig, Config, McpServerConfig};
// Plugin set: `outbound::tools::catalog`. McpPlugin + SkillPlugin are
// registered here, outside the catalog — they need the config's
// `[[mcp_servers]]` / a skill registry.
use crate::adapters::outbound::mcp_client::McpPlugin;
use crate::adapters::outbound::shell::LocalShellExecutor;
use crate::adapters::outbound::tools::skill::SkillPlugin;
use crate::application::skills::registry::SkillRegistry;
use crate::application::tools::registry::{PluginToolExecutor, ToolRegistry};
use crate::domain::message::ToolDef;
use crate::domain::scope::ToolScope;
use crate::domain::secrets::SecretRegistry;
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

/// Bridge tool list for an in-process Claude Code agent: `base` (the catalog
/// defs) plus every `[[mcp_servers]]` tool as `{server}__{tool}`, which the
/// bridge then proxies (the engine passes the servers along via
/// `EngineContext.mcp_servers`). Listed once at agent setup — live
/// `tools/list`, fail-soft per server.
pub(crate) async fn with_mcp_bridge_tools(
    mut base: Vec<ToolDef>,
    mcp_servers: &[McpServerConfig],
) -> Vec<ToolDef> {
    if !base.is_empty() {
        base.extend(crate::adapters::outbound::mcp_client::enumerate_tools(mcp_servers).await);
    }
    base
}

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
// ---------------------------------------------------------------------------
// Golden registry test — asserts the LLM-facing tool surface is unchanged.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod golden_tests {
    use super::*;
    use crate::application::chat::service::{create_chat_loop_state, ChatRuntimeService};
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

    // In-process Claude Code agents (TUI / Telegram): the bridge tool list
    // gains the `[[mcp_servers]]` tools, and `ChatRuntimeService` hands the
    // servers to the engine so its bridge can proxy them. (Engine → bridge
    // env: `claude_code` tests; bridge → server: `tests/mcp_bridge_external.rs`.)
    #[tokio::test]
    async fn in_process_claude_code_agent_gets_mcp_tools_and_servers() {
        use crate::domain::message::{Message, ModelInfo, StreamEvent};
        use crate::ports::engine::{Engine, EngineContext};
        use std::sync::Mutex;

        let servers = vec![crate::adapters::outbound::mcp_client::tests::fake_server(
            "fake",
        )];
        let base = vec![ToolDef::new("read_file", "d", serde_json::json!({}))];
        let bridge = with_mcp_bridge_tools(base, &servers).await;
        let names: Vec<&str> = bridge.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["read_file", "fake__echo"]);
        // No bridge (non-Claude-Code agent) stays empty — servers not dialled.
        assert!(with_mcp_bridge_tools(Vec::new(), &servers).await.is_empty());

        #[derive(Default)]
        struct Recording(Mutex<Option<(Vec<String>, Vec<String>)>>);
        #[async_trait::async_trait]
        impl Engine for Recording {
            fn id(&self) -> &str {
                "recording"
            }
            fn context_window(&self) -> usize {
                100_000
            }
            fn supports_tool_use(&self) -> bool {
                false
            }
            fn manages_own_workspace(&self) -> bool {
                true
            }
            fn available_models(&self) -> Vec<ModelInfo> {
                Vec::new()
            }
            async fn run(
                &self,
                _messages: &[Message],
                _tools: &[ToolDef],
                context: &EngineContext,
            ) -> anyhow::Result<std::pin::Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send>>>
            {
                *self.0.lock().unwrap() = Some((
                    context.mcp_servers.iter().map(|s| s.name.clone()).collect(),
                    context
                        .bridge_tools
                        .iter()
                        .flatten()
                        .map(|t| t.name.clone())
                        .collect(),
                ));
                Ok(Box::pin(futures::stream::iter(vec![
                    StreamEvent::TextDelta { text: "ok".into() },
                    StreamEvent::Done,
                ])))
            }
        }

        let engine = Recording::default();
        let agent = Config::default().agents.get("main").unwrap().clone();
        let service = ChatRuntimeService {
            engine: &engine,
            agent_id: "main",
            agent_config: &agent,
            history_turn_limit: 10,
            compaction_policy: crate::application::chat::flow::resolve_flow_compaction_policy(
                &agent.flow,
                agent.limits.max_tokens_per_flow,
                100_000,
                4_096,
            ),
            system_prompt: "sys".into(),
            tools: &[],
            tool_executor: None,
            memory_manager: None,
            max_recall_entries: 0,
            max_recall_tokens: 0,
            tool_observer: None,
            cancel: None,
            bridge_tools: Some(&bridge),
            mcp_servers: &servers,
            suppress_grounding_nudge: true,
        };
        let mut state = create_chat_loop_state(&agent);
        service.process_user_text(&mut state, "hi").await.unwrap();
        let (seen_servers, seen_bridge) = engine.0.lock().unwrap().clone().unwrap();
        assert_eq!(seen_servers, ["fake"]);
        assert!(seen_bridge.contains(&"fake__echo".to_string()));
    }
}
