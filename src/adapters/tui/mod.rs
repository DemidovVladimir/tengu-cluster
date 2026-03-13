//! Full-screen TUI runtime for interactive chat using cursive.

pub mod app;
pub mod view;

use anyhow::Result;
use cursive::backends::crossterm::crossterm::{event::DisableMouseCapture, execute};
use cursive::Cursive;
use std::path::PathBuf;
use std::sync::{mpsc, Arc};

use crate::adapters::channel_runtime;
use crate::adapters::engine_factory::build_engine;
use crate::adapters::flow_store::FlowStore;
use crate::adapters::skill_source::FileSystemSkillSource;
use crate::adapters::system_prompt;
use crate::adapters::workspace_tools;
use crate::application::chat_commands::{self, CommandResult, EngineInfo};
use crate::application::chat_runtime::{ChatRuntimeService, ChatTurnResult};
use crate::application::engine_runtime::{SanitizedToolExecutor, ToolExecutor};
use crate::application::flow_policy::resolve_flow_compaction_policy;
use crate::application::memory_service::MemoryService;
use crate::application::ports::{ToolActivityPort, ToolApprovalPort};
use crate::application::skill_commands::{SkillCommandMatch, SkillCommandRouter};
use crate::application::skill_registry::SkillRegistry;
use crate::domain::capability::{parse_capability_set, RegisteredTool};
use crate::domain::chat::resolve_history_turn_limit;
use crate::domain::secret_registry::SecretRegistry;
use crate::domain::skill::SkillStatus;
use crate::resolve_tengu_home;
use app::{BubbleRole, ChatRequest, SkillCommand};
use tengu_core::config::{Config, RuntimeProfile};
use tengu_core::types::ToolCall;
use tengu_core::Refiner;
use tengu_optimizer::{NoopRefiner, RuleRefiner};

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
struct SendEngine(Box<dyn tengu_core::Engine>);
unsafe impl Send for SendEngine {}

/// Wrapper to send `Box<dyn Refiner>` to the engine thread.
struct SendRefiner(Box<dyn Refiner>);
unsafe impl Send for SendRefiner {}

/// TUI adapter for publishing tool activity lines.
struct CursiveToolActivityAdapter {
    cb_sink: cursive::CbSink,
}

impl ToolActivityPort for CursiveToolActivityAdapter {
    fn publish_tool_activity(&self, call: &ToolCall) {
        let tool_name = call.name.clone();
        let detail = crate::adapters::tool_ui::summarize_tool_args(&call.arguments);

        let cb = self.cb_sink.clone();
        let _ = cb.send(Box::new(move |siv: &mut Cursive| {
            view::push_tool_activity(siv, &tool_name, &detail);
        }));
    }
}

/// TUI adapter for interactive tool approval prompts.
struct CursiveToolApprovalAdapter {
    cb_sink: cursive::CbSink,
}

impl ToolApprovalPort for CursiveToolApprovalAdapter {
    fn request_tool_approval(&self, call: &ToolCall) -> Result<bool> {
        let (title, description, preview) = crate::adapters::tool_ui::build_approval_text(call);

        let (confirm_tx, confirm_rx) = mpsc::channel::<bool>();
        let cb = self.cb_sink.clone();
        let _ = cb.send(Box::new(move |siv: &mut Cursive| {
            view::show_tool_confirmation(siv, &title, &description, &preview, confirm_tx);
        }));

        // Block the engine thread until the user responds.
        Ok(confirm_rx.recv().unwrap_or(false))
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

    let refiner: Box<dyn Refiner> = match config.refiner.mode.as_str() {
        "rules" => Box::new(RuleRefiner::new()),
        _ => Box::new(NoopRefiner),
    };

    let engine = build_engine(&agent_id, &agent_config)?;

    // Capture engine metadata for slash commands (before moving engine to thread)
    let engine_info = EngineInfo {
        context_window: engine.context_window(),
        diagnostics: engine.diagnostics(),
    };

    let _flow_store = FlowStore::new(&resolve_tengu_home())?;
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
        system_prompt::build_system_prompt(&agent_config, advertise_workspace_tools, &[]);

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

    // Spawn engine thread
    let cb_sink = siv.cb_sink().clone();
    let send_engine = SendEngine(engine);
    let send_refiner = SendRefiner(refiner);
    let engine_agent_id = agent_id.clone();
    let engine_agent_config = agent_config.clone();
    let secret_registry_clone = Arc::clone(&secret_registry);

    std::thread::spawn(move || {
        let secret_registry = secret_registry_clone;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Failed to create tokio runtime for engine thread");

        let engine = send_engine.0;
        let refiner = send_refiner.0;
        let flow_store = match FlowStore::new(&resolve_tengu_home()) {
            Ok(fs) => fs,
            Err(e) => {
                let _ = cb_sink.send(Box::new(move |siv: &mut Cursive| {
                    view::hide_thinking(siv);
                    view::push_bubble(siv, BubbleRole::System, &format!("Flow store error: {}", e));
                }));
                return;
            }
        };

        // Resolve workspace path (expand tilde)
        let workspace: Option<PathBuf> = engine_agent_config
            .workspace
            .as_ref()
            .map(|p| workspace_tools::expand_tilde(p));

        // Build memory subsystem if enabled.
        let memory_handle = channel_runtime::build_memory_handle(&memory_config, &rt);

        // Base workspace tools (built-in + memory, without skills).
        let uses_tools =
            engine.supports_tool_use() && !engine.manages_own_workspace() && workspace.is_some();
        let has_memory = memory_handle.is_some();
        let agent_capabilities = parse_capability_set(&engine_agent_config.capabilities)
            .expect("agent capabilities should validate");

        let mut base_tools = channel_runtime::compute_base_tools(
            uses_tools,
            has_memory,
            &engine_agent_config.capabilities,
        );

        // Skill registry — initialized and loaded once, hot-reloaded each turn.
        let skill_source: Option<FileSystemSkillSource> = workspace
            .as_ref()
            .map(|ws| FileSystemSkillSource::new(ws.clone()));

        let base_reserved: Vec<String> = base_tools.iter().map(|t| t.def.name.clone()).collect();
        let mut skill_registry = SkillRegistry::new(base_reserved)
            .with_allowlist(Some(engine_agent_config.skill_packages.clone()));

        if let Some(ref src) = skill_source {
            skill_registry.reload(src);
        }

        let mut skill_command_router = SkillCommandRouter::from_registry(&skill_registry);

        // Create channel-specific port adapters once, share via Arc.
        let approval: Arc<dyn ToolApprovalPort> = Arc::new(CursiveToolApprovalAdapter {
            cb_sink: cb_sink.clone(),
        });
        let activity: Arc<dyn ToolActivityPort> = Arc::new(CursiveToolActivityAdapter {
            cb_sink: cb_sink.clone(),
        });

        let mut tools_dirty = true;
        let mut current_tools: Vec<RegisteredTool> = vec![];
        let mut current_executor: Option<channel_runtime::ToolServiceExecutor> = None;
        let mut current_system_prompt = system_prompt;

        let mut runtime_state = channel_runtime::create_chat_loop_state(&engine_agent_config);

        // Create MemoryService from handle if available.
        let memory_service_instance = memory_handle
            .as_ref()
            .map(|h| MemoryService::new(h.embedding.as_ref(), h.store.as_ref()));

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
                        if let Some(ref handle) = memory_handle {
                            match rt.block_on(handle.store.clear_all()) {
                                Ok(()) => lines.push("Persistent memory purged.".to_string()),
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
                            &engine_agent_config.capabilities,
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
                            current_tools = channel_runtime::rebuild_tools(
                                &base_tools,
                                &skill_registry,
                                &agent_capabilities,
                            );
                            current_executor = channel_runtime::build_tool_executor(
                                ws,
                                &current_tools,
                                &skill_registry,
                                &memory_handle,
                                &secret_registry,
                                Arc::clone(&approval),
                                Arc::clone(&activity),
                                None,
                            );
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
                                current_tools = channel_runtime::rebuild_tools(
                                    &base_tools,
                                    &skill_registry,
                                    &agent_capabilities,
                                );
                                current_executor = channel_runtime::build_tool_executor(
                                    ws,
                                    &current_tools,
                                    &skill_registry,
                                    &memory_handle,
                                    &secret_registry,
                                    Arc::clone(&approval),
                                    Arc::clone(&activity),
                                    None,
                                );
                                current_system_prompt = channel_runtime::rebuild_system_prompt(
                                    &engine_agent_config,
                                    advertise_workspace_tools,
                                    &skill_registry,
                                    &current_tools,
                                );
                            }
                            tools_dirty = false;
                        }

                        // Process as a chat turn with the injected prompt.
                        rt.block_on(async {
                            let sanitized_executor = current_executor.as_ref().map(|e| {
                                SanitizedToolExecutor::new(e as &dyn ToolExecutor, &secret_registry)
                            });
                            let tool_defs = channel_runtime::tool_defs(&current_tools);

                            let chat_runtime = ChatRuntimeService {
                                engine: engine.as_ref(),
                                refiner: refiner.as_ref(),
                                flow_store: &flow_store,
                                agent_id: &engine_agent_id,
                                agent_config: &engine_agent_config,
                                history_turn_limit,
                                compaction_policy,
                                system_prompt: current_system_prompt.clone(),
                                tools: &tool_defs,
                                tool_executor: sanitized_executor
                                    .as_ref()
                                    .map(|e| e as &dyn ToolExecutor),
                                memory_service: memory_service_instance.as_ref(),
                                max_recall_entries: memory_config.max_recall_entries,
                                max_recall_tokens: memory_config.max_recall_tokens,
                                tool_observer: None,
                                cancel: None,
                            };

                            match chat_runtime
                                .process_user_text(&mut runtime_state, &injected)
                                .await
                            {
                                Ok(res) => {
                                    let memory_stats = if let Some(ref h) = memory_handle {
                                        let count = h.store.entry_count().await;
                                        let bytes = h.store.storage_bytes().await;
                                        Some((count, bytes))
                                    } else {
                                        None
                                    };
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
                    let result = chat_commands::handle_chat_command(
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
                            current_tools = channel_runtime::rebuild_tools(
                                &base_tools,
                                &skill_registry,
                                &agent_capabilities,
                            );
                            current_executor = channel_runtime::build_tool_executor(
                                ws,
                                &current_tools,
                                &skill_registry,
                                &memory_handle,
                                &secret_registry,
                                Arc::clone(&approval),
                                Arc::clone(&activity),
                                None,
                            );
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

                    rt.block_on(async {
                        // Wrap tool executor with secret redaction decorator.
                        let sanitized_executor = current_executor.as_ref().map(|e| {
                            SanitizedToolExecutor::new(e as &dyn ToolExecutor, &secret_registry)
                        });
                        let tool_defs = channel_runtime::tool_defs(&current_tools);

                        let chat_runtime = ChatRuntimeService {
                            engine: engine.as_ref(),
                            refiner: refiner.as_ref(),
                            flow_store: &flow_store,
                            agent_id: &engine_agent_id,
                            agent_config: &engine_agent_config,
                            history_turn_limit,
                            compaction_policy,
                            system_prompt: current_system_prompt.clone(),
                            tools: &tool_defs,
                            tool_executor: sanitized_executor
                                .as_ref()
                                .map(|e| e as &dyn ToolExecutor),
                            memory_service: memory_service_instance.as_ref(),
                            max_recall_entries: memory_config.max_recall_entries,
                            max_recall_tokens: memory_config.max_recall_tokens,
                            tool_observer: None,
                            cancel: None,
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
                            }) => {
                                let memory_stats = if let Some(ref h) = memory_handle {
                                    let count = h.store.entry_count().await;
                                    let bytes = h.store.storage_bytes().await;
                                    Some((count, bytes))
                                } else {
                                    None
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
