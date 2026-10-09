# Context Management in Tengu-Cluster — the canonical reference

> **Read this first** when touching anything that affects what an LLM sees.
> This is the unified map. Two focused companion docs cover slices in more
> depth: `context-cutting-flow-2026-04-27.{svg,html}` (in-turn cuts) and
> `compression-flow-2026-04-27.{md,svg}` (durable step summarisation).

---

## TL;DR

Tengu shapes LLM context across **seven layers**, with **31 numbered
mechanisms** (the summary table below; #27–31 are the metrics
observability layer added 2026-04-28). Section headings carry the same
numbers. None of them is LLM-driven — there is no "summarise the
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
┌──────────────────────────────────────────────────────────────────────────────────────────┐
│ Layer 0  Token primitives        domain/token.rs                                         │
│ Layer 1  Per-flow lifecycle      application/chat/flow.rs, chat/service.rs               │
│ Layer 2  Per-turn assembly       application/chat/prompt_budget.rs, service.rs           │
│ Layer 3  Inner tool loop         application/chat/tool_loop.rs                           │
│ Layer 4  MCP bridge              outbound/engines/claude_code.rs, inbound/mcp_bridge.rs  │
│ Layer 5  Subagent IPC            outbound/subprocess_runner.rs, inbound/cli/run_agent.rs │
│ Layer 6  Open Brain / Wiki state orchestrator/shared_files.rs, tools/agentic_memory/     │
│ Layer 7  File chunking           outbound/tools/memory/persistent_store.rs               │
│ Layer 8  Observability           domain/metrics.rs + application/metrics.rs              │
└──────────────────────────────────────────────────────────────────────────────────────────┘
```

A user message hits Layers 1→2→3 (potentially →4) on the planner-side
turn, then Layers 5→3 (→4) on each subagent step. Layers 0, 6, 7 are
infrastructure consumed by the others. Layer 6 is Open Brain live memory
(Postgres `agentic_memory`) plus Karpathy LLM Wiki compiled state.
**Layer 8 is orthogonal** — it
observes the output of layers 2/3/5 (LLM calls) and the embedder, then
emits records onto a process-global broadcast bus + tracing. It never
mutates a prompt.

---

## Layer 0 — Token estimation primitives

`src/domain/token.rs`

| Helper | What | Used by |
|---|---|---|
| `estimate_tokens_approx(text)` | `text.len() / 4` heuristic (`CHARS_PER_TOKEN = 4`) | Everywhere a token count is needed |
| `estimate_tokens_approx_min1(text)` | Same, but floors to 1 | Per-message token math |
| `truncate_at_boundary(s, max_chars)` | UTF-8-safe slice point | `truncate_tool_result`, `fit_tool_result`, `mcp_bridge::truncate_mcp_result` |
| `truncate_with_suffix(s, max, suffix)` | Truncate + append marker only when actually clipped | Display clipping (`inbound/activity.rs`, `inbound/channel.rs`) — not LLM context |
| `tool_result_char_budget(window)` | `window / 8 × 4` chars — one tool result's share of a local model's window | `LocalEngine::tool_result_char_cap` (Layer 3 #14) |

The `~4 chars/token` heuristic is intentionally crude — it underestimates
for code-heavy strings and overestimates for prose, but errs on the side
of leaving headroom. No tokeniser is invoked.

---

## Layer 1 — Per-flow lifecycle (the auto-compact layer)

`src/application/chat/flow.rs`, called from
`application/chat/service.rs::ChatRuntimeService::process_user_text`.

A "flow" is a conversation scoped by `flow.scope` (one of `main`,
`per-group`, `per-pipe-sender`, or the default `per-sender` —
`config/mod.rs::default_scope`). A turn that lands on another flow key
starts with empty messages and a zero `flow_token_usage`. Each new user
message runs through three flow-level guards **before** the per-turn
assembly:

### 3. `enforce_history_turn_limit` (`application/chat/flow.rs:62`)

Counts back N user turns from the end and drains everything before. Per-scope defaults:

| scope | default `max_history_turns` |
|---|---|
| `main` | 40 |
| `per-group` | 30 |
| `per-pipe-sender` | 25 |
| (other — incl. the default `per-sender`) | 20 |

Override: `[agents.<name>.flow] max_history_turns = N` in `sandboxes/<name>/config.toml`
(`0` counts as 1 — `flow.rs::resolve_history_turn_limit`).

### 4. `maybe_compact_flow` (`application/chat/flow.rs:165`)  — *the auto-compact*

Runs after every push (`phase=pre-engine` after the user message,
`post-engine` after the reply). If `flow_token_usage >= threshold_tokens`
(`max_tokens_per_flow × compaction_threshold_ratio`):
1. Find split index that keeps the most recent `keep_turns` user turns.
2. Concatenate older messages with `[role] content` lines.
3. Truncate to `summary_max_tokens` via `truncate_to_token_budget`.
4. Replace the prefix with a single synthetic Assistant message:
   `[Flow compaction summary]\n<truncated>\n\n[compacted_messages=N, phase=...]`

The "summary" is a **truncated concatenation, not an LLM summary**. That
is a deliberate choice — Layer 2's sliding window catches whatever Layer
1 missed, and bringing the LLM in here would cost a turn.

`FlowCompactionPolicy` defaults (per scope):

| scope | `compaction_threshold_ratio` | history limit | `compaction_keep_turns` |
|---|---|---|---|
| `main` | 0.88 | 40 | 20 |
| `per-group` | 0.86 | 30 | 15 |
| `per-pipe-sender` | 0.84 | 25 | 12 |
| (other — incl. `per-sender`) | 0.82 | 20 | 10 |

`compaction_keep_turns` defaults to half the history limit and is clamped
below it (a set value too): the history limit drops older turns first, so a
larger value left nothing to fold.

`compaction_summary_max_tokens` default (`flow.rs:139`): `15%` of the max
input budget, clamped to `[128, clamp(max_input_budget/3, 256, 4096)]`.

### 5. Hard stop on `max_tokens_per_flow`

`application/chat/service.rs:337` — if `state.flow_token_usage >= max_tokens_per_flow`
(default 100 000) **after** Layer 1 compaction, the turn returns immediately
with a system notice: *"Flow token limit reached. Use /reset to start a
new session."*

Two more notices, same file: an empty assembled prompt returns *"Context
budget exhausted. Use /reset to continue."* (`:418`); at ≥ 80 % of
`max_tokens_per_flow` after the reply, the turn carries *"⚠️ Token budget:
N% used …"* (`:479`).

---

## Layer 2 — Per-turn assembly

`application/chat/service.rs::process_user_text` (continued) +
`prompt_budget.rs`.

### 6. Budget arithmetic (`prompt_budget.rs:53-91`)

```
out              = min(max(output_cap, 1), ctx_window)
reserved_output  = min(ctx_window, max(out + max(out/4, 64), clamp(ctx_window/50, 64, 2048)))
total_input      = min(ctx_window - reserved_output, remaining_flow_tokens)
base_input       = total_input - estimate(system_prompt)
history_budget   = base_input - estimate(memory_block)
```

`output_cap` = `engine.max_output_tokens_per_turn()` (`limits.max_output_tokens_per_turn`,
else `context_window / 8` clamped to `[256, 16 384]`). This is what makes the
model's input fit — it's a chain of subtractions from the model's context window.

### 7. `assemble_recent_history` (`prompt_budget.rs:8`)

Newest contiguous suffix of `state.messages` whose token estimate fits
`history_budget`. Hard ceiling of **20 messages** even if budget allows
more (`MAX_HISTORY_MESSAGES = 20`).

### 8. Memory recall block (+ #9, #10)

Two sides, different knobs (`[memory]`, `MemoryConfig`, `config/mod.rs:956`):

| Side | field | default | what |
|---|---|---|---|
| chat (`service.rs:352`) | `max_recall_entries` | 5 | top-K hits of the in-process `MemoryManager` (disk bincode store), sent as one System message `[Relevant memories]\n- …` |
| chat | `max_recall_tokens` | 600 | **not enforced** — `ChatRuntimeService.max_recall_tokens` is reserved (`#[allow(dead_code)]`) |
| planner | `session_recent_n` | 10 | recent user messages of this session (in-memory ring, lost on restart; no vector search) |
| planner | `cross_session_msg_top_k` (#9) | 0 | semantic match across sessions; 0 = disabled |
| planner | `within_session_output_top_k` | 0 | this session's step outputs on `plan()`; 0 = disabled |
| planner | `cross_plan_top_k` | 5 | replan recall over prior step outputs |

Planner registry context comes from root `TENGU_PLANNER_REGISTRY.md`,
regenerated from the `[agents.*]` blocks that carry a `description`, skills,
and core + MCP tools before planner calls. Builds with
`postgres_memory` use Postgres `agentic_memory` for planner user-message
persistence, cross-session message recall, within-session step-output
recall, and replan cross-plan recall. Without `postgres_memory`, these
durable planner-memory lanes are empty.

**#10 `<memory-context>` fencing** (`application/memory/fencing.rs::build_memory_context_block`):
wraps text in `<memory-context>…</memory-context>` with a system note. Its
only caller is `injector::for_turn` on `ChatOrchestratorPortImpl::run_orchestrator_turn`,
which no live path calls — the planner uses `run_orchestrator_turn_with_system_metered`
(no injection). The chat recall block above is **not** fenced.

### 11. Grounding nudge + `suppress_grounding_nudge`

`application/chat/service.rs:255` (`needs_fresh_history_grounding`) and `:349`.
When the user message contains *"last", "latest", "most recent", "newest",
"previous", "recently", "before that"* (whole words — "Elasticsearch" no
longer counts), the chat layer prepends a system message:

> "For questions about last/latest/most recent history, do not rely on
> recalled memory summaries. Verify against the current conversation and
> available tools/workspace state before answering. If you cannot
> verify, say so clearly."

When triggered, **memory recall is also skipped** for that turn —
recalled summaries from older turns would compete with the freshness
instruction.

The planner-side LLM call sets `suppress_grounding_nudge = true` so the
nudge doesn't compete with the SKILL.md "emit JSON only" instruction.
This is set inside `bootstrap/orchestrator.rs::run_turn_inner` (`:253`,
`suppress_grounding_nudge: system_override.is_some()`), reached through
`run_turn_with_system` / `run_turn_with_system_metered`.

### 12. Skill body assembly (+ #13)

`application/skills/registry.rs::build_system_prompt_with_tools` (`:1253-1360`).
Knobs `[agents.<name>.prompt_budget]` (`PromptBudgetConfig`, `config/mod.rs:1066`):

| knob | default | what |
|---|---|---|
| `max_file_tokens` | 2 000 | `identity.instructions` and each workspace identity file (`IDENTITY.md`, `PROFILE.md`, `CONTEXT.md`) |
| `max_skill_context_tokens` | 16 000 | each skill context fragment |
| `max_total_tokens` | 32 000 | whole system prompt: an identity file past it ends the file list; a skill fragment past it is dropped with a warn (`Skill context DROPPED`); the tool listing gets 10 % slack |
| (#13 each cut) | `truncate_to_token_budget` (`prompt_budget.rs:41`) | appends `\n\n[truncated]` |

No dedup or per-skill-count cap — selection happens upstream.

---

## Layer 3 — Inner tool-calling loop

`application/chat/tool_loop.rs::collect_engine_response`. The **per-turn**
context-cutting layer. See `context-cutting-flow-2026-04-27.html` for
the interactive walkthrough.

### 14. `truncate_tool_result` (`application/chat/tool_loop.rs:481`, via `tool_result_content` `:406`)

Each tool result char-capped at `max_tool_result_chars` (default
**300 000**) when first inserted into messages. Footer:
`[truncated — showing X of Y chars]`. UTF-8 boundary safe.

**Local engines** (`Engine::tool_result_char_cap` = `Some`, only
`LocalEngine`): `fit_tool_result` instead — a result within
`min(context_window / 8 × 4 chars, max_tool_result_chars)` (16 384 tokens →
8 192 chars) stays whole; above it a typed row's `data` line becomes
`data: <n> bytes in observation <key>` (`Observation::compact_text`), then the
result, footer included, is cut to fit. `run-agent` applies the
same rules for every engine (`tool_loop::tool_result_content` + `compact_tool_result`
of older rounds — before 2026-10-01 only for local agents; an OpenRouter step
resent a 2 MB result on every turn).

### 15. `compact_tool_result` mid-loop (`application/chat/tool_loop.rs:458`, caller `compact_older_tool_results` `:389`, called at `:226`)  — *the closest to /compact*

After `round >= 1`, walks `messages[..compact_cutoff]`. Every
`Role::Tool` message in that prefix gets rewritten in place to its first
line, capped at `compact_result_limit` (default **200 chars**; `0` does not
disable it — every older result becomes `…`). `run-agent` calls the same
function from turn 1 on.

```rust
if content.len() <= limit { return content }
let first_line = content.lines().next().unwrap_or(content);
if first_line.len() <= limit { return first_line }
format!("{}…", &first_line[..safe_boundary(limit)])
```

Rationale (from the inline comment): tool results "typically put key
info on the first line" — tx hashes, IDs, status — so keeping the first
line preserves working memory cheaply.

### 16. `[OUTPUT_TRUNCATED]` auto-continue (`application/chat/tool_loop.rs:135`)

The OpenRouter and local engines end the text with the sentinel on a
`finish_reason = length` cut (`openrouter.rs:308`, `local.rs:140`). When a
response with no tool calls ends with it — tools offered, rounds left — the
loop appends `"Your output was truncated. Continue from where you left off."`
as a user turn and re-rolls. Extends, doesn't cut. `run-agent` has no
auto-continue.

### 17. `max_tool_rounds` hard stop (default **70**)

After 70 rounds the loop exits and forces one final `tools=[]` engine
turn (`application/chat/tool_loop.rs:231`). Claude Code counts tool calls
instead and kills the CLI past the cap (`claude_code.rs`).

### 18. `token_budget` mid-loop early-exit (`application/chat/tool_loop.rs:114`)

If a token budget is passed in and `total_input_delta + total_output_delta > budget`,
the loop exits with a warn log. Every caller passes `None` today (chat,
eval, webhooks, `tengu tool turn`, `doctor`) — the chat runtime enforces its
own flow budget (#5).

---

## Layer 4 — MCP bridge

`adapters/outbound/engines/claude_code.rs` (sets it) · `adapters/inbound/mcp_bridge.rs` (cuts).

### 19. `max_mcp_result_chars` (`config/mod.rs:598` → `claude_code.rs:223` → `mcp_bridge.rs:634`)

Caps tool results returned to the Claude Code CLI through the MCP
bridge. Default **50 000 chars / ~12.5K tokens**. The knob is
`[agents.<name>.limits] max_mcp_result_chars`; the engine writes it into the
bridge's env as `TENGU_BRIDGE_MAX_RESULT_CHARS` (`build_mcp_config_json`), and
`tengu mcp-bridge` cuts each result there (`truncate_mcp_result`, same
`[truncated — showing X of Y chars]` footer; 50 000 when the env is unset).

This exists because the **Claude Code CLI engine has no in-loop
compaction at all**. Without this cap, bridge-routed tool results would
leak unbounded context into Claude. Layer 3's `truncate_tool_result` and
`compact_tool_result` only run on the in-process path (OpenRouter, local).

---

## Layer 5 — Subagent IPC

`subprocess_runner.rs`, `adapters/inbound/cli/run_agent.rs::run_agent_subprocess`, plus the
`compress_and_store` protocol. See `compression-flow-2026-04-27.svg` for
the picture.

### 20. Subprocess gets fresh context (`subprocess_runner.rs:26`)

The child `tengu run-agent` process starts with **empty `messages`** —
it never inherits the parent's chat history. The IPC payload
(`AgentIpcInput`) carries `goal`, `agent_name`, `max_turns`, `session_id`,
`step_id`, `sandbox_config`, `plan_state` (plus `compose` for composed steps);
`model` / `tools` / `skills` travel empty — the child re-loads the same sandbox
config and takes them from its own `[agents.<name>]` block. By design — keeps
subagents focused and prevents accidental context leakage between
steps.

### 21. `compress_and_store` durable summary

The harness-enforced "step is done" signal. Implicitly appended to every
subagent's tool list (`bootstrap/tools.rs::build_subprocess_tool_executor`;
definition in `outbound/tools/skill_lifecycle/compress_and_store.rs`). The
model calls it with a `summary` string. With
`postgres_memory`, the summary is written to Postgres `agentic_memory`
with embeddings when available and text-only fallback otherwise
(`try_persist_agentic_step_summary`). Dispatch:

| Path | Trigger |
|---|---|
| **A — Out-of-band** | OpenRouter / local subagents — `run_agent.rs:497` intercepts the call before the executor runs (answers `stored`), then ends the loop after that round |
| **B — Bridge** | Claude Code subagents — the step's bridge (`mcp_bridge.rs::StepSummary`) writes the summary to `TENGU_BRIDGE_SUMMARY_FILE` and answers `stored — stop now`; the engine ends the CLI run once the round's other calls are answered; `run-agent` reads the file as the summary (`:584`) |
| **C — Backstop** | a subagent that never calls it — `compress_called` stays false and `run-agent` writes the final assistant text via `try_persist_agentic_step_summary` (`:612`) |

**Read side** — cross-plan recall: `RagPlanner::replan` (legacy planner type
name) surfaces past step summaries to the planner LLM. With
`postgres_memory`, it queries Postgres `agentic_memory` step outputs
(`RecallStore::recall_step_outputs`) via pgvector first and FTS fallback.
Without that feature, planner memory lanes are empty. Capped by
`cross_plan_top_k` (default 5; `0` = off). `plan()` reads this session's
step outputs too when `within_session_output_top_k > 0`.

### 22. Phase 5c middle-ground protocol (`adapters/inbound/cli/run_agent.rs:642`)

| compress_called? | final_text non-empty? | Verdict |
|---|---|---|
| ✓ | * | **Ok** — canonical good path |
| ✗ | ✓ | **Ok** — graceful (warn logged, summary = final_text) |
| ✗ | ✗ | **Failed** — DagExecutor retries / replans |

### 23. Subprocess `max_turns` (= `limits.max_tool_rounds`, default **70**)

`AgentIpcInput.max_turns` (`subprocess_runner.rs:43`), set by
`SubprocessRunner::run_step` (`:355`) from the agent block's
`limits.max_tool_rounds` (the serde default 20 only applies to payloads
without the field). Hard cap on the subagent's mini-loop; hits the
`compress_and_store` warn path on exhaust. It counts engine turns on
OpenRouter / local, but **tool calls** of the one CLI run on Claude Code
(`EngineContext.max_tool_rounds`: the CLI is killed past it). Wall clock per step:
`limits.step_timeout_secs` (default **600**), enforced by `run_with_timeout` (`:191`).

---

## Layer 6 — Open Brain / Wiki State

`orchestrator/shared_files.rs`, `outbound/tools/agentic_memory/`.

### 24. Open Brain live memory

With `postgres_memory`, user messages, step summaries, files, and transcript
chunks land in Postgres `agentic_memory` tables. Recall uses pgvector first
and FTS fallback. No TTL / cleanup pass.

### 25. Karpathy LLM Wiki + planner files

Root `TENGU_PLANNER_REGISTRY.md` is regenerated from the `[agents.*]` blocks
with a `description`, skill frontmatter, and core + MCP tool definitions before
planner calls. It is loaded
directly into the planner prompt, so planner routing no longer depends on
old registry search or registry fingerprints. Stable promoted knowledge
is compiled into `.tengu/agentic-memory/wiki/*.md`.

---

## Layer 7 — File chunking

`outbound/tools/memory/persistent_store.rs::chunk_text`.

### 26. Char-window splitter (`persistent_store.rs:54`)

Defaults: `[memory] persistent_store_chunk_size = 1000`,
`persistent_store_chunk_overlap = 200`. Used when an agent
calls `persistent_store` to save a file. Mechanical chunking — no LLM,
no summarisation. Per-chunk embeddings live in the disk bincode store
(`outbound/memory/disk_vector.rs`) with a per-file manifest.

Orthogonal to LLM context: never reads back into a turn directly. Only
surfaces via the `memory_search` tool, `persistent_store` `search`, or the
chat-side recall block (Layer 2 #8).

---

## Layer 8 — Observability (metrics)

`src/domain/metrics.rs` (added 2026-04-28). Records what every other
layer produced; never mutates a prompt.

### 27. `MetricsRecord` — one per LLM/embedding call

The struct that carries telemetry across the in-process bus and the
subagent IPC boundary. Fields: `ts_unix`, `session_id`, `kind`
(`Planner` | `Subagent` | `Embedding` | `WikiCompiler` | `Decision` — the last
two: `compile_wiki` and Jev decision-loop calls), `agent`, `model`,
`prompt_tokens`, `completion_tokens`, `total_tokens`, `prompt_chars`,
`prompt_bytes`, `response_chars`, `latency_ms`, `layers`, `step_id`.

Token counts come from `StreamEvent::Usage` frames the engine layer
already produced (Layer 3) — the metrics layer just routes them to a
record. For embeddings, `usage.total_tokens` is read from the
OpenRouter response (was previously discarded). Falls back to
`prompt_chars / 4` when the field is missing.

### 28. Per-context-layer attribution (planner only)

`MetricsRecord.layers` carries one `MetricsLayer { name, chars, bytes }`
row per planner-prompt section — `plan`: `system`, `roster`, `cross_session`,
`history`, `session_recall`, `user_message`; `replan`: `system`, `roster`,
`cross_session`, `history`, `recall`, `failure`, `user_message`
(`orchestrator/planner.rs:570`, `:693`). Emitted by `emit_planner_metrics`
(`:730`) from the same strings the planner just composed for the LLM call.

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
| In-process broadcast bus | `OnceLock<broadcast::Sender<MetricsRecord>>` installed by `build_orchestrator` (`install_global_sink`, `bootstrap/orchestrator.rs:384`) | Subscribers — TUI aggregator, eval recorder |
| Orchestrator event bus | Bridge task in `build_orchestrator` republishes records as `OrchestratorEvent::MetricsRecorded` | Anything already subscribed to the orchestrator bus (TUI, Telegram channel) sees metrics with no extra wiring |

`record()` is fail-soft — `tx.send()` returns Err only when zero
subscribers are attached, which is fine. The tracing line is the
load-bearing surface.

### 30. Subagent IPC — `AgentIpcOutput.metrics`

Subagent records cross the subprocess boundary in
`AgentIpcOutput::{Ok,Failed}.metrics: Vec<MetricsRecord>` (new field
added 2026-04-28). `run_agent_subprocess` records one per
`run_single_engine_turn` and packs them into the IPC payload.
`SubprocessRunner::run_step` re-emits each via `metrics::forward()`
on the parent's global sink (bus only — the child's own `record()` line
already reached the parent log through forwarded stderr) — so the TUI sees
a unified stream regardless of which side of the IPC boundary the LLM call
ran on.

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
| 1 | 0 | `estimate_tokens_approx` | domain/token.rs:11 | `~4 chars/token` |
| 2 | 0 | `truncate_at_boundary` | domain/token.rs:38 | UTF-8 safe |
| 3 | 1 | `enforce_history_turn_limit` | application/chat/flow.rs:62 | 20-40 turns / scope |
| 4 | 1 | `maybe_compact_flow` | application/chat/flow.rs:165 | ratio 0.82-0.88 |
| 5 | 1 | `max_tokens_per_flow` hard stop | application/chat/service.rs:337 | 100 000 |
| 6 | 2 | `compute_total_input_budget` | prompt_budget.rs:82 | derived |
| 7 | 2 | `assemble_recent_history` | prompt_budget.rs:8 | 20 msgs |
| 8 | 2 | memory recall block (chat) | application/chat/service.rs:352 | top-5 (`max_recall_tokens` not enforced) |
| 9 | 2 | `cross_session_msg_top_k` | orchestrator/planner.rs:332 (planner side) | 0 (off) |
| 10 | 2 | `<memory-context>` fencing | application/memory/fencing.rs | no live caller |
| 11 | 2 | grounding nudge / suppress | application/chat/service.rs:255 · :349 | trigger words (whole) |
| 12 | 2 | identity / skill / prompt caps | application/skills/registry.rs:1253 | 2K / 16K / 32K |
| 13 | 2 | `truncate_to_token_budget` | prompt_budget.rs:41 | char cap |
| 14 | 3 | `truncate_tool_result` (local: `fit_tool_result`) | application/chat/tool_loop.rs:406 | 300 000 chars (local: window / 8 × 4) |
| 15 | 3 | `compact_tool_result` | application/chat/tool_loop.rs:458 (caller :226) | 200 chars |
| 16 | 3 | `[OUTPUT_TRUNCATED]` continue | application/chat/tool_loop.rs:135 | sentinel |
| 17 | 3 | `max_tool_rounds` | config/mod.rs:579 · tool_loop.rs:231 | 70 |
| 18 | 3 | `token_budget` early-exit | application/chat/tool_loop.rs:114 | `None` on every caller |
| 19 | 4 | `max_mcp_result_chars` | config/mod.rs:598 · claude_code.rs:223 · inbound/mcp_bridge.rs:634 | 50 000 |
| 20 | 5 | subprocess fresh ctx | subprocess_runner.rs:26 | by design |
| 21 | 5 | `compress_and_store` (+ cross-plan read) | outbound/tools/skill_lifecycle/compress_and_store.rs (def) · inbound/cli/run_agent.rs:497 (intercept) · inbound/mcp_bridge.rs::StepSummary (bridge) | implicit append · `cross_plan_top_k` 5 |
| 22 | 5 | Phase 5c protocol | inbound/cli/run_agent.rs:642 | three rows |
| 23 | 5 | subprocess `max_turns` / step wall clock | subprocess_runner.rs:43 · `run_step` :355 | `limits.max_tool_rounds` 70 / `step_timeout_secs` 600 s |
| 24 | 6 | Open Brain memory | outbound/tools/agentic_memory | Postgres |
| 25 | 6 | LLM Wiki + planner files | `.tengu/agentic-memory/wiki`, `TENGU_PLANNER_REGISTRY.md` | Markdown/root files |
| 26 | 7 | `chunk_text` | persistent_store.rs:54 | 1000/200 |
| 27 | 8 | `MetricsRecord` (Planner/Subagent/Embedding/WikiCompiler/Decision) | domain/metrics.rs:43 | always-on |
| 28 | 8 | Per-context-layer attribution | orchestrator/planner.rs::emit_planner_metrics | approximate |
| 29 | 8 | Three surfaces (trace + bus + TUI bubble) | application/metrics.rs + inbound/tui/mod.rs | `TENGU_TUI_METRICS=1` |
| 30 | 8 | `AgentIpcOutput.metrics` IPC plumbing | subprocess_runner.rs + cli/run_agent.rs | skip-if-empty |
| 31 | 8 | `AggregatorState` rollup (overall + by-agent + by-kind) | domain/metrics.rs:180 | in-memory only |

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
- **No automatic chat-loop recall from Open Brain yet.** Planner-side opt-in
  recall reads Postgres `agentic_memory`; chat-side recall uses the in-process
  `MemoryManager` (disk bincode store).
- **Subagent step traces are NEVER fed back into the planner's history.**
  Only the `compress_and_store` summary survives. With `postgres_memory`,
  `replan` reads those summaries for cross-plan recall and `plan` reads them
  within-session when `within_session_output_top_k > 0`.
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
| `[truncated — showing X of Y chars]` in tool output | Layer 3 #14 (`max_tool_result_chars`; local engines: 1/8 of `context_window`) — bump or paginate |
| Local model forgets the goal / answers off-task | Ollama `num_ctx` below the prompt (older messages dropped silently) — raise it and match `limits.context_window` (`docs/engine-backends.md` § Local) |
| Claude Code subagent stops citing old tool results | Layer 4 #19 (MCP bridge cap) — bump `[agents.<name>.limits] max_mcp_result_chars` (the engine sets the bridge's `TENGU_BRIDGE_MAX_RESULT_CHARS` from it) |
| A skill is missing from the system prompt | Layer 2 #12 — `max_total_tokens` (32 000) dropped it; look for the `Skill context DROPPED` warn |
| "model finished without calling compress_and_store" warn | Layer 5 #22 (Phase 5c) — graceful path; final text became the summary |
| Planner picks the same agent on replan despite obvious progress | Layer 5 #21 read side (`cross_plan_top_k`) — Postgres recall not surfacing the prior summary |
| Stale agents/skills in the planner roster | Layer 6 #25 — inspect root `TENGU_PLANNER_REGISTRY.md`; it regenerates before planner calls |
| "Why is this turn so big?" — diagnose context bloat | Layer 8 #28 — set `RUST_LOG=tengu=debug` and read the `metrics.layer` lines for the planner call (chars per layer) |
| Need rolling totals for cost analysis in the TUI | Layer 8 #29/#31 — set `TENGU_TUI_METRICS=1`; aggregator absorbs records even when panel is off |
| Subagent token counts missing in parent logs | Layer 8 #30 — child binary may be old (no `metrics` field). Rebuild both parent and child. |

---

## Companion docs

- **[`context-management-2026-04-27.svg`](./context-management-2026-04-27.svg)** — the full layered diagram (this doc's picture).
- **[`context-management-2026-04-27.html`](./context-management-2026-04-27.html)** — interactive explorer (search the mechanisms, walk a turn, drag config knobs).
- **[`context-cutting-flow-2026-04-27.{svg,html}`](./context-cutting-flow-2026-04-27.svg)** — focused view: Layer 3 (the inner tool loop). Most useful when debugging something inside `collect_engine_response`.
- **[`compression-flow-2026-04-27.{md,svg}`](./compression-flow-2026-04-27.md)** — focused view: Layer 5 (the subagent step protocol). Most useful when touching `run_agent_subprocess` or Open Brain summary capture.

*Last updated 2026-10-08 (checked against the code: line refs, headings
numbered as the summary table, recall / fencing / skill-cap / bridge-cap
facts). If you change any of the mechanisms above, update this doc in the
same commit.*
