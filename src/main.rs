//! Tengu binary entry point and CLI runtime orchestration.
//!
//! Potential use case:
//! Run one command (`tengu chat`) to execute ingest, budgeting, model call, and response delivery.
//!
//! Architecture notes:
//! - Adapter-first integration boundaries come from `tengu-core` traits (`Engine`, `Pipe`, `Refiner`, `Tool`).
//! - Runtime execution is event-driven today through channel queues and `StreamEvent`.
//! - Runtime orchestration is split by responsibility:
//!   - `src/main.rs`: bootstrap + high-level chat orchestration.
//!   - `src/runtime_bus.rs`: event bus config/subscribers/handoff workers.
//!   - `src/runtime_engine.rs`: engine turn + tool-call stream execution.
//!   - `src/runtime_commands.rs`: slash command and delegated control-plane handlers.
//!   - `src/runtime_prompt.rs`: budget/retrieval prompt assembly helpers.
//! - Internal domain event bus migration (`E11`) is in progress; `DomainEvent`/`EventBus`
//!   contracts, bounded in-process bus, runtime emitters, and audit/metrics/policy
//!   subscribers are implemented.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{debug, error, info, warn};

mod control_plane;
mod control_plane_audit;
mod flow_store;
#[cfg(test)]
mod main_tests;
mod runtime_bus;
mod runtime_commands;
mod runtime_engine;
mod runtime_prompt;
mod tool_audit;
mod tool_runtime;

use control_plane::{
    build_assignment_envelope, ensure_delegated_orchestrator, parse_assign_command,
    parse_unassign_command, CapabilityAssignmentRecord,
};
use control_plane_audit::ControlPlaneAuditStore;
use flow_store::{FlowStore, FlowStoreIntegrityReport};
use tengu_backends::{AnthropicEngine, ClaudeCodeEngine, OllamaEngine, OpenAIEngine};
use tengu_channels::CliPipe;
use tengu_core::config::{
    ensure_capability_governance_actor_allowed, ensure_engine_allowed, Config, RuntimeProfile,
};
use tengu_core::events::{
    DomainEvent, DomainEventMeta, DomainEventPayload, EngineTurnStarted, EventBus, FlowCompacted,
    FlowResolved, InProcessEventBus, InboundTurnReceived, PromptAssembled,
};
use tengu_core::token::estimate_tokens_approx_min1;
use tengu_core::types::{DeliveryOptions, Message, Recipient, Role};
use tengu_core::{Engine, EngineContext, Lens, Pipe, PipeContext, Refiner};
use tengu_memory::{KnowledgeStore, RetrievedKnowledge};
use tengu_optimizer::{NoopRefiner, RuleRefiner};
use tool_audit::ToolAuditStore;
use tool_runtime::ToolRegistry;

/// Fixed heading prepended to runtime retrieval context blocks.
const RETRIEVAL_CONTEXT_HEADER: &str = "Relevant workspace context:\n\n";
/// Separator between multiple retrieval hits packed into one block.
const RETRIEVAL_CONTEXT_SEPARATOR: &str = "\n\n---\n\n";
/// Default max rows retained in delegated assignment audit JSONL.
const CONTROL_PLANE_AUDIT_MAX_ROWS_DEFAULT: usize = 20_000;
/// Default recent rows scanned for startup assignment replay.
const CONTROL_PLANE_AUDIT_REPLAY_LIMIT_DEFAULT: usize = 512;
/// Default recent rows scanned for terminal delegated assignment reconciliation.
const CONTROL_PLANE_AUDIT_CLOSED_SCAN_LIMIT_DEFAULT: usize = 512;
/// Default delegated assignment lifetime before automatic runtime expiry cleanup.
const DELEGATED_ASSIGNMENT_TTL_SECS_DEFAULT: u64 = 900;

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
    /// Approved delegated capability assignments for current runtime session.
    capability_assignments: Vec<CapabilityAssignmentRecord>,
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
        self.capability_assignments.clear();
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
/// tool-loop and multi-channel runtime work (`E6`/`E8`) is completed.
async fn run_chat(config: Config, profile: RuntimeProfile) -> Result<()> {
    // Resolve default agent
    let (agent_id, agent_config) = config
        .agents
        .iter()
        .find(|(_, ac)| ac.default)
        .or_else(|| config.agents.iter().next())
        .map(|(id, ac)| (id.clone(), ac.clone()))
        .expect("No agents configured");

    // Runtime capability-governance guardrail:
    // in delegated mode only delegated orchestrator may request direct capabilities.
    ensure_capability_governance_actor_allowed(&config, &agent_id)
        .with_context(|| "Selected chat agent cannot request direct runtime capabilities")?;

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
    let event_bus_cfg = runtime_bus::resolve_event_bus_runtime_config(profile);
    let event_bus = Arc::new(InProcessEventBus::new(
        event_bus_cfg.capacity,
        event_bus_cfg.policy,
    ));
    info!(
        profile = ?profile,
        bus_capacity = event_bus_cfg.capacity,
        bus_policy = ?event_bus_cfg.policy,
        metrics_log_every = event_bus_cfg.metrics_log_every,
        diagnostics_interval_secs = event_bus_cfg.diagnostics_interval_secs,
        "Configured runtime event bus"
    );
    let tool_audit = match ToolAuditStore::new(&resolve_tengu_home()) {
        Ok(store) => Some(store),
        Err(err) => {
            warn!(error = %err, "Tool audit store unavailable; continuing without audit persistence");
            None
        }
    };
    let control_plane_audit = match ControlPlaneAuditStore::new(&resolve_tengu_home()) {
        Ok(store) => Some(store),
        Err(err) => {
            warn!(error = %err, "Control-plane audit store unavailable; continuing without assignment audit persistence");
            None
        }
    };
    let control_plane_audit_max_rows =
        resolve_positive_usize_env("TENGU_CONTROL_PLANE_AUDIT_MAX_ROWS")
            .unwrap_or(CONTROL_PLANE_AUDIT_MAX_ROWS_DEFAULT);
    let control_plane_replay_limit =
        resolve_positive_usize_env("TENGU_CONTROL_PLANE_AUDIT_REPLAY_LIMIT")
            .unwrap_or(CONTROL_PLANE_AUDIT_REPLAY_LIMIT_DEFAULT)
            .min(control_plane_audit_max_rows);
    let control_plane_closed_scan_limit =
        resolve_positive_usize_env("TENGU_CONTROL_PLANE_AUDIT_CLOSED_SCAN_LIMIT")
            .unwrap_or(CONTROL_PLANE_AUDIT_CLOSED_SCAN_LIMIT_DEFAULT)
            .min(control_plane_audit_max_rows);
    let delegated_assignment_ttl_secs =
        resolve_positive_u64_env("TENGU_DELEGATED_ASSIGNMENT_TTL_SECS")
            .unwrap_or(DELEGATED_ASSIGNMENT_TTL_SECS_DEFAULT);
    if let Some(store) = control_plane_audit.as_ref() {
        match store.prune_retain_last(control_plane_audit_max_rows) {
            Ok(dropped) if dropped > 0 => {
                info!(
                    dropped_rows = dropped,
                    retained_rows = control_plane_audit_max_rows,
                    "Pruned delegated assignment audit rows by retention policy"
                );
            }
            Ok(_) => {}
            Err(err) => {
                warn!(
                    error = %err,
                    retained_rows = control_plane_audit_max_rows,
                    "Failed to prune delegated assignment audit rows"
                );
            }
        }
    }
    let shared_config = Arc::new(config.clone());
    let mut tool_audit_subscriber =
        runtime_bus::spawn_tool_audit_subscriber(Arc::clone(&event_bus), tool_audit.clone());
    let mut control_plane_audit_subscriber = runtime_bus::spawn_control_plane_audit_subscriber(
        Arc::clone(&event_bus),
        control_plane_audit.clone(),
    );
    let mut delegated_handoff_acceptance_subscriber =
        if ensure_delegated_orchestrator(&config, &agent_id).is_ok() {
            info!(
                orchestrator_agent_id = %agent_id,
                "Enabled delegated handoff acceptance subscriber"
            );
            runtime_bus::spawn_delegated_handoff_acceptance_subscriber(
                Arc::clone(&event_bus),
                agent_id.clone(),
            )
        } else {
            None
        };
    let mut delegated_handoff_execution_subscriber =
        if ensure_delegated_orchestrator(&config, &agent_id).is_ok() {
            info!(
                orchestrator_agent_id = %agent_id,
                "Enabled delegated handoff execution subscriber"
            );
            runtime_bus::spawn_delegated_handoff_execution_subscriber(
                Arc::clone(&event_bus),
                Arc::clone(&shared_config),
                agent_id.clone(),
            )
        } else {
            None
        };
    let mut event_metrics_subscriber = runtime_bus::spawn_event_metrics_subscriber(
        Arc::clone(&event_bus),
        event_bus_cfg.metrics_log_every,
    );
    let mut policy_reaction_subscriber =
        runtime_bus::spawn_policy_reaction_subscriber(Arc::clone(&event_bus));
    let mut bus_diagnostics_reporter = runtime_bus::spawn_event_bus_diagnostics_reporter(
        Arc::clone(&event_bus),
        event_bus_cfg.diagnostics_interval_secs,
    );
    let persisted_capability_assignments = runtime_bus::load_persisted_capability_assignments(
        control_plane_audit.as_ref(),
        &agent_id,
        control_plane_replay_limit,
        delegated_assignment_ttl_secs,
    );
    if !persisted_capability_assignments.is_empty() {
        info!(
            agent_id = %agent_id,
            recovered_assignments = persisted_capability_assignments.len(),
            "Recovered delegated capability assignments from audit log"
        );
    }

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
        capability_assignments: persisted_capability_assignments,
        last_prompt_report: None,
    };

    // Build system prompt from workspace files with static token caps.
    let system_prompt = runtime_prompt::build_system_prompt(&agent_config);

    println!("Type your message (Ctrl+D to quit):\n");

    while let Some(inbound) = rx.recv().await {
        let turn_correlation_id = uuid::Uuid::new_v4().to_string();
        let closed_assignments = prune_closed_capability_assignments_from_audit(
            &mut state.capability_assignments,
            control_plane_audit.as_ref(),
            control_plane_closed_scan_limit,
        );
        if closed_assignments > 0 {
            info!(
                closed_assignments,
                scan_limit = control_plane_closed_scan_limit,
                "Removed terminal delegated assignments from active runtime state"
            );
        }
        let expired_assignments = runtime_commands::prune_expired_capability_assignments(
            &mut state.capability_assignments,
            delegated_assignment_ttl_secs,
            event_bus.as_ref(),
            &agent_id,
            &turn_correlation_id,
        )
        .await;
        if expired_assignments > 0 {
            info!(
                expired_assignments,
                ttl_secs = delegated_assignment_ttl_secs,
                "Expired delegated assignments by retention policy"
            );
        }
        emit_domain_event(
            event_bus.as_ref(),
            Some(&agent_id),
            None,
            Some(&turn_correlation_id),
            DomainEventPayload::InboundTurnReceived(InboundTurnReceived {
                pipe_id: inbound.sender.pipe_id.clone(),
                sender_id: format_recipient_identity(&inbound.sender),
                input_tokens: as_u32_saturating(estimate_tokens_approx_min1(&inbound.content)),
            }),
        )
        .await;

        let original_len = inbound.content.len();

        // Handle orchestrator control-plane commands before generic chat commands.
        if inbound.content.starts_with('/') {
            let command_flow_key = resolve_runtime_flow_key(
                &agent_id,
                &agent_config.flow.scope,
                &inbound.sender,
                state.manual_session_id.as_deref(),
            );
            if runtime_commands::handle_control_plane_command(
                inbound.content.as_str(),
                &config,
                &agent_id,
                &command_flow_key,
                &mut state,
                event_bus.as_ref(),
                &turn_correlation_id,
            )
            .await
            {
                continue;
            }
            if runtime_commands::handle_chat_command(
                inbound.content.as_str(),
                &mut state,
                engine.as_ref(),
                &agent_config,
                history_turn_limit,
                compaction_policy,
            ) {
                continue;
            }
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
        let switched_flow = state.active_flow_key.as_deref() != Some(flow_key.as_str());
        if switched_flow {
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
        let reused_existing = if switched_flow {
            !state.messages.is_empty()
        } else {
            true
        };
        emit_domain_event(
            event_bus.as_ref(),
            Some(&agent_id),
            Some(&flow_key),
            Some(&turn_correlation_id),
            DomainEventPayload::FlowResolved(FlowResolved {
                flow_key: flow_key.clone(),
                reused_existing,
            }),
        )
        .await;

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
        let pre_compaction_tokens_before = state.flow_token_usage;
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
        if compaction_outcome.applied {
            emit_domain_event(
                event_bus.as_ref(),
                Some(&agent_id),
                Some(&flow_key),
                Some(&turn_correlation_id),
                DomainEventPayload::FlowCompacted(FlowCompacted {
                    compacted_messages: as_u32_saturating(compaction_outcome.compacted_messages),
                    tokens_before: pre_compaction_tokens_before,
                    tokens_after: state.flow_token_usage,
                }),
            )
            .await;
        }

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
        let base_input_budget = runtime_prompt::compute_base_input_budget(
            engine.context_window(),
            engine_output_token_cap,
            system_prompt.as_deref(),
            remaining_flow_tokens,
        );
        let history_assembly =
            runtime_prompt::assemble_recent_history(&state.messages, base_input_budget);
        let retrieval_budget_requested = runtime_prompt::compute_retrieval_bucket_budget(
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
        let retrieval_assembly =
            runtime_prompt::build_retrieval_block(&retrieved, retrieval_budget_effective);

        let history_messages_selected = history_assembly.messages.len();
        let prompt_messages = history_assembly.messages;
        let system_tokens = system_prompt
            .as_deref()
            .map(estimate_tokens_approx_min1)
            .unwrap_or(0);
        let total_input_budget = runtime_prompt::compute_total_input_budget(
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
            reserved_output_tokens: runtime_prompt::reserved_output_tokens(
                engine.context_window(),
                engine_output_token_cap,
            ),
            output_token_cap: engine_output_token_cap,
            total_input_budget,
            flow_budget_remaining: remaining_flow_tokens as usize,
            compaction_applied: compaction_outcome.applied,
            compacted_messages: compaction_outcome.compacted_messages,
        };
        runtime_prompt::log_prompt_budget_report(
            &flow_key,
            state.active_lens,
            engine.context_window(),
            &report,
            history_messages_selected,
            retrieved_candidates,
        );
        emit_domain_event(
            event_bus.as_ref(),
            Some(&agent_id),
            Some(&flow_key),
            Some(&turn_correlation_id),
            DomainEventPayload::PromptAssembled(PromptAssembled {
                system_tokens: as_u32_saturating(report.system_tokens),
                retrieval_tokens: as_u32_saturating(report.retrieval_tokens),
                history_tokens: as_u32_saturating(report.history_tokens),
                reserved_output_tokens: as_u32_saturating(report.reserved_output_tokens),
                total_input_budget: as_u32_saturating(report.total_input_budget),
            }),
        )
        .await;
        state.last_prompt_report = Some(report);

        if prompt_messages.is_empty() {
            println!("Context budget exhausted. Use /reset to continue.\n");
            continue;
        }

        // Run engine
        let turn_system_prompt = runtime_prompt::merge_system_prompt(
            system_prompt.clone(),
            retrieval_assembly.block.as_deref(),
        );
        let context = EngineContext {
            workspace: agent_config.workspace.clone(),
            system_prompt: turn_system_prompt,
        };
        emit_domain_event(
            event_bus.as_ref(),
            Some(&agent_id),
            Some(&flow_key),
            Some(&turn_correlation_id),
            DomainEventPayload::EngineTurnStarted(EngineTurnStarted {
                engine_id: engine.id().to_string(),
                model_id: agent_config.model.clone(),
            }),
        )
        .await;

        let response_text = runtime_engine::collect_engine_response(
            engine.as_ref(),
            &config,
            &prompt_messages,
            &context,
            &flow_key,
            &agent_id,
            &agent_config,
            &tool_registry,
            event_bus.as_ref(),
            &turn_correlation_id,
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
            let post_compaction_tokens_before = state.flow_token_usage;
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
                emit_domain_event(
                    event_bus.as_ref(),
                    Some(&agent_id),
                    Some(&flow_key),
                    Some(&turn_correlation_id),
                    DomainEventPayload::FlowCompacted(FlowCompacted {
                        compacted_messages: as_u32_saturating(post_outcome.compacted_messages),
                        tokens_before: post_compaction_tokens_before,
                        tokens_after: state.flow_token_usage,
                    }),
                )
                .await;
                compaction_outcome.applied = true;
                compaction_outcome.compacted_messages = compaction_outcome
                    .compacted_messages
                    .saturating_add(post_outcome.compacted_messages);
            }
        }
    }

    pipe.disconnect().await?;
    if let Some(handle) = tool_audit_subscriber.take() {
        handle.abort();
    }
    if let Some(handle) = control_plane_audit_subscriber.take() {
        handle.abort();
    }
    if let Some(handle) = delegated_handoff_acceptance_subscriber.take() {
        handle.abort();
    }
    if let Some(handle) = delegated_handoff_execution_subscriber.take() {
        handle.abort();
    }
    if let Some(handle) = event_metrics_subscriber.take() {
        handle.abort();
    }
    if let Some(handle) = policy_reaction_subscriber.take() {
        handle.abort();
    }
    if let Some(handle) = bus_diagnostics_reporter.take() {
        handle.abort();
    }
    Ok(())
}

/// Remove active assignments that already reached terminal audit state.
///
/// Terminal statuses: `completed`, `failed`, `denied`, `revoked`, `expired`.
fn prune_closed_capability_assignments_from_audit(
    assignments: &mut Vec<CapabilityAssignmentRecord>,
    store: Option<&ControlPlaneAuditStore>,
    scan_limit: usize,
) -> usize {
    if assignments.is_empty() || scan_limit == 0 {
        return 0;
    }
    let Some(store) = store else {
        return 0;
    };
    let Ok(events) = store.read_recent(scan_limit) else {
        return 0;
    };

    let closed_handoffs = events
        .into_iter()
        .filter(|event| {
            matches!(
                event.status.as_str(),
                "completed" | "failed" | "denied" | "revoked" | "expired"
            )
        })
        .map(|event| event.handoff_id)
        .collect::<std::collections::HashSet<_>>();
    if closed_handoffs.is_empty() {
        return 0;
    }

    let before = assignments.len();
    assignments.retain(|assignment| !closed_handoffs.contains(&assignment.handoff_id));
    before.saturating_sub(assignments.len())
}

/// Emit one runtime domain event without affecting user-visible flow on failure.
pub(crate) async fn emit_domain_event(
    event_bus: &dyn EventBus,
    agent_id: Option<&str>,
    flow_key: Option<&str>,
    correlation_id: Option<&str>,
    payload: DomainEventPayload,
) {
    let event = DomainEvent {
        meta: DomainEventMeta {
            ts_epoch_ms: now_epoch_ms(),
            flow_key: flow_key.map(ToOwned::to_owned),
            agent_id: agent_id.map(ToOwned::to_owned),
            correlation_id: correlation_id.map(ToOwned::to_owned),
            source: Some("chat-runtime".to_string()),
        },
        payload,
    };
    if let Err(err) = event_bus.publish(event).await {
        warn!(error = %err, "Failed to publish runtime domain event");
    }
}

/// Convert usize counters to a saturating `u32` payload-safe value.
pub(crate) fn as_u32_saturating(value: usize) -> u32 {
    value.min(u32::MAX as usize) as u32
}

/// Build deterministic sender identity for domain events.
fn format_recipient_identity(sender: &Recipient) -> String {
    let mut parts = vec![sender.pipe_id.clone(), sender.peer_id.clone()];
    if let Some(thread_id) = sender.thread_id.as_deref() {
        parts.push(thread_id.to_string());
    }
    parts.join(":")
}

/// Return current unix epoch timestamp in milliseconds.
pub(crate) fn now_epoch_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Keep the latest usage snapshot emitted during one model turn.
///
/// Contract: providers should emit cumulative per-turn usage in `StreamEvent::Usage`.
/// Runtime stores the latest snapshot and applies it once when the turn ends.
pub(crate) fn absorb_turn_usage_snapshot(
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
pub(crate) fn apply_turn_usage_to_session_totals(
    total_input_tokens: &mut u32,
    total_output_tokens: &mut u32,
    turn_usage_snapshot: Option<(u32, u32)>,
) {
    if let Some((input_tokens, output_tokens)) = turn_usage_snapshot {
        *total_input_tokens = total_input_tokens.saturating_add(input_tokens);
        *total_output_tokens = total_output_tokens.saturating_add(output_tokens);
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
pub(crate) fn build_engine(
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

/// Parse positive integer environment override used for runtime retention limits.
fn resolve_positive_usize_env(var_name: &str) -> Option<usize> {
    let raw = std::env::var(var_name).ok()?;
    let parsed = raw.trim().parse::<usize>().ok()?;
    (parsed > 0).then_some(parsed)
}

/// Parse positive `u64` environment override for runtime TTL values.
fn resolve_positive_u64_env(var_name: &str) -> Option<u64> {
    let raw = std::env::var(var_name).ok()?;
    let parsed = raw.trim().parse::<u64>().ok()?;
    (parsed > 0).then_some(parsed)
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

    let max_input_budget = runtime_prompt::compute_total_input_budget(
        context_window,
        output_token_cap,
        max_tokens_per_flow,
    );
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
        _ => runtime_prompt::truncate_to_token_budget(
            &compaction_source,
            policy.summary_max_tokens as usize,
        ),
    };
    let summary = runtime_prompt::truncate_to_token_budget(
        raw_summary.trim(),
        policy.summary_max_tokens as usize,
    );

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
