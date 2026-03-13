//! Headless Telegram bot adapter that wires TelegramPipe → ChatRuntimeService.
//!
//! This adapter provides the full Telegram integration:
//!
//! - **Inline keyboard approval** — tools with `requires_approval: true` prompt
//!   the user with Approve/Deny buttons via `TelegramInlineApprovalAdapter`.
//!   60-second timeout auto-denies.
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

use crate::adapters::channel_runtime;
use crate::adapters::engine_factory::build_engine;
use crate::adapters::flow_store::FlowStore;
use crate::adapters::skill_source::FileSystemSkillSource;
use crate::adapters::workspace_tools;
use crate::application::chat_commands::{self, CommandResult, EngineInfo};
use crate::application::chat_runtime::ChatRuntimeService;
use crate::application::engine_runtime::{SanitizedToolExecutor, ToolExecutor};
use crate::application::flow_policy::resolve_flow_compaction_policy;
use crate::application::memory_service::MemoryService;
use crate::application::ports::{ToolActivityPort, ToolApprovalPort};
use crate::application::skill_commands::{SkillCommandMatch, SkillCommandRouter};
use crate::application::skill_registry::SkillRegistry;
use crate::domain::capability::{parse_capability_set, CapabilityId, RegisteredTool};
use crate::domain::chat::{resolve_history_turn_limit, ChatLoopState};
use crate::domain::secret_registry::SecretRegistry;
use crate::resolve_tengu_home;
use tengu_core::config::Config;
use tengu_core::types::{DeliveryOptions, ToolCall};
use tengu_core::{Engine, Pipe, PipeContext, Refiner};
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
    cancel: Arc<std::sync::atomic::AtomicBool>,
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

        let (title, description, preview) = crate::adapters::tool_ui::build_approval_text(call);
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
                                if !approved {
                                    // User denied — set cancel flag to stop the entire turn.
                                    self.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                                }
                                Ok(approved)
                            }
                            Ok(Err(_)) => {
                                warn!(tool = %call.name, "Approval channel closed — denying");
                                self.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                                Ok(false)
                            }
                            Err(_) => {
                                warn!(tool = %call.name, "Approval timed out — denying");
                                self.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
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
/// Only tools that require approval (mutations) are interesting for other agents.
fn format_tool_for_activity(
    call: &ToolCall,
    tools: &[tengu_core::types::ToolDef],
) -> Option<String> {
    // Find the tool definition to check if it requires approval.
    let tool_def = tools.iter().find(|t| t.name == call.name);
    let requires_approval = tool_def
        .and_then(|t| t.policy.as_ref())
        .map(|p| p.requires_approval)
        .unwrap_or(false);

    if !requires_approval {
        return None;
    }

    let summary = crate::adapters::tool_ui::summarize_tool_args(&call.arguments);
    let truncated = if summary.len() > 80 {
        let mut end = 80;
        while end > 0 && !summary.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &summary[..end])
    } else {
        summary
    };
    Some(format!("{}: {}", call.name, truncated))
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
         IMPORTANT: Use your available tools to check files they created before making changes.\n\n",
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

/// Max chars of each dependency output to embed inline in task prompts.
const MAX_DEP_OUTPUT_CHARS: usize = 3000;

/// Build the prompt for a task, embedding dependency outputs inline.
///
/// `dep_outcomes` elements: `(task_id, role, output_text)`.
fn build_task_prompt(
    goal: &str,
    task_description: &str,
    dep_outcomes: &[(String, String, String)],
) -> String {
    let mut prompt = format!("## Goal\n{}\n\n## Your Task\n{}\n", goal, task_description);

    if !dep_outcomes.is_empty() {
        prompt.push_str("\n## Results from Previous Steps\n\n");
        for (task_id, role, output_text) in dep_outcomes {
            prompt.push_str(&format!("### {} ({})\n", task_id, role));
            prompt.push_str(&channel_runtime::truncate_output(
                output_text,
                MAX_DEP_OUTPUT_CHARS,
            ));
            prompt.push('\n');
        }
    }

    prompt.push_str("\n## IMPORTANT\n\
        When you finish, write a concise outcome summary to the file path provided below.\n\
        Include: what you did, key results, file paths you created, URLs, IDs, and any info the next agent needs.\n\n\
        ## STRICT RULES\n\
        - NEVER fabricate, guess, or hallucinate URLs, transaction hashes, token IDs, or API responses.\n\
        - Every URL, hash, and ID in your output MUST come from an actual tool call result.\n\
        - If a tool call fails, STOP and report the error. Do NOT invent a successful result.\n\
        - If you cannot complete your task, say so clearly — do NOT make up data.\n");

    prompt
}

/// Orchestrate a multi-agent goal: plan → parallel batch execution → collect results.
///
/// Uses `collect_engine_response` directly instead of `ChatRuntimeService` so that
/// tasks within a batch can run concurrently via `tokio::task::JoinSet`. This means
/// orchestrated tasks do NOT get:
/// - Per-user conversation history (tasks are one-shot with a fresh prompt)
/// - Flow compaction or persistence (tasks are ephemeral)
/// - Automatic memory recall (context comes from planner prompt + dependency outcomes)
///
/// Memory **write** (the `remember` tool) still works through the tool executor.
/// This matches the CLI orchestrator's approach (`src/adapters/orchestrator.rs`).
async fn orchestrate_team_goal(
    pipe: &Arc<tengu_channels::telegram::TelegramPipe>,
    sender: &tengu_core::types::Recipient,
    goal: &str,
    media: Option<&Vec<tengu_core::types::MediaPayload>>,
    delivery_opts: &DeliveryOptions,
    agent_states: &mut HashMap<String, TelegramAgentState>,
    default_agent_id: &str,
    agent_descriptions: &HashMap<String, String>,
    role_to_agent: &HashMap<String, String>,
    current_recipient: &CurrentRecipient,
    memory_handle: &Option<Arc<crate::adapters::memory_tool_executor::MemoryServiceHandle>>,
    secret_registry: &Arc<SecretRegistry>,
    approval_adapter: &Arc<dyn ToolApprovalPort>,
    activity_adapter: &Arc<dyn ToolActivityPort>,
    turn_cancel: &Arc<std::sync::atomic::AtomicBool>,
    _user_states: &mut HashMap<String, ChatLoopState>,
    _sender_id: &str,
    _flow_store: &FlowStore,
    _refiner: &dyn Refiner,
    _memory_service_instance: Option<&MemoryService<'_>>,
    _memory_config: &tengu_core::config::MemoryConfig,
) -> Result<()> {
    *current_recipient.lock().unwrap() = Some(sender.clone());

    let mut attachment_notes: Vec<String> = Vec::new();
    if let Some(media) = media {
        let ws = agent_states
            .get(default_agent_id)
            .and_then(|a| a.workspace.as_ref());
        if let Some(ws) = ws {
            let attachments_dir = ws.join(".tengu-attachments");
            std::fs::create_dir_all(&attachments_dir).ok();
            for m in media {
                let fname =
                    sanitize_attachment_filename(m.filename.as_deref().unwrap_or("attachment"));
                let path = attachments_dir.join(&fname);
                match std::fs::write(&path, &m.data) {
                    Ok(()) => {
                        info!(path = %path.display(), size = m.data.len(), "Saved Telegram attachment for orchestration");
                        attachment_notes.push(format!(
                            "[Attached file: {} ({}, {} bytes)]",
                            path.display(),
                            m.mime_type,
                            m.data.len()
                        ));
                    }
                    Err(e) => {
                        warn!(error = %e, "Failed to save Telegram attachment for orchestration");
                    }
                }
            }
        }
    }

    let goal = if attachment_notes.is_empty() {
        goal.to_string()
    } else {
        format!("{}\n{}", attachment_notes.join("\n"), goal)
    };

    pipe.send_text(sender, "Planning…", delivery_opts).await?;

    let planner_engine = match agent_states.get(default_agent_id) {
        Some(a) => a.engine.as_ref(),
        None => {
            pipe.send_text(sender, "Planner engine unavailable.", delivery_opts)
                .await?;
            return Ok(());
        }
    };

    // RAG: recall only orchestrator topic overviews for planner context.
    let enriched_goal = if let Some(ref handle) = memory_handle {
        let mem_svc = MemoryService::new(handle.embedding.as_ref(), handle.store.as_ref());
        let mut filter = std::collections::HashMap::new();
        filter.insert("kind".into(), "topic_overview".into());
        filter.insert("source".into(), "orchestrator".into());
        match mem_svc.recall_filtered(&goal, 3, 600, &filter).await {
            Ok(results) if !results.is_empty() => {
                let mut enriched = String::from("## Relevant Prior Work\n");
                for r in &results {
                    enriched.push_str(&format!("- {}\n", r.entry.content));
                }
                enriched.push_str(&format!("\n## Current Goal\n{}", goal));
                enriched
            }
            _ => goal.clone(),
        }
    } else {
        goal.clone()
    };

    let tasks = match crate::application::task_planner::generate_plan(
        planner_engine,
        &enriched_goal,
        agent_descriptions,
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            pipe.send_text(
                sender,
                &format!("Failed to generate plan: {}", e),
                delivery_opts,
            )
            .await?;
            return Ok(());
        }
    };

    let batches = match crate::application::task_planner::resolve_execution_order(&tasks) {
        Ok(b) => b,
        Err(e) => {
            pipe.send_text(sender, &format!("Bad plan: {}", e), delivery_opts)
                .await?;
            return Ok(());
        }
    };

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
    pipe.send_text(sender, &plan_text, delivery_opts).await?;

    let outcomes_dir = agent_states
        .get(default_agent_id)
        .and_then(|a| a.workspace.as_ref())
        .map(|ws| ws.join(TASK_OUTCOMES_DIR));
    if let Some(ref dir) = outcomes_dir {
        std::fs::create_dir_all(dir).ok();
    }

    // task_id → (outcome_rel_path, output_text)
    let mut outcome_data: HashMap<String, (String, String)> = HashMap::new();
    turn_cancel.store(false, std::sync::atomic::Ordering::Relaxed);
    let mut completed_count = 0usize;
    let mut stopped = false;

    'batch_loop: for (bi, batch) in batches.iter().enumerate() {
        if turn_cancel.load(std::sync::atomic::Ordering::Relaxed) {
            pipe.send_text(sender, "Stopped by /stop.", delivery_opts)
                .await?;
            stopped = true;
            break;
        }

        if batch.len() > 1 {
            let labels: Vec<String> = batch.iter().map(|&idx| tasks[idx].role.clone()).collect();
            pipe.send_text(
                sender,
                &format!(
                    "Batch {}/{} — running in parallel: {}",
                    bi + 1,
                    batches.len(),
                    labels.join(", ")
                ),
                delivery_opts,
            )
            .await?;
        }

        // Phase 1: Hot-reload skills and build task snapshots (needs &mut agent_states).
        struct TaskSnapshot {
            task_id: String,
            task_desc: String,
            role: String,
            agent_label: String,
            engine: Arc<dyn Engine>,
            tools: Vec<tengu_core::types::ToolDef>,
            system_prompt: String,
            workspace: Option<std::path::PathBuf>,
            tool_executor: Option<Arc<dyn ToolExecutor>>,
            prompt: String,
            outcome_rel_path: String,
        }

        let mut snapshots: Vec<TaskSnapshot> = Vec::new();

        for &task_idx in batch {
            if turn_cancel.load(std::sync::atomic::Ordering::Relaxed) {
                pipe.send_text(sender, "Stopped by /stop.", delivery_opts)
                    .await?;
                stopped = true;
                break 'batch_loop;
            }

            let task = &tasks[task_idx];
            let task_agent_id = match role_to_agent.get(&task.role) {
                Some(id) => id.clone(),
                None => {
                    pipe.send_text(
                        sender,
                        &format!("Unknown role '{}', skipping task '{}'.", task.role, task.id),
                        delivery_opts,
                    )
                    .await?;
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

            pipe.send_text(
                sender,
                &format!("[{}] {}", agent_label, task.task),
                delivery_opts,
            )
            .await?;

            // Hot-reload skills (mutable operation — must happen before spawning).
            if let Some(ref src) = agent.skill_source {
                if agent.skill_registry.reload(src) {
                    agent.current_tools = channel_runtime::rebuild_tools(
                        &agent.base_tools,
                        &agent.skill_registry,
                        &agent.capabilities,
                    );
                    agent.current_system_prompt = channel_runtime::rebuild_system_prompt(
                        &agent.agent_config,
                        agent.advertise_workspace_tools,
                        &agent.skill_registry,
                        &agent.current_tools,
                    );
                }
            }

            // Build tool executor and wrap in Arc for sharing across spawn boundary.
            let tool_executor: Option<Arc<dyn ToolExecutor>> =
                agent.workspace.as_ref().and_then(|ws| {
                    channel_runtime::build_tool_executor(
                        ws,
                        &agent.current_tools,
                        &agent.skill_registry,
                        memory_handle,
                        secret_registry,
                        Arc::clone(approval_adapter),
                        Arc::clone(activity_adapter),
                        Some(Arc::clone(turn_cancel)),
                    )
                    .map(|e| Arc::new(e) as Arc<dyn ToolExecutor>)
                });

            let dep_outcomes: Vec<(String, String, String)> = task
                .depends_on
                .iter()
                .filter_map(|dep_id| {
                    outcome_data.get(dep_id).map(|(_, output_text)| {
                        let role = tasks
                            .iter()
                            .find(|t| t.id == *dep_id)
                            .map(|t| t.role.clone())
                            .unwrap_or_default();
                        (dep_id.clone(), role, output_text.clone())
                    })
                })
                .collect();

            let outcome_rel_path = format!("{}/{}.md", TASK_OUTCOMES_DIR, task.id);
            let mut prompt = build_task_prompt(&goal, &task.task, &dep_outcomes);
            prompt.push_str(&format!(
                "\nWrite your outcome summary to: `{}`\n",
                outcome_rel_path
            ));

            snapshots.push(TaskSnapshot {
                task_id: task.id.clone(),
                task_desc: task.task.clone(),
                role: task.role.clone(),
                agent_label,
                engine: Arc::clone(&agent.engine),
                tools: channel_runtime::tool_defs(&agent.current_tools),
                system_prompt: agent.current_system_prompt.clone(),
                workspace: agent.workspace.clone(),
                tool_executor,
                prompt,
                outcome_rel_path,
            });
        }
        // &mut agent_states borrow ends here.

        if stopped {
            break;
        }

        // Phase 2: Spawn all tasks in this batch concurrently via JoinSet.
        let mut set = tokio::task::JoinSet::new();

        for snap in snapshots {
            let sr = Arc::clone(secret_registry);
            let cancel = Arc::clone(turn_cancel);
            let obs_pipe = Arc::clone(pipe);
            let obs_sender = sender.clone();
            let obs_label = snap.agent_label.clone();

            set.spawn(async move {
                // Typing indicator for this task.
                let typing_pipe = Arc::clone(&obs_pipe);
                let typing_sender = obs_sender.clone();
                let typing_handle = tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(std::time::Duration::from_secs(4)).await;
                        let _ = typing_pipe.send_chat_action(&typing_sender).await;
                    }
                });

                // Tool observer sends tool-call labels to Telegram.
                let obs_p = Arc::clone(&obs_pipe);
                let obs_s = obs_sender.clone();
                let obs_sr = Arc::clone(&sr);
                let obs_l = obs_label.clone();
                let observer = move |call: &ToolCall, result: &str| {
                    let _ = obs_sr.redact(result);
                    let text = format!("[{}] 🔧 `{}`", obs_l, call.name);
                    let p = Arc::clone(&obs_p);
                    let r = obs_s.clone();
                    tokio::task::block_in_place(move || {
                        let handle = tokio::runtime::Handle::current();
                        handle.block_on(async {
                            let _ = p.send_text(&r, &text, &DeliveryOptions::default()).await;
                        });
                    });
                };

                // Build messages and context (owned — no lifetimes).
                let messages = vec![tengu_core::types::Message {
                    role: tengu_core::types::Role::User,
                    content: snap.prompt,
                    tool_call_id: None,
                    tool_calls: None,
                }];

                let context = tengu_core::EngineContext {
                    workspace: snap.workspace,
                    system_prompt: Some(snap.system_prompt),
                };

                // Wrap executor in SanitizedToolExecutor for secret redaction.
                let sanitized = snap
                    .tool_executor
                    .as_ref()
                    .map(|e| SanitizedToolExecutor::new(e.as_ref(), &sr));

                let result = crate::application::engine_runtime::collect_engine_response(
                    snap.engine.as_ref(),
                    &messages,
                    &snap.tools,
                    &context,
                    sanitized.as_ref().map(|s| s as &dyn ToolExecutor),
                    Some(&observer),
                    Some(&cancel),
                )
                .await;

                typing_handle.abort();

                let output = match result {
                    Ok(resp) => Ok(resp.text),
                    Err(e) => Err(e),
                };

                (
                    snap.task_id,
                    snap.task_desc,
                    snap.role,
                    snap.agent_label,
                    snap.outcome_rel_path,
                    output,
                )
            });
        }

        // Phase 3: Collect results from this batch.
        while let Some(join_result) = set.join_next().await {
            let (task_id, task_desc, _role, agent_label, outcome_rel_path, outcome) = match join_result {
                Ok(r) => r,
                Err(e) => {
                    warn!(error = %e, "Orchestrated task panicked");
                    continue;
                }
            };

            match outcome {
                Ok(output) => {
                    if !output.is_empty() {
                        let reply = secret_registry.redact(&output);
                        for chunk in channel_runtime::chunk_message(&reply, TELEGRAM_MAX_LEN) {
                            let _ = pipe.send_text(sender, chunk, delivery_opts).await;
                        }
                    }
                    outcome_data
                        .insert(task_id, (outcome_rel_path, output));
                    completed_count += 1;
                }
                Err(e) => {
                    let _ = pipe
                        .send_text(
                            sender,
                            &format!("[{}] Failed: {}", agent_label, e),
                            delivery_opts,
                        )
                        .await;
                    let error_text = format!("FAILED: {}", e);
                    if let Some(ref dir) = outcomes_dir {
                        let path = dir.join(format!("{}.md", task_id));
                        let _ = std::fs::write(
                            &path,
                            format!("# FAILED\n\nTask: {}\nError: {}\n", task_desc, e),
                        );
                    }
                    outcome_data.insert(task_id, (outcome_rel_path, error_text));
                }
            }
        }
    }

    // Auto-summarize: store a topic overview in memory for future recall.
    if let Some(ref handle) = memory_handle {
        let mut mem_summary = format!("Goal: {}\n\nResults:\n", goal);
        for task in &tasks {
            if let Some((_, output_text)) = outcome_data.get(&task.id) {
                mem_summary.push_str(&format!(
                    "- {} ({}): {}\n",
                    task.role,
                    task.id,
                    channel_runtime::truncate_output(output_text, 500),
                ));
            }
        }
        let mem_svc = MemoryService::new(handle.embedding.as_ref(), handle.store.as_ref());
        let mut meta = std::collections::HashMap::new();
        meta.insert("kind".into(), "topic_overview".into());
        meta.insert("source".into(), "orchestrator".into());
        meta.insert("goal".into(), goal.to_string());
        if let Some(ws) = agent_states
            .get(default_agent_id)
            .and_then(|a| a.workspace.as_ref())
        {
            if let Some(name) = ws.file_name() {
                meta.insert("workspace_id".into(), name.to_string_lossy().to_string());
            }
        }
        match mem_svc
            .remember_with_metadata(&mem_summary, "orchestrator", meta)
            .await
        {
            Ok(id) => {
                tracing::debug!(id, "Stored topic overview in memory");
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to store topic overview");
            }
        }
    }

    let status = if stopped { "stopped" } else { "completed" };

    // Build a final summary from outcome files so the user gets actionable results.
    let mut summary = format!("Team {} — {}/{} tasks done.\n", status, completed_count, tasks.len());

    if let Some(ref dir) = outcomes_dir {
        for task in &tasks {
            let path = dir.join(format!("{}.md", task.id));
            if let Ok(content) = std::fs::read_to_string(&path) {
                let content = content.trim();
                if !content.is_empty() {
                    summary.push_str(&format!("\n── {} ({}) ──\n", task.role, task.id));
                    summary.push_str(content);
                    summary.push('\n');
                }
            }
        }
    }

    let summary = secret_registry.redact(&summary);
    for chunk in channel_runtime::chunk_message(&summary, TELEGRAM_MAX_LEN) {
        let _ = pipe.send_text(sender, chunk, delivery_opts).await;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Build the allowed-users set from config + env
// ---------------------------------------------------------------------------

fn build_allowed_users(config: &Config) -> HashSet<String> {
    let mut allowed: HashSet<String> = config.telegram.allowed_users.iter().cloned().collect();

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
    engine: Arc<dyn Engine>,
    engine_info: EngineInfo,
    workspace: Option<std::path::PathBuf>,
    base_tools: Vec<RegisteredTool>,
    skill_source: Option<FileSystemSkillSource>,
    skill_registry: SkillRegistry,
    current_tools: Vec<RegisteredTool>,
    current_system_prompt: String,
    advertise_workspace_tools: bool,
    history_turn_limit: usize,
    compaction_policy: crate::domain::chat::FlowCompactionPolicy,
    role: Option<String>,
    capabilities: HashSet<CapabilityId>,
}

/// Run the headless Telegram bot adapter (sync — call from `block_in_place`).
///
/// Creates its own tokio runtime internally so that state containing nested
/// runtimes (e.g. `MemoryToolExecutionAdapter`) drops in a sync context,
/// avoiding the "Cannot drop a runtime in a context where blocking is not
/// allowed" panic.
///
/// Supports multi-agent routing: prefix messages with `@role: message` to
/// target a specific agent. In multi-agent mode, unrouted plain messages are
/// orchestrated across the team automatically. Use `/agents` to list available
/// agents and their roles.
pub(crate) fn run_telegram(config: Config, secret_registry: Arc<SecretRegistry>) -> Result<()> {
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

    // Find first agent's workspace for per-sandbox memory scoping.
    let first_workspace: Option<std::path::PathBuf> = config
        .agents
        .values()
        .find_map(|ac| ac.workspace.as_ref().map(|p| workspace_tools::expand_tilde(p)));

    // Build shared memory handle (scoped to workspace if available).
    let memory_handle =
        channel_runtime::build_memory_handle(&memory_config, &rt, first_workspace.as_deref());

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
        let engine: Arc<dyn Engine> = match build_engine(agent_id, agent_config) {
            Ok(e) => Arc::from(e),
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
        let agent_capabilities = parse_capability_set(&agent_config.capabilities)
            .expect("agent capabilities should validate");

        let base_tools =
            channel_runtime::compute_base_tools(uses_tools, has_memory, &agent_config.capabilities);

        let skill_source: Option<FileSystemSkillSource> = workspace
            .as_ref()
            .map(|ws| FileSystemSkillSource::new(ws.clone()));

        let base_reserved: Vec<String> = base_tools.iter().map(|t| t.def.name.clone()).collect();
        let mut skill_registry = SkillRegistry::new(base_reserved)
            .with_allowlist(Some(agent_config.skill_packages.clone()));
        if let Some(ref src) = skill_source {
            skill_registry.reload(src);
        }

        let current_tools =
            channel_runtime::rebuild_tools(&base_tools, &skill_registry, &agent_capabilities);
        let current_system_prompt = channel_runtime::rebuild_system_prompt(
            agent_config,
            advertise_workspace_tools,
            &skill_registry,
            &current_tools,
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
                capabilities: agent_capabilities,
            },
        );
    }

    let default_agent_id =
        default_agent_id.ok_or_else(|| anyhow::anyhow!("No agents configured"))?;

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
            let name = astate.agent_config.identity.name.as_deref().unwrap_or(aid);
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
        cancel: Arc::clone(&turn_cancel),
    });
    let activity_adapter: Arc<dyn ToolActivityPort> = Arc::new(TelegramToolActivityAdapter);

    let memory_service_instance = memory_handle
        .as_ref()
        .map(|h| MemoryService::new(h.embedding.as_ref(), h.store.as_ref()));

    // Per-user-per-agent conversation states: keyed by "sender_id:agent_id".
    let mut user_states: HashMap<String, ChatLoopState> = HashMap::new();

    // Track which agent each user last talked to (for showing in prompts).
    let mut user_active_agent: HashMap<String, String> = HashMap::new();

    // Build skill command router from all agents' registries.
    let mut skill_command_router = {
        // Use the default agent's skill registry to build the initial router.
        let default_agent = agent_states.get(&default_agent_id);
        default_agent
            .map(|a| SkillCommandRouter::from_registry(&a.skill_registry))
            .unwrap_or_else(|| SkillCommandRouter::from_registry(&SkillRegistry::new(vec![])))
    };

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
                    // Primary /stop is intercepted at the pipe level (TelegramPipe)
                    // which sets turn_cancel to true. This is a fallback in case
                    // the message reaches the runtime loop directly.
                    turn_cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                    let _ = pipe
                        .send_text(&msg.sender, "⏹ Stop requested.", &delivery_opts)
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

                    let _ = orchestrate_team_goal(
                        &pipe,
                        &msg.sender,
                        goal,
                        msg.media.as_ref(),
                        &delivery_opts,
                        &mut agent_states,
                        &default_agent_id,
                        &agent_descriptions,
                        &role_to_agent,
                        &current_recipient,
                        &memory_handle,
                        &secret_registry,
                        &approval_adapter,
                        &activity_adapter,
                        &turn_cancel,
                        &mut user_states,
                        sender_id,
                        &flow_store,
                        refiner.as_ref(),
                        memory_service_instance.as_ref(),
                        &memory_config,
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
                            agent.current_tools = channel_runtime::rebuild_tools(
                                &agent.base_tools,
                                &agent.skill_registry,
                                &agent.capabilities,
                            );

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
                    lines.push("Auto:    plain messages  (plan & execute across agents)".to_string());
                    lines.push("Team:    /team <goal>     (explicit team planning command)".to_string());
                    lines.push("Project: /project <name>  (new project subfolder)".to_string());
                    lines.push("Example: @backend_engineer: add rate limiting".to_string());
                    lines.push("Example: register POI and mint the IP-NFT".to_string());
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

                // /purge — clear conversation state + wipe persistent memory + clean disk artifacts.
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

                    // Clean workspace disk artifacts (.tengu-tasks, .tengu-attachments).
                    let ws_set: HashSet<std::path::PathBuf> = agent_states
                        .values()
                        .filter_map(|s| s.workspace.clone())
                        .collect();
                    for ws in &ws_set {
                        for subdir in &[".tengu-tasks", ".tengu-attachments"] {
                            let p = ws.join(subdir);
                            if p.exists() {
                                match std::fs::remove_dir_all(&p) {
                                    Ok(()) => lines.push(format!("Cleaned {}", p.display())),
                                    Err(e) => {
                                        lines.push(format!("Failed to clean {}: {}", p.display(), e))
                                    }
                                }
                            }
                        }
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
                    agent.current_tools = channel_runtime::rebuild_tools(
                        &agent.base_tools,
                        &agent.skill_registry,
                        &agent.capabilities,
                    );
                    agent.current_system_prompt = channel_runtime::rebuild_system_prompt(
                        &agent.agent_config,
                        agent.advertise_workspace_tools,
                        &agent.skill_registry,
                        &agent.current_tools,
                    );
                    // Rebuild the skill command router after reload.
                    skill_command_router = SkillCommandRouter::from_registry(&agent.skill_registry);
                    lines.push(format!("{} tool(s) active.", agent.current_tools.len()));
                    let _ = pipe
                        .send_text(&msg.sender, &lines.join("\n"), &delivery_opts)
                        .await;
                    continue;
                }

                // Check skill-declared commands before built-in commands.
                if let SkillCommandMatch::Matched {
                    skill_name,
                    command: cmd_name,
                    args,
                } = skill_command_router.route(&msg.content)
                {
                    // Delegate to the agent — inject a system message so
                    // the agent follows the skill's ## Commands section.
                    let injected = format!(
                        "[System: User invoked /{cmd} {args}. Follow the instructions in the ## Commands section of the {skill} skill.]",
                        cmd = cmd_name,
                        args = args,
                        skill = skill_name,
                    );
                    // Fall through to normal message processing with the injected prompt.
                    // We break out of the slash-command block and let the message-routing
                    // code below handle it as a regular user message.

                    // Use the active (or default) agent.
                    user_active_agent.insert(sender_id.clone(), active_aid.clone());

                    // Hot-reload skills.
                    if let Some(ref src) = agent.skill_source {
                        if agent.skill_registry.reload(src) {
                            agent.current_tools = channel_runtime::rebuild_tools(
                                &agent.base_tools,
                                &agent.skill_registry,
                                &agent.capabilities,
                            );
                            agent.current_system_prompt = channel_runtime::rebuild_system_prompt(
                                &agent.agent_config,
                                agent.advertise_workspace_tools,
                                &agent.skill_registry,
                                &agent.current_tools,
                            );
                            skill_command_router = SkillCommandRouter::from_registry(&agent.skill_registry);
                        }
                    }

                    // Set recipient for inline approvals.
                    *current_recipient.lock().unwrap() = Some(msg.sender.clone());

                    // Build executor.
                    let current_executor =
                        agent.workspace.as_ref().and_then(|ws| {
                            channel_runtime::build_tool_executor(
                                ws,
                                &agent.current_tools,
                                &agent.skill_registry,
                                &memory_handle,
                                &secret_registry,
                                Arc::clone(&approval_adapter),
                                Arc::clone(&activity_adapter),
                                Some(Arc::clone(&turn_cancel)),
                            )
                        });
                    let sanitized_executor = current_executor.as_ref().map(|e| {
                        SanitizedToolExecutor::new(e as &dyn ToolExecutor, &secret_registry)
                    });

                    turn_cancel.store(false, std::sync::atomic::Ordering::Relaxed);

                    let state_key = format!("{}:{}", sender_id, active_aid);
                    let state = user_states.entry(state_key).or_insert_with(|| {
                        channel_runtime::create_chat_loop_state(&agent.agent_config)
                    });

                    // Typing indicator.
                    let _ = pipe.send_chat_action(&msg.sender).await;
                    let t_pipe = Arc::clone(&pipe);
                    let t_sender = msg.sender.clone();
                    let typing_handle = tokio::spawn(async move {
                        loop {
                            tokio::time::sleep(std::time::Duration::from_secs(4)).await;
                            let _ = t_pipe.send_chat_action(&t_sender).await;
                        }
                    });

                    let active_tools = channel_runtime::tool_defs(&agent.current_tools);
                    let chat_runtime = ChatRuntimeService {
                        engine: agent.engine.as_ref(),
                        refiner: refiner.as_ref(),
                        flow_store: &flow_store,
                        agent_id: &agent.agent_id,
                        agent_config: &agent.agent_config,
                        history_turn_limit: agent.history_turn_limit,
                        compaction_policy: agent.compaction_policy,
                        system_prompt: agent.current_system_prompt.clone(),
                        tools: &active_tools,
                        tool_executor: sanitized_executor
                            .as_ref()
                            .map(|e| e as &dyn ToolExecutor),
                        memory_service: memory_service_instance.as_ref(),
                        max_recall_entries: memory_config.max_recall_entries,
                        max_recall_tokens: memory_config.max_recall_tokens,
                        tool_observer: None,
                        cancel: Some(&turn_cancel),
                    };

                    let result = chat_runtime.process_user_text(state, &injected).await;
                    typing_handle.abort();

                    match result {
                        Ok(res) => {
                            if let Some(notice) = res.system_notice {
                                let _ = pipe.send_text(&msg.sender, &notice, &delivery_opts).await;
                            }
                            if let Some(ref text) = res.assistant_text {
                                let reply = secret_registry.redact(text);
                                for chunk in channel_runtime::chunk_message(&reply, TELEGRAM_MAX_LEN) {
                                    let _ = pipe.send_text(&msg.sender, chunk, &delivery_opts).await;
                                }
                            }
                        }
                        Err(e) => {
                            let _ = pipe
                                .send_text(&msg.sender, &format!("Error: {}", e), &delivery_opts)
                                .await;
                        }
                    }
                    continue;
                }

                let state_key = format!("{}:{}", sender_id, active_aid);
                let state = user_states.entry(state_key).or_insert_with(|| {
                    channel_runtime::create_chat_loop_state(&agent.agent_config)
                });

                let skill_cmds = skill_command_router.list();
                let cmd_result = chat_commands::handle_chat_command(
                    &msg.content,
                    state,
                    &agent.engine_info,
                    &agent.agent_config,
                    agent.history_turn_limit,
                    agent.compaction_policy,
                    &skill_cmds,
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
            let (mut routed_role, user_text) =
                channel_runtime::parse_agent_routing(&msg.content, Some(&role_to_agent));

            if routed_role.is_none() && is_multi_agent {
                // Classify: single-agent or multi-agent?
                let decision = match agent_states.get(&default_agent_id) {
                    Some(a) => {
                        crate::application::task_planner::classify_request(
                            a.engine.as_ref(),
                            &user_text,
                            &agent_descriptions,
                        )
                        .await
                    }
                    None => Ok(crate::application::task_planner::RouteDecision::MultiAgent),
                };

                match decision {
                    Ok(crate::application::task_planner::RouteDecision::SingleAgent(role_key)) => {
                        if role_to_agent.contains_key(&role_key) {
                            info!(role = %role_key, "Classifier routed to single agent");
                            routed_role = Some(role_key);
                            // Fall through to normal single-agent handling below.
                        } else {
                            warn!(role = %role_key, "Classifier returned unknown role, falling back to planner");
                            let _ = orchestrate_team_goal(
                                &pipe, &msg.sender, &user_text, msg.media.as_ref(),
                                &delivery_opts, &mut agent_states, &default_agent_id,
                                &agent_descriptions, &role_to_agent, &current_recipient,
                                &memory_handle, &secret_registry, &approval_adapter,
                                &activity_adapter, &turn_cancel, &mut user_states,
                                sender_id, &flow_store, refiner.as_ref(),
                                memory_service_instance.as_ref(), &memory_config,
                            ).await;
                            continue;
                        }
                    }
                    Ok(crate::application::task_planner::RouteDecision::MultiAgent) => {
                        let _ = orchestrate_team_goal(
                            &pipe, &msg.sender, &user_text, msg.media.as_ref(),
                            &delivery_opts, &mut agent_states, &default_agent_id,
                            &agent_descriptions, &role_to_agent, &current_recipient,
                            &memory_handle, &secret_registry, &approval_adapter,
                            &activity_adapter, &turn_cancel, &mut user_states,
                            sender_id, &flow_store, refiner.as_ref(),
                            memory_service_instance.as_ref(), &memory_config,
                        ).await;
                        continue;
                    }
                    Err(e) => {
                        warn!(error = %e, "Classifier failed, falling back to planner");
                        let _ = orchestrate_team_goal(
                            &pipe, &msg.sender, &user_text, msg.media.as_ref(),
                            &delivery_opts, &mut agent_states, &default_agent_id,
                            &agent_descriptions, &role_to_agent, &current_recipient,
                            &memory_handle, &secret_registry, &approval_adapter,
                            &activity_adapter, &turn_cancel, &mut user_states,
                            sender_id, &flow_store, refiner.as_ref(),
                            memory_service_instance.as_ref(), &memory_config,
                        ).await;
                        continue;
                    }
                }
            }

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
                    agent.current_tools = channel_runtime::rebuild_tools(
                        &agent.base_tools,
                        &agent.skill_registry,
                        &agent.capabilities,
                    );
                    agent.current_system_prompt = channel_runtime::rebuild_system_prompt(
                        &agent.agent_config,
                        agent.advertise_workspace_tools,
                        &agent.skill_registry,
                        &agent.current_tools,
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
                    let fname = sanitize_attachment_filename(
                        m.filename.as_deref().unwrap_or("attachment"),
                    );
                    let path = attachments_dir.join(&fname);
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
            let current_executor =
                agent.workspace.as_ref().and_then(|ws| {
                    channel_runtime::build_tool_executor(
                        ws,
                        &agent.current_tools,
                        &agent.skill_registry,
                        &memory_handle,
                        &secret_registry,
                        Arc::clone(&approval_adapter),
                        Arc::clone(&activity_adapter),
                        Some(Arc::clone(&turn_cancel)),
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
            let observer_tools = channel_runtime::tool_defs(&agent.current_tools);
            let tool_result_observer = move |call: &ToolCall, result: &str| {
                // Record mutation-type tool calls for activity log.
                if let Some(entry) = format_tool_for_activity(call, &observer_tools) {
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
                channel_runtime::create_chat_loop_state(&agent.agent_config)
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

            // No per-user env var injection needed — Privy credentials are global env vars
            // and the agent obtains Molecule service tokens autonomously via the skill.

            let turn_tools = channel_runtime::tool_defs(&agent.current_tools);
            let chat_runtime = ChatRuntimeService {
                engine: agent.engine.as_ref(),
                refiner: refiner.as_ref(),
                flow_store: &flow_store,
                agent_id: &agent.agent_id,
                agent_config: &agent.agent_config,
                history_turn_limit: agent.history_turn_limit,
                compaction_policy: agent.compaction_policy,
                system_prompt: turn_system_prompt,
                tools: &turn_tools,
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

            // (No per-user env vars to clean up — Privy creds are global.)

            match result {
                Ok(result) => {
                    if let Some(notice) = result.system_notice {
                        let _ = pipe.send_text(&msg.sender, &notice, &delivery_opts).await;
                    }
                    if let Some(ref text) = result.assistant_text {
                        let reply = secret_registry.redact(text);
                        for chunk in channel_runtime::chunk_message(&reply, TELEGRAM_MAX_LEN) {
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

/// Replace spaces, commas, and other shell-unsafe characters in attachment
/// filenames with underscores so agents can reference them in shell commands
/// without quoting issues.
fn sanitize_attachment_filename(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(ctx.contains("available tools"));
    }

    fn test_tool_defs() -> Vec<tengu_core::types::ToolDef> {
        use tengu_core::types::{ToolDef, ToolPolicyMetadata, ToolRiskLevel};
        vec![
            ToolDef {
                name: "write_file".into(),
                description: "Write a file".into(),
                parameters: serde_json::json!({}),
                policy: Some(ToolPolicyMetadata {
                    risk_level: ToolRiskLevel::Medium,
                    requires_approval: true,
                }),
            },
            ToolDef {
                name: "read_file".into(),
                description: "Read a file".into(),
                parameters: serde_json::json!({}),
                policy: Some(ToolPolicyMetadata {
                    risk_level: ToolRiskLevel::Low,
                    requires_approval: false,
                }),
            },
        ]
    }

    #[test]
    fn format_tool_for_activity_write_file() {
        let tools = test_tool_defs();
        let call = ToolCall {
            id: "1".into(),
            name: "write_file".into(),
            arguments: serde_json::json!({"path": "src/main.js", "content": "..."}),
        };
        let result = format_tool_for_activity(&call, &tools);
        assert!(result.is_some());
        let text = result.unwrap();
        assert!(
            text.contains("write_file"),
            "should contain the tool name: {}",
            text
        );
    }

    #[test]
    fn format_tool_for_activity_read_file_skipped() {
        let tools = test_tool_defs();
        let call = ToolCall {
            id: "1".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({"path": "src/main.js"}),
        };
        assert!(format_tool_for_activity(&call, &tools).is_none());
    }

    #[test]
    fn sanitize_filename_replaces_spaces_and_commas() {
        assert_eq!(
            sanitize_attachment_filename("ChatGPT Image Mar 12, 2026, 10_53_35 AM.png"),
            "ChatGPT_Image_Mar_12__2026__10_53_35_AM.png"
        );
    }

    #[test]
    fn sanitize_filename_preserves_safe_names() {
        assert_eq!(
            sanitize_attachment_filename("photo-2026.jpg"),
            "photo-2026.jpg"
        );
    }
}
