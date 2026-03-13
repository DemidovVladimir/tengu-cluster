# Multi-Agent Transparent Routing — Research Brief

## Current State (hexagonal-arch branch)

### What Codex 5.4 Already Implemented

The routing change is **already in place** at `telegram_runtime.rs:1398`:

```rust
if routed_role.is_none() && is_multi_agent {
    orchestrate_team_goal(...).await;
    continue;
}
```

- `is_multi_agent = agent_states.len() > 1` (line 921)
- Plain messages in multi-agent mode → `orchestrate_team_goal()` (plan + execute)
- `@role: message` → direct routing to specified agent (bypasses planning)
- `/team <goal>` → same `orchestrate_team_goal()` path (now redundant with plain messages)

### How orchestrate_team_goal() Works (lines 278-620)

1. Save attachments to `.tengu-attachments/`
2. Send "Planning…" to user
3. Call `task_planner::generate_plan()` — LLM decomposes goal into `PlanTask[]` with `{id, role, task, depends_on}`
4. Call `resolve_execution_order()` — topological sort into `Vec<Vec<usize>>` parallel batches
5. Send plan text to user (shows roles, tasks, batches)
6. Execute batches sequentially; within each batch, tasks run one-by-one (sequential due to `&mut` borrow on `agent_states`)
7. Each task: hot-reload skills → build executor → run through `ChatRuntimeService::process_user_text()`
8. Outcomes written to `.tengu-outcomes/<task_id>.md`; dependent tasks get file paths
9. Send final "Team completed — X/Y tasks done"

### What Users See Now

Users see ALL of this:
- "Planning…"
- Plan text with role names and task descriptions
- "[Agent Name] task description" per task
- "[Agent Name] tool_name" per tool call
- Agent output per task
- "Team completed — X/Y tasks done"

### Problems

1. **Every plain message hits the planner** — even "hello" or "what's 2+2" triggers full decomposition. No fast path for simple single-agent tasks.
2. ~~**Agent names still visible**~~ — KEEP visible. User wants agent names shown for debugging.
3. **No return-to-supervisor** — after agents complete, there's no synthesis step. Raw per-agent outputs are sent directly.
4. **Sequential execution within batches** — the `&mut agent_states` borrow prevents true parallelism.
5. **`/team` is now redundant** — plain messages already use the same path.

---

## Industry Patterns (Research Summary)

### Pattern 1: Simple LLM Router (Anthropic, Swarm)
- One LLM classifies input → routes to one agent
- Zero decomposition overhead, minimal latency
- Best for: atomic tasks, well-separated domains, <10 agents

### Pattern 2: Supervisor with Agent-as-Tool (LangGraph)
- Supervisor has sub-agents registered as callable tools
- LLM's native function-calling selects which agent to invoke
- After agent completes, control returns to supervisor for next decision
- Best for: iterative multi-agent workflows, quality gating

### Pattern 3: Hierarchical Manager (CrewAI)
- Manager LLM receives full task list + agent capabilities
- Dynamically delegates, validates outputs, can re-delegate
- Synthesizes final unified response
- Best for: complex goals requiring quality control

### Pattern 4: LLM Speaker Selection (AutoGen)
- Group chat with LLM picking next speaker each turn
- Retries with corrective feedback on invalid selections
- Custom selector functions for deterministic overrides
- Best for: conversational multi-agent collaboration

### Pattern 5: Plan-and-Execute (Current Tengu)
- Planner decomposes goal into tasks with dependency graph
- Tasks assigned to agents by role, executed in parallel batches
- Outcomes passed as file-based context between dependent tasks
- Best for: compound goals with clear parallelism

### The Hybrid Pattern (Recommended)

The most robust approach combines routing + decomposition:

```
User message
    ↓
Supervisor LLM (invisible):
  → Simple request? Route to ONE agent directly (fast path)
  → Compound goal?  Plan → decompose → parallel execute (slow path)
    ↓
Single coherent response to user
```

**This is what Tengu should do.** The current implementation already has the slow path (plan-and-execute). What's missing is:
1. The fast path (route simple requests to one agent without planning)
2. A synthesis step (supervisor collects all outputs and produces one response)
3. True parallel execution within batches

---

## Concrete Implementation Plan

### Step 1: Add a Routing Classifier

Before calling `orchestrate_team_goal()`, add a lightweight LLM classification step:

```rust
// In telegram_runtime.rs, replace the current block at line 1398:
if routed_role.is_none() && is_multi_agent {
    // NEW: classify first
    let classification = classify_request(
        planner_engine, &user_text, &agent_descriptions
    ).await;

    match classification {
        RouteDecision::SingleAgent(role) => {
            // Fast path: route directly to one agent
            // Same as @role: routing but invisible to user
        }
        RouteDecision::MultiAgent => {
            // Slow path: full plan-and-execute
            orchestrate_team_goal(...).await;
        }
    }
    continue;
}
```

The classifier prompt would be:
```
Given the user message and available agents, decide:
1. If this is a simple request for ONE agent, return {"route": "single", "role": "agent_role"}
2. If this requires multiple agents, return {"route": "multi"}

Available agents:
{agent_descriptions}

User message: {message}
```

### Step 2: Add a Synthesis Step

After all tasks complete in `orchestrate_team_goal()`, add a final synthesis:

```rust
// After the batch loop, before sending "Team completed":
let synthesis = synthesize_results(
    planner_engine, &goal, &completed_outcomes
).await;
// Send synthesis as the user-facing response
// Hide individual agent outputs (or make them debug-only)
```

### Step 3: Keep Agent Visibility

- Keep plan text, per-task labels, and tool call notifications WITH agent names
- Agent names are useful for debugging and transparency
- The user should not need to know agents exist to USE the system, but seeing which agent does what is valuable

### Step 4: True Parallel Execution

The `&mut agent_states` borrow prevents parallelism. Options:
- Wrap each `TelegramAgentState` in `Arc<Mutex<>>`
- Clone the necessary state before spawning tasks
- Extract read-only state (engine, tools, prompt) into Arc-wrapped structs

---

## Key Files to Modify

| File | Change |
|------|--------|
| `src/adapters/telegram_runtime.rs` | Add classifier before orchestration, synthesis after, reduce noise |
| `src/application/task_planner.rs` | Add `classify_request()` function (lightweight single-shot LLM call) |
| `src/adapters/channel_runtime.rs` | No changes needed |
| `src/adapters/orchestrator.rs` | Same classifier + synthesis pattern for CLI |

---

## Framework Comparison Table

| Framework | Routing | Decomposition | Parallel | User Sees Agents | Synthesis |
|-----------|---------|---------------|----------|-------------------|-----------|
| OpenClaw | Config bindings (deterministic) | LLM-driven delegation | Via sessions | No | No (sub-agent results) |
| Swarm | LLM handoff functions | None | No | Yes (sender field) | No |
| CrewAI | Manager LLM | Yes (hierarchical) | No | No (in logs only) | Yes (manager) |
| LangGraph | Agent-as-tool | Via supervisor | Graph branching | Optional | Via supervisor |
| AutoGen | LLM speaker selection | No | No | Yes (in history) | No |
| Semantic Kernel | Strategy-based | No | No | Yes (author) | Optional (filter) |
| LlamaIndex | Control plane | Workflow-based | Yes | No (behind API) | Yes |
| Hermes | Single agent + delegate_task | delegate_task tool | Yes (batch) | No (single agent) | No |
| ZeroClaw | N/A (single agent) | N/A | N/A | N/A | N/A |
| NullClaw | N/A (NullBoiler handles) | DAG (NullBoiler) | Yes | No | Via NullBoiler |
| **Tengu (current)** | None (all→planner) | Yes (task_planner) | No (sequential) | Yes (all exposed) | No |
| **Tengu (target)** | LLM classifier | Yes (task_planner) | Yes (JoinSet) | No (hidden) | Yes (synthesis) |

---

## OpenClaw / ZeroClaw / NullClaw

### OpenClaw (TypeScript/Swift, 307k+ stars)
Creator: Peter Steinberger. Hub-and-spoke architecture with persistent Gateway daemon.

**Routing — Deterministic Bindings (NOT LLM-based):**
- Routes via `(channel, accountId, peer/guild)` → `agentId` mappings
- Most specific binding wins (exact peer > guild+roles > account > channel > default)
- Entirely config-driven, no LLM overhead for routing decisions
- User never needs to know agents exist — routing is transparent at infrastructure level

**Multi-Agent — Coordinator/Sub-Agent:**
- Coordinator has `sessions_spawn` + `sessions_send` tools (coordination only, no exec)
- Specialists have domain tools but NO `sessions_send` — prevents delegation loops
- Sub-agents run in own sessions, announce results back
- Sub-agents cannot spawn other sub-agents (prevents cascading)
- LLM reasoning decides WHEN to delegate, framework provides HOW

**Key insight:** Routing is deterministic (config), delegation is LLM-driven (tool calls). Clean separation.

### ZeroClaw (Rust, trait-driven)
- **Single-agent runtime** — no multi-agent support yet
- Issue #218 proposes `delegate` tool for sub-agent spawning (not implemented)
- Architecturally closest to Tengu (trait-driven Rust, hexagonal-ish)
- Optimized for edge devices (12 MB binary, 4 MB RAM)

### NullClaw (Zig, vtable-driven, 678 KB)
- **Single-agent runtime** with clean separation:
  - NullClaw = runtime (providers, tools, memory, channels)
  - NullBoiler = multi-step DAG orchestration (runs, steps, workers, retries)
  - NullTickets = task control plane
- Key insight: runtime ≠ orchestrator — separate concerns

---

## Hermes Agent (Nous Research)

Python-based, single-agent architecture with delegation capability.

**Architecture:** Provider-agnostic tool loop: `User → AIAgent._run_agent_loop() → LLM → tool_calls → execute → loop`

**Delegation:** `delegate_task` tool intercepted by `run_agent.py` before registry dispatch. Spawns child agents for subtasks with parallel execution. Implementation details sparse but follows the "agent-as-tool-caller" pattern — the LLM decides when to delegate, framework handles spawning.

**Skills = Markdown instructions** (SKILL.md files) — extremely similar to Tengu's skill system:
- YAML frontmatter + markdown body
- Instructions + shell commands + existing tools
- No code changes needed
- Platform filtering, tags, related skills
- Trust levels (builtin, official, trusted, community)

**Gateway:** Hub-spoke, platform adapters (Telegram, Discord, Slack, WhatsApp, Signal, Email) → per-chat session store → AIAgent. Single agent handles all platforms. Session reset policies: daily (4am), idle (120min), or both.

**Key insight:** Single-agent with delegation tool is simpler than multi-agent routing. The agent itself decides when to spawn sub-agents — no separate routing/classification step needed.

---

## Three Fundamental Routing Paradigms

Across ALL frameworks studied, routing falls into three paradigms:

### Paradigm A: Infrastructure Routing (OpenClaw, Hermes Gateway)
- Deterministic config maps channels/users → agents
- No LLM overhead for routing
- User never sees agent names
- Best when: different agents serve different contexts (channels, users, topics)

### Paradigm B: LLM Triage/Supervisor (LangGraph, Swarm, Agency Swarm, AutoGen)
- One front-facing agent classifies and routes via tool calls or handoffs
- LLM reasoning handles routing — flexible but adds latency
- User sees one conversation
- Best when: single channel, multiple domains, conversation-style interaction

### Paradigm C: Plan-and-Execute Orchestration (Tengu, OpenClaw sessions_spawn, NullBoiler, CrewAI hierarchical)
- Planner decomposes goal → tasks with dependencies
- Tasks assigned to specialist agents, executed in parallel batches
- Results collected and (optionally) synthesized
- Best when: compound goals requiring multiple specialists working together

### What Tengu Should Implement: B + C Hybrid

```
User message
    ↓
LLM Classifier (Paradigm B — one fast call):
  → Simple/single-domain? Route to ONE agent (fast path)
  → Compound/multi-domain? Fall through to Paradigm C
    ↓
Plan-and-Execute (Paradigm C — existing code):
  → generate_plan() → resolve_execution_order() → batch execute
    ↓
Synthesis step (NEW):
  → Collect all outputs → LLM produces one coherent response
    ↓
Single response to user (no agent names visible)
```
