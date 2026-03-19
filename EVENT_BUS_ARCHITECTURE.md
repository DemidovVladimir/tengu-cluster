# Event-Bus Orchestration Architecture

Design document for replacing the static batch-execution orchestrator with a
reactive event-bus model. All Rust type examples target the existing crate
layout (`src/application/`, `src/adapters/`, `src/domain/`).

---

## 1. Problem Statement

Six concrete problems with the current `src/adapters/orchestrator.rs`:

| # | Problem | Where it hurts |
|---|---------|----------------|
| 1 | **Static plan** — `generate_plan()` produces a fixed task list up-front; the orchestrator cannot add, remove, or re-order tasks after execution starts. | A failing step that could be retried with different parameters, or a step whose output reveals the need for a new task, forces the user to re-submit. |
| 2 | **Context bloat** — `build_step_context()` dumps *every* prior `StepResult` (truncated to 8 kB each) into the next agent's prompt. With 6 completed steps the context prefix alone is ~48 kB of irrelevant text. | Token waste, slower inference, and prompt-injection surface area. |
| 3 | **Rigid dependencies** — `resolve_execution_order()` computes a topological sort into `Vec<Vec<usize>>` batches *before* execution. A task that finishes early cannot unblock dependents until the entire batch completes. | Artificial serialization — 3 tasks in a batch, 2 finish in 2 s, 1 takes 30 s → the 2 fast outputs sit idle for 28 s. |
| 4 | **Flat RunState** — `RunState` (run_state.rs) tracks `task_outputs: HashMap`, `task_records: HashMap`, `artifacts: BTreeMap`, and `stop_reason`. While more structured than a single bag, it has no per-agent scoping, no TTL, and no visibility rules. Every agent sees every artifact via `artifacts_for_prompt()`. | No isolation between agents; impossible to enforce least-privilege data routing. |
| 5 | **Duplicated orchestration logic** — The CLI orchestrator (`boot_orchestrator`) and the Telegram adapter (`telegram_runtime.rs`) both implement their own dispatch loops, task tracking, and result formatting. | Bug fixes and new features must be applied twice. |
| 6 | **No agent-to-orchestrator signaling** — Agents have no way to request plan changes, report progress, or signal that they need input from another agent mid-execution. The orchestrator only learns about outcomes *after* a task completes. | No adaptive behavior; no progress visibility. |

---

## 2. Event Types

All messages on the bus are variants of a single `OrchestratorEvent` enum.
Each carries a `Utc` timestamp and a string `correlation_id` (the plan's
revision id) for tracing.

```rust
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::HashMap;

/// Unique identifier for a task within a plan.
pub type TaskId = String;

/// Unique identifier for an agent worker.
pub type AgentId = String;

/// Every message on the bus.
#[derive(Debug, Clone)]
pub enum OrchestratorEvent {
    /// Orchestrator → Agent: here is your assignment.
    TaskAssignment {
        task_id: TaskId,
        agent_id: AgentId,
        description: String,
        /// Curated data from upstream tasks — NOT a dump of all outputs.
        context: HashMap<String, Value>,
        correlation_id: String,
        timestamp: DateTime<Utc>,
    },

    /// Agent → Orchestrator: task finished successfully.
    TaskCompletion {
        task_id: TaskId,
        agent_id: AgentId,
        output: String,
        /// Structured artifacts extracted from tool results.
        artifacts: HashMap<String, Value>,
        token_usage: TokenUsage,
        /// Wall-clock execution time (measured inside agent_worker,
        /// from permit acquisition to result — excludes queue wait).
        duration: Duration,
        correlation_id: String,
        timestamp: DateTime<Utc>,
    },

    /// Agent → Orchestrator: task failed.
    TaskError {
        task_id: TaskId,
        agent_id: AgentId,
        error: String,
        retryable: bool,
        correlation_id: String,
        timestamp: DateTime<Utc>,
    },

    /// Agent → Orchestrator: request to modify the live plan.
    PlanModificationRequest {
        requested_by: AgentId,
        kind: PlanModification,
        reason: String,
        correlation_id: String,
        timestamp: DateTime<Utc>,
    },

    /// Agent → Orchestrator: progress update (non-blocking).
    Progress {
        task_id: TaskId,
        agent_id: AgentId,
        message: String,
        percent: Option<u8>,
        timestamp: DateTime<Utc>,
    },

    /// Orchestrator → Agent: cancel a running task.
    TaskCancellation {
        task_id: TaskId,
        reason: String,
        timestamp: DateTime<Utc>,
    },

    /// Orchestrator → All: graceful shutdown.
    Shutdown {
        reason: String,
        timestamp: DateTime<Utc>,
    },
}

#[derive(Debug, Clone)]
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

#[derive(Debug, Clone)]
pub enum PlanModification {
    /// Add a new task to the live plan.
    AddTask {
        id: TaskId,
        role: String,
        description: String,
        depends_on: Vec<TaskId>,
    },
    /// Remove a pending task.
    RemoveTask { id: TaskId },
    /// Change a task's dependencies.
    UpdateDependencies {
        id: TaskId,
        new_depends_on: Vec<TaskId>,
    },
}
```

---

## 3. Bus Topology

### Star topology with dedicated mpsc channels

```
                    ┌─────────────────┐
                    │   Orchestrator   │
                    │   (event loop)   │
                    └──┬──┬──┬──┬──┬──┘
          Tx(A)  Tx(B)│  │  │  │  │ Tx(N)
           ┌─────────┘  │  │  │  └─────────┐
           ▼             ▼  ▼  ▼            ▼
        ┌──────┐    ┌──────┐  ┌──────┐  ┌──────┐
        │Agent │    │Agent │  │Agent │  │Agent │
        │  A   │    │  B   │  │  C   │  │  N   │
        └──┬───┘    └──┬───┘  └──┬───┘  └──┬───┘
           │           │         │          │
           └───────────┴────┬────┴──────────┘
                            │
                     shared Tx(orch)
                            │
                            ▼
                    ┌─────────────────┐
                    │  Orchestrator   │
                    │  Rx (mpsc)      │
                    └─────────────────┘
```

**Orchestrator → Agent**: each agent gets a dedicated `mpsc::Sender<OrchestratorEvent>`.
The orchestrator holds all senders. This allows targeted dispatch — only the
assigned agent receives a `TaskAssignment`.

**Agent → Orchestrator**: all agents share a single `mpsc::Sender<OrchestratorEvent>`
(cloned per agent). The orchestrator holds the single receiver.

### Why not broadcast?

`tokio::sync::broadcast` delivers every message to every subscriber. Since
agents should only see their own assignments, broadcast would require each
agent to filter by `agent_id` — wasted CPU and a data-isolation footgun.

### Backpressure

Both channel directions use bounded `mpsc::channel(32)`. If an agent's inbox
is full, the orchestrator's `send()` awaits, naturally throttling dispatch.
If the orchestrator's inbox is full, agents block on `send()` — this is
fine because an agent has nothing else to do after sending `TaskCompletion`.

### Ordering

Per-agent ordering is guaranteed by mpsc FIFO. Cross-agent ordering is
irrelevant — the orchestrator serializes all decisions through its own
event loop.

---

## 4. Orchestrator State Machine

Replace the current `Vec<Vec<usize>>` batch model with a mutable `LivePlan`.

```rust
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskStatus {
    Pending,
    Ready,
    Running,
    Completed,
    Failed,
    Skipped,
}

#[derive(Debug, Clone)]
pub struct LiveTask {
    pub id: TaskId,
    pub role: String,
    pub description: String,
    pub depends_on: Vec<TaskId>,
    pub status: TaskStatus,
    pub output: Option<String>,
    pub artifacts: HashMap<String, Value>,
    pub assigned_agent: Option<AgentId>,
    /// Set by dispatch_task() when status transitions to Running.
    /// Used by timeout detection (§8) to compute elapsed wall-clock time.
    pub started_at: Option<Instant>,
    pub attempt: u32,
}

#[derive(Debug)]
pub struct LivePlan {
    pub goal: String,
    pub tasks: HashMap<TaskId, LiveTask>,
    /// Monotonically increasing revision counter. Bumped on every mutation.
    pub revision: u64,
    pub created_at: DateTime<Utc>,
}

impl LivePlan {
    /// Mark all tasks whose dependencies are fully satisfied as Ready.
    /// Returns the set of task IDs that became ready.
    pub fn dispatch_ready_tasks(&mut self) -> Vec<TaskId> {
        let completed: HashSet<&TaskId> = self
            .tasks
            .values()
            .filter(|t| t.status == TaskStatus::Completed)
            .map(|t| &t.id)
            .collect();

        let mut newly_ready = Vec::new();
        for task in self.tasks.values_mut() {
            if task.status != TaskStatus::Pending {
                continue;
            }
            let deps_met = task.depends_on.iter().all(|dep| completed.contains(dep));
            if deps_met {
                task.status = TaskStatus::Ready;
                newly_ready.push(task.id.clone());
            }
        }
        self.revision += 1;
        newly_ready
    }

    /// Apply a plan modification. Returns Err if the modification is invalid.
    pub fn apply_modification(&mut self, m: &PlanModification) -> Result<(), String> {
        match m {
            PlanModification::AddTask {
                id,
                role,
                description,
                depends_on,
            } => {
                if self.tasks.contains_key(id) {
                    return Err(format!("Task '{}' already exists", id));
                }
                // Verify all deps exist.
                for dep in depends_on {
                    if !self.tasks.contains_key(dep) {
                        return Err(format!("Dependency '{}' not found", dep));
                    }
                }
                self.tasks.insert(
                    id.clone(),
                    LiveTask {
                        id: id.clone(),
                        role: role.clone(),
                        description: description.clone(),
                        depends_on: depends_on.clone(),
                        status: TaskStatus::Pending,
                        output: None,
                        artifacts: HashMap::new(),
                        assigned_agent: None,
                        started_at: None,
                        attempt: 0,
                    },
                );
            }
            PlanModification::RemoveTask { id } => {
                let task = self
                    .tasks
                    .get(id)
                    .ok_or_else(|| format!("Task '{}' not found", id))?;
                if task.status == TaskStatus::Running {
                    return Err(format!("Cannot remove running task '{}'", id));
                }
                // Check nothing depends on it.
                let dependents: Vec<&str> = self
                    .tasks
                    .values()
                    .filter(|t| t.depends_on.contains(id))
                    .map(|t| t.id.as_str())
                    .collect();
                if !dependents.is_empty() {
                    return Err(format!(
                        "Cannot remove '{}' — depended on by: {}",
                        id,
                        dependents.join(", ")
                    ));
                }
                self.tasks.remove(id);
            }
            PlanModification::UpdateDependencies {
                id,
                new_depends_on,
            } => {
                let task = self
                    .tasks
                    .get_mut(id)
                    .ok_or_else(|| format!("Task '{}' not found", id))?;
                if task.status != TaskStatus::Pending {
                    return Err(format!("Can only update deps on Pending tasks, '{}' is {:?}", id, task.status));
                }
                task.depends_on = new_depends_on.clone();
            }
        }
        self.revision += 1;
        Ok(())
    }

    /// True when all tasks are in a terminal state.
    pub fn is_complete(&self) -> bool {
        self.tasks.values().all(|t| {
            matches!(
                t.status,
                TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Skipped
            )
        })
    }
}
```

### Event Loop

The orchestrator runs a single `tokio::select!` loop:

```rust
pub async fn run_orchestrator(
    mut plan: LivePlan,
    mut rx: mpsc::Receiver<OrchestratorEvent>,
    agent_senders: &HashMap<AgentId, mpsc::Sender<OrchestratorEvent>>,
    role_to_agent: &HashMap<String, AgentId>,
    config: &OrchestratorConfig,
) -> Result<PlanOutcome> {
    // Initial dispatch.
    let ready = plan.dispatch_ready_tasks();
    for task_id in ready {
        dispatch_task(&mut plan, &task_id, agent_senders, role_to_agent).await?;
    }

    loop {
        tokio::select! {
            Some(event) = rx.recv() => {
                match event {
                    OrchestratorEvent::TaskCompletion {
                        task_id,
                        artifacts,
                        output,
                        ..
                    } => {
                        let task = plan.tasks.get_mut(&task_id).unwrap();
                        task.status = TaskStatus::Completed;
                        task.output = Some(output);
                        task.artifacts = artifacts;

                        // Dispatch newly unblocked tasks.
                        let ready = plan.dispatch_ready_tasks();
                        for tid in ready {
                            dispatch_task(&mut plan, &tid, agent_senders, role_to_agent).await?;
                        }
                    }

                    OrchestratorEvent::TaskError {
                        task_id,
                        retryable,
                        error,
                        ..
                    } => {
                        handle_task_error(&mut plan, &task_id, retryable, &error, config).await;
                        let ready = plan.dispatch_ready_tasks();
                        for tid in ready {
                            dispatch_task(&mut plan, &tid, agent_senders, role_to_agent).await?;
                        }
                    }

                    OrchestratorEvent::PlanModificationRequest {
                        kind,
                        reason,
                        ..
                    } => {
                        handle_plan_modification(&mut plan, kind, &reason, config).await;
                    }

                    OrchestratorEvent::Progress { task_id, message, percent, .. } => {
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
        }

        if plan.is_complete() {
            break;
        }
    }

    Ok(plan_outcome(&plan))
}
```

### Task state transitions

`dispatch_ready_tasks()` handles **Pending → Ready**. The missing
transition — **Ready → Running** — happens in `dispatch_task()`, which
also records the assigned agent and start time:

```rust
async fn dispatch_task(
    plan: &mut LivePlan,
    task_id: &TaskId,
    agent_senders: &HashMap<AgentId, mpsc::Sender<OrchestratorEvent>>,
    role_to_agent: &HashMap<String, AgentId>,
) -> Result<()> {
    // Extract what we need before mutating.
    let (role, raw_description) = {
        let task = plan.tasks.get(task_id).unwrap();
        (task.role.clone(), task.description.clone())
    };
    let agent_id = role_to_agent
        .get(&role)
        .ok_or_else(|| anyhow::anyhow!("No agent for role '{}'", role))?
        .clone();

    // Build context from dependency outputs (immutable borrow on plan).
    let context = build_task_context(plan, task_id);

    // Ready → Running transition.
    let task = plan.tasks.get_mut(task_id).unwrap();
    task.status = TaskStatus::Running;
    task.assigned_agent = Some(agent_id.clone());
    task.started_at = Some(Instant::now());

    // The description field carries the FULLY RENDERED prompt.
    // format_task_prompt() is called HERE, once. Workers use it as-is.
    let rendered_prompt = format_task_prompt(&plan.goal, &raw_description, &context);

    agent_senders[&agent_id]
        .send(OrchestratorEvent::TaskAssignment {
            task_id: task_id.clone(),
            agent_id: agent_id.clone(),
            description: rendered_prompt,
            context,
            correlation_id: plan.revision.to_string(),
            timestamp: Utc::now(),
        })
        .await?;

    Ok(())
}
```

Complete lifecycle:

```
Pending ──dispatch_ready_tasks()──► Ready ──dispatch_task()──► Running
                                                                  │
                                              TaskCompletion ◄────┤
                                                  │               │
                                              Completed       TaskError
                                                              │       │
                                                          retryable  permanent
                                                              │       │
                                                          Pending   Failed
                                                         (attempt++)    │
                                                                   cascade_skip()
                                                                        │
                                                                    Skipped
```

The `Running` status is what enables:
- Removal guard (§7, line 295): `Cannot remove running task`
- Timeout detection (§8): `find_timed_out_tasks()` checks
  `LiveTask.started_at` against `config.task_timeout_secs` for all
  Running tasks

**Emergent parallelism**: `dispatch_ready_tasks()` is called after every
completion. If tasks B and C both depend only on A, they are both dispatched
the instant A completes — no batch boundary needed. Parallelism emerges
from the dependency graph, not from pre-computed batches.

---

## 5. Agent Worker Lifecycle

Each agent worker is a long-lived `tokio::spawn` task that listens on its
dedicated inbox.

```rust
pub async fn agent_worker(
    agent_id: AgentId,
    runtime: Arc<AgentRuntime>,
    mut inbox: mpsc::Receiver<OrchestratorEvent>,
    outbox: mpsc::Sender<OrchestratorEvent>,
    secret_registry: Arc<SecretRegistry>,
) {
    while let Some(event) = inbox.recv().await {
        match event {
            OrchestratorEvent::TaskAssignment {
                task_id,
                description,
                correlation_id,
                ..
            } => {
                // `description` is the fully rendered prompt, assembled
                // by dispatch_task() via format_task_prompt(). The worker
                // passes it through to execute_agent_task() as-is.
                let exec_start = Instant::now();

                match execute_agent_task(&runtime, &description, &secret_registry).await {
                    Ok((output, tool_outcomes)) => {
                        let artifacts = extract_artifacts(&tool_outcomes);
                        let _ = outbox
                            .send(OrchestratorEvent::TaskCompletion {
                                task_id,
                                agent_id: agent_id.clone(),
                                output,
                                artifacts,
                                token_usage: TokenUsage {
                                    input_tokens: 0,
                                    output_tokens: 0,
                                },
                                duration: exec_start.elapsed(),
                                correlation_id,
                                timestamp: Utc::now(),
                            })
                            .await;
                    }
                    Err(e) => {
                        let _ = outbox
                            .send(OrchestratorEvent::TaskError {
                                task_id,
                                agent_id: agent_id.clone(),
                                error: e.to_string(),
                                retryable: is_retryable_error(&e),
                                correlation_id,
                                timestamp: Utc::now(),
                            })
                            .await;
                    }
                }
            }

            OrchestratorEvent::TaskCancellation { task_id, .. } => {
                tracing::info!(agent = %agent_id, task = %task_id, "Task cancelled");
                // If a cancel flag exists for the running task, set it.
                // The engine_runtime already checks AtomicBool cancel flags.
            }

            OrchestratorEvent::Shutdown { .. } => {
                tracing::info!(agent = %agent_id, "Shutting down");
                break;
            }

            _ => {}
        }
    }
}
```

Workers reuse the existing `AgentRuntime` struct and `execute_agent_task()`
function. The only change is that the caller is now the worker loop instead
of a JoinSet closure.

---

## 6. Data Routing

### Problem with the current approach

`build_step_context()` in `orchestrator.rs:761` concatenates ALL prior
step outputs (truncated to `MAX_STEP_CONTEXT_CHARS = 8000` each) into a
single prompt string. This is wasteful — an agent minting an IP-NFT
doesn't need the full text of a literature review.

### Two-tier routing

**Tier 1: Automatic envelope extraction** (zero-LLM-cost)

When a task completes, `parse_tool_result_envelope()` extracts structured
artifacts (IDs, URLs, hashes). These are stored in the `LiveTask.artifacts`
map. When dispatching a downstream task, the orchestrator builds a `context`
map containing *only* the artifacts from declared dependencies:

```rust
fn build_task_context(
    plan: &LivePlan,
    task_id: &TaskId,
) -> HashMap<String, Value> {
    let task = &plan.tasks[task_id];
    let mut context = HashMap::new();
    for dep_id in &task.depends_on {
        if let Some(dep_task) = plan.tasks.get(dep_id) {
            // Include structured artifacts, keyed by dep task id.
            if !dep_task.artifacts.is_empty() {
                context.insert(
                    dep_id.clone(),
                    serde_json::to_value(&dep_task.artifacts).unwrap(),
                );
            }
        }
    }
    context
}
```

**Tier 2: LLM-based selective forwarding** (for unstructured outputs)

When a dependency's output has no structured artifacts (e.g., a research
summary), the orchestrator asks the planner LLM to extract only the
relevant subset for the downstream task:

```rust
async fn summarize_for_downstream(
    planner: &dyn Engine,
    upstream_output: &str,
    downstream_task: &str,
) -> Result<String> {
    let prompt = format!(
        "Extract ONLY the information from the upstream output that is \
         needed for the following task. Be concise.\n\n\
         ## Upstream Output\n{}\n\n\
         ## Downstream Task\n{}",
        upstream_output, downstream_task
    );
    // Single planner call with no tools, capped at 500 output tokens.
    let response = collect_engine_response(
        planner, &[Message::user(&prompt)], &[], &EngineContext::default(),
        None, None, None, Some(2000),
    ).await?;
    Ok(response.text)
}
```

This replaces the `MAX_STEP_CONTEXT_CHARS` truncation with intelligent
selection. The LLM cost is small (one short call per unstructured
handoff) but the token savings downstream are large.

### When each tier applies

| Condition | Tier used |
|-----------|-----------|
| Upstream produced structured artifacts | Tier 1 only (free) |
| Upstream produced text only, < 500 chars | Tier 1: include full text |
| Upstream produced text only, ≥ 500 chars | Tier 2: LLM summarization |

---

## 7. Dynamic Re-Planning

### How it works

1. An agent sends `PlanModificationRequest` during or after task execution.
2. The orchestrator evaluates the request:
   - **Structural validation**: Does the modification create cycles? Do referenced tasks exist?
   - **LLM evaluation** (optional, configurable): The planner LLM reviews the request against the original goal and current plan state.
3. If approved, `LivePlan::apply_modification()` mutates the plan and bumps `revision`.
4. `dispatch_ready_tasks()` runs to check if new tasks became ready.

```rust
async fn handle_plan_modification(
    plan: &mut LivePlan,
    modification: PlanModification,
    reason: &str,
    config: &OrchestratorConfig,
) {
    // Structural validation.
    if let Err(e) = plan.apply_modification(&modification) {
        tracing::warn!(error = %e, "Plan modification rejected (structural)");
        return;
    }

    tracing::info!(
        revision = plan.revision,
        reason = %reason,
        "Plan modified"
    );
}
```

### Revision tracking

Every `LivePlan` mutation increments `revision`. Events carry a
`correlation_id` that maps to the plan revision at dispatch time. If an
agent sends a `TaskCompletion` for a task that has since been removed
(plan moved on), the orchestrator drops the event.

### Guards against runaway re-planning

- **Max modifications per plan**: configurable cap (default: 10). After
  this limit, modification requests are rejected.
- **No self-referential adds**: an agent cannot add a task assigned to
  itself (prevents infinite self-spawning).
- **Cycle detection**: `apply_modification` must verify the new DAG is
  still acyclic before committing.

---

## 8. Error Handling

### Retry strategy

```rust
async fn handle_task_error(
    plan: &mut LivePlan,
    task_id: &TaskId,
    retryable: bool,
    error: &str,
    config: &OrchestratorConfig,
) {
    let task = plan.tasks.get_mut(task_id).unwrap();

    if retryable && task.attempt < config.max_retries {
        task.attempt += 1;
        task.status = TaskStatus::Pending; // Will be picked up by dispatch_ready_tasks.
        tracing::info!(
            task = %task_id,
            attempt = task.attempt,
            "Retrying failed task"
        );
    } else {
        task.status = TaskStatus::Failed;
        tracing::error!(task = %task_id, error = %error, "Task failed permanently");

        // Cascade-skip: mark all transitive dependents as Skipped.
        cascade_skip(plan, task_id);
    }
}

fn cascade_skip(plan: &mut LivePlan, failed_id: &TaskId) {
    let dependents: Vec<TaskId> = plan
        .tasks
        .values()
        .filter(|t| {
            t.depends_on.contains(failed_id)
                && matches!(t.status, TaskStatus::Pending | TaskStatus::Ready)
        })
        .map(|t| t.id.clone())
        .collect();

    for dep_id in dependents {
        plan.tasks.get_mut(&dep_id).unwrap().status = TaskStatus::Skipped;
        cascade_skip(plan, &dep_id);
    }
}
```

### Retryable classification

```rust
fn is_retryable_error(error: &anyhow::Error) -> bool {
    let msg = error.to_string();
    msg.contains("timed out")
        || msg.contains("rate limit")
        || msg.contains("HTTP 429")
        || msg.contains("HTTP 502")
        || msg.contains("HTTP 503")
}
```

### Timeout detection

**DAG mode**: `LiveTask.started_at` (set by `dispatch_task()` on the
Ready → Running transition) provides the wall-clock anchor. This
measures from dispatch, not from actual execution start — in DAG mode
these are nearly identical because there is no semaphore queue. The
orchestrator event loop uses `tokio::select!` with a sleep branch that
fires at the earliest running task's deadline:

**Fan-out mode**: timeout is enforced inside `agent_worker()` via
`tokio::time::timeout()`, measured from semaphore permit acquisition
(§11.4). The orchestrator does not use `LiveTask.started_at` for fan-out
— it relies on the worker-side timeout + a safety deadline (§11.7).

```rust
fn earliest_running_deadline(plan: &LivePlan, timeout: Duration) -> Instant {
    plan.tasks
        .values()
        .filter(|t| t.status == TaskStatus::Running)
        .filter_map(|t| t.started_at.map(|s| s + timeout))
        .min()
        .unwrap_or_else(|| Instant::now() + timeout)
}

fn find_timed_out_tasks(plan: &LivePlan, timeout: Duration) -> Vec<TaskId> {
    let now = Instant::now();
    plan.tasks
        .values()
        .filter(|t| t.status == TaskStatus::Running)
        .filter(|t| t.started_at.is_some_and(|s| now.duration_since(s) >= timeout))
        .map(|t| t.id.clone())
        .collect()
}

// Inside the event loop:
let deadline = earliest_running_deadline(&plan, config.task_timeout);

tokio::select! {
    Some(event) = rx.recv() => { /* handle event */ }
    _ = tokio::time::sleep_until(deadline) => {
        let timed_out = find_timed_out_tasks(&plan, config.task_timeout);
        for task_id in timed_out {
            handle_task_error(
                &mut plan, &task_id, true, "Task timed out", config,
            ).await;
        }
    }
}
```

### Agent isolation

A crashing agent worker does not affect other agents. Each worker runs in
its own `tokio::spawn` with a `catch_unwind` guard:

```rust
tokio::spawn(async move {
    if let Err(e) = std::panic::AssertUnwindSafe(
        agent_worker(agent_id.clone(), runtime, inbox, outbox, secrets)
    )
    .catch_unwind()
    .await
    {
        tracing::error!(agent = %agent_id, "Agent worker panicked: {:?}", e);
    }
});
```

---

## 9. What Stays vs What Changes

| Component | Current | After | Notes |
|-----------|---------|-------|-------|
| `task_planner::generate_plan()` | Produces `Vec<PlanTask>` | Produces `Vec<PlanTask>` (unchanged) | Initial plan generation is the same |
| `task_planner::resolve_execution_order()` | Returns `Vec<Vec<usize>>` batches | **Removed** | Replaced by `dispatch_ready_tasks()` |
| `task_planner::repair_plan_dependencies()` | Fixes plan before execution | Unchanged — runs before `LivePlan` construction | Still needed for LLM-generated plans |
| `task_planner::validate_plan_dependencies()` | Validates before execution | Unchanged | |
| `task_planner::classify_request()` | Routes single vs multi | Unchanged | |
| `AgentRuntime` struct | Per-agent engine + tools | Unchanged | |
| `execute_agent_task()` | Called from JoinSet closure | Called from agent worker loop | Same function, different caller |
| `build_step_context()` | Dumps all prior outputs | **Removed** | Replaced by `build_task_context()` (§6) |
| `StepResult` struct | Collects outputs in Vec | **Removed** | Replaced by `LiveTask.output` + `.artifacts` |
| `RunState` | Tracks task_outputs, task_records, artifacts (BTreeMap), stop_reason — no per-agent scoping | **Removed** | Artifacts live on `LiveTask`; `build_task_context()` assembles per-task with visibility scoped to declared dependencies |
| `boot_orchestrator()` | 700-line monolith with stdin loop | Split into `EventBus::new()` + `run_orchestrator()` + `agent_worker()` | Stdin loop becomes a thin I/O adapter |
| JoinSet batch execution | `set.spawn()` per batch | **Removed** | Replaced by persistent agent workers |
| `TaskStorePort` / `InMemoryTaskStore` | Stores task history | Unchanged — still stores completed tasks | |
| `TaskOrchestratorService` | Creates/assigns/completes tasks | Simplified — only persists to store | LivePlan owns runtime status |
| Telegram orchestration code | Separate dispatch loop | Reuses `run_orchestrator()` | UI adapter only renders events |
| `execute_agent_task()` return | `Result<(String, Vec<…>)>` — tokens discarded | `Result<AgentTaskResult>` — includes token counts | §11.3 Change 1 |
| `MemoryService` for task results | Direct `remember_with_metadata()` calls | **Moved** to storage agent (JSONL) | §12 — MemoryService stays for semantic recall |
| `MemoryToolExecutionAdapter` | Per-agent remember/recall tools | Unchanged for agent-level memory | Storage agent is separate from agent memory tools |
| `/stop` command | Cancels current chat turn via `AtomicBool` | Sends `Shutdown` event to all workers + cancels event loop | §13 |
| `/purge` command | Clears conversation + `MemoryStorePort::clear_all()` | Also clears storage agent JSONL + pool workspaces | §13 |
| `/team <goal>` command | Triggers batch orchestration | Routes to `run_orchestrator()` or `run_fan_out()` | §13 |
| Sandbox config loading | `load_sandbox_or()` → full Config | Unchanged — event bus is scoped to one sandbox process | §14 |
| Per-agent token budget | `max_tokens_per_flow` → `collect_engine_response()` | Unchanged per-agent + new aggregate run budget | §15 |

---

## 10. Implementation Phases

### Phase 1: Domain types and LivePlan (no behavior change)

- Add `src/domain/orchestrator_event.rs` with `OrchestratorEvent`, `PlanModification`, `TokenUsage`.
- Add `src/application/live_plan.rs` with `LivePlan`, `LiveTask`, `TaskStatus`.
- Add `src/application/event_bus.rs` with `EventBus`.
- Unit tests for `dispatch_ready_tasks()`, `apply_modification()`, cycle detection.
- **Zero changes** to `orchestrator.rs` — new code exists alongside old.

# !!! IMPORTANT
I should never guess as an ingeneer of what happens. So every step, every hop, everythign should be tracked with loggin. Logs should be reach enough to be able to deliver me enough info to be able to debug, what been passed where, what info stored and as much available info as possible, what function is executed, where and so on.

### Phase 2: Agent worker loop

- Add `src/application/agent_worker.rs` with the `agent_worker()` function.
- Workers call the existing `execute_agent_task()` — no engine changes.
- Unit test: send `TaskAssignment`, verify `TaskCompletion` arrives on outbox.

# !!! IMPORTANT
I should never guess as an ingeneer of what happens. So every step, every hop, everythign should be tracked with loggin. Logs should be reach enough to be able to deliver me enough info to be able to debug, what been passed where, what info stored and as much available info as possible, what function is executed, where and so on.

### Phase 3: Event-driven orchestrator core

- Add `src/application/event_orchestrator.rs` with `run_orchestrator()`.
- Implements the event loop from §4: receives events, updates LivePlan, dispatches ready tasks.
- Integrates Tier 1 data routing (`build_task_context()`).
- Integration test: construct a 3-task plan, run through event bus, verify execution order.

# !!! IMPORTANT
I should never guess as an ingeneer of what happens. So every step, every hop, everythign should be tracked with loggin. Logs should be reach enough to be able to deliver me enough info to be able to debug, what been passed where, what info stored and as much available info as possible, what function is executed, where and so on.

### Phase 4: Replace batch execution in CLI

- Modify `boot_orchestrator()` to use `EventBus` + `run_orchestrator()` instead of JoinSet batches.
- Remove `resolve_execution_order()` calls.
- Remove `build_step_context()` and `StepResult`.
- The stdin loop becomes: read input → `generate_plan()` → construct `LivePlan` → `run_orchestrator()`.

# !!! IMPORTANT
I should never guess as an ingeneer of what happens. So every step, every hop, everythign should be tracked with loggin. Logs should be reach enough to be able to deliver me enough info to be able to debug, what been passed where, what info stored and as much available info as possible, what function is executed, where and so on.

### Phase 5: Unified channel support

- Modify Telegram adapter to feed events into the same `run_orchestrator()`.
- Telegram-specific code only handles message rendering and user I/O.
- Remove duplicated orchestration logic from `telegram_runtime.rs`.

# !!! IMPORTANT
I should never guess as an ingeneer of what happens. So every step, every hop, everythign should be tracked with loggin. Logs should be reach enough to be able to deliver me enough info to be able to debug, what been passed where, what info stored and as much available info as possible, what function is executed, where and so on.

### Phase 6: Dynamic re-planning and Tier 2 routing

- Enable `PlanModificationRequest` handling in the orchestrator.
- Add LLM-based selective forwarding (Tier 2) for unstructured handoffs.
- Add configurable guards: max modifications, cycle detection, timeout-based re-dispatch.
- This phase is the only one that adds new LLM calls to the orchestrator.

# !!! IMPORTANT
I should never guess as an ingeneer of what happens. So every step, every hop, everythign should be tracked with loggin. Logs should be reach enough to be able to deliver me enough info to be able to debug, what been passed where, what info stored and as much available info as possible, what function is executed, where and so on.

### Phase 7: Storage agent

- Add `src/application/storage_agent.rs` with `storage_agent_worker()` (§12).
- JSONL backend: `StorageEntry` type, write/read/query/list operations.
- Wire into `EventBus` as a system agent with its own mpsc channel.
- Modify orchestrator event loop to route `TaskCompletion` outputs to storage.
- Replace direct `MemoryService` calls for structured data.
- Update `/purge` to clear JSONL alongside memory store.

# !!! IMPORTANT
I should never guess as an ingeneer of what happens. So every step, every hop, everythign should be tracked with loggin. Logs should be reach enough to be able to deliver me enough info to be able to debug, what been passed where, what info stored and as much available info as possible, what function is executed, where and so on.

### Phase 8: Fan-out and agent pools

- Add `[[agent_pools]]` config parsing and `expand_agent_pools()` (§11.6).
- Add `FanOutPlan`, `AgentPool`, `EnsembleReport` types (§11.5).
- Add `run_fan_out()` event loop (§11.7).
- Add `Semaphore` + `CircuitBreaker` concurrency control (§11.8).
- Wire `/team` command to detect fan-out vs DAG mode.
- Integration test: 5-model pool, verify all results collected.

# !!! IMPORTANT
I should never guess as an ingeneer of what happens. So every step, every hop, everythign should be tracked with loggin. Logs should be reach enough to be able to deliver me enough info to be able to debug, what been passed where, what info stored and as much available info as possible, what function is executed, where and so on.

### Phase 9: Commands and token boundaries

- Adapt `/stop` to send `Shutdown` to all workers (§13).
- Adapt `/purge` for storage agent + workspace cleanup (§13).
- Add `/agents` listing with pool/storage agent info (§13).
- Add aggregate `max_tokens_per_run` enforcement (§15).
- Add `/cost` aggregation across agent workers.

# !!! IMPORTANT
I should never guess as an ingeneer of what happens. So every step, every hop, everythign should be tracked with loggin. Logs should be reach enough to be able to deliver me enough info to be able to debug, what been passed where, what info stored and as much available info as possible, what function is executed, where and so on.

---

## 11. Fan-Out Ensemble Validation

This section validates whether the event-bus architecture (§1–11) supports
a second execution pattern: **fan-out**, where one orchestrator dispatches
the same job to N agents (100–200) running in parallel isolation and
collects their results for comparison.

References to *existing* code (e.g., `execute_agent_task()`,
`collect_engine_response()`, `MemoryService`) point to current codebase
symbols. References to *proposed* constructs (`EventBus`, `agent_worker()`,
`FanOutPlan`, `run_fan_out()`) are new types defined by this document.
Where existing code needs modification, this is called out explicitly
with before/after.

---

### 11.1 Why this matters — product-owner view

| Use case | Who cares | Value |
|----------|-----------|-------|
| **Model benchmarking** | Platform operator | "Which of our 20 OpenRouter models answers DeSci questions best?" — answers model selection with real data instead of vibes. |
| **Prompt regression testing** | Skill authors | After editing a SKILL.md system prompt, run the same 10 test questions across models. Detect regressions before merge. |
| **Consensus verification** | High-stakes DeSci workflows | Before minting an IP-NFT, ask 5 independent models to verify the molecule data. Majority agreement = proceed; disagreement = flag for human review. |
| **Cost/latency profiling** | Platform operator | Same task across Llama-4-Maverick ($0.002/req) vs Claude Sonnet ($0.03/req). Is the 15x cost justified by quality? |
| **Reproducibility audits** | Researchers | Run the same model 20 times on the same prompt. Measure output variance. High variance = unreliable for that task type. |

**Priority**: model benchmarking and prompt regression are the MVP cases.
Consensus verification requires quality scoring (deferred — see §11.13).

**Cost awareness**: 200 agents × 1 run = 200 LLM API calls. At ~$0.01
per OpenRouter call (mid-tier model), one ensemble run costs ~$2. At
10 runs/day that's $20/day. The architecture must make cost visible
*before* dispatch (see §11.6 config), not after.

---

### 11.2 Architecture fit — what works as-is

| Component | Section | Anchor (existing = current code, proposed = this doc) | Why it fits at N=200 |
|-----------|---------|------------------------------------------------------|---------------------|
| **Star topology** | §3 | **Proposed**: `EventBus::new()` creates per-agent mpsc channels | 200 bounded channels. Each idle channel costs ~a few hundred bytes. Standard tokio pattern. |
| **Agent workers** | §5 | **Proposed**: `agent_worker()` loops on `inbox.recv()` | 200 `tokio::spawn` tasks, each blocked on its inbox. Idle task overhead: ~8 KB stack. Total: ~1.6 MB. Trivial. |
| **Task execution** | §5 | **Existing**: `execute_agent_task()` in `orchestrator.rs:712` calls `collect_engine_response()` | Same function, different `AgentRuntime.engine` per agent (different model). Works unmodified for dispatch. |
| **Event types** | §2 | **Proposed**: `OrchestratorEvent` enum | `TaskAssignment`, `TaskCompletion`, `TaskError`, `Progress` all apply. `correlation_id` groups the ensemble run. |
| **Error handling** | §8 | **Existing**: `is_retryable_error()` matches `HTTP 429`, `HTTP 502`, `HTTP 503` | Exactly the errors 200 concurrent OpenRouter calls will trigger. Per-agent `catch_unwind` isolation prevents one crash from killing the pool. |
| **Agent runtime storage** | — | **Existing**: `HashMap<String, Arc<AgentRuntime>>` in `orchestrator.rs:105` | `Arc` allows zero-copy sharing across 200 spawn tasks. Each `AgentRuntime` holds an engine + tools + system_prompt — not duplicated data. |
| **Token budget enforcement** | — | **Existing**: `collect_engine_response(..., token_budget)` in `engine_runtime.rs` | Per-agent budgets work. A runaway model hitting its budget gets truncated independently. |
| **Orchestrator event loop** | §4 | **Proposed**: `run_orchestrator()` with `rx.recv()` + LivePlan update | Event processing is O(1) per event. Can handle thousands of events/sec. Not a bottleneck. |

---

### 11.3 Required changes to existing code

Fan-out is *mostly* additive, but six things in the current codebase need
modification. Listed by severity.

#### Change 1: Surface token usage from `execute_agent_task()`

**Problem**: `execute_agent_task()` (orchestrator.rs:712) calls
`collect_engine_response()` which returns `EngineResponse { text,
input_tokens_delta, output_tokens_delta, tool_outcomes }` — but then
discards the token fields:

```rust
// Current (orchestrator.rs:744-757) — tokens dropped
let mut combined = response.text;
// ...
Ok((combined, response.tool_outcomes))
```

Without token data, the ensemble report's cost/token columns are all
zeros — rendering cost benchmarking useless.

**Fix**: return the full `EngineResponse` (or a new wrapper):

```rust
pub(crate) struct AgentTaskResult {
    pub text: String,
    pub tool_outcomes: Vec<(String, String)>,
    pub input_tokens: u32,
    pub output_tokens: u32,
}

async fn execute_agent_task(
    runtime: &AgentRuntime,
    task_description: &str,
    secret_registry: &SecretRegistry,
) -> Result<AgentTaskResult> {
    // ... existing code ...
    let response = collect_engine_response(/* ... */).await?;

    let mut combined = response.text;
    if !response.tool_outcomes.is_empty() {
        combined.push_str("\n\n## Tool Results\n");
        for (name, result) in &response.tool_outcomes {
            combined.push_str(&format!(
                "### {}\n{}\n",
                name,
                channel_runtime::truncate_output(result, 2000),
            ));
        }
    }

    Ok(AgentTaskResult {
        text: combined,
        tool_outcomes: response.tool_outcomes,
        input_tokens: response.input_tokens_delta,
        output_tokens: response.output_tokens_delta,
    })
}
```

**Impact**: Existing callers (DAG orchestrator, CLI direct dispatch) need
to destructure `AgentTaskResult` instead of a tuple. Mechanical change.

#### Change 2: Add `task_timeout` to `OrchestratorConfig`

**Problem**: The current `OrchestratorConfig` (config/schema.rs:249) has
`enabled`, `max_retries`, `planner_engine`, `planner_model` — no timeout.
The existing orchestrator relies on engine-level network timeouts (~60s
from the shared HTTP client). With 200 agents queued behind a semaphore,
there is no wall-clock deadline.

**Fix**: add to config schema:

```rust
pub struct OrchestratorConfig {
    // ... existing fields ...
    /// Per-task wall-clock timeout.
    ///
    /// Semantics differ by execution mode:
    ///
    /// - **DAG mode (§4)**: measured from dispatch_task(), which sets
    ///   LiveTask.started_at before sending the TaskAssignment. This
    ///   includes channel delivery + any worker-side queuing. The
    ///   orchestrator detects timeout via find_timed_out_tasks() (§8)
    ///   and sends TaskCancellation. This is a safety net — the primary
    ///   timeout is the engine's own HTTP timeout (~60s).
    ///
    /// - **Fan-out mode (§11.4)**: measured inside agent_worker() from
    ///   semaphore permit acquisition (actual execution start), enforced
    ///   via tokio::time::timeout(). Does NOT include queue wait time.
    ///   This is the primary timeout mechanism since fan-out agents may
    ///   wait minutes for a permit.
    ///
    /// The difference exists because DAG tasks typically start executing
    /// immediately (no semaphore), so dispatch ≈ execution start. Fan-out
    /// tasks may queue behind a semaphore for minutes, so dispatch time
    /// is meaningless — only execution time matters.
    #[serde(default = "default_task_timeout_secs")]
    pub task_timeout_secs: u64,
}

fn default_task_timeout_secs() -> u64 { 180 }  // 3 minutes
```

```toml
[orchestrator]
task_timeout_secs = 180
```

#### Change 3: `duration` field on `TaskCompletion` event

The canonical `TaskCompletion` in §2 now includes `duration: Duration`.
Both the §5 DAG worker and §11.4 fan-out worker measure execution time
from an `Instant` captured after semaphore acquisition (or immediately
in DAG mode) and include it in the emitted event.

This is the *execution* duration (permit acquired → result ready), not
the queueing duration. The orchestrator can derive queue time from
`TaskAssignment.timestamp` vs `TaskCompletion.timestamp - duration`.

#### Change 4: Scale orchestrator inbox buffer

**Problem**: `EventBus::new()` (§9) uses a single `buffer_size` for all
channels. §3 suggests `mpsc::channel(32)`. With 200 agents completing in
rapid succession, the shared return channel fills at 32 and agents block
on `outbox.send()`. Not a correctness bug (agents have nothing else to
do after completing), but it adds hidden latency that skews duration
measurements — the agent finishes, blocks 200ms on send, and the
`timestamp` in `TaskCompletion` reflects post-block time.

**Fix**: scale the orchestrator inbox independently:

```rust
impl EventBus {
    pub fn new(agent_ids: &[AgentId], per_agent_buffer: usize) -> Self {
        // Orchestrator inbox sized to agent count.
        // At N=200, buffer=128 gives headroom for burst completions.
        let orch_buffer = (agent_ids.len() / 2).max(32).min(512);
        let (orchestrator_tx, orchestrator_rx) = mpsc::channel(orch_buffer);

        // Per-agent inboxes stay small — each receives 1 assignment at a time.
        let mut agent_txs = HashMap::new();
        let mut agent_rxs = HashMap::new();
        for id in agent_ids {
            let (tx, rx) = mpsc::channel(per_agent_buffer);
            agent_txs.insert(id.clone(), tx);
            agent_rxs.insert(id.clone(), rx);
        }
        Self { orchestrator_rx, orchestrator_tx, agent_txs, agent_rxs }
    }
}
```

#### Change 5: `role_to_agent` must support N:1 (pool → role)

**Problem**: `role_to_agent: HashMap<String, AgentId>` (orchestrator.rs:106)
maps one agent per role. In fan-out mode, 200 agents share the same role.

**Fix**: DAG mode keeps the 1:1 map. Fan-out mode uses `AgentPool` (§11.5)
which owns the agent ID list directly. No change to the DAG path — the
pool is a parallel lookup structure.

#### Change 6: Result persistence routes through storage agent (§12)

**Problem**: the current `MemoryService` (memory_service.rs) calls
`EmbeddingPort::embed()` + `MemoryStorePort::store()` for every
`remember_with_metadata()`. At 200 agents this means 200 embedding API
round-trips (~$0.02 + 30-60s latency) and 200 vector store writes — just
to persist structured data that doesn't need semantic search.

**Fix**: ensemble results are persisted by the **storage agent** (§13),
not by direct `MemoryService` calls. The orchestrator sends structured
write events to the storage agent, which appends JSONL lines — zero
embedding cost, sub-millisecond writes. See §12 for full design.

The existing `MemoryService` + vector store remain available for semantic
recall (`recall_filtered()` for "find memories about topic X"). The
storage agent handles structured/exact data ("get result for run abc123,
agent gpt-4o"). Different access patterns, different backends.

---

### 11.4 Isolation model

Fan-out agents must be isolated from each other for valid benchmarking.
The architecture provides three isolation levels, configured per pool:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub enum AgentIsolation {
    /// Agent receives task → executes → returns result. Period.
    /// No Progress events sent. PlanModificationRequest rejected.
    /// Workspace is per-agent. Agent has zero awareness of other agents.
    /// Required for valid benchmarking and reproducibility audits.
    #[default]
    Full,

    /// Same as Full, but agent may send Progress events during execution.
    /// Orchestrator aggregates progress across the pool.
    /// Good for long-running ensemble tasks (e.g., multi-tool research
    /// that takes minutes per agent).
    Supervised,

    /// Agent can request data from other agents via orchestrator (DAG
    /// pattern from §7). Not used in fan-out — exists for completeness.
    OrchestratorMediated,
}
```

**Enforcement in `agent_worker()`**: the isolation level controls what
events the worker is *allowed to send*:

```rust
pub async fn agent_worker(
    agent_id: AgentId,
    runtime: Arc<AgentRuntime>,
    mut inbox: mpsc::Receiver<OrchestratorEvent>,
    outbox: mpsc::Sender<OrchestratorEvent>,
    secret_registry: Arc<SecretRegistry>,
    concurrency_semaphore: Option<Arc<Semaphore>>,
    isolation: AgentIsolation,
    task_timeout: Duration,
) {
    while let Some(event) = inbox.recv().await {
        match event {
            OrchestratorEvent::TaskAssignment {
                task_id, description, correlation_id, ..
            } => {
                // Acquire semaphore permit — this is where queuing happens.
                let _permit = match &concurrency_semaphore {
                    Some(sem) => Some(sem.acquire().await.unwrap()),
                    None => None,
                };

                // Timing starts HERE, after permit acquired.
                let exec_start = Instant::now();

                // `description` is the fully rendered prompt (assembled
                // by dispatch_task() or run_fan_out()). Use as-is.

                // Wrap execution in per-task timeout.
                let result = tokio::time::timeout(
                    task_timeout,
                    execute_agent_task(&runtime, &description, &secret_registry),
                ).await;

                let duration = exec_start.elapsed();

                match result {
                    Ok(Ok(task_result)) => {
                        let artifacts = extract_artifacts(&task_result.tool_outcomes);
                        let _ = outbox.send(OrchestratorEvent::TaskCompletion {
                            task_id,
                            agent_id: agent_id.clone(),
                            output: task_result.text,
                            artifacts,
                            token_usage: TokenUsage {
                                input_tokens: task_result.input_tokens,
                                output_tokens: task_result.output_tokens,
                            },
                            duration,
                            correlation_id,
                            timestamp: Utc::now(),
                        }).await;
                    }
                    Ok(Err(e)) => {
                        let _ = outbox.send(OrchestratorEvent::TaskError {
                            task_id,
                            agent_id: agent_id.clone(),
                            error: e.to_string(),
                            retryable: is_retryable_error(&e),
                            correlation_id,
                            timestamp: Utc::now(),
                        }).await;
                    }
                    Err(_elapsed) => {
                        let _ = outbox.send(OrchestratorEvent::TaskError {
                            task_id,
                            agent_id: agent_id.clone(),
                            error: format!("Timed out after {}s", task_timeout.as_secs()),
                            retryable: false,
                            correlation_id,
                            timestamp: Utc::now(),
                        }).await;
                    }
                }
                // _permit dropped here — next queued agent starts.
            }

            OrchestratorEvent::Shutdown { .. } => break,

            other => {
                // Isolation enforcement: in Full mode, log unexpected events.
                if matches!(isolation, AgentIsolation::Full) {
                    tracing::debug!(
                        agent = %agent_id,
                        event = ?std::mem::discriminant(&other),
                        "Dropped event in Full isolation mode"
                    );
                }
            }
        }
    }
}
```

Key differences from §5's `agent_worker()`:

| Concern | §5 (DAG) | §11 (Fan-out) |
|---------|----------|---------------|
| Semaphore | None (DAG parallelism from dependency graph) | Required (controls concurrent LLM calls) |
| Duration | Not measured | Measured from permit acquisition to result |
| Timeout | Relies on engine HTTP timeout (~60s) | Explicit `tokio::time::timeout` per task |
| Token usage | Hardcoded `TokenUsage { 0, 0 }` | Real values from `AgentTaskResult` |
| Isolation | All event types accepted | `Full` mode drops non-assignment events |
| Progress | Always forwarded | Only in `Supervised` mode |

---

### 11.5 New types

#### `ExecutionMode` — orchestrator dispatch selector

```rust
pub enum ExecutionMode {
    /// Standard DAG execution (§1–11).
    DagPlan(LivePlan),
    /// Fan-out: identical task dispatched to N agents, no dependencies.
    FanOut(FanOutPlan),
}
```

#### `FanOutPlan` — ensemble run state

```rust
#[derive(Debug)]
pub struct FanOutPlan {
    pub run_id: String,
    pub task_description: String,
    pub pool: AgentPool,
    /// Per-agent results, populated as completions arrive.
    pub results: HashMap<AgentId, FanOutResult>,
    pub max_concurrency: usize,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub enum FanOutResult {
    Completed {
        output: String,
        duration: Duration,
        token_usage: TokenUsage,
    },
    Failed {
        error: String,
        retryable: bool,
        duration: Duration,
    },
    TimedOut {
        after: Duration,
    },
}

impl FanOutPlan {
    pub fn is_complete(&self) -> bool {
        self.pool.agent_ids.len() == self.results.len()
    }

    pub fn completion_count(&self) -> usize {
        self.results.values()
            .filter(|r| matches!(r, FanOutResult::Completed { .. }))
            .count()
    }

    pub fn failure_count(&self) -> usize {
        self.results.values()
            .filter(|r| !matches!(r, FanOutResult::Completed { .. }))
            .count()
    }
}
```

#### `AgentPool` — thin grouping over existing runtimes

```rust
#[derive(Debug, Clone)]
pub struct AgentPool {
    /// The role name shared by all agents in the pool.
    pub role: String,
    /// Agent IDs — each maps to an entry in the existing
    /// `HashMap<String, Arc<AgentRuntime>>`.
    pub agent_ids: Vec<AgentId>,
}
```

Does **not** own `AgentRuntime` instances. Those live in the same
`HashMap<String, Arc<AgentRuntime>>` (orchestrator.rs:105) used by DAG
mode. The pool is just a grouping index.

#### `EnsembleReport` and `AgentResult`

```rust
#[derive(Debug, Clone, Serialize)]
pub struct AgentResult {
    pub agent_id: AgentId,
    pub model: String,
    pub status: String,         // "completed" | "failed" | "timed_out"
    pub output: Option<String>,
    pub error: Option<String>,
    pub duration_ms: u64,
    pub input_tokens: u32,
    pub output_tokens: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct EnsembleReport {
    pub run_id: String,
    pub task_description: String,
    pub total_agents: usize,
    pub completed: usize,
    pub failed: usize,
    pub timed_out: usize,
    pub fastest_agent: Option<AgentId>,
    pub slowest_agent: Option<AgentId>,
    pub mean_duration_ms: u64,
    pub median_duration_ms: u64,
    pub total_input_tokens: u32,
    pub total_output_tokens: u32,
    pub agent_results: Vec<AgentResult>,
}
```

#### `build_ensemble_report()`

```rust
fn build_ensemble_report(
    plan: &FanOutPlan,
    agent_models: &HashMap<AgentId, String>,
) -> EnsembleReport {
    let mut agent_results: Vec<AgentResult> = plan.pool.agent_ids.iter()
        .filter_map(|id| {
            let result = plan.results.get(id)?;
            let (status, output, error, dur_ms, in_tok, out_tok) = match result {
                FanOutResult::Completed { output, duration, token_usage } => (
                    "completed", Some(output.clone()), None,
                    duration.as_millis() as u64,
                    token_usage.input_tokens, token_usage.output_tokens,
                ),
                FanOutResult::Failed { error, duration, .. } => (
                    "failed", None, Some(error.clone()),
                    duration.as_millis() as u64, 0, 0,
                ),
                FanOutResult::TimedOut { after } => (
                    "timed_out", None, None,
                    after.as_millis() as u64, 0, 0,
                ),
            };
            Some(AgentResult {
                agent_id: id.clone(),
                model: agent_models.get(id).cloned().unwrap_or_default(),
                status: status.into(), output, error,
                duration_ms: dur_ms, input_tokens: in_tok, output_tokens: out_tok,
            })
        })
        .collect();

    agent_results.sort_by_key(|r| r.duration_ms);

    let durations: Vec<u64> = agent_results.iter().map(|r| r.duration_ms).collect();
    let mean = durations.iter().sum::<u64>().checked_div(durations.len() as u64).unwrap_or(0);
    let median = durations.get(durations.len() / 2).copied().unwrap_or(0);
    let completed = agent_results.iter().filter(|r| r.status == "completed").count();
    let failed = agent_results.iter().filter(|r| r.status == "failed").count();
    let timed_out = agent_results.iter().filter(|r| r.status == "timed_out").count();

    EnsembleReport {
        run_id: plan.run_id.clone(),
        task_description: plan.task_description.clone(),
        total_agents: plan.pool.agent_ids.len(),
        completed, failed, timed_out,
        fastest_agent: agent_results.first().map(|r| r.agent_id.clone()),
        slowest_agent: agent_results.last().map(|r| r.agent_id.clone()),
        mean_duration_ms: mean,
        median_duration_ms: median,
        total_input_tokens: agent_results.iter().map(|r| r.input_tokens).sum(),
        total_output_tokens: agent_results.iter().map(|r| r.output_tokens).sum(),
        agent_results,
    }
}
```

---

### 11.6 Config: agent pools

Declaring 200 agents individually in TOML = ~5000 lines. `[[agent_pools]]`
expands a template at load time:

```toml
[[agent_pools]]
role = "benchmark-runner"
system_prompt = "You are a research assistant. Answer the question concisely."
skill_packages = ["beach-science"]
isolation = "full"                # "full" | "supervised" (default: "full")
models = [
    "anthropic/claude-sonnet-4",
    "openai/gpt-4o",
    "google/gemini-2.0-flash",
    "meta-llama/llama-4-maverick",
    "deepseek/deepseek-r1",
]
copies_per_model = 1              # 5 models × 1 copy = 5 agents
max_concurrency = 10
workspace_isolation = true

# Per-agent limits (inherited from LimitsConfig schema).
[agent_pools.limits]
max_tokens_per_flow = 8000
```

For 200 agents: 40 models × 5 copies, or 10 models × 20 copies. The
`copies_per_model` field controls reproducibility tests (same model, N
runs, measure variance).

**Expansion** generates `AgentConfig` entries that plug into the existing
agent registration path (orchestrator.rs:105-200):

```rust
fn expand_agent_pools(pools: &[AgentPoolConfig]) -> Vec<AgentConfig> {
    let mut agents = Vec::new();
    for pool in pools {
        for model in &pool.models {
            let model_slug = model.replace('/', "-");
            for copy in 0..pool.copies_per_model.unwrap_or(1) {
                let agent_id = format!("{}-{}-{}", pool.role, model_slug, copy);
                agents.push(AgentConfig {
                    default: false,
                    engine: engine_from_model(model),
                    model: model.clone(),
                    workspace: pool.workspace_path(&agent_id),
                    identity: pool.identity.clone(),
                    flow: FlowConfig::default(),
                    limits: pool.limits.clone().unwrap_or_default(),
                    role: Some(pool.role.clone()),
                    capabilities: pool.capabilities.clone(),
                    skill_packages: pool.skill_packages.clone(),
                    requires: vec![],
                });
            }
        }
    }
    agents
}
```

**Pre-flight cost estimate**: before dispatching, the orchestrator
can log: "This ensemble run will make {N} LLM calls across {M} models.
Estimated cost: ${estimated}". The estimate uses the per-model pricing
from OpenRouter's API (already available via `available_models()` on the
Engine trait) × estimated input tokens (from the prompt length).

---

### 11.7 Fan-out event loop

`run_fan_out()` is structurally simpler than `run_orchestrator()` (§4) —
no dependency graph, no re-planning, no data routing. Dispatch N
assignments and collect N results.

Key difference from the original design: **no global timeout**. Each
agent enforces its own per-task timeout inside `agent_worker()` (§11.4).
The orchestrator simply waits for all N results, with a safety deadline
to catch agents that somehow fail to report.

```rust
pub async fn run_fan_out(
    mut plan: FanOutPlan,
    mut rx: mpsc::Receiver<OrchestratorEvent>,
    agent_senders: &HashMap<AgentId, mpsc::Sender<OrchestratorEvent>>,
    progress_tx: Option<mpsc::Sender<FanOutProgress>>,
    config: &OrchestratorConfig,
) -> Result<FanOutPlan> {
    let total = plan.pool.agent_ids.len();

    // Phase 1: render prompt and dispatch all assignments.
    // Same contract as DAG mode: description carries the fully rendered
    // prompt via format_task_prompt(). Workers use it as-is.
    // Fan-out has no upstream context (all agents get the same task),
    // so context is empty, but the goal and rules are still injected.
    let rendered_prompt = format_task_prompt(
        &plan.task_description, // In fan-out, the task_description IS the goal.
        &plan.task_description,
        &HashMap::new(),
    );

    for agent_id in &plan.pool.agent_ids {
        let task_id = format!("{}-{}", plan.run_id, agent_id);
        agent_senders[agent_id]
            .send(OrchestratorEvent::TaskAssignment {
                task_id,
                agent_id: agent_id.clone(),
                description: rendered_prompt.clone(),
                context: HashMap::new(),
                correlation_id: plan.run_id.clone(),
                timestamp: Utc::now(),
            })
            .await?;
    }

    // Phase 2: collect results.
    // Safety deadline: if all agents have individual 180s timeouts,
    // the absolute worst case is (N / concurrency) × timeout.
    // Add 30s buffer for scheduling overhead.
    let max_rounds = (total as u64)
        .div_ceil(plan.max_concurrency as u64);
    let safety_deadline = Instant::now()
        + Duration::from_secs(max_rounds * config.task_timeout_secs + 30);

    let mut last_progress_milestone = 0usize;

    loop {
        let remaining = safety_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            // Mark any silent agents as timed out.
            for agent_id in &plan.pool.agent_ids {
                plan.results.entry(agent_id.clone()).or_insert(
                    FanOutResult::TimedOut {
                        after: Duration::from_secs(config.task_timeout_secs),
                    },
                );
            }
            tracing::warn!(
                run_id = %plan.run_id,
                missing = total - plan.results.len(),
                "Safety deadline reached, marking remaining agents as timed out"
            );
            break;
        }

        tokio::select! {
            Some(event) = rx.recv() => {
                match event {
                    OrchestratorEvent::TaskCompletion {
                        agent_id, output, token_usage, duration, ..
                    } => {
                        plan.results.insert(agent_id, FanOutResult::Completed {
                            output, duration, token_usage,
                        });
                    }
                    OrchestratorEvent::TaskError {
                        agent_id, error, retryable, ..
                    } => {
                        // In fan-out mode, we don't retry — just record.
                        // Retry would re-run the same model, which is what
                        // copies_per_model is for (controlled variance, not
                        // error recovery).
                        plan.results.insert(agent_id, FanOutResult::Failed {
                            error, retryable, duration: Duration::ZERO,
                        });
                    }
                    other => {
                        tracing::trace!(
                            event = ?std::mem::discriminant(&other),
                            "Ignored non-result event in fan-out loop"
                        );
                    }
                }

                // Progress reporting: emit at every 10% milestone.
                let done = plan.results.len();
                let milestone = done * 10 / total.max(1);
                if milestone > last_progress_milestone {
                    last_progress_milestone = milestone;
                    if let Some(tx) = &progress_tx {
                        let _ = tx.send(FanOutProgress {
                            completed: plan.completion_count(),
                            failed: plan.failure_count(),
                            total,
                        }).await;
                    }
                }
            }
            _ = tokio::time::sleep(remaining) => {
                // Safety deadline (handled above on next iteration).
            }
        }

        if plan.is_complete() {
            break;
        }
    }

    Ok(plan)
}

#[derive(Debug, Clone)]
pub struct FanOutProgress {
    pub completed: usize,
    pub failed: usize,
    pub total: usize,
}
```

**Why no retry in fan-out**: DAG retry (§8) makes sense because a failed
task blocks dependents. In fan-out, nothing depends on any single agent.
If a model returns HTTP 429, the result is "this model failed under load"
— that *is* useful benchmark data. For variance testing, `copies_per_model`
provides controlled repetition.

---

### 11.8 Concurrency and back-pressure

#### Semaphore gating

200 simultaneous LLM API calls would exhaust OpenRouter rate limits and
consume ~200× conversation-state memory. A `tokio::sync::Semaphore`
(configured via `max_concurrency` in the pool config) limits how many
agents actually call their engine at once:

```
200 agents spawned → 10 acquire permits → execute → complete → release
                     10 more acquire → ...
                     (20 rounds to clear 200 agents)
```

At `max_concurrency = 10` with 30s average task time:
- Total wall-clock: ~600s (10 minutes) for 200 agents
- Peak memory: 10 active conversations × ~2 MB each = ~20 MB
- Peak API load: 10 concurrent requests (well within OpenRouter limits)

#### Circuit breaker for rate limits

Without pool-level awareness, the semaphore creates a thundering herd:
10 agents hit 429, release permits, 10 more start, also hit 429. The
circuit breaker detects this pattern and pauses dispatch.

```rust
struct CircuitBreaker {
    /// Rolling window of recent results.
    recent: VecDeque<(Instant, bool)>,  // (time, was_rate_limited)
    /// When set, no permits are issued until this instant.
    backoff_until: Option<Instant>,
    window: Duration,
    threshold: f64,
}

impl CircuitBreaker {
    fn new() -> Self {
        Self {
            recent: VecDeque::new(),
            backoff_until: None,
            window: Duration::from_secs(30),
            threshold: 0.5,  // >50% rate-limited → trip
        }
    }

    fn record(&mut self, rate_limited: bool) {
        let now = Instant::now();
        self.recent.push_back((now, rate_limited));

        // Prune old entries.
        while self.recent.front().is_some_and(|(t, _)| now - *t > self.window) {
            self.recent.pop_front();
        }

        // Check threshold.
        if self.recent.len() >= 5 {
            let limited = self.recent.iter().filter(|(_, rl)| *rl).count();
            let ratio = limited as f64 / self.recent.len() as f64;
            if ratio > self.threshold {
                // Exponential backoff: 5s, 10s, 20s, capped at 60s.
                let backoff = Duration::from_secs(5)
                    * 2u32.pow(self.consecutive_trips().min(4));
                self.backoff_until = Some(now + backoff);
                tracing::warn!(
                    backoff_secs = backoff.as_secs(),
                    rate = format!("{:.0}%", ratio * 100.0),
                    "Circuit breaker tripped — pausing dispatch"
                );
            }
        }
    }

    fn should_pause(&self) -> bool {
        self.backoff_until.is_some_and(|t| Instant::now() < t)
    }
}
```

Integration: the circuit breaker wraps the semaphore in `agent_worker()`.
Before acquiring a permit, the worker checks `circuit_breaker.should_pause()`
and sleeps if tripped. After execution, it calls `circuit_breaker.record()`
with whether the error was a rate limit.

---

### 11.9 Workspace lifecycle

When `workspace_isolation = true`, each agent gets a unique workspace
directory derived from its ID:

```rust
fn workspace_path(base: &Path, agent_id: &str, isolated: bool) -> PathBuf {
    if isolated {
        base.join(agent_id)  // ~/workspace/benchmark-runner-gpt-4o-0/
    } else {
        base.to_path_buf()
    }
}
```

The directory is created on first `write_file` call by the existing
`WorkspaceToolExecutor`. No pre-creation needed.

**Cleanup**: 200 agents × 10 runs = 2000 directories. The orchestrator
cleans up after a completed run:

```rust
async fn cleanup_pool_workspaces(
    base: &Path,
    pool: &AgentPool,
    keep_on_failure: bool,
    results: &HashMap<AgentId, FanOutResult>,
) -> Result<()> {
    for agent_id in &pool.agent_ids {
        let ws = base.join(agent_id);
        if !ws.exists() {
            continue;
        }
        // Keep workspace for failed agents (debugging).
        if keep_on_failure {
            if let Some(FanOutResult::Failed { .. }) = results.get(agent_id) {
                tracing::debug!(agent = %agent_id, "Keeping workspace for failed agent");
                continue;
            }
        }
        tokio::fs::remove_dir_all(&ws).await?;
    }
    Ok(())
}
```

Config knob:

```toml
[[agent_pools]]
# ...
workspace_cleanup = "on_success"  # "on_success" | "always" | "never"
```

---

### 11.10 Result persistence

Ensemble results are persisted by the **storage agent** (§13), not by
direct `MemoryService` calls. After `run_fan_out()` completes:

1. The orchestrator builds the `EnsembleReport` (§11.5).
2. For each `AgentResult`, the orchestrator sends a structured write to
   the storage agent (one event per result).
3. The storage agent appends JSONL lines — one per agent result, one for
   the aggregate report.

```jsonl
{"ts":1710756000,"key":"run/abc123/agent/gpt-4o-0","kind":"agent_result","tags":{"run_id":"abc123","model":"openai/gpt-4o","status":"completed","duration_ms":"4100","input_tokens":"1200","output_tokens":"890"},"data":"The molecule SMILES is CC(=O)Oc1ccccc1C(=O)O..."}
{"ts":1710756000,"key":"run/abc123/agent/gemini-2.0-flash-0","kind":"agent_result","tags":{"run_id":"abc123","model":"google/gemini-2.0-flash","status":"completed","duration_ms":"3200","input_tokens":"1100","output_tokens":"760"},"data":"The SMILES representation is..."}
{"ts":1710756001,"key":"run/abc123/report","kind":"ensemble_report","tags":{"run_id":"abc123","total":"200","completed":"178","failed":"15","timed_out":"7"},"data":"{\"fastest_agent\":\"gemini-2.0-flash-0\",...}"}
```

**Why not `MemoryService`?** The current `MemoryService::remember_with_metadata()`
(memory_service.rs:44) calls `EmbeddingPort::embed()` for every write.
At 200 agents that's 200 embedding API calls (~$0.02 + 60s). Ensemble
data is structured — it doesn't need semantic search. JSONL gives
sub-millisecond writes with exact key/tag retrieval.

**Retrieval**: users or downstream agents query via the storage agent:

```
/recall run_id:abc123 kind:ensemble_report
```

The storage agent scans JSONL by tag match and returns matching entries.
For cross-run comparison ("compare abc123 vs def456"), query both run_ids
and diff by model.

See §12 for the full storage agent design.

---

### 11.11 Channel UX (Telegram)

Telegram messages are capped at 4096 characters (`TELEGRAM_MAX_LEN = 4000`
with safety margin, per telegram_runtime.rs). Fan-out with 200 agents
requires aggregated output.

**During execution**: progress message on each 10% milestone, updating
the same message via `edit_message` (not sending new ones):

```
Ensemble run abc123: 60/200 agents done (47 ok, 13 failed)
```

**On completion**: single summary message:

```
Ensemble run abc123 complete

200 agents | 178 completed | 15 failed | 7 timed out
Mean: 14.2s | Median: 11.8s | Tokens: 1.2M in / 340K out

Top 5 fastest:
1. gemini-2.0-flash — 3.2s (1.2K tok)
2. gpt-4o-mini — 4.1s (1.8K tok)
3. claude-haiku — 4.8s (1.5K tok)
4. llama-4-maverick — 6.1s (2.1K tok)
5. deepseek-r1 — 7.9s (3.4K tok)

5 failures:
- mistral-large: HTTP 429 (rate limit)
- command-r-plus: timed out (180s)
- qwen-2.5-72b: context window exceeded
...and 2 more

Full report: /recall run_id:abc123
```

The formatter truncates to fit 4000 chars. If the top-5 list alone
exceeds the limit, it falls back to a stats-only summary with a pointer
to the stored report.

---

### 11.12 Scalability budget (100–200 agents)

Concrete resource analysis for N=200 with `max_concurrency=10`:

| Resource | Formula | Value at N=200 |
|----------|---------|---------------|
| **tokio tasks** | N spawned, 10 active | 200 tasks × ~8 KB = 1.6 MB idle |
| **mpsc channels** | N agent inboxes + 1 orchestrator inbox | 201 channels × ~256 bytes = ~50 KB |
| **Orchestrator inbox buffer** | `max(32, N/2)` | 100 slots × ~1 KB/event = ~100 KB |
| **Active conversation memory** | `max_concurrency` × ~2 MB | 10 × 2 MB = 20 MB |
| **Peak API requests** | `max_concurrency` concurrent | 10 (within OpenRouter's default limits) |
| **Wall-clock time** | `ceil(N / concurrency) × avg_task_time` | ceil(200/10) × 30s = 600s (10 min) |
| **LLM API cost** | N × avg_cost_per_call | 200 × $0.01 = $2.00 per run |
| **Workspace disk** | N × avg_workspace_size (if isolated) | 200 × ~1 MB = 200 MB (cleaned up after) |
| **Storage writes** | N+1 JSONL lines (via storage agent §12) | 201 append operations, <1ms each, zero embedding cost |

**Bottleneck**: wall-clock time. At 30s/task and concurrency=10, a 200-
agent run takes 10 minutes. Increasing concurrency to 20 halves this to
5 minutes but doubles peak API load and memory. The `max_concurrency`
config is the primary tuning knob.

**Scaling beyond 200**: the architecture has no hard limits at 200. At
N=1000, the storage agent's JSONL append scales linearly (1000 lines,
~1 MB file). The bottleneck is wall-clock time from LLM API calls, not
storage. At N=10,000+ the JSONL scan for retrieval becomes slow — at
that point, add an in-memory index (HashMap<key, byte_offset>) or
migrate to SQLite.

---

### 11.13 What's deferred

| Item | Why deferred | Path to add later |
|------|-------------|-------------------|
| **Output quality scoring** | Requires domain-specific evaluation criteria or an LLM judge. No universal "correctness" metric exists. | Add a `JudgeAgent` that receives all `AgentResult.output` texts and produces scores. This is just another DAG task that depends on the fan-out completion — no architectural change. |
| **Consensus voting** | Needs quality scoring first (you can't vote on which answer is "right" without a scoring function). | Once quality scores exist, majority vote or weighted vote is a simple aggregation over `EnsembleReport.agent_results`. |
| **Live result streaming** | During a 10-minute run, users might want to see partial results as they arrive (not just progress counts). | The `progress_tx` channel could carry `FanOutProgress` with optional `latest_result: Option<AgentResult>`. Telegram adapter renders as an expanding thread. |
| **Cross-run comparison** | "Compare results of run abc123 vs def456" — different prompts or different model sets. | Query storage agent with two `run_id` values, join JSONL entries on `model`, diff outputs. Pure application-layer logic, no new types. |
| **Adaptive concurrency** | Auto-tune `max_concurrency` based on observed rate limits instead of fixed config value. | The circuit breaker (§11.8) already tracks rate-limit ratios. Extend it to dynamically adjust the semaphore permit count (shrink on rate limits, grow on sustained success). |

---

## 12. Storage Agent

### 12.1 Problem: context bloat and data coupling

The current orchestrator (orchestrator.rs:760-792) passes data between
agents via `build_step_context()`, which concatenates all prior
`StepResult.output` strings (truncated to `MAX_STEP_CONTEXT_CHARS = 8000`
each) into the downstream agent's prompt. At 6 completed steps that's
~48 KB of irrelevant text stuffed into context — this is Problem #2 from
§1.

The event-bus architecture (§6) improves this with two-tier routing:
envelope extraction + LLM summarization. But at 200 agents, even
summaries add up. And more fundamentally, there's no cross-session
persistence — when the orchestrator exits, all task outputs are gone.

The existing `MemoryService` (memory_service.rs) provides persistence,
but it's designed for **semantic recall** (embed query → vector search →
ranked results). Storing 200 structured task results through it means
200 embedding API calls ($0.02 + 60s) for data that will be retrieved
by exact key, not by meaning.

### 12.2 Solution: storage agent as data broker

A dedicated **storage agent** handles all cross-agent data persistence.
It is the only agent (besides the orchestrator) that exists as a system
role. It is **optional** — without it the system works as before (in-
memory context passing). With it, the orchestrator gains:

1. **Cross-session persistence** — data survives between runs
2. **Context bloat prevention** — agents get summaries, not full outputs
3. **Cross-agent data sharing** — any agent can store/retrieve via the
   orchestrator without seeing other agents' full responses
4. **Zero embedding cost** — JSONL append is free, retrieval by key/tag

**Design principle**: the orchestrator is the only mandatory agent.
The storage agent is pluggable — enable it by adding `[storage_agent]`
to config. The orchestrator checks for its presence and routes
accordingly. Without it, data routing falls back to §6 (in-memory).

### 12.3 Architecture

```
                    ┌─────────────────┐
                    │   Orchestrator   │
                    │                  │
                    │  On TaskCompletion:
                    │  1. Extract summary (200 chars)
                    │  2. Update LivePlan/FanOutPlan
                    │  3. Route full output → storage agent
                    │                  │
                    │  On downstream dispatch:
                    │  1. Include summary in context
                    │  2. If agent needs specific data:
                    │     query storage agent → inject result
                    └──┬───────────┬───┘
                       │           │
               Tx(storage)    Tx(agents...)
                       │           │
                       ▼           ▼
               ┌──────────┐  ┌──────────┐
               │ Storage   │  │ Agent A  │
               │ Agent     │  │ Agent B  │
               │ (JSONL)   │  │ Agent N  │
               └──────────┘  └──────────┘
```

**Communication flow**:
- All agent communication goes through the orchestrator (star topology, §3)
- The orchestrator decides what to store and what to forward
- Agents never talk to the storage agent directly
- Agents never see other agents' full outputs

This makes context bloat **structurally impossible**:
1. Agents send results to orchestrator — full output, one direction
2. Orchestrator keeps only a summary in its state (200 chars per task)
3. Full output goes to storage agent (JSONL append, not in orchestrator memory)
4. Downstream agents get: summary + selectively retrieved data
5. At 200 agents, orchestrator state = 200 × 200 chars = 40 KB (bounded)

### 12.4 Storage agent implementation

The storage agent is a **non-LLM system worker**. It does not use an
`Engine` for reasoning — it processes structured operations directly
from the `context` field of `TaskAssignment`. This means zero LLM cost
for every store/retrieve operation.

```rust
/// The storage agent is a system worker, not an LLM agent.
/// It receives structured TaskAssignments and executes JSONL operations.
pub async fn storage_agent_worker(
    agent_id: AgentId,
    mut inbox: mpsc::Receiver<OrchestratorEvent>,
    outbox: mpsc::Sender<OrchestratorEvent>,
    store_path: PathBuf,
) {
    // Open JSONL file in append mode.
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&store_path)
        .expect("Failed to open storage JSONL");

    while let Some(event) = inbox.recv().await {
        match event {
            OrchestratorEvent::TaskAssignment {
                task_id, context, correlation_id, ..
            } => {
                let result = match context.get("operation").and_then(|v| v.as_str()) {
                    Some("write") => handle_write(&mut file, &context),
                    Some("read") => handle_read(&store_path, &context),
                    Some("query") => handle_query(&store_path, &context),
                    Some("list") => handle_list(&store_path, &context),
                    Some(op) => Err(anyhow::anyhow!("Unknown operation: {}", op)),
                    None => Err(anyhow::anyhow!("Missing 'operation' in context")),
                };

                match result {
                    Ok(output) => {
                        let _ = outbox.send(OrchestratorEvent::TaskCompletion {
                            task_id,
                            agent_id: agent_id.clone(),
                            output,
                            artifacts: HashMap::new(),
                            token_usage: TokenUsage { input_tokens: 0, output_tokens: 0 },
                            duration: Duration::ZERO,
                            correlation_id,
                            timestamp: Utc::now(),
                        }).await;
                    }
                    Err(e) => {
                        let _ = outbox.send(OrchestratorEvent::TaskError {
                            task_id,
                            agent_id: agent_id.clone(),
                            error: e.to_string(),
                            retryable: false,
                            correlation_id,
                            timestamp: Utc::now(),
                        }).await;
                    }
                }
            }
            OrchestratorEvent::Shutdown { .. } => break,
            _ => {}
        }
    }
}
```

### 12.5 JSONL format

Each line is a self-contained JSON object. The format follows the same
pattern as the existing `flow_store.rs` transcript JSONL
(`~/.tengu/state/flows/{hash}/transcript.jsonl`), which is already
proven in the codebase.

```jsonl
{"ts":1710756000,"key":"task/research-1/output","kind":"task_output","tags":{"task_id":"research-1","role":"researcher","agent_id":"claude-sonnet-0","status":"completed"},"data":"Found 3 relevant papers on aspirin synthesis..."}
{"ts":1710756001,"key":"task/mint-ipnft/artifact/tx_hash","kind":"artifact","tags":{"task_id":"mint-ipnft","type":"tx_hash"},"data":"0xabc123def456..."}
{"ts":1710756100,"key":"run/abc123/agent/gpt-4o-0","kind":"agent_result","tags":{"run_id":"abc123","model":"openai/gpt-4o","status":"completed","duration_ms":"4100"},"data":"The molecule SMILES is CC(=O)Oc1ccccc1C(=O)O"}
{"ts":1710756101,"key":"run/abc123/report","kind":"ensemble_report","tags":{"run_id":"abc123","total":"200","completed":"178"},"data":"{\"fastest_agent\":\"gemini-2.0-flash-0\",...}"}
```

**Fields**:

| Field | Purpose |
|-------|---------|
| `ts` | Unix epoch seconds — ordering, TTL pruning |
| `key` | Hierarchical path — exact lookup (`task/{id}/output`, `run/{id}/report`) |
| `kind` | Category — `task_output`, `artifact`, `agent_result`, `ensemble_report`, `user_data` |
| `tags` | Flat key-value pairs — flexible filtering (`run_id=abc123 AND status=completed`) |
| `data` | The actual content — string or JSON string. This is what gets returned to the requesting agent. |

**Operations**:

```rust
/// Append a single entry to the JSONL file.
fn handle_write(file: &mut File, context: &HashMap<String, Value>) -> Result<String> {
    let entry = StorageEntry {
        ts: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
        key: context["key"].as_str().unwrap().to_string(),
        kind: context["kind"].as_str().unwrap().to_string(),
        tags: serde_json::from_value(context["tags"].clone())?,
        data: context["data"].as_str().unwrap().to_string(),
    };
    let line = serde_json::to_string(&entry)?;
    writeln!(file, "{}", line)?;
    file.flush()?;
    Ok(format!("stored:{}", entry.key))
}

/// Read a single entry by exact key (last match wins for overwrites).
fn handle_read(path: &Path, context: &HashMap<String, Value>) -> Result<String> {
    let target_key = context["key"].as_str().unwrap();
    let file = BufReader::new(File::open(path)?);
    let mut last_match: Option<String> = None;
    for line in file.lines() {
        let line = line?;
        if let Ok(entry) = serde_json::from_str::<StorageEntry>(&line) {
            if entry.key == target_key {
                last_match = Some(entry.data);
            }
        }
    }
    last_match.ok_or_else(|| anyhow::anyhow!("Key not found: {}", target_key))
}

/// Query entries matching all specified tags.
fn handle_query(path: &Path, context: &HashMap<String, Value>) -> Result<String> {
    let required_tags: HashMap<String, String> =
        serde_json::from_value(context["tags"].clone())?;
    let limit = context.get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(100) as usize;

    let file = BufReader::new(File::open(path)?);
    let mut matches: Vec<StorageEntry> = Vec::new();
    for line in file.lines() {
        let line = line?;
        if let Ok(entry) = serde_json::from_str::<StorageEntry>(&line) {
            let all_match = required_tags.iter().all(|(k, v)| {
                entry.tags.get(k).map(|tv| tv == v).unwrap_or(false)
            });
            if all_match {
                matches.push(entry);
                if matches.len() >= limit { break; }
            }
        }
    }
    Ok(serde_json::to_string(&matches)?)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StorageEntry {
    ts: u64,
    key: String,
    kind: String,
    tags: HashMap<String, String>,
    data: String,
}
```

### 12.6 Data flow: store and retrieve

#### Storing task output (automatic on TaskCompletion)

When any agent completes a task and the storage agent is configured, the
orchestrator automatically persists the full output:

```rust
// Inside run_orchestrator() or run_fan_out(), after receiving TaskCompletion:

fn handle_completion_with_storage(
    plan: &mut LivePlan,
    task_id: &TaskId,
    agent_id: &AgentId,
    output: &str,
    storage_tx: Option<&mpsc::Sender<OrchestratorEvent>>,
) {
    let task = plan.tasks.get_mut(task_id).unwrap();

    // 1. Keep only a summary in LivePlan state (bounded context).
    task.output = Some(truncate(output, 200));
    task.status = TaskStatus::Completed;

    // 2. Persist full output via storage agent.
    if let Some(tx) = storage_tx {
        let _ = tx.send(OrchestratorEvent::TaskAssignment {
            task_id: format!("storage-write-{}", task_id),
            agent_id: "storage".into(),
            description: "write".into(),
            context: HashMap::from([
                ("operation".into(), json!("write")),
                ("key".into(), json!(format!("task/{}/output", task_id))),
                ("kind".into(), json!("task_output")),
                ("tags".into(), json!({
                    "task_id": task_id,
                    "agent_id": agent_id,
                    "role": &task.role,
                    "status": "completed",
                })),
                ("data".into(), json!(output)),
            ]),
            correlation_id: plan.revision.to_string(),
            timestamp: Utc::now(),
        });
    }
}
```

**Key insight**: `task.output` in `LivePlan` holds only a 200-char
summary. The full output lives in the JSONL file. This is how context
stays bounded at 200 agents — the orchestrator never accumulates full
outputs in memory.

#### Retrieving data for a downstream agent

When the orchestrator dispatches a downstream task that depends on
upstream data, it queries the storage agent for specific values instead
of dumping full outputs:

```rust
// Replace the current build_step_context() with storage-aware routing.

async fn build_task_context_with_storage(
    plan: &LivePlan,
    task: &LiveTask,
    storage_tx: &mpsc::Sender<OrchestratorEvent>,
    storage_rx: &mut mpsc::Receiver<OrchestratorEvent>,
) -> HashMap<String, Value> {
    let mut context = HashMap::new();

    for dep_id in &task.depends_on {
        if let Some(dep_task) = plan.tasks.get(dep_id) {
            // Include the summary (already in LivePlan, ~200 chars).
            context.insert(
                format!("{}_summary", dep_id),
                json!(dep_task.output.as_deref().unwrap_or("(no output)")),
            );

            // If the dep produced artifacts, include those (small, structured).
            if !dep_task.artifacts.is_empty() {
                context.insert(
                    format!("{}_artifacts", dep_id),
                    serde_json::to_value(&dep_task.artifacts).unwrap(),
                );
            }

            // For full output, query storage agent on demand.
            // This is optional — only if the downstream skill
            // declares it needs the full upstream output.
        }
    }

    context
}
```

**Context size comparison at N=200 (fan-out mode)**:

| Approach | Orchestrator memory | Downstream context |
|----------|-------------------|--------------------|
| Current `build_step_context()` | 200 × 8 KB = 1.6 MB | 48 KB+ per agent |
| §6 Tier 1 + Tier 2 (envelope + LLM summarize) | 200 × artifacts only | Artifacts + LLM summary |
| Storage agent (this design) | 200 × 200 chars = 40 KB | Summary + on-demand retrieval |

### 12.7 How agents request permanent storage

An agent's skill can declare that certain data should be persisted. The
agent doesn't call the storage agent directly — it expresses intent in
its output or tool results, and the orchestrator routes accordingly.

Three mechanisms, from simplest to most explicit:

**1. Automatic (default)**: the orchestrator stores every
`TaskCompletion.output` via the storage agent. No agent awareness needed.
This is the fan-out default — all 200 results are stored automatically.

**2. Skill-declared**: a skill's SKILL.md can include a `persist` field
in its frontmatter:

```yaml
---
name: molecule-analysis
persist:
  - field: smiles_data
    kind: artifact
  - field: analysis_summary
    kind: task_output
---
```

The orchestrator reads the skill's persist declarations and extracts
named fields from tool results for structured storage.

**3. Explicit tool call**: add a `request_storage` platform tool that
agents can call during execution:

```json
{
  "name": "request_storage",
  "description": "Request that data be stored permanently for other agents to access.",
  "parameters": {
    "key": "A descriptive key for retrieval (e.g., 'molecule_smiles_aspirin')",
    "data": "The data to store",
    "tags": "Optional metadata tags for filtering"
  }
}
```

This tool doesn't write directly — it records the request. The
orchestrator processes it after task completion, routing to the storage
agent. This keeps the agent isolated (it doesn't know about the storage
agent) while allowing explicit storage intent.

### 12.8 Config

The storage agent is optional. Enable it by adding a `[storage_agent]`
section to config. The orchestrator is the only mandatory agent.

```toml
[storage_agent]
enabled = true
store_path = "~/.tengu/storage/data.jsonl"

# Maximum JSONL file size before rotation.
# Old data archived to data.jsonl.1, data.jsonl.2, etc.
max_file_size_mb = 100

# Auto-prune entries older than this.
prune_after_days = 90
```

**If `storage_agent.enabled = false`** (or section missing): the
orchestrator falls back to in-memory context passing (§6). No JSONL
file created. This is the zero-dependency default.

**If `storage_agent.enabled = true`**: the orchestrator spawns the
storage agent worker alongside other agents. It gets its own mpsc inbox
in the star topology (§3). The orchestrator holds its sender like any
other agent.

```rust
// In boot_orchestrator(), after building agent_runtimes:
let storage_tx = if config.storage_agent.enabled {
    let store_path = resolve_storage_path(&config.storage_agent);
    let (tx, rx) = mpsc::channel(64);  // Larger buffer — many writes.
    let outbox = orchestrator_tx.clone();
    tokio::spawn(storage_agent_worker(
        "storage".into(), rx, outbox, store_path,
    ));
    Some(tx)
} else {
    None
};
```

### 12.9 Relationship to existing MemoryService

The storage agent and `MemoryService` coexist. They serve different
access patterns:

| | Storage Agent (JSONL) | MemoryService (vectors) |
|-|----------------------|------------------------|
| **Access pattern** | Exact key/tag lookup | Semantic similarity search |
| **Use case** | Task outputs, artifacts, ensemble reports, structured data | "Find memories about aspirin synthesis", topic overviews |
| **Write cost** | File append (~0ms, $0) | Embedding API call (~200ms, ~$0.0001) |
| **Read cost** | JSONL scan (ms for <100K entries) | Vector search (ms for Qdrant, O(n) for disk) |
| **Cross-session** | Yes (JSONL file persists) | Yes (vectors.bin or Qdrant persists) |
| **Backend** | JSONL file (no dependencies) | Disk vectors or Qdrant (optional) |

**Migration path**: the existing orchestrator auto-summarize
(orchestrator.rs:673-705) currently stores `kind=topic_overview` via
`MemoryService`. This continues unchanged — topic overviews benefit from
semantic search. Only structured/exact data moves to the storage agent.

### 12.10 Impact on the event-bus architecture

The storage agent changes how data flows in both DAG (§1-11) and
fan-out (§11) modes:

**§4 Orchestrator event loop**: after processing `TaskCompletion`, the
orchestrator optionally routes the full output to the storage agent.
The `LivePlan.tasks[id].output` field stores only a summary (200 chars
instead of full text).

**§6 Data routing**: the two-tier model gains a Tier 0:

| Tier | Condition | Data source | Cost |
|------|-----------|-------------|------|
| **0: Storage retrieval** | Storage agent available, specific key known | JSONL query | $0 |
| **1: Artifact envelope** | Structured tool results | `LiveTask.artifacts` | $0 |
| **2: LLM summarization** | Unstructured text, > 500 chars, no storage agent | LLM planner call | ~$0.001 |

**§11 Fan-out**: ensemble results are persisted by the storage agent
(§11.10). No embedding cost for 200 writes. Retrieval by `run_id` tag
returns all results for comparison.

**§8 Error handling**: failed task outputs are also stored (with
`status=failed` tag). The storage agent enables post-mortem analysis of
failed runs without re-running agents.

### 12.11 Why JSONL

The storage format was chosen specifically for this architecture:

| Requirement | JSONL | SQLite | bincode (current vectors.bin) | Qdrant |
|-------------|-------|--------|-------------------------------|--------|
| Append-only writes | Native (file append) | WAL journal | Full rewrite | gRPC call |
| Human-debuggable | `cat data.jsonl \| jq` | `sqlite3 db` | No | API only |
| Zero dependencies | Yes | libsqlite3 | Yes | Server process |
| Concurrent reads | Yes (immutable past lines) | Yes (WAL) | RwLock | Yes |
| Concurrent writes | Flock per append | WAL | RwLock + flush | Yes |
| Streaming ingestion | Tail -f | Polling | No | Watch API |
| Crash safety | Lost at most last line | WAL recovery | Atomic rename | Server handles |

JSONL is the right choice for the MVP. If query patterns become complex
(joins, aggregations, time-range queries over millions of entries), the
storage agent's backend can be swapped to SQLite without changing the
event protocol between orchestrator and storage agent.

### 12.12 Context bloat prevention — the complete picture

With the storage agent in place, context bloat is prevented at every
layer:

```
Agent completes task
  │
  ▼
TaskCompletion arrives at orchestrator
  │
  ├─► LivePlan.task.output = truncate(output, 200)  ← bounded
  │
  ├─► Storage agent ← full output persisted to JSONL (off-heap)
  │
  ▼
Orchestrator dispatches downstream task
  │
  ├─► Context = {
  │     dep_summary: "200 char summary",        ← from LivePlan
  │     dep_artifacts: { tx_hash: "0x..." },    ← from LiveTask.artifacts
  │     retrieved_data: "specific field"         ← from storage agent (on demand)
  │   }
  │
  ▼
Downstream agent receives focused context (< 2 KB)
  instead of dumped prior outputs (48 KB+)
```

At 200 agents in fan-out mode:
- Orchestrator holds: 200 × 200 chars = **40 KB** in LivePlan
- Storage agent holds: 200 × ~2 KB = **400 KB** in JSONL file (on disk)
- No agent ever sees another agent's full output
- The orchestrator's event loop processes one event at a time — no
  accumulation of response bodies in memory

---

## 13. Commands and Cancellation

The event-bus architecture changes how user-facing commands work. The
orchestrator is the only mandatory agent, and all commands route through
it.

### 13.1 Command impact analysis

| Command | Current behavior | Event-bus behavior | Change needed |
|---------|-----------------|-------------------|---------------|
| `/stop` | Sets `AtomicBool` cancel on current chat turn (telegram_runtime.rs:1423) | Sends `OrchestratorEvent::Shutdown` to all running workers + cancels event loop | Yes — must propagate to N agents |
| `/reset` | Resets `ChatLoopState` for current agent (chat_commands.rs:30) | Resets conversation state for all agents + clears LivePlan/FanOutPlan | Yes — must reset pool state |
| `/purge` | Clears conversation + `MemoryStorePort::clear_all()` + deletes `.tengu-tasks`, `.tengu-attachments` (telegram_runtime.rs:1649) | Also clears storage agent JSONL + cleans pool workspace directories | Yes — must clear storage agent |
| `/team <goal>` | Triggers `orchestrate_team_goal()` with JoinSet batch execution (telegram_runtime.rs:1435) | Routes to `run_orchestrator()` (DAG) or `run_fan_out()` (ensemble) based on config | Yes — add mode detection |
| `/agents` | Lists agents with roles and tool counts (telegram_runtime.rs:1590) | Also lists pool agents (grouped by pool) + storage agent status | Yes — show pool info |
| `/project <name>` | Creates workspace subdirectory for all agents (telegram_runtime.rs:1475) | Unchanged — workspace creation is per-agent, event bus doesn't affect it | No |
| `/cost` | Shows session token stats for current agent (chat_commands.rs) | Shows aggregate token usage across all agents in current orchestration run | Yes — aggregate from `TaskCompletion` events |
| `/reload` | Re-scans skills, rebuilds tools (tui/mod.rs:300) | Must also rebuild pool agent runtimes if pool config changed | Yes — pool-aware reload |
| `/eco`, `/standard`, `/precise` | Lens switching for refiner | Unchanged — lens applies per-agent | No |
| `/context` | Shows context window usage | Unchanged per-agent, but orchestrator could show aggregate | Minor |

### 13.2 `/stop` — cancelling a running orchestration

Current `/stop` sets a single `AtomicBool` that is checked in
`collect_engine_response()` before each tool round and during stream
collection. This cancels one agent's current turn.

In the event-bus architecture, `/stop` must cancel **all** running
agents across the entire orchestration:

```rust
async fn handle_stop(
    agent_senders: &HashMap<AgentId, mpsc::Sender<OrchestratorEvent>>,
    cancel_flags: &HashMap<AgentId, Arc<AtomicBool>>,
) {
    // 1. Set cancel flags on all active agents (stops in-progress LLM calls).
    for flag in cancel_flags.values() {
        flag.store(true, Ordering::Relaxed);
    }

    // 2. Send Shutdown event to all workers (stops the inbox loop).
    let reason = "User requested /stop".to_string();
    for (_, tx) in agent_senders {
        let _ = tx.send(OrchestratorEvent::Shutdown {
            reason: reason.clone(),
            timestamp: Utc::now(),
        }).await;
    }
}
```

Each `agent_worker()` already handles `Shutdown` (§5) by breaking from
its inbox loop. The `cancel` flag (passed through to
`collect_engine_response()`) interrupts in-progress LLM streaming.

For Telegram, the response is:

```
/stop
→ Stopping orchestration... 47/200 agents completed, 10 in-progress cancelled, 143 not started.
→ Partial results available: /recall run_id:abc123
```

### 13.3 `/purge` — clearing storage and state

`/purge` currently calls `MemoryStorePort::clear_all()` and removes
`.tengu-tasks` + `.tengu-attachments`. With the storage agent:

```rust
async fn handle_purge(
    // Existing behavior:
    memory_handle: Option<&MemoryServiceHandle>,
    user_states: &mut HashMap<String, ChatLoopState>,
    workspaces: &[PathBuf],
    // New for event-bus:
    storage_tx: Option<&mpsc::Sender<OrchestratorEvent>>,
    pool_config: Option<&AgentPoolConfig>,
) {
    // 1. Reset all conversation states (existing).
    for state in user_states.values_mut() {
        state.reset_for_new_session();
    }

    // 2. Clear memory store (existing).
    if let Some(handle) = memory_handle {
        let _ = handle.store.clear_all().await;
    }

    // 3. Clear storage agent JSONL (NEW).
    if let Some(tx) = storage_tx {
        tx.send(OrchestratorEvent::TaskAssignment {
            task_id: "purge-storage".into(),
            agent_id: "storage".into(),
            description: "clear".into(),
            context: HashMap::from([
                ("operation".into(), json!("clear")),
            ]),
            correlation_id: "purge".into(),
            timestamp: Utc::now(),
        }).await.ok();
    }

    // 4. Clean pool workspaces (NEW).
    if let Some(pool) = pool_config {
        for ws in workspaces {
            cleanup_pool_workspaces(ws, &pool.agent_ids(), true, &HashMap::new())
                .await.ok();
        }
    }

    // 5. Clean .tengu-tasks, .tengu-attachments (existing).
    for ws in workspaces {
        for subdir in &[".tengu-tasks", ".tengu-attachments"] {
            let p = ws.join(subdir);
            if p.exists() { let _ = tokio::fs::remove_dir_all(&p).await; }
        }
    }
}
```

**Selective purge** — a new `/purge-run <run_id>` command deletes only
data from a specific ensemble run (storage agent entries matching
`run_id` tag + that run's workspace directories), without wiping
everything.

### 13.4 `/agents` — pool-aware listing

Current `/agents` lists agents with roles and tool counts. With pools:

```
Available agents:

Roles:
  hypothesis_researcher (default) — 12 tools
  onchain_minter — 8 tools

Pools:
  benchmark-runner — 20 agents (5 models × 4 copies)
    Models: claude-sonnet-4, gpt-4o, gemini-2.0-flash, llama-4-maverick, deepseek-r1
    Isolation: full | Max concurrency: 10

System:
  storage — JSONL backend (142 entries, 284 KB)

Routing: @role: message | /team <goal> | /fan-out <task>
```

### 13.5 `/fan-out` — new command for ensemble dispatch

A new command triggers fan-out mode explicitly:

```
/fan-out Analyze the mechanism of action of aspirin and provide a concise summary.
```

This dispatches to `run_fan_out()` (§11.7) with the configured agent
pool. Distinct from `/team <goal>` which uses DAG planning.

---

## 14. Sandbox and Fleet Isolation

### 14.1 What is a sandbox

A sandbox is a **standalone agent fleet configuration** loaded from
`sandboxes/<name>/config.toml`. Each sandbox defines its own agents,
orchestrator settings, memory backend, workspace, and skills. There is
no inheritance from the root config.

Current loading (main.rs:451):

```rust
fn load_sandbox_or(sandbox: Option<String>, default: Config) -> Result<Config> {
    match sandbox {
        None => Ok(default),
        Some(name) => {
            let path = PathBuf::from("sandboxes").join(&name).join("config.toml");
            Config::load(&path)
        }
    }
}
```

Usage: `tengu orchestrate --sandbox desci` loads
`sandboxes/desci/config.toml` which defines a 6-agent DeSci team.

### 14.2 Sandbox isolation guarantees

The event-bus architecture preserves and strengthens sandbox isolation:

| Boundary | How it's isolated |
|----------|-------------------|
| **Process** | Each `tengu orchestrate --sandbox X` is a separate OS process. No shared state between sandboxes at runtime. |
| **Config** | Standalone TOML. Agents, pools, orchestrator config, memory backend — all defined per-sandbox. |
| **Event bus** | One `EventBus` per process. Star topology (§3) exists entirely within the sandbox. No cross-sandbox channels. |
| **Storage agent** | JSONL file scoped to sandbox workspace: `~/desci-workspace/storage/data.jsonl`. Different sandboxes write to different files. |
| **Memory store** | Already scoped: disk backend uses `<workspace>/memory/`, Qdrant uses `tengu-memory-<workspace-name>` collection. |
| **Workspace** | Each sandbox has its own `scaffold.root` (e.g., `~/desci-workspace/`, `~/webstudio-project/`). No file overlap. |
| **Network** | Hub port per sandbox (DeSci: 7071, default: 7070). No port conflict. |
| **Agent pools** | `[[agent_pools]]` expand within the sandbox's agent map. Pool agents inherit the sandbox's workspace, memory scope, and skill packages. |
| **Token budgets** | Per-sandbox `LimitsConfig`. A DeSci sandbox budget doesn't affect a WebStudio sandbox. |

### 14.3 Running multiple sandboxes simultaneously

Each sandbox is a separate process:

```bash
tengu orchestrate --sandbox desci &     # PID 1234, port 7071
tengu orchestrate --sandbox webstudio & # PID 1235, port 7070
tengu telegram --sandbox desci &        # Telegram bot for DeSci
```

No shared state. If the DeSci sandbox's storage agent writes data, the
WebStudio sandbox cannot see it. If one sandbox crashes, others continue.

### 14.4 Primary pattern: per-agent config (DAG orchestration)

The main use case is a small team of precisely configured agents, each
with its own model, skills, capabilities, and dependency declarations.
This is the DeSci sandbox pattern — it works with the event bus
**unchanged**. No pools, no fan-out, no new config syntax.

```toml
# sandboxes/desci/config.toml — the primary pattern

[orchestrator]
enabled = true

[storage_agent]
enabled = true

[agents.hypothesis_researcher]
engine = "openrouter"
model = "anthropic/claude-sonnet-4.6"
role = "hypothesis_researcher"
default = true
workspace = "~/desci-workspace"
skill_packages = []
capabilities = ["workspace.read", "workspace.list", "workspace.write", "memory.remember"]

[agents.onchain_minter]
engine = "openrouter"
model = "anthropic/claude-sonnet-4.6"
role = "onchain_minter"
requires = ["hypothesis_researcher"]
workspace = "~/desci-workspace"
skill_packages = ["poi-register", "ipnft-mint"]
capabilities = ["workspace.read", "workspace.list", "workspace.write", "memory.remember",
                "http.request", "crypto.sign_tx", "crypto.sign_message",
                "crypto.wallet_address", "crypto.abi_encode"]

[agents.mol_labs]
engine = "openrouter"
model = "anthropic/claude-sonnet-4.6"
role = "mol_labs"
requires = ["onchain_minter"]
workspace = "~/desci-workspace"
skill_packages = ["molecule-auth", "molecule-project", "molecule-upload", "molecule-announcement"]
capabilities = ["workspace.read", "workspace.list", "workspace.write", "memory.remember",
                "http.request", "crypto.sign_message", "crypto.wallet_address"]

[agents.beach_scientist]
engine = "openrouter"
model = "anthropic/claude-sonnet-4.6"
role = "beach_scientist"
requires = ["hypothesis_researcher", "onchain_minter", "mol_labs"]
workspace = "~/desci-workspace"
skill_packages = ["beach-science"]
capabilities = ["workspace.read", "workspace.list", "workspace.write", "memory.remember",
                "http.request"]
```

This config has **zero** `[[agent_pools]]`. The event bus handles it
exactly as described in §4: `generate_plan()` decomposes the goal,
`repair_plan_dependencies()` enforces `requires` constraints,
`dispatch_ready_tasks()` drives execution. Each agent is individually
configured with its own model, skills, capabilities, and identity. The
`/team` command triggers DAG mode via `run_orchestrator()`.

This is the primary pattern because:
- Most real workflows need precise per-agent control (different models,
  different capabilities, different skill packages)
- Dependency chains are explicit (`requires` field)
- The planner LLM sees each agent's description and routes tasks
  to the right specialist
- 3-6 agents is the typical fleet size

### 14.5 Secondary pattern: agent pools (fan-out benchmarking)

Pools are an **optional addition** for a specific use case: running the
same task across many models for comparison. A sandbox can include both
per-agent configs AND pools:

```toml
# sandboxes/benchmark/config.toml — pools are optional

[orchestrator]
enabled = true
task_timeout_secs = 180

[storage_agent]
enabled = true

# Individual agent for planning/judging — normal per-agent config.
[agents.planner]
default = true
role = "planner"
engine = "openrouter"
model = "google/gemini-2.5-flash"
workspace = "~/benchmark-workspace"

# Pool — only used when /fan-out is invoked.
[[agent_pools]]
role = "benchmark-runner"
system_prompt = "Answer the research question concisely."
skill_packages = ["beach-science"]
isolation = "full"
workspace_isolation = true
models = [
    "anthropic/claude-sonnet-4",
    "openai/gpt-4o",
    "google/gemini-2.0-flash",
    "meta-llama/llama-4-maverick",
    "deepseek/deepseek-r1",
]
copies_per_model = 4
max_concurrency = 10
```

Pool agents are expanded at config load time (§11.6) and added to the
same `HashMap<String, Arc<AgentRuntime>>` as individually configured
agents. But they are only activated by `/fan-out` — the `/team` command
ignores pool agents and uses only the `[agents.*]` entries for DAG
planning.

**A sandbox with no `[[agent_pools]]` section works exactly as today.**
The pool feature is purely additive — it does not change how per-agent
configs are loaded, how the planner works, or how DAG execution runs.

### 14.6 Storage agent scoping

The storage agent's JSONL file lives within the sandbox workspace:

```rust
fn resolve_storage_path(config: &StorageAgentConfig, workspace: Option<&Path>) -> PathBuf {
    match workspace {
        Some(ws) => ws.join("storage").join("data.jsonl"),
        None => {
            let path = config.store_path.replace(
                "~", &dirs_next::home_dir().unwrap_or_default().to_string_lossy(),
            );
            PathBuf::from(path)
        }
    }
}
```

This mirrors the existing `resolve_memory_store_path()` pattern
(channel_runtime.rs:293) that scopes `vectors.bin` to the workspace.

---

## 15. Token and Cost Boundaries

### 15.1 Current token enforcement

The codebase has three layers of token control:

| Layer | Config field | Where enforced | What it does |
|-------|-------------|----------------|--------------|
| **Per-turn output cap** | `limits.max_output_tokens_per_turn` | Engine API call parameter | Limits tokens per single LLM response |
| **Per-flow budget** | `limits.max_tokens_per_flow` (default: 100K) | `chat_runtime.rs:149` — hard block at 100% | Accumulates input+output across all turns in a conversation flow. Warns at 80%, blocks at 100%. `/reset` clears. |
| **Per-task budget** | Same `max_tokens_per_flow`, cast to `u32` | `collect_engine_response(..., token_budget)` in orchestrator.rs:740 | Caps total tokens for a single orchestrated task (including all tool rounds). |

### 15.2 Gaps for the event-bus architecture

These per-agent/per-task budgets work at the individual level but miss
the aggregate:

| Problem | Scenario | Impact |
|---------|----------|--------|
| **No run-level budget** | 200 agents × 100K tokens each = 20M tokens | One fan-out run could cost $100+ with no cap |
| **No cost visibility before dispatch** | User types `/fan-out <task>` | No "this will cost ~$X, proceed?" prompt |
| **Token aggregation not visible** | `/cost` shows one agent's usage | No aggregate view across orchestration run |
| **No cost-based abort** | Rate-limited agents retry, spending more | No circuit breaker at the dollar level |

### 15.3 New: run-level token budget

Add `max_tokens_per_run` and `max_cost_per_run` to orchestrator config:

```toml
[orchestrator]
enabled = true
max_retries = 2
task_timeout_secs = 180
max_tokens_per_run = 2_000_000    # 2M tokens total across all agents
max_cost_per_run = 5.00           # $5 hard cap
```

Enforcement in the event loop:

```rust
struct RunBudget {
    max_tokens: u64,
    max_cost: Option<f64>,
    used_tokens: u64,
    used_input_tokens: u64,
    used_output_tokens: u64,
}

impl RunBudget {
    fn record(&mut self, usage: &TokenUsage) {
        self.used_input_tokens += usage.input_tokens as u64;
        self.used_output_tokens += usage.output_tokens as u64;
        self.used_tokens = self.used_input_tokens + self.used_output_tokens;
    }

    fn exceeded(&self) -> bool {
        self.used_tokens >= self.max_tokens
    }

    fn cost_exceeded(&self, price_per_1k_input: f64, price_per_1k_output: f64) -> bool {
        if let Some(max_cost) = self.max_cost {
            let cost = (self.used_input_tokens as f64 / 1000.0) * price_per_1k_input
                     + (self.used_output_tokens as f64 / 1000.0) * price_per_1k_output;
            cost >= max_cost
        } else {
            false
        }
    }
}
```

In `run_fan_out()` and `run_orchestrator()`, after each `TaskCompletion`:

```rust
OrchestratorEvent::TaskCompletion { token_usage, .. } => {
    run_budget.record(&token_usage);

    if run_budget.exceeded() {
        tracing::warn!(
            used = run_budget.used_tokens,
            max = run_budget.max_tokens,
            "Run token budget exceeded — cancelling remaining agents"
        );
        // Send Shutdown to all workers that haven't completed.
        for agent_id in &plan.pool.agent_ids {
            if !plan.results.contains_key(agent_id) {
                let _ = agent_senders[agent_id].send(
                    OrchestratorEvent::Shutdown {
                        reason: "Token budget exceeded".into(),
                        timestamp: Utc::now(),
                    }
                ).await;
            }
        }
        break;
    }
}
```

### 15.4 Pre-flight cost estimate

Before dispatching a fan-out run, show the estimated cost:

```
/fan-out Analyze aspirin mechanism of action

Estimated cost:
  20 agents × ~2K input tokens × avg $0.003/1K = ~$0.12 input
  20 agents × ~1K output tokens × avg $0.010/1K = ~$0.20 output
  Total estimate: ~$0.32

Proceed? (y/n)
```

The estimate uses:
- Input tokens: measured from the prompt length (known before dispatch)
- Output tokens: estimated from `max_output_tokens_per_turn` or a default
- Per-model pricing: from `Engine::available_models()` which already
  returns `ModelInfo` including pricing data

### 15.5 Aggregate `/cost` command

After a run, `/cost` shows aggregate usage:

```
/cost

Current run (abc123):
  Agents: 200 (178 completed, 15 failed, 7 timed out)
  Input tokens:  1,247,000 (avg 6,235/agent)
  Output tokens:   342,000 (avg 1,710/agent)
  Total tokens:  1,589,000
  Estimated cost: $2.47

  By model:
    claude-sonnet-4:  312K tokens, $0.94
    gpt-4o:           289K tokens, $0.58
    gemini-2.0-flash: 401K tokens, $0.12
    llama-4-maverick: 298K tokens, $0.45
    deepseek-r1:      289K tokens, $0.38

Session lifetime: 3 runs, 4,231,000 total tokens, $7.12
```

### 15.6 Per-agent budget still applies

The existing per-agent `max_tokens_per_flow` (LimitsConfig) continues to
work unchanged. It prevents a single agent from consuming the entire run
budget. In pool configs, it's inherited from the pool's `[limits]` section:

```toml
[[agent_pools]]
# ...
[agent_pools.limits]
max_tokens_per_flow = 8000    # Each agent capped at 8K tokens
```

At 200 agents × 8K max = 1.6M tokens worst case. The run-level
`max_tokens_per_run = 2_000_000` provides a safety net above the
per-agent sum.

**Token boundary layers (complete picture)**:

```
Per-turn output:  max_output_tokens_per_turn (Engine API limit)
  ↓
Per-task total:   max_tokens_per_flow (agent config, enforced in collect_engine_response)
  ↓
Per-run total:    max_tokens_per_run (orchestrator config, enforced in event loop)
  ↓
Per-run cost:     max_cost_per_run (orchestrator config, enforced in event loop)
```

Each layer is independent. An agent can hit its per-task limit without
triggering the run limit. The run limit catches the aggregate.

---

## 16. Gap Analysis: What the Event-Bus Must Not Lose

This section documents behaviors that exist in the current orchestrator
(CLI `boot_orchestrator()` and Telegram `orchestrate_team_goal()`) that
the event-bus design must preserve. Each gap is listed with the current
behavior, what the event-bus document currently says (or doesn't say),
and the resolution.

### 16.1 Secret redaction

**Current**: Both orchestrators wrap the tool executor in
`SanitizedToolExecutor` (orchestrator.rs:730, telegram_runtime.rs:819)
which redacts secrets from tool results. Telegram also redacts the final
summary before sending (line 958).

**Event-bus gap**: §5 and §11.4 `agent_worker()` take
`secret_registry: Arc<SecretRegistry>` as a parameter but the code
examples do not wrap the executor in `SanitizedToolExecutor`.

**Resolution**: `agent_worker()` must wrap the executor before calling
`execute_agent_task()`:

```rust
let sanitized = SanitizedToolExecutor::new(
    runtime.tool_executor.as_ref(),
    &secret_registry,
);
// Pass &sanitized to collect_engine_response(), not runtime.tool_executor
```

The Telegram adapter must also redact `TaskCompletion.output` before
rendering to the user.

### 16.2 Skill hot-reload

**Current**: Telegram hot-reloads skills before each task in a batch
(telegram_runtime.rs:618-631):
```rust
if skill_registry.reload(src) {
    rebuild_tools();
    rebuild_system_prompt();
}
```
If a SKILL.md file changes during a long orchestration run, the next
task uses the updated tools and system prompt.

**Event-bus gap**: `agent_worker()` receives a fixed `Arc<AgentRuntime>`
at spawn time. Skills are baked in. A 10-minute fan-out run would use
stale skills if SKILL.md was edited after spawn.

**Resolution**: For DAG mode (where tasks may take minutes each), the
agent worker should check for skill updates before each task execution:

```rust
OrchestratorEvent::TaskAssignment { .. } => {
    // Hot-reload skills before execution (DAG mode only).
    // Fan-out agents in Full isolation skip this — stale skills
    // are acceptable for benchmark validity.
    if !matches!(isolation, AgentIsolation::Full) {
        if let Some(registry) = &runtime.skill_registry {
            if registry.needs_reload() {
                // Rebuild tools and system_prompt in-place.
                // Requires interior mutability (RwLock) on AgentRuntime.
            }
        }
    }
    // ... proceed with execution
}
```

For fan-out (`AgentIsolation::Full`), hot-reload is intentionally
skipped — all agents must use the same skill version for valid
comparison.

### 16.3 Tool approval (Telegram inline keyboard)

**Current**: Telegram has `TelegramInlineApprovalAdapter` with 60-second
timeout (telegram_runtime.rs:142). Before executing certain tools, the
adapter sends an inline keyboard to the user asking for approval. If the
user denies, the cancel flag is set and the entire turn aborts.

**Event-bus gap**: `agent_worker()` has no approval adapter. Tool
execution proceeds unconditionally. In a 200-agent fan-out, individual
tool approvals are impractical anyway — but in DAG mode with 3-5 agents,
users expect approval prompts.

**Resolution**: The approval adapter must be injected into the agent
worker via `AgentRuntime`:

```rust
struct AgentRuntime {
    // ... existing fields ...
    approval_adapter: Option<Arc<dyn ToolApprovalPort>>,
}
```

- **DAG mode**: approval adapter is present (Telegram inline keyboard or
  CLI y/n prompt). Each agent can trigger approval.
- **Fan-out mode**: approval adapter is `None`. All tools execute without
  approval. This is intentional — you can't approve 200 concurrent tool
  calls. The pool config should restrict capabilities to safe tools only
  (read-only workspace, no shell, no crypto signing).

### 16.4 Typing indicators (Telegram)

**Current**: Telegram spawns a typing loop per task
(telegram_runtime.rs:769-778) that sends `ChatAction::Typing` every 4
seconds. This shows the user that work is happening.

**Event-bus gap**: The document mentions `Progress` events but these are
agent-initiated. The typing indicator is channel-adapter-initiated and
independent of agent behavior.

**Resolution**: The Telegram adapter subscribes to `TaskAssignment`
events (via the orchestrator outbox or a separate broadcast) and starts
a typing loop for each running task:

```rust
// Telegram adapter, listening to orchestrator event stream:
OrchestratorEvent::TaskAssignment { agent_id, .. } => {
    // Start typing indicator for this agent's task.
    let handle = tokio::spawn(typing_loop(pipe.clone(), recipient.clone()));
    typing_handles.insert(agent_id, handle);
}
OrchestratorEvent::TaskCompletion { agent_id, .. }
| OrchestratorEvent::TaskError { agent_id, .. } => {
    // Stop typing indicator.
    if let Some(handle) = typing_handles.remove(&agent_id) {
        handle.abort();
    }
}
```

In fan-out mode (200 agents), the adapter sends a single typing
indicator (not 200 concurrent ones) until all agents complete.

### 16.5 Plan display before execution

**Current**: Both orchestrators display the generated plan before
executing:
- CLI: prints batches with task descriptions (orchestrator.rs:501-521)
- Telegram: sends plan message with parallel notation + dependency info
  (telegram_runtime.rs:520-537)

**Event-bus gap**: `run_orchestrator()` and `run_fan_out()` receive a
ready-to-execute plan and start dispatching. No plan review step.

**Resolution**: The plan display is the channel adapter's responsibility,
not the orchestrator's. The flow is:

1. Generate plan (same as current)
2. **Channel adapter renders the plan** for the user
3. (Optional) User approves or modifies
4. Channel adapter calls `run_orchestrator()` / `run_fan_out()`

For fan-out, the display is:

```
Fan-out: "Analyze aspirin mechanism of action"
Pool: benchmark-runner (20 agents, 5 models × 4 copies)
Max concurrency: 10 | Estimated cost: ~$0.32
Proceed? (/stop to cancel)
```

### 16.6 RAG enrichment before planning

**Current**: Both orchestrators query memory for `kind=topic_overview` +
`source=orchestrator` before calling `generate_plan()`
(orchestrator.rs:427-449, telegram_runtime.rs:443-474). Past
orchestration summaries are prepended to the goal.

**Event-bus gap**: The document shows `generate_plan()` taking a `goal`
string but doesn't mention RAG enrichment. The event-bus diagram
(§4) starts at "Initial dispatch" with a ready LivePlan.

**Resolution**: RAG enrichment happens *before* the event loop, in the
same layer that calls `generate_plan()`:

```
User input → RAG enrich (MemoryService recall) → generate_plan() → LivePlan → run_orchestrator()
```

With the storage agent (§12), enrichment can also query JSONL for
recent run results:

```
User input → query storage agent (recent ensemble reports)
           → query MemoryService (topic overviews)
           → generate_plan()
```

This is the caller's responsibility, not the event loop's. The existing
orchestrator code moves unchanged into the pre-loop setup.

### 16.7 Direct dispatch ("role: task")

**Current**: CLI supports `role: task` syntax (orchestrator.rs:369-406)
for single-agent dispatch without planning. This bypasses
`generate_plan()` entirely.

**Event-bus gap**: The document describes `ExecutionMode::DagPlan` and
`ExecutionMode::FanOut` but has no single-agent dispatch mode.

**Resolution**: Single-agent dispatch doesn't need the event bus. It's
a direct call to `execute_agent_task()` on the specified agent's
runtime — the same function the agent worker calls internally. The event
bus is only used for multi-agent orchestration.

```rust
enum ExecutionMode {
    /// Direct dispatch: one agent, one task, no planning.
    Direct { agent_id: AgentId, task: String },
    /// DAG execution (§1–8).
    DagPlan(LivePlan),
    /// Fan-out: identical task to N agents (§11).
    FanOut(FanOutPlan),
}
```

For Telegram, `classify_request()` (task_planner.rs:22-64) decides the
mode:
- `RouteDecision::SingleAgent(role)` → `ExecutionMode::Direct`
- `RouteDecision::MultiAgent` → `ExecutionMode::DagPlan`
- `/fan-out` command → `ExecutionMode::FanOut`

### 16.8 Attachment handling (Telegram)

**Current**: `/team` saves Telegram file attachments to
`.tengu-attachments/` and prepends file notes to the goal
(telegram_runtime.rs:389-421).

**Event-bus gap**: Not mentioned. If a user sends a PDF with `/team
Analyze this paper`, the event-bus orchestrator would receive only the
text, not the file.

**Resolution**: Attachment handling is a channel adapter concern. The
Telegram adapter processes attachments *before* entering the event loop:

```
User sends: /team Analyze this paper [file.pdf]
  → Telegram adapter saves file to workspace/.tengu-attachments/
  → Adapter prepends "Attached: /path/to/file.pdf" to goal string
  → Goal enters plan generation
  → run_orchestrator() receives enriched goal
```

No change needed in the event-bus itself. The adapter transforms the
input before the orchestrator sees it.

### 16.9 REPL behavior (CLI interactive loop)

**Current**: `boot_orchestrator()` is a REPL — the user submits goals
repeatedly, each gets a fresh plan, `step_results` and `run_state` are
cleared per goal.

**Event-bus gap**: `run_orchestrator()` is described as a one-shot
function that returns `PlanOutcome`.

**Resolution**: This is correct. The REPL lives *outside* the event loop:

```rust
loop {
    let input = read_stdin().await;
    let plan = generate_plan(engine, &input, &agent_descriptions).await?;
    let live_plan = LivePlan::from(plan);
    let outcome = run_orchestrator(live_plan, rx, agent_senders, ...).await?;
    display_outcome(&outcome);
    // Agent workers stay alive between goals (long-lived §5).
    // Only the LivePlan is created fresh per goal.
}
```

Agent workers (§5) are long-lived — spawned once at boot, reused across
goals. Only the LivePlan and RunBudget are created fresh per goal.

### 16.10 Storage agent `clear` operation

**Current**: §13.3 `/purge` sends a `clear` operation to the storage
agent, but §12.5 only defines `write`, `read`, `query`, `list`.

**Resolution**: Add `clear` and `prune` to the storage agent operations:

```rust
Some("clear") => {
    // Truncate the JSONL file.
    file.set_len(0)?;
    file.seek(std::io::SeekFrom::Start(0))?;
    Ok("cleared".to_string())
}
Some("prune") => {
    // Remove entries older than N days (from config).
    let cutoff = context["before_epoch"].as_u64().unwrap();
    handle_prune(&store_path, cutoff)
}
```

### 16.11 JSONL concurrent write safety

**Current**: The storage agent is a single `tokio::spawn` worker
processing events sequentially from its mpsc inbox. This means writes
are serialized — no concurrent file access.

**However**: if two orchestrator runs somehow share a storage worker, or
if the JSONL file is opened by an external process (e.g., `tail -f`),
there's a race.

**Resolution**: The design is safe as-is for the expected usage (one
storage worker per sandbox process). The storage agent owns the file
handle exclusively. External readers that use `tail -f` will see
consistent lines because each write is a single `writeln!()` + `flush()`
— atomic at the OS level for lines under the pipe buffer size (4 KB on
most systems, JSONL lines are typically 1-2 KB).

### 16.12 format_task_prompt() specification

**Current**: CLI uses `build_step_context()` (orchestrator.rs:760-792)
and Telegram uses `build_task_prompt()` (telegram_runtime.rs:1082-1128)
— different formats, different truncation limits, different footer
instructions.

**Event-bus gap**: `format_task_prompt()` was called in agent workers as
if it were their responsibility to render the prompt. This was resolved —
`dispatch_task()` (§4) now calls `format_task_prompt(goal, description,
context)` once, and workers receive the fully rendered prompt in the
`description` field. The function definition below is the canonical one.

**Resolution**: `format_task_prompt()` replaces both `build_step_context`
and `build_task_prompt` with a unified format:

```rust
fn format_task_prompt(
    goal: &str,
    description: &str,
    context: &HashMap<String, Value>,
) -> String {
    // The goal MUST be included. The agent's system prompt is built from
    // agent config + skills during runtime construction (orchestrator.rs:150),
    // NOT from the current orchestration goal. Both current prompt builders
    // include it: build_step_context() (orchestrator.rs:761) and
    // build_task_prompt() (telegram_runtime.rs:320).
    let mut prompt = format!("## Overall Goal\n{}\n\n## Your Task\n{}\n", goal, description);

    if !context.is_empty() {
        prompt.push_str("\n## Context from Prior Steps\n");
        for (key, value) in context {
            if key.ends_with("_summary") {
                prompt.push_str(&format!("### {}\n{}\n\n", key, value));
            } else if key.ends_with("_artifacts") {
                prompt.push_str(&format!(
                    "### Artifacts: {}\n```json\n{}\n```\n\n",
                    key, value
                ));
            }
        }
    }

    prompt.push_str(
        "\n## Rules\n\
        - Return a concise outcome summary\n\
        - Include key results, file paths, URLs, IDs\n\
        - NEVER fabricate URLs, hashes, or IDs\n\
        - Every ID must come from a tool result\n\
        - If a tool fails, STOP and report the error\n"
    );

    prompt
}
```

### Summary: regression risk assessment

| Gap | Severity | Status |
|-----|----------|--------|
| Secret redaction missing in worker | **Critical** — secrets leak | Must fix before Phase 2 |
| Skill hot-reload missing | **Medium** — stale tools during long DAG runs | Fix in Phase 2, skip for fan-out |
| Tool approval missing | **Medium** — Telegram users lose approval UX | Fix in Phase 5, not needed for fan-out |
| Typing indicators missing | **Low** — UX regression, not functional | Fix in Phase 5 |
| Plan display missing | **Low** — UX regression | Channel adapter concern, Phase 4-5 |
| RAG enrichment not shown | **Low** — logic exists, just not in event loop | Pre-loop setup, no code change |
| Direct dispatch missing | **Medium** — CLI loses "role: task" mode | Add `ExecutionMode::Direct`, Phase 4 |
| Attachment handling missing | **Low** — Telegram-only, adapter concern | Phase 5 |
| REPL behavior unclear | **Low** — workers are long-lived, loop is external | Clarified above |
| Storage `clear` operation missing | **Low** — `/purge` incomplete | Phase 7 |
| `format_task_prompt()` contract | **Resolved** — defined in §16.12, called once in `dispatch_task()` (§4) | Workers receive rendered prompt in `description` field |
| JSONL concurrent safety | **None** — single worker, serialized writes | Already safe |

---

## 17. Worked Example: DeSci Full Pipeline via Telegram

End-to-end trace of a real user request through the event-bus
architecture using the `sandboxes/desci/config.toml` fleet.

### 17.1 User request

The user sends a Telegram message with a PDF attachment and an image:

```
/team Read hypothesis document I provided here, use privy wallet to
register POI and mint ip-nft for it based on the hypothesis, description
based on it. Use image for ip-nft logo I added here as image. After
that create Project (Data Room) for the pdf document, after you create
data room upload provided hypothesis to it and create an announcement.
Then create post on beach-science based on the hypothesis document and
add clickable link to ipnft created and data-room, use format
[title](link), so it would be pretty and clickable.
```

Attached: `hypothesis.pdf`, `logo.png`

### 17.2 Pre-loop: Telegram adapter processing

Before the event loop starts, the Telegram adapter handles channel-
specific concerns (§16.8):

```
1. Save attachments:
   ~/desci-workspace/.tengu-attachments/hypothesis.pdf
   ~/desci-workspace/.tengu-attachments/logo.png

2. Prepend file notes to goal:
   "Attached files:\n- .tengu-attachments/hypothesis.pdf\n- .tengu-attachments/logo.png\n\n"
   + original user message

3. RAG enrichment (§16.6):
   Query MemoryService for kind=topic_overview, source=orchestrator
   → Maybe finds: "Goal: Mint IPNFT for aspirin hypothesis\nResults: ..."
   → Prepend as "## Relevant Prior Work" (informational only)
```

### 17.3 Plan generation

The planner engine (Gemini 2.5 Flash, per sandbox config) decomposes
the goal into tasks. It sees the agent descriptions and `requires`
constraints:

```
Agents available:
- hypothesis_researcher: senior scientific researcher, PDF analysis
- onchain_minter: DeSci on-chain specialist (requires: hypothesis_researcher)
- mol_labs: Molecule Labs project manager (requires: onchain_minter)
- beach_scientist: DeSci communicator (requires: hypothesis_researcher, onchain_minter, mol_labs)
- custodian: NFT transfer (requires: onchain_minter, mol_labs, beach_scientist)
- wallet_manager: Privy wallet operations (no requires)
```

Generated plan (JSON from LLM):

```json
{
  "tasks": [
    {
      "id": "research",
      "role": "hypothesis_researcher",
      "task": "Read hypothesis.pdf from .tengu-attachments/, extract core hypothesis, methodology, and findings. Write summary to research/summaries/summary.md and hypothesis to research/hypothesis.md.",
      "depends_on": []
    },
    {
      "id": "mint",
      "role": "onchain_minter",
      "task": "Register POI for hypothesis.pdf, then mint IP-NFT using the hypothesis description from research/hypothesis.md. Use .tengu-attachments/logo.png as the IP-NFT image. Save results to mint/metadata/.",
      "depends_on": ["research"]
    },
    {
      "id": "data-room",
      "role": "mol_labs",
      "task": "Authenticate with Molecule, create a Project (Data Room) for the minted IP-NFT, upload hypothesis.pdf to the data room, and create an announcement. Save project URL to uploads/project_result.json.",
      "depends_on": ["mint"]
    },
    {
      "id": "publish",
      "role": "beach_scientist",
      "task": "Create a Beach.science post about the hypothesis. Include clickable links using [title](url) format for: the IP-NFT (Etherscan tx link) and the Molecule data room (project URL from uploads/project_result.json).",
      "depends_on": ["research", "mint", "data-room"]
    }
  ]
}
```

Note: `custodian` is NOT included — the user didn't ask for NFT
transfer. The planner follows the "only create tasks user explicitly
asked for" rule.

### 17.4 Dependency validation

```
repair_plan_dependencies():
  - onchain_minter requires hypothesis_researcher
    → "mint" depends_on ["research"] ✓ (already satisfied)
  - mol_labs requires onchain_minter
    → "data-room" depends_on ["mint"] ✓ (already satisfied)
  - beach_scientist requires hypothesis_researcher, onchain_minter, mol_labs
    → "publish" depends_on ["research", "mint", "data-room"] ✓ (all satisfied)
  → 0 edges added (LLM got it right)

validate_plan_dependencies():
  → All requires constraints satisfied transitively ✓
```

### 17.5 Plan display to user

Telegram adapter sends:

```
Plan (4 tasks):

1. [hypothesis_researcher] research
   Read hypothesis.pdf, extract hypothesis, write summary

2. [onchain_minter] mint (after: research)
   Register POI + mint IP-NFT with logo.png

3. [mol_labs] data-room (after: mint)
   Create project, upload PDF, announce

4. [beach_scientist] publish (after: research, mint, data-room)
   Beach.science post with [title](link) format
```

### 17.6 LivePlan construction and event loop

```rust
LivePlan {
    goal: "Read hypothesis document...",
    tasks: {
        "research":  LiveTask { depends_on: [],                          status: Pending },
        "mint":      LiveTask { depends_on: ["research"],                status: Pending },
        "data-room": LiveTask { depends_on: ["mint"],                    status: Pending },
        "publish":   LiveTask { depends_on: ["research","mint","data-room"], status: Pending },
    },
    revision: 0,
}
```

### 17.7 Event-bus execution trace

```
t=0.0s  ORCHESTRATOR: dispatch_ready_tasks()
        → "research" has no deps → Ready → DISPATCHED
        → "mint" blocked by ["research"]
        → "data-room" blocked by ["mint"]
        → "publish" blocked by ["research","mint","data-room"]

        dispatch_task() calls format_task_prompt() and sends:
        TaskAssignment {
            task_id: "research",
            agent_id: "hypothesis_researcher",
            description: "## Overall Goal\nRead hypothesis document...\n\n## Your Task\nRead hypothesis.pdf from .tengu-attachments/...\n\n## Rules\n- Return a concise outcome summary\n...",
            context: {},     // No upstream data — first task
        }

        [hypothesis_researcher worker receives assignment]
        → Typing indicator starts (Telegram adapter)

t=0.0s  HYPOTHESIS_RESEARCHER executes:
        1. list_directory(".tengu-attachments/")
           → ["hypothesis.pdf", "logo.png"]
        2. read_file(".tengu-attachments/hypothesis.pdf")
           → (PDF content extracted by workspace tool)
        3. Analyzes document, identifies hypothesis, methods, findings
        4. write_file("research/summaries/summary.md", ...)
        5. write_file("research/hypothesis.md", ...)
        → Returns EngineResponse with text + tool_outcomes

t=25s   EventBus receives: TaskCompletion {
            task_id: "research",
            agent_id: "hypothesis_researcher",
            output: "Analyzed hypothesis.pdf. Core hypothesis: ... [truncated]",
            artifacts: {},
            token_usage: TokenUsage { input: 3200, output: 1800 },
            duration: 25s,
        }

        ORCHESTRATOR:
        → plan.tasks["research"].status = Completed
        → plan.tasks["research"].output = truncate(output, 200)
          = "Analyzed hypothesis.pdf. Core hypothesis: Aspirin
             inhibits COX-2 via acetylation of Ser530..."
        → Storage agent: write full output to JSONL
          key="task/research/output", kind="task_output"

        → dispatch_ready_tasks():
          "mint": depends_on ["research"] → research=Completed ✓ → Ready → DISPATCHED
          "data-room": depends_on ["mint"] → mint=Pending ✗
          "publish": depends_on ["research","mint","data-room"] → mint=Pending ✗

        dispatch_task() renders prompt and sends:
        TaskAssignment {
            task_id: "mint",
            agent_id: "onchain_minter",
            description: "## Overall Goal\nRead hypothesis document...\n\n## Your Task\nRegister POI + mint IP-NFT...\n\n## Context from Prior Steps\n### research_summary\nAnalyzed hypothesis.pdf...\n\n## Rules\n...",
            context: {
                "research_summary": "Analyzed hypothesis.pdf. Core hypothesis: Aspirin inhibits...",
                "research_artifacts": {}
            },
        }

        Telegram: "✓ research — Analyzed hypothesis.pdf"
        [onchain_minter worker receives assignment, typing starts]

t=25s   ONCHAIN_MINTER executes:
        1. read_file("research/hypothesis.md") → gets hypothesis text
        2. POI Registration (poi-register skill):
           a. http_request POST to Molecule POI endpoint
              with hypothesis.pdf reference
           b. → poi_hash, merkle_root
           c. write_file("mint/metadata/poi_result.json", ...)
        3. IP-NFT Minting (ipnft-mint skill, 10 steps):
           a. get_wallet_address() → privy wallet address
           b-h. http_request calls to Molecule GraphQL
                (reserve, metadata, authorization signature)
           i. abi_encode → build mint calldata
           j. sign_and_send_transaction → SENDS TX
              ← Telegram inline keyboard: "Approve mint tx? [Yes] [No]"
              ← User taps [Yes]
              → tx_hash: 0xabc123...
           k. write_file("mint/metadata/mint_result.json", {
                ipnft_token_id: "42",
                ipnft_symbol: "IPNFT-42",
                tx_hash: "0xabc123...",
                image_url: "ipfs://..."
              })

t=85s   EventBus receives: TaskCompletion {
            task_id: "mint",
            agent_id: "onchain_minter",
            output: "POI registered (hash: 0xdef...). IP-NFT minted:
                     token #42, tx 0xabc123... Image: ipfs://...",
            artifacts: { "tx_hash": "0xabc123...", "token_id": "42" },
            token_usage: TokenUsage { input: 8500, output: 3200 },
            duration: 60s,
        }

        ORCHESTRATOR:
        → plan.tasks["mint"].status = Completed
        → plan.tasks["mint"].output = truncate(output, 200)
        → plan.tasks["mint"].artifacts = { tx_hash, token_id }
        → Storage agent: write full output to JSONL

        → dispatch_ready_tasks():
          "data-room": depends_on ["mint"] → mint=Completed ✓ → Ready → DISPATCHED
          "publish": depends_on ["research","mint","data-room"]
                     → research ✓, mint ✓, data-room=Pending ✗

        dispatch_task() renders prompt and sends:
        TaskAssignment {
            task_id: "data-room",
            agent_id: "mol_labs",
            description: "## Overall Goal\nRead hypothesis document...\n\n## Your Task\nAuthenticate with Molecule, create Project...\n\n## Context from Prior Steps\n### research_summary\n...\n### mint_summary\n...\n### Artifacts: mint_artifacts\n...\n\n## Rules\n...",
            context: {
                "research_summary": "Analyzed hypothesis.pdf...",
                "mint_summary": "POI registered. IP-NFT minted: token #42, tx 0xabc123...",
                "mint_artifacts": { "tx_hash": "0xabc123...", "token_id": "42" },
            },
        }

        Telegram: "✓ mint — IP-NFT #42 minted (tx: 0xabc123...)"
        [mol_labs worker receives assignment, typing starts]

t=85s   MOL_LABS executes:
        1. Authenticate (molecule-auth skill):
           a. get_wallet_address() → wallet
           b. http_request to get SIWE nonce
           c. sign_message (SIWE message)
           d. http_request to verify → bearer token
        2. Create Project (molecule-project skill):
           a. read_file("mint/metadata/mint_result.json") → token_id, symbol
           b. http_request POST to Molecule GraphQL: createProject
           c. → project_id, data_room_id
        3. Upload (molecule-upload skill):
           a. http_request: initiate upload → presigned S3 URL
           b. http_request: PUT hypothesis.pdf to S3
           c. http_request: finalize upload
        4. Announcement (molecule-announcement skill):
           a. http_request POST: create announcement
        5. write_file("uploads/project_result.json", {
             project_url: "https://testnet.molecule.to/ipnfts/42",
             data_room_id: "dr_789",
             announcement_id: "ann_456"
           })

t=130s  EventBus receives: TaskCompletion {
            task_id: "data-room",
            agent_id: "mol_labs",
            output: "Project created. Data room: dr_789. Hypothesis uploaded.
                     Announcement posted. URL: https://testnet.molecule.to/ipnfts/42",
            artifacts: {
                "project_url": "https://testnet.molecule.to/ipnfts/42",
                "data_room_id": "dr_789",
            },
            token_usage: TokenUsage { input: 6100, output: 2400 },
            duration: 45s,
        }

        ORCHESTRATOR:
        → plan.tasks["data-room"].status = Completed
        → Storage agent: write full output

        → dispatch_ready_tasks():
          "publish": depends_on ["research","mint","data-room"]
                     → research ✓, mint ✓, data-room ✓ → ALL MET → Ready → DISPATCHED

        dispatch_task() renders prompt and sends:
        TaskAssignment {
            task_id: "publish",
            agent_id: "beach_scientist",
            description: "## Overall Goal\nRead hypothesis document...\n\n## Your Task\nCreate Beach.science post with [title](link)...\n\n## Context from Prior Steps\n### research_summary\n...\n### mint_summary\n...\n### Artifacts: mint_artifacts\n...\n### data-room_summary\n...\n### Artifacts: data-room_artifacts\n...\n\n## Rules\n...",
            context: {
                "research_summary": "Analyzed hypothesis.pdf. Core hypothesis: Aspirin...",
                "mint_summary": "POI registered. IP-NFT minted: token #42, tx 0xabc123...",
                "mint_artifacts": { "tx_hash": "0xabc123...", "token_id": "42" },
                "data-room_summary": "Project created. URL: https://testnet.molecule.to/ipnfts/42",
                "data-room_artifacts": {
                    "project_url": "https://testnet.molecule.to/ipnfts/42",
                    "data_room_id": "dr_789",
                },
            },
        }

        Telegram: "✓ data-room — Project created, hypothesis uploaded"
        [beach_scientist worker receives assignment, typing starts]

t=130s  BEACH_SCIENTIST executes:
        1. read_file("uploads/project_result.json") → project_url
        2. Compose post body with:
           - Hypothesis summary from context
           - [View IP-NFT on Etherscan](https://sepolia.etherscan.io/tx/0xabc123...)
           - [View Data Room](https://testnet.molecule.to/ipnfts/42)
        3. http_request POST to Beach.science API:
           auth_bearer_env="BEACH_API_KEY"
           body: { content: "## Aspirin COX-2 Hypothesis\n\n..." }
        4. write_file("posts/beach_post_result.json", { post_id, post_url })

t=148s  EventBus receives: TaskCompletion {
            task_id: "publish",
            agent_id: "beach_scientist",
            output: "Published to Beach.science. Post URL: https://beach.science/p/123",
            artifacts: { "post_url": "https://beach.science/p/123" },
            token_usage: TokenUsage { input: 4200, output: 1100 },
            duration: 18s,
        }

        ORCHESTRATOR:
        → plan.tasks["publish"].status = Completed
        → Storage agent: write full output + aggregate report
        → dispatch_ready_tasks() → no Pending tasks left
        → plan.is_complete() = true → EXIT LOOP
```

### 17.8 Execution timeline (dependency graph)

```
t=0          t=25         t=85           t=130        t=148
│            │            │              │            │
▼            ▼            ▼              ▼            ▼
┌──────────┐
│ research │ (25s, no deps)
│ PDF→md   │
└────┬─────┘
     │ depends_on
     ▼
     ┌───────────────────┐
     │ mint              │ (60s, after research)
     │ POI + IPNFT       │
     │ [user approves tx]│
     └────┬──────────────┘
          │ depends_on
          ▼
          ┌──────────────────────┐
          │ data-room            │ (45s, after mint)
          │ auth→project→upload  │
          │ →announce            │
          └────┬─────────────────┘
               │ depends_on (+ research, mint)
               ▼
               ┌───────────────┐
               │ publish       │ (18s, after all)
               │ Beach.science │
               └───────────────┘

Total wall-clock: 148s (~2.5 min)
Total tokens: 30,500 (22,000 in / 8,500 out)
```

This pipeline is fully sequential because each step genuinely depends
on the previous one's output (tx_hash, token_id, project_url). There
is no artificial batching — each task starts the instant its
dependencies complete.

### 17.9 What the user sees in Telegram

```
[User sends /team message with PDF + image]

Bot: Plan (4 tasks):
     1. [hypothesis_researcher] research
     2. [onchain_minter] mint (after: research)
     3. [mol_labs] data-room (after: mint)
     4. [beach_scientist] publish (after: research, mint, data-room)

[typing indicator...]

Bot: ✓ research — Analyzed hypothesis.pdf. Core hypothesis:
     Aspirin inhibits COX-2 via acetylation of Ser530.
     Summary: research/summaries/summary.md

[typing indicator...]

Bot: 🔐 Approve transaction?
     sign_and_send_transaction: mint IP-NFT #42
     To: 0x1234...IPNFTContract
     Value: 0 ETH
     [Approve] [Deny]

[User taps Approve]

Bot: ✓ mint — IP-NFT #42 minted
     POI: 0xdef456...
     TX: https://sepolia.etherscan.io/tx/0xabc123...

[typing indicator...]

Bot: ✓ data-room — Project created
     Data room: https://testnet.molecule.to/ipnfts/42
     Hypothesis uploaded, announcement posted

[typing indicator...]

Bot: ✓ publish — Posted to Beach.science
     https://beach.science/p/123

Bot: Team complete — 4/4 tasks done.

     ✓ research (hypothesis_researcher)
       Analyzed PDF, wrote summary + hypothesis

     ✓ mint (onchain_minter)
       POI registered, IP-NFT #42 minted

     ✓ data-room (mol_labs)
       Project created, PDF uploaded, announced

     ✓ publish (beach_scientist)
       Beach.science post with clickable links

     Tokens: 30.5K | Cost: ~$0.18 | Duration: 2m 28s
```

### 17.10 Storage agent JSONL after this run

```jsonl
{"ts":1710756025,"key":"task/research/output","kind":"task_output","tags":{"task_id":"research","agent_id":"hypothesis_researcher","role":"hypothesis_researcher","status":"completed"},"data":"Analyzed hypothesis.pdf. Core hypothesis: Aspirin inhibits COX-2 via acetylation of Ser530, preventing prostaglandin synthesis. The paper presents in-vitro evidence with IC50 measurements across three cell lines...\n\n## Tool Results\n### write_file\nWrote research/summaries/summary.md (2.4 KB)..."}
{"ts":1710756085,"key":"task/mint/output","kind":"task_output","tags":{"task_id":"mint","agent_id":"onchain_minter","role":"onchain_minter","status":"completed"},"data":"POI registered (hash: 0xdef456..., merkle_root: 0x789...). IP-NFT minted: token #42, symbol IPNFT-42, tx 0xabc123... Image uploaded to IPFS: ipfs://Qm...\n\n## Tool Results\n### sign_and_send_transaction\n{\"tx_hash\":\"0xabc123...\",\"status\":\"confirmed\"}..."}
{"ts":1710756130,"key":"task/data-room/output","kind":"task_output","tags":{"task_id":"data-room","agent_id":"mol_labs","role":"mol_labs","status":"completed"},"data":"Authenticated with Molecule (wallet: 0x999...). Project created: dr_789, linked to IPNFT-42. Hypothesis.pdf uploaded (3 steps: initiate, S3 PUT, finalize). Announcement posted: ann_456. Project URL: https://testnet.molecule.to/ipnfts/42..."}
{"ts":1710756148,"key":"task/publish/output","kind":"task_output","tags":{"task_id":"publish","agent_id":"beach_scientist","role":"beach_scientist","status":"completed"},"data":"Published hypothesis post to Beach.science (post_id: 123). Content includes clickable links: [View IP-NFT on Etherscan](https://sepolia.etherscan.io/tx/0xabc123...) and [View Data Room on Molecule](https://testnet.molecule.to/ipnfts/42)..."}
{"ts":1710756148,"key":"orchestration/summary","kind":"topic_overview","tags":{"source":"orchestrator","goal":"Read hypothesis document..."},"data":"Goal: Read hypothesis document...\n\nResults:\n- hypothesis_researcher (ok): Analyzed PDF, wrote summary\n- onchain_minter (ok): POI + IPNFT #42 minted\n- mol_labs (ok): Project created, uploaded, announced\n- beach_scientist (ok): Beach.science post published"}
```

### 17.11 Context routing — what each agent actually saw

The key difference from the current architecture: no agent ever received
the full output of all prior agents. Here's exactly what each got:

| Agent | Context received | Size |
|-------|-----------------|------|
| **hypothesis_researcher** | Empty `{}` — first task, no upstream | 0 bytes |
| **onchain_minter** | `research_summary` (200 chars) | ~200 bytes |
| **mol_labs** | `research_summary` + `mint_summary` + `mint_artifacts` (tx_hash, token_id) | ~500 bytes |
| **beach_scientist** | `research_summary` + `mint_summary` + `mint_artifacts` + `data-room_summary` + `data-room_artifacts` (project_url, data_room_id) | ~800 bytes |

Current architecture would have passed: 8KB × 3 prior tasks = **24 KB**
of prior output into `beach_scientist`'s context. Event-bus passes
**800 bytes** of curated summaries + artifacts. The agents read specific
files from workspace (`mint_result.json`, `project_result.json`) for
detailed data — which is what they already do today.

### 17.12 Where parallelism WOULD happen

This specific pipeline is sequential because each step genuinely needs
the prior step's output. But if the user had asked a different question:

```
/team Research the hypothesis PDF. Also create a Beach.science post
about our general DeSci approach (no links needed).
```

The planner would generate:

```json
{
  "tasks": [
    { "id": "research", "role": "hypothesis_researcher", "depends_on": [] },
    { "id": "post", "role": "beach_scientist", "depends_on": [] }
  ]
}
```

Both tasks have no dependencies → `dispatch_ready_tasks()` marks both
Ready at t=0 → both execute in parallel on separate agent workers.
Current batch model would also parallelize this, but only because they
happen to be in the same batch. The event-bus does it for the right
reason: no dependency edges.

### !!! IMPORTANT
I should never guess as an ingeneer of what happens. So every step, every hop, everythign should be tracked with loggin. Logs should be reach enough to be able to deliver me enough info to be able to debug, what been passed where, what info stored and as much available info as possible, what function is executed, where and so on.