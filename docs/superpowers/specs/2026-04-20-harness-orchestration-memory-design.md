# Harness-Owned Orchestration + Memory — Design

**Date:** 2026-04-20
**Status:** Draft pending user review
**Supersedes:** PR #5 (`feature/phase-b-orchestration-collapse`) and the deleted Phase B/C/D/E specs
**Replaces:** `docs/architecture.md` "Doctrine" section (the heart/brain/sensors framing)

---

## 1. Context

Phase 0 and Phase A shipped. Phase B (PR #5) attempted to collapse orchestration into an LLM-readable playbook skill (`skills/orchestration/SKILL.md`). That approach is rejected here for two reasons:

- **Context pollution by default.** A skill body teaching decomposition/delegation/failure-handling loads into context on every turn for any agent whose `skill_packages` includes it. Every turn pays for that body.
- **Unreliable policy delivery.** Whether the LLM follows the playbook is stochastic. Policy that matters (retry counts, replan bounds, cancellation, cache discipline) must run in code.

The new direction: **the harness owns orchestration, memory, and cache discipline. Agents are narrow LLM workers defined by config; they do not see orchestration machinery, and they do not manage conversational memory.**

Research corroboration: the Hermes Python agent's design makes prompt caching sacred (no mid-conversation system prompt mutation, no toolset swaps, memory injection at API-call time only, never persisted into history). We adopt the same discipline. Where Hermes puts delegation in the LLM's hands via a `delegate_task` tool, we diverge: the user wants a dedicated orchestrator agent with a LangGraph-style supervisor pattern, event-bus dispatch, and bounded replanning.

---

## 2. Doctrine (replaces heart/brain/sensors)

Three principles, checked against every PR:

### 2.1 The harness owns control flow

Orchestration, routing, memory retrieval, memory writes, retries, replans, cancellation, cache discipline, turn lifecycle — all of it is Rust code. No skill teaches these behaviours to an LLM, because no LLM is asked to decide them.

### 2.2 Agents are narrow LLM workers

An agent is an `AgentConfig` entry: a name, a system prompt, a model, a tool list, optional memory scope. Agents do not know about other agents. Agents do not decompose user requests. Agents do not spawn subagents. Each agent's conversation is a single stable system prompt + a user message + tool calls — the shape prompt caching demands.

### 2.3 The orchestrator is "just an agent with one tool"

The orchestrator is not a Rust class with baked-in planning logic. It is an `AgentConfig` entry like any other worker, with exactly one tool (`memory_search`) and a system prompt that teaches it to emit structured JSON plans. What makes it the orchestrator is its position in the runtime: it runs first, its output drives the DAG executor, and workers never invoke it back.

**No-compromise corollary:** if something in the codebase tries to encode per-user routing preferences, per-workflow templates, or task-specific retry strategies, stop. Those are config or agent system-prompt concerns, not Rust code. The harness owns *mechanism*, not *policy intent*.

---

## 3. Architecture

### 3.1 Two new subsystems under `src/adapters/`

```
src/adapters/
├── orchestrator/   # routing, planning, DAG execution, retry, replan, events
└── memory/         # provider trait, builtin provider, injector, writer, fencing
```

Existing `plugins/memory/` is refactored (not deleted) to hold the LLM-callable memory tools (`memory_ingest`, `memory_search`, `persistent_store`). Existing `plugins/subagents/` is deleted in full — no LLM-callable orchestration tools.

### 3.2 Request lifecycle

```
user_msg (from Telegram / CLI)
  ↓
Orchestrator::handle(user_msg, session_id) → event stream + final response
  ↓
  1. Orchestrator agent turn:
       inject memory block (query = raw user_msg)
       LLM loop with tool=[memory_search]
       final output: {kind:"direct", response} OR {kind:"plan", steps:[...]}
  ↓
  2. If direct:   emit PlanCompleted → channel renders → done
     If plan:     emit PlanCreated → DagExecutor runs
  ↓
  3. DagExecutor:
       ready-set scheduling (parallel via tokio::spawn)
       per-step: inject memory (query = step goal) → worker LLM turn → sync_turn
       emit StepStarted / StepSucceeded / StepFailed / StepExhausted
  ↓
  4. On StepExhausted (any step): emit ReplanTriggered
       re-invoke orchestrator with failure summary
       new plan OR direct bail-out
       bounded by max_replans
  ↓
  5. emit PlanCompleted{final_response}
     Channel renders final_response
```

### 3.3 Cache discipline (invariants)

Borrowed from Hermes. Every change to this codebase must preserve these:

- System prompts are stable for the lifetime of a conversation.
- Tool schemas are stable for the lifetime of a conversation.
- Memory context is injected at API-call time as a fenced block appended to the user message of the current turn. It is never persisted into the stored message history.
- No skill body is appended to a system prompt mid-conversation.
- Past context is never mutated. The only approved mutation is context compression (deferred — not part of this spec).

---

## 4. Components + file layout

```
src/adapters/
├── orchestrator/
│   ├── mod.rs            # pub: Orchestrator::handle, OrchestratorHandle, events
│   ├── config.rs         # orchestrator+workers roster parsing
│   ├── planner.rs        # runs the orchestrator agent's LLM call → PlannerVerdict
│   ├── plan.rs           # Step, StepId, Plan; topology helpers (ready-steps, cycle check)
│   ├── executor.rs       # DagExecutor: ready-set loop, spawn, join, dependent-step input assembly
│   ├── retry.rs          # RetryPolicy: per-step attempt counter, backoff, escalation
│   ├── replan.rs         # outer loop: on StepExhausted, re-invoke orchestrator
│   ├── events.rs         # OrchestratorEvent enum + broadcast channel type
│   └── telemetry.rs      # event→log bridge
│
├── memory/
│   ├── mod.rs            # MemoryManager (holds providers)
│   ├── provider.rs       # MemoryProvider trait (Hermes-shaped)
│   ├── builtin.rs        # BuiltinMemoryProvider — MEMORY.md + identity + daily log + vector
│   ├── injector.rs       # for_turn(agent, query) → PinnedMemoryBlock
│   ├── writer.rs         # sync_turn(agent, user, asst) — spawned, non-blocking
│   ├── fencing.rs        # <memory-context> block building + sanitization
│   ├── vector.rs         # embedding + Qdrant/bincode write/search
│   └── context_block.rs  # shared types: PinnedMemoryBlock, metadata shapes
│
├── plugins/
│   └── memory/           # REFACTORED: LLM-callable memory tools only
│       ├── mod.rs        # MemoryPlugin registering the 3 tools
│       ├── ingest.rs     # memory_ingest (was `remember` — write path)
│       ├── search.rs     # memory_search (new — targeted read path)
│       └── persistent.rs # persistent_store (kept as-is; deterministic KV)
│
├── channel_runtime.rs    # TOUCHED: routes user_msg → Orchestrator::handle
├── chat_builder.rs       # TOUCHED: process_user_text becomes worker entry point
├── telegram_builder.rs   # TOUCHED: subscribes to OrchestratorEvent for quiet progress
└── (deleted)
    ├── plugins/subagents/      # sessions_spawn, sessions_fan_out, subagents
    └── memory_builder.rs       # contents absorbed into src/adapters/memory/
```

### 4.1 Orchestrator module responsibilities

**`planner.rs`** — wraps the orchestrator agent's LLM call. Inputs: raw user prompt, failure context (replan only), worker roster descriptions. Loops with `tool=[memory_search]` until the agent emits structured JSON. Parses to `PlannerVerdict::Direct { response }` or `PlannerVerdict::Plan { steps }`. Handles malformed-JSON retries (budget-capped, default 3).

**`plan.rs`** — types + pure helpers:

```rust
pub struct StepId(String);
pub struct Step {
    pub id: StepId,
    pub agent: String,
    pub goal: String,
    pub depends_on: Vec<StepId>,
}
pub struct Plan {
    pub steps: Vec<Step>,
}

// helpers:
impl Plan {
    pub fn ready_steps(&self, completed: &HashSet<StepId>) -> Vec<&Step>;
    pub fn validate(&self) -> Result<(), PlanError>;     // cycle check, single-leaf check, agent-exists check
    pub fn single_leaf(&self) -> Option<&Step>;
}
```

Single-leaf constraint enforced at validation: a plan with multiple leaves is rejected, forcing the orchestrator to include a synthesizer step. `final_response` is always `leaf_step.output`.

**`executor.rs`** — the DagExecutor:

```rust
loop {
    let ready = plan.ready_steps(&completed);
    if ready.is_empty() && in_flight.is_empty() { break; }
    for step in ready {
        if !in_flight.contains(&step.id) && !completed.contains(&step.id) {
            spawn_step(step, &completed_outputs);
            in_flight.insert(step.id.clone());
        }
    }
    let finished = join_any_completed().await;  // returns first completed step
    match finished {
        StepOk(id, output) => { completed.insert(id); completed_outputs.insert(id, output); }
        StepExhausted(id, err) => { return ExecResult::NeedsReplan { failed: id, err }; }
    }
}
ExecResult::Done { final_output: plan.single_leaf().unwrap().output }
```

Dependent-step input assembly (`completed_outputs` → user message of next step):

```
<step-input from="s1">
<s1.output>
</step-input>

Your task:
<step.goal>
```

**`retry.rs`** — per-step retry wrapping. Public API:

```rust
pub async fn run_step_with_retry(
    step: &Step,
    worker: Arc<dyn WorkerHandle>,
    policy: &RetryPolicy,
    events: &EventBus,
) -> StepResult;
```

Exponential backoff `{1s, 3s, 9s}`. `max_attempts` from config, default 3.

**`replan.rs`** — outer loop around executor:

```rust
let mut replans_left = config.max_replans;
let mut plan = planner.initial_plan(user_msg).await?;
loop {
    match executor.run(&plan).await {
        ExecResult::Done { final_output } => return final_output,
        ExecResult::NeedsReplan { failed, err } if replans_left > 0 => {
            replans_left -= 1;
            events.emit(ReplanTriggered { reason: err.clone() });
            plan = planner.replan(user_msg, plan, failed, err).await?;
        }
        ExecResult::NeedsReplan { .. } => return orchestrator_bail_out(),
    }
}
```

**`events.rs`**:

```rust
pub enum OrchestratorEvent {
    PlanCreated { plan: Plan },
    StepStarted { step_id: StepId, agent: String },
    StepProgress { step_id: StepId, chunk: String },
    StepFailed { step_id: StepId, attempt: u32, error: String },
    StepExhausted { step_id: StepId, final_error: String },
    StepSucceeded { step_id: StepId, output: String },
    ReplanTriggered { reason: String },
    PlanCompleted { final_response: String, cancelled: bool },
}

pub type EventBus = tokio::sync::broadcast::Sender<OrchestratorEvent>;
```

Channels subscribe via `broadcast::Receiver`. The executor and replan loop are the only publishers.

### 4.2 Memory module responsibilities

**`provider.rs`** — trait modeled on Hermes's `MemoryProvider`:

```rust
#[async_trait]
pub trait MemoryProvider: Send + Sync {
    fn name(&self) -> &str;
    fn is_available(&self) -> bool;
    async fn initialize(&self, session_id: &str, workspace: &Path) -> Result<()>;

    fn system_prompt_block(&self) -> String { String::new() }
    async fn prefetch(&self, agent: &str, query: &str) -> String;
    async fn sync_turn(&self, agent: &str, user: &str, asst: &str);
    async fn on_pre_compress(&self, messages: &[Message]) -> String { String::new() }
    async fn shutdown(&self);
}
```

**`builtin.rs`** — `BuiltinMemoryProvider`:

- `system_prompt_block`: emits AGENTS.md + MEMORY.md + identity files + today's + yesterday's daily logs
- `prefetch`: runs vector search on the query, returns top-K chunks formatted with provenance metadata
- `sync_turn`: summarizes `(user, asst)` into a vector embedding and appends to today's daily log (file append)

**`injector.rs`** — public API:

```rust
pub async fn for_turn(mgr: &MemoryManager, agent: &str, query: &str) -> PinnedMemoryBlock;
```

Always wraps provider output in the fenced `<memory-context>` block with the Hermes-style system note. Called by orchestrator before its LLM turn (query=raw user_msg) and by executor before each worker step (query=step.goal).

**`writer.rs`** — public API:

```rust
pub fn sync_turn(mgr: Arc<MemoryManager>, agent: String, user: String, asst: String);
```

Spawns a detached `tokio::spawn` task that calls `mgr.sync_all(...)`. Returns immediately — the user-facing reply never waits on memory writes.

**`fencing.rs`** — pure string helpers:

```rust
pub fn build_memory_context_block(raw_context: &str) -> String; // wraps in <memory-context>…</memory-context> + system note
pub fn sanitize_context(text: &str) -> String;                  // strips any nested memory fences (defense in depth)
```

### 4.3 Plugin memory tools (LLM-callable)

- **`memory_ingest(text, metadata?, chunks?)`** — explicit document/fact ingestion. Chunks if needed, embeds, stores with metadata. For agents that need to push content into the vector store (PDF ingestion, URL archival).
- **`memory_search(query, top_k?, metadata_filter?)`** — targeted vector read. Returns chunks with scores + metadata. For mid-turn iterative lookups (agent B pulls specific chunks agent A ingested).
- **`persistent_store(action: get|set|delete|list, key, value?)`** — deterministic KV scratchpad, SQLite-backed. For exact-value recall (wallet addresses, user IDs, tx hashes) where vector search is too lossy.

The orchestrator agent gets `memory_search` only. Workers get any subset per their `AgentConfig.tools`.

---

## 5. Config shape

The project config is TOML-backed (`tengu.toml`), not YAML. The existing `Config` struct in `src/adapters/config.rs` already has `agents: HashMap<String, AgentConfig>` and `orchestrator: Option<OrchestratorConfig>` — we reshape these in place, we do not add a new config file.

### 5.1 `OrchestratorConfig` (reshape)

Current shape includes fields obsoleted by this design (`enabled`, `max_concurrent`, `planner_engine`, `planner_model`) — they supported the deleted `plugins/subagents/` path. New shape:

```rust
pub struct OrchestratorConfig {
    /// Name of the agent (in Config.agents) that acts as the orchestrator.
    /// Its AgentConfig defines the system prompt, model, and tools.
    pub agent: String,

    /// Tier 1: how many times a single step is retried before escalation.
    #[serde(default = "default_max_attempts_per_step")]
    pub max_attempts_per_step: u32,

    /// Tier 2: how many times the orchestrator is re-invoked to replan
    /// after exhaustion before bailing out.
    #[serde(default = "default_max_replans")]
    pub max_replans: u32,
}

fn default_max_attempts_per_step() -> u32 { 3 }
fn default_max_replans() -> u32 { 2 }
```

Presence of the `[orchestrator]` block in `tengu.toml` activates orchestration. Absence falls back to the single-agent-direct-dispatch path (the `AgentConfig { default = true }` entry in `Config.agents` handles the user message directly, skipping the orchestrator). This gives us a clean off-switch without a boolean.

### 5.2 `AgentConfig` — no schema change needed

The existing fields are sufficient:

- `identity.instructions` — serves as the system prompt (free-form text injected at prompt-build time). The orchestrator's system prompt goes here.
- `workspace_tools` — lists the first-party tools this agent can use. `"memory_search"` / `"memory_ingest"` / `"persistent_store"` slot in here.
- `skill_packages` — unchanged. Workers can still have skills for their domain-specific knowledge; just nothing orchestration-related.
- `limits` — per-agent iteration/token budgets.
- `scopes` — per-tool `ToolScope` entries, unchanged.

### 5.3 Example `tengu.toml` (new agents added to existing file)

```toml
[orchestrator]
agent = "orchestrator"          # references [agents.orchestrator] below
max_attempts_per_step = 3
max_replans = 2

[agents.orchestrator]
default = false
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

Roster injection: the `{{ roster }}` token in `agents.orchestrator.identity.instructions` is replaced at orchestrator-conversation-init time with a stable Markdown table of `(name, description)` pairs derived from `Config.agents`. Substitution happens ONCE per conversation, not per turn — cache stays warm.

---

## 6. Data flow specifics

### 6.1 Memory-injection queries

| Agent | Query used for vector search |
|-------|-----------------------------|
| Orchestrator | Raw user prompt (unmodified) |
| Worker | The step goal assigned by orchestrator |

The raw user prompt never reaches workers. Workers see only `[system: their config prompt + memory block] [user: step.goal + <step-input> from deps]`.

### 6.2 Step output → final response

Plan validation enforces a single leaf (§4.1). `leaf_step.output` is the `final_response` in `PlanCompleted`. If the orchestrator emits multiple leaves, the plan is rejected with a validation error returned as a replan trigger.

### 6.3 Cancellation (`/stop`)

Channels call `Orchestrator::cancel(session_id)` which sets an `AtomicBool` on the executor. The executor checks between step dispatches (not mid-step). In-flight workers finish their current LLM call — their outputs are discarded but their `sync_turn` writes still fire (partial progress is valid for future memory). Emits `PlanCompleted { cancelled: true, final_response: "Stopped by user." }`.

### 6.4 Concurrency — memory writes

- `memory_ingest`: serialized at the Qdrant/bincode write layer (atomic append). No extra locking.
- `sync_turn`: detached tasks, order across concurrent workers undefined, safe (each write is self-contained).
- `persistent_store`: SQLite `INSERT OR REPLACE` handles last-writer-wins per key.

---

## 7. Error handling — three tiers

### 7.1 Tier 1 — Step retry (invisible to LLM)

`RetryPolicy { max_attempts, backoff }` wraps each worker call. On error: `StepFailed` emitted, sleep, retry. After `max_attempts` exhausted: `StepExhausted`.

### 7.2 Tier 2 — Replan (visible to orchestrator)

On `StepExhausted`, executor returns `ExecResult::NeedsReplan`. Replan loop re-invokes orchestrator with a failure summary appended to its conversation history:

```
A previous plan failed.

Failed step: <id> (<agent>)
Goal: <goal>
Error after <n> attempts: <error text>
Completed steps (outputs preserved in memory):
  - <id> (<agent>): <short summary>

Produce a new plan that avoids this failure, or respond directly if recovery is not possible.
```

Orchestrator either emits a fresh plan or `{kind:"direct", response:"<bail-out>"}`. Bounded by `max_replans` (default 2).

### 7.3 Tier 3 — Orchestrator-itself failure

- LLM call fails 3× → emit `PlanCompleted { final_response: "System error: orchestrator unavailable." }`. Log loudly.
- Malformed JSON after parser retries exhausted → same.

---

## 8. Testing strategy

Tight scope per project convention — cap individual test runs ≤30s, no blind full `cargo test`.

### 8.1 Unit tests

- `planner.rs`: mocked LLM fixture, assert JSON parsing + malformed-output retry
- `plan.rs`: ready-steps detection, cycle detection, single-leaf validation
- `retry.rs`: injected failing `WorkerHandle`, attempt counter, backoff sequence, escalation
- `replan.rs`: driver loop against scripted exhaustion events, bound enforcement
- `memory/injector.rs`: fenced block shape, sanitization of nested fences in provider output
- `memory/writer.rs`: `sync_turn` spawns and returns immediately (no `.await` that would block)

### 8.2 Integration tests (single-process, in-memory backends)

- Single-step plan, happy path → `PlanCompleted`
- Multi-step DAG with parallel fan-out + synthesizer join → `PlanCompleted` with synthesizer output
- Step fails 3× → `StepExhausted` → replan → success on 2nd plan
- Replan budget exhausted → graceful `PlanCompleted` with bail-out text
- `/stop` mid-plan → cancellation → partial `sync_turn` writes preserved, discarded workers noted in log
- Orchestrator emits malformed JSON twice then valid → planner retry loop works
- `memory_ingest` from agent A → `memory_search` from agent B retrieves the ingested chunks
- Automatic pre-turn injection surfaces a recently ingested doc when the step goal semantically matches

### 8.3 Eval (uses existing `tengu eval` runner)

New config `evals/orchestration-e2e.yaml` with 5–10 representative workflows, LLM-judged on decomposition correctness + final answer quality. Compared against a baseline single-agent (no orchestrator) config as a cost/quality sanity check.

### 8.4 Deliberately skipped for v1

Property-based fuzzing on plan shapes, cross-process tests, load testing. Revisit once we have production telemetry.

---

## 9. Migration + disposition of existing work

### 9.1 PR #5 (`feature/phase-b-orchestration-collapse`)

**Close without merging.** Moves in the opposite direction (orchestration → LLM-readable playbook skill). Cherry-pick the independently useful eval-runner commits onto the new work branch; SHAs listed in the implementation plan.

### 9.2 Deleted specs / plans (already marked `D` in current `git status`)

Commit the deletions on the new work branch: Phase B orchestration-collapse spec, Phase C engine/channel/store spec, Phase D context-spill-to-RAG spec, Phase E self-alignment spec + plan. All subsumed or deliberately out of scope here.

### 9.3 Phase 0 doctrine docs — scrub, not archive

The user's instruction is to scrub. Delete:

- `docs/superpowers/specs/2026-04-15-phase-0-doctrine-design.md`
- `docs/superpowers/specs/2026-04-16-phase-0-implementation-design.md`
- `docs/superpowers/plans/2026-04-16-phase-0-doctrine.md`

Rewrite `docs/architecture.md` Doctrine section (lines 9–40) with the §2 content from this spec. Phase A specs (`2026-04-15-phase-a-tool-plugin-architecture-design.md`, `2026-04-16-phase-a-tool-plugin-architecture.md`) are retained — they are about tool plugins, orthogonal to the doctrine change.

### 9.4 Code deletions on the work branch

- `src/adapters/plugins/subagents/` — entire directory
- `src/adapters/memory_builder.rs` — contents absorbed into `src/adapters/memory/`
- `Config.orchestrator.enabled`, `.max_concurrent`, `.planner_engine`, `.planner_model` — removed; replaced by the reshaped `OrchestratorConfig` described in §5.1

### 9.5 Code refactors on the work branch

- `src/adapters/plugins/memory/` — rename `remember` → `memory_ingest`; add `memory_search`; keep `persistent_store`
- `src/adapters/channel_runtime.rs` — route user messages to `Orchestrator::handle`
- `src/adapters/chat_builder.rs` — `process_user_text` becomes the worker entry point
- `src/adapters/telegram_builder.rs` — subscribe to `OrchestratorEvent`, render quiet progress by default (single message edited in place)

---

## 10. v1 cuts (explicit non-goals)

- **Mid-step streaming to the user.** v1 renders at step boundaries only.
- **Cross-plan memoization.** Identical prompt re-planned; no plan cache.
- **Plan-level prompt caching across orchestrator conversations.** Each orchestrator session is independent.
- **Human-in-the-loop approval before plan execution.** If needed later, separate design.
- **Hot-reload of `tengu.toml` agent roster.** Roster or orchestrator config changes require harness restart.
- **Context compression.** Preserved as a future `ContextEngine`-style pluggable subsystem, out of scope here.
- **Multi-tenant concurrency in the orchestrator.** One session = one orchestrator conversation; existing per-user isolation model continues unchanged.

---

## 11. Implementation sequencing (rough)

Drives the shape of the follow-on plan document:

1. `memory/` subsystem first (provider trait, BuiltinMemoryProvider, injector, writer) — unit-testable, unblocks everything downstream
2. `plugins/memory/` refactor — rename `remember` → `memory_ingest`, add `memory_search`, keep `persistent_store`
3. `orchestrator/` skeleton — config, planner, plan types, executor, retry, replan — unit-testable without channels
4. Channel integration — Telegram + CLI route through `Orchestrator::handle`, event subscription, quiet rendering
5. Eval suite + doctrine scrub — `evals/orchestration-e2e.yaml`, `docs/architecture.md` rewrite, Phase 0 spec deletions, PR #5 closure
6. Tune + ship

---

## 12. Open items (to address in the plan, not the spec)

- Exact list of eval-runner commits to cherry-pick from PR #5 — enumerated in plan.
- Wire format for the `ChatRuntimeService` worker entry (changes to `process_user_text` signature, passing of memory block + step goal) — designed in plan.
- Backoff schedule tuning for `RetryPolicy` — spec defaults `{1s, 3s, 9s}` but production may want jitter; decided in plan against real LLM error patterns.
