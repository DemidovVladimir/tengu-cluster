//! Tengu binary entry point and CLI runtime orchestration.
//!
//! Potential use case:
//! Run one command (`tengu chat`) to execute ingest, budgeting, model call, and response delivery.

use anyhow::Result;
use clap::{Parser, Subcommand};
use futures::StreamExt;
use std::path::PathBuf;
use tracing::{error, info};

mod flow_store;

use flow_store::{FlowStore, FlowStoreIntegrityReport};
use tengu_backends::OllamaEngine;
use tengu_channels::CliPipe;
use tengu_core::config::{Config, RuntimeProfile};
use tengu_core::token::estimate_tokens_approx_min1;
use tengu_core::types::{DeliveryOptions, Message, Recipient, Role};
use tengu_core::{Engine, EngineContext, Lens, Pipe, PipeContext, Refiner};
use tengu_memory::{KnowledgeStore, RetrievedKnowledge};
use tengu_optimizer::{NoopRefiner, RuleRefiner};

#[derive(Parser)]
#[command(name = "tengu")]
#[command(about = "Model-agnostic AI agent hub. Single binary, zero dependencies.")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    #[arg(short, long)]
    config: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Commands {
    Chat,
    Serve,
    Status,
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
    messages: Vec<Message>,
    used_tokens: usize,
    dropped_messages: usize,
}

#[derive(Debug, Clone, Default)]
struct RetrievalAssembly {
    block: Option<String>,
    used_tokens: usize,
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

    let config = Config::load_or_default(&config_path);

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
/// Current implemented path is CLI + Ollama + optional refiner + flow store.
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
    let engine: Box<dyn Engine> = match agent_config.engine.as_str() {
        "ollama" => {
            let base_url = std::env::var("OLLAMA_HOST")
                .unwrap_or_else(|_| "http://localhost:11434".to_string());
            Box::new(OllamaEngine::new(&base_url, &agent_config.model))
        }
        other => {
            // TODO(epic-multi-engine): Add Anthropic/OpenAI/HuggingFace backends and
            // runtime model switching with capability checks.
            error!(engine = %other, "Engine not yet implemented");
            return Err(anyhow::anyhow!("Engine '{}' not yet implemented", other));
        }
    };

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

    // Conversation state for the currently active flow.
    let mut messages: Vec<Message> = Vec::new();
    let mut active_flow_key: Option<String> = None;
    let mut manual_session_id: Option<String> = None;
    let mut flow_token_usage: u64 = 0;
    let mut active_lens = agent_config.default_lens.parse().unwrap_or(Lens::Eco);
    let history_turn_limit = resolve_history_turn_limit(&agent_config.flow);
    let compaction_policy = resolve_flow_compaction_policy(
        &agent_config.flow,
        agent_config.limits.max_tokens_per_flow,
        engine.context_window(),
    );
    let mut total_input_tokens: u32 = 0;
    let mut total_output_tokens: u32 = 0;
    let mut tokens_saved: u32 = 0;
    let mut last_prompt_report: Option<PromptAssemblyReport> = None;

    // Build system prompt from workspace files with static token caps.
    let system_prompt = build_system_prompt(&agent_config);

    println!("Type your message (Ctrl+D to quit):\n");

    while let Some(inbound) = rx.recv().await {
        let original_len = inbound.content.len();

        // Handle slash commands
        if inbound.content.starts_with('/') {
            match inbound.content.as_str() {
                "/eco" => {
                    active_lens = Lens::Eco;
                    println!("Switched to eco lens (summaries only)\n");
                    continue;
                }
                "/standard" => {
                    active_lens = Lens::Standard;
                    println!("Switched to standard lens (auto-expand)\n");
                    continue;
                }
                "/precise" => {
                    active_lens = Lens::Precise;
                    println!("Switched to precise lens (full content)\n");
                    continue;
                }
                "/cost" => {
                    println!("Session Stats");
                    println!("─────────────────────────────");
                    println!(" Input tokens:  {}", total_input_tokens);
                    println!(" Output tokens: {}", total_output_tokens);
                    println!(
                        " Total:         {}",
                        total_input_tokens + total_output_tokens
                    );
                    if tokens_saved > 0 {
                        println!();
                        println!(" Saved by refiner:");
                        println!("   Prompt compression: -{} tokens", tokens_saved);
                    }
                    println!();
                    continue;
                }
                "/context" => {
                    let used: usize = messages
                        .iter()
                        .map(|m| estimate_tokens_approx_min1(&m.content))
                        .sum();
                    let window = engine.context_window();
                    let lens_name = active_lens.as_str();
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
                    if let Some(report) = &last_prompt_report {
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
                        println!("  Budget:    {} tokens", report.total_input_budget);
                        println!("  Flow left: {} tokens\n", report.flow_budget_remaining);
                        println!(
                            "  Compaction: applied={}, compacted_messages={}\n",
                            report.compaction_applied, report.compacted_messages
                        );
                    }
                    continue;
                }
                "/reset" => {
                    manual_session_id = Some(uuid::Uuid::new_v4().to_string());
                    active_flow_key = None;
                    messages.clear();
                    flow_token_usage = 0;
                    total_input_tokens = 0;
                    total_output_tokens = 0;
                    tokens_saved = 0;
                    println!("Flow reset and rotated to a new session.\n");
                    continue;
                }
                "/engine" => {
                    println!("Current: {}/{}", agent_config.engine, agent_config.model);
                    println!("Context window: {}\n", engine.context_window());
                    continue;
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
                    continue;
                }
                _ => {
                    println!("Unknown command. Type /help for available commands.\n");
                    continue;
                }
            }
        }

        // Apply refiner
        let compressed = refiner.compress(&inbound.content).await?;
        let compressed_len = compressed.len();
        if compressed_len < original_len {
            let saved = ((original_len - compressed_len) / 4) as u32;
            tokens_saved += saved;
        }

        let flow_key = resolve_runtime_flow_key(
            &agent_id,
            &agent_config.flow.scope,
            &inbound.sender,
            manual_session_id.as_deref(),
        );
        if active_flow_key.as_deref() != Some(flow_key.as_str()) {
            messages = flow_store
                .load_messages(&flow_key, history_load_message_cap(history_turn_limit))?;
            let dropped = enforce_history_turn_limit(&mut messages, history_turn_limit);
            if dropped > 0 {
                info!(
                    flow_key = %flow_key,
                    dropped_messages = dropped,
                    history_turn_limit,
                    "Applied history turn limit to loaded flow messages"
                );
            }
            flow_token_usage = messages
                .iter()
                .map(|m| estimate_tokens_approx_min1(&m.content) as u64)
                .sum();
            active_flow_key = Some(flow_key.clone());
        }

        let user_message = Message {
            role: Role::User,
            content: compressed,
            tool_call_id: None,
            tool_calls: None,
        };
        flow_store.append_message(&flow_key, &agent_id, &user_message)?;
        flow_token_usage += estimate_tokens_approx_min1(&user_message.content) as u64;
        messages.push(user_message);
        enforce_history_turn_limit(&mut messages, history_turn_limit);
        let mut compaction_outcome = maybe_compact_flow(
            &flow_store,
            &flow_key,
            &agent_id,
            &mut messages,
            &mut flow_token_usage,
            &*refiner,
            compaction_policy,
            "pre-engine",
        )
        .await?;

        if flow_token_usage >= agent_config.limits.max_tokens_per_flow {
            println!(
                "Flow token limit reached ({}). Use /reset to start a new session.\n",
                agent_config.limits.max_tokens_per_flow
            );
            continue;
        }

        let remaining_flow_tokens = agent_config
            .limits
            .max_tokens_per_flow
            .saturating_sub(flow_token_usage);
        let base_input_budget = compute_base_input_budget(
            engine.context_window(),
            system_prompt.as_deref(),
            remaining_flow_tokens,
        );
        let history_assembly = assemble_recent_history(&messages, base_input_budget);
        let retrieval_budget_requested =
            compute_retrieval_bucket_budget(active_lens, &agent_config.lens, base_input_budget);
        let retrieval_budget_effective = retrieval_budget_requested
            .min(base_input_budget.saturating_sub(history_assembly.used_tokens));

        let retrieved = if let Some(store) = knowledge_store.as_ref() {
            // TODO(epic-runtime-retrieval): Add periodic/incremental re-ingestion hooks.
            store.query_with_budget(
                &inbound.content,
                active_lens,
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
        let total_input_budget =
            compute_total_input_budget(engine.context_window(), remaining_flow_tokens);
        let report = PromptAssemblyReport {
            system_tokens,
            retrieval_tokens: retrieval_assembly.used_tokens,
            retrieval_budget_requested,
            retrieval_budget_effective,
            history_tokens: history_assembly.used_tokens,
            dropped_history_messages: history_assembly.dropped_messages,
            dropped_retrieval_items: retrieval_assembly.dropped_items,
            reserved_output_tokens: reserved_output_tokens(engine.context_window()),
            total_input_budget,
            flow_budget_remaining: remaining_flow_tokens as usize,
            compaction_applied: compaction_outcome.applied,
            compacted_messages: compaction_outcome.compacted_messages,
        };
        log_prompt_budget_report(
            &flow_key,
            active_lens,
            engine.context_window(),
            &report,
            history_messages_selected,
            retrieved_candidates,
        );
        last_prompt_report = Some(report);

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

        match engine.run(&prompt_messages, &[], &context).await {
            Ok(mut stream) => {
                let mut response_text = String::new();

                while let Some(event) = stream.next().await {
                    match event {
                        tengu_core::types::StreamEvent::TextDelta { text } => {
                            response_text.push_str(&text);
                        }
                        tengu_core::types::StreamEvent::Usage {
                            input_tokens,
                            output_tokens,
                        } => {
                            total_input_tokens += input_tokens;
                            total_output_tokens += output_tokens;
                        }
                        tengu_core::types::StreamEvent::Error { message } => {
                            eprintln!("Engine error: {}", message);
                        }
                        tengu_core::types::StreamEvent::Done => {}
                        _ => {}
                    }
                }

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
                    flow_token_usage +=
                        estimate_tokens_approx_min1(&assistant_message.content) as u64;
                    messages.push(assistant_message);
                    enforce_history_turn_limit(&mut messages, history_turn_limit);
                    let post_outcome = maybe_compact_flow(
                        &flow_store,
                        &flow_key,
                        &agent_id,
                        &mut messages,
                        &mut flow_token_usage,
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
            Err(e) => {
                eprintln!("Engine error: {}\n", e);
            }
        }
    }

    pipe.disconnect().await?;
    Ok(())
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

/// Reserve output tokens as a fraction of the model context window.
fn reserved_output_tokens(context_window: usize) -> usize {
    const RESERVED_OUTPUT_TOKENS_MIN: usize = 256;
    (context_window / 5).max(RESERVED_OUTPUT_TOKENS_MIN)
}

/// Compute input budget after output reserve and static system prompt footprint.
fn compute_base_input_budget(
    context_window: usize,
    system_prompt: Option<&str>,
    remaining_flow_tokens: u64,
) -> usize {
    let total_budget = compute_total_input_budget(context_window, remaining_flow_tokens);
    let system_tokens = system_prompt.map(estimate_tokens_approx_min1).unwrap_or(0);
    total_budget.saturating_sub(system_tokens)
}

/// Compute maximum input budget before prompt-bucket allocation.
fn compute_total_input_budget(context_window: usize, remaining_flow_tokens: u64) -> usize {
    let reserved_output = reserved_output_tokens(context_window);
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

    let header = "Relevant workspace context:\n\n";
    let separator = "\n\n---\n\n";
    let header_tokens = estimate_tokens_approx_min1(header);
    let separator_tokens = estimate_tokens_approx_min1(separator);
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
                "Relevant workspace context:\n\n{}",
                chunks.join("\n\n---\n\n")
            ))
        },
        used_tokens: if chunks.is_empty() { 0 } else { used },
        dropped_items: dropped,
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
    );
    println!();
    println!("  TENGU CLUSTER");
    println!("  ─────────────────────────────────────");
    println!("  Agent:    {} ({})", identity, agent_id);
    println!("  Engine:   {}/{}", agent_config.engine, agent_config.model);
    println!("  Context:  {} tokens", engine.context_window());
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

    // Check Ollama connectivity
    for (id, ac) in &config.agents {
        if ac.engine == "ollama" {
            let base_url = std::env::var("OLLAMA_HOST")
                .unwrap_or_else(|_| "http://localhost:11434".to_string());
            print!("  Ollama ({})... ", id);
            match reqwest::get(format!("{}/api/tags", base_url)).await {
                Ok(resp) if resp.status().is_success() => println!("OK"),
                Ok(resp) => println!("Error: HTTP {}", resp.status()),
                Err(e) => println!("Unreachable: {}", e),
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
fn resolve_flow_compaction_policy(
    flow: &tengu_core::config::FlowConfig,
    max_tokens_per_flow: u64,
    context_window: usize,
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

    let max_input_budget = compute_total_input_budget(context_window, max_tokens_per_flow);
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
        let budget = estimate_tokens_approx_min1("Relevant workspace context:\n\n")
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
        let budget = estimate_tokens_approx_min1("Relevant workspace context:\n\n")
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

        let policy = resolve_flow_compaction_policy(&flow, 1_000, 8_192);
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

        let small_ctx = resolve_flow_compaction_policy(&flow, 500_000, 8_192);
        let large_ctx = resolve_flow_compaction_policy(&flow, 500_000, 128_000);

        assert!(small_ctx.summary_max_tokens >= 128);
        assert!(small_ctx.summary_max_tokens < 1_500);
        assert!(large_ctx.summary_max_tokens > small_ctx.summary_max_tokens);
        assert!(large_ctx.summary_max_tokens <= 4_096);
    }
}
