# Tengu-Cluster: Redesign Briefing for Implementation

> **Purpose of this document**: This is a cold-start briefing for a new Claude session.
> It captures every design decision made in the prior architecture session so you can
> begin implementation immediately without re-litigating any decisions.
> Do not re-open closed questions. Implement what is written here.

---

## 1. Project Background

**Tengu-cluster** is a Rust single-binary AI agent harness. It runs on a user's machine
(Linux/macOS), exposes a TUI and Telegram interface, and orchestrates multi-agent workflows
via a DAG planner.

The codebase is "vibecoded" — architecturally coherent principles but the implementation
accumulated dead weight: hardcoded agent rosters, per-sandbox config directories, static
system prompts, and no semantic discovery. The redesign strips this away and replaces it
with a single central authority: **Qdrant RAG**.

### What exists today (v1) — the problems

| Problem | Root cause |
|---------|-----------|
| Orchestrator has a hardcoded agent roster | `orchestrator/roster.rs` and `orchestrator/wiring.rs` build static markdown tables |
| Adding an agent requires knowing Rust internals | No file-based agent spec |
| Context balloons with every task | No semantic selection — entire roster stuffed into every prompt |
| Memory exists but is reactive, not a discovery system | `memory/` stores facts but is never queried for planning |
| Skill evolution only exists in design docs | `evolve.rs` and `ideas/` — never wired |
| Per-sandbox config dirs are rigid | `sandboxes/*/config.toml` — dozens of dirs for what should be a single `.toml` |

---

## 2. The New Architecture — Core Doctrine

Three principles. Every PR is reviewed against them. Violating any one blocks the PR.

### Principle 1 — LLM is the heart

It consumes tokens and emits tokens. It has no behaviour of its own. The Rust core never
hard-codes behaviour that belongs to the model.

### Principle 2 — RAG is the brain

Everything the system knows lives in the RAG store: skill descriptions, agent specs, tool
descriptions, user messages, step outputs. The orchestrator never loads everything into
context — it queries Qdrant for what is relevant to the current task and loads only that.
Context stays small by design.

Corollary: if something needs to influence agent behaviour, it must be in the RAG store or
in a skill body. There is no other path.

### Principle 3 — Tools and MCP are the hands

Tools are the only way any LLM touches the world. Compiled-in tools and MCP-served tools
are equal citizens — same `{name, description, parameters}` interface to the LLM, different
execution path internally.

### The composability corollary

Skills compose agents. Agents compose plans. Plans compose the user response. Nothing is
hardwired. A user changes what an agent does by editing a `.toml` or a `SKILL.md`. They
do not file a PR. **If behaviour requires a PR to change, that is a doctrine violation.**

---

## 3. Complete System Flow

```
Startup
  ├── scan skills/, agents/
  ├── connect to each MCP server → tools/list
  ├── embed descriptions → Qdrant (tengu_registry, permanent)
  ├── TTL cleanup: delete from tengu_memory older than 7 days
  └── if tengu_registry empty → cold-start fallback (see §Cold Start)

User message arrives (TUI / Telegram)
  │
  ├── embed message → store in tengu_memory (TTL 7 days)
  │
  ├── Orchestrator loads:
  │     skills/orchestrator/SKILL.md       ← system prompt (user-editable)
  │     skills/orchestrator/plan_schema.json ← plan output contract (harness-validated)
  │
  ├── Orchestrator calls RAG:
  │     query = embed(user_message)
  │     results = qdrant.search(tengu_registry, query, top_k=20)
  │     → ranked list of agents + skills + tools by cosine similarity
  │
  ├── If no agent above similarity threshold:
  │     C → ask user: "Closest match is X (confidence Y%). Use it or describe what you need?"
  │     user confirms → B → compose generic agent from closest-matching spec + top RAG hits
  │     (composed config used for this run only — not persisted unless user asks)
  │
  ├── Orchestrator LLM call:
  │     input  = [system_prompt, roster_of_retrieved_items, user_message]
  │     output = PlannerVerdict (validated against plan_schema.json):
  │               Direct { response: String }    ← respond immediately
  │             | Plan   { steps: Vec<Step> }    ← pass to DagExecutor
  │
  ├── If Direct → return response, done.
  │
  └── If Plan → DagExecutor:
        for each ready step (dependencies satisfied), tokio::spawn:
          │
          └── Runner (src/adapters/runner.rs):
                stdin JSON:
                  { goal, agent_name, model, tools: [...], skills: [...],
                    max_turns, sandbox? }
                assembles system prompt:
                  [base_agent_template]          ← hardcoded in runner.rs
                  [skill body 1..N]              ← loaded from disk
                  [mandatory suffix]             ← hardcoded:
                    "When you have completed your task, your FINAL action
                     MUST be to call compress_and_store(summary) with a
                     concise summary of what you accomplished."
                starts LLM mini-loop:
                  tools = resolved compiled-in + MCP tools from agent spec
                  workspace = spec.sandbox path OR temp dir (default)
                  runs until: compress_and_store called OR max_turns OR error
                on compress_and_store(summary):
                  write to tengu_memory (Qdrant, TTL 7 days)
                  type: "step_output", session_id, step_id, created_at
                exit → stdout JSON:
                  ok:     { status:"ok", output:"<full text>", summary:"<compressed>" }
                  failed: { status:"failed", error:"...", output:"<partial>" }

        DagExecutor tracks:
          completed_outputs: HashMap<StepId, String>
          ← EXACT, in-memory. Used for within-plan step dependency injection.
          ← NOT RAG. NOT fuzzy. Always reliable for direct dependencies.

        On step failure:
          NeedsReplan → orchestrator queries tengu_memory for context
                      → replan(user_message, prior_plan, failed_step, error)
                      → repeat until max_replans reached

        On PlanCompleted:
          broadcast OrchestratorEvent → TUI / Telegram
          return final response to user
```

---

## 4. Directory Layout

```
<workspace>/
│
├── skills/
│   ├── orchestrator/
│   │   ├── SKILL.md            ← orchestrator system prompt (user-editable)
│   │   └── plan_schema.json    ← plan JSON schema (harness validates this)
│   ├── web-research/
│   │   └── SKILL.md
│   ├── summarizer/
│   │   └── SKILL.md
│   └── [any skill]/
│       └── SKILL.md
│
├── agents/
│   ├── researcher.toml
│   ├── coder.toml
│   └── [any agent].toml
│
└── (no tools/ directory)
    Tools = compiled-in plugins + MCP servers in config.toml
```

The orchestrator has NO agent spec file. It is the entry point — hardcoded to load
`skills/orchestrator/SKILL.md`. It is not a peer agent.

---

## 5. Qdrant Collections

### tengu_registry (permanent — skills, agents, tools)

```
payload:
  type:         "skill" | "agent" | "tool"
  name:         string
  source_path:  string      ← path on disk; used for dedup
  content_hash: string      ← sha256 of description text; skip re-embed if unchanged
  description:  string      ← the text that was embedded
```

### tengu_memory (ephemeral — messages, step outputs, files)

```
payload:
  type:         "message" | "step_output" | "file_chunk"
  session_id:   string
  step_id:      string      ← for step_output entries
  created_at:   unix timestamp (u64)
  content:      string      ← the text that was embedded
```

TTL = 7 days. Purged at startup before indexing:

```rust
let cutoff = Utc::now() - Duration::days(7);
qdrant.delete(collection: "tengu_memory", filter: created_at < cutoff)
```

### Startup indexer

```
for each item in (skills + agents + MCP tools + compiled-in tools):
  hash = sha256(item.description)
  existing = qdrant.scroll(filter: source_path == item.path)
  if existing and existing.payload.content_hash == hash:
    skip     ← already up to date, saves embedding API cost
  else:
    qdrant.upsert(embed(description), payload)
```

No file watcher. No daemon. Runs at startup + when user provides new files via channel.

---

## 6. Agent Spec Format

`agents/<name>.toml` — one file per agent.

```toml
name        = "researcher"
description = "Researches topics using web search and document reading. \
               Best for: fact-finding, summarising sources, due diligence. \
               NOT for: code generation, system administration."
model       = "openai/gpt-4o"          # any OpenRouter slug
tools       = ["http_request", "read_file", "remember"]
skills      = ["web-research", "summarizer"]
max_turns   = 20
timeout_secs = 180

# Optional: if set, this path is the agent's persistent workspace.
# If absent, runner creates a temp dir and deletes it after the step.
# sandbox = "workspaces/researcher"
```

The `description` field IS what gets embedded in `tengu_registry`. Write it for semantic
search: what tasks the agent handles, what it is NOT good for. More specific = better recall.

---

## 7. Runner IPC Protocol

### Subprocess spawn

```rust
// src/adapters/runner.rs
let child = Command::new(current_exe())
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())    // piped — runner forwards to OrchestratorEvent::StepProgress
                                //  (NEVER inherit; would clobber the TUI)
    .env("TENGU_AGENT_IPC", "1") // re-entry guard; main.rs refuses run-agent without it
    .arg("run-agent")          // dedicated subcommand
    .spawn()?;
```

### stdin (JSON)

```json
{
  "goal":        "Find the three most cited papers on protein folding from 2023.",
  "agent_name":  "researcher",
  "model":       "openai/gpt-4o",
  "tools":       ["http_request", "read_file", "remember"],
  "skills":      ["web-research", "summarizer"],
  "max_turns":   20,
  "sandbox":     null
}
```

### stdout (JSON)

Success:
```json
{
  "status":  "ok",
  "output":  "<full agent text output — may be long>",
  "summary": "<compressed summary already written to Qdrant by compress_and_store>"
}
```

Failure:
```json
{
  "status": "failed",
  "error":  "max_turns exceeded without calling compress_and_store",
  "output": "<partial output>"
}
```

Exit code 0 = harness reads JSON and handles status. Non-zero = subprocess crash → hard
failure → DagExecutor retry policy kicks in.

### System prompt assembly (inside `run-agent` mode)

```
[base_agent_template]        ← hardcoded string in runner.rs; not user-editable
[skill body 1]               ← full SKILL.md text for skills[0]
[skill body 2]               ← full SKILL.md text for skills[1]
[...]
[mandatory suffix]           ← hardcoded:
  "When you have completed your task, your FINAL action MUST be to call
   compress_and_store(summary) with a concise summary of what you
   accomplished and found. Failure to do so will be treated as task failure."
```

The mandatory suffix is appended by the runner, not the skill. The LLM cannot avoid it.
This is the harness-enforced approach (the design session called it "Option C"):
the prompt suffix is owned by the harness, not by SKILL.md authors, so the behaviour
cannot be lost by a skill edit.

### Three-tier skill loader (required — mirrors `docs/architecture-v2.md`)

When resolving a skill name in `ipc.skills`, the runner looks it up in this order
(later tiers shadow earlier ones):

| Tier | Path | Precedence |
|------|------|------------|
| 1. Managed        | `~/.tengu/skills/<name>/SKILL.md`              | lowest  |
| 2. Workspace dotdir | `<workspace>/.tengu/skills/<name>/SKILL.md`  | shadows 1 |
| 3. Workspace root | `<workspace>/skills/<name>/SKILL.md`            | shadows 1 & 2 |

If the skill is not found in any tier, the runner fails fast with a clear
`skill_not_found: <name>` error before the LLM is called. Do not fall back to
empty prompt — that silently degrades agent quality.

### Trust boundary on the stdin `tools` list

The runner MUST NOT trust the stdin `tools` list on its own. It loads
`agents/<agent_name>.toml` itself and computes:

```
effective_tools = (spec.tools ∩ ipc.tools) ∪ {compress_and_store}
```

`compress_and_store` is always appended implicitly — never listed in agent specs,
never listed in stdin. If stdin asks for a tool the spec does not declare, the
runner drops it silently and logs a warning. This keeps doctrine intact ("tools
are the only way the LLM touches the world") even if the IPC channel is ever
reached by something other than the orchestrator.

---

## 8. `compress_and_store` Tool

Compiled-in. Always registered for every subagent. Not optional. Not user-configurable.

```json
{
  "name": "compress_and_store",
  "description": "Store a compressed summary of your completed work. Call this as your FINAL action when the task is done. Do not call it mid-task.",
  "parameters": {
    "type": "object",
    "properties": {
      "summary": {
        "type": "string",
        "description": "Concise summary of what you accomplished, found, or produced."
      }
    },
    "required": ["summary"]
  }
}
```

On call:
1. Writes `{summary}` to `tengu_memory` (Qdrant), payload: `type:"step_output"`, `session_id`, `step_id`, `created_at:now`
2. Sets an internal flag in the runner
3. Runner checks this flag when mini-loop ends — if set, step completed cleanly

There is NO separate compression LLM call. The subagent writes its own summary. Latency
is zero beyond what the LLM already does. (Decision from design session: Option B for
who compresses — subagent itself.)

---

## 9. Plan Schema

`skills/orchestrator/plan_schema.json` — stored beside `SKILL.md`, version-controlled,
read by the harness at startup and used to validate every planner LLM output.

```json
{
  "$schema": "http://json-schema.org/draft-07/schema",
  "oneOf": [
    {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "kind":     { "const": "direct" },
        "response": { "type": "string" }
      },
      "required": ["kind", "response"]
    },
    {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "kind": { "const": "plan" },
        "steps": {
          "type": "array",
          "items": {
            "type": "object",
            "additionalProperties": false,
            "properties": {
              "id":         { "type": "string" },
              "agent":      { "type": "string" },
              "goal":       { "type": "string" },
              "depends_on": { "type": "array", "items": { "type": "string" } }
            },
            "required": ["id", "agent", "goal", "depends_on"]
          }
        }
      },
      "required": ["kind", "steps"]
    }
  ]
}
```

Each variant is explicitly `type: "object"` with `additionalProperties: false` so
the `oneOf` discriminator is unambiguous — a blob with keys from both branches is
rejected.

On parse failure: harness retries the planning LLM call (max 3 retries, logs each).
The retry prompt MUST include the previous invalid output and the validator error
message — retrying with the same prompt produces the same failure.

---

## 10. Orchestrator Skill (User Action Required)

`skills/orchestrator/SKILL.md` must be rewritten by the user to match the new RAG-based
roster input. The harness no longer passes a static roster — it passes the top-K RAG
results in this format:

```
## Available agents (ranked by relevance)

1. researcher (score: 0.91)
   Researches topics using web search and document reading. Best for: fact-finding,
   summarising sources, due diligence. NOT for: code generation, system administration.

2. coder (score: 0.73)
   Writes, edits, and debugs code. Best for: implementing features, fixing bugs,
   writing tests. NOT for: research, data retrieval.

## Available skills (ranked by relevance)

1. web-research (score: 0.88)
   ...

## Available tools (ranked by relevance)

1. http_request (score: 0.65)
   ...
```

The orchestrator SKILL.md should instruct the LLM to:
- Use this ranked list to decide which agents/skills to include in the plan
- Output `{"kind":"direct","response":"..."}` for simple questions
- Output `{"kind":"plan","steps":[...]}` for multi-step tasks
- Always reference agents by their exact `name` field from the list above
- When no agent is above threshold, ask the user (do not fabricate an agent)

**The user must write/update `skills/orchestrator/SKILL.md`** — this is configuration,
not code. The harness loads whatever is there at runtime.

### Minimum viable template (ship this in Phase 4 so the system is bootable)

```markdown
---
name: orchestrator
description: "Plans and dispatches multi-agent work from a ranked RAG roster."
---

You are the orchestrator for the tengu-cluster harness. For every user message
you receive two inputs from the harness:

1. A ranked list of available agents, skills, and tools (retrieved by semantic
   similarity to the user's message). Each item has a score in [0,1].
2. The user's message, plus optionally the last N messages of the current session.

Decide between two outputs:

- If the message is a simple question you can answer directly without spawning
  any subagent, output `{"kind":"direct","response":"..."}`.
- Otherwise, output `{"kind":"plan","steps":[...]}` where each step picks an agent
  by its EXACT `name` field from the ranked list above. Set `depends_on` to the
  ids of prior steps whose output this step needs.

Rules:
- NEVER invent an agent name. If no agent is above score 0.6, respond with
  `{"kind":"direct","response":"<question asking the user to clarify or confirm
  the closest match>"}`.
- Keep plans minimal — if one step is enough, produce one step.
- Use `depends_on` only for true data dependencies; parallel steps finish faster.

Your output MUST validate against `plan_schema.json`. Invalid output is retried
up to 3 times with the validator error appended.
```

This template is intentionally short. Expand it as the system matures; keep it
file-based so no PR is needed.

---

## 11. Unknown Agent Fallback (C → B)

When RAG returns no agent above the similarity threshold (configurable, default 0.6):

**Step C** — Orchestrator LLM outputs a question to the user:
> "I don't have an agent for this task. The closest I have is `researcher`
> (confidence: 45%). Do you want me to try with it, or describe what kind
> of agent you need?"

**Step B** — After user responds and confirms:
- Orchestrator composes a generic agent config on the fly
- Base: closest-matching `.toml` spec
- Augmented with highest-scoring skills and tools from the RAG results
- Config used for this run only — NOT persisted as a new agent spec
- Unless user explicitly says "save this as a new agent" — then the runner writes
  `agents/<user-provided-name>.toml` and triggers re-indexing

---

## 12. What Gets Deleted

Remove these. Their functionality is replaced or deferred.

| Delete | Reason |
|--------|--------|
| `src/adapters/orchestrator/roster.rs` | Replaced by RAG |
| `src/adapters/orchestrator/wiring.rs` | Replaced by RAG + runner.rs |
| `sandboxes/aura/config.toml`, `sandboxes/storage-test/config.toml` | Migrated to `agents/*.toml` in Phase 2 |
| `src/adapters/eval_builder.rs` | Deferred → move to `tengu/ideas/` |
| `src/adapters/skill_lifecycle/evolve.rs` | Deferred → move to `tengu/ideas/` (path corrected — evolve.rs lives under `skill_lifecycle/`, NOT under `plugins/skill_lifecycle/`) |

> **Reconciled with repo, 2026-04-24 (revised):** verified against the actual
> checkout. `sandboxes/` DOES exist (contains `aura/` and `storage-test/`) — those
> two files ARE in scope for deletion in Phase 5 after migration. The paths
> `src/adapters/agent_builder.rs` and `src/adapters/task_builder.rs` do NOT
> exist — they were cleaned up before this document was written, so earlier
> drafts of this table were stale. The four paths above are the confirmed
> deletion set.

---

## 13. What Gets Kept (Do Not Touch)

These modules are correct. Touching them without a specific bug to fix is a doctrine
violation.

| Keep | Why |
|------|-----|
| `src/adapters/orchestrator/events.rs` | Progress streaming (TUI/Telegram) |
| `src/adapters/orchestrator/executor.rs` | DagExecutor — parallel DAG, retry, cancel |
| `src/adapters/orchestrator/planner.rs` | Planner trait — CHANGE the implementation, not the trait |
| `src/adapters/orchestrator/replan.rs` | drive() loop — already correct |
| `src/adapters/orchestrator/retry.rs` | Retry policy |
| `src/adapters/plugins/` (all) | Compiled-in tools unchanged |
| `src/adapters/plugins/skill_lifecycle/distill.rs` | skill_distill tool — keep, working |
| `src/adapters/plugins/skill_lifecycle/metrics.rs` | Keep for skill evals |
| `src/adapters/memory/` | Elevated to central RAG authority |
| `src/adapters/tui/` | Unchanged |
| `src/adapters/telegram_builder.rs` | Unchanged |
| `src/adapters/mcp/` | Unchanged |
| `src/types.rs`, `src/ports.rs`, `src/config.rs` | Unchanged |
| `src/adapters/skill_builder.rs` | Unchanged |
| `src/adapters/shell_executor.rs` | Unchanged |

### DagExecutor contract (critical — do not break)

```rust
// src/adapters/orchestrator/executor.rs
// Keep WorkerHandle trait exactly as-is:
#[async_trait]
pub trait WorkerHandle: Send + Sync {
    async fn run_step(&self, step: &Step, step_inputs: &str) -> anyhow::Result<String>;
}

// completed_outputs: HashMap<StepId, String>
// This is EXACT in-memory lookup. NOT replaced by RAG.
// Used for injecting prior step outputs into dependent steps.
// FuturesUnordered for parallel dispatch. Keep all of this.
```

### OrchestratorEvent enum (extended with one new variant)

Add `RagQueried` so the TUI and logs can show what the planner saw. Everything else
stays as-is.

```rust
// src/adapters/orchestrator/events.rs
pub enum OrchestratorEvent {
    PlanCreated     { plan: Plan },
    StepStarted     { step_id: StepId, agent: String },
    StepProgress    { step_id: StepId, chunk: String },
    StepFailed      { step_id: StepId, attempt: u32, error: String },
    StepExhausted   { step_id: StepId, final_error: String },
    StepSucceeded   { step_id: StepId, output: String },
    ReplanTriggered { reason: String },
    PlanCompleted   { final_response: String, cancelled: bool },

    // NEW — emitted before every planner LLM call, captures RAG input.
    // Lets the user see which agents/skills/tools the planner had in hand.
    RagQueried      {
        query:   String,
        results: Vec<(String, f32)>,   // (name, score), top_k
    },
}
pub type EventBus = broadcast::Sender<OrchestratorEvent>;
```

---

## 14. New Modules to Build

> **Framing note, 2026-04-24:** `src/adapters/memory/` already contains a working
> `QdrantVectorStore` plus a full memory manager/provider. The "RAG layer" below is
> best built as a thin facade on top of that, not as a parallel stack. Think
> "promote memory to central RAG" rather than "write RAG from scratch."

### `src/adapters/rag/mod.rs`

RAG facade. Public API:

```rust
pub struct RagStore { /* Qdrant client + config */ }

impl RagStore {
    pub async fn startup_index(&self, skills: &[Skill], agents: &[AgentSpec], tools: &[ToolDef]) -> Result<()>;
    pub async fn ttl_cleanup(&self) -> Result<u64>; // returns count deleted
    pub async fn search_registry(&self, query: &str, top_k: usize) -> Result<Vec<RagResult>>;
    pub async fn store_memory(&self, entry: MemoryEntry) -> Result<()>;
    pub async fn search_memory(&self, query: &str, top_k: usize) -> Result<Vec<RagResult>>;
}

pub struct RagResult {
    pub kind: RagKind,       // Skill | Agent | Tool
    pub name: String,
    pub description: String,
    pub score: f32,
    pub source_path: Option<String>,
}

pub struct MemoryEntry {
    pub kind: MemoryKind,    // Message | StepOutput | FileChunk
    pub session_id: String,
    pub step_id: Option<String>,
    pub content: String,
    pub created_at: u64,     // unix timestamp
}
```

### `src/adapters/rag/indexer.rs`

Startup indexer. Scans skills/, agents/, MCP tool list, compiled-in tool list. Diffs by
content_hash (sha256 of description). Only embeds changed or new entries.

### `src/adapters/rag/query.rs`

Thin wrapper over Qdrant search. Deserializes payload into `RagResult`.

### `src/adapters/rag/cleanup.rs`

TTL purge. Runs at startup before indexing.

### `src/adapters/runner.rs`

Implements `WorkerHandle` trait by spawning a subprocess. Loads agent spec from
`agents/<name>.toml`. Assembles system prompt. Writes stdin JSON. Reads stdout JSON.
Returns output as `anyhow::Result<String>`.

```rust
pub struct SubprocessRunner {
    pub rag: Arc<RagStore>,
    pub config: Arc<Config>,
}

#[async_trait]
impl WorkerHandle for SubprocessRunner {
    async fn run_step(&self, step: &Step, step_inputs: &str) -> anyhow::Result<String> {
        let spec = load_agent_spec(&step.agent)?;
        let ipc_input = build_ipc_input(&spec, &step.goal, step_inputs);
        let output = spawn_agent_subprocess(ipc_input, spec.timeout_secs).await?;
        // parse stdout JSON, return output.output on ok, bail! on failed
    }
}
```

### `src/adapters/agents/mod.rs`

Loads and validates `agents/*.toml`. Caches in memory after startup.

```rust
pub struct AgentSpec {
    pub name:         String,
    pub description:  String,
    pub model:        String,
    pub tools:        Vec<String>,
    pub skills:       Vec<String>,
    pub max_turns:    u32,
    pub timeout_secs: u64,
    pub sandbox:      Option<PathBuf>,
}
```

### Changes to `src/adapters/orchestrator/planner.rs`

The `Planner` trait stays unchanged. Change `OrchestratorAgentPlanner`:
- Replace `roster_md: String` (static) with `rag: Arc<RagStore>`
- In `plan()`: call `rag.search_registry(user_message, 20)`, format results into the
  ranked roster markdown block, build system prompt from `skills/orchestrator/SKILL.md`
- In `replan()`: additionally query `rag.search_memory(...)` for cross-plan context,
  inject as additional context block

### Changes to `src/main.rs`

Add `run-agent` subcommand that:
1. Reads stdin as JSON (`AgentIpcInput`)
2. Loads specified skills from disk
3. Assembles system prompt (base + skills + mandatory suffix)
4. Resolves tools from compiled-in registry filtered by `ipc_input.tools`
5. Runs LLM mini-loop
6. On `compress_and_store` call: stores to Qdrant, sets flag
7. Writes stdout JSON (`AgentIpcOutput`) and exits

---

## 15. Tool Access Control (Unchanged — Do Not Alter)

Two-layer enforcement stays exactly as-is:

1. **Allow-list**: agent spec declares `tools = [...]`. Runner registers only those tools.
   `compress_and_store` is always added implicitly (not listed in spec, always present).

2. **ToolScope**: per-agent filesystem and network scope. Enforced structurally by
   `tests/scope_lint.rs`. Every `Tool::execute` body must call `ctx.scope.check_*()` as
   its first logic line. If sandbox is defined in agent spec, that path is the workspace.
   If no sandbox, runner creates a temp dir.

---

## 16. Cold Start Fallback

On startup, if `tengu_registry` returns zero results for any query:

1. Log warning: `[tengu] RAG registry empty — falling back to sandbox config`
2. Load agent configs from `sandboxes/*/config.toml` (still present: `aura`,
   `storage-test`) — parse their `[agents.*]` blocks and use them as the static
   roster for this session only.
3. Background-index from `skills/` and `agents/` anyway; next session uses RAG.

This is a temporary compatibility shim, unchanged from v1 behaviour. Sandbox
directories are not deleted automatically — the user migrates at their own pace
via the steps below.

Migration steps (happens in Phase 2 of IMPLEMENTATION_PLAN.md):
1. For each `sandboxes/<name>/config.toml` → extract the `[agents.*]` blocks
   into `agents/<name>.toml` per §6's format.
2. Run `tengu` once — startup indexer picks up new files.
3. Verify: `tengu registry list --type agent` shows the migrated agents.
4. Delete `sandboxes/<name>/` directories in Phase 5.

---

## 17. Memory Lifecycle Summary

```
tengu_registry (permanent — never expires)
  Written: startup indexer
  Read: orchestrator planning, unknown agent fallback
  Deleted: only when source file is deleted (detected by missing source_path on next startup)

tengu_memory (TTL configurable — default 0 = never purge; set ttl_days > 0 to opt in)
  Two sub-collections to keep signal/noise apart:
    tengu_messages   ← raw user messages (high noise, conversational)
    tengu_outputs    ← compress_and_store summaries + file chunks (high signal)
  Written by:
    - User message handler       → tengu_messages (each user turn)
    - compress_and_store tool    → tengu_outputs  (step completion)
    - File input handler         → tengu_outputs  (user uploads, chunked)
  Read by:
    - Orchestrator on user message → tengu_messages, last N for same session_id
                                     (N default = 10, configurable), injected into
                                     system prompt as "recent dialogue" block
    - Orchestrator on replan       → tengu_outputs, fuzzy RAG top_k for planner context
  Purged: at startup, all entries older than the configured TTL
```

**Multi-turn dialogue is explicit, not fuzzy.** Each orchestrator turn loads the last
N messages for the current `session_id` by `created_at` ORDER BY desc, not by vector
similarity. Conversational coherence does not depend on embedding quality.

**Cross-plan recall** is fuzzy (vector search on `tengu_outputs` only — never on
`tengu_messages`, to avoid noise pollution). It is supplemental to the planner LLM —
hints, not hard data. The exact data (step output text) is always in
`completed_outputs: HashMap<StepId, String>` in DagExecutor — never go to Qdrant for that.

### Config

```toml
# config.toml
[memory]
ttl_days            = 0          # 0 = never purge (default, permanent memory).
                                 # Set >0 to enable startup sweep of older entries.
session_recent_n    = 10         # last-N messages reloaded each orchestrator turn
cross_plan_top_k    = 5          # fuzzy recall breadth during replan
embedding_model     = "openai/text-embedding-3-small"
```

---

## 18. Implementation Phases

Execute in order. Each phase is independently shippable (tests pass, binary builds).

### Phase 0 — Delete the dead weight

Delete files listed in §12. Move `eval_builder.rs` and `evolve.rs` to `tengu/ideas/`.
Fix compilation errors from removed modules. No new functionality yet.
**Acceptance**: `cargo build` passes, `cargo test` passes.

### Phase 1 — RAG layer

Build `src/adapters/rag/` (mod, indexer, query, cleanup). Wire to Qdrant client
(already available in `memory/` — extend or wrap it). Add startup sequence:
TTL cleanup → indexing → done.
**Acceptance**: `cargo test` for rag module passes. `tengu registry list` shows skills.

### Phase 2 — Agent specs

Build `src/adapters/agents/mod.rs`. Create 2-3 example `agents/*.toml` files matching
current sandbox configs. Wire startup indexer to read these.
**Acceptance**: `tengu registry list` shows agents. Descriptions searchable via `search_registry`.

### Phase 3 — Runner + IPC

Build `src/adapters/runner.rs` implementing `WorkerHandle`. Add `run-agent` subcommand
to `main.rs`. Wire `compress_and_store` to Qdrant write.
**Acceptance**: Manual test — spawn runner subprocess with a simple goal, confirm JSON output
and Qdrant write.

### Phase 4 — Orchestrator wiring

Change `OrchestratorAgentPlanner` to use RAG instead of static roster. Update
`channel_runtime.rs` (or wherever planner is constructed) to pass `RagStore`.
User action: rewrite `skills/orchestrator/SKILL.md` for RAG-based roster format.
**Acceptance**: Full end-to-end test — send user message via TUI, orchestrator retrieves
agents from RAG, produces plan, DagExecutor runs runner subprocess, compress_and_store
writes to Qdrant, final response delivered.

### Phase 5 — Polish and migration

Remove sandbox fallback code if no longer needed. Migrate remaining sandbox configs.
Wire unknown agent fallback (C → B). Add `tengu registry search <query>` CLI command.
**Acceptance**: All existing TUI and Telegram functionality works. No regressions.

---

## 19. Deferred — Do Not Implement Now

These are real ideas that were discussed but intentionally deferred to keep the redesign
scope manageable:

- **Skill evolution loop** (auto-improving skills, Karpathy-style eval loop)
  → Files: `tengu/ideas/auto-skill-research/`
- **Eval harness** (skill performance benchmarking)
  → Files: `tengu/ideas/eval/`
- **File watcher** (re-index on SKILL.md / agent spec change without restart)
  → Startup re-index is sufficient for now
- **Sandbox persistence for composed agents** (C→B result saved as permanent spec)
  → Manual save is sufficient for now
- **MCP tool creation by user** (user-defined MCP in config.toml)
  → MCP bridge already exists; user configures `config.toml` directly

---

## 20. Key Design Decisions — Rationale Log

These are closed decisions. Do not re-open them. If you disagree with the rationale, note
it in a comment but implement what is specified here.

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Orchestrator context growth | **Option B**: clean context before each new orchestrator prompt | Prevents unbounded context growth; orchestrator is stateless per turn |
| Who compresses subagent output | **Subagent itself** (no extra LLM call) | Zero added latency; LLM already knows what it did; model writes summary as final tool call |
| Cold start | **Fallback to sandbox configs** temporarily | Backwards compatibility; user migrates at their own pace |
| When to index new files | **At startup + when user provides files via channel** | RAG is the central authority; all user-provided content goes into tengu_memory immediately |
| Plan schema location | **File beside SKILL.md** (`plan_schema.json`) | User can read it; version-controlled; harness loads from disk |
| Agent spec format | **`.toml` files in `agents/`** | Simple, readable, file-based — no Rust changes to add an agent |
| Unknown agent handling | **C → B** | Ask user first (C), then compose generic agent (B); never silently use wrong agent |
| Sandbox for agents | **Optional in agent spec**; if absent use temp dir | Keeps stateless agents stateless; persistent agents declare their workspace |
| Tool access for orchestrator | **Orchestrator has no separate config** — it is the default user entry point | Orchestrator is not a peer agent; it loads SKILL.md and that's it |
| Skill/tool storage duration | **Skills, agents, tools = permanent**; conversations/outputs/files = 7 days | Registry is configuration; memory is ephemeral context |
| Event bus | **Keep** — `tokio::sync::broadcast` | TUI and Telegram both subscribe; subprocess model doesn't eliminate the need for streaming |
| MCP tools | **User configures `config.toml`** | MCP bridge already exists; no new infrastructure needed |
| Within-plan step dependencies | **Exact `HashMap<StepId, String>`** — NOT RAG | Correctness-critical; fuzzy search is wrong for "step A's exact output → step B's input" |
| compress_and_store enforcement | **Harness appends mandatory suffix** (Option C) | Skill-independent; harness-enforced; LLM cannot avoid it regardless of skill content |

---

## 21. What to Ask Vladimir

Before beginning implementation, confirm:

1. Is Qdrant already running and reachable? Check `config.toml` for the connection URL.
2. Is the default embedding model (`openai/text-embedding-3-small` — see `[memory]`
   block in §17) the right choice, or should it be a local Ollama model?
3. Are the two or three agent specs you want as the first `agents/*.toml` examples
   derived from anything, or should they be drafted from scratch against the §6 format?
4. Is the minimum-viable `skills/orchestrator/SKILL.md` template in §10 acceptable as
   the initial landing, or do you want to author one yourself before Phase 4 lands?

If no answer, proceed with the §17 defaults: Qdrant at `localhost:6333`,
`openai/text-embedding-3-small`, and land the §10 template verbatim in Phase 4.

---

## Appendix A — Files to Read Before Writing Code

```bash
# Understand what exists and what to keep:
cat src/adapters/orchestrator/executor.rs    # DagExecutor — keep as-is
cat src/adapters/orchestrator/events.rs      # OrchestratorEvent — keep as-is
cat src/adapters/orchestrator/planner.rs     # Planner trait — change implementation
cat src/adapters/orchestrator/replan.rs      # drive() — keep as-is
cat src/adapters/memory/mod.rs               # Existing memory layer — extend for RAG
cat src/adapters/plugins/skill_lifecycle/distill.rs  # compress_and_store predecessor — keep
cat src/config.rs                            # Config struct — understand Qdrant config
cat src/main.rs                              # Entry point — add run-agent subcommand here
```

## Appendix B — Architecture Doctrine File

The authoritative architecture reference is `docs/architecture-v2.md` in this repository.
This REDESIGN.md is the implementation brief. If there is a conflict between the two,
`docs/architecture-v2.md` is the doctrine and this document provides the implementation
detail. Neither supersedes the other — they are complementary.

---

*This document was written 2026-04-24 following a full architecture session with Vladimir.*
*All decisions in §20 were made collaboratively and confirmed by the user.*
*Do not ask the user to re-confirm these decisions — implement them.*
