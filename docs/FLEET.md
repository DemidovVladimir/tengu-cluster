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
     │   Agent A      │ │   Agent B      │ │   Agent C      │
     │ (role: qa)     │ │ (role: backend)│ │ (role: devops) │
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

Roles are fully dynamic — any non-empty string works. Define agent behavior through `identity.instructions` and restrict tools with `allowed_tools`.

```toml
[orchestrator]
enabled = true
heartbeat_interval_s = 30
max_retries = 3

[agents.qa]
engine = "openrouter"
model = "anthropic/claude-sonnet-4"
role = "qa"
workspace = "~/my-project"
allowed_tools = ["read_file", "list_directory", "run_command"]

[agents.qa.identity]
name = "QA Agent"
instructions = "You review code, run tests, and verify correctness."

[agents.qa.flow]
scope = "per-sender"
reset_mode = "idle"

[agents.qa.limits]
max_tokens_per_flow = 100_000

[agents.backend]
engine = "openrouter"
model = "anthropic/claude-sonnet-4"
role = "backend_engineer"
workspace = "~/my-project"
allowed_tools = ["read_file", "list_directory", "write_file", "run_command"]

[agents.backend.identity]
name = "Backend Engineer"
instructions = "You write server code, design APIs, and manage databases."

[agents.backend.flow]
scope = "per-sender"
reset_mode = "idle"

[agents.backend.limits]
max_tokens_per_flow = 100_000
```

### 2. Start the Orchestrator

```bash
cargo run -- orchestrate

# Or with a sandbox config:
cargo run -- orchestrate --sandbox webstudio
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

Roles are dynamic strings — any non-empty value works. The orchestrator routes tasks by matching the role in user input to agents with that role. Define what each role does through `identity.instructions`.

### Tool Restrictions

Use `allowed_tools` to restrict which workspace tools an agent can access:

| Tool | Risk Level | Description |
|------|-----------|-------------|
| `read_file` | Low | Read file contents |
| `list_directory` | Low | List files and directories |
| `write_file` | Medium | Write/create files (requires approval) |
| `run_command` | High | Execute shell commands (requires approval) |

```toml
# Read-only advisor
allowed_tools = ["read_file", "list_directory"]

# Full access (or omit allowed_tools entirely)
allowed_tools = ["read_file", "list_directory", "write_file", "run_command"]
```

### Mixing Models Per Role

Use different models for different roles — all on one OpenRouter bill:

```toml
[agents.reviewer]
engine = "openrouter"
model = "anthropic/claude-sonnet-4"    # Claude for careful analysis
role = "code_reviewer"

[agents.coder]
engine = "openrouter"
model = "anthropic/claude-sonnet-4"    # Claude for code generation
role = "developer"

[agents.planner]
engine = "openrouter"
model = "openai/gpt-4o"               # GPT for planning
role = "project_manager"
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

## Per-Agent Restrictions

Enforce separation of concerns with two independent allowlists:

- **`allowed_tools`** — restricts workspace tools (read_file, write_file, etc.)
- **`skills`** — restricts frontmatter skills (from workspace `skills/`, or global `skills/` in CWD)

```toml
# Read-only advisor — no write or execute
[agents.reviewer]
role = "code_reviewer"
allowed_tools = ["read_file", "list_directory"]
skills = ["search", "lint"]

# Full-access developer
[agents.developer]
role = "developer"
# No allowed_tools = all workspace tools available
# No skills = all skills available
```

See [Skills Guide](SKILLS.md) for custom skills and [Sandboxes Guide](SANDBOXES.md) for domain-specific team setups.

## Telegram Team Orchestration (`/team`)

In Telegram mode, use `/team <goal>` to decompose a goal into tasks with dependency tracking and parallel execution:

```
/team Build a REST API with auth, write tests, and deploy docs
```

The orchestrator:
1. Analyzes the goal and available agents
2. Creates tasks with unique IDs, assigns each to an agent role
3. Resolves dependencies — independent tasks are grouped into parallel batches
4. Executes batches: all tasks in a batch run (dependent tasks wait for prerequisites)
5. Agents communicate via outcome files in `.tengu-tasks/` — each agent writes its results, dependent agents read them

Example plan output:
```
Plan (3 tasks, 2 batches):
Batch 1 [parallel]:
  - [backend_engineer] Implement REST API with authentication
  - [qa] Write integration tests for auth endpoints
Batch 2:
  - [tech_writer] Document the API (after: implement_api, write_tests)
```

Use `/stop` to cancel mid-execution. Agents can also send multiple files (PDF + images) with the `/team` message — they're saved to `.tengu-attachments/` and included in the goal context.

## Sandboxes

Sandboxes let you define domain-specific multi-agent teams in isolated config files.

```bash
# Create a sandbox
mkdir -p sandboxes/webstudio
# Edit sandboxes/webstudio/config.toml with your agents...

# Run with sandbox
cargo run -- orchestrate --sandbox webstudio
cargo run -- telegram --sandbox webstudio
```

See [Sandboxes Guide](SANDBOXES.md) for full details and examples.

## Complete Example Config

```toml
runtime_profile = "auto"

[hub]
bind = "127.0.0.1"
port = 7070

[refiner]
mode = "off"

[orchestrator]
enabled = true
heartbeat_interval_s = 30
max_retries = 2

[agents.qa]
engine = "openrouter"
model = "anthropic/claude-sonnet-4"
role = "qa"
workspace = "~/my-project"
allowed_tools = ["read_file", "list_directory", "run_command"]

[agents.qa.identity]
name = "QA Agent"
instructions = "You review code, run tests, and verify correctness."

[agents.qa.flow]
scope = "per-sender"
reset_mode = "idle"

[agents.qa.limits]
max_tokens_per_flow = 100_000

[agents.backend]
engine = "openrouter"
model = "anthropic/claude-sonnet-4"
role = "backend_engineer"
workspace = "~/my-project"
allowed_tools = ["read_file", "list_directory", "write_file", "run_command"]

[agents.backend.identity]
name = "Backend Engineer"
instructions = "You write server code, design APIs, and manage databases."

[agents.backend.flow]
scope = "per-sender"
reset_mode = "idle"

[agents.backend.limits]
max_tokens_per_flow = 100_000
```

```bash
# Store your key in the encrypted vault (or export OPENROUTER_API_KEY)
cargo run -- secret set OPENROUTER_API_KEY sk-or-...
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
