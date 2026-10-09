# Compression / Compaction in Tengu-Cluster

> **This is a focused slice — Layer 5 only.** For the full picture across
> all 7 layers and 31 mechanisms, read
> [`context-management-2026-04-27.md`](./context-management-2026-04-27.md)
> (or the interactive `.html`).
>
> Companion to `compression-flow-2026-04-27.svg`. Open both together.
>
> **Different axis: in-turn context cutting** — see
> `context-cutting-flow-2026-04-27.svg`. That diagram covers Layer 3 —
> the FIVE mechanisms that shrink the LLM's prompt each turn (sliding
> history, mid-loop tool-result compaction, per-result truncation, MCP
> bridge cap, output-truncated auto-continue). This file is about
> durable per-step summarisation; that one is about per-turn context shape.

There are **three** distinct "shrink the context" mechanisms in the harness.
Only one of them is the load-bearing protocol the orchestration loop depends
on; the other two are mechanical helpers.

---

## 1. `compress_and_store` — the step-completion protocol *(the important one)*

**File:** `src/adapters/outbound/tools/skill_lifecycle/compress_and_store.rs`

This is the harness-enforced "step is done" signal. Every subagent has the
tool implicitly appended to its tool list (in
`bootstrap::tools::build_subprocess_tool_executor`; only the tool *definition* —
there is no plugin handler). The model is told in its system prompt that its FINAL action MUST
be calling `compress_and_store(summary)`.

### Why "compression"?
Each step's multi-turn LLM trace (potentially many tool calls, tool results,
intermediate reasoning) gets distilled by the LLM itself into a single
short summary string. That string — and only that string — is what
persists across plan/replan cycles.

### Three dispatch paths

| Path | When | Where it runs |
|---|---|---|
| **A — Out-of-band** | OpenRouter / local subagents | `adapters/inbound/cli/run_agent.rs::run_agent_subprocess` (`:497`) intercepts the tool call BEFORE the executor. Sets `compress_called = true`, captures `summary`, persists it via `try_persist_agentic_step_summary` (`postgres_memory`), answers `stored`, breaks the loop after that round. |
| **B — Bridge** | Claude Code subagents | The step's bridge (`mcp_bridge.rs::StepSummary`) writes `summary` to `TENGU_BRIDGE_SUMMARY_FILE` and answers `stored — stop now`; the engine ends the CLI run once the round's other calls are answered; `run-agent` reads the file as the step summary (+ the same `postgres_memory` write). |
| **C — Backstop** | a subagent that never calls it | `compress_called` stays false; `run-agent` writes the final assistant text via `try_persist_agentic_step_summary`. |

With `postgres_memory`, `adapters/inbound/cli/run_agent.rs::run_agent_subprocess` writes the final
summary into Open Brain-style Postgres `agentic_memory`, with embeddings when
available and text-only fallback otherwise
(`agentic_memory::write_step_summary_with_embedding`). Without the feature the
summary only travels back to the parent over IPC.

### Phase 5c — middle-ground protocol *(graceful degradation)*

`adapters/inbound/cli/run_agent.rs:642` decides the IPC verdict:

| `compress_and_store` called? | final_text non-empty? | Verdict |
|---|---|---|
| ✓ | * | **Ok** — canonical good path |
| ✗ | ✓ | **Ok** — graceful (warn logged, summary = final_text) |
| ✗ | ✗ | **Failed** — DagExecutor retries / replans |

Strict original-spec doctrine would fail row 2 too; we deliberately stay
pragmatic because many models (Claude Code subagents in particular) don't
reliably call protocol tools but do produce useful text.

### Read path
`RagPlanner::replan` (legacy planner type name) reads prior step outputs from
Postgres `agentic_memory` (`RecallStore::recall_step_outputs`, top
`[memory] cross_plan_top_k`, default 5) via pgvector first and FTS fallback
when `postgres_memory` is enabled; `plan()` reads this session's outputs too
when `within_session_output_top_k > 0`. Planner routing no longer depends on
a vector DB.

---

## 2. `persistent_store` — file chunking with overlap

**File:** `src/adapters/outbound/tools/memory/persistent_store.rs::chunk_text`

A char-window splitter with overlap (`:54`). Defaults from `[memory]` (`MemoryConfig`):

```
persistent_store_chunk_size    = 1000
persistent_store_chunk_overlap = 200
```

When an agent calls `persistent_store` to save a file:
1. Extract text (PDF / DOCX / XLSX / UTF-8 fallback).
2. Run `chunk_text` to split into overlapping windows.
3. Embed each chunk, upsert into the disk bincode store (`outbound/memory/disk_vector.rs`) with a manifest.

This is **mechanical chunking** — no LLM in the loop, no summarisation. It
exists so vector search can hit sub-document granularity. Different layer
entirely from `compress_and_store`; they coexist without overlap.

---

## 3. Prompt-budget truncation

**File:** `src/application/chat/prompt_budget.rs`

Three helpers used by the runtime when assembling a turn:

- `assemble_recent_history(messages, budget)` — newest contiguous suffix
  that fits the budget, capped at 20 messages. Drops oldest.
- `truncate_to_token_budget(content, max_tokens)` — char-cap using the
  shared `~4 chars/token` heuristic, appends `"\n\n[truncated]"`.
- `reserved_output_tokens` / `compute_total_input_budget` — arithmetic
  over the model's context window with a `ctx / 50` adaptive floor
  (clamped to 64–2 048) and `max(64, capped_output / 4)` headroom.

This is **budget enforcement**, not compression in any LLM sense. There is
no summarisation pass — oldest turns simply drop off the end.

---

## Cross-references

- `CLAUDE.md` § "Key gotchas" — `compress_and_store is appended IMPLICITLY`
  and the Phase 5c degradation path.
- `docs/architecture-2026-04-27.md` § 1 step 7 — narrative of the full
  per-turn flow.
- `TENGU_PLANNER_REGISTRY.md` — root file-backed planner roster generated from
  agents, skills, and tools.
- `src/adapters/outbound/tools/agentic_memory/mod.rs` — Open Brain Postgres memory
  (`postgres_memory`) that receives the step summaries.
- `REDESIGN.md` § 7 (runner IPC) + § 8 (`compress_and_store` tool) — original spec for the protocol (file removed 2026-10-07; last version: `git show 9f0e98f96704eca960575d6c453c5dabb5487d65:REDESIGN.md`).

---

*Last updated 2026-10-08 (checked against the code).*
