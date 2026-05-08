# Context Management in Tengu-Cluster — the canonical reference

> **Read this first** when touching anything that affects what an LLM sees.
> This is the unified map. Two focused companion docs cover slices in more
> depth: `context-cutting-flow-2026-04-27.{svg,html}` (in-turn cuts) and
> `compression-flow-2026-04-27.{md,svg}` (durable step summarisation).

---

## TL;DR

Tengu shapes LLM context across **seven layers**, with about **27 distinct
mechanisms** (the original 25 + the metrics observability layer added
2026-04-28). None of them is LLM-driven — there is no "summarise the
conversation" pass. Everything is mechanical: char/token caps, sliding
windows, threshold-triggered concatenation, first-line clipping. The
pieces compose; no single entry point owns "context management".

**Layer 8 (added 2026-04-28) is observability, not shaping.** It records
what every other layer produced — token counts, latency, per-context-layer
attribution — without changing the bytes. Read it before debugging "why
is this turn so big": the `metrics` info line tells you which layer ran
heaviest before you go fishing through code.

The closest tengu has to Claude Code's `/compact` is a **two-mechanism
combo**: `maybe_compact_flow` (Layer 1, replaces older messages with a
truncated concatenation) plus `compact_tool_result` (Layer 3, rewrites
prior-round tool results to first-line-only mid-loop).

---

## The seven layers, top-down

```
┌─────────────────────────────────────────────────────────────────┐
│ Layer 0  Token primitives        token.rs                        │
│ Layer 1  Per-flow lifecycle      flow_builder.rs, chat_builder.rs│
│ Layer 2  Per-turn assembly       prompt_budget.rs, chat_builder  │
│ Layer 3  Inner tool loop         engine_builder.rs               │
│ Layer 4  MCP bridge              claude_code_engine.rs           │
│ Layer 5  Subagent IPC            runner.rs, main.rs::run_agent   │
│ Layer 6  RAG storage hygiene     rag/cleanup.rs, rag/indexer.rs  │
│ Layer 7  File chunking           plugins/memory/persistent_store │
│ Layer 8  Observability           metrics.rs (added 2026-04-28)   │
└─────────────────────────────────────────────────────────────────┘
```

A user message hits Layers 1→2→3 (potentially →4) on the planner-side
turn, then Layers 5→3 (→4) on each subagent step. Layers 0, 6, 7 are
infrastructure consumed by the others. **Layer 8 is orthogonal** — it
observes the output of layers 2/3/5 (LLM calls) and the embedder, then
emits records onto a process-global broadcast bus + tracing. It never
mutates a prompt.

---

## Layer 0 — Token estimation primitives

`src/adapters/token.rs`

| Helper | What | Used by |
|---|---|---|
| `estimate_tokens_approx(text)` | `text.len() / 4` heuristic | Everywhere a token count is needed |
| `estimate_tokens_approx_min1(text)` | Same, but floors to 1 | Per-message token math |
| `truncate_at_boundary(s, max_chars)` | UTF-8-safe slice point | Builders of truncation strings |
| `truncate_with_suffix(s, max, suffix)` | Truncate + append marker only when actually clipped | `truncate_tool_result`, history truncation |

The `~4 chars/token` heuristic is intentionally crude — it underestimates
for code-heavy strings and overestimates for prose, but errs on the side
of leaving headroom. No tokeniser is invoked.

---

## Layer 1 — Per-flow lifecycle (the auto-compact layer)

`src/adapters/flow_builder.rs`, called from
`chat_builder.rs::ChatRuntimeService::process_user_text`.

A "flow" is a conversation scoped by `flow.scope` (one of `main`,
`per-group`, `per-pipe-sender`, anything else). Each new user message
runs through three flow-level guards **before** the per-turn assembly:

### 1. `enforce_history_turn_limit` (`flow_builder.rs:61`)

Counts back N user turns from the end and drains everything before. Per-scope defaults:

| scope | default `max_history_turns` |
|---|---|
| `main` | 40 |
| `per-group` | 30 |
| `per-pipe-sender` | 25 |
| (other) | 20 |

Override in `agents/<name>.toml::[flow] max_history_turns = N`.

### 2. `maybe_compact_flow` (`flow_builder.rs:165`)  — *the auto-compact*

Runs after every push. If `flow_token_usage >= threshold_tokens`:
1. Find split index that keeps the most recent `keep_turns` user turns.
2. Concatenate older messages with `[role] content` lines.
3. Truncate to `summary_max_tokens` via `truncate_to_token_budget`.
4. Replace the prefix with a single synthetic Assistant message:
   `[Flow compaction summary]\n<truncated>\n\n[compacted_messages=N, phase=...]`

The "summary" is a **truncated concatenation, not an LLM summary**. That
is a deliberate choice — Layer 2's sliding window catches whatever Layer
1 missed, and bringing the LLM in here would cost a turn.

`FlowCompactionPolicy` defaults (per scope):

| scope | `compaction_threshold_ratio` | `compaction_keep_turns` |
|---|---|---|
| `main` | 0.88 | 60 |
| `per-group` | 0.86 | 40 |
| `per-pipe-sender` | 0.84 | 32 |
| (other) | 0.82 | 24 |

`summary_max_tokens` default: `15%` of max input budget, clamped to
`[128, max_input_budget/3]` and absolute `[256, 4096]`.

### 3. Hard stop on `max_tokens_per_flow`

`chat_builder.rs:309` — if `state.flow_token_usage >= max_tokens_per_flow`
(default 100 000) **after** Layer 1 compaction, the turn returns immediately
with a system notice: *"Flow token limit reached. Use /reset to start a
new session."*

---

## Layer 2 — Per-turn assembly

`chat_builder.rs::process_user_text` (continued) +
`prompt_budget.rs`.

### 4. Budget arithmetic (`prompt_budget.rs:52-91`)

```
reserved_output  = clamp(max(output_cap + output_cap/4, ctx_window/50))
total_input      = min(ctx_window - reserved_output, remaining_flow_tokens)
base_input       = total_input - estimate(system_prompt)
history_budget   = base_input - estimate(memory_block)
```

This is what makes the model's input fit — it's a chain of subtractions
from the model's context window.

### 5. `assemble_recent_history` (`prompt_budget.rs:8`)

Newest contiguous suffix of `state.messages` whose token estimate fits
`history_budget`. Hard ceiling of **20 messages** even if budget allows
more (`MAX_HISTORY_MESSAGES = 20`).

### 6. Memory recall block (Qdrant)

Driven by `MemoryConfig`. Default knobs:

| field | default | what |
|---|---|---|
| `max_recall_entries` | 5 | top-K results returned |
| `max_recall_tokens` | 600 | total budget for the recall block |
| `session_recent_n` | 10 | recent messages from same session, deterministic (no vector search) |
| `cross_session_msg_top_k` | 0 | semantic match across sessions; 0 = disabled |
| `cross_plan_top_k` | 5 | recall over `tengu_outputs` (planner replan path) |

The block is fenced via `fencing.rs::build_memory_context_block` — wrapped
in `<memory-context>...</memory-context>` with a system note instructing
the model not to treat the contents as user input. ~50 token overhead
per recall block.

### 7. Grounding nudge + `suppress_grounding_nudge`

`chat_builder.rs:248`. When the user message contains *"last", "latest",
"most recent", "newest", "previous", "recently", "before that"*, the
chat layer prepends a system message:

> "For questions about last/latest/most recent history, do not rely on
> recalled memory summaries. Verify against the current conversation and
> available tools/workspace state before answering. If you cannot
> verify, say so clearly."

When triggered, **memory recall is also skipped** for that turn —
recalled summaries from older turns would compete with the freshness
instruction.

The planner-side LLM call sets `suppress_grounding_nudge = true` so the
nudge doesn't compete with the SKILL.md "emit JSON only" instruction.
This is set inside `channel_runtime::run_turn_with_system:1038`.

### 8. Skill body assembly

`skill_builder.rs:1120-1150`. Three nested truncations:

| knob | default | what |
|---|---|---|
| `max_skill_context_tokens` | 16 000 | total skill bodies in system prompt |
| `max_file_tokens` | 2 000 | per-file cap inside a skill |
| (each truncation) | uses `truncate_to_token_budget` | appends `\n\n[truncated]` |

No dedup or per-skill-count cap — selection happens upstream.

---

## Layer 3 — Inner tool-calling loop

`engine_builder.rs::collect_engine_response`. The **per-turn**
context-cutting layer. See `context-cutting-flow-2026-04-27.html` for
the interactive walkthrough.

### 9. `truncate_tool_result` (`engine_builder.rs:878`)

Each tool result char-capped at `max_tool_result_chars` (default
**300 000**) when first inserted into messages. Footer:
`[truncated — showing X of Y chars]`. UTF-8 boundary safe.

### 10. `compact_tool_result` mid-loop (`engine_builder.rs:855`)  — *the closest to /compact*

After `round >= 1`, walks `messages[..compact_cutoff]`. Every
`Role::Tool` message in that prefix gets rewritten in place to its first
line, capped at `compact_result_limit` (default **200 chars**).

```rust
if content.len() <= limit { return content }
let first_line = content.lines().next().unwrap_or(content);
if first_line.len() <= limit { return first_line }
format!("{}…", &first_line[..safe_boundary(limit)])
```

Rationale (from the inline comment): tool results "typically put key
info on the first line" — tx hashes, IDs, status — so keeping the first
line preserves working memory cheaply.

### 11. `[OUTPUT_TRUNCATED]` auto-continue (`engine_builder.rs:637`)

When the model's response ends with the sentinel, the engine appends
`"Your output was truncated. Continue from where you left off."` as a
user turn and re-rolls. Extends, doesn't cut.

### 12. `max_tool_rounds` hard stop (default **70**)

After 70 rounds the loop exits and forces one final `tools=[]` engine
turn at `engine_builder.rs:727`.

### 13. `token_budget` mid-loop early-exit

If a per-flow token budget is passed in (it isn't on the chat path
today, but is on the eval path) and `total_input_delta + total_output_delta > budget`,
the loop exits with a warn log.

---

## Layer 4 — MCP bridge

`claude_code_engine.rs`.

### 14. `max_mcp_result_chars` (`claude_code_engine.rs:520`)

Caps tool results returned to the Claude Code CLI through the MCP
bridge. Default **50 000 chars / ~12.5K tokens**, env-overridable
through `TENGU_BRIDGE_MAX_RESULT_CHARS`.

This exists because the **Claude Code CLI engine has no in-loop
compaction at all**. Without this cap, bridge-routed tool results would
leak unbounded context into Claude. Layer 3's `truncate_tool_result` and
`compact_tool_result` only run on the OpenRouter path.

---

## Layer 5 — Subagent IPC

`runner.rs`, `main.rs::run_agent_subprocess`, plus the
`compress_and_store` protocol. See `compression-flow-2026-04-27.svg` for
the picture.

### 15. Subprocess gets fresh context

The child `tengu run-agent` process starts with **empty `messages`** —
it never inherits the parent's chat history. The IPC payload
(`AgentIpcInput`) carries only `goal`, `agent_name`, `model`, `tools`,
`skills`, `session_id`, `step_id`, `sandbox_config`. By design — keeps
subagents focused and prevents accidental context leakage between
steps.

### 16. `compress_and_store` durable summary

The harness-enforced "step is done" signal. Implicitly appended to every
subagent's tool list. The model calls it with a `summary` string; that
string is embedded and persisted to Qdrant `tengu_outputs` keyed by
`session_id` + `step_id`. Two dispatch paths converge on the same
`write_summary`:

| Path | Trigger |
|---|---|
| **A — Out-of-band** | OpenRouter subagents — `main.rs:821` intercepts the call before the executor runs |
| **B — Plugin** | Claude Code subagents via MCP bridge — `CompressAndStoreTool::execute` |

### 17. Phase 5c middle-ground protocol (`main.rs:900`)

| compress_called? | final_text non-empty? | Verdict |
|---|---|---|
| ✓ | * | **Ok** — canonical good path |
| ✗ | ✓ | **Ok** — graceful (warn logged, summary = final_text) |
| ✗ | ✗ | **Failed** — DagExecutor retries / replans |

### 18. Subprocess `max_turns` (default **20**)

`AgentIpcInput.max_turns`. Hard cap on the subagent's mini-loop. Hits
the `compress_and_store` warn path on exhaust.

### 19. Cross-plan recall (planner replan side)

`RagPlanner::replan` calls `search_memory(query, OUTPUTS_COLLECTION)`
to surface past step summaries to the planner LLM. This is the entire
point of writing to `tengu_outputs` — the planner's later turns can
recall what subagents already produced. Capped by `cross_plan_top_k`
(default 5).

---

## Layer 6 — RAG storage hygiene

`rag/cleanup.rs`, `rag/indexer.rs`.

### 20. `ttl_cleanup` (Phase 6.3)

Qdrant filter-based delete over `tengu_messages` and `tengu_outputs`
where `rag_created_at < (now - ttl_days * 86_400)`. Trigger:
`MemoryConfig.ttl_days > 0`. Default `0` (permanent retention). Runs at
chat cold-start.

### 21. Workspace fingerprint dedup (Phase 6.2)

`<workspace_root>/.tengu/registry-fingerprint`. SHA-256 over agent TOMLs
+ skill MDs + tool defs. `reindex_all_workspace` skips the full embed
when fingerprint matches. Bypass: `TENGU_REGISTRY_FORCE_REINDEX=1`.

This is "context management" only in the sense that it controls what
the *planner* sees in `tengu_registry` — but a stale registry is
effectively a context-shape bug, so it earns the layer.

---

## Layer 7 — File chunking

`plugins/memory/persistent_store.rs::chunk_text`.

### 22. Char-window splitter

Defaults: `chunk_size=1000`, `chunk_overlap=200`. Used when an agent
calls `persistent_store` to save a file. Mechanical chunking — no LLM,
no summarisation. Per-chunk embeddings live in the vector backend with
a per-file manifest.

Orthogonal to LLM context: never reads back into a turn directly. Only
surfaces via vector search through `memory_recall` (Layer 2 #6).

---

## Layer 8 — Observability (metrics)

`src/adapters/metrics.rs` (added 2026-04-28). Records what every other
layer produced; never mutates a prompt.

### 27. `MetricsRecord` — one per LLM/embedding call

The struct that carries telemetry across the in-process bus and the
subagent IPC boundary. Fields: `ts_unix`, `session_id`, `kind`
(`Planner` | `Subagent` | `Embedding`), `agent`, `model`,
`prompt_tokens`, `completion_tokens`, `total_tokens`, `prompt_chars`,
`prompt_bytes`, `response_chars`, `latency_ms`, `layers`, `step_id`.

Token counts come from `StreamEvent::Usage` frames the engine layer
already produced (Layer 3) — the metrics layer just routes them to a
record. For embeddings, `usage.total_tokens` is read from the
OpenRouter response (was previously discarded). Falls back to
`prompt_chars / 4` when the field is missing.

### 28. Per-context-layer attribution (planner only)

`MetricsRecord.layers` carries one `MetricsLayer { name, chars, bytes }`
row per planner-prompt section: `system`, `roster`, `cross_session`,
`history`, `recall`, `failure`, `user_message`. Built by
`emit_planner_metrics` in `orchestrator/planner.rs` from the same
strings the planner just composed for the LLM call.

**Approximate by design.** The layer measures only what the planner
built — it does not see engine-side framing (Anthropic `<thinking>`
blocks, OpenAI `tools` schema, function-calling message wrappers,
provider tokenisation overhead). Sum-of-layers ≠ `prompt_tokens`.
Treat layers as relative attribution ("which planner block bloated
this turn") not as a byte-perfect tokeniser. Subagent records leave
`layers` empty; embedding records too.

### 29. Three surfaces, one record

| Surface | Trigger | Use |
|---|---|---|
| Tracing baseline | Always on. Emitted by `metrics::record()` | `RUST_LOG=tengu=info` for the info line, `=debug` to also see per-layer breakdown |
| In-process broadcast bus | `OnceLock<broadcast::Sender<MetricsRecord>>` installed by `build_orchestrator` | Subscribers — TUI aggregator, eval recorder |
| Orchestrator event bus | Bridge task in `build_orchestrator` republishes records as `OrchestratorEvent::MetricsRecorded` | Anything already subscribed to the orchestrator bus (TUI, Telegram channel) sees metrics with no extra wiring |

`record()` is fail-soft — `tx.send()` returns Err only when zero
subscribers are attached, which is fine. The tracing line is the
load-bearing surface.

### 30. Subagent IPC — `AgentIpcOutput.metrics`

Subagent records cross the subprocess boundary in
`AgentIpcOutput::{Ok,Failed}.metrics: Vec<MetricsRecord>` (new field
added 2026-04-28). `run_agent_subprocess` records one per
`run_single_engine_turn` and packs them into the IPC payload.
`SubprocessRunner::run_step` re-emits each via `metrics::record()`
on the parent's global sink — so the TUI sees a unified stream
regardless of which side of the IPC boundary the LLM call ran on.

`#[serde(default, skip_serializing_if = "Vec::is_empty")]` keeps the
IPC payload byte-compatible with older child binaries that don't
emit metrics — they produce JSON without the `metrics` key, and the
parent's serde fills in an empty vec.

### 31. `AggregatorState` — in-process rollup (TUI bottom bubble)

`AggregatorState` keeps overall + by-agent + by-kind running totals
plus the last record. Used by the TUI when `TENGU_TUI_METRICS=1` is
set — renders one System bubble per call:

```
metrics: tok in/out 4.5k/812 · last subagent (2.1k) · session 12.3k · researcher 8.7k
```

The aggregator absorbs records even when the panel is OFF, so flipping
it on mid-session shows real numbers, not zero. Bumping the panel up to
the chat-pane bottom (a true status bar) is on the open list.

---

## Mechanism summary table

| # | Layer | Mechanism | File | Default knob |
|---:|---|---|---|---|
| 1 | 0 | `estimate_tokens_approx` | token.rs | `~4 chars/token` |
| 2 | 0 | `truncate_at_boundary` | token.rs | UTF-8 safe |
| 3 | 1 | `enforce_history_turn_limit` | flow_builder.rs:61 | 20-40 turns / scope |
| 4 | 1 | `maybe_compact_flow` | flow_builder.rs:165 | ratio 0.82-0.88 |
| 5 | 1 | `max_tokens_per_flow` hard stop | chat_builder.rs:309 | 100 000 |
| 6 | 2 | `compute_total_input_budget` | prompt_budget.rs:82 | derived |
| 7 | 2 | `assemble_recent_history` | prompt_budget.rs:8 | 20 msgs |
| 8 | 2 | `memory recall` block | chat_builder.rs:326 | top-5, 600 tok |
| 9 | 2 | `cross_session_msg_top_k` | (planner side) | 0 (off) |
| 10 | 2 | `<memory-context>` fencing | fencing.rs | +50 tok |
| 11 | 2 | grounding nudge / suppress | chat_builder.rs:248 | trigger words |
| 12 | 2 | skill body / file caps | skill_builder.rs:1120 | 16K / 2K |
| 13 | 2 | `truncate_to_token_budget` | prompt_budget.rs:41 | char cap |
| 14 | 3 | `truncate_tool_result` | engine_builder.rs:878 | 300 000 chars |
| 15 | 3 | `compact_tool_result` | engine_builder.rs:855 | 200 chars |
| 16 | 3 | `[OUTPUT_TRUNCATED]` continue | engine_builder.rs:637 | sentinel |
| 17 | 3 | `max_tool_rounds` | config.rs:424 | 70 |
| 18 | 3 | `token_budget` early-exit | engine_builder.rs:617 | per-flow |
| 19 | 4 | `max_mcp_result_chars` | claude_code_engine.rs:520 | 50 000 |
| 20 | 5 | subprocess fresh ctx | runner.rs:23 | by design |
| 21 | 5 | `compress_and_store` | skill_lifecycle/...rs | implicit append |
| 22 | 5 | Phase 5c protocol | main.rs:900 | three rows |
| 23 | 5 | subprocess `max_turns` | runner.rs:39 | 20 |
| 24 | 6 | `ttl_cleanup` | rag/cleanup.rs:24 | `ttl_days=0` |
| 25 | 6 | fingerprint dedup | rag/indexer.rs:64 | sha256 |
| 26 | 7 | `chunk_text` | persistent_store.rs:54 | 1000/200 |
| 27 | 8 | `MetricsRecord` (Planner/Subagent/Embedding) | metrics.rs | always-on |
| 28 | 8 | Per-context-layer attribution | orchestrator/planner.rs::emit_planner_metrics | approximate |
| 29 | 8 | Three surfaces (trace + bus + TUI bubble) | metrics.rs + tui/mod.rs | `TENGU_TUI_METRICS=1` |
| 30 | 8 | `AgentIpcOutput.metrics` IPC plumbing | runner.rs + main.rs | skip-if-empty |
| 31 | 8 | `AggregatorState` rollup (overall + by-agent + by-kind) | metrics.rs | in-memory only |

---

## What we deliberately do NOT have

Comparison points for future sessions:

- **No LLM-driven auto-summarise pass.** Every "summary" in the harness
  is either a truncated concatenation (`maybe_compact_flow`,
  `compact_tool_result`) or written by the model itself as a tool call
  (`compress_and_store`). There is no in-harness LLM call whose only
  job is to compress prior context.
- **No `/compact` slash command** in the TUI.
- **No semantic dedupe** of repeated tool calls. Two identical
  `http_request` calls keep both full responses.
- **No retrieval over prior turns of the current session for the chat
  loop.** `tengu_messages` exists, but only `cross_session_recall_block`
  (planner-side, opt-in) reads it. The chat-side recall path queries
  the legacy memory provider.
- **Subagent step traces are NEVER fed back into the planner's history.**
  Only the `compress_and_store` summary survives, and only
  `RagPlanner::replan` reads it for cross-plan recall.
- **Claude Code engine has zero in-loop compaction.** Layer 4's
  `max_mcp_result_chars` is the only governor on that path. If a Claude
  Code subagent fans out a lot of tool calls, Claude's own context
  fills up — the harness can only cap individual results, not compact
  prior ones.
- **No persistent metrics store.** Layer 8 records are tracing-only +
  in-memory aggregator. Persisting to a JSONL file (or a
  `tengu metrics` CLI subcommand that prints rolled-up totals) is on
  the open list — easy bolt-on via `metrics::subscribe()`.

---

## Where to start when something is wrong

| Symptom | Likely layer / mechanism |
|---|---|
| Model claims it doesn't know about something the user told it 5 turns ago | Layer 1 #3 (turn limit) or Layer 2 #7 (history budget) |
| Model loops on the same tool call | Layer 3 #15 (`compact_tool_result` hid the result body) |
| `Flow token limit reached` notice | Layer 1 #5 (`max_tokens_per_flow`) — bump it or `/reset` |
| `[truncated — showing X of Y chars]` in tool output | Layer 3 #14 (`max_tool_result_chars`) — bump or paginate |
| Claude Code subagent stops citing old tool results | Layer 4 #19 (MCP bridge cap) — bump `TENGU_BRIDGE_MAX_RESULT_CHARS` |
| "model finished without calling compress_and_store" warn | Layer 5 #22 (Phase 5c) — graceful path; final text became the summary |
| Planner picks the same agent on replan despite obvious progress | Layer 5 #19 (`cross_plan_top_k`) — recall not surfacing the prior summary |
| Stale agents/skills in the planner roster | Layer 6 #25 (fingerprint dedup) — `TENGU_REGISTRY_FORCE_REINDEX=1` |
| "Why is this turn so big?" — diagnose context bloat | Layer 8 #28 — set `RUST_LOG=tengu=debug` and read the `metrics.layer` lines for the planner call (chars per layer) |
| Need rolling totals for cost analysis in the TUI | Layer 8 #29/#31 — set `TENGU_TUI_METRICS=1`; aggregator absorbs records even when panel is off |
| Subagent token counts missing in parent logs | Layer 8 #30 — child binary may be old (no `metrics` field). Rebuild both parent and child. |

---

## Companion docs

- **[`context-management-2026-04-27.svg`](./context-management-2026-04-27.svg)** — the full layered diagram (this doc's picture).
- **[`context-management-2026-04-27.html`](./context-management-2026-04-27.html)** — interactive explorer (search the mechanisms, walk a turn, drag config knobs).
- **[`context-cutting-flow-2026-04-27.{svg,html}`](./context-cutting-flow-2026-04-27.svg)** — focused view: Layer 3 (the inner tool loop). Most useful when debugging something inside `collect_engine_response`.
- **[`compression-flow-2026-04-27.{md,svg}`](./compression-flow-2026-04-27.md)** — focused view: Layer 5 (the subagent step protocol). Most useful when touching `run_agent_subprocess` or `tengu_outputs`.

*Last updated 2026-04-27. If you change any of the mechanisms above,
update this doc in the same commit.*
