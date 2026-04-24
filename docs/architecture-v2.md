# Architecture v2

> This document supersedes `docs/architecture.md`. Every PR is reviewed against it.
> Every phase spec references it. If code and this document conflict, this document wins.

---

## Doctrine

Three principles. Violating any of them blocks the PR.

### 1. LLM is the heart

It consumes tokens and emits tokens. It has no behaviour of its own. Everything that looks like "the agent decided X" is really "the context instructed the LLM and the LLM produced X." The Rust core never hard-codes behaviour that belongs to the model.

### 2. RAG is the brain

Everything the system knows lives in the RAG store: skill descriptions, agent specs, tool descriptions, user communications, step outputs. The orchestrator never loads everything into context — it queries the RAG store for what is relevant to the current task and loads only that. Context stays small by design.

The corollary: if something needs to influence agent behaviour, it must be in the RAG store or in a skill body. There is no other path. No hidden Rust structs encoding policy.

### 3. Tools and MCP are the hands

Tools are the only way any LLM touches the world. Compiled-in tools (http, filesystem, crypto, memory) and MCP-served tools are equal citizens — from the LLM's perspective they are all `{name, description, parameters}`. From the harness perspective, execution paths differ but the interface does not.

### The composability corollary

Skills compose agents. Agents compose plans. Plans compose the response to the user. Nothing is hardwired. A user can change what an agent does by editing a `.toml` or a `SKILL.md`. They do not file a PR. If behaviour requires a PR to change, it is a doctrine violation.

---

## System Flow

```
Startup
  ├── scan skills/, agents/
  ├── connect to each MCP server → tools/list
  ├── embed descriptions → Qdrant (tengu_registry, permanent)
  ├── TTL cleanup: delete from tengu_memory older than ttl_days (0 = never)
  └── if tengu_registry empty → cold-start fallback (see §Cold Start)

User message arrives (TUI / Telegram)
  │
  ├── embed message → store in tengu_memory (ttl_days if > 0, else permanent)
  │
  ├── Orchestrator loads:
  │     skills/orchestrator/SKILL.md       ← system prompt
  │     skills/orchestrator/plan_schema.json ← plan output contract
  │
  ├── Orchestrator calls RAG:
  │     query = embed(user_message)
  │     results = qdrant.search(tengu_registry, query, top_k=20)
  │     → ranked list of agents + skills + tools by cosine similarity
  │
  ├── If no agent above threshold:
  │     C → ask user ("closest match is X, want me to use it?")
  │     user confirms → B → compose generic agent from top matches
  │
  ├── Orchestrator LLM call:
  │     input  = [system_prompt, roster_of_retrieved_items, user_message]
  │     output = PlannerVerdict: Direct | Plan (validated against plan_schema.json)
  │
  ├── If Direct → respond immediately, done.
  │
  └── If Plan → DagExecutor:
        for each ready step (dependencies satisfied), tokio::spawn:
          │
          └── Runner:
                input (stdin JSON):
                  { goal, agent_name, model, tools: [...], skills: [...],
                    max_turns, sandbox? }
                assemble system prompt:
                  base_agent_template
                  + body of each skill (loaded from disk, three-tier)
                  + mandatory suffix: "When done, call compress_and_store(summary)."
                start LLM mini-loop:
                  tools available = resolved compiled-in tools + MCP tools from spec
                  workspace = sandbox path (if spec.sandbox) OR temp dir (default)
                  run until: compress_and_store called OR max_turns reached OR error
                on compress_and_store(summary):
                  write summary to tengu_outputs (Qdrant)
                exit, write stdout JSON:
                  { status: "ok"|"failed", output: "<full text>", summary: "<compressed>" }

        DagExecutor collects:
          completed_outputs: HashMap<StepId, String>  ← raw output, exact, in-memory
          (used for within-plan step dependency injection — not fuzzy)

        On step failure:
          NeedsReplan → orchestrator recalls tengu_outputs for relevant context
                      → replan(user_message, prior_plan, failed_step, error)
                      → repeat (max N replans, configured per orchestrator skill)

        On PlanCompleted:
          broadcast OrchestratorEvent → TUI spinner / Telegram message
          return final response to user
```

---

## Directory Layout

```
<workspace>/
│
├── skills/
│   ├── orchestrator/
│   │   ├── SKILL.md            ← orchestrator system prompt (user-editable)
│   │   └── plan_schema.json    ← plan JSON schema (validated by harness)
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
    Tools = compiled-in plugins + MCP servers configured in config.toml
```

The orchestrator has no agent spec file. It is the entry point — always running, always loaded. It is not a peer agent.

---

## RAG — Central Authority

### What gets indexed

| Source | Collection | TTL | Trigger |
|--------|-----------|-----|---------|
| `skills/*/SKILL.md` (frontmatter description) | `tengu_registry` | permanent | startup + file added |
| `agents/*.toml` (description field) | `tengu_registry` | permanent | startup + file added |
| MCP tool descriptions (from `tools/list`) | `tengu_registry` | permanent | startup |
| Compiled-in tool descriptions | `tengu_registry` | permanent | startup (hardcoded) |
| User messages | `tengu_messages` | configurable | each user turn |
| Subagent `compress_and_store` summaries | `tengu_outputs` | configurable | step completion |
| User-provided file contents (chunked) | `tengu_outputs` | configurable | file input event |

### Qdrant collections

```
tengu_registry   skills, agents, tools
  payload:
    type:         "skill" | "agent" | "tool"
    name:         string
    source_path:  string      ← for deduplication
    content_hash: string      ← sha256 of source; skip re-embed if unchanged
    description:  string      ← what was embedded

tengu_messages   raw user turns (noisy, conversational)
tengu_outputs    compress_and_store summaries + file chunks (signal)
  payload (both):
    type:         "message" | "step_output" | "file_chunk"
    session_id:   string
    step_id:      string      ← for step outputs
    created_at:   unix timestamp
    content:      string      ← what was embedded
```

### Startup indexer behaviour

```
for each item in (skills + agents + MCP tools + compiled tools):
  hash = sha256(description_text)
  existing = qdrant.scroll(filter: source_path == item.path)
  if existing and existing.payload.content_hash == hash:
    skip   ← already up to date
  else:
    qdrant.upsert(embed(description), {type, name, source_path, content_hash})
```

No file watcher. No daemon. Re-index runs at startup and when the user provides new files via the channel. Simple and predictable.

### TTL cleanup

Runs at startup before indexing. Controlled by `[memory] ttl_days` in `config.toml`. Default is `0` — never purge (MemPalace-style permanent memory). A positive value enables sweep:

```rust
if cfg.memory.ttl_days > 0 {
    let cutoff = Utc::now() - Duration::days(cfg.memory.ttl_days as i64);
    qdrant.delete(
        collection: "tengu_messages",
        filter: created_at < cutoff
    );
    qdrant.delete(
        collection: "tengu_outputs",
        filter: created_at < cutoff
    );
}
```

---

## Orchestrator

The orchestrator is not a peer agent. It is the harness entry point for every user message.

**What it owns:**
- Loading `skills/orchestrator/SKILL.md` as system prompt
- Calling RAG to retrieve relevant agents/skills/tools
- Producing a plan validated against `plan_schema.json`
- Driving the `replan` outer loop
- Broadcasting `OrchestratorEvent` for progress streaming

**What it does not own:**
- Executing steps — that is the Runner
- Running tool calls — that is the tool registry
- Deciding when a step is "done" — that is the subagent's `compress_and_store` call

### Plan schema contract

The orchestrator LLM must output JSON matching `plan_schema.json`. The harness validates strictly. On parse failure the harness retries the planning call (max 3 retries, logged) with the previous output and validator error appended. The schema is version-controlled beside `SKILL.md`.

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

### Unknown agent fallback (C → B)

When RAG returns no agent above the similarity threshold:

1. **C** — Orchestrator asks user: *"I don't have an agent for this task. The closest I have is `<name>` (confidence: X%). Do you want me to try with it, or describe what kind of agent you need?"*
2. User responds. If user confirms or refines:
3. **B** — Orchestrator composes a generic agent config on the fly using the closest-matching agent spec as base, augmented with the highest-scoring skills and tools from the RAG results. Used for this run only; not persisted unless the user explicitly asks.

---

## Agent Specs

Each agent is a `.toml` file in `agents/`. No subdirectory. No workspace directory.

### Format

```toml
name        = "researcher"
description = "Researches topics using web search and document reading. \
               Best for: fact-finding, summarising sources, due diligence."
model       = "openai/gpt-4o"          # any OpenRouter slug
tools       = ["http_request", "read_file", "remember"]
skills      = ["web-research", "summarizer"]
max_turns   = 20
timeout_secs = 180

# Optional: if set, this path is the agent's persistent workspace.
# If not set, the runner creates a temp dir and deletes it after the step.
# sandbox = "workspaces/researcher"
```

The `description` field is what gets embedded into `tengu_registry`. Write it to be informative for semantic search — what tasks this agent handles, what it is NOT good for.

### System prompt assembly (runtime)

The runner never uses a static system prompt. It assembles one per invocation:

```
[base_agent_template]          ← hardcoded in runner.rs, not user-editable
[skill body 1]                 ← full SKILL.md body of skills[0]
[skill body 2]                 ← full SKILL.md body of skills[1]
[...]
[mandatory suffix]             ← hardcoded in runner.rs, not user-editable:
                                  "When you have completed your task, your
                                   final action MUST be to call
                                   compress_and_store(summary) with a concise
                                   summary of what you accomplished and found."
```

Skills are loaded from disk in the order declared in the agent spec. The runner respects the three-tier skill loading order (managed → workspace-dotdir → workspace-root); later tiers shadow earlier ones.

---

## Runner and IPC

The runner is a thin Rust module (`src/adapters/runner.rs`) that bridges the orchestrator to a subagent subprocess.

### Subprocess spawn

```rust
let child = Command::new(current_exe())
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())        // piped — runner forwards to StepProgress
    .env("TENGU_AGENT_IPC", "1")   // re-entry guard; main.rs refuses run-agent without it
    .arg("run-agent")              // dedicated subcommand
    .spawn()?;
```

Each subagent is an isolated child process. A panic in the child does not crash the orchestrator. The runner monitors the child via `child.wait()` with a timeout.

### Input (stdin JSON)

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

`compress_and_store` is NOT in this list — it is appended implicitly by the runner for every invocation. The runner computes `effective_tools = spec.tools ∩ ipc.tools ∪ {compress_and_store}`.

### Output (stdout JSON)

```json
{
  "status":  "ok",
  "output":  "<full agent output, may be long>",
  "summary": "<compressed summary written to Qdrant by compress_and_store>"
}
```

On failure:
```json
{
  "status": "failed",
  "error":  "max_turns exceeded without calling compress_and_store",
  "output": "<partial output up to failure>"
}
```

Exit code 0 = status ok or graceful failure (harness reads JSON). Non-zero exit = subprocess crash (harness treats as hard failure, triggers retry via DagExecutor policy).

### `compress_and_store` tool

A compiled-in tool, always registered for every subagent invocation. Not optional. Not user-configurable.

```json
{
  "name": "compress_and_store",
  "description": "Store a compressed summary of your completed work. Call this as your FINAL action when the task is done. Do not call it mid-task.",
  "parameters": {
    "summary": "string — concise summary of what you accomplished, found, or produced"
  }
}
```

On call: writes `{summary}` to `tengu_outputs` (Qdrant) with `type: "step_output"`, `session_id`, `step_id`, `created_at`. Then sets an internal flag that the runner checks — if this flag is set when the mini-loop ends, the runner knows the step completed cleanly.

---

## DagExecutor (unchanged)

`src/adapters/orchestrator/executor.rs` is correct and stays as-is.

Key properties preserved:
- `completed_outputs: HashMap<StepId, String>` — exact in-memory lookup for within-plan step dependencies. Not RAG. Not fuzzy. Always reliable.
- Parallel dispatch via `FuturesUnordered` — steps with no unmet dependencies run concurrently.
- `RetryPolicy` — configurable per-step retry with backoff.
- `cancel: Arc<AtomicBool>` — user-initiated stop at any dispatch boundary.
- `OrchestratorEvent` broadcast — progress streaming to TUI and Telegram subscribers.

The `WorkerHandle` trait is the only interface the DagExecutor cares about. The runner implements it.

```rust
#[async_trait]
pub trait WorkerHandle: Send + Sync {
    async fn run_step(&self, step: &Step, step_inputs: &str) -> anyhow::Result<String>;
}
```

One event added to support debugging: `OrchestratorEvent::RagQueried { query, results: Vec<(String, f32)> }` is broadcast before every planner LLM call so the TUI can show what the planner saw.

---

## Tools

### Compiled-in plugins

Implemented as Rust structs in `src/adapters/plugins/`. These are the base tools available to all agents unless restricted by the agent spec's `tools` list.

| Plugin | Tools |
|--------|-------|
| `workspace` | `read_file`, `list_directory`, `write_file`, `run_command` |
| `http` | `http_request` |
| `crypto` | `sign_and_send_transaction`, `sign_message`, `get_wallet_address`, `abi_encode`, `hex_to_uint256` |
| `cache` | `shared_cache` |
| `memory` | `remember`, `persistent_store` |
| `skill_lifecycle` | `compress_and_store`, `skill_distill` |
| `mcp` | dynamic — one proxy tool per remote MCP tool |

At startup, compiled-in tool descriptions are embedded and written to `tengu_registry`. This happens once; content hash prevents re-embedding on subsequent startups.

### MCP tools

Configured in `config.toml` under `[[mcp_servers]]`. At startup the harness connects to each server, calls `tools/list`, embeds each tool description, writes to `tengu_registry`. MCP tools are proxied at call time by the existing `mcp/client.rs`.

From the orchestrator's perspective: compiled-in tools and MCP tools are identical — both appear as `{name, description}` entries in RAG query results. The distinction is execution-path only, invisible to the LLM.

### Tool access control (unchanged)

Two-layer enforcement, unchanged from v1:

1. **Allow-list**: agent spec declares `tools = [...]`. Runner registers only those tools.
2. **ToolScope**: per-agent scope (filesystem roots, network hosts). Enforced structurally by `tests/scope_lint.rs`. Every `Tool::execute` body calls `ctx.scope.check_*()` as its first logic line.

---

## Skills

Three-tier loading order, unchanged:

| Tier | Path | Shadow |
|------|------|--------|
| Managed | `~/.tengu/skills/` | lowest priority |
| Workspace dotdir | `<workspace>/.tengu/skills/` | shadows managed |
| Workspace root | `<workspace>/skills/` | shadows both |

SKILL.md frontmatter controls gating:

```yaml
---
name: web-research
description: "Research topics using HTTP requests and document reading."
requires_bins: ["curl"]
requires_env: []
os: ["linux", "macos"]
---
```

The `description` field is indexed in `tengu_registry`. Write it for semantic search.

Shell skills (classic): create named tools with execution templates, injected inline into the tool list. Unchanged.

Documentation skills (frontmatter-only): body loaded on demand, injected into agent system prompt by runner. Unchanged.

---

## Memory and RAG Lifecycle

```
tengu_registry (permanent)
  ├── Never expires
  ├── Updated on startup (diff by content_hash)
  ├── Updated when user adds new agent spec or skill
  └── Entry deleted only when source file is deleted

tengu_messages (TTL configurable — default 0 = never)
  └── Raw user turns, kept for multi-turn dialogue coherence
      Multi-turn is deterministic: last-N by session_id + timestamp,
      not by vector similarity — embedding noise doesn't matter.

tengu_outputs (TTL configurable — default 0 = never)
  ├── compress_and_store summaries (signal)
  ├── Chunked file content
  └── Written by:
        - compress_and_store tool (step completion)
        - file input handler
```

### Cross-plan recall

When the orchestrator replans after a step failure, it queries `tengu_outputs` for relevant context:

```
query = embed(user_message + " " + failed_step.goal + " " + error)
results = qdrant.search("tengu_outputs", query, top_k=cross_plan_top_k)
→ inject into replan() call as additional context
```

Messages are excluded from this query to keep signal and noise separated.

This recall is fuzzy. Correctness of direct step dependencies uses the `completed_outputs` HashMap, not this query.

---

## What Gets Deleted

These modules are removed. Their functionality is either replaced by the new design or deferred to a future phase.

| Deleted | Reason |
|---------|--------|
| `orchestrator/roster.rs` | Replaced by RAG agent discovery |
| `orchestrator/wiring.rs` | Replaced by RAG-based plan assembly |
| `sandboxes/aura/config.toml`, `sandboxes/storage-test/config.toml` | Migrated to `agents/*.toml` in Phase 2 |
| `eval_builder.rs` | Deferred — moved to `tengu/ideas/` |
| `skill_lifecycle/evolve.rs` | Deferred — moved to `tengu/ideas/` |

These modules are **kept**:

| Kept | Why |
|------|-----|
| `orchestrator/events.rs` | Progress streaming (TUI/Telegram); add `RagQueried` variant |
| `orchestrator/executor.rs` | DagExecutor — already correct |
| `orchestrator/planner.rs` | Planner trait + OrchestratorAgentPlanner |
| `orchestrator/replan.rs` | Outer drive loop — already correct |
| `orchestrator/retry.rs` | Retry policy |
| `plugins/` (all tool plugins) | Unchanged |
| `skill_lifecycle/distill.rs` | skill_distill tool — keep, useful |
| `skill_lifecycle/metrics.rs` | Metric definitions — keep for skill evals |
| `memory/` (manager, provider, fencing, writer) | Elevated to central RAG |
| `tui/` | Unchanged |
| `telegram_builder.rs` | Unchanged |
| `mcp/` | Unchanged |
| `types.rs`, `ports.rs`, `config.rs` | Unchanged (config.rs extended in Phase 0) |

---

## Cold Start and Migration

### Cold start (empty Qdrant)

On startup, if `tengu_registry` returns zero results for any query:

1. Log a warning: `[tengu] RAG registry empty — falling back to sandbox config`
2. Load agent configs from `sandboxes/*/config.toml` (legacy path)
3. Use static roster for this session
4. Index from `skills/` and `agents/` proceeds in background; next session uses RAG

The sandbox fallback is a temporary compatibility shim. Once `agents/*.toml` files exist and are indexed, the cold-start path is never hit again. Sandbox configs are not deleted automatically — the user migrates at their own pace.

### Migration path from v1

1. For each `sandboxes/<name>/config.toml`, create `agents/<name>.toml` with equivalent fields.
2. Move any workspace-specific SKILL.md files from sandbox directories to `skills/`.
3. Run `tengu` once — startup indexer picks up new agents and skills.
4. Verify RAG is populated: `tengu registry list`.
5. Delete sandbox directories (Phase 5 of the implementation plan).

---

## Module Map (v2)

### New

| Module | Purpose |
|--------|---------|
| `rag/mod.rs` | RAG facade — startup indexer, query, TTL cleanup |
| `rag/indexer.rs` | Scans skills/, agents/, MCP tools; diffs by content hash |
| `rag/query.rs` | `search(collection, query, top_k)` → ranked results |
| `rag/cleanup.rs` | TTL purge on startup |
| `runner.rs` | Spawns subagent subprocess; assembles system prompt; handles IPC |
| `agents/mod.rs` | Loads `agents/*.toml` specs; validates fields |

### Changed

| Module | Change |
|--------|--------|
| `memory/mod.rs` | Elevated to central RAG — `tengu_registry` + `tengu_messages` + `tengu_outputs` |
| `orchestrator/planner.rs` | Calls RAG for roster instead of static `roster_md`; flag-gated `engine` field |
| `orchestrator/wiring.rs` | Deleted — replaced by `rag/` + `runner.rs` |
| `channel_runtime.rs` | Simplified — no sandbox resolution, no roster building |
| `main.rs` | Adds `run-agent` subcommand for subprocess mode |

### Unchanged

`orchestrator/executor.rs`, `orchestrator/events.rs` (extended only), `orchestrator/replan.rs`, `orchestrator/retry.rs`, `plugins/` (all), `tui/`, `telegram_builder.rs`, `mcp/`, `types.rs`, `ports.rs`, `config.rs` (extended only), `skill_builder.rs`, `shell_executor.rs`, `secret_builder.rs`, `scaffold.rs`

---

## Related

- [[configuration]] — config.toml reference (updated for v2 extensions)
- [[skills]] — skill format and loading (mostly unchanged)
- [[engine-backends]] — OpenRouter and Claude Code engines (unchanged)
- [[mcp-bridge]] — MCP tool bridge (unchanged)
- `tengu/ideas/` — deferred features (skill evolution, eval harness)
