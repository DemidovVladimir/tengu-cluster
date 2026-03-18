---
tags:
  - core
  - orchestrator
  - multi-agent
  - parallel-execution
aliases:
  - Fleet Orchestration
  - Task Planner
  - Team Orchestration
---

# Orchestrator

Run multiple AI [[Agents]] as a coordinated fleet. Each agent has a role, tasks are assigned and tracked, and independent tasks execute in parallel via JoinSet batches.

## Overview

```
                        ┌─────────────────┐
                        │  Orchestrator   │
                        │  (task planner  │
                        │   + JoinSet)    │
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
- Registers [[Agents]] with roles from [[Configuration|config]]
- Matches incoming tasks to agents by role
- Decomposes goals into tasks with dependency tracking
- Executes independent tasks in parallel via `tokio::task::JoinSet`
- Sequential batches for dependent tasks
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
3. Wraps each agent in `Arc<AgentRuntime>` for parallel task sharing
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

[[Skills|Skill]]-defined [[Tools]] require both a loaded `skill_packages` entry and a matching capability such as `skill.search` or `skill.privy`.

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

| From | To | Trigger |
|------|----|---------|
| Pending | InProgress | `assign_task()` |
| InProgress | Completed | `complete_task()` |
| InProgress | Failed | task failure |
| Failed | InProgress | `retry_failed_task()` (auto) |

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

## Parallel Execution

The orchestrator uses `tokio::task::JoinSet` for parallel batch execution:

1. The task planner decomposes a goal into tasks with `depends_on` fields
2. `resolve_execution_order()` groups tasks into batches — tasks in the same batch have no mutual dependencies
3. Each batch spawns tasks concurrently via `JoinSet::spawn`
4. Results are collected before the next batch starts

```
Batch 1 (parallel): [research, design]  <- no dependencies, run concurrently
Batch 2 (parallel): [implement, test]   <- depend on batch 1, run after it
Batch 3:            [deploy]            <- depends on batch 2
```

Each agent runtime is wrapped in `Arc<AgentRuntime>` and cloned per-task. Engine trait is `Send + Sync`, `ToolUseService` is `Clone` — no shared mutable state needed.

### Inline Data Passing

Dependent tasks receive prior step output **embedded directly in their prompt** — not file paths. This eliminates inter-agent hallucination (see [[Memory]] Layer 1: Handoff Context for full details).

- Output truncated to 3000 chars per dependency (char-boundary-safe)
- Shared `truncate_output()` helper in `channel_runtime.rs`
- Outcome files still written to `.tengu-tasks/` for audit, but prompts no longer depend on agents reading them

### [[Memory]] Integration

After all batches complete, the orchestrator:

1. **Auto-summarizes** results into a `topic_overview` [[Memory]] entry with metadata tags (`kind`, `source=orchestrator`, `goal`, `workspace_id`)
2. **Before planning** new goals, recalls prior `topic_overview` entries via filtered RAG and injects them as planner context

This gives the system **continuity across sessions** — it learns from past runs.

### [[Configuration]]

```toml
[orchestrator]
enabled = true
max_retries = 3              # Retry failed tasks up to 3 times (default)
```

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

`filter_tools_by_allowlist()` in `src/application/workspace_tools_catalog.rs` enforces the capability filter at runtime across all adapters.

## Telegram Team Orchestration

In Telegram multi-agent mode, plain messages are decomposed into tasks with dependency tracking and parallel execution automatically. Use `@role: message` to bypass the planner and talk to one agent directly. `/team <goal>` remains available when you want to make the team-planning step explicit.

Example explicit planning command:

```
/team Build a REST API with auth, write tests, and deploy docs
```

The orchestrator:
1. Recalls relevant prior topic overviews from [[Memory]] (filtered to `kind=topic_overview, source=orchestrator`) and injects them into the planner context
2. Analyzes the goal and available [[Agents]]
3. Creates tasks with unique IDs, assigns each to an agent role
4. Resolves dependencies — independent tasks are grouped into parallel batches
5. Executes batches: all tasks in a batch run concurrently (dependent tasks wait for prerequisites)
6. Dependent tasks receive prior step output embedded inline in their prompt (up to 3000 chars per dependency, char-boundary-safe truncation) — no file-path indirection, eliminating inter-agent hallucination
7. After all batches complete, auto-summarizes results into a `topic_overview` [[Memory]] entry with metadata tags (`kind`, `source`, `goal`, `workspace_id`)
8. Outcome files are still written to `.tengu-tasks/` for audit, but prompts no longer depend on agents reading them

Example plan output:
```
Plan (3 tasks, 2 batches):
Batch 1 [parallel]:
  - [backend_engineer] Implement REST API with authentication
  - [qa] Write integration tests for auth endpoints
Batch 2:
  - [tech_writer] Document the API (after: implement_api, write_tests)
```

Use `/stop` to cancel mid-execution. [[Agents]] can also send multiple files (PDF + images) with the `/team` message — they are saved to `.tengu-attachments/` and included in the goal context.

### Routing Modes

| Mode | Trigger | Behavior |
|------|---------|----------|
| **Single agent** | Default config | Direct agent handling |
| **Explicit routing** | `@role: message` | Routes to specific agent, bypasses planner |
| **Team orchestration** | Plain message in multi-agent mode | Full decomposition + parallel execution |
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
| `src/adapters/orchestrator.rs` | CLI fleet orchestrator with JoinSet parallel batch execution |
| `src/adapters/telegram_runtime.rs` | Telegram orchestrator with inline data passing and auto-summarize |
| `src/application/task_planner.rs` | LLM-based goal decomposition into tasks with `depends_on` dependencies |
| `src/adapters/channel_runtime.rs` | Shared logic: tool/executor/prompt rebuild, [[Memory]] init, agent routing, message chunking, `truncate_output()` |

`resolve_execution_order()` returns `Vec<Vec<usize>>` — batches of task indices for parallel execution. `ToolExecutor` trait has `Send + Sync` bounds for spawning across tokio tasks. `ToolResultObserver` type alias requires `Send + Sync`.

## Troubleshooting

**"No idle agent found for role X"**
All [[Agents]] with that role are busy. Wait for a task to complete, or add more agents with the same role.

**Tasks stuck in InProgress**
Check logs at `~/.tengu/logs/tengu.log`. Engine stream timeouts (120s default) will surface stalled tasks.

**Agent can't reach endpoint**
Run `cargo run -- doctor` to test connectivity for all configured [[Agents]].

**Task retries exhausted**
A task that fails `max_retries` times stays in Failed state. Check the failure reason in logs or events. Increase `max_retries` if the failures are transient.

## Related

- [[Agents]] — execution units routed by role
- [[Tools]] — workspace primitives and skill-defined tools
- [[Skills]] — knowledge/instructions loaded per agent
- [[Capabilities]] — hard runtime permissions
- [[Channels]] — how goals arrive (TUI, Telegram, CLI)
- [[Configuration]] — orchestrator and agent config
- [[Sandboxes]] — domain-specific team configs
- [[Memory]] — auto-summarize, RAG planner recall, handoff context
