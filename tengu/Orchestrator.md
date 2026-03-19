---
tags:
  - core
  - orchestrator
  - multi-agent
  - event-bus
aliases:
  - Fleet Orchestration
  - Task Planner
  - Team Orchestration
---

# Orchestrator

Run multiple AI [[Agents]] as a coordinated fleet. Each agent has a role, tasks are assigned and tracked, and parallelism emerges from the dependency graph via a reactive event-bus architecture.

## Overview

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
                            ▼
                    ┌─────────────────┐
                    │  Orchestrator   │
                    │  Rx (mpsc)      │
                    └─────────────────┘
```

The orchestrator:
- Registers [[Agents]] with roles from [[Configuration|config]]
- Matches incoming tasks to agents by role
- Decomposes goals into tasks with dependency tracking
- Each agent runs as a persistent worker with a dedicated mpsc channel (star topology)
- Tasks are dispatched the instant their dependencies are satisfied — no batch boundaries
- Selective context routing: structured artifacts + short text verbatim, long text via LLM summarization (Tier 2)
- Dynamic re-planning: agents can request plan modifications at runtime (with cycle detection + guards)
- Cascade-skip: failed tasks automatically skip all transitive dependents
- Aggregate token budget (`max_tokens_per_run`) enforced across all agents
- Recalls prior topic overviews from [[Memory]] before planning
- Auto-summarizes results into [[Memory]] after each run

## Quick Setup

### 1. Configure [[Agents]] with Roles

Roles are fully dynamic — any non-empty string works. Define agent behavior through `identity.instructions` and restrict [[Tools]] with `capabilities`.

```toml
[orchestrator]
enabled = true
max_retries = 3
planner_engine = "openrouter"                    # Optional: dedicated engine for planning
planner_model = "google/gemini-2.5-flash"        # Optional: cheaper model for plan generation

[agents.qa]
engine = "openrouter"
model = "nvidia/nemotron-3-super-120b-a12b:free"
role = "qa"
workspace = "~/my-project"
capabilities = ["workspace.read", "workspace.list", "workspace.shell"]

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
model = "nvidia/nemotron-3-super-120b-a12b:free"
role = "backend_engineer"
workspace = "~/my-project"
capabilities = ["workspace.read", "workspace.list", "workspace.write", "workspace.shell"]

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
1. Reads all [[Agents]] with `role` set from [[Configuration|config]]
2. For each agent with a workspace: loads [[Skills]] and builds a system prompt with skill context
3. Wraps each agent in `Arc<AgentRuntime>` for sharing across workers
4. Enters the interactive dispatch loop

### 3. Verify with Doctor

```bash
cargo run -- doctor
```

Confirms all configured [[Agents]] can reach their backend endpoints.

## Agent Roles

Roles are dynamic strings — any non-empty value works. The orchestrator routes tasks by matching the role in user input to [[Agents]] with that role. Define what each role does through `identity.instructions`.

### Tool Restrictions

Use [[Capabilities]] to restrict what an agent can actually execute. Typical workspace capabilities are:

| Capability | Effect | Description |
|-----------|--------|-------------|
| `workspace.read` | read | Read file contents |
| `workspace.list` | read | List files and directories |
| `workspace.write` | write | Write/create files (requires approval) |
| `workspace.shell` | shell_exec | Execute shell commands (requires approval) |
| `memory.remember` | read | Store long-term [[Memory]] entries |

[[Skills]] are controlled separately via `skill_packages` — they don't require capabilities.

```toml
# Read-only advisor
capabilities = ["workspace.read", "workspace.list"]

# Shell-enabled agent
capabilities = ["workspace.read", "workspace.list", "workspace.write", "workspace.shell"]
skill_packages = ["search"]
```

### Mixing Models Per Role

Use different models for different roles — all on one OpenRouter bill:

```toml
[agents.reviewer]
engine = "openrouter"
model = "nvidia/nemotron-3-super-120b-a12b:free"    # Nemotron 120B (free)
role = "code_reviewer"

[agents.coder]
engine = "openrouter"
model = "google/gemini-2.5-flash"                    # Gemini Flash (fast)
role = "developer"

[agents.planner]
engine = "openrouter"
model = "openai/gpt-4o"                              # GPT-4o (premium)
role = "project_manager"
```

## Task Lifecycle

Tasks flow through a LivePlan state machine:

```
Pending ──dispatch_ready_tasks()──> Ready ──dispatch_task()──> Running
                                                                  │
                                              TaskCompletion <────┤
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

### States

| State | Meaning |
|-------|---------|
| **Pending** | Created, waiting for dependencies to be satisfied |
| **Ready** | All dependencies met, eligible for dispatch |
| **Running** | Assigned to an agent worker, execution in progress |
| **Completed** | Successfully finished |
| **Failed** | Permanent failure (retries exhausted or non-retryable error) |
| **Skipped** | Cascade-skipped because a dependency failed |

### LiveTask Fields

Each task tracks:

| Field | Type | Description |
|-------|------|-------------|
| `id` | TaskId (String) | Unique identifier |
| `role` | String | Which role should handle this task |
| `description` | String | What the task is about |
| `depends_on` | Vec\<TaskId\> | Tasks that must complete before this one |
| `status` | LiveTaskStatus | Pending / Ready / Running / Completed / Failed / Skipped |
| `output` | Option\<String\> | Task output (populated on completion) |
| `artifacts` | HashMap | Structured artifacts extracted from tool envelopes |
| `assigned_agent` | Option\<AgentId\> | Agent currently working on it |
| `started_at` | Option\<Instant\> | Wall-clock start time (for timeout detection) |
| `attempt` | u32 | Number of execution attempts |

## Event-Bus Execution

The orchestrator uses a reactive event-bus architecture. Each agent runs as a persistent worker with a dedicated mpsc channel. The orchestrator dispatches tasks as dependencies are satisfied and processes results in a single `tokio::select!` event loop.

### How It Works

1. The task planner decomposes a goal into tasks with `depends_on` fields
2. `LivePlan::dispatch_ready_tasks()` marks tasks with satisfied dependencies as Ready
3. `dispatch_task()` transitions Ready tasks to Running and sends a `TaskAssignment` to the agent's channel
4. The agent worker executes the task and sends back a `TaskCompletion` or `TaskError`
5. The orchestrator updates the LivePlan and dispatches newly-unblocked tasks
6. This continues until all tasks reach a terminal state (Completed, Failed, or Skipped)

Parallelism emerges from the dependency graph — if tasks B and C both depend only on A, they are both dispatched the instant A completes.

### Context Routing (Two-Tier)

When dispatching a downstream task, the orchestrator builds selective context from completed dependencies:

| Condition | Tier | Action |
|-----------|------|--------|
| Upstream produced structured artifacts | Tier 1 (free) | Include artifacts in context |
| Upstream text output < 500 chars | Tier 1 (free) | Include full text verbatim |
| Upstream text output >= 500 chars | Tier 2 (LLM) | Planner LLM extracts relevant subset |

### Error Handling

- **Retryable errors** (timeout, rate limit, HTTP 429/502/503): task resets to Pending with `attempt++`, re-dispatched automatically
- **Permanent errors**: task marked Failed, all transitive dependents cascade-skipped
- **Timeout detection**: `tokio::select!` with a sleep branch that fires at the earliest running task's deadline

### Dynamic Re-Planning

Agents can send `PlanModificationRequest` events to add, remove, or update task dependencies at runtime. Guards prevent abuse:
- **Max modifications cap** (default: 10 per run)
- **No self-referential adds** (agent cannot add task for itself)
- **Cycle detection** (DAG validation before committing)

### Token Budget

Aggregate `max_tokens_per_run` (default: 2M) enforced across all agents. When exceeded, remaining pending/ready tasks are skipped.

### [[Memory]] Integration

After all tasks complete, the orchestrator:

1. **Auto-summarizes** results into a `topic_overview` [[Memory]] entry with metadata tags (`kind`, `source=orchestrator`, `goal`, `workspace_id`)
2. **Before planning** new goals, recalls prior `topic_overview` entries via filtered RAG and injects them as planner context

This gives the system **continuity across sessions** — it learns from past runs.

### [[Configuration]]

```toml
[orchestrator]
enabled = true
max_retries = 3              # Retry failed tasks up to 3 times (default)
```

## Fan-Out Ensemble Execution

For model benchmarking and validation, the same task can be dispatched to N agents in parallel via `run_fan_out()`. Results are collected into an `EnsembleReport` with per-agent duration, token usage, and status.

Use cases: model benchmarking, prompt regression testing, consensus verification, cost/latency profiling.

## Per-Agent Restrictions

Enforce separation of concerns with [[Capabilities]] and [[Skills|skill_packages]]:

- **`capabilities`** — hard runtime permissions controlling workspace primitives and subsystem access
- **`skill_packages`** — [[Skills|skill/workflow packages]] loaded into the agent prompt and [[Tools|tool]] registry

```toml
# Read-only advisor — no write or execute
[agents.reviewer]
role = "code_reviewer"
capabilities = ["workspace.read", "workspace.list"]
skill_packages = ["search", "lint"]

# Full-access developer
[agents.developer]
role = "developer"
capabilities = ["workspace.read", "workspace.list", "workspace.write", "workspace.shell"]
# No skill_packages = no extra skills loaded
```

`filter_tools_by_capability()` in `src/adapters/tool_builder.rs` enforces the capability filter at runtime across all adapters.

## Telegram Team Orchestration

In Telegram multi-agent mode, plain messages are decomposed into tasks with dependency tracking and reactive execution automatically. Use `@role: message` to bypass the planner and talk to one agent directly. `/team <goal>` remains available when you want to make the team-planning step explicit.

Example explicit planning command:

```
/team Build a REST API with auth, write tests, and deploy docs
```

The orchestrator:
1. Recalls relevant prior topic overviews from [[Memory]] (filtered to `kind=topic_overview, source=orchestrator`) and injects them into the planner context
2. Analyzes the goal and available [[Agents]]
3. Creates tasks with unique IDs, assigns each to an agent role
4. Converts PlanTasks into a LivePlan and wires up the EventBus
5. Spawns agent workers (with typing indicators and tool observers for Telegram)
6. Runs the event-bus orchestrator: tasks dispatch as dependencies are satisfied
7. Dependent tasks receive selective upstream context (artifacts + short text or LLM-summarized long text)
8. After completion, auto-summarizes results into a `topic_overview` [[Memory]] entry

Example plan output:
```
Plan (3 tasks, DAG dispatch):
  - [backend_engineer] Implement REST API with authentication
  - [qa] Write integration tests for auth endpoints (after: implement_api)
  - [tech_writer] Document the API (after: implement_api, write_tests)
```

Use `/stop` to cancel mid-execution (broadcasts Shutdown to all workers). [[Agents]] can also send multiple files (PDF + images) with the `/team` message — they are saved to `.tengu-attachments/` and included in the goal context.

### Routing Modes

| Mode | Trigger | Behavior |
|------|---------|----------|
| **Single agent** | Default config | Direct agent handling |
| **Explicit routing** | `@role: message` | Routes to specific agent, bypasses planner |
| **Team orchestration** | Plain message in multi-agent mode | Full decomposition + event-bus execution |
| **`/team <goal>`** | Explicit command | Forces team orchestration |

`parse_agent_routing()` parses the `@role: message` syntax and falls back to default/last-used agent. Per-user-per-agent conversation states are keyed by `"sender_id:agent_id"`.

## [[Sandboxes]]

[[Sandboxes]] let you define domain-specific multi-agent teams in isolated config files.

```bash
# Create a sandbox
mkdir -p sandboxes/webstudio
# Edit sandboxes/webstudio/config.toml with your agents...

# Run with sandbox
cargo run -- orchestrate --sandbox webstudio
cargo run -- telegram --sandbox webstudio
```

Sandbox [[Configuration|configs]] live in `sandboxes/<name>/config.toml`. Each sandbox defines its own set of [[Agents]] with roles, workspaces, [[Capabilities]], and [[Skills]].

See the Sandboxes guide for full details and examples.

## Complete Example [[Configuration|Config]]

```toml
runtime_profile = "auto"

[hub]
bind = "127.0.0.1"
port = 7070

[refiner]
mode = "off"

[orchestrator]
enabled = true
max_retries = 2

[agents.qa]
engine = "openrouter"
model = "nvidia/nemotron-3-super-120b-a12b:free"
role = "qa"
workspace = "~/my-project"
capabilities = ["workspace.read", "workspace.list", "workspace.shell"]

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
model = "nvidia/nemotron-3-super-120b-a12b:free"
role = "backend_engineer"
workspace = "~/my-project"
capabilities = ["workspace.read", "workspace.list", "workspace.write", "workspace.shell"]

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

## Implementation

| File | Purpose |
|------|---------|
| `src/adapters/types.rs` | Event types (`OrchestratorEvent`, `PlanModification`, `TokenUsage`), `LivePlan` state machine, `EventBus` channels |
| `src/adapters/agent_builder.rs` | Agent worker loop: listens on inbox, executes tasks, sends results, artifact extraction |
| `src/adapters/event_orchestrator.rs` | Event-bus orchestrator core: event loop, dispatch, error handling, data routing, RunBudget |
| `src/adapters/orchestrator.rs` | CLI adapter: plan generation, EventBus wiring, agent worker spawning, fan-out ensemble |
| `src/adapters/task_builder.rs` | LLM-based goal decomposition into tasks with `depends_on` dependencies |
| `src/adapters/telegram_builder.rs` | Telegram adapter: `TelegramTaskExecutor`, EventBus wiring, message rendering |
| `src/adapters/channel_runtime.rs` | Shared logic: tool/executor/prompt rebuild, [[Memory]] init, agent routing |

## Troubleshooting

**"No agent for role X"**
No [[Agents]] with that role are configured. Check your config file.

**Tasks stuck in Running**
Check logs at `~/.tengu/logs/tengu.log`. The orchestrator has timeout detection that will cancel stuck tasks. Engine stream timeouts (120s default) will also surface stalled tasks.

**Agent can't reach endpoint**
Run `cargo run -- doctor` to test connectivity for all configured [[Agents]].

**Task retries exhausted**
A task that fails `max_retries` times stays in Failed state and cascade-skips dependents. Check the failure reason in logs. Increase `max_retries` if the failures are transient.

## Related

- [[Agents]] — execution units routed by role
- [[Tools]] — workspace primitives and skill-defined tools
- [[Skills]] — knowledge/instructions loaded per agent
- [[Capabilities]] — hard runtime permissions
- [[Channels]] — how goals arrive (TUI, Telegram, CLI)
- [[Configuration]] — orchestrator and agent config
- [[Sandboxes]] — domain-specific team configs
- [[Memory]] — auto-summarize, RAG planner recall, handoff context
