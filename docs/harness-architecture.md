# Harness-Owned Orchestration + Memory — Architecture

> **Superseded (2026-09-18):** PR #6–#8-era snapshot. Current architecture: `docs/architecture-2026-04-27.md`; config shape: `docs/configuration.md` — the `tengu.toml` examples in §7 / §12 predate today's `AgentConfig` (`workspace_tools = ["memory_search"]` fails validation; subagents use `tools` + `description`).
> Doctrine here was replaced by "LLM = heart, Open Brain + Karpathy LLM Wiki = brain, tools = hands" (`CLAUDE.md`); plan steps run as `tengu run-agent` subprocesses (`runner.rs`), not `ChatWorker`.

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

### `src/application/orchestrator/`

| File | What it does |
|---|---|
| `mod.rs` | `Orchestrator` struct. Public API: `new`, `handle`, `subscribe`, `cancel`. |
| `config.rs` | Removed — `OrchestratorConfig` lives in `src/config/mod.rs`. |
| `events.rs` | `OrchestratorEvent` enum + `tokio::sync::broadcast` bus. |
| `plan.rs` | `Step`, `StepId`, `Plan` types. Topology helpers: `ready_steps`, `validate` (cycles, single-leaf, known agents), `single_leaf`. |
| `planner.rs` | `Planner` trait + `OrchestratorAgentPlanner` impl. Wraps the orchestrator agent's LLM call. Parses verdicts (handles markdown fences). |
| `retry.rs` | `RetryPolicy` + `run_step_with_retry`. Exponential backoff, emits `StepFailed` / `StepExhausted`. |
| `executor.rs` | `DagExecutor` + `WorkerHandle` trait. Parallel ready-set scheduling via `tokio::spawn`. Observes cancel flag. |
| `replan.rs` | `drive()` — outer loop. On `StepExhausted`, re-invokes planner with failure context. Bounded by `max_replans`. |
| `wiring.rs` | `ChatServiceFactory` trait + `ChatOrchestratorPortImpl` (planner LLM port). `ChatWorker` removed in Phase 7.1 — `SubprocessRunner` (`runner.rs`) is the only `WorkerHandle`. |
| `roster.rs` | Removed (Phase 7.1) — roster is rendered to `TENGU_PLANNER_REGISTRY.md` by `shared_files.rs`. |
| `telemetry.rs` | Removed — see `src/domain/metrics.rs`. |

### `src/application/memory/`

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
| `vector/qdrant.rs` | Removed Phase 6 (2026-05-14) — `DiskVectorStore` is the only built-in `VectorStore`; durable memory is Postgres `outbound/tools/agentic_memory/` (feature `postgres_memory`). |
| `vector/embedder.rs` | `Embedder` — OpenRouter embeddings client, `text-embedding-3-small`. |

LLM-callable memory tools live in `src/adapters/outbound/tools/memory/` and go through `MemoryManager`:

| Tool | File | Purpose |
|---|---|---|
| `memory_ingest` | `outbound/tools/memory/ingest.rs` | Explicit document/fact ingestion (chunks + metadata). |
| `memory_search` | `outbound/tools/memory/search.rs` | Targeted vector read mid-turn. |
| `persistent_store` | `outbound/tools/memory/persistent_store.rs` | Deterministic KV scratchpad (SQLite-backed). |

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

The solution in `bootstrap/`:

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

Helpers in `bootstrap/`:

- `ChatInputsFn = Arc<dyn Fn(&str) -> Result<ChatTurnInputs> + Send + Sync>` — the closure shape.
- `RuntimeChatServiceFactory::new(inputs_fn)` — impl of the `ChatServiceFactory` trait.
- `snapshots_inputs_fn(snapshots: OrchestratorSnapshots) -> ChatInputsFn` — returns a closure that reads from the shared map.
- `build_orchestrator(config, factory, memory) -> Option<Orchestrator>` — builds the full stack.

**Telegram** (`adapters/inbound/telegram.rs`): `execute_orchestrator_turn` is called when orchestrator is configured AND the user did NOT `@role:`-route explicitly. Writes snapshots for every agent, awaits `handle`, sends the reply chunked + secret-redacted.

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
# planner turn: tools are stripped (bootstrap::orchestrator::run_turn_with_system)
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
description = "Focused researcher"   # present ⇒ planner-routable subagent
tools = ["http_request", "memory_search", "memory_ingest"]
[agents.researcher.identity]
instructions = "You are a focused researcher..."

[agents.writer]
engine = "openrouter"
model = "anthropic/claude-sonnet-4-6"
description = "Drafts polished responses"
tools = ["memory_search"]
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
- `src/adapters/outbound/tools/subagents/` (entire dir: `mod.rs`, `spawn.rs`, `fan_out.rs`, `manage.rs`)
- `src/adapters/outbound/tools/memory/remember.rs` (renamed to `ingest.rs`)

### Deleted types
- `MemoryService`, `MemoryServiceHandle`, `EmbeddingPort`, `MemoryStorePort`, `MemoryEntry`, `MemorySearchResult`
- `DiskVectorMemoryStore`, `QdrantMemoryStore`, `OpenRouterEmbeddingAdapter`
- `SubagentRegistry`, `SubagentHandle` + all session-spawn LLM-callable tools

### Deleted doctrine
- `docs/superpowers/specs/2026-04-15-phase-0-doctrine-design.md` (not in the archive)
- `docs/superpowers/specs/2026-04-16-phase-0-implementation-design.md` (not in the archive)
- `docs/superpowers/plans/2026-04-16-phase-0-doctrine.md`
- "heart / brain / sensors" language in `docs/architecture.md`

### Renamed / refactored
- `remember` tool → `memory_ingest` (extended with `chunks`, `metadata`)
- `OrchestratorConfig` fields: dropped `enabled` / `max_concurrent` / `planner_engine` / `planner_model`; added `agent` / `max_attempts_per_step` / `max_replans`
- `outbound/tools/memory/mod.rs` — three tools, all routing through `MemoryManager`

---

## 9. Status of known limitations

All six limitations originally noted at the time of the four-PR merge have been addressed in a follow-up polish pass. Current state:

1. **Batch embedding.** ✅ Fixed. `Embedder::embed_batch(&[&str]) -> Vec<Vec<f32>>` does one HTTP round-trip for N inputs. `MemoryManager::ingest_batch(texts, agent, metadata)` uses it; `memory_ingest` multi-chunk path routes through it. Single-text `embed()` is a thin wrapper.

2. **Per-id delete.** ✅ Fixed. `VectorStore::delete(id)` + `VectorStore::write` now returns the synthetic entry id. `persistent_store` tracks `chunk_ids` in its `FileManifest` and calls `MemoryManager::delete_entry(id)` per chunk on file delete. Legacy manifests without ids log a warning.

3. **`/purge` clears memory.** ✅ Fixed. Telegram `/purge` and TUI `/purge` both call `MemoryManager::clear_all()` which delegates to `VectorStore::clear_all()`. Disk impl truncates + flushes; legacy vector DB impl deletes the collection and recreates it with the same vector config.

4. **TUI status line shows memory stats.** ✅ Fixed. `VectorStore::entry_count()` + `storage_bytes()` added. `MemoryManager::stats() -> Option<(usize, u64)>` surfaces them. TUI updates the status bar after each turn: `mem: N entries / M KB`.

5. **`@role:` explicit routing.** ✅ Configurable. New `OrchestratorConfig.route_explicit_agents` (default `false`) preserves the direct-dispatch escape hatch; set `true` to route explicit targets through the orchestrator so the planner sees the request and can honor or override.

6. **Cross-agent Recent Team Activity.** ✅ Fixed. `execute_orchestrator_turn` injects `build_activity_context(&self.activity_log, agent_id)` into each per-agent snapshot's system prompt — same as the direct dispatch path.

---

## 10. Sequence diagrams

### 10.1 Orchestrator happy path (sequential multi-step)

```
User          Channel         Orchestrator    Planner         Executor         Worker(s)         Memory
  │              │                 │              │                │                │                │
  │─── msg ─────▶│                 │              │                │                │                │
  │              │──── handle ─────▶              │                │                │                │
  │              │                 │── plan ─────▶│                │                │                │
  │              │                 │              │── LLM call ───────────────────────────────────────▶ prefetch
  │              │                 │              ◀─── verdict ──── (plan: s1 → s2)  │                │
  │              │                 │─ PlanCreated ▶ bus                               │                │
  │              │                 │── run plan ─────────────────▶│                │                │
  │              │                 │              │                │─ ready=[s1] ──▶│                │
  │              │                 │              │                │                │── mem inject ─▶│
  │              │                 │              │                │                │── LLM turn ───▶│
  │              │                 │              │                │                │◀── output ─────│
  │              │                 │              │                │                │── sync_turn ──▶│ (spawned)
  │              │                 │              │                │◀─ StepSucceeded │                │
  │              │                 │              │                │─ ready=[s2] ──▶│                │
  │              │                 │              │                │                │ ...            │
  │              │                 │              │                │◀─ StepSucceeded │                │
  │              │                 │              │                │─ all done ────▶│                │
  │              │                 │◀─── final_response from leaf ─│                │                │
  │              │◀── reply ────── PlanCompleted                                                      │
  │◀── reply ───│                                                                                    │
```

### 10.2 Replan on step exhaustion

```
Executor                Worker                RetryPolicy               Replan          Planner
    │                     │                        │                       │               │
    │── run_step ────────▶│                        │                       │               │
    │                     │─── fail #1 ───────────▶│                       │               │
    │                     │◀─── backoff 1s ────────│                       │               │
    │                     │─── fail #2 ───────────▶│                       │               │
    │                     │◀─── backoff 3s ────────│                       │               │
    │                     │─── fail #3 ───────────▶│                       │               │
    │◀───── StepExhausted ─│ (max_attempts_per_step=3)                     │               │
    │── NeedsReplan ───────────────────────────────▶│                      │               │
    │                                                │──── planner.replan ─▶│               │
    │                                                │                     │── LLM call ──▶│ (orchestrator
    │                                                │                     │               │  agent, with
    │                                                │                     │               │  failure ctx)
    │                                                │                     │◀── new plan ──│
    │◀─── run new plan ──────────────────────────────│                     │               │
    │  ... continues until done or max_replans=2 exceeded ...
```

### 10.3 Cancel (`/stop`) mid-plan

```
User         Channel         Orchestrator      Executor         Worker
  │            │                   │                │               │
  │─/stop ────▶│                   │                │               │
  │            │── cancel() ───────▶ (flag=true)    │               │
  │            │                   │                │ (in-flight    │
  │            │                   │                │  worker keeps │
  │            │                   │                │  running until│
  │            │                   │                │  LLM returns) │
  │            │                   │◀── check flag ─│               │
  │            │                   │                │← skip dispatch│
  │            │                   │  (ExecResult::Cancelled)       │
  │            │◀── "Stopped by user." ────────────                 │
  │◀── reply ──│                                                    │
```

Note: AtomicBool is checked between step dispatches, NOT mid-step. A worker already running an LLM call finishes that call and its output is discarded.

---

## 11. Code walkthrough

When the TL;DR isn't enough. Follow these paths in order.

### 11.1 A user message arrives in Telegram

1. `adapters/inbound/telegram.rs:878` — dispatcher calls `route_and_chat(msg, sender_id)`.
2. `adapters/inbound/telegram.rs:1026` — `route_and_chat` parses `@role:` prefix via `adapters::inbound::channel::parse_agent_routing`. Resolves `target_agent_id`.
3. `adapters/inbound/telegram.rs:1072` — `execute_chat_turn` is called. Hot-reloads skills, rebuilds tools/prompt if dirty.
4. `adapters/inbound/telegram.rs:1146` — orchestrator-gate check:
   - orchestrator configured AND (default agent OR `route_explicit_agents=true`) → branches to `execute_orchestrator_turn`
   - otherwise → direct dispatch below
5. **Direct path:** `adapters/inbound/telegram.rs:1258` injects activity context, builds `ChatRuntimeService<'a>` inline, calls `process_user_text`.
6. **Orchestrator path:** `adapters/inbound/telegram.rs:1384` is `execute_orchestrator_turn`:
   - Hot-reload every agent (lines 1392–1407).
   - For each agent, build a `ChatTurnInputs` snapshot (lines 1416–1486) with per-agent `current_tools`, `current_system_prompt` + activity context, per-agent `OwnedSanitizedToolExecutor`, the shared memory manager.
   - Publish the snapshots atomically (lines 1492–1508).
   - `orchestrator.handle(user_content).await` (line 1521).
   - Redact + chunk the reply.

### 11.2 Inside `Orchestrator::handle`

- `orchestrator/mod.rs:83` — `handle()` resets cancel flag and calls `replan::drive`.
- `orchestrator/replan.rs` — `drive` runs the planner once, loops the executor + planner-on-exhaustion until `Done` / exhausted replans / cancelled.
- `orchestrator/planner.rs` — `OrchestratorAgentPlanner::plan` calls the orchestrator agent's LLM turn through the `OrchestratorChatPort` (backed by `ChatOrchestratorPortImpl` → `ChatServiceFactory.run_turn`).
- `orchestrator/planner.rs:135` — `parse_verdict` strips markdown fences and deserializes the JSON into `PlannerVerdict::Direct` or `PlannerVerdict::Plan`.

### 11.3 Inside `DagExecutor`

- `orchestrator/executor.rs:64` — `run(plan, worker, policy, events, cancel)` main loop.
- `orchestrator/plan.rs:94` — `Plan::ready_steps(&completed)` returns steps with all deps satisfied.
- `orchestrator/executor.rs` spawns each ready step via `tokio::spawn`; collects completions via `FuturesUnordered`.
- `orchestrator/retry.rs` — `run_step_with_retry` wraps each worker call with backoff.

### 11.4 Inside a worker step

- `orchestrator/wiring.rs` — `ChatWorker::run_step`:
  1. `injector::for_turn(memory, step.agent, step.goal)` → fenced `<memory-context>` block.
  2. Assemble user content: `memory_block + (step_inputs if any) + step.goal`.
  3. `ChatServiceFactory::run_turn(agent, content)` → real LLM call.
  4. `writer::sync_turn(memory, agent, step.goal, reply)` — spawned, non-blocking.
- `bootstrap/` `RuntimeChatServiceFactory::run_turn`:
  1. Calls `inputs_fn(agent)` → snapshot.
  2. Constructs `ChatRuntimeService<'a>` borrowing into the snapshot.
  3. Calls `process_user_text(&mut ChatLoopState::default(), content)`.
  4. Returns `assistant_text` from the `ChatTurnResult`.

### 11.5 Memory flow for a single turn

Pre-turn (inside `ChatWorker::run_step`):
- `injector::for_turn(mgr, agent, query)`:
  - `mgr.prefetch_all(agent, query)` → iterates providers, each returns formatted hits.
  - `fencing::build_memory_context_block(raw)` → wraps in `<memory-context>`.
  - Returns `PinnedMemoryBlock` — NOT persisted in message history.

Post-turn (inside `ChatWorker::run_step`, after LLM call):
- `writer::sync_turn(mgr, agent, user, asst)`:
  - `tokio::spawn` a detached task that calls `mgr.sync_all(agent, user, asst)`.
  - Returns immediately — user-visible reply never waits.

LLM-tool path (inside `process_user_text` if the LLM calls `memory_search`):
- `outbound/tools/memory/search.rs` → `MemoryManager::search(query, top_k, filter)` → `Embedder::embed` + `VectorStore::search`.

Explicit ingest path (if the LLM calls `memory_ingest`):
- `outbound/tools/memory/ingest.rs` → `MemoryManager::ingest_batch` (multi-chunk) OR `ingest_one` (single) → `Embedder::embed_batch` + N × `VectorStore::write`.

---

## 12. Operational runbook

### 12.1 Enabling orchestration

1. Add `[orchestrator]` + matching `[agents.<name>]` block to `tengu.toml` (see §7).
2. Ensure `OPENROUTER_API_KEY` (or equivalent for your engine) is set.
3. Ensure a default agent exists (`default = true` on one of `[agents.*]`).
4. Restart the binary. On startup logs:
   - `"TUI orchestrator constructed with per-turn snapshot factory"` — TUI.
   - `"Telegram orchestrator constructed with per-message snapshot factory"` — Telegram.
5. Send a user message. First-turn overhead is ~1 extra LLM call (the planner).

### 12.2 Disabling orchestration

Remove or comment out the `[orchestrator]` block. Falls back to single-agent-default dispatch (same behavior as before PR #6). No code changes needed.

### 12.3 Debugging: orchestrator didn't fire

Checklist:
- `[orchestrator]` block present and parses? Check startup config errors.
- Agent named in `orchestrator.agent` exists in `[agents.*]`?
- Message routed by `@role:`? Then it's bypassed by default. Set `route_explicit_agents = true` if you want it through.
- Channel is Telegram or TUI? Other adapters (if any) don't yet wire orchestrator.

Logs to grep for:
- `"TUI orchestrator constructed"` / `"Telegram orchestrator constructed"` — confirms construction at startup.
- `"orch: plan created"` — confirms a plan was built on a message.
- `"orch: ▶ <step> [<agent>]"` — confirms a step fired.
- `"orch: replan triggered"` — step exhausted, planner re-invoked.

### 12.4 Debugging: memory didn't kick in

Memory is automatic IF `build_memory_manager` returned `Some` at startup. Preconditions:
- `[memory] enabled = true` in `tengu.toml` (default yes).
- `OPENROUTER_API_KEY` set (the embedder needs it — even the BuiltinMemoryProvider requires it for `prefetch`).
- `<workspace>/memory/` is writable. Disk store creates it on first write.

Logs:
- `"DiskVectorStore loaded"` — disk backend ready.
- `"QdrantVectorStore connected"` — legacy vector DB backend ready.
- `"OPENROUTER_API_KEY not set, memory disabled"` — backend didn't initialize.

Status:
- `mgr.has_vector_backend()` returns `true` once a backend is installed.
- TUI bottom bar: `mem: N entries / K KB` updates after every turn.

### 12.5 Wiping memory

- Telegram: `/purge` — clears conversations + calls `MemoryManager::clear_all()`.
- TUI: `/purge` — same.
- CLI file-level: delete `<store_path>/vectors.bin` (`[memory] store_path`, default `~/.tengu/memory/`) manually. Still supported as a fallback (safe to do when the harness isn't running).

### 12.6 Adding a new worker agent

1. Add `[agents.<name>]` + `[agents.<name>.identity]` block to `tengu.toml` (see the example in §7).
2. Set `description` (makes it planner-routable) and list its tools in `tools`.
3. Restart.
4. `TENGU_PLANNER_REGISTRY.md` is regenerated on the next planner turn — the planner sees the new agent as a dispatch target.

### 12.7 Tuning retry / replan budgets

Default: `max_attempts_per_step = 3`, `max_replans = 2`. Raise either in `[orchestrator]` if your workers are flaky or your plans need more refinement opportunities. Don't raise them without a reason — each replan costs a full orchestrator-agent LLM call.

---

## 13. Extension points

For future work, the harness is explicitly designed to accept extensions at these boundaries:

| Extension | Interface | Typical use |
|---|---|---|
| New channel (Slack, Discord, HTTP API) | Implement `ChatInputsFn` closure + call `build_orchestrator` | Wiring a new IM/API endpoint into the same orchestration flow |
| New memory backend | `impl VectorStore` (and register via `MemoryManager::set_vector_backend`) | Swap legacy vector DB for Pinecone, or add an in-memory mock for tests |
| External memory provider (LangMem, Letta, Mem0) | `impl MemoryProvider` + `MemoryManager::add_provider` | Layer semantic-memory products over the builtin store |
| Custom orchestrator (rules-based, cheaper) | `impl Planner` instead of `OrchestratorAgentPlanner` | Skip LLM planner calls for recognized patterns |
| Custom worker handle (delegate to remote cluster, Modal, etc.) | `impl WorkerHandle` | Fan work out to hosted inference instead of local engine |
| Replay / deterministic test | `impl ChatServiceFactory` with scripted responses | Golden-test orchestration flows without live LLM calls |

Each interface is ≤10 methods and documented in its defining file.

---

## 14. Test coverage

| Area | Location | Count |
|---|---|---|
| orchestrator unit + e2e | `src/application/orchestrator/*` inline | 28 |
| memory subsystem | `src/application/memory/*` inline | 19 |
| memory plugin tools | `src/adapters/outbound/tools/memory/*` inline | 17 |
| scope_lint | `tests/scope_lint.rs` | 2 |
| config | `src/config/mod.rs` inline | 12 |
| eval runner | `src/adapters/inbound/eval.rs` inline | ~25 |

All pass under `cargo test --bin tengu <filter>` with narrow filters (per project rule: ≤30s test runs, no blind full `cargo test`).

### 14.1 End-to-end eval dispatch

`tengu eval <skill>` has two dispatch paths in `src/adapters/inbound/eval.rs::run_row`:

- **Direct** (default): row prompt → default agent's `collect_engine_response`. Used by config files without an `[orchestrator]` block — `skills/<name>/evals/config.toml`.
- **Orchestrator** (when `cfg.orchestrator.is_some()`): row prompt → `Orchestrator::handle`. The orchestrator agent plans, the DAG executor spawns worker steps, each worker step calls `collect_engine_response` inside an `EvalChatServiceFactory` closure that threads the row's stubs + observer + token accumulator. Used by `skills/orchestration-e2e/evals/config.toml`.

Both paths produce the same `RowResult` shape for the judge — observations accumulate across worker steps, tokens sum, final text is the plan's leaf output.

Runbook for smoke testing orchestration: see `docs/orchestration-test-scenarios.md`.

---

## 15. PR history

| PR | Title | Merged SHA | Net LOC |
|---|---|---|---|
| #9 | ci: remove step referencing missing scripts/check_rust_file_descriptions.sh | `6139d78` | −3 |
| #6 | feat: harness-owned orchestration + memory subsystem | `e76c08d` | +10,252 / −12,226 |
| #7 | refactor(memory): port MemoryService to memory::vector::* and delete old files | `9667ea3` | +575 / −1,175 |
| #8 | feat(channels): wire Telegram + TUI through Orchestrator::handle | `1533c48` | +570 / −58 |
| post-merge polish | embed_batch, delete/clear_all/entry_count/storage_bytes, route_explicit_agents, activity-in-snapshots, /purge → clear_all, TUI stats | (current branch) | +~400 / −~50 |
