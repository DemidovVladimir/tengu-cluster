//! Full-screen TUI runtime for interactive chat using cursive.

pub mod app;
pub mod view;

use anyhow::Result;
use cursive::backends::crossterm::crossterm::{event::DisableMouseCapture, execute};
use cursive::Cursive;
use std::path::PathBuf;
use std::sync::{mpsc, Arc};

use crate::adapters::channel_runtime;
use crate::adapters::chat_builder::{
    handle_chat_command, ChatRuntimeService, ChatTurnResult, CommandResult, EngineInfo,
};
use crate::adapters::config::{Config, RuntimeProfile};
use crate::adapters::engine_builder::build_engine;
use crate::adapters::engine_builder::{SanitizedToolExecutor, ToolExecutor};
use crate::adapters::flow_builder::{resolve_flow_compaction_policy, resolve_history_turn_limit};
use crate::adapters::ports::ToolActivityPort;
use crate::adapters::secret_builder::SecretRegistry;
use crate::adapters::skill_builder::{
    self, FileSystemSkillSource, SkillCommandMatch, SkillCommandRouter, SkillRegistry, SkillStatus,
};
use crate::adapters::types::{ToolCall, ToolDef};
use app::{BubbleRole, ChatRequest, SkillCommand};
fn disable_terminal_mouse_capture() -> Result<()> {
    #[cfg(unix)]
    {
        let mut tty = std::fs::OpenOptions::new().write(true).open("/dev/tty")?;
        execute!(tty, DisableMouseCapture)?;
    }

    #[cfg(windows)]
    {
        let mut out = std::io::stdout();
        execute!(out, DisableMouseCapture)?;
    }

    Ok(())
}

/// Wrapper to send `Box<dyn Engine>` to the engine thread.
/// Safe because the engine is only ever accessed from the single engine thread.
struct SendEngine(Box<dyn crate::adapters::Engine>);
unsafe impl Send for SendEngine {}

/// TUI adapter for publishing tool activity lines.
struct CursiveToolActivityAdapter {
    cb_sink: cursive::CbSink,
}

impl ToolActivityPort for CursiveToolActivityAdapter {
    fn publish_tool_activity(&self, call: &ToolCall) {
        let (tool_name, detail) = crate::adapters::tool_builder::build_tool_activity_text(call);
        let detail = detail.unwrap_or_default();

        let cb = self.cb_sink.clone();
        let _ = cb.send(Box::new(move |siv: &mut Cursive| {
            view::push_tool_activity(siv, &tool_name, &detail);
        }));
    }
}

/// Run the full-screen TUI chat (blocking — call from `block_in_place`).
pub fn run_tui(
    config: Config,
    _profile: RuntimeProfile,
    secret_registry: Arc<SecretRegistry>,
) -> Result<()> {
    // Resolve agent config
    let (agent_id, agent_config) = config
        .agents
        .iter()
        .find(|(_, ac)| ac.default)
        .or_else(|| config.agents.iter().next())
        .map(|(id, ac)| (id.clone(), ac.clone()))
        .ok_or_else(|| anyhow::anyhow!("No agents configured"))?;

    let engine = build_engine(&agent_id, &agent_config, config.claude_code.as_ref())?;

    // Capture engine metadata for slash commands (before moving engine to thread)
    let engine_info = EngineInfo {
        context_window: engine.context_window(),
        diagnostics: engine.diagnostics(),
    };

    let history_turn_limit = resolve_history_turn_limit(&agent_config.flow);
    let compaction_policy = resolve_flow_compaction_policy(
        &agent_config.flow,
        agent_config.limits.max_tokens_per_flow,
        engine.context_window(),
        engine.max_output_tokens_per_turn() as usize,
    );
    let advertise_workspace_tools = engine.supports_tool_use() && !engine.manages_own_workspace();

    // Build initial system prompt without skill contexts — the engine thread will rebuild
    // with actual skills on the first turn (tools_dirty = true).
    let system_prompt =
        skill_builder::build_system_prompt(&agent_config, advertise_workspace_tools, &[]);

    // Channel: UI → Engine thread
    let (request_tx, request_rx) = mpsc::channel::<ChatRequest>();

    // Build cursive
    let mut siv = cursive::default();

    // Apply dark theme by default
    view::apply_theme(&mut siv, app::ThemeMode::Dark);

    // Build UI
    view::build_ui(&mut siv, request_tx);

    // Set header
    let identity = agent_config.identity.name.as_deref().unwrap_or("Tengu");
    let engine_label = format!("{}/{}", agent_config.engine, agent_config.model);
    let default_lens = agent_config.default_lens.clone();
    view::update_header(&mut siv, identity, &engine_label, &default_lens);
    view::push_welcome(&mut siv);

    let memory_config = config.memory.clone();
    let mcp_servers = config.mcp_servers.clone();

    // Harness-owned orchestration (Track A — factory-closure wiring).
    //
    // `MemoryManager` stays empty (no providers) — Track B owns the port
    // wiring that registers `BuiltinMemoryProvider`. An empty manager is a
    // no-op for `prefetch_all` / `sync_all`, which is correct for now.
    //
    // `orchestrator_snapshots` is shared between this main thread (where
    // the orchestrator + factory are constructed) and the engine thread
    // (where snapshots are written before each user-message dispatch).
    // The engine thread runs `rt.block_on(orchestrator.handle(...))` and the
    // orchestrator spawns step tasks that call our factory closure, which
    // reads inputs back from the same map.
    let _memory_manager: Arc<crate::adapters::memory::manager::MemoryManager> =
        Arc::new(crate::adapters::memory::manager::MemoryManager::new());
    let orchestrator_snapshots: channel_runtime::OrchestratorSnapshots =
        Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
    let orchestrator: Option<Arc<crate::adapters::orchestrator::Orchestrator>> = {
        let inputs_fn = channel_runtime::snapshots_inputs_fn(Arc::clone(&orchestrator_snapshots));
        let factory: Arc<dyn crate::adapters::orchestrator::wiring::ChatServiceFactory> =
            Arc::new(channel_runtime::RuntimeChatServiceFactory::new(inputs_fn));
        channel_runtime::build_orchestrator(&config, factory, Arc::clone(&_memory_manager))
            .map(Arc::new)
    };
    if orchestrator.is_some() {
        tracing::info!("TUI orchestrator constructed with per-turn snapshot factory");
    }

    // Task 5.4 — verbose event rendering for CLI. Subscribe to the
    // orchestrator bus (if configured) and push each progress event as a
    // System bubble into the TUI. When the factory-closure refactor lands,
    // this will render the live DAG progress tree from the same subscriber.
    // Cursive owns stdout, so we render through `cb_sink` rather than
    // printing directly — avoids corrupting the TUI frame buffer.
    if let Some(orch) = orchestrator.as_ref() {
        let mut rx = orch.subscribe();
        let sink = siv.cb_sink().clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                use crate::adapters::orchestrator::OrchestratorEvent;
                loop {
                    match rx.recv().await {
                        Ok(event) => {
                            let line: Option<String> = match event {
                                OrchestratorEvent::PlanCreated { plan } => Some(format!(
                                    "orch: plan created ({} step{})",
                                    plan.steps.len(),
                                    if plan.steps.len() == 1 { "" } else { "s" },
                                )),
                                OrchestratorEvent::StepStarted { step_id, agent } => {
                                    Some(format!("orch: ▶ {} [{}]", step_id.0, agent))
                                }
                                OrchestratorEvent::StepSucceeded { step_id, .. } => {
                                    Some(format!("orch: ✓ {}", step_id.0))
                                }
                                OrchestratorEvent::StepFailed {
                                    step_id,
                                    attempt,
                                    error,
                                } => Some(format!(
                                    "orch: ✗ {} (attempt {}): {}",
                                    step_id.0, attempt, error
                                )),
                                OrchestratorEvent::StepExhausted {
                                    step_id,
                                    final_error,
                                } => Some(format!(
                                    "orch: ⊘ {} exhausted: {}",
                                    step_id.0, final_error
                                )),
                                OrchestratorEvent::ReplanTriggered { reason } => {
                                    Some(format!("orch: ↻ replan — {}", reason))
                                }
                                OrchestratorEvent::PlanCompleted { cancelled, .. } => {
                                    Some(if cancelled {
                                        "orch: plan cancelled".to_string()
                                    } else {
                                        "orch: plan completed".to_string()
                                    })
                                }
                                OrchestratorEvent::StepProgress { .. } => None,
                            };
                            if let Some(text) = line {
                                let _ = sink.send(Box::new(move |siv: &mut Cursive| {
                                    view::push_bubble(siv, BubbleRole::System, &text);
                                }));
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(dropped = n, "orch event subscriber lagged");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
        } else {
            tracing::warn!(
                "TUI orchestrator configured but no tokio runtime — event subscriber not spawned"
            );
        }
    }

    // Spawn engine thread
    let cb_sink = siv.cb_sink().clone();
    let send_engine = SendEngine(engine);
    let engine_agent_id = agent_id.clone();
    let engine_agent_config = agent_config.clone();
    let secret_registry_clone = Arc::clone(&secret_registry);
    let engine_orchestrator = orchestrator.clone();
    let engine_orchestrator_snapshots = Arc::clone(&orchestrator_snapshots);

    std::thread::spawn(move || {
        let secret_registry = secret_registry_clone;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Failed to create tokio runtime for engine thread");

        // Promote the engine to an Arc so orchestrator snapshots can share it.
        let engine: Arc<dyn crate::adapters::Engine> = Arc::from(send_engine.0);

        // Resolve workspace path (expand tilde)
        let workspace: Option<PathBuf> = engine_agent_config
            .workspace
            .as_ref()
            .map(|p| crate::adapters::tool_builder::expand_tilde(p));

        // Build memory subsystem if enabled. Backed by a shared
        // `Embedder` + `VectorStore` pair via `MemoryManager`.
        let memory_manager_handle: Option<Arc<crate::adapters::memory::manager::MemoryManager>> = {
            let mgr =
                channel_runtime::build_memory_manager(&memory_config, &rt, workspace.as_deref());
            let vector_ready =
                futures::executor::block_on(async { mgr.has_vector_backend().await });
            if vector_ready {
                Some(mgr)
            } else {
                None
            }
        };

        // Base workspace tools (built-in + memory, without skills).
        let uses_tools =
            engine.supports_tool_use() && !engine.manages_own_workspace() && workspace.is_some();
        let has_memory = memory_manager_handle.is_some();
        let manages_workspace = engine.manages_own_workspace();
        let mut base_tools = channel_runtime::compute_base_tools(
            uses_tools,
            has_memory,
            &engine_agent_config.workspace_tools,
        );
        // For engines that manage their own workspace (claude_code), build bridge
        // tools so Tengu-native tools are still accessible via MCP bridge.
        let bridge_base_tools: Vec<crate::adapters::types::ToolDef> = if manages_workspace
            && workspace.is_some()
        {
            channel_runtime::compute_bridge_tools(has_memory, &engine_agent_config.workspace_tools)
        } else {
            vec![]
        };

        // Skill registry — initialized and loaded once, hot-reloaded each turn.
        let skill_source: Option<FileSystemSkillSource> = workspace
            .as_ref()
            .map(|ws| FileSystemSkillSource::new(ws.clone()));

        let base_reserved: Vec<String> = base_tools.iter().map(|t| t.name.clone()).collect();
        let mut skill_registry = SkillRegistry::new(base_reserved)
            .with_allowlist(Some(engine_agent_config.skill_packages.clone()));

        if let Some(ref src) = skill_source {
            skill_registry.reload(src);
        }

        let mut skill_command_router = SkillCommandRouter::from_registry(&skill_registry);

        // Create channel-specific port adapters once, share via Arc.
        let activity: Arc<dyn ToolActivityPort> = Arc::new(CursiveToolActivityAdapter {
            cb_sink: cb_sink.clone(),
        });

        let mut tools_dirty = true;
        let mut current_tools: Vec<ToolDef> = vec![];
        let mut current_bridge_tools: Vec<ToolDef> = vec![];
        let mut current_executor: Option<Arc<crate::adapters::tool_plugin::PluginToolExecutor>> =
            None;
        let mut current_system_prompt = system_prompt;

        let mut runtime_state = channel_runtime::create_chat_loop_state(&engine_agent_config);

        // Memory recall during chat turns goes through the
        // `MemoryManager` directly (ChatRuntimeService::memory_manager).

        while let Ok(request) = request_rx.recv() {
            match request {
                ChatRequest::SkillCommand {
                    command,
                    response_tx,
                } => {
                    let response = match command {
                        SkillCommand::List => channel_runtime::format_skill_list(&skill_registry),
                        SkillCommand::Enable(ref name) => match skill_registry.enable(name) {
                            Ok(true) => {
                                tools_dirty = true;
                                format!("Enabled skill '{}'.", name)
                            }
                            Ok(false) => format!("Skill '{}' is already active.", name),
                            Err(e) => e,
                        },
                        SkillCommand::Disable(ref name) => match skill_registry.disable(name) {
                            Ok(true) => {
                                tools_dirty = true;
                                format!("Disabled skill '{}'.", name)
                            }
                            Ok(false) => format!("Skill '{}' is already disabled.", name),
                            Err(e) => e,
                        },
                    };
                    let _ = response_tx.send(response);
                    continue; // don't process as chat turn
                }

                ChatRequest::SlashCommand { text, response_tx } => {
                    // /purge — reset conversation + clear persistent memory.
                    // Handled here (not in chat_commands) because it needs
                    // async access to the memory store via the engine thread's
                    // tokio runtime.
                    if text == "/purge" {
                        runtime_state.reset_for_new_session();
                        let mut lines = vec!["Conversation cleared.".to_string()];
                        if let Some(ref mgr) = memory_manager_handle {
                            match rt.block_on(mgr.clear_all()) {
                                Ok(()) => lines.push("Persistent memory cleared.".to_string()),
                                Err(e) => lines.push(format!("Memory clear failed: {}", e)),
                            }
                        } else {
                            lines.push("No persistent memory active.".to_string());
                        }
                        let _ = response_tx.send(lines.join("\n"));
                        continue;
                    }

                    // /reload — re-read env vars, re-scan skill files, and
                    // rebuild tools + executor + system prompt immediately.
                    if text == "/reload" {
                        let mut lines = Vec::new();

                        // Re-read env vars and rebuild base tool set.
                        let new_base = channel_runtime::compute_base_tools(
                            uses_tools,
                            has_memory,
                            &engine_agent_config.workspace_tools,
                        );
                        let env_changed = new_base.len() != base_tools.len();
                        base_tools = new_base;
                        if env_changed {
                            lines.push("Environment refreshed.".to_string());
                        }

                        // Re-scan skill files.
                        if let Some(ref src) = skill_source {
                            let changed = skill_registry.reload(src);
                            if changed {
                                tools_dirty = true;
                                lines.push("Skills reloaded (changes detected).".to_string());
                            } else {
                                lines.push("Skills reloaded (no changes).".to_string());
                            }
                        } else {
                            lines.push("No workspace — skills unavailable.".to_string());
                        }

                        // Always force a full rebuild to pick up env + skill changes.
                        if let Some(ref ws) = workspace {
                            current_tools =
                                channel_runtime::rebuild_tools(&base_tools, &skill_registry);
                            current_executor = channel_runtime::build_tool_executor(
                                ws,
                                &current_tools,
                                &skill_registry,
                                &memory_manager_handle,
                                &secret_registry,
                                Arc::clone(&activity),
                                None,
                                None,
                                Some(&memory_config),
                                &engine_agent_config,
                                &mcp_servers,
                            )
                            .map(Arc::new);
                            if let Some(ref exec) = current_executor {
                                let extra = exec.additional_tool_defs(&current_tools);
                                if !extra.is_empty() {
                                    current_tools.extend(extra);
                                }
                            }
                            current_system_prompt = channel_runtime::rebuild_system_prompt(
                                &engine_agent_config,
                                advertise_workspace_tools,
                                &skill_registry,
                                &current_tools,
                            );
                            tools_dirty = false;
                        }

                        // Rebuild the skill command router after reload.
                        skill_command_router = SkillCommandRouter::from_registry(&skill_registry);

                        let skill_list = skill_registry.list_all();
                        lines.push(format!("{} skill(s) registered.", skill_list.len()));
                        for (name, status) in &skill_list {
                            let tag = match status {
                                SkillStatus::Active => "active",
                                SkillStatus::Inactive => "disabled",
                            };
                            lines.push(format!("  {} ({})", name, tag));
                        }
                        lines.push(format!("{} tool(s) active.", current_tools.len()));
                        let _ = response_tx.send(lines.join("\n"));
                        continue;
                    }

                    // Check skill commands before built-in commands.
                    if let SkillCommandMatch::Matched {
                        skill_name,
                        command: cmd_name,
                        args,
                    } = skill_command_router.route(&text)
                    {
                        // Inject a system-augmented user message so the agent handles it.
                        let injected = format!(
                            "[System: User invoked /{cmd} {args}. Follow the instructions in the ## Commands section of the {skill} skill.]",
                            cmd = cmd_name,
                            args = args,
                            skill = skill_name,
                        );
                        // Don't respond via response_tx — instead drop into a chat turn.
                        let _ = response_tx.send(
                            format!("Running /{} {}…", cmd_name, args)
                                .trim()
                                .to_string(),
                        );

                        // Hot-reload + rebuild if dirty.
                        if let Some(ref src) = skill_source {
                            if skill_registry.reload(src) {
                                tools_dirty = true;
                                skill_command_router =
                                    SkillCommandRouter::from_registry(&skill_registry);
                            }
                        }
                        if tools_dirty {
                            if let Some(ref ws) = workspace {
                                current_tools =
                                    channel_runtime::rebuild_tools(&base_tools, &skill_registry);
                                current_executor = channel_runtime::build_tool_executor(
                                    ws,
                                    &current_tools,
                                    &skill_registry,
                                    &memory_manager_handle,
                                    &secret_registry,
                                    Arc::clone(&activity),
                                    None,
                                    None,
                                    Some(&memory_config),
                                    &engine_agent_config,
                                    &mcp_servers,
                                )
                                .map(Arc::new);
                                if let Some(ref exec) = current_executor {
                                    let extra = exec.additional_tool_defs(&current_tools);
                                    if !extra.is_empty() {
                                        current_tools.extend(extra);
                                    }
                                }
                                current_system_prompt = channel_runtime::rebuild_system_prompt(
                                    &engine_agent_config,
                                    advertise_workspace_tools,
                                    &skill_registry,
                                    &current_tools,
                                );
                                if manages_workspace {
                                    current_bridge_tools = channel_runtime::rebuild_tools(
                                        &bridge_base_tools,
                                        &skill_registry,
                                    );
                                }
                            }
                            tools_dirty = false;
                        }

                        // Process as a chat turn with the injected prompt.
                        rt.block_on(async {
                            let sanitized_executor = current_executor.as_ref().map(|e| {
                                let inner: std::sync::Arc<dyn ToolExecutor> =
                                    Arc::clone(e) as std::sync::Arc<dyn ToolExecutor>;
                                SanitizedToolExecutor::new(inner, Arc::clone(&secret_registry))
                            });
                            let tool_defs = current_tools.clone();

                            let chat_runtime = ChatRuntimeService {
                                engine: engine.as_ref(),
                                agent_id: &engine_agent_id,
                                agent_config: &engine_agent_config,
                                history_turn_limit,
                                compaction_policy,
                                system_prompt: current_system_prompt.clone(),
                                tools: &tool_defs,
                                tool_executor: sanitized_executor
                                    .as_ref()
                                    .map(|e| e as &dyn ToolExecutor),
                                memory_manager: memory_manager_handle.as_deref(),
                                max_recall_entries: memory_config.max_recall_entries,
                                max_recall_tokens: memory_config.max_recall_tokens,
                                tool_observer: None,
                                cancel: None,
                                bridge_tools: if current_bridge_tools.is_empty() {
                                    None
                                } else {
                                    Some(&current_bridge_tools)
                                },
                                suppress_grounding_nudge: false,
                            };

                            match chat_runtime
                                .process_user_text(&mut runtime_state, &injected)
                                .await
                            {
                                Ok(res) => {
                                    // Pull fresh memory stats from the
                                    // manager (entry_count + storage_bytes).
                                    let memory_stats: Option<(usize, u64)> = memory_manager_handle
                                        .as_ref()
                                        .and_then(|mgr| rt.block_on(mgr.stats()));
                                    let _ = cb_sink.send(Box::new(move |siv: &mut Cursive| {
                                        view::hide_thinking(siv);
                                        if let Some(notice) = res.system_notice {
                                            view::push_bubble(siv, BubbleRole::System, &notice);
                                        }
                                        if let Some(text) = res.assistant_text {
                                            view::push_bubble(siv, BubbleRole::Assistant, &text);
                                        }
                                        view::update_status(
                                            siv,
                                            res.total_input_tokens,
                                            res.total_output_tokens,
                                            memory_stats,
                                        );
                                    }));
                                }
                                Err(e) => {
                                    let err_msg = format!("Engine error: {}", e);
                                    let _ = cb_sink.send(Box::new(move |siv: &mut Cursive| {
                                        view::hide_thinking(siv);
                                        view::push_bubble(siv, BubbleRole::System, &err_msg);
                                    }));
                                }
                            }
                        });
                        continue;
                    }

                    let skill_cmds = skill_command_router.list();
                    let result = handle_chat_command(
                        &text,
                        &mut runtime_state,
                        &engine_info,
                        &engine_agent_config,
                        history_turn_limit,
                        compaction_policy,
                        &skill_cmds,
                    );
                    let response = match result {
                        CommandResult::Handled(output) => output.lines.join("\n"),
                        CommandResult::NotHandled => {
                            "Unknown command. Type /help for available commands.".to_string()
                        }
                    };
                    let _ = response_tx.send(response);
                    continue;
                }

                ChatRequest::UserMessage { user_text } => {
                    // Hot-reload: re-scan skill files before each turn.
                    if let Some(ref src) = skill_source {
                        if skill_registry.reload(src) {
                            tools_dirty = true;
                            skill_command_router =
                                SkillCommandRouter::from_registry(&skill_registry);
                        }
                    }

                    // Rebuild tools/executor/prompt when dirty.
                    if tools_dirty {
                        if let Some(ref ws) = workspace {
                            current_tools =
                                channel_runtime::rebuild_tools(&base_tools, &skill_registry);
                            current_executor = channel_runtime::build_tool_executor(
                                ws,
                                &current_tools,
                                &skill_registry,
                                &memory_manager_handle,
                                &secret_registry,
                                Arc::clone(&activity),
                                None,
                                None,
                                Some(&memory_config),
                                &engine_agent_config,
                                &mcp_servers,
                            )
                            .map(Arc::new);
                            if let Some(ref exec) = current_executor {
                                let extra = exec.additional_tool_defs(&current_tools);
                                if !extra.is_empty() {
                                    current_tools.extend(extra);
                                }
                            }
                            current_system_prompt = channel_runtime::rebuild_system_prompt(
                                &engine_agent_config,
                                advertise_workspace_tools,
                                &skill_registry,
                                &current_tools,
                            );
                        }
                        tools_dirty = false;
                    }

                    let text = user_text;

                    // Orchestrator dispatch path. When orchestration is
                    // configured we route the user message through
                    // `Orchestrator::handle` instead of a single
                    // `ChatRuntimeService` turn. The snapshot map is shared
                    // with the factory closure; we publish a fresh entry for
                    // this agent each turn so hot-reloaded tools / system
                    // prompt / owned executor are visible when the DAG
                    // executor spawns a step.
                    if let Some(ref orch) = engine_orchestrator {
                        // Move the current executor into an `Arc` and wrap
                        // with the owned sanitizer. Because the executor is
                        // rebuilt whenever `tools_dirty` fires, this snapshot
                        // is safe to share across spawned orchestrator tasks.
                        let sanitized_exec_arc: Option<Arc<dyn ToolExecutor>> =
                            current_executor.take().map(|exec| {
                                let inner: Arc<dyn ToolExecutor> = exec as Arc<dyn ToolExecutor>;
                                let wrapped: Arc<dyn ToolExecutor> = Arc::new(
                                    crate::adapters::engine_builder::SanitizedToolExecutor::new(
                                        Arc::clone(&inner),
                                        Arc::clone(&secret_registry),
                                    ),
                                );
                                wrapped
                            });

                        let bridge_tools_opt = if current_bridge_tools.is_empty() {
                            None
                        } else {
                            Some(current_bridge_tools.clone())
                        };

                        let snapshot = channel_runtime::ChatTurnInputs {
                            engine: Arc::clone(&engine),
                            agent_id: engine_agent_id.clone(),
                            agent_config: Arc::new(engine_agent_config.clone()),
                            history_turn_limit,
                            compaction_policy,
                            system_prompt: current_system_prompt.clone(),
                            tools: current_tools.clone(),
                            tool_executor: sanitized_exec_arc,
                            memory_manager: memory_manager_handle.clone(),
                            max_recall_entries: memory_config.max_recall_entries,
                            max_recall_tokens: memory_config.max_recall_tokens,
                            bridge_tools: bridge_tools_opt,
                            tool_observer: None,
                            cancel: None,
                        };
                        if let Ok(mut guard) = engine_orchestrator_snapshots.write() {
                            guard.insert(engine_agent_id.clone(), snapshot);
                        } else {
                            tracing::error!("orchestrator snapshots lock poisoned");
                        }

                        let final_output = rt.block_on(orch.handle(text.clone()));

                        let redacted = secret_registry.redact(&final_output);
                        let _ = cb_sink.send(Box::new(move |siv: &mut Cursive| {
                            view::hide_thinking(siv);
                            if redacted.trim().is_empty() {
                                view::push_bubble(
                                    siv,
                                    BubbleRole::System,
                                    "(Orchestrator returned no response)",
                                );
                            } else {
                                view::push_bubble(siv, BubbleRole::Assistant, &redacted);
                            }
                        }));

                        // The orchestrator took ownership of the executor
                        // (via the sanitized Arc). Mark tools dirty so the
                        // next direct or orchestrator turn rebuilds a fresh
                        // one against the latest skill registry.
                        tools_dirty = true;
                        continue;
                    }

                    rt.block_on(async {
                        // Wrap tool executor with secret redaction decorator.
                        let sanitized_executor = current_executor.as_ref().map(|e| {
                            let inner: std::sync::Arc<dyn ToolExecutor> =
                                Arc::clone(e) as std::sync::Arc<dyn ToolExecutor>;
                            SanitizedToolExecutor::new(inner, Arc::clone(&secret_registry))
                        });
                        let tool_defs = current_tools.clone();

                        let chat_runtime = ChatRuntimeService {
                            engine: engine.as_ref(),
                            agent_id: &engine_agent_id,
                            agent_config: &engine_agent_config,
                            history_turn_limit,
                            compaction_policy,
                            system_prompt: current_system_prompt.clone(),
                            tools: &tool_defs,
                            tool_executor: sanitized_executor
                                .as_ref()
                                .map(|e| e as &dyn ToolExecutor),
                            memory_manager: memory_manager_handle.as_deref(),
                            max_recall_entries: memory_config.max_recall_entries,
                            max_recall_tokens: memory_config.max_recall_tokens,
                            tool_observer: None,
                            cancel: None,
                            bridge_tools: None,
                            suppress_grounding_nudge: false,
                        };

                        match chat_runtime
                            .process_user_text(&mut runtime_state, &text)
                            .await
                        {
                            Ok(ChatTurnResult {
                                assistant_text,
                                system_notice,
                                total_input_tokens,
                                total_output_tokens,
                                ..
                            }) => {
                                // Pull fresh memory stats from the manager.
                                // NOTE: we're already inside `rt.block_on(async { ... })`
                                // at line ~728. Calling `rt.block_on(...)` again here
                                // panics with "Cannot start a runtime from within a
                                // runtime". Use .await instead.
                                let memory_stats: Option<(usize, u64)> =
                                    match memory_manager_handle.as_ref() {
                                        Some(mgr) => mgr.stats().await,
                                        None => None,
                                    };
                                let _ = cb_sink.send(Box::new(move |siv: &mut Cursive| {
                                    view::hide_thinking(siv);
                                    if let Some(notice) = system_notice {
                                        view::push_bubble(siv, BubbleRole::System, &notice);
                                    }
                                    if let Some(text) = assistant_text {
                                        view::push_bubble(siv, BubbleRole::Assistant, &text);
                                    }
                                    view::update_status(
                                        siv,
                                        total_input_tokens,
                                        total_output_tokens,
                                        memory_stats,
                                    );
                                }));
                            }
                            Err(e) => {
                                let err_msg = format!("Engine error: {}", e);
                                let _ = cb_sink.send(Box::new(move |siv: &mut Cursive| {
                                    view::hide_thinking(siv);
                                    view::push_bubble(siv, BubbleRole::System, &err_msg);
                                }));
                            }
                        }
                    });
                }
            }
        }
    });

    // Run cursive via explicit runner so we can disable mouse capture.
    let mut runner = siv
        .try_runner()
        .map_err(|e| anyhow::Error::msg(e.to_string()))?;
    disable_terminal_mouse_capture()?;
    runner.run();

    Ok(())
}
