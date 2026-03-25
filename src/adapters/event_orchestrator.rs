//! Event-driven orchestrator core for the event-bus architecture.
//!
//! Implements the orchestrator event loop from §4 of EVENT_BUS_ARCHITECTURE.md:
//! receives events from agents, updates the Plan, dispatches ready tasks,
//! handles errors with retry + cascade-skip, and integrates Tier 1 data routing.

use crate::adapters::types::{AgentId, OrchestratorEvent, Plan, TaskId, TaskStatus};
use chrono::Utc;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use crate::adapters::Engine;
use tokio::sync::mpsc;

/// Maximum output length included verbatim in downstream context.
/// Longer outputs use Tier 2 LLM summarization when a planner engine is available.
const SHORT_OUTPUT_THRESHOLD: usize = 4000;

/// Artifacts that are internal to a task and useless for downstream agents.
/// These are large blobs (calldata, raw signatures) that waste context tokens.
fn should_skip_artifact(key: &str) -> bool {
    key.contains("calldata")
        || key.starts_with("sign_message.id.signature")
        || key.starts_with("sign_message.id.signer")
        || key.starts_with("sign_and_send_transaction.id.chain_id")
}

/// Extract a structured output block from an agent's response.
/// Looks for patterns like `RESEARCH_OUTPUT:`, `MINT_OUTPUT:`, `MOL_LABS_OUTPUT:` etc.
/// Returns the block content if found, otherwise None.
fn extract_structured_output(output: &str) -> Option<String> {
    let lines: Vec<&str> = output.lines().collect();

    // Search from the END — the structured block is at the end of the LLM's response,
    // but may be followed by appended "## Tool Results" section.
    let mut last_block_start: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.ends_with("_OUTPUT:") || trimmed.ends_with("_OUTPUT: ") {
            last_block_start = Some(i);
        }
    }

    let start = last_block_start?;

    // Collect from the _OUTPUT: line until:
    // - a closing ``` (code fence)
    // - a line starting with "## " (new section like "## Tool Results")
    // - end of output
    let mut block = Vec::new();
    for i in start..lines.len() {
        let line = lines[i];
        if i > start {
            let trimmed = line.trim();
            if trimmed == "```" || trimmed.starts_with("## ") {
                break;
            }
        }
        block.push(line);
    }

    if !block.is_empty() {
        let result = block.join("\n").trim().to_string();
        if !result.is_empty() {
            tracing::info!(
                block_len = result.len(),
                block_preview = %if result.len() > 300 { &result[..300] } else { &result },
                "extract_structured_output — found block"
            );
            return Some(result);
        }
    }
    None
}

/// Configuration for the orchestrator event loop.
#[derive(Clone)]
pub(crate) struct OrchestratorConfig {
    pub max_retries: u32,
    pub task_timeout: Duration,
    /// Maximum plan modifications allowed per run (guards against runaway re-planning).
    pub max_modifications: u32,
    /// Optional planner engine for Tier 2 LLM summarization of long unstructured outputs.
    /// When `None`, long outputs are truncated instead.
    pub planner: Option<Arc<dyn Engine>>,
    /// Aggregate token budget for the entire orchestration run.
    /// When exceeded, remaining agents are cancelled.
    pub max_tokens_per_run: u64,
    /// Optional external cancel flag (e.g., from Telegram /stop).
    /// Checked in the event loop — when set, remaining tasks are skipped.
    pub cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// Optional notification channel for immediate feedback to the user
    /// (e.g., task failures sent to Telegram as they happen, not batched).
    pub notifications_tx: Option<mpsc::Sender<String>>,
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        Self {
            max_retries: 2,
            task_timeout: Duration::from_secs(300),
            max_modifications: 10,
            planner: None,
            max_tokens_per_run: 2_000_000,
            cancel: None,
            notifications_tx: None,
        }
    }
}

impl std::fmt::Debug for OrchestratorConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrchestratorConfig")
            .field("max_retries", &self.max_retries)
            .field("task_timeout", &self.task_timeout)
            .field("max_modifications", &self.max_modifications)
            .field("planner", &self.planner.is_some())
            .field("max_tokens_per_run", &self.max_tokens_per_run)
            .field("cancel", &self.cancel.is_some())
            .field("notifications", &self.notifications_tx.is_some())
            .finish()
    }
}

/// Aggregate token usage tracker for an orchestration run.
#[derive(Debug, Default)]
pub(crate) struct RunBudget {
    pub max_tokens: u64,
    pub used_input_tokens: u64,
    pub used_output_tokens: u64,
}

impl RunBudget {
    pub fn new(max_tokens: u64) -> Self {
        Self {
            max_tokens,
            ..Self::default()
        }
    }

    pub fn record(&mut self, usage: &crate::adapters::types::TokenUsage) {
        self.used_input_tokens += usage.input_tokens as u64;
        self.used_output_tokens += usage.output_tokens as u64;
    }

    pub fn used_tokens(&self) -> u64 {
        self.used_input_tokens + self.used_output_tokens
    }

    pub fn exceeded(&self) -> bool {
        self.max_tokens > 0 && self.used_tokens() >= self.max_tokens
    }
}

/// Status of a single task after the plan completes.
#[derive(Debug)]
pub(crate) struct TaskOutcome {
    pub id: TaskId,
    pub status: TaskStatus,
    pub output: Option<String>,
}

/// Result of a completed orchestration run.
#[derive(Debug)]
#[allow(dead_code)] // Fields used for logging/debugging via Debug.
pub(crate) struct PlanOutcome {
    pub goal: String,
    pub tasks: Vec<TaskOutcome>,
    pub revision: u64,
}

fn plan_outcome(plan: &Plan) -> PlanOutcome {
    PlanOutcome {
        goal: plan.goal.clone(),
        tasks: plan
            .tasks
            .values()
            .map(|t| TaskOutcome {
                id: t.id.clone(),
                status: t.status.clone(),
                output: t.output.clone(),
            })
            .collect(),
        revision: plan.revision,
    }
}

// ---------------------------------------------------------------------------
// Shared orchestration pipeline (used by CLI + Telegram)
// ---------------------------------------------------------------------------

/// Result of plan preparation — ready for execution.
#[allow(dead_code)]
pub(crate) struct PreparedPlan {
    pub plan: Plan,
    pub enriched_goal: String,
    /// Human-readable plan summary for display.
    pub summary: String,
}

/// Phase 1: Enrich goal with memory, generate plan, validate dependencies.
/// Channel adapters call this, display the summary, then call `execute_plan`.
pub(crate) async fn prepare_plan(
    goal: &str,
    planner_engine: &dyn Engine,
    agent_descriptions: &HashMap<String, String>,
    role_deps: &crate::adapters::types::RoleDependencies,
    memory_handle: &Option<Arc<crate::adapters::memory_builder::MemoryServiceHandle>>,
) -> anyhow::Result<PreparedPlan> {
    use crate::adapters::memory_builder::MemoryService;
    use crate::adapters::task_builder;
    use crate::adapters::types::{Task, TaskStatus};

    // RAG: recall orchestrator topic overviews for planner context.
    let enriched_goal = if let Some(ref handle) = memory_handle {
        let mem_svc = MemoryService::new(handle.embedding.as_ref(), handle.store.as_ref());
        let mut filter = HashMap::new();
        filter.insert("kind".into(), "topic_overview".into());
        filter.insert("source".into(), "orchestrator".into());
        match mem_svc.recall_filtered(goal, 3, 600, &filter).await {
            Ok(results) if !results.is_empty() => {
                let mut enriched = String::from(
                    "## Relevant Prior Work\n\
                     Background only. Use this for continuity or implementation hints.\n\
                     Do NOT treat it as additional requested deliverables, and do NOT expand scope beyond the current goal.\n",
                );
                for r in &results {
                    enriched.push_str(&format!("- {}\n", r.entry.content));
                }
                enriched.push_str(&format!("\n## Current Goal\n{}", goal));
                enriched
            }
            _ => goal.to_string(),
        }
    } else {
        goal.to_string()
    };

    // Generate plan via planner LLM.
    let mut tasks = task_builder::generate_plan(planner_engine, &enriched_goal, agent_descriptions).await?;

    // Resolve role-name references, auto-repair dependencies, validate.
    let role_refs = task_builder::resolve_role_refs_in_depends(&mut tasks);
    if role_refs > 0 {
        tracing::info!(role_refs, "Resolved role-name references in depends_on");
    }
    let repaired = task_builder::repair_plan_dependencies(&mut tasks, role_deps);
    if repaired > 0 {
        tracing::info!(repaired, "Auto-repaired plan: added {} dependency edges", repaired);
    }
    task_builder::validate_plan_dependencies(&tasks, role_deps)?;

    // Build human-readable summary.
    let mut summary = format!("Plan ({} tasks, DAG dispatch):\n", tasks.len());
    for pt in &tasks {
        let deps = if pt.depends_on.is_empty() {
            String::new()
        } else {
            format!(" (after: {})", pt.depends_on.join(", "))
        };
        summary.push_str(&format!("  - [{}] {}{}\n", pt.role, pt.task, deps));
    }

    // Convert PlanTask → live Plan.
    let live_tasks: Vec<Task> = tasks
        .iter()
        .map(|pt| Task {
            id: pt.id.clone(),
            role: pt.role.clone(),
            description: pt.task.clone(),
            depends_on: pt.depends_on.clone(),
            status: TaskStatus::Pending,
            output: None,
            artifacts: HashMap::new(),
            assigned_agent: None,
            started_at: None,
            attempt: 0,
            last_error: None,
        })
        .collect();
    let plan = crate::adapters::types::Plan::new(enriched_goal.clone(), live_tasks);

    Ok(PreparedPlan {
        plan,
        enriched_goal,
        summary,
    })
}

/// Phase 2: Wire EventBus, spawn agent workers, run the orchestrator loop,
/// store memory summary. Returns the final PlanOutcome.
pub(crate) async fn execute_plan(
    prepared: PreparedPlan,
    role_to_agent: &HashMap<String, String>,
    executors: &HashMap<String, Arc<dyn crate::adapters::types::AgentTaskExecutor>>,
    memory_handle: &Option<Arc<crate::adapters::memory_builder::MemoryServiceHandle>>,
    original_goal: &str,
    workspace_name: Option<&str>,
    config: &OrchestratorConfig,
) -> Result<PlanOutcome, String> {
    use crate::adapters::agent_builder::agent_worker;
    use crate::adapters::memory_builder::MemoryService;
    use crate::adapters::types::EventBus;
    use std::collections::HashSet;

    // Determine which agents are needed for this plan.
    let needed_agent_ids: Vec<String> = prepared
        .plan
        .tasks
        .values()
        .filter_map(|t| role_to_agent.get(&t.role).cloned())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();

    // Create per-plan EventBus + spawn workers.
    let EventBus {
        mut orchestrator_rx,
        orchestrator_tx,
        agent_txs,
        mut agent_rxs,
    } = EventBus::new(&needed_agent_ids, 32);

    let mut worker_handles = vec![];
    for agent_id in &needed_agent_ids {
        if let Some(executor) = executors.get(agent_id) {
            let rx = agent_rxs.remove(agent_id).unwrap();
            let tx = orchestrator_tx.clone();
            worker_handles.push(tokio::spawn(agent_worker(
                agent_id.clone(),
                rx,
                tx,
                Arc::clone(executor),
            )));
        }
    }
    drop(orchestrator_tx);

    // Run the orchestrator event loop.
    let outcome = run_orchestrator(
        prepared.plan,
        &mut orchestrator_rx,
        &agent_txs,
        role_to_agent,
        config,
    )
    .await;

    // Shut down workers.
    drop(agent_txs);
    for handle in worker_handles {
        let _ = handle.await;
    }

    // Store topic overview in memory for future RAG enrichment.
    // Skip storing if no tasks completed — failed/skipped runs add noise.
    if let (Ok(ref plan_outcome), Some(ref handle)) = (&outcome, memory_handle) {
        let has_completed = plan_outcome
            .tasks
            .iter()
            .any(|t| t.status == TaskStatus::Completed);
        if !has_completed {
            tracing::debug!("Skipping topic overview storage — no completed tasks");
        } else {
        // Strip file attachment paths from the goal — they are ephemeral and
        // would pollute future plan prompts when recalled as prior work.
        let clean_goal: String = original_goal
            .lines()
            .filter(|line| !line.starts_with("[Attached file:"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut mem_summary = format!("Goal: {}\n\nResults:\n", clean_goal.trim());
        for t in &plan_outcome.tasks {
            let status = match t.status {
                TaskStatus::Completed => "ok",
                TaskStatus::Failed => "failed",
                TaskStatus::Skipped => "skipped",
                _ => "unknown",
            };
            let output_preview = t
                .output
                .as_deref()
                .map(|o| crate::adapters::channel_runtime::truncate_output(o, 500))
                .unwrap_or_default();
            mem_summary.push_str(&format!("- {} ({}): {}\n", t.id, status, output_preview));
        }

        let mem_svc = MemoryService::new(handle.embedding.as_ref(), handle.store.as_ref());
        let mut meta = HashMap::new();
        meta.insert("kind".into(), "topic_overview".into());
        meta.insert("source".into(), "orchestrator".into());
        meta.insert("goal".into(), original_goal.to_string());
        if let Some(name) = workspace_name {
            meta.insert("workspace_id".into(), name.to_string());
        }
        match mem_svc
            .remember_with_metadata(&mem_summary, "orchestrator", meta)
            .await
        {
            Ok(id) => tracing::debug!(id, "Stored topic overview in memory"),
            Err(e) => tracing::warn!(error = %e, "Failed to store topic overview"),
        }
        } // else (has_completed)
    }

    outcome
}

// ---------------------------------------------------------------------------
// Tier 1 data routing
// ---------------------------------------------------------------------------

/// Build context from completed dependency outputs.
///
/// Data sources:
/// 1. In-memory Task artifacts (tool envelopes + structured block fields)
/// 2. Structured output block from LLM output (MINT_OUTPUT:, MOL_LABS_OUTPUT:, etc.)
pub(crate) fn build_task_context(
    plan: &Plan,
    task_id: &TaskId,
) -> HashMap<String, Value> {
    let task = &plan.tasks[task_id];
    tracing::info!(
        task = %task_id,
        deps = ?task.depends_on,
        "build_task_context — building context from deps"
    );
    let mut context = HashMap::new();

    for dep_id in &task.depends_on {
        let mut all_fields: HashMap<String, String> = HashMap::new();

        // In-memory artifacts (tool envelopes + structured block fields).
        // Filter out bulky/internal artifacts that downstream agents don't need.
        if let Some(dep_task) = plan.tasks.get(dep_id) {
            for (k, v) in &dep_task.artifacts {
                if should_skip_artifact(k) {
                    continue;
                }
                let val_str = match v {
                    Value::String(s) => s.clone(),
                    other => serde_json::to_string(other).unwrap_or_default(),
                };
                all_fields.insert(k.clone(), val_str);
            }
        }

        if all_fields.is_empty() {
            // No structured data — try including short LLM output.
            if let Some(dep_task) = plan.tasks.get(dep_id) {
                if let Some(output) = &dep_task.output {
                    if let Some(structured) = extract_structured_output(output) {
                        tracing::info!(
                            task = %task_id,
                            dep = %dep_id,
                            "build_task_context — using structured output block (no stored data)"
                        );
                        let mut dep_ctx = serde_json::Map::new();
                        dep_ctx.insert("output".into(), Value::String(structured));
                        context.insert(dep_id.clone(), Value::Object(dep_ctx));
                    } else if !output.is_empty() && output.len() <= SHORT_OUTPUT_THRESHOLD {
                        let mut dep_ctx = serde_json::Map::new();
                        dep_ctx.insert("output".into(), Value::String(output.clone()));
                        context.insert(dep_id.clone(), Value::Object(dep_ctx));
                    }
                }
            }
            continue;
        }

        // Build human-readable summary for the downstream agent.
        let mut summary = format!("Data from task '{dep_id}':\n");
        for (key, value) in &all_fields {
            let preview = if value.len() > 200 {
                format!("{}...[{} chars]", &value[..200], value.len())
            } else {
                value.clone()
            };
            summary.push_str(&format!("  {key}: {preview}\n"));
        }

        tracing::info!(
            task = %task_id,
            dep = %dep_id,
            field_count = all_fields.len(),
            fields = ?all_fields.keys().collect::<Vec<_>>(),
            "build_task_context — assembled data for dep"
        );

        let mut dep_ctx = serde_json::Map::new();
        dep_ctx.insert("data_summary".into(), Value::String(summary));
        dep_ctx.insert(
            "data".into(),
            serde_json::to_value(&all_fields).unwrap_or_default(),
        );
        context.insert(dep_id.clone(), Value::Object(dep_ctx));
    }

    context
}

/// Render the task prompt with goal, description, and upstream context.
pub(crate) fn format_task_prompt(
    goal: &str,
    description: &str,
    context: &HashMap<String, Value>,
) -> String {
    let mut prompt = format!("## Goal\n{goal}\n\n## Task\n{description}");
    if !context.is_empty() {
        prompt.push_str("\n\n## Context from upstream tasks\n");
        for (key, value) in context {
            prompt.push_str(&format!(
                "### {key}\n{}\n",
                serde_json::to_string_pretty(value).unwrap_or_default()
            ));
        }
    }
    prompt
}

// ---------------------------------------------------------------------------
// Tier 2 data routing
// ---------------------------------------------------------------------------

/// Use the planner LLM to extract only the relevant subset of an upstream output
/// for a downstream task. Capped at 2000 output tokens.
async fn summarize_for_downstream(
    planner: &dyn Engine,
    upstream_output: &str,
    downstream_task: &str,
) -> Result<String, String> {
    use crate::adapters::engine_builder::collect_engine_response;
    use crate::adapters::types::{Message, Role};
    use crate::adapters::EngineContext;

    tracing::debug!(
        upstream_len = upstream_output.len(),
        downstream_task_len = downstream_task.len(),
        "summarize_for_downstream — Tier 2 LLM call"
    );

    let prompt = format!(
        "Extract ONLY the information from the upstream output that is \
         needed for the following task. Be concise.\n\n\
         ## Upstream Output\n{upstream_output}\n\n\
         ## Downstream Task\n{downstream_task}"
    );

    let messages = vec![Message {
        role: Role::User,
        content: prompt,
        tool_call_id: None,
        tool_calls: None,
    }];

    let defaults = crate::adapters::config::LimitsConfig::default();
    let response = collect_engine_response(
        planner,
        &messages,
        &[],
        &EngineContext {
            workspace: None,
            system_prompt: None,
            bridge_tools: None,
        },
        None,
        None,
        None,
        Some(2000),
        defaults.max_tool_rounds,
        defaults.max_tool_result_chars,
        defaults.stream_event_timeout_secs,
        defaults.compact_result_limit,
    )
    .await
    .map_err(|e| format!("Tier 2 summarization failed: {e}"))?;

    Ok(response.text)
}

/// Build context with Tier 2 LLM summarization for long unstructured outputs.
async fn build_task_context_tier2(
    plan: &Plan,
    task_id: &TaskId,
    planner: &dyn Engine,
) -> HashMap<String, Value> {
    let task = &plan.tasks[task_id];
    let mut context = HashMap::new();

    for dep_id in &task.depends_on {
        if let Some(dep_task) = plan.tasks.get(dep_id) {
            let mut dep_ctx = serde_json::Map::new();

            if !dep_task.artifacts.is_empty() {
                dep_ctx.insert(
                    "artifacts".into(),
                    serde_json::to_value(&dep_task.artifacts).unwrap_or_default(),
                );
            }

            if let Some(output) = &dep_task.output {
                if !output.is_empty() {
                    if output.len() <= SHORT_OUTPUT_THRESHOLD || !dep_task.artifacts.is_empty() {
                        // Short text or already has artifacts → include verbatim.
                        dep_ctx.insert("output".into(), Value::String(output.clone()));
                    } else {
                        // Tier 2: LLM summarization for long unstructured text.
                        match summarize_for_downstream(planner, output, &task.description).await {
                            Ok(summary) => {
                                dep_ctx.insert("output".into(), Value::String(summary));
                            }
                            Err(e) => {
                                tracing::warn!(dep = %dep_id, error = %e, "Tier 2 fallback to truncation");
                                let truncated = format!(
                                    "{}...[truncated, {} chars total]",
                                    &output[..SHORT_OUTPUT_THRESHOLD],
                                    output.len()
                                );
                                dep_ctx.insert("output".into(), Value::String(truncated));
                            }
                        }
                    }
                }
            }

            if !dep_ctx.is_empty() {
                context.insert(dep_id.clone(), Value::Object(dep_ctx));
            }
        }
    }

    context
}

// ---------------------------------------------------------------------------
// Task dispatch
// ---------------------------------------------------------------------------

/// Transition a Ready task to Running and send a TaskAssignment to its agent.
async fn dispatch_task(
    plan: &mut Plan,
    task_id: &TaskId,
    agent_senders: &HashMap<AgentId, mpsc::Sender<OrchestratorEvent>>,
    role_to_agent: &HashMap<String, AgentId>,
    planner: Option<&dyn Engine>,
) -> Result<(), String> {
    let (role, raw_description, last_error, attempt) = {
        let task = plan
            .tasks
            .get(task_id)
            .ok_or_else(|| format!("task {task_id} not found"))?;
        (task.role.clone(), task.description.clone(), task.last_error.clone(), task.attempt)
    };

    let agent_id = role_to_agent
        .get(&role)
        .ok_or_else(|| format!("No agent for role '{role}'"))?
        .clone();

    let context = if let Some(engine) = planner {
        build_task_context_tier2(plan, task_id, engine).await
    } else {
        build_task_context(plan, task_id)
    };

    // Ready → Running transition.
    let task = plan.tasks.get_mut(task_id).unwrap();
    task.status = TaskStatus::Running;
    task.assigned_agent = Some(agent_id.clone());
    task.started_at = Some(Instant::now());

    // On retry, include the previous error so the agent takes a different approach.
    let description = if let Some(ref error) = last_error {
        format!(
            "{}\n\n## Previous Attempt Failed (attempt {})\nError: {}\n\nDo NOT repeat the same approach. Analyze what went wrong and try a different strategy.",
            raw_description, attempt, error
        )
    } else {
        raw_description.clone()
    };

    let rendered_prompt = format_task_prompt(&plan.goal, &description, &context);

    // Log the full context being passed to this task.
    for (dep_id, dep_data) in &context {
        tracing::info!(
            task = %task_id,
            from_dep = %dep_id,
            context_data = %serde_json::to_string(dep_data).unwrap_or_default(),
            "dispatch_task — upstream context for task"
        );
    }

    tracing::info!(
        task = %task_id,
        agent = %agent_id,
        role = %role,
        description = %raw_description,
        context_deps = context.len(),
        prompt_len = rendered_prompt.len(),
        revision = plan.revision,
        "dispatch_task — Ready → Running, sending TaskAssignment"
    );

    agent_senders
        .get(&agent_id)
        .ok_or_else(|| format!("No sender for agent '{agent_id}'"))?
        .send(OrchestratorEvent::TaskAssignment {
            task_id: task_id.clone(),
            agent_id: agent_id.clone(),
            description: rendered_prompt,
            context,
            correlation_id: plan.revision.to_string(),
            timestamp: Utc::now(),
        })
        .await
        .map_err(|e| format!("Failed to send to agent {agent_id}: {e}"))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Error handling
// ---------------------------------------------------------------------------

/// Handle a failed task: retry with error context if eligible, otherwise fail
/// permanently, cascade-skip dependents, and notify the user immediately.
fn handle_task_error(
    plan: &mut Plan,
    task_id: &TaskId,
    retryable: bool,
    error: &str,
    config: &OrchestratorConfig,
) {
    let Some(task) = plan.tasks.get_mut(task_id) else {
        return;
    };

    if retryable && task.attempt < config.max_retries {
        task.attempt += 1;
        // Store error so the retry prompt includes it — prevents the agent
        // from repeating the exact same approach that failed.
        task.last_error = Some(error.to_string());
        task.status = TaskStatus::Pending;
        tracing::info!(task = %task_id, attempt = task.attempt, "Retrying failed task with error context");
    } else {
        task.status = TaskStatus::Failed;
        task.last_error = Some(error.to_string());
        tracing::error!(task = %task_id, error = %error, "Task failed permanently");

        // Notify user immediately — don't wait for plan completion.
        if let Some(ref tx) = config.notifications_tx {
            let truncated_error = if error.len() > 500 {
                format!("{}...", &error[..500])
            } else {
                error.to_string()
            };
            let msg = format!("Task '{}' failed: {}", task_id, truncated_error);
            let _ = tx.try_send(msg);
        }

        cascade_skip(plan, task_id);
    }
}

/// Mark all non-terminal dependents of a failed task as Skipped (transitive).
fn cascade_skip(plan: &mut Plan, failed_id: &TaskId) {
    let dependents: Vec<TaskId> = plan
        .tasks
        .values()
        .filter(|t| {
            t.depends_on.contains(failed_id)
                && matches!(t.status, TaskStatus::Pending | TaskStatus::Ready)
        })
        .map(|t| t.id.clone())
        .collect();

    for dep_id in &dependents {
        tracing::info!(
            failed = %failed_id,
            skipping = %dep_id,
            "cascade_skip — marking dependent as Skipped"
        );
        plan.tasks.get_mut(dep_id).unwrap().status = TaskStatus::Skipped;
    }
    for dep_id in dependents {
        cascade_skip(plan, &dep_id);
    }
}

// ---------------------------------------------------------------------------
// Re-planning guards
// ---------------------------------------------------------------------------

use crate::adapters::types::PlanModification;

/// Check whether a PlanModification::AddTask would assign work to the
/// requesting agent (prevents infinite self-spawning loops).
fn is_self_referential_add(
    kind: &PlanModification,
    requested_by: &AgentId,
    role_to_agent: &HashMap<String, AgentId>,
) -> bool {
    if let PlanModification::AddTask { role, .. } = kind {
        if let Some(agent) = role_to_agent.get(role) {
            return agent == requested_by;
        }
    }
    false
}


/// Parse `KEY: VALUE` lines from a structured output block (e.g. MINT_OUTPUT:).
/// Returns a map of clean field names to values.
fn parse_structured_block_fields(block: &str) -> HashMap<String, String> {
    let mut fields = HashMap::new();
    for line in block.lines() {
        let trimmed = line.trim().trim_start_matches('-').trim();
        if let Some((key, value)) = trimmed.split_once(':') {
            let key = key.trim();
            let value = value.trim();
            if !key.is_empty() && !value.is_empty() && !key.ends_with("_OUTPUT") {
                fields.insert(key.to_string(), value.to_string());
            }
        }
    }
    fields
}

// ---------------------------------------------------------------------------
// Timeout detection
// ---------------------------------------------------------------------------

/// Returns the earliest deadline among all Running tasks.
fn earliest_running_deadline(plan: &Plan, timeout: Duration) -> Instant {
    plan.tasks
        .values()
        .filter(|t| t.status == TaskStatus::Running)
        .filter_map(|t| t.started_at.map(|s| s + timeout))
        .min()
        .unwrap_or_else(|| Instant::now() + timeout)
}

/// Find all Running tasks whose wall-clock time exceeds `timeout`.
fn find_timed_out_tasks(plan: &Plan, timeout: Duration) -> Vec<TaskId> {
    let now = Instant::now();
    plan.tasks
        .values()
        .filter(|t| t.status == TaskStatus::Running)
        .filter(|t| t.started_at.is_some_and(|s| now.duration_since(s) >= timeout))
        .map(|t| t.id.clone())
        .collect()
}

// ---------------------------------------------------------------------------
// Main event loop
// ---------------------------------------------------------------------------

/// Run the orchestrator event loop to completion.
///
/// Dispatches ready tasks, processes agent events (completions, errors,
/// plan modifications, progress), and handles timeouts. Returns a
/// `PlanOutcome` when all tasks reach a terminal state.
pub(crate) async fn run_orchestrator(
    mut plan: Plan,
    rx: &mut mpsc::Receiver<OrchestratorEvent>,
    agent_senders: &HashMap<AgentId, mpsc::Sender<OrchestratorEvent>>,
    role_to_agent: &HashMap<String, AgentId>,
    config: &OrchestratorConfig,
) -> Result<PlanOutcome, String> {
    if plan.tasks.is_empty() {
        tracing::debug!("run_orchestrator — empty plan, returning immediately");
        return Ok(plan_outcome(&plan));
    }

    // Log full plan structure at startup.
    for t in plan.tasks.values() {
        tracing::info!(
            task = %t.id,
            role = %t.role,
            description = %t.description,
            depends_on = ?t.depends_on,
            "run_orchestrator — plan task"
        );
    }
    tracing::info!(
        goal = %plan.goal,
        task_count = plan.tasks.len(),
        max_retries = config.max_retries,
        timeout_secs = config.task_timeout.as_secs(),
        max_tokens = config.max_tokens_per_run,
        "run_orchestrator — starting event loop"
    );

    let planner_ref = config.planner.as_deref();
    let mut modification_count: u32 = 0;
    let mut run_budget = RunBudget::new(config.max_tokens_per_run);

    // Initial dispatch.
    let ready = plan.dispatch_ready_tasks();
    tracing::debug!(ready_count = ready.len(), "run_orchestrator — initial dispatch");
    for task_id in ready {
        dispatch_task(&mut plan, &task_id, agent_senders, role_to_agent, planner_ref).await?;
    }

    if plan.is_complete() {
        return Ok(plan_outcome(&plan));
    }

    loop {
        let deadline = earliest_running_deadline(&plan, config.task_timeout);

        tokio::select! {
            event = rx.recv() => {
                match event {
                    Some(event) => {
                        match event {
                            OrchestratorEvent::TaskCompletion {
                                task_id,
                                agent_id: completing_agent,
                                artifacts,
                                output,
                                token_usage,
                                duration,
                                ..
                            } => {
                                run_budget.record(&token_usage);
                                let output_preview = if output.len() > 500 {
                                    format!("{}...[{} chars total]", &output[..500], output.len())
                                } else {
                                    output.clone()
                                };
                                tracing::info!(
                                    task = %task_id,
                                    agent = %completing_agent,
                                    duration_ms = duration.as_millis() as u64,
                                    output_len = output.len(),
                                    output_preview = %output_preview,
                                    artifact_count = artifacts.len(),
                                    artifact_keys = ?artifacts.keys().collect::<Vec<_>>(),
                                    input_tokens = token_usage.input_tokens,
                                    output_tokens = token_usage.output_tokens,
                                    budget_used = run_budget.used_tokens(),
                                    budget_max = run_budget.max_tokens,
                                    "run_orchestrator — TaskCompletion received"
                                );

                                if let Some(task) = plan.tasks.get_mut(&task_id) {
                                    task.status = TaskStatus::Completed;
                                    task.last_error = None; // Clear on success (may have been set by prior failed attempt).
                                    // Extract structured output block fields and merge into artifacts.
                                    // This captures LLM-computed values (token_id, ipnft_symbol, project_url)
                                    // that don't appear in tool envelopes.
                                    if let Some(block) = extract_structured_output(&output) {
                                        let fields = parse_structured_block_fields(&block);
                                        for (k, v) in fields {
                                            tracing::info!(
                                                task = %task_id,
                                                field = %k,
                                                value = %v,
                                                "run_orchestrator — adding structured field to task artifacts"
                                            );
                                            task.artifacts.insert(k, Value::String(v));
                                        }
                                    }
                                    task.output = Some(output);
                                    for (k, v) in artifacts {
                                        task.artifacts.insert(k, v);
                                    }
                                }
                                if run_budget.exceeded() {
                                    tracing::warn!(
                                        used = run_budget.used_tokens(),
                                        max = run_budget.max_tokens,
                                        "Run token budget exceeded — skipping remaining tasks"
                                    );
                                    // Skip all pending/ready tasks.
                                    let pending_ids: Vec<TaskId> = plan
                                        .tasks
                                        .values()
                                        .filter(|t| matches!(t.status, TaskStatus::Pending | TaskStatus::Ready))
                                        .map(|t| t.id.clone())
                                        .collect();
                                    for id in pending_ids {
                                        plan.tasks.get_mut(&id).unwrap().status = TaskStatus::Skipped;
                                    }
                                } else {
                                    let ready = plan.dispatch_ready_tasks();
                                    for tid in ready {
                                        dispatch_task(&mut plan, &tid, agent_senders, role_to_agent, planner_ref).await?;
                                    }
                                }
                            }
                            OrchestratorEvent::TaskError {
                                task_id,
                                agent_id: failing_agent,
                                retryable,
                                error,
                                ..
                            } => {
                                let attempt = plan.tasks.get(&task_id).map(|t| t.attempt).unwrap_or(0);
                                tracing::info!(
                                    task = %task_id,
                                    agent = %failing_agent,
                                    error = %error,
                                    retryable = retryable,
                                    attempt = attempt,
                                    max_retries = config.max_retries,
                                    "run_orchestrator — TaskError received"
                                );
                                handle_task_error(&mut plan, &task_id, retryable, &error, config);
                                let ready = plan.dispatch_ready_tasks();
                                for tid in ready {
                                    dispatch_task(&mut plan, &tid, agent_senders, role_to_agent, planner_ref).await?;
                                }
                            }
                            OrchestratorEvent::PlanModificationRequest {
                                kind,
                                requested_by,
                                reason,
                                ..
                            } => {
                                // Guard: max modifications cap.
                                if modification_count >= config.max_modifications {
                                    tracing::warn!(
                                        count = modification_count,
                                        "Plan modification rejected: limit reached"
                                    );
                                } else if is_self_referential_add(&kind, &requested_by, role_to_agent) {
                                    tracing::warn!(
                                        agent = %requested_by,
                                        "Plan modification rejected: agent cannot add task for itself"
                                    );
                                } else if let Err(e) = plan.apply_modification(&kind) {
                                    tracing::warn!(error = %e, "Plan modification rejected");
                                } else {
                                    modification_count += 1;
                                    tracing::info!(
                                        revision = plan.revision,
                                        modifications = modification_count,
                                        reason = %reason,
                                        "Plan modified"
                                    );
                                    let ready = plan.dispatch_ready_tasks();
                                    for tid in ready {
                                        dispatch_task(&mut plan, &tid, agent_senders, role_to_agent, planner_ref).await?;
                                    }
                                }
                            }
                            OrchestratorEvent::Progress {
                                task_id,
                                message,
                                percent,
                                ..
                            } => {
                                tracing::info!(
                                    task = %task_id,
                                    progress = ?percent,
                                    "{}",
                                    message
                                );
                            }
                            _ => {}
                        }
                    }
                    None => {
                        if plan.is_complete() {
                            break;
                        }
                        return Err("Event bus closed before plan completed".into());
                    }
                }
            }
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                let timed_out = find_timed_out_tasks(&plan, config.task_timeout);
                for task_id in timed_out {
                    handle_task_error(&mut plan, &task_id, true, "Task timed out", config);
                }
                let ready = plan.dispatch_ready_tasks();
                for tid in ready {
                    dispatch_task(&mut plan, &tid, agent_senders, role_to_agent, planner_ref).await?;
                }
            }
            // Cancel flag polling — wakes up every 500ms to check /stop.
            _ = async {
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    if config.cancel.as_ref().is_some_and(|f| f.load(std::sync::atomic::Ordering::Relaxed)) {
                        return;
                    }
                }
            }, if config.cancel.is_some() => {
                tracing::info!("run_orchestrator — /stop received, cancelling orchestration");
                // Skip all pending/ready tasks.
                let pending_ids: Vec<TaskId> = plan
                    .tasks
                    .values()
                    .filter(|t| matches!(t.status, TaskStatus::Pending | TaskStatus::Ready))
                    .map(|t| t.id.clone())
                    .collect();
                for id in &pending_ids {
                    tracing::info!(task = %id, "run_orchestrator — skipping task due to /stop");
                    plan.tasks.get_mut(id).unwrap().status = TaskStatus::Skipped;
                }
                // Mark running tasks as failed (their executors will also see the cancel flag).
                let running_ids: Vec<TaskId> = plan
                    .tasks
                    .values()
                    .filter(|t| t.status == TaskStatus::Running)
                    .map(|t| t.id.clone())
                    .collect();
                for id in &running_ids {
                    tracing::info!(task = %id, "run_orchestrator — cancelling running task due to /stop");
                    plan.tasks.get_mut(id).unwrap().status = TaskStatus::Failed;
                }
                // Send Shutdown to all agent workers.
                for (aid, tx) in agent_senders {
                    let _ = tx.send(OrchestratorEvent::Shutdown {
                        reason: "User requested /stop".into(),
                        timestamp: Utc::now(),
                    }).await;
                    tracing::info!(agent = %aid, "run_orchestrator — sent Shutdown to agent");
                }
                break;
            }
        }

        if plan.is_complete() {
            let completed = plan.tasks.values().filter(|t| t.status == TaskStatus::Completed).count();
            let failed = plan.tasks.values().filter(|t| t.status == TaskStatus::Failed).count();
            let skipped = plan.tasks.values().filter(|t| t.status == TaskStatus::Skipped).count();
            tracing::info!(
                completed = completed,
                failed = failed,
                skipped = skipped,
                total_tokens = run_budget.used_tokens(),
                revision = plan.revision,
                modifications = modification_count,
                "run_orchestrator — plan complete"
            );
            break;
        }
    }

    Ok(plan_outcome(&plan))
}
