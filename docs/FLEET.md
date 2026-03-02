# Fleet Orchestration Guide

Run multiple AI agents as a coordinated fleet. Each agent has a role, tasks are assigned and tracked, and a heartbeat loop monitors for stalled work.

## Overview

```
                        ┌─────────────────┐
                        │  Orchestrator   │
                        │  (heartbeat +   │
                        │   task router)  │
                        └────────┬────────┘
                                 │
              ┌──────────────────┼──────────────────┐
              │                  │                   │
     ┌────────▼───────┐ ┌───────▼────────┐ ┌───────▼────────┐
     │   QA Agent     │ │ Backend Agent  │ │ Integrator     │
     │ (testing,      │ │ (code,         │ │ (APIs,         │
     │  validation)   │ │  architecture) │ │  data flow)    │
     └────────────────┘ └────────────────┘ └────────────────┘
```

The orchestrator:
- Registers agents with roles from config
- Matches incoming tasks to idle agents by role
- Runs a periodic heartbeat to detect stalled work
- Retries failed tasks automatically
- Emits domain events for observability

## Quick Setup

### 1. Configure Agents with Roles

```toml
[orchestrator]
enabled = true
heartbeat_interval_s = 30
max_retries = 3

[agents.qa]
engine = "openrouter"
model = "anthropic/claude-sonnet-4"
role = "qa"
skills = ["search", "test_runner", "lint"]

[agents.qa.identity]
name = "QA Agent"

[agents.qa.flow]
scope = "per-sender"
reset_mode = "idle"

[agents.qa.limits]
max_tokens_per_flow = 500_000

[agents.backend]
engine = "openrouter"
model = "anthropic/claude-sonnet-4"
role = "backend_engineer"
skills = ["read_file", "write_file", "search"]

[agents.backend.identity]
name = "Backend Engineer"

[agents.backend.flow]
scope = "per-sender"
reset_mode = "idle"

[agents.backend.limits]
max_tokens_per_flow = 500_000

[agents.integrator]
engine = "openrouter"
model = "openai/gpt-4o"
role = "integration_master"

[agents.integrator.identity]
name = "Integration Master"

[agents.integrator.flow]
scope = "per-sender"
reset_mode = "idle"

[agents.integrator.limits]
max_tokens_per_flow = 500_000
```

### 2. Start the Orchestrator

```bash
cargo run -- orchestrate
```

The orchestrator:
1. Reads all agents with `role` set from config
2. For each agent with a workspace: loads skills (classic + API frontmatter) and builds a system prompt with skill context
3. Registers agents in the fleet registry with their prompt, tools, and role
4. Starts the heartbeat loop
5. Waits for task assignments

### 3. Verify with Doctor

```bash
cargo run -- doctor
```

Confirms all configured agents can reach their backend endpoints.

## Agent Roles

Each agent has a role that determines its system prompt focus and task affinity.

| Role | Config Value | System Prompt Focus | Best For |
|------|-------------|-------------------|----------|
| QA | `qa` | Testing, validation, edge cases, correctness verification | Running tests, reviewing for bugs, checking edge cases |
| Backend Engineer | `backend_engineer` | Implementation, architecture, code quality, performance | Writing code, fixing bugs, refactoring, optimization |
| Integration Master | `integration_master` | Connecting systems, APIs, data flow, end-to-end coherence | API integration, cross-service issues, data pipelines |

### Role System Prompts

Each role injects a focused fragment into the agent's system prompt:

- **QA**: Emphasizes testing methodology, edge case analysis, correctness verification
- **Backend Engineer**: Emphasizes clean architecture, code quality, performance optimization
- **Integration Master**: Emphasizes system connectivity, data flow coherence, API design

### Mixing Models Per Role

Use different models for different roles — all on one OpenRouter bill:

```toml
[agents.qa]
engine = "openrouter"
model = "anthropic/claude-sonnet-4"    # Claude for careful analysis
role = "qa"

[agents.backend]
engine = "openrouter"
model = "anthropic/claude-sonnet-4"    # Claude for code generation
role = "backend_engineer"

[agents.integrator]
engine = "openrouter"
model = "openai/gpt-4o"               # GPT for integration reasoning
role = "integration_master"
```

Or use direct providers:

```toml
[agents.qa]
engine = "anthropic"
model = "claude-sonnet-4-20250514"
role = "qa"

[agents.backend]
engine = "ollama"
model = "codellama:34b"
role = "backend_engineer"
```

## Task Lifecycle

Tasks flow through a state machine:

```
  ┌─────────┐     assign      ┌─────────────┐    complete    ┌───────────┐
  │ Pending │ ───────────────> │ InProgress  │ ─────────────> │ Completed │
  └─────────┘                  └──────┬──────┘                └───────────┘
                                      │
                                      │ fail
                                      ▼
                                ┌──────────┐
                                │  Failed  │
                                └─────┬────┘
                                      │
                           (if retries remain)
                                      │
                                      ▼
                                ┌─────────┐
                                │ Pending │  (re-queued)
                                └─────────┘
```

### States

| State | Meaning |
|-------|---------|
| **Pending** | Created, waiting for an available agent |
| **InProgress** | Assigned to an agent, work underway |
| **Completed** | Successfully finished |
| **Failed** | Execution error. May retry if `retry_count < max_retries` |

### Transitions

| From | To | Trigger | Event Published |
|------|----|---------|----------------|
| Pending | InProgress | `assign_task()` | `TaskAssigned` |
| InProgress | Completed | `complete_task()` | `TaskCompleted` |
| InProgress | Failed | task failure | `TaskFailed` |
| Failed | InProgress | `retry_failed_task()` (auto) | `TaskAssigned` |

Invalid transitions (e.g., Pending -> Completed, Completed -> InProgress) are rejected.

### Task Fields

Each task tracks:

| Field | Type | Description |
|-------|------|-------------|
| `id` | string | UUID, auto-generated |
| `description` | string | What the task is about |
| `assigned_agent` | string? | Agent currently working on it |
| `status` | enum | Pending / InProgress / Completed / Failed |
| `role` | AgentRole | Which role should handle this task |
| `result` | string? | Output or error message |
| `retry_count` | u32 | Number of failed attempts |
| `max_retries` | u32 | Retry limit (from `orchestrator.max_retries`) |
| `created_at` | u64 | Unix epoch milliseconds |
| `updated_at` | u64 | Last state change timestamp |

## Heartbeat and Stall Detection

The heartbeat loop runs continuously during `orchestrate`:

1. **Every `heartbeat_interval_s` seconds** (default: 30):
   - Publishes a `HeartbeatTick` event with a sequence number
   - Calls `heartbeat_check()` on the orchestrator
2. **`heartbeat_check()` scans for stalled tasks**:
   - Finds tasks stuck in `InProgress` state
   - Checks if the assigned agent is responsive
   - Failed tasks with remaining retries are re-queued
3. **Re-assignment**:
   - The orchestrator finds an idle agent with the matching role
   - Task transitions back to `InProgress` with the new agent
   - `TaskAssigned` event is published

### Configuration

```toml
[orchestrator]
enabled = true
heartbeat_interval_s = 30    # Check every 30 seconds (default)
max_retries = 3              # Retry failed tasks up to 3 times (default)
```

Lower `heartbeat_interval_s` for faster stall detection (minimum practical: ~10s).
Higher `max_retries` for flaky tasks (careful: retries consume tokens).

## Fleet Agent Registry

The fleet maintains an in-memory registry of all agents:

| Agent State | Meaning |
|------------|---------|
| **Idle** | Available for task assignment |
| **Busy** | Currently working on a task |
| **Failed** | Agent experienced an error (not currently assignable) |

When a task needs assignment:
1. Orchestrator looks for an idle agent whose role matches the task's required role
2. First idle match gets the task
3. Agent is marked as Busy
4. When task completes/fails, agent returns to Idle

## Domain Events

The fleet emits events through the in-process EventBus. Subscribe to them for logging, metrics, or custom automation.

### Fleet Events

| Event | Fields | When |
|-------|--------|------|
| `TaskAssigned` | task_id, agent_id, role | Task assigned to agent |
| `TaskCompleted` | task_id, agent_id | Task finished successfully |
| `TaskFailed` | task_id, agent_id, reason | Task execution error |
| `HeartbeatTick` | seq | Heartbeat timer fired |
| `AgentStatusReport` | agent_id, status, current_task_id | Agent status snapshot |

### Subscribing to Events

```rust
let event_bus = InProcessEventBus::default();
let mut stream = event_bus.subscribe();

tokio::spawn(async move {
    while let Some(event) = stream.next().await {
        match event.payload {
            DomainEventPayload::TaskCompleted { task_id, agent_id } => {
                println!("Task {} completed by {}", task_id, agent_id);
            }
            DomainEventPayload::TaskFailed { task_id, reason, .. } => {
                eprintln!("Task {} failed: {}", task_id, reason);
            }
            _ => {}
        }
    }
});
```

## Per-Agent Skill Filtering

Restrict tool access per agent role. This enforces separation of concerns:

```toml
# QA agent: only testing and search tools
[agents.qa]
role = "qa"
skills = ["search", "test_runner", "lint"]

# Backend agent: code manipulation tools
[agents.backend]
role = "backend_engineer"
skills = ["read_file", "write_file", "search", "build"]

# Integration agent: all tools (no restriction)
[agents.integrator]
role = "integration_master"
# No skills field = all skills available
```

See [Skills Guide](SKILLS.md) for how to define custom skills.

## Complete Example Config

```toml
runtime_profile = "auto"

[hub]
bind = "127.0.0.1"
port = 7070
auth_mode = "token"

[refiner]
mode = "rules"

[orchestrator]
enabled = true
heartbeat_interval_s = 30
max_retries = 3

# --- Fleet Agents ---

[agents.qa]
engine = "openrouter"
model = "anthropic/claude-sonnet-4"
role = "qa"
skills = ["search", "test_runner"]

[agents.qa.identity]
name = "QA Agent"

[agents.qa.flow]
scope = "per-sender"
reset_mode = "idle"
idle_timeout_minutes = 30

[agents.qa.limits]
max_tokens_per_flow = 500_000

[agents.backend]
engine = "openrouter"
model = "anthropic/claude-sonnet-4"
role = "backend_engineer"
skills = ["read_file", "write_file", "search"]

[agents.backend.identity]
name = "Backend Engineer"

[agents.backend.flow]
scope = "per-sender"
reset_mode = "idle"

[agents.backend.limits]
max_tokens_per_flow = 500_000

[agents.integrator]
engine = "openrouter"
model = "openai/gpt-4o"
role = "integration_master"

[agents.integrator.identity]
name = "Integration Master"

[agents.integrator.flow]
scope = "per-sender"
reset_mode = "idle"

[agents.integrator.limits]
max_tokens_per_flow = 500_000
```

```bash
export OPENROUTER_API_KEY=sk-or-...
cargo run -- orchestrate
```

## Troubleshooting

**"No idle agent found for role X"**
All agents with that role are busy. Wait for a task to complete, or add more agents with the same role.

**Tasks stuck in InProgress**
The heartbeat should catch stalled tasks. Check `heartbeat_interval_s` is reasonable (default: 30s). Check logs at `~/.tengu/logs/tengu.log`.

**Agent can't reach endpoint**
Run `cargo run -- doctor` to test connectivity for all configured agents.

**Task retries exhausted**
A task that fails `max_retries` times stays in Failed state. Check the failure reason in logs or events. Increase `max_retries` if the failures are transient.
