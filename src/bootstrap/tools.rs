//! Tool wiring — builds the `PluginToolExecutor` an agent runs with: the tool
//! catalog (`adapters/outbound/tools`), shell skills, `[[mcp_servers]]`, and
//! the per-tool scope map. Also the `run-agent` subagent variant, the
//! Claude Code bridge tool list and a step / one-shot turn's workspace
//! (`workspace_or_temp`).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
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
    // Defense in depth for `[generation]`: `Config::load` already refuses a
    // config listing a tool outside the bound generation.
    let tools = &within_generation(agent_config, tools);
    if tools.is_empty() {
        return None;
    }
    // The workspace-tool opt-ins the agent lists in `tools` are on for its
    // plugins too (`persistent_store` reads `workspace_tools`), on every
    // surface — chat passes the config block as loaded.
    let agent_config = &subagent_config(agent_config);

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
    // its tools as `{server_name}__{tool_name}`: all of them when the agent's
    // `tools` is empty, else only the listed ones — the allow-list the Claude
    // Code bridge applies to the servers its engine passes
    // (`TENGU_BRIDGE_MCP_SERVERS`), so an unlisted server tool runs nowhere.
    if !mcp_servers.is_empty() {
        let mcp_plugin = McpPlugin::new(mcp_servers.to_vec());
        register_plugin_safe(
            &mut registry,
            &mcp_plugin,
            &plugin_ctx,
            &agent_config.tools,
            "Failed to register mcp plugin — external MCP tools unavailable",
        );
    }

    // Per-tool scope map — ENFORCED. `agent_config.scopes` already has the
    // parent's `[default_scopes]` folded in (`Config::fold_default_scopes`
    // at `Config::load`; `run-agent` children load the same config and go
    // through `subagent_config`). A tool with no configured entry falls
    // back to `permissive_scope`.
    let scopes = resolve_tool_scopes(
        workspace,
        &agent_config.scopes,
        registry.tool_names(),
        agent_config.no_shell_fallback,
    );

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

/// `tools` minus the ones the agent's bound generation refuses
/// (`[generation]`, `GenerationScope::tool_refusal`: a tool another
/// generation's capability binds, an opt-in tool no capability binds), each
/// dropped with a warning; all of them when the sandbox binds none.
pub(crate) fn within_generation(agent: &AgentConfig, tools: &[ToolDef]) -> Vec<ToolDef> {
    let Some(scope) = &agent.sandbox.generation else {
        return tools.to_vec();
    };
    tools
        .iter()
        .filter(|t| match scope.tool_refusal(&t.name) {
            None => true,
            Some(why) => {
                tracing::warn!(tool = %t.name, generation = %scope.id, "{why} — not registered");
                false
            }
        })
        .cloned()
        .collect()
}

/// Resolve the per-tool scope map for an executor: a configured entry in
/// `configured` (per-agent `[agents.*.scopes.<tool>]`, with `[default_scopes]`
/// already folded in) wins; any tool without one gets `permissive_scope` —
/// minus the shell when `no_shell` (a `[solana]` signing sandbox: a shell
/// could read the key file, `config/solana.rs`).
/// Shared by `build_tool_executor` and `mcp_bridge::build_bridge_executor`.
pub(crate) fn resolve_tool_scopes(
    workspace: &Path,
    configured: &HashMap<String, ToolScope>,
    tool_names: impl IntoIterator<Item = String>,
    no_shell: bool,
) -> HashMap<String, ToolScope> {
    let mut fallback = permissive_scope(workspace);
    if no_shell {
        fallback.shell_bins.clear();
    }
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
/// via `permissive_scope`, so this only equalises the configured ones. An
/// explicit deny (every field empty, `ToolScope::is_deny_all` — e.g.
/// `[default_scopes.write_file]` with no keys) stays a deny: a plan step's
/// `compose.tools` must not turn it into a workspace grant.
pub(crate) fn grant_workspace_root(scopes: &mut HashMap<String, ToolScope>, workspace: &Path) {
    for scope in scopes.values_mut() {
        if !scope.is_deny_all() && !scope.fs_roots.iter().any(|r| r == workspace) {
            scope.fs_roots.push(workspace.to_path_buf());
        }
    }
}

/// The workspace of one `run-agent` step or one-shot turn (webhooks, `tengu
/// tool turn` / `call`) — the executor's and the engine's (a Claude Code
/// CLI's cwd, its bridge). Callers keep the memory store at `[memory]
/// store_path` when it is a temp dir.
///
/// | `[agents.<a>] workspace` | Workspace |
/// |---|---|
/// | set | it, `~` expanded; a relative path made absolute against this process's cwd — one path for the executor, the engine and the bridge |
/// | unset | a fresh `<prefix>*` temp dir (canonical), returned too: removed when it drops at the end of the step / turn |
///
/// Never the bare cwd: a Claude Code CLI runs there with its built-ins, and
/// the permissive fallback scope (`permissive_scope`) roots every tool in it.
pub(crate) fn workspace_or_temp(
    configured: Option<&Path>,
    prefix: &str,
) -> anyhow::Result<(PathBuf, Option<tempfile::TempDir>)> {
    use anyhow::Context;
    if let Some(path) = configured {
        let path = crate::config::paths::expand_tilde(path);
        let path = if path.is_relative() {
            crate::config::paths::absolute_path(&path)
        } else {
            path
        };
        return Ok((path, None));
    }
    let dir = tempfile::Builder::new()
        .prefix(prefix)
        .tempdir()
        .context("create a temp workspace")?;
    // Canonical: scope checks compare resolved paths (macOS /var → /private/var).
    let path = std::fs::canonicalize(dir.path()).context("temp workspace")?;
    Ok((path, Some(dir)))
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
/// defs) plus the `[[mcp_servers]]` tools as `{server}__{tool}` — every one
/// when `allow` (the agent's `tools`) is empty, else only the listed ones —
/// which the bridge then proxies (the engine passes the servers along via
/// `EngineContext.mcp_servers`). Listed once at agent setup — live
/// `tools/list`, fail-soft per server.
pub(crate) async fn with_mcp_bridge_tools(
    mut base: Vec<ToolDef>,
    allow: &[String],
    mcp_servers: &[McpServerConfig],
) -> Vec<ToolDef> {
    if !base.is_empty() {
        base.extend(
            crate::adapters::outbound::mcp_client::enumerate_tools(mcp_servers)
                .await
                .into_iter()
                .filter(|d| allow.is_empty() || allow.contains(&d.name)),
        );
    }
    base
}

/// What a Claude Code engine (one that manages its own workspace) needs to
/// reach tengu tools on a per-turn surface: the tool list for its MCP bridge
/// — catalog, skills and `[[mcp_servers]]` tools, i.e. exactly what the
/// executor advertises — plus the servers behind `{server}__{tool}` entries.
/// Other engines get tools through the model API and need neither. An empty
/// list (an orchestrator agent) means no bridge. Webhook turns and
/// `tengu eval` / `tengu skill evolve` rows build their engine context with it.
pub(crate) fn bridge_inputs(
    manages_own_workspace: bool,
    tool_defs: &[ToolDef],
    mcp_servers: &[McpServerConfig],
) -> (Option<Vec<ToolDef>>, Vec<McpServerConfig>) {
    if !manages_own_workspace || tool_defs.is_empty() {
        return (None, Vec::new());
    }
    (Some(tool_defs.to_vec()), mcp_servers.to_vec())
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

/// The catalog tools an agent gets on every surface — in-process chat (TUI,
/// Telegram, webhooks, eval, `tengu tool turn`), `run-agent`, `tengu tool
/// call`, decision loops, and so the Claude Code bridge list: the base tools
/// (`compute_base_tools`) with the workspace-tool opt-ins the agent lists in
/// `tools` switched on (`subagent_config`); a non-empty `tools` then keeps
/// only the listed tools and the opted-in ones. Empty `tools` = every base
/// tool. `[[mcp_servers]]` tools follow the same list
/// (`build_tool_executor`); shell skills come from `skill_packages`.
pub(crate) fn agent_base_tools(
    agent: &AgentConfig,
    uses_tools: bool,
    has_memory: bool,
) -> Vec<ToolDef> {
    let cfg = subagent_config(agent);
    let base = compute_base_tools(uses_tools, has_memory, &cfg.workspace_tools);
    let listed: Vec<ToolDef> = if agent.tools.is_empty() {
        base
    } else {
        base.into_iter()
            .filter(|t| agent.tools.contains(&t.name) || cfg.workspace_tools.contains(&t.name))
            .collect()
    };
    within_generation(agent, &listed)
}

/// A plan step's `compose` applied to its base `[agents.<compose.base_agent>]`
/// block: `skills` and `tools` replace the base's wholesale (doctrine #3).
/// In a hardened sandbox (`hardened`: a `[solana]` signer or `[risk]`,
/// `config::hardening`) a compose may only narrow: a catalog tool the
/// composed agent would get that the base does not (an empty `tools` = every
/// catalog tool), or a skill the base does not list, refuses the step — a
/// planner fed hostile text must not hand a routable agent `write_file` or
/// an exec tool. (A hardened sandbox has no `[[mcp_servers]]` and loads no
/// shell skill, so the catalog is every tool there is.)
pub(crate) fn compose_agent(
    base_name: &str,
    base: &AgentConfig,
    compose: &crate::domain::plan::AgentCompose,
    hardened: bool,
    has_memory: bool,
) -> anyhow::Result<AgentConfig> {
    let mut agent = base.clone();
    agent.skill_packages = compose.skills.clone();
    agent.tools = compose.tools.clone();
    if !hardened {
        return Ok(agent);
    }
    let names = |a: &AgentConfig| -> HashSet<String> {
        agent_base_tools(a, true, has_memory)
            .into_iter()
            .map(|t| t.name)
            .collect()
    };
    let held = names(base);
    let mut tools: Vec<String> = names(&agent)
        .into_iter()
        .filter(|t| !held.contains(t))
        .collect();
    tools.sort();
    let mut skills: Vec<&str> = compose
        .skills
        .iter()
        .filter(|s| !base.skill_packages.contains(s))
        .map(String::as_str)
        .collect();
    skills.sort();
    if tools.is_empty() && skills.is_empty() {
        return Ok(agent);
    }
    anyhow::bail!(
        "compose would widen agent `{base_name}` in a hardened sandbox (Solana signer or \
         [risk]): a composed plan step may only narrow its base agent's tools and skills — \
         adds tools {tools:?}, skills {skills:?}"
    )
}

/// The skills an agent loads where no channel keeps a hot-reloaded registry
/// (`run-agent`, `tengu tool`, decision loops, the MCP bridge) — the rule
/// in-process chat applies: the three-tier scan from `workspace`
/// (`FileSystemSkillSource`), only names in `skill_packages`, none named like
/// one of its catalog tools, and shell skills only when the agent runs a
/// shell (`no_shell_fallback` — a `[risk]` / signer sandbox — loads none).
pub(crate) fn agent_skill_registry(
    workspace: &Path,
    agent: &AgentConfig,
    has_memory: bool,
) -> SkillRegistry {
    let reserved = compute_base_tools(true, has_memory, &subagent_config(agent).workspace_tools)
        .into_iter()
        .map(|t| t.name)
        .collect();
    let mut registry = SkillRegistry::new(reserved)
        .with_allowlist(Some(agent.skill_packages.clone()))
        .with_shell_skills(!agent.no_shell_fallback);
    registry.reload(
        &crate::application::skills::registry::FileSystemSkillSource::new(workspace.to_path_buf()),
    );
    registry
}

/// Phase 5b — build the per-subprocess tool stack for `tengu run-agent`
/// (also `tengu tool call` and decision loops).
///
/// Resolves `effective_tools = agent_base_tools ∪ {compress_and_store} ∪
/// shell skills (skill_packages) ∪ [[mcp_servers]] tools (tools list)`, then
/// constructs a `PluginToolExecutor` over those tools. Returns the resolved
/// ToolDef list (so `run-agent` can pass it to `engine.run`, or a Claude
/// Code bridge as `bridge_tools`) plus the executor.
///
/// `compress_and_store` is dispatched out-of-band by the run-agent loop
/// (the summary is captured there and persisted to Postgres
/// `agentic_memory` with `postgres_memory`; a Claude Code step's bridge
/// writes it to the step's summary file) so its `ToolDef` is appended to the
/// advertised list but its execution path bypasses the `PluginToolExecutor`.
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

    // The agent's catalog tools: `tools` when listed (its workspace-tool
    // opt-ins included), else every base tool.
    let mut effective = agent_base_tools(agent, true, config.memory.enabled);

    // Always-on protocol tool. Phase 5b dispatches it out-of-band, so we
    // only need its description here for the LLM to see + call.
    effective
        .push(crate::adapters::outbound::tools::skill_lifecycle::compress_and_store::definition());

    // Shell skills named in `skill_packages` are tools here too, as in
    // in-process chat (skill bodies also reach the system prompt, loaded by
    // `run-agent`); none in a sandbox that runs no shell.
    let skill_registry = agent_skill_registry(workspace, &agent_cfg, config.memory.enabled);
    effective.extend(skill_registry.active_tools());

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

    /// `[generation]` defense in depth: an agent bound to W1 (the lineage
    /// fixture) gets no W2-only tool and no unregistered opt-in tool, even
    /// when its list names them; base tools stay.
    #[test]
    fn a_tool_outside_the_bound_generation_is_not_registered() {
        let reg = crate::config::lineage::load_registry(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/lineage/registry"),
        )
        .unwrap();
        let scope = crate::domain::lineage::generation::GenerationScope::of(&reg, "W1").unwrap();
        let def = |name: &str| ToolDef {
            name: name.into(),
            description: String::new(),
            parameters: serde_json::json!({"type": "object"}),
        };
        let tools = vec![
            def("backtest"),
            def("read_file"),
            def("w2_news_probe"),
            def("hl_ctx"),
        ];
        let mut agent = Config::default().agents["main"].clone();
        let names = |a: &AgentConfig| -> Vec<String> {
            within_generation(a, &tools)
                .into_iter()
                .map(|t| t.name)
                .collect()
        };
        assert_eq!(names(&agent).len(), 4, "unbound: every tool");
        let mut sections = (*agent.sandbox).clone();
        sections.generation = Some(Arc::new(scope));
        agent.sandbox = Arc::new(sections);
        assert_eq!(names(&agent), vec!["backtest", "read_file"]);
        // The advertised list goes through the same filter.
        agent.tools = vec!["backtest".into(), "read_file".into(), "hl_ctx".into()];
        let advertised: Vec<String> = agent_base_tools(&agent, true, false)
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert!(
            advertised.contains(&"backtest".to_string()),
            "{advertised:?}"
        );
        assert!(
            !advertised.contains(&"hl_ctx".to_string()),
            "{advertised:?}"
        );
    }

    /// No `workspace`: a fresh temp dir (canonical, named by the prefix,
    /// removed when it drops) — never the cwd; a configured one as given,
    /// `~` expanded, a relative one made absolute.
    #[test]
    fn workspace_or_temp_is_the_agents_or_a_temp_dir() {
        let (ws, dir) = workspace_or_temp(None, "tengu-step-").unwrap();
        let dir = dir.expect("a temp dir for an agent without workspace");
        assert!(ws.is_absolute() && ws.is_dir(), "{}", ws.display());
        assert_eq!(ws, std::fs::canonicalize(dir.path()).unwrap());
        assert!(
            ws.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("tengu-step-"),
            "{}",
            ws.display()
        );
        assert_ne!(ws, std::env::current_dir().unwrap());
        drop(dir);
        assert!(!ws.exists(), "removed when dropped: {}", ws.display());

        let pinned = TempDir::new().unwrap();
        let (ws, dir) = workspace_or_temp(Some(pinned.path()), "x-").unwrap();
        assert_eq!(ws, pinned.path());
        assert!(dir.is_none(), "a configured workspace is never removed");
        let (ws, _) = workspace_or_temp(Some(Path::new(".")), "x-").unwrap();
        assert_eq!(ws, std::fs::canonicalize(".").unwrap());
        let (ws, _) = workspace_or_temp(Some(Path::new("~/tengu-ws-x")), "x-").unwrap();
        assert!(
            ws.is_absolute() && ws.ends_with("tengu-ws-x"),
            "{}",
            ws.display()
        );
    }

    /// A `[solana]` signing sandbox: the fallback runs no shell (a shell
    /// could read the key file); everything else stays permissive and a
    /// configured scope is untouched.
    #[test]
    fn signing_sandbox_fallback_runs_no_shell() {
        let tmp = TempDir::new().unwrap();
        let configured = HashMap::from([(
            "run_command".to_string(),
            ToolScope {
                shell_bins: vec!["ls".to_string()],
                ..Default::default()
            },
        )]);
        let names = || vec!["run_command".to_string(), "my_shell_skill".to_string()];
        let open = resolve_tool_scopes(tmp.path(), &configured, names(), false);
        assert!(open["my_shell_skill"].check_shell_bin("cat").is_ok());
        let closed = resolve_tool_scopes(tmp.path(), &configured, names(), true);
        assert!(closed["my_shell_skill"].check_shell_bin("cat").is_err());
        assert!(closed["my_shell_skill"]
            .check_net_host("any.example")
            .is_ok());
        assert!(closed["run_command"].check_shell_bin("ls").is_ok());

        let mut cfg = crate::config::Config::default();
        cfg.fold_default_scopes();
        assert!(!cfg.agents["main"].no_shell_fallback);
        cfg.solana.signer_key_file = Some("/keys/signer.json".into());
        cfg.fold_default_scopes();
        assert!(cfg.agents["main"].no_shell_fallback);
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
            false,
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

    /// W1-gate review regression (was `run_agent_grant_turns_an_empty_
    /// writer_scope_into_the_workspace`): a routable agent handed
    /// `write_file` (a plan step's `compose.tools`) in a sandbox that denies
    /// it with an empty `[default_scopes.write_file]` (sandboxes/xmarket) is
    /// denied in-process AND in the run-agent child — the workspace grant
    /// leaves a deny-all scope alone, while a scope that configures
    /// something (the matrix fixtures' `fs_roots` elsewhere) still gets it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_agent_grant_keeps_an_empty_writer_scope_a_deny() {
        use crate::ports::engine::ToolExecutor;
        let ws = tempfile::TempDir::new().unwrap();
        let toml = format!(
            "[agents.arch]\nengine = \"openrouter\"\nmodel = \"m\"\ndescription = \"routable\"\n\
             workspace = \"{}\"\ntools = [\"write_file\", \"read_file\"]\n\n\
             [default_scopes.write_file]\n\n\
             [default_scopes.read_file]\nfs_roots = [\"/nonexistent/elsewhere\"]\n",
            ws.path().display()
        );
        let mut cfg: Config = toml::from_str(&toml).unwrap();
        cfg.fold_default_scopes();
        let agent = cfg.agents["arch"].clone();
        let secrets = Arc::new(SecretRegistry::new());
        std::fs::write(ws.path().join("notes.txt"), "hello").unwrap();
        let write = ToolCall {
            id: "probe:1".into(),
            name: "write_file".into(),
            arguments: serde_json::json!({"path": "answer.txt", "content": "not allowed"}),
        };
        let read = ToolCall {
            id: "probe:2".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({"path": "notes.txt"}),
        };

        // In-process executor (chat): the empty scope denies.
        let tools = agent_base_tools(&agent, true, false);
        let skills = agent_skill_registry(ws.path(), &agent, false);
        let inproc = build_tool_executor(
            ws.path(),
            &tools,
            &skills,
            &None,
            &secrets,
            Arc::new(crate::adapters::outbound::noop::NoopActivity),
            None,
            None,
            &agent,
            &[],
        )
        .unwrap();
        assert!(inproc.execute(&write, &[]).await.is_err());

        // run-agent child executor (plan step): still denied; the configured
        // read_file scope gets the workspace grant.
        let (_, exec) = build_subprocess_tool_executor(
            &agent,
            &cfg,
            ws.path(),
            &secrets,
            Arc::new(crate::adapters::outbound::noop::NoopActivity),
            None,
        );
        let exec = exec.unwrap();
        let denied = exec.execute(&write, &[]).await;
        assert!(denied.is_err(), "run-agent child wrote: {denied:?}");
        assert!(!ws.path().join("answer.txt").exists());
        assert_eq!(exec.execute(&read, &[]).await.unwrap(), "hello");
        assert!(exec.scopes["write_file"].is_deny_all());
    }

    /// `grant_workspace_root`: a deny-all scope stays one; every other
    /// configured scope gains the workspace once.
    #[test]
    fn grant_workspace_root_skips_deny_all_scopes() {
        let ws = Path::new("/srv/step-ws");
        let mut scopes = HashMap::from([
            ("write_file".to_string(), ToolScope::default()),
            (
                "http_request".to_string(),
                ToolScope {
                    net_hosts: vec!["api.example.com".into()],
                    ..Default::default()
                },
            ),
            (
                "read_file".to_string(),
                ToolScope {
                    fs_roots: vec![ws.to_path_buf()],
                    ..Default::default()
                },
            ),
        ]);
        grant_workspace_root(&mut scopes, ws);
        assert!(scopes["write_file"].is_deny_all());
        assert_eq!(scopes["http_request"].fs_roots, [ws]);
        assert_eq!(scopes["read_file"].fs_roots, [ws]);
    }

    /// Hardened sandbox: a compose may only narrow its base agent — a tool
    /// or skill the base lacks, or an empty `tools` (= every catalog tool)
    /// over a listed base, refuses the step. Elsewhere it replaces wholesale.
    #[test]
    fn compose_only_narrows_in_a_hardened_sandbox() {
        use crate::domain::plan::AgentCompose;
        let mut base = Config::default().agents.remove("main").unwrap();
        base.tools = vec!["read_file".into(), "list_directory".into(), "hl_ctx".into()];
        base.skill_packages = vec!["research".into()];
        let compose = |tools: &[&str], skills: &[&str]| AgentCompose {
            base_agent: "arch".into(),
            tools: tools.iter().map(|s| s.to_string()).collect(),
            skills: skills.iter().map(|s| s.to_string()).collect(),
        };
        let run = |base: &AgentConfig, c: &AgentCompose, hardened: bool| {
            compose_agent("arch", base, c, hardened, false)
        };

        let narrowed = run(&base, &compose(&["read_file"], &[]), true).unwrap();
        assert_eq!(narrowed.tools, ["read_file"]);
        assert!(narrowed.skill_packages.is_empty());
        run(
            &base,
            &compose(&["hl_ctx", "list_directory"], &["research"]),
            true,
        )
        .unwrap();

        for (c, want) in [
            (
                compose(&["read_file", "write_file"], &[]),
                "[\"write_file\"]",
            ),
            (compose(&["paper_order"], &[]), "[\"paper_order\"]"),
            (compose(&[], &[]), "\"run_command\""),
            (compose(&["read_file"], &["evil"]), "skills [\"evil\"]"),
        ] {
            let err = run(&base, &c, true).unwrap_err().to_string();
            assert!(
                err.contains("compose would widen agent `arch` in a hardened sandbox")
                    && err.contains(want),
                "{c:?}: {err}"
            );
            let open = run(&base, &c, false).unwrap();
            assert_eq!(open.tools, c.tools, "not hardened: wholesale");
        }

        // A base with every catalog tool (`tools = []`): an opt-in tool it
        // never enabled (an exec tool) is still a widening.
        base.tools.clear();
        run(&base, &compose(&["write_file"], &[]), true).unwrap();
        let err = run(&base, &compose(&["paper_order"], &[]), true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("paper_order"), "{err}");
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

    // Claude Code webhook and eval agents used to get `bridge_tools: None` —
    // no tengu tools and no `[[mcp_servers]]` tools at all.
    #[test]
    fn bridge_inputs_go_to_engines_that_manage_their_workspace() {
        let tools = vec![
            ToolDef::new("http_request", "d", serde_json::json!({})),
            ToolDef::new("fake__echo", "d", serde_json::json!({})),
        ];
        let servers = vec![crate::adapters::outbound::mcp_client::tests::fake_server(
            "fake",
        )];

        let (bridge, passed) = bridge_inputs(true, &tools, &servers);
        let names: Vec<String> = bridge.unwrap().into_iter().map(|t| t.name).collect();
        assert_eq!(names, ["http_request", "fake__echo"]);
        assert_eq!(passed.len(), 1);

        // OpenRouter-style engines: tools go through the model API instead.
        let (bridge, passed) = bridge_inputs(false, &tools, &servers);
        assert!(bridge.is_none() && passed.is_empty());
        // Orchestrator agent (no tools): no bridge.
        assert!(bridge_inputs(true, &[], &servers).0.is_none());
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
        let bridge = with_mcp_bridge_tools(base.clone(), &[], &servers).await;
        let names: Vec<&str> = bridge.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["read_file", "fake__echo"]);
        // A `tools` list keeps only the listed server tools.
        let listed = with_mcp_bridge_tools(base, &["read_file".to_string()], &servers).await;
        assert_eq!(listed.len(), 1, "fake__echo is not in the agent's tools");
        // No bridge (non-Claude-Code agent) stays empty — servers not dialled.
        assert!(with_mcp_bridge_tools(Vec::new(), &[], &servers)
            .await
            .is_empty());

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
