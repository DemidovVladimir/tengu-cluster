# Harness-Owned Orchestration + Memory — Architecture

This document describes the architecture that landed across PRs #6, #7, #8. It replaces the heart/brain/sensors doctrine and all prior skill-based orchestration.

---

## 1. The one-sentence version

**The harness (Rust) owns control flow. Agents (LLMs) do narrow work. An orchestrator agent emits structured JSON plans that a Rust DAG executor dispatches, and memory is a harness-driven subsystem that no agent sees directly.**

---

## 2. Top-level picture

```
┌──────────────────────────────────────────────────────────────────────────┐
│                         User Channel (Telegram / TUI)                     │
│                                                                           │
│   user_msg ───┐                                                           │
│               ▼                                                           │
│       ┌─────────────┐                                                     │
│       │ Orchestrator│  (single Rust struct, per-session)                  │
│       │  .handle()  │                                                     │
│       └──────┬──────┘                                                     │
│              │                                                            │
│              │ ┌────────────────────────────────────────────────────┐    │
│              │ │  Orchestrator-agent LLM turn:                      │    │
│              ├─▶  planner.rs → OrchestratorAgentPlanner              │    │
│              │ │  inputs: user_msg + failure-context (on replan)    │    │
│              │ │  outputs: {kind:"direct"} | {kind:"plan", steps}   │    │
│              │ └────────────────────────────────────────────────────┘    │
│              │                                                            │
│              │ ┌────────────────────────────────────────────────────┐    │
│              │ │  DAG executor (executor.rs):                       │    │
│              ├─▶  ready-set scheduling via tokio::spawn              │    │
│              │ │  per-step: RetryPolicy (max_attempts_per_step)     │    │
│              │ │  emits OrchestratorEvent on a broadcast bus        │    │
│              │ └────────────────────────────────────────────────────┘    │
│              │                                                            │
│              │ ┌────────────────────────────────────────────────────┐    │
│              │ │  Replan driver (replan.rs):                        │    │
│              ├─▶  on StepExhausted → re-invoke planner              │    │
│              │ │  bounded by max_replans                            │    │
│              │ └────────────────────────────────────────────────────┘    │
│              │                                                            │
│              ▼                                                            │
│        final_response                                                     │
└──────────────────────────────────────────────────────────────────────────┘

Each step dispatch:

      step ──▶ ChatWorker.run_step(step, step_inputs)
                 │
                 │ 1. MemoryInjector.for_turn(agent, step.goal)
                 │    → fenced <memory-context> block prepended to user
                 │      message. NEVER persisted in message history.
                 │
                 │ 2. ChatServiceFactory.run_turn(agent, content)
                 │    → per-call ChatRuntimeService<'a> built by the
                 │      channel's closure with fresh ChatLoopState.
                 │      Actual LLM call + tool loop.
                 │
                 │ 3. MemoryWriter.sync_turn(agent, user, reply)
                 │    → spawned, non-blocking; detached tokio task.
                 │
                 ▼
              step output (String)
```

---

## 3. The two subsystems

### `src/adapters/orchestrator/`

| File | What it does |
|---|---|
| `mod.rs` | `Orchestrator` struct. Public API: `new`, `handle`, `subscribe`, `cancel`. |
| `config.rs` | `OrchestratorConfig` bridging to the runtime config. |
| `events.rs` | `OrchestratorEvent` enum + `tokio::sync::broadcast` bus. |
| `plan.rs` | `Step`, `StepId`, `Plan` types. Topology helpers: `ready_steps`, `validate` (cycles, single-leaf, known agents), `single_leaf`. |
| `planner.rs` | `Planner` trait + `OrchestratorAgentPlanner` impl. Wraps the orchestrator agent's LLM call. Parses verdicts (handles markdown fences). |
| `retry.rs` | `RetryPolicy` + `run_step_with_retry`. Exponential backoff, emits `StepFailed` / `StepExhausted`. |
| `executor.rs` | `DagExecutor` + `WorkerHandle` trait. Parallel ready-set scheduling via `tokio::spawn`. Observes cancel flag. |
| `replan.rs` | `drive()` — outer loop. On `StepExhausted`, re-invokes planner with failure context. Bounded by `max_replans`. |
| `wiring.rs` | `ChatServiceFactory` trait. `ChatWorker` + `ChatOrchestratorPortImpl` concrete impls that do memory injection + dispatch. |
| `roster.rs` | Agent-roster table rendering and `{{ roster }}` template substitution. |
| `telemetry.rs` | Event → `tracing` bridge (placeholder). |

### `src/adapters/memory/`

| File | What it does |
|---|---|
| `mod.rs` | Module root. |
| `provider.rs` | `MemoryProvider` trait (Hermes-shaped). Methods: `system_prompt_block`, `prefetch(agent, query)`, `sync_turn(agent, user, asst)`, `on_pre_compress`, `shutdown`. |
| `builtin.rs` | `BuiltinMemoryProvider` — always-registered. Loads AGENTS.md + MEMORY.md + identity files + daily logs into system prompt; vector-searches on prefetch. |
| `manager.rs` | `MemoryManager` — holds one builtin + at most one external. Exposes `prefetch_all`, `sync_all`, `ingest_one`, `search`, `set_vector_backend`. |
| `injector.rs` | `for_turn(mgr, agent, query) -> PinnedMemoryBlock`. Called pre-turn by the orchestrator + executor. |
| `writer.rs` | `sync_turn` — spawns a detached task for post-turn writes. Never blocks the user reply. |
| `fencing.rs` | `<memory-context>` block building + sanitization (nested fences stripped defensively). |
| `context_block.rs` | Shared types: `PinnedMemoryBlock`, `ChunkMetadata`, `MemoryHit`. |
| `vector.rs` | `VectorStore` trait + submodules. |
| `vector/disk.rs` | `DiskVectorStore` — bincode on disk. |
| `vector/qdrant.rs` | `QdrantVectorStore` — optional, behind `qdrant` feature. |
| `vector/embedder.rs` | `Embedder` — OpenRouter embeddings client, `text-embedding-3-small`. |

LLM-callable memory tools live in `src/adapters/plugins/memory/` and go through `MemoryManager`:

| Tool | File | Purpose |
|---|---|---|
| `memory_ingest` | `plugins/memory/ingest.rs` | Explicit document/fact ingestion (chunks + metadata). |
| `memory_search` | `plugins/memory/search.rs` | Targeted vector read mid-turn. |
| `persistent_store` | `plugins/memory/persistent_store.rs` | Deterministic KV scratchpad (SQLite-backed). |

Pre-turn retrieval and post-turn writes are ALSO automatic through `MemoryInjector` / `MemoryWriter` — the LLM-callable tools are a complement, not the only path.

---

## 4. Request lifecycle (sequence)

```
User types "Summarize the compliance section of the PDF from yesterday
and draft a reply to legal."
│
├── Channel (Telegram / TUI) receives message
│   → Orchestrator.handle(user_msg)
│
├── Orchestrator agent LLM turn  (planner.rs)
│   inputs:  system_prompt + user_msg + tool_defs=[memory_search]
│   LLM loop may call memory_search("compliance PDF yesterday")
│   final output JSON:
│     { "kind": "plan", "steps": [
│         {"id": "s1", "agent": "researcher",
│          "goal": "Extract compliance obligations from chunks <ids>",
│          "depends_on": []},
│         {"id": "s2", "agent": "writer",
│          "goal": "Draft reply to legal based on s1 output",
│          "depends_on": ["s1"]}
│     ]}
│   emit PlanCreated
│
├── DagExecutor.run(plan, worker, policy, events, cancel)
│   loop:
│     ready = plan.ready_steps(completed)     # topology
│     spawn each ready step not in-flight:
│       emit StepStarted
│       → ChatWorker.run_step(step, step_inputs_from_upstream)
│           → inject memory (fenced block via MemoryInjector)
│           → ChatServiceFactory.run_turn(agent, content)
│               → builds ChatRuntimeService<'a> per-call
│               → fresh ChatLoopState::default()
│               → process_user_text()  (tool loop, may call memory_search etc.)
│           → MemoryWriter.sync_turn spawned (fire-and-forget)
│       awaits worker + retry wrapper
│       retry policy: {1s, 3s, 9s} backoff, 3 attempts
│       on success:     emit StepSucceeded
│       on exhaustion:  emit StepExhausted → return NeedsReplan
│   if all done: emit PlanCompleted(leaf_step.output)
│
├── If NeedsReplan and replans_left > 0:
│   emit ReplanTriggered
│   re-invoke planner with failure context appended
│   → new Plan → back to executor
│
├── If exceeded max_replans: graceful failure text → PlanCompleted(cancelled:false)
│
└── Channel renders PlanCompleted.final_response to user
```

**Cancel (`/stop`):** Channel calls `Orchestrator.cancel()` → AtomicBool flips → executor checks between dispatches → `ExecResult::Cancelled` → `PlanCompleted{cancelled:true, final_response:"Stopped by user."}`.

---

## 5. Channel wiring (the `ChatServiceFactory` shared-snapshot pattern)

The problem: `ChatRuntimeService<'a>` is lifetime-parameterized with 14 borrowed fields and bound to one agent at construction. It can't live behind `Arc`, and the orchestrator needs to dispatch to any agent per step.

The solution in `channel_runtime.rs`:

```
OrchestratorSnapshots = Arc<RwLock<HashMap<agent, ChatTurnInputs>>>
                         │                         │
                         │                         ▼
                         │                  ChatTurnInputs: owned
                         │                  versions of everything a
                         │                  ChatRuntimeService borrows
                         │                  (engine Arc, tool_executor Arc,
                         │                   tools clone, system_prompt
                         │                   clone, memory_manager Arc,
                         │                   cancel flag, etc.)
                         │
                         └── Channel side: BEFORE each orch.handle(), hot-reload
                             every agent and write fresh per-agent snapshots.
                             Factory side: closure reads the map by agent name
                             at call time, constructs ChatRuntimeService<'a>
                             borrowing into the snapshot, calls process_user_text.
```

Helpers in `channel_runtime.rs`:

- `ChatInputsFn = Arc<dyn Fn(&str) -> Result<ChatTurnInputs> + Send + Sync>` — the closure shape.
- `RuntimeChatServiceFactory::new(inputs_fn)` — impl of the `ChatServiceFactory` trait.
- `snapshots_inputs_fn(snapshots: OrchestratorSnapshots) -> ChatInputsFn` — returns a closure that reads from the shared map.
- `build_orchestrator(config, factory, memory) -> Option<Orchestrator>` — builds the full stack.

**Telegram** (`telegram_builder.rs`): `execute_orchestrator_turn` is called when orchestrator is configured AND the user did NOT `@role:`-route explicitly. Writes snapshots for every agent, awaits `handle`, sends the reply chunked + secret-redacted.

**TUI** (`tui/mod.rs`): engine thread promotes `Box<dyn Engine>` → `Arc<dyn Engine>`. The `ChatRequest::UserMessage` branch writes snapshots, runs `rt.block_on(orch.handle(text))`, marks `tools_dirty = true` so the next direct-dispatch turn rebuilds its executor.

**Known gap:** `@role:` explicit routing bypasses the orchestrator and goes direct-dispatch. Multi-agent "Recent Team Activity" injection is preserved only on the direct path.

---

## 6. Doctrine (replaces heart / brain / sensors)

Three rules, checked against every PR:

1. **The harness owns control flow.** Orchestration, routing, memory retrieval, memory writes, retries, replans, cancellation, cache discipline, turn lifecycle — all of it is Rust code. No skill teaches these behaviors to an LLM, because no LLM is asked to decide them.

2. **Agents are narrow LLM workers.** An agent is an `AgentConfig` entry: a name, a system prompt, a model, a tool list, optional memory scope. Agents do not know about other agents. Agents do not decompose user requests. Agents do not spawn subagents. Each agent's conversation is a single stable system prompt + a user message + tool calls — the shape prompt caching demands.

3. **The orchestrator is "just an agent with one tool."** Not a Rust class with baked-in planning logic. An `AgentConfig` entry like any other worker, with exactly one tool (`memory_search`) and a system prompt that teaches it to emit structured JSON plans. What makes it the orchestrator is its position in the runtime: it runs first, its output drives the DAG executor, and workers never invoke it back.

**No-compromise corollary:** If the codebase tries to encode per-user routing preferences, per-workflow templates, or task-specific retry strategies — stop. Those are config or agent system-prompt concerns, not Rust code. The harness owns *mechanism*, not *policy intent*. Test: *does a non-engineer user need to change this behaviour by editing a config file (`tengu.toml`) or by filing a PR?* If "config file," it's config. If "PR," it's Rust code.

---

## 7. Configuration shape

`tengu.toml`:

```toml
[orchestrator]
agent = "orchestrator"           # must match an [agents.<name>] below
max_attempts_per_step = 3        # tier 1: step retry budget
max_replans = 2                  # tier 2: planner re-invocation budget

[agents.orchestrator]
engine = "openrouter"
model = "anthropic/claude-haiku-4-5"
workspace_tools = ["memory_search"]
[agents.orchestrator.identity]
instructions = """
You decompose user requests into plans of steps dispatched to specialist agents.
You emit JSON only. Available agents and their capabilities:

{{ roster }}

Output shape:
  {"kind": "direct", "response": "..."}
  {"kind": "plan", "steps": [{"id": "...", "agent": "...", "goal": "...", "depends_on": [...]}]}

If a plan fans out, always include a final synthesizer step that depends on the fan-out outputs.
Use memory_search before planning when the request references prior work.
"""

[agents.researcher]
engine = "openrouter"
model = "anthropic/claude-sonnet-4-6"
workspace_tools = ["http_request", "memory_search", "memory_ingest"]
[agents.researcher.identity]
instructions = "You are a focused researcher..."

[agents.writer]
engine = "openrouter"
model = "anthropic/claude-sonnet-4-6"
workspace_tools = ["memory_search"]
[agents.writer.identity]
instructions = "You draft polished responses..."
```

Presence of `[orchestrator]` activates orchestration. Absence → single-agent dispatch (the `AgentConfig { default = true }` entry handles the user message directly).

---

## 8. What disappeared

### Deleted files
- `src/adapters/agent_builder.rs` (~130 LOC)
- `src/adapters/event_orchestrator.rs` (~1,180 LOC)
- `src/adapters/task_builder.rs` (~440 LOC)
- `src/adapters/orchestrator.rs` (legacy single file, ~670 LOC — the directory `orchestrator/` replaced it)
- `src/adapters/memory_builder.rs` (~290 LOC)
- `src/adapters/qdrant_memory_store.rs` (~230 LOC)
- `src/adapters/embedding.rs` (~80 LOC)
- `src/adapters/plugins/subagents/` (entire dir: `mod.rs`, `spawn.rs`, `fan_out.rs`, `manage.rs`)
- `src/adapters/plugins/memory/remember.rs` (renamed to `ingest.rs`)

### Deleted types
- `MemoryService`, `MemoryServiceHandle`, `EmbeddingPort`, `MemoryStorePort`, `MemoryEntry`, `MemorySearchResult`
- `DiskVectorMemoryStore`, `QdrantMemoryStore`, `OpenRouterEmbeddingAdapter`
- `SubagentRegistry`, `SubagentHandle` + all session-spawn LLM-callable tools

### Deleted doctrine
- `docs/superpowers/specs/2026-04-15-phase-0-doctrine-design.md`
- `docs/superpowers/specs/2026-04-16-phase-0-implementation-design.md`
- `docs/superpowers/plans/2026-04-16-phase-0-doctrine.md`
- "heart / brain / sensors" language in `docs/architecture.md`

### Renamed / refactored
- `remember` tool → `memory_ingest` (extended with `chunks`, `metadata`)
- `OrchestratorConfig` fields: dropped `enabled` / `max_concurrent` / `planner_engine` / `planner_model`; added `agent` / `max_attempts_per_step` / `max_replans`
- `plugins/memory/mod.rs` — three tools, all routing through `MemoryManager`

---

## 9. Known limitations (live on main)

1. **Batch embedding gone.** Old port took `&[&str]`; new `Embedder::embed` is single-text. N HTTP calls instead of 1 for multi-text ingestion. Follow-up: add `Embedder::embed_batch`.

2. **`persistent_store` delete is manifest-only.** `VectorStore` trait has no `delete(id)`. Manifest entries are removed but vector entries linger in `vectors.bin` until rebuild.

3. **`/purge` no longer clears memory.** Prints pointer to `<workspace>/memory/vectors.bin` — no programmatic clear.

4. **TUI status line drops `entry_count` / `storage_bytes`.** Not exposed on `VectorStore`.

5. **`@role:` explicit routing bypasses the orchestrator.** By design — `@researcher: do X` goes direct-dispatch, orchestrator only fires for default-agent messages.

6. **Cross-agent "Recent Team Activity" preserved only on direct path.** Orchestrator-dispatched steps don't see other agents' recent activity.

---

## 10. Test coverage

| Area | Location | Count |
|---|---|---|
| orchestrator unit + e2e | `src/adapters/orchestrator/*` inline | 28 |
| memory subsystem | `src/adapters/memory/*` inline | 19 |
| memory plugin tools | `src/adapters/plugins/memory/*` inline | 17 |
| scope_lint | `tests/scope_lint.rs` | 2 |
| config | `src/adapters/config.rs` inline | 12 |
| eval runner | `src/adapters/eval_builder.rs` inline | ~25 |

All pass under `cargo test --bin tengu <filter>` with narrow filters (per project rule: ≤30s test runs, no blind full `cargo test`).

---

## 11. PR history

| PR | Title | Merged SHA | Net LOC |
|---|---|---|---|
| #9 | ci: remove step referencing missing scripts/check_rust_file_descriptions.sh | `6139d78` | −3 |
| #6 | feat: harness-owned orchestration + memory subsystem | `e76c08d` | +10,252 / −12,226 |
| #7 | refactor(memory): port MemoryService to memory::vector::* and delete old files | `9667ea3` | +575 / −1,175 |
| #8 | feat(channels): wire Telegram + TUI through Orchestrator::handle | `1533c48` | +570 / −58 |

Main is at `1533c48` with all four merged.
