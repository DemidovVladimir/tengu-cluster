//! Full-screen TUI runtime for interactive chat using cursive.

pub mod app;
pub mod view;

use anyhow::Result;
use cursive::backends::crossterm::crossterm::{event::DisableMouseCapture, execute};
use cursive::Cursive;
use std::path::PathBuf;
use std::sync::{mpsc, Arc};

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
use crate::application::chat_runtime::{ChatRuntimeService, ChatTurnResult};
use crate::application::engine_runtime::ToolExecutor;
use crate::application::flow_policy::resolve_flow_compaction_policy;
use crate::application::memory_service::MemoryService;
use crate::application::ports::{ToolActivityPort, ToolApprovalPort};
use crate::application::skill_registry::SkillRegistry;
use crate::application::tool_use_service::ToolUseService;
use crate::application::workspace_tools_catalog::build_workspace_tools;
use crate::application::workspace_tools_catalog::build_memory_tools;
#[cfg(feature = "evm")]
use crate::application::workspace_tools_catalog::build_evm_tools;
use crate::domain::chat::{resolve_history_turn_limit, ChatLoopState};
use crate::domain::skill::SkillStatus;
use crate::domain::tool_policy::ToolPolicyCatalog;
use crate::resolve_tengu_home;
use app::{BubbleRole, ChatRequest, SkillCommand};
use tengu_core::config::{Config, RuntimeProfile};
use tengu_core::types::{ToolCall, ToolDef};
use tengu_core::{Lens, Refiner};
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

/// Tool executor adapter for application engine runtime.
struct TuiToolExecutor {
    service: ToolUseService,
}

/// TUI adapter for publishing tool activity lines.
struct CursiveToolActivityAdapter {
    cb_sink: cursive::CbSink,
}

impl ToolActivityPort for CursiveToolActivityAdapter {
    fn publish_tool_activity(&self, call: &ToolCall) {
        let tool_name = call.name.clone();
        let detail = summarize_tool_args(&call.name, &call.arguments);

        let cb = self.cb_sink.clone();
        let _ = cb.send(Box::new(move |siv: &mut Cursive| {
            view::push_tool_activity(siv, &tool_name, &detail);
        }));
    }
}

/// Build a short human-readable summary of tool arguments for the activity line.
fn summarize_tool_args(tool_name: &str, args: &serde_json::Value) -> String {
    // Try well-known keys in priority order per tool type.
    let key = match tool_name {
        "run_command" => "cmd",
        "search_files" | "search" => "query",
        _ => "path",
    };

    if let Some(val) = args.get(key).and_then(|v| v.as_str()) {
        return truncate_detail(val, 120);
    }

    // Fallback: show all string arguments as key=value pairs.
    let obj = match args.as_object() {
        Some(m) if !m.is_empty() => m,
        _ => return String::new(),
    };

    let parts: Vec<String> = obj
        .iter()
        .filter_map(|(k, v)| {
            v.as_str().map(|s| format!("{}={}", k, truncate_detail(s, 60)))
        })
        .collect();
    parts.join(" ")
}

/// Truncate a display string, appending "…" if it exceeds the limit.
fn truncate_detail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// TUI adapter for interactive tool approval prompts.
struct CursiveToolApprovalAdapter {
    cb_sink: cursive::CbSink,
}

impl ToolApprovalPort for CursiveToolApprovalAdapter {
    fn request_tool_approval(&self, call: &ToolCall) -> Result<bool> {
        let (title, description, preview) = build_approval_dialog_content(call);

        let (confirm_tx, confirm_rx) = mpsc::channel::<bool>();
        let cb = self.cb_sink.clone();
        let _ = cb.send(Box::new(move |siv: &mut Cursive| {
            view::show_tool_confirmation(siv, &title, &description, &preview, confirm_tx);
        }));

        // Block the engine thread until the user responds.
        Ok(confirm_rx.recv().unwrap_or(false))
    }
}

/// Build title, description, and preview text for the tool approval dialog.
fn build_approval_dialog_content(call: &ToolCall) -> (String, String, String) {
    match call.name.as_str() {
        "run_command" => {
            let cmd = call
                .arguments
                .get("command")
                // Also check "cmd" — XML-parsed calls may use the shorter key.
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
                "Write Confirmation".to_string(),
                format!("Allow write to '{}'?", path),
                content.to_string(),
            )
        }
        _ => {
            // Generic fallback: show tool name and all arguments.
            let summary = summarize_tool_args(&call.name, &call.arguments);
            (
                "Tool Confirmation".to_string(),
                format!("Allow '{}' to run?", call.name),
                summary,
            )
        }
    }
}

impl ToolExecutor for TuiToolExecutor {
    fn execute(&self, call: &ToolCall) -> Result<String> {
        self.service.execute(call)
    }
}

// ---------------------------------------------------------------------------
// Helper: rebuild tools, executor, and system prompt from registry state.
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
    cb_sink: &cursive::CbSink,
) -> Option<TuiToolExecutor> {
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
    let skill_names: std::collections::HashSet<String> =
        skill_defs.iter().map(|s| s.name.clone()).collect();

    let skill_exec: Option<Arc<dyn crate::application::ports::ToolExecutionPort>> =
        if skill_defs.is_empty() {
            None
        } else {
            Some(Arc::new(SkillToolExecutionAdapter::new(
                skill_defs,
                Arc::clone(&shell),
                workspace.to_path_buf(),
            )))
        };

    let mut composite = CompositeToolExecutionAdapter::new(workspace_exec, skill_exec, skill_names);

    // Attach memory executor if available.
    if let Some(ref handle) = memory_handle {
        if let Ok(mem_exec) = MemoryToolExecutionAdapter::new(Arc::clone(handle)) {
            let mem_names: std::collections::HashSet<String> =
                build_memory_tools().iter().map(|t| t.name.clone()).collect();
            composite = composite.with_memory_executor(Arc::new(mem_exec), mem_names);
        }
    }

    // Attach EVM executor if env vars are set and feature is enabled.
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
                        let evm_names: std::collections::HashSet<String> =
                            build_evm_tools().iter().map(|t| t.name.clone()).collect();
                        composite = composite.with_evm_executor(Arc::new(evm_exec), evm_names);
                    }
                }
                Err(e) => tracing::warn!("EVM signer init failed: {e}"),
            }
        }
    }

    let composite = Arc::new(composite);

    let service = ToolUseService::new(
        ToolPolicyCatalog::from_tools(tools),
        Arc::new(CursiveToolActivityAdapter {
            cb_sink: cb_sink.clone(),
        }),
        Arc::new(CursiveToolApprovalAdapter {
            cb_sink: cb_sink.clone(),
        }),
        composite,
    );
    Some(TuiToolExecutor { service })
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

fn format_skill_list(skills: Vec<(String, SkillStatus)>) -> String {
    if skills.is_empty() {
        return "No skills discovered.".to_string();
    }
    let mut lines = vec!["Skills:".to_string()];
    for (name, status) in skills {
        let tag = match status {
            SkillStatus::Active => "active",
            SkillStatus::Inactive => "disabled",
        };
        lines.push(format!("  {} ({})", name, tag));
    }
    lines.join("\n")
}

/// Run the full-screen TUI chat (blocking — call from `block_in_place`).
pub fn run_tui(config: Config, _profile: RuntimeProfile) -> Result<()> {
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
    let system_prompt = system_prompt::build_system_prompt(
        &agent_config,
        advertise_workspace_tools,
        &[],
        false, // EVM tools not yet resolved — rebuilt on first turn
    );

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

    std::thread::spawn(move || {
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
                                    &dirs_next::home_dir()
                                        .unwrap_or_default()
                                        .to_string_lossy(),
                                );
                                DiskVectorMemoryStore::new(std::path::Path::new(&store_path_str))
                                    .ok()
                                    .map(|s| Arc::new(s) as Arc<dyn crate::application::ports::MemoryStorePort>)
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
                                    .map(|s| Arc::new(s) as Arc<dyn crate::application::ports::MemoryStorePort>)
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
        } else {
            None
        };

        // Base workspace tools (built-in + memory, without skills).
        let uses_tools =
            engine.supports_tool_use() && !engine.manages_own_workspace() && workspace.is_some();
        let has_memory = memory_handle.is_some();

        // Build base tool list + EVM availability from current env vars.
        // Extracted so `/reload` can recompute when env vars change at runtime.
        fn compute_base_tools(uses_tools: bool, has_memory: bool) -> (Vec<ToolDef>, bool) {
            if !uses_tools {
                return (vec![], false);
            }
            let mut all_tools = build_workspace_tools();
            if has_memory {
                all_tools.extend(build_memory_tools());
            }
            let evm_available;
            #[cfg(feature = "evm")]
            {
                evm_available = std::env::var("EVM_PRIVATE_KEY").is_ok()
                    && std::env::var("EVM_RPC_URL").is_ok();
                if evm_available {
                    all_tools.extend(build_evm_tools());
                }
            }
            #[cfg(not(feature = "evm"))]
            {
                evm_available = false;
            }
            (all_tools, evm_available)
        }

        let (mut base_tools, mut evm_tools_available) =
            compute_base_tools(uses_tools, has_memory);

        // Skill registry — initialized and loaded once, hot-reloaded each turn.
        let skill_source: Option<FileSystemSkillSource> =
            workspace.as_ref().map(|ws| FileSystemSkillSource::new(ws.clone()));

        let base_reserved: Vec<String> = base_tools.iter().map(|t| t.name.clone()).collect();
        let mut skill_registry = SkillRegistry::new(base_reserved);

        if let Some(ref src) = skill_source {
            skill_registry.reload(src);
        }

        // Transpile scan: detect foreign runtime deps, auto-prefer Rust variants,
        // generate scaffolds for skills that need transpilation.
        {
            let scan = skill_registry.transpile_scan();

            // Generate Rust scaffolds for skills without Rust variants.
            if let Some(ref ws) = workspace {
                let transpile_dir = ws.join(".tengu").join("transpiled");
                for report in &scan.needs_transpile {
                    let content =
                        crate::adapters::scaffold_writer::build_scaffold_content(report);
                    match crate::adapters::scaffold_writer::write_scaffold(
                        &content,
                        &transpile_dir,
                    ) {
                        Ok(path) => {
                            tracing::info!(
                                "Generated Rust scaffold for '{}' at {}",
                                report.skill_name,
                                path.display()
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                "Failed to generate scaffold for '{}': {}",
                                report.skill_name,
                                e
                            );
                        }
                    }
                }
            }

            if let Some(summary) =
                crate::application::skill_transpile::format_scan_summary(&scan)
            {
                let cb = cb_sink.clone();
                let _ = cb.send(Box::new(move |siv: &mut Cursive| {
                    view::push_bubble(siv, BubbleRole::System, &summary);
                }));
            }
        }

        let mut tools_dirty = true;
        let mut current_tools: Vec<ToolDef> = vec![];
        let mut current_executor: Option<TuiToolExecutor> = None;
        let mut current_system_prompt = system_prompt;

        let mut runtime_state = ChatLoopState {
            messages: Vec::new(),
            active_flow_key: None,
            manual_session_id: None,
            flow_token_usage: 0,
            active_lens: engine_agent_config
                .default_lens
                .parse()
                .unwrap_or(Lens::Eco),
            total_input_tokens: 0,
            total_output_tokens: 0,
            tokens_saved: 0,
            last_prompt_report: None,
        };

        // Create MemoryService from handle if available.
        let memory_service_instance = memory_handle.as_ref().map(|h| {
            MemoryService::new(h.embedding.as_ref(), h.store.as_ref())
        });

        while let Ok(request) = request_rx.recv() {
            match request {
                ChatRequest::SkillCommand {
                    command,
                    response_tx,
                } => {
                    let response = match command {
                        SkillCommand::List => format_skill_list(skill_registry.list_all()),
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
                        let (new_base, new_evm) =
                            compute_base_tools(uses_tools, has_memory);
                        let env_changed = new_evm != evm_tools_available
                            || new_base.len() != base_tools.len();
                        base_tools = new_base;
                        evm_tools_available = new_evm;
                        if env_changed {
                            lines.push("Environment refreshed.".to_string());
                        }

                        if evm_tools_available {
                            lines.push("EVM tools: active".to_string());
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
                            current_tools = rebuild_tools(&base_tools, &skill_registry);
                            current_executor = rebuild_executor(
                                ws,
                                &current_tools,
                                &skill_registry,
                                &memory_handle,
                                &cb_sink,
                            );
                            current_system_prompt = rebuild_system_prompt(
                                &engine_agent_config,
                                advertise_workspace_tools,
                                &skill_registry,
                                evm_tools_available,
                            );
                            tools_dirty = false;
                        }

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

                    let result = chat_commands::handle_chat_command(
                        &text,
                        &mut runtime_state,
                        &engine_info,
                        &engine_agent_config,
                        history_turn_limit,
                        compaction_policy,
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
                        }
                    }

                    // Rebuild tools/executor/prompt when dirty.
                    if tools_dirty {
                        if let Some(ref ws) = workspace {
                            current_tools = rebuild_tools(&base_tools, &skill_registry);
                            current_executor = rebuild_executor(
                                ws,
                                &current_tools,
                                &skill_registry,
                                &memory_handle,
                                &cb_sink,
                            );
                            current_system_prompt = rebuild_system_prompt(
                                &engine_agent_config,
                                advertise_workspace_tools,
                                &skill_registry,
                                evm_tools_available,
                            );
                        }
                        tools_dirty = false;
                    }

                    let text = user_text;

                    rt.block_on(async {
                        let chat_runtime = ChatRuntimeService {
                            engine: engine.as_ref(),
                            refiner: refiner.as_ref(),
                            flow_store: &flow_store,
                            agent_id: &engine_agent_id,
                            agent_config: &engine_agent_config,
                            history_turn_limit,
                            compaction_policy,
                            system_prompt: current_system_prompt.clone(),
                            tools: &current_tools,
                            tool_executor: current_executor.as_ref().map(|e| e as &dyn ToolExecutor),
                            memory_service: memory_service_instance.as_ref(),
                            max_recall_entries: memory_config.max_recall_entries,
                            max_recall_tokens: memory_config.max_recall_tokens,
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
                                    view::update_status(siv, total_input_tokens, total_output_tokens, memory_stats);
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
