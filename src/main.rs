//! Tengu binary entry point and CLI runtime orchestration.
//!
//! Potential use case:
//! Run one command (`tengu chat`) to execute ingest, budgeting, model call, and response delivery.
//!
//! Architecture notes:
//! - Adapter-first integration boundaries come from `tengu-core` traits (`Engine`, `Pipe`, `Refiner`, `Tool`).
//! - Runtime execution is event-driven today through channel queues and `StreamEvent`.
//! - Internal domain event bus migration (`E11`) is in progress; `DomainEvent`/`EventBus`
//!   contracts and bounded in-process bus are implemented, and side-effects will
//!   move to subscribers incrementally.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use futures::StreamExt;
use std::path::PathBuf;
use tracing::{debug, error, info, warn};

mod flow_store;
mod tool_audit;
mod tool_runtime;

use flow_store::{FlowStore, FlowStoreIntegrityReport};
use tengu_backends::{AnthropicEngine, ClaudeCodeEngine, OllamaEngine, OpenAIEngine};
use tengu_channels::CliPipe;
use tengu_core::config::{ensure_engine_allowed, evaluate_tool_policy, Config, RuntimeProfile};
use tengu_core::token::estimate_tokens_approx_min1;
use tengu_core::types::{DeliveryOptions, Message, Recipient, Role};
use tengu_core::{Engine, EngineContext, Lens, Pipe, PipeContext, Refiner, ToolContext};
use tengu_memory::{KnowledgeStore, RetrievedKnowledge};
use tengu_optimizer::{NoopRefiner, RuleRefiner};
use tool_audit::{audit_now_epoch_s, truncate_audit_text, ToolAuditEvent, ToolAuditStore};
use tool_runtime::ToolRegistry;

/// Fixed heading prepended to runtime retrieval context blocks.
const RETRIEVAL_CONTEXT_HEADER: &str = "Relevant workspace context:\n\n";
/// Separator between multiple retrieval hits packed into one block.
const RETRIEVAL_CONTEXT_SEPARATOR: &str = "\n\n---\n\n";

#[derive(Parser)]
#[command(name = "tengu")]
#[command(about = "Model-agnostic AI agent hub. Single binary, zero dependencies.")]
#[command(version)]
/// CLI entry arguments for runtime command dispatch.
struct Cli {
    /// Selected subcommand (`chat` by default).
    #[command(subcommand)]
    command: Option<Commands>,

    /// Optional explicit config path (otherwise `$TENGU_HOME/config.toml`).
    #[arg(short, long)]
    config: Option<PathBuf>,
}

#[derive(Subcommand)]
/// Top-level runtime commands.
enum Commands {
    /// Run interactive chat loop.
    Chat,
    /// Planned daemon/hub runtime.
    Serve,
    /// Print static runtime status snapshot.
    Status,
    /// Run runtime/environment diagnostics.
    Doctor,
}

#[derive(Debug, Clone, Default)]
struct PromptAssemblyReport {
    /// Tokens contributed by static system/workspace prompt.
    system_tokens: usize,
    /// Tokens contributed by retrieval block injected for this request.
    retrieval_tokens: usize,
    /// Requested retrieval bucket before final budget arbitration.
    retrieval_budget_requested: usize,
    /// Effective retrieval bucket after history/system usage.
    retrieval_budget_effective: usize,
    /// Tokens contributed by selected recent history suffix.
    history_tokens: usize,
    /// Count of history messages excluded by budget/window.
    dropped_history_messages: usize,
    /// Count of retrieval hits excluded by retrieval block packer.
    dropped_retrieval_items: usize,
    /// Reserved tokens kept for model output.
    reserved_output_tokens: usize,
    /// Engine output cap used to derive the reserve for this turn.
    output_token_cap: usize,
    /// Total available input budget for this turn.
    total_input_budget: usize,
    /// Remaining flow token budget before executing the request.
    flow_budget_remaining: usize,
    /// Whether compaction executed before assembling this prompt.
    compaction_applied: bool,
    /// Number of messages compacted in current turn (if any).
    compacted_messages: usize,
}

#[derive(Debug, Clone, Default)]
struct HistoryAssembly {
    /// Selected history suffix kept for current prompt.
    messages: Vec<Message>,
    /// Token estimate used by selected history.
    used_tokens: usize,
    /// Count of history messages dropped by budgeting/windowing.
    dropped_messages: usize,
}

#[derive(Debug, Clone, Default)]
struct RetrievalAssembly {
    /// Optional formatted retrieval block merged into system prompt.
    block: Option<String>,
    /// Token estimate used by selected retrieval entries.
    used_tokens: usize,
    /// Retrieval entries dropped during packing.
    dropped_items: usize,
}

#[derive(Debug, Clone, Copy)]
struct FlowCompactionPolicy {
    /// Flow token usage threshold that triggers compaction.
    threshold_tokens: u64,
    /// Number of recent user turns to preserve verbatim.
    keep_turns: usize,
    /// Maximum token budget for generated summary block.
    summary_max_tokens: u32,
}

#[derive(Debug, Clone, Copy, Default)]
struct CompactionOutcome {
    /// Whether compaction was executed in this step.
    applied: bool,
    /// Number of old messages replaced by summary.
    compacted_messages: usize,
}

/// In-flight tool call assembly state for streamed tool arguments.
#[derive(Debug, Clone)]
struct PendingToolCall {
    id: String,
    name: String,
    arguments_delta: String,
}

/// Normalized tool execution outcome used by runtime rendering and audit logging.
#[derive(Debug, Clone)]
struct ToolExecutionOutcome {
    user_message: String,
    status: &'static str,
    reason: Option<String>,
    result_preview: Option<String>,
}

/// Mutable per-session runtime state for chat loop execution.
#[derive(Debug, Clone)]
struct ChatLoopState {
    /// In-memory conversation messages for active flow key.
    messages: Vec<Message>,
    /// Currently loaded flow key.
    active_flow_key: Option<String>,
    /// Manual rotation key set by `/reset`.
    manual_session_id: Option<String>,
    /// Approximate total tokens accumulated in active flow.
    flow_token_usage: u64,
    /// Active prompt lens for retrieval behavior.
    active_lens: Lens,
    /// Session-level input token total.
    total_input_tokens: u32,
    /// Session-level output token total.
    total_output_tokens: u32,
    /// Approximate tokens saved by refiner compression.
    tokens_saved: u32,
    /// Last prompt assembly report surfaced by `/context`.
    last_prompt_report: Option<PromptAssemblyReport>,
}

impl ChatLoopState {
    /// Rotate to a new manual session and clear in-memory flow/tokens.
    fn reset_for_new_session(&mut self) {
        self.manual_session_id = Some(uuid::Uuid::new_v4().to_string());
        self.active_flow_key = None;
        self.messages.clear();
        self.flow_token_usage = 0;
        self.total_input_tokens = 0;
        self.total_output_tokens = 0;
        self.tokens_saved = 0;
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("tengu=info".parse().unwrap()),
        )
        .compact()
        .init();

    let cli = Cli::parse();

    // Load config
    let config_path = cli
        .config
        .unwrap_or_else(|| resolve_tengu_home().join("config.toml"));

    let config = if config_path.exists() {
        Config::load(&config_path)
            .with_context(|| format!("Failed to load config at {}", config_path.display()))?
    } else {
        info!(
            path = %config_path.display(),
            "Config file not found; using built-in defaults"
        );
        let config = Config::default();
        config.validate().with_context(|| {
            "Built-in default config failed validation; this is a runtime bug".to_string()
        })?;
        config
    };

    // Detect runtime profile
    let profile = RuntimeProfile::resolve(Some(&config.runtime_profile));

    match cli.command.unwrap_or(Commands::Chat) {
        Commands::Chat => run_chat(config, profile).await,
        Commands::Serve => {
            // TODO(epic-hub-runtime): Implement long-running daemon mode with pipe multiplexing,
            // auth, and graceful shutdown semantics.
            info!("Hub daemon not yet implemented (Phase 8)");
            Ok(())
        }
        Commands::Status => {
            print_status(&config, profile);
            Ok(())
        }
        Commands::Doctor => {
            run_doctor(&config).await;
            Ok(())
        }
    }
}

/// Run interactive chat runtime for the configured default agent.
///
/// Current implemented path is CLI + selected engine + optional refiner + flow store.
///
/// Execution phases:
/// 1) Resolve agent/refiner/engine and connect CLI pipe.
/// 2) Receive inbound turns and process slash commands.
/// 3) Apply refiner + flow persistence + budget/retrieval assembly.
/// 4) Execute engine turn and persist assistant output.
///
/// Note:
/// This function is intentionally transitional and will be decomposed further as
/// internal event bus subscribers are introduced for audit/metrics/policy side-effects.
async fn run_chat(config: Config, profile: RuntimeProfile) -> Result<()> {
    // Resolve default agent
    let (agent_id, agent_config) = config
        .agents
        .iter()
        .find(|(_, ac)| ac.default)
        .or_else(|| config.agents.iter().next())
        .map(|(id, ac)| (id.clone(), ac.clone()))
        .expect("No agents configured");

    // Create refiner based on config
    let refiner: Box<dyn Refiner> = match config.refiner.mode.as_str() {
        "rules" => Box::new(RuleRefiner::new()),
        "off" => Box::new(NoopRefiner),
        _ => Box::new(NoopRefiner),
    };

    // Create engine
    let engine = build_engine(&agent_id, &agent_config)?;

    // Print startup banner
    print_banner(
        &agent_id,
        &agent_config,
        profile,
        &config.refiner.mode,
        engine.as_ref(),
    );

    // Create CLI pipe
    let pipe = CliPipe::new();
    let (tx, mut rx) = tokio::sync::mpsc::channel(32);
    pipe.connect(PipeContext { inbound_tx: tx }).await?;

    let flow_store = FlowStore::new(&resolve_tengu_home())?;
    let knowledge_store = init_knowledge_store(&agent_config, refiner.as_ref()).await;
    let tool_registry = ToolRegistry::with_defaults();
    let tool_audit = match ToolAuditStore::new(&resolve_tengu_home()) {
        Ok(store) => Some(store),
        Err(err) => {
            warn!(error = %err, "Tool audit store unavailable; continuing without audit persistence");
            None
        }
    };

    let history_turn_limit = resolve_history_turn_limit(&agent_config.flow);
    let compaction_policy = resolve_flow_compaction_policy(
        &agent_config.flow,
        agent_config.limits.max_tokens_per_flow,
        engine.context_window(),
        engine.max_output_tokens_per_turn() as usize,
    );
    let engine_output_token_cap = engine.max_output_tokens_per_turn() as usize;
    let mut state = ChatLoopState {
        messages: Vec::new(),
        active_flow_key: None,
        manual_session_id: None,
        flow_token_usage: 0,
        active_lens: agent_config.default_lens.parse().unwrap_or(Lens::Eco),
        total_input_tokens: 0,
        total_output_tokens: 0,
        tokens_saved: 0,
        last_prompt_report: None,
    };

    // Build system prompt from workspace files with static token caps.
    let system_prompt = build_system_prompt(&agent_config);

    println!("Type your message (Ctrl+D to quit):\n");

    while let Some(inbound) = rx.recv().await {
        let original_len = inbound.content.len();

        // Handle slash commands
        if inbound.content.starts_with('/')
            && handle_chat_command(
                inbound.content.as_str(),
                &mut state,
                engine.as_ref(),
                &agent_config,
                history_turn_limit,
                compaction_policy,
            )
        {
            continue;
        }

        // Apply refiner
        let compressed = refiner.compress(&inbound.content).await?;
        let compressed_len = compressed.len();
        if compressed_len < original_len {
            let saved = ((original_len - compressed_len) / 4) as u32;
            state.tokens_saved += saved;
        }

        let flow_key = resolve_runtime_flow_key(
            &agent_id,
            &agent_config.flow.scope,
            &inbound.sender,
            state.manual_session_id.as_deref(),
        );
        if state.active_flow_key.as_deref() != Some(flow_key.as_str()) {
            state.messages = flow_store
                .load_messages(&flow_key, history_load_message_cap(history_turn_limit))?;
            let dropped = enforce_history_turn_limit(&mut state.messages, history_turn_limit);
            if dropped > 0 {
                info!(
                    flow_key = %flow_key,
                    dropped_messages = dropped,
                    history_turn_limit,
                    "Applied history turn limit to loaded flow messages"
                );
            }
            state.flow_token_usage = state
                .messages
                .iter()
                .map(|m| estimate_tokens_approx_min1(&m.content) as u64)
                .sum();
            state.active_flow_key = Some(flow_key.clone());
        }

        let user_message = Message {
            role: Role::User,
            content: compressed,
            tool_call_id: None,
            tool_calls: None,
        };
        flow_store.append_message(&flow_key, &agent_id, &user_message)?;
        state.flow_token_usage += estimate_tokens_approx_min1(&user_message.content) as u64;
        state.messages.push(user_message);
        enforce_history_turn_limit(&mut state.messages, history_turn_limit);
        let mut compaction_outcome = maybe_compact_flow(
            &flow_store,
            &flow_key,
            &agent_id,
            &mut state.messages,
            &mut state.flow_token_usage,
            &*refiner,
            compaction_policy,
            "pre-engine",
        )
        .await?;

        if state.flow_token_usage >= agent_config.limits.max_tokens_per_flow {
            println!(
                "Flow token limit reached ({}). Use /reset to start a new session.\n",
                agent_config.limits.max_tokens_per_flow
            );
            continue;
        }

        let remaining_flow_tokens = agent_config
            .limits
            .max_tokens_per_flow
            .saturating_sub(state.flow_token_usage);
        let base_input_budget = compute_base_input_budget(
            engine.context_window(),
            engine_output_token_cap,
            system_prompt.as_deref(),
            remaining_flow_tokens,
        );
        let history_assembly = assemble_recent_history(&state.messages, base_input_budget);
        let retrieval_budget_requested = compute_retrieval_bucket_budget(
            state.active_lens,
            &agent_config.lens,
            base_input_budget,
        );
        let retrieval_budget_effective = retrieval_budget_requested
            .min(base_input_budget.saturating_sub(history_assembly.used_tokens));

        let retrieved = if let Some(store) = knowledge_store.as_ref() {
            // TODO(epic-runtime-retrieval): Add periodic/incremental re-ingestion hooks.
            store.query_with_budget(
                &inbound.content,
                state.active_lens,
                6,
                retrieval_budget_effective as u32,
            )
        } else {
            Vec::new()
        };
        let retrieved_candidates = retrieved.len();
        let retrieval_assembly = build_retrieval_block(&retrieved, retrieval_budget_effective);

        let history_messages_selected = history_assembly.messages.len();
        let prompt_messages = history_assembly.messages;
        let system_tokens = system_prompt
            .as_deref()
            .map(estimate_tokens_approx_min1)
            .unwrap_or(0);
        let total_input_budget = compute_total_input_budget(
            engine.context_window(),
            engine_output_token_cap,
            remaining_flow_tokens,
        );
        let report = PromptAssemblyReport {
            system_tokens,
            retrieval_tokens: retrieval_assembly.used_tokens,
            retrieval_budget_requested,
            retrieval_budget_effective,
            history_tokens: history_assembly.used_tokens,
            dropped_history_messages: history_assembly.dropped_messages,
            dropped_retrieval_items: retrieval_assembly.dropped_items,
            reserved_output_tokens: reserved_output_tokens(
                engine.context_window(),
                engine_output_token_cap,
            ),
            output_token_cap: engine_output_token_cap,
            total_input_budget,
            flow_budget_remaining: remaining_flow_tokens as usize,
            compaction_applied: compaction_outcome.applied,
            compacted_messages: compaction_outcome.compacted_messages,
        };
        log_prompt_budget_report(
            &flow_key,
            state.active_lens,
            engine.context_window(),
            &report,
            history_messages_selected,
            retrieved_candidates,
        );
        state.last_prompt_report = Some(report);

        if prompt_messages.is_empty() {
            println!("Context budget exhausted. Use /reset to continue.\n");
            continue;
        }

        // Run engine
        let turn_system_prompt =
            merge_system_prompt(system_prompt.clone(), retrieval_assembly.block.as_deref());
        let context = EngineContext {
            workspace: agent_config.workspace.clone(),
            system_prompt: turn_system_prompt,
        };

        let response_text = collect_engine_response(
            engine.as_ref(),
            &prompt_messages,
            &context,
            &flow_key,
            &agent_id,
            &agent_config,
            &tool_registry,
            tool_audit.as_ref(),
            &mut state.total_input_tokens,
            &mut state.total_output_tokens,
        )
        .await?;

        if !response_text.is_empty() {
            // Send through pipe
            pipe.send_text(&inbound.sender, &response_text, &DeliveryOptions::default())
                .await?;

            let assistant_message = Message {
                role: Role::Assistant,
                content: response_text,
                tool_call_id: None,
                tool_calls: None,
            };
            flow_store.append_message(&flow_key, &agent_id, &assistant_message)?;
            state.flow_token_usage +=
                estimate_tokens_approx_min1(&assistant_message.content) as u64;
            state.messages.push(assistant_message);
            enforce_history_turn_limit(&mut state.messages, history_turn_limit);
            let post_outcome = maybe_compact_flow(
                &flow_store,
                &flow_key,
                &agent_id,
                &mut state.messages,
                &mut state.flow_token_usage,
                &*refiner,
                compaction_policy,
                "post-engine",
            )
            .await?;
            if post_outcome.applied {
                compaction_outcome.applied = true;
                compaction_outcome.compacted_messages = compaction_outcome
                    .compacted_messages
                    .saturating_add(post_outcome.compacted_messages);
            }
        }
    }

    pipe.disconnect().await?;
    Ok(())
}

/// Process one slash command and return `true` when loop should continue.
fn handle_chat_command(
    command: &str,
    state: &mut ChatLoopState,
    engine: &dyn Engine,
    agent_config: &tengu_core::config::AgentConfig,
    history_turn_limit: usize,
    compaction_policy: FlowCompactionPolicy,
) -> bool {
    match command {
        "/eco" => {
            state.active_lens = Lens::Eco;
            println!("Switched to eco lens (summaries only)\n");
            true
        }
        "/standard" => {
            state.active_lens = Lens::Standard;
            println!("Switched to standard lens (auto-expand)\n");
            true
        }
        "/precise" => {
            state.active_lens = Lens::Precise;
            println!("Switched to precise lens (full content)\n");
            true
        }
        "/cost" => {
            println!("Session Stats");
            println!("─────────────────────────────");
            println!(" Input tokens:  {}", state.total_input_tokens);
            println!(" Output tokens: {}", state.total_output_tokens);
            println!(
                " Total:         {}",
                state.total_input_tokens + state.total_output_tokens
            );
            if state.tokens_saved > 0 {
                println!();
                println!(" Saved by refiner:");
                println!("   Prompt compression: -{} tokens", state.tokens_saved);
            }
            println!();
            true
        }
        "/context" => {
            let used: usize = state
                .messages
                .iter()
                .map(|m| estimate_tokens_approx_min1(&m.content))
                .sum();
            let window = engine.context_window();
            let lens_name = state.active_lens.as_str();
            println!(
                "Context: ~{} / {} tokens ({}%)\n",
                used,
                window,
                (used * 100) / window.max(1)
            );
            println!("Lens: {}\n", lens_name);
            println!("History turn limit: {}\n", history_turn_limit);
            println!(
                "Compaction: threshold={} keep_turns={} summary_max_tokens={}\n",
                compaction_policy.threshold_tokens,
                compaction_policy.keep_turns,
                compaction_policy.summary_max_tokens
            );
            if let Some(report) = &state.last_prompt_report {
                println!("Last prompt assembly:");
                println!("  System:    {} tokens", report.system_tokens);
                println!("  Retrieval: {} tokens", report.retrieval_tokens);
                println!(
                    "    requested/effective: {}/{}",
                    report.retrieval_budget_requested, report.retrieval_budget_effective
                );
                println!("  History:   {} tokens", report.history_tokens);
                println!("    dropped messages: {}", report.dropped_history_messages);
                println!("    dropped retrieval: {}", report.dropped_retrieval_items);
                println!("  Reserved:  {} tokens", report.reserved_output_tokens);
                println!("  Output cap: {} tokens", report.output_token_cap);
                println!("  Budget:    {} tokens", report.total_input_budget);
                println!("  Flow left: {} tokens\n", report.flow_budget_remaining);
                println!(
                    "  Compaction: applied={}, compacted_messages={}\n",
                    report.compaction_applied, report.compacted_messages
                );
            }
            true
        }
        "/reset" => {
            state.reset_for_new_session();
            println!("Flow reset and rotated to a new session.\n");
            true
        }
        "/engine" => {
            let diagnostics = engine.diagnostics();
            let caps = &diagnostics.capabilities;
            println!("Current: {}/{}", agent_config.engine, agent_config.model);
            println!("Engine id: {}", diagnostics.engine_id);
            println!(
                "Configured model: {}",
                diagnostics.configured_model.as_deref().unwrap_or("n/a")
            );
            println!(
                "Endpoint: {}",
                diagnostics.endpoint.as_deref().unwrap_or("n/a")
            );
            println!(
                "Transport: {}",
                diagnostics.transport.as_deref().unwrap_or("n/a")
            );
            println!("Context window: {}\n", caps.context_window);
            println!("Output cap: {}\n", caps.max_output_tokens_per_turn);
            println!(
                "Capabilities: tools={}, streaming={}, manages_workspace={}\n",
                caps.supports_tool_use, caps.supports_streaming, caps.manages_own_workspace
            );
            true
        }
        "/help" => {
            println!("Commands:");
            println!("  /eco       — Eco lens (summaries)");
            println!("  /standard  — Standard lens (auto-expand)");
            println!("  /precise   — Precise lens (full content)");
            println!("  /engine    — Show current engine");
            println!("  /cost      — Token usage stats");
            println!("  /context   — Context window usage");
            println!("  /reset     — Clear conversation");
            println!("  /help      — This help\n");
            true
        }
        _ => {
            println!("Unknown command. Type /help for available commands.\n");
            true
        }
    }
}

/// Execute one engine call and collect text/usage events into session counters.
async fn collect_engine_response(
    engine: &dyn Engine,
    prompt_messages: &[Message],
    context: &EngineContext,
    flow_key: &str,
    agent_id: &str,
    agent_config: &tengu_core::config::AgentConfig,
    tool_registry: &ToolRegistry,
    tool_audit: Option<&ToolAuditStore>,
    total_input_tokens: &mut u32,
    total_output_tokens: &mut u32,
) -> Result<String> {
    match engine.run(prompt_messages, &[], context).await {
        Ok(mut stream) => {
            let mut response_text = String::new();
            let mut turn_usage_snapshot: Option<(u32, u32)> = None;
            let mut policy_terminal_message: Option<String> = None;
            let mut pending_tool_call: Option<PendingToolCall> = None;
            let mut tool_runtime_messages: Vec<String> = Vec::new();

            while let Some(event) = stream.next().await {
                match event {
                    tengu_core::types::StreamEvent::TextDelta { text } => {
                        response_text.push_str(&text);
                    }
                    tengu_core::types::StreamEvent::Usage {
                        input_tokens,
                        output_tokens,
                    } => {
                        absorb_turn_usage_snapshot(
                            &mut turn_usage_snapshot,
                            input_tokens,
                            output_tokens,
                        );
                    }
                    tengu_core::types::StreamEvent::Error { message } => {
                        eprintln!("Engine error: {}", message);
                    }
                    tengu_core::types::StreamEvent::ToolCallStart { id, name } => {
                        if pending_tool_call.is_some() {
                            append_tool_audit(
                                tool_audit,
                                ToolAuditEvent {
                                    ts_epoch_s: audit_now_epoch_s(),
                                    flow_key: flow_key.to_string(),
                                    agent_id: agent_id.to_string(),
                                    tool_call_id: id.clone(),
                                    tool_name: name.clone(),
                                    phase: "protocol".to_string(),
                                    status: "error".to_string(),
                                    reason: Some(
                                        "runtime supports only one active tool call".to_string(),
                                    ),
                                    arguments_preview: None,
                                    result_preview: None,
                                },
                            );
                            policy_terminal_message = Some(
                                "Tool runtime currently supports one active tool call at a time."
                                    .to_string(),
                            );
                            break;
                        }
                        let decision = evaluate_tool_policy(agent_config, &name);
                        if !decision.is_allowed() {
                            append_tool_audit(
                                tool_audit,
                                ToolAuditEvent {
                                    ts_epoch_s: audit_now_epoch_s(),
                                    flow_key: flow_key.to_string(),
                                    agent_id: agent_id.to_string(),
                                    tool_call_id: id.clone(),
                                    tool_name: name.clone(),
                                    phase: "policy".to_string(),
                                    status: "denied".to_string(),
                                    reason: Some(decision.reason().to_string()),
                                    arguments_preview: None,
                                    result_preview: None,
                                },
                            );
                            policy_terminal_message = Some(format!(
                                "Tool call '{}' denied by policy for agent '{}': {}",
                                name,
                                agent_id,
                                decision.reason()
                            ));
                            warn!(
                                agent_id,
                                tool = %name,
                                reason = decision.reason(),
                                "Tool call denied by runtime capability policy"
                            );
                            break;
                        }
                        append_tool_audit(
                            tool_audit,
                            ToolAuditEvent {
                                ts_epoch_s: audit_now_epoch_s(),
                                flow_key: flow_key.to_string(),
                                agent_id: agent_id.to_string(),
                                tool_call_id: id.clone(),
                                tool_name: name.clone(),
                                phase: "policy".to_string(),
                                status: "allowed".to_string(),
                                reason: None,
                                arguments_preview: None,
                                result_preview: None,
                            },
                        );
                        pending_tool_call = Some(PendingToolCall {
                            id,
                            name,
                            arguments_delta: String::new(),
                        });
                    }
                    tengu_core::types::StreamEvent::ToolCallDelta {
                        id,
                        arguments_delta,
                    } => {
                        if let Some(pending) = pending_tool_call.as_mut() {
                            if pending.id != id {
                                append_tool_audit(
                                    tool_audit,
                                    ToolAuditEvent {
                                        ts_epoch_s: audit_now_epoch_s(),
                                        flow_key: flow_key.to_string(),
                                        agent_id: agent_id.to_string(),
                                        tool_call_id: pending.id.clone(),
                                        tool_name: pending.name.clone(),
                                        phase: "protocol".to_string(),
                                        status: "error".to_string(),
                                        reason: Some(format!(
                                            "delta id mismatch: expected '{}', got '{}'",
                                            pending.id, id
                                        )),
                                        arguments_preview: Some(truncate_audit_text(
                                            &pending.arguments_delta,
                                            256,
                                        )),
                                        result_preview: None,
                                    },
                                );
                                policy_terminal_message = Some(format!(
                                    "Tool call delta id mismatch: expected '{}', got '{}'.",
                                    pending.id, id
                                ));
                                break;
                            }
                            pending.arguments_delta.push_str(&arguments_delta);
                        }
                    }
                    tengu_core::types::StreamEvent::ToolCallEnd { id } => {
                        if let Some(pending) = pending_tool_call.take() {
                            if pending.id != id {
                                append_tool_audit(
                                    tool_audit,
                                    ToolAuditEvent {
                                        ts_epoch_s: audit_now_epoch_s(),
                                        flow_key: flow_key.to_string(),
                                        agent_id: agent_id.to_string(),
                                        tool_call_id: pending.id.clone(),
                                        tool_name: pending.name.clone(),
                                        phase: "protocol".to_string(),
                                        status: "error".to_string(),
                                        reason: Some(format!(
                                            "end id mismatch: expected '{}', got '{}'",
                                            pending.id, id
                                        )),
                                        arguments_preview: Some(truncate_audit_text(
                                            &pending.arguments_delta,
                                            256,
                                        )),
                                        result_preview: None,
                                    },
                                );
                                policy_terminal_message = Some(format!(
                                    "Tool call end id mismatch: expected '{}', got '{}'.",
                                    pending.id, id
                                ));
                                break;
                            }
                            let outcome = execute_tool_call(
                                &pending,
                                tool_registry,
                                context.workspace.as_ref(),
                                agent_id,
                            )
                            .await;
                            append_tool_audit(
                                tool_audit,
                                ToolAuditEvent {
                                    ts_epoch_s: audit_now_epoch_s(),
                                    flow_key: flow_key.to_string(),
                                    agent_id: agent_id.to_string(),
                                    tool_call_id: pending.id.clone(),
                                    tool_name: pending.name.clone(),
                                    phase: "execute".to_string(),
                                    status: outcome.status.to_string(),
                                    reason: outcome.reason.clone(),
                                    arguments_preview: Some(truncate_audit_text(
                                        &pending.arguments_delta,
                                        256,
                                    )),
                                    result_preview: outcome.result_preview.clone(),
                                },
                            );
                            tool_runtime_messages.push(outcome.user_message);
                        }
                    }
                    tengu_core::types::StreamEvent::Done => {}
                    _ => {}
                }
            }
            apply_turn_usage_to_session_totals(
                total_input_tokens,
                total_output_tokens,
                turn_usage_snapshot,
            );
            if let Some(message) = policy_terminal_message {
                return Ok(message);
            }
            if let Some(pending) = pending_tool_call {
                append_tool_audit(
                    tool_audit,
                    ToolAuditEvent {
                        ts_epoch_s: audit_now_epoch_s(),
                        flow_key: flow_key.to_string(),
                        agent_id: agent_id.to_string(),
                        tool_call_id: pending.id.clone(),
                        tool_name: pending.name.clone(),
                        phase: "protocol".to_string(),
                        status: "error".to_string(),
                        reason: Some("missing ToolCallEnd".to_string()),
                        arguments_preview: Some(truncate_audit_text(&pending.arguments_delta, 256)),
                        result_preview: None,
                    },
                );
                return Ok(format!(
                    "Tool call '{}' did not complete (missing ToolCallEnd).",
                    pending.name
                ));
            }
            if !tool_runtime_messages.is_empty() {
                let tool_block = tool_runtime_messages.join("\n\n");
                if response_text.trim().is_empty() {
                    return Ok(tool_block);
                }
                response_text.push_str("\n\n");
                response_text.push_str(&tool_block);
            }
            Ok(response_text)
        }
        Err(err) => {
            eprintln!("Engine error: {}\n", err);
            Ok(String::new())
        }
    }
}

/// Execute one assembled tool call through registry and return user-facing result text.
async fn execute_tool_call(
    pending: &PendingToolCall,
    tool_registry: &ToolRegistry,
    workspace: Option<&PathBuf>,
    agent_id: &str,
) -> ToolExecutionOutcome {
    let args_value = if pending.arguments_delta.trim().is_empty() {
        serde_json::json!({})
    } else {
        match serde_json::from_str::<serde_json::Value>(&pending.arguments_delta) {
            Ok(parsed) => parsed,
            Err(err) => {
                return ToolExecutionOutcome {
                    user_message: format!(
                        "[tool:{} parse-error]\nInvalid JSON arguments: {}",
                        pending.name, err
                    ),
                    status: "parse_error",
                    reason: Some(err.to_string()),
                    result_preview: None,
                };
            }
        }
    };

    if !tool_registry.has(&pending.name) {
        return ToolExecutionOutcome {
            user_message: format!("[tool:{} error]\nTool is not registered.", pending.name),
            status: "not_registered",
            reason: Some("tool is not registered".to_string()),
            result_preview: None,
        };
    }

    let Some(workspace) = workspace else {
        return ToolExecutionOutcome {
            user_message: format!(
                "[tool:{} error]\nWorkspace is not configured for this agent.",
                pending.name
            ),
            status: "workspace_missing",
            reason: Some("workspace is not configured".to_string()),
            result_preview: None,
        };
    };

    let tool_ctx = ToolContext {
        workspace: workspace.clone(),
        agent_id: agent_id.to_string(),
    };
    match tool_registry
        .execute(&pending.name, args_value, &tool_ctx)
        .await
    {
        Ok(output) if output.is_error => ToolExecutionOutcome {
            user_message: format!("[tool:{} error]\n{}", pending.name, output.content),
            status: "error",
            reason: Some("tool returned error output".to_string()),
            result_preview: Some(truncate_audit_text(&output.content, 512)),
        },
        Ok(output) => ToolExecutionOutcome {
            user_message: format!("[tool:{} ok]\n{}", pending.name, output.content),
            status: "ok",
            reason: None,
            result_preview: Some(truncate_audit_text(&output.content, 512)),
        },
        Err(err) => ToolExecutionOutcome {
            user_message: format!("[tool:{} error]\n{}", pending.name, err),
            status: "exec_error",
            reason: Some(err.to_string()),
            result_preview: None,
        },
    }
}

/// Append tool audit event and keep runtime resilient on audit write failure.
fn append_tool_audit(store: Option<&ToolAuditStore>, event: ToolAuditEvent) {
    let Some(store) = store else {
        return;
    };
    if let Err(err) = store.append(&event) {
        warn!(
            error = %err,
            tool = %event.tool_name,
            phase = %event.phase,
            status = %event.status,
            "Failed to append tool audit event"
        );
    }
}

/// Emit per-request prompt budget telemetry grouped by prompt assembly bucket.
fn log_prompt_budget_report(
    flow_key: &str,
    lens: Lens,
    context_window: usize,
    report: &PromptAssemblyReport,
    history_messages_selected: usize,
    retrieval_candidates: usize,
) {
    info!(
        flow_key = %flow_key,
        lens = lens.as_str(),
        context_window,
        output_token_cap = report.output_token_cap,
        total_input_budget = report.total_input_budget,
        reserved_output_tokens = report.reserved_output_tokens,
        flow_budget_remaining = report.flow_budget_remaining,
        system_tokens = report.system_tokens,
        history_tokens = report.history_tokens,
        history_messages_selected,
        dropped_history_messages = report.dropped_history_messages,
        retrieval_tokens = report.retrieval_tokens,
        retrieval_candidates,
        retrieval_budget_requested = report.retrieval_budget_requested,
        retrieval_budget_effective = report.retrieval_budget_effective,
        dropped_retrieval_items = report.dropped_retrieval_items,
        compaction_applied = report.compaction_applied,
        compacted_messages = report.compacted_messages,
        "Prompt budget report"
    );
}

/// Keep the latest usage snapshot emitted during one model turn.
///
/// Contract: providers should emit cumulative per-turn usage in `StreamEvent::Usage`.
/// Runtime stores the latest snapshot and applies it once when the turn ends.
fn absorb_turn_usage_snapshot(
    snapshot: &mut Option<(u32, u32)>,
    input_tokens: u32,
    output_tokens: u32,
) {
    if let Some((prev_input, prev_output)) = *snapshot {
        if input_tokens < prev_input || output_tokens < prev_output {
            debug!(
                prev_input,
                prev_output,
                input_tokens,
                output_tokens,
                "Usage snapshot is non-monotonic; replacing with latest frame"
            );
        }
    }
    *snapshot = Some((input_tokens, output_tokens));
}

/// Apply one finalized turn usage snapshot to session-level cumulative totals.
fn apply_turn_usage_to_session_totals(
    total_input_tokens: &mut u32,
    total_output_tokens: &mut u32,
    turn_usage_snapshot: Option<(u32, u32)>,
) {
    if let Some((input_tokens, output_tokens)) = turn_usage_snapshot {
        *total_input_tokens = total_input_tokens.saturating_add(input_tokens);
        *total_output_tokens = total_output_tokens.saturating_add(output_tokens);
    }
}

/// Build bounded static system prompt from workspace identity/profile/context files.
fn build_system_prompt(agent_config: &tengu_core::config::AgentConfig) -> Option<String> {
    const MAX_FILE_TOKENS: usize = 1200;
    const MAX_TOTAL_TOKENS: usize = 2400;

    let workspace = agent_config.workspace.as_ref()?;
    let mut parts = Vec::new();
    let mut total_tokens = 0usize;

    // Load workspace files in order: IDENTITY.md, PROFILE.md, CONTEXT.md
    for filename in &["IDENTITY.md", "PROFILE.md", "CONTEXT.md"] {
        let path = workspace.join(filename);
        if let Ok(content) = std::fs::read_to_string(&path) {
            if !content.trim().is_empty() {
                let truncated = truncate_to_token_budget(&content, MAX_FILE_TOKENS);
                let chunk = format!("# {}\n\n{}", filename, truncated);
                let chunk_tokens = estimate_tokens_approx_min1(&chunk);
                if total_tokens + chunk_tokens > MAX_TOTAL_TOKENS {
                    break;
                }
                total_tokens += chunk_tokens;
                parts.push(chunk);
            }
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n---\n\n"))
    }
}

/// Assemble newest contiguous history suffix that fits the token budget.
fn assemble_recent_history(messages: &[Message], history_budget: usize) -> HistoryAssembly {
    const MAX_HISTORY_MESSAGES: usize = 120;

    if history_budget == 0 || messages.is_empty() {
        return HistoryAssembly::default();
    }

    let mut selected_rev: Vec<Message> = Vec::new();
    let mut used = 0usize;
    let start = messages.len().saturating_sub(MAX_HISTORY_MESSAGES);
    let recent = &messages[start..];

    for msg in recent.iter().rev() {
        let msg_tokens = estimate_tokens_approx_min1(&msg.content);
        if used + msg_tokens > history_budget {
            break;
        }
        used += msg_tokens;
        selected_rev.push(msg.clone());
    }

    selected_rev.reverse();
    HistoryAssembly {
        dropped_messages: recent.len().saturating_sub(selected_rev.len()),
        messages: selected_rev,
        used_tokens: used,
    }
}

/// Truncate string content using the shared `~4 chars/token` approximation.
fn truncate_to_token_budget(content: &str, max_tokens: usize) -> String {
    let max_chars = max_tokens.saturating_mul(4);
    if content.len() <= max_chars {
        content.to_string()
    } else {
        let mut truncated = content.chars().take(max_chars).collect::<String>();
        truncated.push_str("\n\n[truncated]");
        truncated
    }
}

/// Reserve output tokens from explicit engine output cap with safety headroom.
///
/// This avoids over-reserving on very large context windows while still keeping
/// room for provider overhead and streamed terminal frames.
fn reserved_output_tokens(context_window: usize, output_token_cap: usize) -> usize {
    if context_window == 0 {
        return 0;
    }

    let capped_output = output_token_cap.max(1).min(context_window);
    let headroom = (capped_output / 4).max(64);
    let adaptive_floor = (context_window / 50).clamp(64, 2_048);

    capped_output
        .saturating_add(headroom)
        .max(adaptive_floor)
        .min(context_window)
}

/// Compute input budget after output reserve and static system prompt footprint.
fn compute_base_input_budget(
    context_window: usize,
    output_token_cap: usize,
    system_prompt: Option<&str>,
    remaining_flow_tokens: u64,
) -> usize {
    let total_budget =
        compute_total_input_budget(context_window, output_token_cap, remaining_flow_tokens);
    let system_tokens = system_prompt.map(estimate_tokens_approx_min1).unwrap_or(0);
    total_budget.saturating_sub(system_tokens)
}

/// Compute maximum input budget before prompt-bucket allocation.
///
/// Output reserve is derived from effective output cap, not from context ratio.
fn compute_total_input_budget(
    context_window: usize,
    output_token_cap: usize,
    remaining_flow_tokens: u64,
) -> usize {
    let reserved_output = reserved_output_tokens(context_window, output_token_cap);
    context_window
        .saturating_sub(reserved_output)
        .min(remaining_flow_tokens as usize)
}

/// Compute retrieval bucket budget from lens settings and overall input budget.
fn compute_retrieval_bucket_budget(
    lens: Lens,
    lens_cfg: &tengu_core::config::LensConfig,
    base_input_budget: usize,
) -> usize {
    if base_input_budget == 0 {
        return 0;
    }

    let desired = match lens {
        Lens::Eco => lens_cfg.eco_max_tokens as usize,
        Lens::Standard => ((base_input_budget as f32) * 0.2) as usize,
        Lens::Precise => ((base_input_budget as f32) * lens_cfg.precise_budget) as usize,
    };
    let hard_cap = (base_input_budget / 2).max(32).min(base_input_budget);
    desired.min(hard_cap).max(32.min(hard_cap))
}

/// Build retrieval context block under a fixed token budget.
fn build_retrieval_block(hits: &[RetrievedKnowledge], max_tokens: usize) -> RetrievalAssembly {
    if hits.is_empty() || max_tokens == 0 {
        return RetrievalAssembly::default();
    }

    let header_tokens = estimate_tokens_approx_min1(RETRIEVAL_CONTEXT_HEADER);
    let separator_tokens = estimate_tokens_approx_min1(RETRIEVAL_CONTEXT_SEPARATOR);
    if header_tokens >= max_tokens {
        return RetrievalAssembly {
            block: None,
            used_tokens: 0,
            dropped_items: hits.len(),
        };
    }

    let mut chunks = Vec::new();
    let mut used = 0usize;
    let mut dropped = 0usize;

    for (idx, hit) in hits.iter().enumerate() {
        let section = format!(
            "[{} | {} | score {:.2}]\n{}",
            hit.source.display(),
            if hit.is_summary { "summary" } else { "full" },
            hit.score,
            hit.content
        );
        let section_tokens = estimate_tokens_approx_min1(&section);
        let additional_tokens = if chunks.is_empty() {
            header_tokens.saturating_add(section_tokens)
        } else {
            separator_tokens.saturating_add(section_tokens)
        };
        if additional_tokens > max_tokens {
            dropped += 1;
            continue;
        }
        if used.saturating_add(additional_tokens) > max_tokens {
            dropped += hits.len().saturating_sub(idx);
            break;
        }
        used = used.saturating_add(additional_tokens);
        chunks.push(section);
    }

    RetrievalAssembly {
        block: if chunks.is_empty() {
            None
        } else {
            Some(format!(
                "{}{}",
                RETRIEVAL_CONTEXT_HEADER,
                chunks.join(RETRIEVAL_CONTEXT_SEPARATOR)
            ))
        },
        used_tokens: if chunks.is_empty() { 0 } else { used },
        dropped_items: dropped,
    }
}

/// Format one compact engine diagnostics line for CLI status/doctor output.
fn format_engine_diagnostics_compact(diagnostics: &tengu_core::EngineDiagnostics) -> String {
    let caps = &diagnostics.capabilities;
    format!(
        "model={} endpoint={} transport={} context={} output_cap={} tools={} streaming={} workspace={}",
        diagnostics.configured_model.as_deref().unwrap_or("n/a"),
        diagnostics.endpoint.as_deref().unwrap_or("n/a"),
        diagnostics.transport.as_deref().unwrap_or("n/a"),
        caps.context_window,
        caps.max_output_tokens_per_turn,
        caps.supports_tool_use,
        caps.supports_streaming,
        caps.manages_own_workspace
    )
}

/// Run provider connectivity probe for diagnostics-capable engines.
///
/// Returns a human-readable probe status when probe logic exists for the engine.
async fn run_engine_probe(diagnostics: &tengu_core::EngineDiagnostics) -> Option<String> {
    match diagnostics.engine_id.as_str() {
        "ollama" => {
            let base_url = diagnostics
                .endpoint
                .as_deref()
                .unwrap_or("http://localhost:11434");
            let probe = reqwest::get(format!("{}/api/tags", base_url)).await;
            let status = match probe {
                Ok(resp) if resp.status().is_success() => "OK".to_string(),
                Ok(resp) => format!("Error: HTTP {}", resp.status()),
                Err(err) => format!("Unreachable: {}", err),
            };
            Some(format!("probe /api/tags... {}", status))
        }
        _ => None,
    }
}

/// Merge static system prompt with dynamic per-turn retrieval context.
fn merge_system_prompt(base: Option<String>, retrieval_block: Option<&str>) -> Option<String> {
    match (base, retrieval_block) {
        (None, None) => None,
        (Some(b), None) => Some(b),
        (None, Some(r)) => Some(r.to_string()),
        (Some(b), Some(r)) => Some(format!("{b}\n\n---\n\n{r}")),
    }
}

/// Initialize workspace knowledge store and run startup ingest patterns.
async fn init_knowledge_store(
    agent_config: &tengu_core::config::AgentConfig,
    refiner: &dyn Refiner,
) -> Option<KnowledgeStore> {
    let workspace = agent_config.workspace.clone()?;
    let mut store = KnowledgeStore::new(workspace);

    let mut patterns = agent_config.store.files.clone();
    patterns.extend(agent_config.store.extra_paths.clone());
    if patterns.is_empty() {
        return Some(store);
    }

    match store.ingest_patterns(&patterns, refiner).await {
        Ok(indexed) => info!(
            indexed_files = indexed,
            "Knowledge store ingested workspace files"
        ),
        Err(e) => error!(error = %e, "Knowledge store ingest failed"),
    }
    Some(store)
}

/// Build configured engine instance for one agent.
///
/// Provider-specific environment requirements:
/// - `ollama`: optional `OLLAMA_HOST` (defaults to local Ollama endpoint)
/// - `anthropic`: `ANTHROPIC_API_KEY`, optional `ANTHROPIC_BASE_URL`
/// - `openai`: `OPENAI_API_KEY`, optional `OPENAI_BASE_URL`
/// - `claude-code`: optional `CLAUDE_CODE_BIN` (defaults to `claude`)
///
/// Shared per-agent limit overrides:
/// - `agents.<id>.limits.context_window_override`
/// - `agents.<id>.limits.max_output_tokens_per_turn`
fn build_engine(
    agent_id: &str,
    agent_config: &tengu_core::config::AgentConfig,
) -> Result<Box<dyn Engine>> {
    // Runtime defense-in-depth: enforce policy even if config was loaded from
    // non-validating entry points.
    ensure_engine_allowed(agent_id, agent_config)?;

    let context_window_override = agent_config
        .limits
        .context_window_override
        .map(|value| value.max(1) as usize);
    let max_output_tokens_override = agent_config
        .limits
        .max_output_tokens_per_turn
        .map(|value| value.max(1));

    match agent_config.engine.as_str() {
        "ollama" => {
            let base_url = std::env::var("OLLAMA_HOST")
                .unwrap_or_else(|_| "http://localhost:11434".to_string());
            Ok(Box::new(OllamaEngine::new(
                &base_url,
                &agent_config.model,
                context_window_override,
                max_output_tokens_override,
            )))
        }
        "anthropic" => {
            let api_key = std::env::var("ANTHROPIC_API_KEY").map_err(|_| {
                anyhow::anyhow!("ANTHROPIC_API_KEY is required for anthropic engine")
            })?;
            let base_url = std::env::var("ANTHROPIC_BASE_URL")
                .unwrap_or_else(|_| "https://api.anthropic.com".to_string());
            Ok(Box::new(AnthropicEngine::new(
                &base_url,
                &agent_config.model,
                &api_key,
                context_window_override,
                max_output_tokens_override,
            )))
        }
        "openai" => {
            let api_key = std::env::var("OPENAI_API_KEY")
                .map_err(|_| anyhow::anyhow!("OPENAI_API_KEY is required for openai engine"))?;
            let base_url = std::env::var("OPENAI_BASE_URL")
                .unwrap_or_else(|_| "https://api.openai.com".to_string());
            Ok(Box::new(OpenAIEngine::new(
                &base_url,
                &agent_config.model,
                &api_key,
                context_window_override,
                max_output_tokens_override,
            )))
        }
        "claude-code" => Ok(Box::new(ClaudeCodeEngine::new(
            &agent_config.model,
            context_window_override,
            max_output_tokens_override,
        ))),
        other => {
            // TODO(epic-multi-engine): Add Google/HuggingFace backends and
            // runtime model switching with capability checks.
            error!(engine = %other, "Engine not yet implemented");
            Err(anyhow::anyhow!("Engine '{}' not yet implemented", other))
        }
    }
}

/// Print startup runtime banner.
fn print_banner(
    agent_id: &str,
    agent_config: &tengu_core::config::AgentConfig,
    profile: RuntimeProfile,
    refiner_mode: &str,
    engine: &dyn Engine,
) {
    let identity = agent_config.identity.name.as_deref().unwrap_or("Tengu");
    let history_turn_limit = resolve_history_turn_limit(&agent_config.flow);
    let compaction_policy = resolve_flow_compaction_policy(
        &agent_config.flow,
        agent_config.limits.max_tokens_per_flow,
        engine.context_window(),
        engine.max_output_tokens_per_turn() as usize,
    );
    println!();
    println!("  TENGU CLUSTER");
    println!("  ─────────────────────────────────────");
    println!("  Agent:    {} ({})", identity, agent_id);
    println!("  Engine:   {}/{}", agent_config.engine, agent_config.model);
    let diagnostics = engine.diagnostics();
    let caps = &diagnostics.capabilities;
    println!("  Context:  {} tokens", caps.context_window);
    println!(
        "  Output cap: {} tokens/turn",
        caps.max_output_tokens_per_turn
    );
    println!(
        "  Engine capabilities: tools={} streaming={} manages_workspace={}",
        caps.supports_tool_use, caps.supports_streaming, caps.manages_own_workspace
    );
    println!(
        "  Engine transport: endpoint={} transport={}",
        diagnostics.endpoint.as_deref().unwrap_or("n/a"),
        diagnostics.transport.as_deref().unwrap_or("n/a")
    );
    println!("  Refiner:  {}", refiner_mode);
    println!("  Lens:     {}", agent_config.default_lens);
    println!(
        "  Flow:     scope={}, history_turn_limit={}",
        agent_config.flow.scope, history_turn_limit
    );
    println!(
        "            compaction_threshold={} keep_turns={} summary_max_tokens={}",
        compaction_policy.threshold_tokens,
        compaction_policy.keep_turns,
        compaction_policy.summary_max_tokens
    );
    println!("  Profile:  {:?}", profile);
    println!("  ─────────────────────────────────────");
    println!();
}

/// Print compact runtime status snapshot.
fn print_status(config: &Config, profile: RuntimeProfile) {
    println!();
    println!("  TENGU CLUSTER — Status");
    println!("  ─────────────────────────────────────");
    println!("  Profile:  {:?}", profile);
    println!("  Refiner:  {}", config.refiner.mode);
    println!("  Agents:   {}", config.agents.len());
    for (id, ac) in &config.agents {
        println!(
            "    - {} ({}/{}){}",
            id,
            ac.engine,
            ac.model,
            if ac.default { " [default]" } else { "" }
        );
        match build_engine(id, ac) {
            Ok(engine) => {
                let diagnostics = engine.diagnostics();
                println!(
                    "      diagnostics: {}",
                    format_engine_diagnostics_compact(&diagnostics)
                );
            }
            Err(err) => {
                println!("      diagnostics: unavailable ({})", err);
            }
        }
    }
    println!("  Hub:      {}:{}", config.hub.bind, config.hub.port);
    println!("  ─────────────────────────────────────");
    println!();
}

/// Run environment diagnostics for configured runtimes.
async fn run_doctor(config: &Config) {
    println!();
    println!("  TENGU CLUSTER — Doctor");
    println!("  ─────────────────────────────────────");

    println!("  Backend diagnostics:");
    // Check backend metadata and provider reachability where probes exist.
    for (id, ac) in &config.agents {
        match build_engine(id, ac) {
            Ok(engine) => {
                let diagnostics = engine.diagnostics();
                println!(
                    "    {}: engine={} {}",
                    id,
                    diagnostics.engine_id,
                    format_engine_diagnostics_compact(&diagnostics)
                );

                let probe = run_engine_probe(&diagnostics)
                    .await
                    .unwrap_or_else(|| "probe: skipped (no provider probe configured)".to_string());
                println!("      {}", probe);
            }
            Err(err) => {
                println!("    {}: backend init error: {}", id, err);
            }
        }
    }

    let flow_store = FlowStore::new(&resolve_tengu_home());
    match flow_store {
        Ok(store) => {
            print!("  Flow store... ");
            match store.health_check() {
                Ok(_) => println!("OK"),
                Err(e) => {
                    println!("Error: {}", e);
                    println!("  ─────────────────────────────────────");
                    println!();
                    return;
                }
            }

            print!("  Flow integrity... ");
            match store.integrity_report() {
                Ok(report) if !report.has_issues() => {
                    println!("OK (checked {} flows)", report.checked_flows);
                }
                Ok(report) => {
                    println!("WARN");
                    print_flow_integrity_findings(&report);
                }
                Err(e) => println!("Error: {}", e),
            }
        }
        Err(e) => {
            print!("  Flow store... ");
            println!("Error: {}", e);
        }
    }

    println!("  ─────────────────────────────────────");
    println!();
}

/// Print actionable integrity findings from `FlowStore::integrity_report`.
fn print_flow_integrity_findings(report: &FlowStoreIntegrityReport) {
    println!("    checked flows: {}", report.checked_flows);
    print_findings("missing transcripts", &report.missing_transcripts);
    print_findings("unsafe transcript paths", &report.unsafe_transcript_paths);
    print_findings("unreadable transcripts", &report.unreadable_transcripts);
    print_findings("invalid transcript lines", &report.invalid_transcript_lines);
    print_findings(
        "index/transcript metadata mismatches",
        &report.metadata_mismatches,
    );
    println!("    guidance:");
    println!("      1) Back up `~/.tengu/state/flows`.");
    println!("      2) Inspect listed flow entries and transcript files.");
    println!("      3) Repair or remove broken flow entries from `index.json` if needed.");
}

/// Print at most three findings per category to keep `doctor` output readable.
fn print_findings(label: &str, entries: &[String]) {
    if entries.is_empty() {
        return;
    }
    println!("    {}: {}", label, entries.len());
    for entry in entries.iter().take(3) {
        println!("      - {}", entry);
    }
    if entries.len() > 3 {
        println!("      - ... and {} more", entries.len() - 3);
    }
}

/// Resolve Tengu home path from `TENGU_HOME` or `~/.tengu`.
fn resolve_tengu_home() -> PathBuf {
    if let Ok(home) = std::env::var("TENGU_HOME") {
        return PathBuf::from(home);
    }
    dirs_next::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".tengu")
}

/// Resolve deterministic flow key for the current inbound turn.
fn resolve_runtime_flow_key(
    agent_id: &str,
    flow_scope: &str,
    sender: &Recipient,
    manual_session_id: Option<&str>,
) -> String {
    FlowStore::resolve_flow_key(agent_id, flow_scope, sender, manual_session_id)
}

/// Resolve active history turn limit from flow config.
///
/// If config does not provide `max_history_turns`, runtime applies scope defaults:
/// - `main`: 160 turns
/// - `per-group`: 120 turns
/// - `per-pipe-sender`: 100 turns
/// - default (`per-sender`): 80 turns
fn resolve_history_turn_limit(flow: &tengu_core::config::FlowConfig) -> usize {
    flow.max_history_turns
        .map(|v| v.max(1) as usize)
        .unwrap_or_else(|| default_history_turn_limit_for_scope(&flow.scope))
}

/// Return scope-aware default history turn limit.
fn default_history_turn_limit_for_scope(scope: &str) -> usize {
    match scope {
        "main" => 160,
        "per-group" => 120,
        "per-pipe-sender" => 100,
        _ => 80,
    }
}

/// Compute transcript load cap for disk reads based on turn limit.
///
/// Uses a multiplier to account for assistant/tool messages around each user turn.
fn history_load_message_cap(history_turn_limit: usize) -> usize {
    history_turn_limit.saturating_mul(4).max(64)
}

/// Enforce max number of user turns in active in-memory history.
///
/// Returns number of dropped messages from the oldest side.
fn enforce_history_turn_limit(messages: &mut Vec<Message>, max_turns: usize) -> usize {
    if max_turns == 0 || messages.is_empty() {
        let dropped = messages.len();
        messages.clear();
        return dropped;
    }

    let mut seen_user_turns = 0usize;
    let mut start_index = None;

    for (idx, message) in messages.iter().enumerate().rev() {
        if matches!(message.role, Role::User) {
            seen_user_turns += 1;
            if seen_user_turns == max_turns {
                start_index = Some(idx);
                break;
            }
        }
    }

    let Some(start) = start_index else {
        return 0;
    };

    if start == 0 {
        return 0;
    }

    messages.drain(0..start);
    start
}

/// Resolve compaction policy from flow config and limits.
///
/// Summary defaults are derived from effective input budget (context minus reserve).
fn resolve_flow_compaction_policy(
    flow: &tengu_core::config::FlowConfig,
    max_tokens_per_flow: u64,
    context_window: usize,
    output_token_cap: usize,
) -> FlowCompactionPolicy {
    let threshold_ratio = flow
        .compaction_threshold_ratio
        .unwrap_or_else(|| default_compaction_threshold_ratio_for_scope(&flow.scope))
        .clamp(0.1, 1.0);
    let threshold_tokens = ((max_tokens_per_flow as f32) * threshold_ratio) as u64;

    let keep_turns = flow
        .compaction_keep_turns
        .map(|v| v.max(1) as usize)
        .unwrap_or_else(|| default_compaction_keep_turns_for_scope(&flow.scope));

    let max_input_budget =
        compute_total_input_budget(context_window, output_token_cap, max_tokens_per_flow);
    let summary_max_tokens = flow
        .compaction_summary_max_tokens
        .unwrap_or_else(|| default_compaction_summary_max_tokens(max_input_budget));

    FlowCompactionPolicy {
        threshold_tokens: threshold_tokens.max(1),
        keep_turns,
        summary_max_tokens,
    }
}

/// Derive default summary token budget from effective model input budget.
///
/// This is intentionally tied to context-window economics (not lifetime flow limits),
/// so defaults remain intuitive across small and large models.
fn default_compaction_summary_max_tokens(max_input_budget: usize) -> u32 {
    let target = ((max_input_budget as f32) * 0.15).round() as usize;
    let max_cap = (max_input_budget / 3).clamp(256, 4096);
    target.clamp(128, max_cap) as u32
}

/// Return scope-aware default compaction threshold ratio.
fn default_compaction_threshold_ratio_for_scope(scope: &str) -> f32 {
    match scope {
        "main" => 0.88,
        "per-group" => 0.86,
        "per-pipe-sender" => 0.84,
        _ => 0.82,
    }
}

/// Return scope-aware default number of recent user turns to keep verbatim.
fn default_compaction_keep_turns_for_scope(scope: &str) -> usize {
    match scope {
        "main" => 60,
        "per-group" => 40,
        "per-pipe-sender" => 32,
        _ => 24,
    }
}

/// Compact long-running flow history when threshold/overflow triggers are hit.
async fn maybe_compact_flow(
    flow_store: &FlowStore,
    flow_key: &str,
    agent_id: &str,
    messages: &mut Vec<Message>,
    flow_token_usage: &mut u64,
    refiner: &dyn Refiner,
    policy: FlowCompactionPolicy,
    phase: &str,
) -> Result<CompactionOutcome> {
    if messages.is_empty() {
        return Ok(CompactionOutcome::default());
    }

    let should_compact = *flow_token_usage >= policy.threshold_tokens;
    if !should_compact {
        return Ok(CompactionOutcome::default());
    }

    let Some(split_idx) = compaction_split_index(messages, policy.keep_turns) else {
        return Ok(CompactionOutcome::default());
    };

    if split_idx == 0 {
        return Ok(CompactionOutcome::default());
    }

    let compacted_slice = &messages[..split_idx];
    if !compacted_slice.iter().any(|m| matches!(m.role, Role::User)) {
        return Ok(CompactionOutcome::default());
    }
    let compaction_source = build_compaction_source(compacted_slice);
    let raw_summary = match refiner
        .summarize(&compaction_source, policy.summary_max_tokens)
        .await
    {
        Ok(text) if !text.trim().is_empty() => text,
        _ => truncate_to_token_budget(&compaction_source, policy.summary_max_tokens as usize),
    };
    let summary = truncate_to_token_budget(raw_summary.trim(), policy.summary_max_tokens as usize);

    let compacted_messages = compacted_slice.len();
    let summary_message = Message {
        role: Role::Assistant,
        content: format!(
            "[Flow compaction summary]\n{}\n\n[compacted_messages={}, phase={}]",
            summary.trim(),
            compacted_messages,
            phase
        ),
        tool_call_id: None,
        tool_calls: None,
    };

    messages.drain(0..split_idx);
    messages.insert(0, summary_message.clone());
    *flow_token_usage = messages
        .iter()
        .map(|m| estimate_tokens_approx_min1(&m.content) as u64)
        .sum();

    flow_store.append_message(flow_key, agent_id, &summary_message)?;
    info!(
        flow_key = %flow_key,
        phase,
        compacted_messages,
        flow_tokens = *flow_token_usage,
        threshold_tokens = policy.threshold_tokens,
        "Applied flow compaction summary"
    );

    Ok(CompactionOutcome {
        applied: true,
        compacted_messages,
    })
}

/// Find split index for compaction based on number of recent user turns to keep.
fn compaction_split_index(messages: &[Message], keep_turns: usize) -> Option<usize> {
    if keep_turns == 0 || messages.is_empty() {
        return Some(messages.len());
    }

    let mut seen_user_turns = 0usize;
    for (idx, message) in messages.iter().enumerate().rev() {
        if matches!(message.role, Role::User) {
            seen_user_turns += 1;
            if seen_user_turns == keep_turns {
                return Some(idx);
            }
        }
    }

    None
}

/// Build compaction source text from a slice of older messages.
fn build_compaction_source(messages: &[Message]) -> String {
    messages
        .iter()
        .map(|msg| format!("[{}] {}", role_label(&msg.role), msg.content))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Render canonical label for message role in compaction source text.
fn role_label(role: &Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn msg(content: &str) -> Message {
        Message {
            role: Role::User,
            content: content.to_string(),
            tool_call_id: None,
            tool_calls: None,
        }
    }

    fn assistant_msg(content: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: content.to_string(),
            tool_call_id: None,
            tool_calls: None,
        }
    }

    /// Build a deterministic retrieval hit fixture for retrieval budget tests.
    ///
    /// The helper defaults to `is_summary = true` and `score = 1.0` so tests
    /// can focus on token packing/drop behavior instead of ranking variance.
    fn hit(source: &str, content: &str) -> RetrievedKnowledge {
        RetrievedKnowledge {
            source: PathBuf::from(source),
            content: content.to_string(),
            is_summary: true,
            score: 1.0,
        }
    }

    #[test]
    fn history_drop_policy_keeps_newest_contiguous_suffix() {
        let messages = vec![
            msg(&"a".repeat(40)),
            msg(&"b".repeat(400)),
            msg(&"c".repeat(40)),
        ];
        let assembled = assemble_recent_history(&messages, 25);

        assert_eq!(assembled.messages.len(), 1);
        assert_eq!(assembled.messages[0].content, "c".repeat(40));
        assert_eq!(assembled.dropped_messages, 2);
    }

    #[test]
    fn retrieval_drop_policy_drops_tail_after_first_overflow() {
        let first = hit("a.md", &"alpha".repeat(24));
        let second = hit("b.md", &"beta".repeat(24));
        let first_section = format!(
            "[{} | {} | score {:.2}]\n{}",
            first.source.display(),
            "summary",
            first.score,
            first.content
        );
        let budget = estimate_tokens_approx_min1(RETRIEVAL_CONTEXT_HEADER)
            .saturating_add(estimate_tokens_approx_min1(&first_section))
            .saturating_add(1);

        let assembled = build_retrieval_block(&[first, second], budget);
        let block = assembled.block.unwrap_or_default();

        assert!(block.contains("a.md"));
        assert!(!block.contains("b.md"));
        assert_eq!(assembled.dropped_items, 1);
        assert!(assembled.used_tokens <= budget);
    }

    #[test]
    fn retrieval_drop_policy_skips_individually_oversized_entries() {
        let huge = hit("huge.md", &"x".repeat(2_000));
        let small = hit("small.md", &"y".repeat(160));
        let small_section = format!(
            "[{} | {} | score {:.2}]\n{}",
            small.source.display(),
            "summary",
            small.score,
            small.content
        );
        let budget = estimate_tokens_approx_min1(RETRIEVAL_CONTEXT_HEADER)
            .saturating_add(estimate_tokens_approx_min1(&small_section))
            .saturating_add(2);

        let assembled = build_retrieval_block(&[huge, small], budget);
        let block = assembled.block.unwrap_or_default();

        assert!(block.contains("small.md"));
        assert!(!block.contains("huge.md"));
        assert_eq!(assembled.dropped_items, 1);
        assert!(assembled.used_tokens <= budget);
    }

    #[test]
    fn budget_overflow_small_context_window_preserves_output_reserve() {
        // For tiny windows, capped output reserve can consume all available input.
        let total_input_budget = compute_total_input_budget(128, 1_024, 10_000);
        assert_eq!(total_input_budget, 0);
    }

    #[test]
    fn usage_accounting_keeps_latest_turn_snapshot() {
        let mut snapshot = None;
        absorb_turn_usage_snapshot(&mut snapshot, 120, 20);
        absorb_turn_usage_snapshot(&mut snapshot, 130, 30);

        assert_eq!(snapshot, Some((130, 30)));
    }

    #[test]
    fn usage_accounting_applies_turn_snapshot_once_to_session_totals() {
        let mut total_in = 100u32;
        let mut total_out = 40u32;
        apply_turn_usage_to_session_totals(&mut total_in, &mut total_out, Some((50, 10)));
        apply_turn_usage_to_session_totals(&mut total_in, &mut total_out, None);

        assert_eq!(total_in, 150);
        assert_eq!(total_out, 50);
    }

    #[test]
    fn budget_overflow_remaining_flow_tokens_hard_caps_input_budget() {
        let total_input_budget = compute_total_input_budget(8_192, 1_024, 500);
        assert_eq!(total_input_budget, 500);
    }

    #[test]
    fn budget_overflow_base_budget_saturates_when_system_prompt_is_too_large() {
        let large_system = "x".repeat(4_096);
        let base_input_budget = compute_base_input_budget(512, 1_024, Some(&large_system), 1_000);
        assert_eq!(base_input_budget, 0);
    }

    #[test]
    fn budget_overflow_large_context_uses_output_cap_aligned_reserve() {
        let reserve = reserved_output_tokens(1_047_576, 8_192);
        let total_input_budget = compute_total_input_budget(1_047_576, 8_192, 2_000_000);

        assert!(reserve < 20_000);
        assert!(total_input_budget > 1_000_000);
    }

    #[test]
    fn budget_overflow_retrieval_bucket_respects_half_input_hard_cap() {
        let lens_cfg = tengu_core::config::LensConfig {
            eco_max_tokens: 10_000,
            standard_threshold: 0.7,
            precise_budget: 0.9,
        };
        let budget = compute_retrieval_bucket_budget(Lens::Eco, &lens_cfg, 100);
        assert_eq!(budget, 50);
    }

    #[test]
    fn history_overflow_applies_recent_window_cap_even_with_large_budget() {
        let messages: Vec<Message> = (0..200).map(|i| msg(&format!("m{i}"))).collect();
        let assembled = assemble_recent_history(&messages, 100_000);
        assert_eq!(assembled.messages.len(), 120);
        // `dropped_messages` is tracked within the 120-message recent window.
        assert_eq!(assembled.dropped_messages, 0);
        assert_eq!(assembled.messages[0].content, "m80");
        assert_eq!(assembled.messages[119].content, "m199");
    }

    #[test]
    fn history_turn_limit_keeps_latest_user_turn_suffix() {
        let mut messages = vec![
            msg("u1"),
            assistant_msg("a1"),
            msg("u2"),
            assistant_msg("a2"),
            msg("u3"),
            assistant_msg("a3"),
        ];

        let dropped = enforce_history_turn_limit(&mut messages, 2);

        assert_eq!(dropped, 2);
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].content, "u2");
        assert_eq!(messages[3].content, "a3");
    }

    #[test]
    fn history_turn_limit_scope_defaults_are_stable() {
        assert_eq!(default_history_turn_limit_for_scope("main"), 160);
        assert_eq!(default_history_turn_limit_for_scope("per-group"), 120);
        assert_eq!(default_history_turn_limit_for_scope("per-pipe-sender"), 100);
        assert_eq!(default_history_turn_limit_for_scope("per-sender"), 80);
    }

    #[test]
    fn flow_config_override_history_turn_limit_takes_precedence() {
        let flow = tengu_core::config::FlowConfig {
            scope: "per-sender".to_string(),
            reset_mode: "idle".to_string(),
            idle_timeout_minutes: 30,
            max_history_turns: Some(42),
            compaction_threshold_ratio: None,
            compaction_keep_turns: None,
            compaction_summary_max_tokens: None,
        };

        assert_eq!(resolve_history_turn_limit(&flow), 42);
    }

    #[test]
    fn compaction_split_index_keeps_recent_turn_suffix() {
        let messages = vec![
            msg("u1"),
            assistant_msg("a1"),
            msg("u2"),
            assistant_msg("a2"),
            msg("u3"),
            assistant_msg("a3"),
        ];

        assert_eq!(compaction_split_index(&messages, 2), Some(2));
    }

    #[test]
    fn compaction_policy_defaults_are_scope_aware() {
        assert_eq!(default_compaction_keep_turns_for_scope("main"), 60);
        assert_eq!(default_compaction_keep_turns_for_scope("per-group"), 40);
        assert_eq!(
            default_compaction_keep_turns_for_scope("per-pipe-sender"),
            32
        );
        assert_eq!(default_compaction_keep_turns_for_scope("per-sender"), 24);

        assert!(default_compaction_threshold_ratio_for_scope("main") > 0.85);
        assert!(default_compaction_threshold_ratio_for_scope("per-sender") < 0.85);
    }

    #[test]
    fn resolve_compaction_policy_honors_overrides() {
        let flow = tengu_core::config::FlowConfig {
            scope: "per-sender".to_string(),
            reset_mode: "idle".to_string(),
            idle_timeout_minutes: 30,
            max_history_turns: Some(50),
            compaction_threshold_ratio: Some(0.9),
            compaction_keep_turns: Some(12),
            compaction_summary_max_tokens: Some(300),
        };

        let policy = resolve_flow_compaction_policy(&flow, 1_000, 8_192, 1_024);
        assert_eq!(policy.threshold_tokens, 900);
        assert_eq!(policy.keep_turns, 12);
        assert_eq!(policy.summary_max_tokens, 300);
    }

    #[test]
    fn compaction_summary_default_scales_with_context_budget() {
        let flow = tengu_core::config::FlowConfig {
            scope: "per-sender".to_string(),
            reset_mode: "idle".to_string(),
            idle_timeout_minutes: 30,
            max_history_turns: None,
            compaction_threshold_ratio: None,
            compaction_keep_turns: None,
            compaction_summary_max_tokens: None,
        };

        let small_ctx = resolve_flow_compaction_policy(&flow, 500_000, 8_192, 1_024);
        let large_ctx = resolve_flow_compaction_policy(&flow, 500_000, 128_000, 8_192);

        assert!(small_ctx.summary_max_tokens >= 128);
        assert!(small_ctx.summary_max_tokens < 1_500);
        assert!(large_ctx.summary_max_tokens > small_ctx.summary_max_tokens);
        assert!(large_ctx.summary_max_tokens <= 4_096);
    }

    #[tokio::test]
    async fn execute_tool_call_reports_unregistered_tool() {
        let registry = ToolRegistry::with_defaults();
        let pending = PendingToolCall {
            id: "tool-1".to_string(),
            name: "shell".to_string(),
            arguments_delta: "{\"command\":\"ls\"}".to_string(),
        };

        let outcome =
            execute_tool_call(&pending, &registry, Some(&PathBuf::from(".")), "main").await;
        assert_eq!(outcome.status, "not_registered");
        assert!(outcome.user_message.contains("not registered"));
    }

    #[tokio::test]
    async fn execute_tool_call_runs_read_file_tool() {
        let workspace =
            std::env::temp_dir().join(format!("tengu-main-tool-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).expect("create temp workspace");
        std::fs::write(workspace.join("note.txt"), "hello from tool").expect("write fixture file");

        let registry = ToolRegistry::with_defaults();
        let pending = PendingToolCall {
            id: "tool-2".to_string(),
            name: "read_file".to_string(),
            arguments_delta: "{\"path\":\"note.txt\"}".to_string(),
        };
        let outcome = execute_tool_call(&pending, &registry, Some(&workspace), "main").await;

        assert_eq!(outcome.status, "ok");
        assert!(outcome.user_message.contains("[tool:read_file ok]"));
        assert!(outcome.user_message.contains("hello from tool"));
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[test]
    fn tool_policy_denies_blocked_tool_name() {
        let mut config = tengu_core::config::Config::default();
        let agent = config.agents.get_mut("main").expect("main");
        agent.kit.allow.clear();
        agent.kit.deny = vec!["shell".to_string()];

        let decision = evaluate_tool_policy(agent, "shell");
        assert!(!decision.is_allowed());
    }
}
