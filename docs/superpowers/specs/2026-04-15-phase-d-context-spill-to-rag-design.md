# Phase D — Context Spill to RAG (design)

**Status:** Design, pending approval
**Date:** 2026-04-15
**Depends on:** Phase A (tool plugin architecture), Phase B (orchestration skill), Phase C (engine/memory store plugins) — all must be merged and exercised.
**Blocks:** Nothing. This is the terminal phase of the context-handling redesign.

---

## 1. Problem

Context management in `tengu-cluster` today is **cap-based** and **lossy**. Every mechanism that enforces a limit discards information the model can never recover:

- `engine_builder.rs::truncate_tool_result` (line 861) chops tool output at a char limit (`result_chars_limit`). Anything past the cap is replaced with a `[truncated — showing X of Y chars]` marker and is *gone*.
- `engine_builder.rs` runs an adaptive truncation pass that shrinks tool results further as the tool-call round count grows.
- `engine_builder.rs` runs a 2-phase pruning pass that drops whole messages when the turn assembly is over budget.
- `prompt_budget.rs::assemble_recent_history` walks the transcript newest-first and stops as soon as it has filled `history_budget`, capped at `MAX_HISTORY_MESSAGES = 20`. Older messages are dropped from the turn.
- `chat_builder.rs` warns at 80% / 100% of the token budget but has no graceful degradation path.

Each of these mechanisms exists because in-context tokens are expensive and context windows are finite. But we already have the infrastructure to solve this differently: a vector memory store (`DiskVectorMemoryStore` / `QdrantMemoryStore`) with embeddings, semantic search, and — after Phase C — a clean `MemoryStorePlugin` trait. We are throwing context away instead of putting it in RAG.

The Rust core does not need to keep enforcing context-size limits. It needs to keep context *accessible* — in-context when the model is actively using it, in RAG when it is not.

## 2. Goal

Replace **context-size caps** (not loop-safety caps) with **lazy context**. When a turn's live tokens would exceed a configurable soft threshold (default 60% of the engine's context window), spill the oldest eligible items into a session-scoped RAG namespace and replace them in-context with a short summary plus a ref. The model can `context_fetch(ref)` to get any spilled item back, or `memory_search(scope="session")` to find content by meaning. A single hard ceiling at 10× the context window refuses further work with a clear "use `/clear` or `/new`" system error — that ceiling is the only absolute cost cap that remains.

**Expected Rust LOC delta:** approximately net-neutral, slightly positive (roughly −600 deletions from pruning / truncation / history-budgeting code and roughly +620 additions for the spill service, `context_fetch` tool, scope plumbing, and engine integration). The value of Phase D is not in deleting code — it is in replacing *lossy* code with *lossless* code at roughly the same footprint.

**Design principles:**

1. **Nothing is lost.** Anything that used to be truncated or dropped is now embedded, summarized, and persisted in the session's RAG namespace until the namespace itself is cleared.
2. **Summaries are authoritative.** When the model sees a `[previously-seen]` placeholder, it should treat the summary as sufficient by default. Fetching is for recovery, not routine access.
3. **One namespace per agent run.** Main agent, subagent, and fan-out sibling each own their own namespace. Children live and die with their parent. Cleanup is a single namespace drop.
4. **No new primitives beyond `context_fetch`.** Everything else reuses existing memory tools, gaining only a `scope` filter.
5. **File-storage RAG (MEMORY.md, daily logs, curated vectors) stays separate** from session-context RAG. They share the vector store but live under disjoint scopes, and `memory_search`'s scope filter routes reads cleanly.

## 3. Non-goals

- **Touching `MAX_TOOL_ROUNDS`** (engine_builder.rs, currently 30). This is a runaway-loop safety cap, not a context cap. A broken skill can call tools 100× without spilling any context; the tool-round cap stops that. Phase D leaves it alone. If we want to replace it with a smarter loop guard (e.g., same-tool-same-args detection), that is a separate change.
- **Touching `max_tokens`** per-turn output ceiling. That is the model provider's API parameter, not our context cap.
- **New memory store backends or embedders.** Those are Phase C territory.
- **New tool primitives beyond `context_fetch`.** Anything else reuses `memory_search` / `memory_get` / `remember` / `memory_write`.
- **LLM-driven compaction** of the session RAG to rescue a session that hit 10×. Flagged as an optional follow-up (`/compact`) but not part of Phase D core. The hard cap is supposed to fire; if it does, the user clears and continues.
- **A "pin" mechanism** for keeping items in-context forever. FIFO is FIFO. If the model needs something that has been spilled, it fetches it, and the fetched copy is itself eligible to re-spill on the next turn.
- **Caps on user input.** A 10 MB paste is allowed; it lands in the first turn, immediately pushes over 60%, and spills on the same turn through the normal spill path. No special-cased input truncation.

## 4. Architecture

### 4.1 The 60% / 10× budget model

Each engine already exposes `context_window_tokens` (`engine_builder.rs:320`). Phase D adds three configuration knobs, all with sensible defaults:

```toml
[agents.limits]
spill_threshold_pct = 60              # soft — spill starts when live_tokens exceeds this
session_rag_hard_cap_multiplier = 10  # hard — refuse further work at N × context_window total
session_rag_ttl_days = 30             # janitor — idle session namespaces older than this get swept on boot

[memory]
session_chunk_tokens = 1000           # chunk size for embedding spilled items
session_chunk_overlap_tokens = 100    # chunk overlap for search recall
summary_model = "claude-haiku-4-5-20251001"  # cheap model used to summarize spilled items
```

On every turn assembly, the engine recomputes three quantities:

- **`live_tokens`** = system prompt + bootstrap (AGENTS.md, MEMORY.md, daily logs, identity files) + surviving (un-spilled) transcript messages + pending tool call
- **`spilled_tokens`** = total tokens currently stored under `scope = Session(current_session_id)` in the memory store
- **`total_session_tokens`** = `live_tokens + spilled_tokens`

Two budget gates then apply:

1. **Soft — 60% spill trigger.** If `live_tokens > spill_threshold_pct * context_window`, run a FIFO spill pass (see §4.2) until `live_tokens` is back under `(spill_threshold_pct - 5)%` of the window — that 5-point slack prevents the engine from triggering spill on every subsequent turn over a tiny delta. The spill target is `0.55 * context_window` for the default 60% threshold.

2. **Hard — 10× absolute ceiling.** If `total_session_tokens > session_rag_hard_cap_multiplier * context_window`, the turn is refused. The engine emits a transcript-level system error:

   ```
   Session exceeded 10× context budget (10.4M / 10.0M tokens).
   Use /clear to wipe the session RAG and continue, or /new to start a fresh session.
   ```

   No further tool calls execute for that turn. The channel prints the error to the user. The model run ends cleanly. On the next `/clear` or `/new` the namespace wipes and work resumes.

**Bootstrap injection is non-spillable.** AGENTS.md, MEMORY.md, daily logs, identity files, and similar files in the system prompt are regenerated from disk on every turn (see `memory_builder.rs` bootstrap injection code). Spilling them would be a no-op — they would come right back on the next turn's bootstrap pass. They count toward `live_tokens` for the 60% check, but the spill walker skips them.

### 4.2 Spill policy — plain FIFO, oldest-first

When `live_tokens > 60% * context_window`, the engine runs `SessionSpill::spill_fifo(&mut history, target)`. The walker iterates the transcript oldest-first, considering each item for spill. An item is **eligible** if it is one of:

- A tool result (`Message::ToolResult`)
- An assistant text message
- A user text message

An item is **ineligible** if it is:

- Part of the system prompt or bootstrap
- A `[previously-seen ...]` placeholder (already spilled)
- An assistant message containing a pending or unreturned `tool_use` block — spilling it would break the `tool_use` ↔ `tool_result` pairing the model API requires. Such a message becomes eligible only once all of its tool calls have been answered by `tool_result` messages further down the transcript, at which point the whole pair (assistant `tool_use` + matching `tool_result`) is spilled together as a unit.
- The pending tool call whose result the model is about to consume
- An item that was inflated by `context_fetch` during the current turn (see §6 — one-turn grace period prevents thrashing)

For each eligible item in oldest-to-newest order, until `live_tokens < target`:

1. **Summarize.** The spill service collects eligible items into a batch and issues a *single* call to the configured summarizer model (default `claude-haiku-4-5-20251001`). The summarizer prompt asks for a compact, factual summary of each item (2–4 lines max, tool-specific structure when the item is a tool result). If the summarizer call fails, a deterministic fallback kicks in:

   - `read_file` → `"read_file(path=X) — N tokens, N lines"`
   - `http_request` → `"http_request(METHOD URL) — status, content-type, N tokens"`
   - `run_command` → `"run_command($ cmd) — exit=N, stdout N tokens, stderr N tokens"`
   - Default tool → `"<tool_name> result — N tokens, first 200 chars: …"`
   - Message → first 200 + last 100 chars

2. **Chunk.** The full content is split into `session_chunk_tokens`-token chunks with `session_chunk_overlap_tokens` overlap (defaults 1000 / 100). Chunking uses the same token estimator the engine uses for the budget check.

3. **Embed.** Each chunk is embedded via the active `EmbeddingPort` (from Phase C's embedder plugin).

4. **Write.** Each chunk is written to the memory store as a `MemoryEntry` with:
   - `scope = MemoryScope::Session(session_id)`
   - `parent_ref = "msg_{index}"` (stable within the session)
   - `chunk_index = N` (0-based within the parent)
   - `kind` = tool result / assistant message / user message
   - `summary` (from step 1; identical across all chunks of the same parent)
   - `tokens` (parent total)
   - `created_at` timestamp

5. **Replace.** The original item in the live transcript is replaced with a `[previously-seen ...]` placeholder (see §5.1 for exact format). The placeholder carries the ref, the kind, the summary, and the token count.

6. **Recompute.** `live_tokens` is re-estimated. If still over target, continue; otherwise stop.

**Chunking policy notes.** The `context_fetch(ref)` tool always returns the whole parent item (reassembled from chunks in order). `memory_search` returns individual chunks — this is what makes semantic search useful. One-time embedding cost is amortized: spill only fires past 60%, and the summary-batch call is one haiku call per spill pass regardless of how many items spill in that pass.

**A single huge item that exceeds 60% in isolation** still resolves under plain FIFO. If a single 800k-token `read_file` lands in a 1M-token window, the first 60% check after it arrives trips the spill loop; the walker walks oldest-first; eventually it reaches that item and spills it. On the same spill pass. No special case is needed.

### 4.3 `context_fetch` — the one new tool

One new tool plugin, one operation, one parameter. File: `src/adapters/context_fetch.rs`.

```rust
pub(crate) struct ContextFetchTool;

impl ToolPlugin for ContextFetchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "context_fetch",
            description: concat!(
                "Retrieve the full content of a previously-seen context item whose body ",
                "was moved to session RAG. Use ONLY when:\n",
                "  1. The summary in the [previously-seen] placeholder is genuinely ",
                "     insufficient for your current reasoning.\n",
                "  2. You are about to guess or hallucinate specific details that you need ",
                "     to get exactly right.\n",
                "  3. A tool retry needs the original payload.\n",
                "Do NOT fetch just because a [previously-seen] block is visible — the summary ",
                "is the intended content. Each fetch re-inflates the item into your turn and ",
                "costs tokens."
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "ref": {
                        "type": "string",
                        "description": "The ref from a [previously-seen] placeholder, e.g. session:abc123/msg_17"
                    }
                },
                "required": ["ref"]
            }),
        }
    }

    async fn execute(&self, ctx: &ToolCtx<'_>, args: Value) -> ToolResult {
        // 1. Parse ref → (session_id, parent_ref)
        // 2. Verify session_id is in the current agent's visibility scope:
        //      — own session, OR
        //      — any descendant session (prefix match: child, grandchild, fan-out sibling
        //        spawned by the same parent chain).
        //    Reject unrelated refs (sibling of an ancestor, unrelated workspace session).
        // 3. Query memory store for all chunks where
        //      scope = Session(session_id) AND parent_ref = "msg_X"
        //    ordered by chunk_index
        // 4. Reassemble the chunks in order, stripping overlap
        // 5. Mark the inflated item with a `fetched_this_turn` flag so the next spill pass
        //    gives it a one-turn grace period
        // 6. Return the full content as the tool result
    }
}
```

The fetched content becomes that turn's tool result — and is immediately eligible for re-spill on the *turn after next* (one-turn grace period, see §6). There is no pinning. The model fetches what it needs right now; old fetches flow back to RAG automatically.

`memory_search` and `memory_get` also gain a `scope` parameter with three values:

- `"session"` — search/read only the current session namespace
- `"file"` — search/read only the permanent file-store scope (MEMORY.md, daily logs, curated `remember()` entries)
- `"all"` — default; search/read both

This change is backward-compatible: existing skills that call `memory_search(query=...)` without a scope get `"all"` and see both surfaces. Skills that want to restrict to one can opt in.

### 4.4 Session namespace ownership

Namespace IDs follow a hierarchical convention:

- **Main agent run** creates `session:{ulid}` on first spill. The ULID is the session ID already used by `channel_runtime.rs::SessionRegistry`.
  - CLI: one session per `tengu run` invocation, or one per `/new`.
  - Telegram: one session per per-user-per-agent thread, persisting across bot restarts.

- **Subagent run** (spawned via `sessions_spawn`) gets `session:{parent_ulid}:{child_ulid}`. The parent can read the child's spill via `memory_search(scope="session")` because the scope filter matches on the session-ID prefix — a parent can see its descendants, a sibling cannot see another sibling.

- **Fan-out siblings** (`sessions_fan_out`) each get their own `session:{parent_ulid}:{child_ulid_n}`. They do not see each other's spill, only each other's final results (via the `<<<BEGIN_SUBAGENT_RESULT>>>` markers that are passed back through the parent's transcript).

**Lifecycle (hybrid — approved in Q6):**

| Trigger | Effect |
|---|---|
| `/clear` | Wipe the current main-agent session namespace and all live descendants. Keeps the conversation thread alive (CLI flow continues, Telegram thread keeps state). Next spill creates a fresh namespace under a new session ID. |
| `/new` | Wipe the current main-agent session namespace and all live descendants. Start a new session ID. |
| `tengu prune` CLI | Wipe all session namespaces on the machine, across all workspaces. `prune.rs::plan_prune` gains a scan that enumerates `scope = Session(*)` entries and includes them in the drop set. |
| Clean process exit (Ctrl+C) | Keep namespaces intact — user can relaunch and pick up. |
| Crash (panic, OOM) | Keep namespaces intact — post-mortem is possible. |
| Subagent run completes | Child namespace stays until the parent's namespace is dropped. This preserves post-hoc debugging of what a subagent saw. |
| Fan-out sibling completes | Same as subagent — child namespace lives until parent drops. |
| Boot-time janitor | At channel-runtime startup, a non-blocking sweep enumerates all session namespaces and drops any whose newest entry is older than `session_rag_ttl_days` (default 30). Runs once, in the background, does not block channel startup. |
| No spill ever happened in a session | No namespace was ever created — nothing to clean. |

### 4.5 What gets deleted from the Rust core

From Q7 — the full list of code that goes away, with approximate LOC:

**`engine_builder.rs`:**
- `truncate_tool_result` function (line 861) — gone. Replaced: oversized tool results spill whole.
- Adaptive result truncation call sites (around line 687, the `result_chars_limit` loop) — gone.
- 2-phase pruning pass — gone. Replaced by FIFO spill (nothing is dropped, just moved to RAG with a summary).
- `result_chars_limit` config field and its callers — gone.
- Approximate delta: **−300 LOC.**

**`prompt_budget.rs`:**
- `assemble_recent_history` function — gone. History is now unlimited-but-spilled.
- `MAX_HISTORY_MESSAGES = 20` constant — gone.
- Tokenwise history dropping logic — gone.
- The module's remaining surface is token estimation only and may collapse into `token.rs`.
- Approximate delta: **−150 LOC.**

**`chat_builder.rs`:**
- 80% / 100% token-budget warning paths — gone. Replaced by the 60% spill trigger and 10× hard cap at the engine layer.
- Approximate delta: **−50 LOC.**

**Scattered call sites and dead types:**
- Any `HistoryAssembly` / budget-related helpers that become unused.
- Approximate delta: **−100 LOC.**

**Total deletions: approximately −600 LOC.**

### 4.6 What gets added to the Rust core

**`types.rs`:**
- `MemoryScope` enum: `File | Session(String)`. The default for backward-compatible deserialization is `File`.
- `MemoryEntry` gains a `scope: MemoryScope` field, a `parent_ref: Option<String>` field, and a `chunk_index: Option<u32>` field.
- `MemoryStorePort` gains an async method: `drop_scope(&self, scope: &MemoryScope) -> Result<()>`.
- Approximate delta: **+80 LOC.**

**`memory_builder.rs`:**
- `memory_search` and `memory_get` tools gain a `scope` parameter (default `"all"`).
- New `SessionSpill` service: takes a mutable transcript and a budget target, runs the FIFO walker, batches summaries, chunks, embeds, writes, and replaces.
- New helper `count_spilled_tokens(&self, session_id: &str) -> usize` for the 10× hard-cap check.
- Approximate delta: **+250 LOC.**

**New `context_fetch.rs`:**
- One `ToolPlugin` impl. One tool. One operation.
- Approximate delta: **+80 LOC.**

**`engine_builder.rs` — the new hot path:**
- `assemble_turn` gains a spill phase: after bootstrap + history assembly but before tokenization, invoke `SessionSpill::spill_fifo` if `live_tokens > spill_threshold_pct * context_window`.
- `assemble_turn` gains the 10× hard-cap check: if `total_session_tokens > hard_cap`, return `Err(SessionBudgetExceeded)`.
- Error propagation: `SessionBudgetExceeded` is a new engine error that `channel_runtime.rs` translates into a user-visible system error and a clean end-of-run.
- Approximate delta: **+150 LOC** (offset by the −300 from §4.5).

**`prune.rs`:**
- `plan_prune` scans the memory store for `scope = Session(*)` entries and includes them in the prune set, alongside the existing `state/flows`, `memory/`, and workspace scans.
- Approximate delta: **+60 LOC.**

**Total additions: approximately +620 LOC.** Net: approximately +20 LOC — essentially neutral. Phase D is not a code-deletion win; it is a *semantics* win. The same footprint of Rust now stores and replays context instead of truncating and losing it.

### 4.7 Data flow on a turn

```
┌──────────────────────────────────────────────────────────────────┐
│  channel_runtime: user message arrives                           │
│  → engine.complete(messages, tools)                              │
│                                                                   │
│  engine.assemble_turn():                                         │
│    1. Build bootstrap (AGENTS.md, MEMORY.md, daily logs, etc.)   │
│    2. Build system prompt                                        │
│    3. Append transcript history + new user message               │
│    4. live_tokens = estimate(bootstrap + system + history + msg) │
│                                                                   │
│    5. if live_tokens > 0.60 * context_window:                    │
│         SessionSpill::spill_fifo(&mut history, target=0.55)      │
│         → walk oldest-first:                                     │
│              collect eligible batch                              │
│              single haiku call → summaries[N]                    │
│              for each item:                                      │
│                chunk(1000 tokens, 100 overlap)                   │
│                embed each chunk                                  │
│                write to MemoryStore (scope=Session(X))           │
│                replace item with [previously-seen ...]           │
│              recompute live_tokens                               │
│              continue until under target                        │
│                                                                   │
│    6. spilled_tokens = memory_store.count(scope=Session(X))      │
│    7. total = live_tokens + spilled_tokens                       │
│                                                                   │
│    8. if total > 10 * context_window:                            │
│         return Err(SessionBudgetExceeded)                        │
│                                                                   │
│  engine makes the API call, streams response                     │
│  model may call context_fetch(ref) during the turn               │
│    → look up ref in session RAG                                  │
│    → reassemble chunks in order                                  │
│    → return full content as tool result                          │
│    → mark item as `fetched_this_turn` (one-turn grace period)    │
│                                                                   │
│  next assemble_turn() may re-spill the just-fetched item         │
│  (only after its grace period expires)                           │
└──────────────────────────────────────────────────────────────────┘
```

## 5. Behavioral guardrails — summaries are authoritative

The biggest risk in a spill-based system is the model getting nervous about placeholders and fetching every one of them "to be sure." That defeats the entire point. Three mechanisms enforce the rule that **summaries are authoritative, fetching is recovery**.

### 5.1 Placeholder wording signals "you already saw this"

The inline marker the spill walker inserts uses authoritative, not provisional, language:

```
[previously-seen ref=session:abc123/msg_17 kind=tool_result tool=read_file
 summary="read_file(./README.md) — 42k tokens, 1200 lines. Covers: install, PORT config, deploy, troubleshooting."
 tokens=42000  available-via=context_fetch]
```

- **`previously-seen`** — signals *you already know this*, not *this is absent*.
- **`available-via=context_fetch`** — fetch is an option, not an obligation.
- The summary is rich enough (from the haiku call) to answer most follow-up questions on its own.

### 5.2 `context_fetch` tool description discourages casual use

See §4.3 for the full description. Key lines:

> Use ONLY when the summary is genuinely insufficient, you are about to guess/hallucinate specifics, or a retry needs the original payload. Do NOT fetch just because a [previously-seen] block is visible — the summary is the intended content.

The description is part of the tool schema the model sees every turn, so this instruction is always in-context.

### 5.3 The `orchestration` skill (Phase B) gets one new paragraph

Phase B authored `skills/orchestration/SKILL.md`. Phase D adds a short section under "Context management":

```
## Context management

When you see a [previously-seen ref=...] block in your transcript, treat the
summary as what you saw. Continue reasoning from it without fetching.

Only call context_fetch if you literally cannot answer without the exact bytes —
for example, if the user asks "what was the exact value of X on line 47 of that
file" and the summary doesn't say, or if a tool call is retrying and the retry
needs the original payload verbatim.

The summary is deliberately authoritative. Trust it by default. Fetching costs
tokens and re-inflates the item into your turn.
```

This is the only skill change Phase D requires.

## 6. Avoiding re-spill thrashing

If the model fetches a big item, the fetched content pushes the transcript back over 60%, the next turn re-spills it, the model re-fetches it, and so on. This is the re-spill thrash loop. The mitigation is a **one-turn grace period**:

- When `context_fetch` inflates an item into the current turn, the inflated item is marked with a `fetched_this_turn` flag on the transcript entry.
- The next turn's spill walker (§4.2 eligibility) treats any item with `fetched_this_turn` as **ineligible for spill** for exactly one turn.
- At the end of that grace turn, the flag is cleared. The item is now eligible again.

This guarantees the model has at least one turn to use the fetched content for whatever purpose it fetched it, before it can be re-spilled. It cannot loop: the grace period is one turn, so a model that fetches, uses, and moves on will see the item re-spill exactly once; a model that fetches the same item again will see the same placeholder again and (per §5.2) should not re-fetch.

## 7. Migration plan

Each step is self-contained and leaves the system working. D5 is the only step with genuine rewrite risk because it touches the hot turn-assembly path.

| # | Step | Risk | LOC delta |
|---|---|---|---|
| D1 | `MemoryScope` enum on `MemoryEntry`; `drop_scope` on `MemoryStorePort`; `DiskVectorMemoryStore` + `QdrantMemoryStore` impls honor scope filter. Non-breaking — existing entries default to `MemoryScope::File`. | low | +120 |
| D2 | `memory_search` + `memory_get` tools gain `scope` parameter (default `"all"`). Backward-compatible for skills that already call them. | low | +40 |
| D3 | `SessionSpill` service in `memory_builder.rs` — FIFO walker, chunker, batch-summarizer model caller with deterministic fallback. New config keys (`spill_threshold_pct`, `session_rag_hard_cap_multiplier`, `session_rag_ttl_days`, `session_chunk_tokens`, `session_chunk_overlap_tokens`, `summary_model`). | med | +250 |
| D4 | `context_fetch` tool plugin (`context_fetch.rs`). Registered via Phase A's `ToolPlugin`. | low | +80 |
| D5 | Engine loop integration: in `engine_builder.rs::assemble_turn`, invoke spill before tokenization, enforce 10× hard cap, emit `SessionBudgetExceeded` on overflow. **Delete** `truncate_tool_result`, adaptive truncation, 2-phase pruning (engine_builder.rs); `assemble_recent_history`, `MAX_HISTORY_MESSAGES`, history dropping (prompt_budget.rs); 80%/100% warning paths (chat_builder.rs). Includes transcript-level `SessionBudgetExceeded` propagation into `channel_runtime.rs`. | **high** | −600 / +150 |
| D6 | `orchestration` skill gets the "Context management" paragraph (§5.3). One file edit. | none | 0 |
| D7 | `prune.rs::plan_prune` scans session-scoped memory entries and includes them in the prune set. Boot-time janitor in `channel_runtime.rs` sweeps namespaces older than `session_rag_ttl_days`. | low | +60 |
| D8 | Documentation pass — `docs/architecture.md` gets the new budget model diagram, §4 content-flow walkthrough, and config reference for the new keys. | none | 0 |

**Ordering constraint:** D1 and D2 can ship independently. D3 depends on D1 + D2. D4 depends on D1. D5 depends on D3 + D4 — it is the integration step and should be its own PR with a wide review window. D6 can ship any time after D5 lands. D7 depends on D1. D8 ships last.

**Net Rust LOC after all steps land: approximately +20 (net-neutral).** The point of Phase D is not code reduction; it is replacing lossy context handling with lossless, searchable session RAG at the same footprint.

## 8. Testing strategy

Phase D touches hot code and changes the semantics of long conversations. Testing falls into four layers.

### 8.1 Unit tests
- `SessionSpill::spill_fifo` — walker correctness, eligibility filter, grace-period flag, target-token convergence.
- Chunker — exact chunk boundaries, overlap behavior, last-chunk handling, empty-content edge case.
- Summary fallback path — deterministic summaries for each known tool kind plus the default.
- `MemoryScope` serialization round-trip in the disk and Qdrant stores.
- `memory_search` scope filter — `"session"` excludes file entries, `"file"` excludes session entries, `"all"` returns both.
- `context_fetch` ref parsing, reassembly, cross-session rejection.
- 10× hard-cap trip — transcript construction that produces `total > 10 * window` raises `SessionBudgetExceeded`.

### 8.2 Integration tests
- Real engine + real disk memory store + real embedder (or test double):
  - Turn assembly over 60% → spill fires → transcript shrinks → next turn runs.
  - Model calls `context_fetch(ref)` → correct content returned → grace period prevents immediate re-spill → eligible again after one turn.
  - 10× hard cap fires → `SessionBudgetExceeded` reaches `channel_runtime.rs` → user sees system error → `/clear` wipes the namespace → next turn runs cleanly.
  - Subagent namespace isolation — parent's spill and child's spill do not collide; fan-out siblings do not see each other.
  - `/clear` and `/new` drop the right namespaces and leave the file scope untouched.
  - `tengu prune` removes all session namespaces in a workspace.

### 8.3 Regression guards
- A large real-workload trace (e.g., "summarize this large repo" style run) is replayed through the engine, and total API cost is compared before and after Phase D. The target is a net reduction: spill is cheaper than re-sending truncated-and-lossy tool results across turns.
- Same trace is replayed with `/clear` midway to verify the namespace wipe leaves the session in a usable state.
- `MAX_TOOL_ROUNDS` is not triggered by spill — a spilled item should not cause the tool-round counter to advance. (It should not; spill happens inside assembly, outside the tool loop. A test guards it.)

### 8.4 Manual QA (CLI only)
- Long CLI session, paste a large README, watch the spill placeholder appear in the transcript log, verify the next question still answers correctly.
- Same but with Ctrl+C restart — verify the namespace survives and the next `tengu run` continues to see `[previously-seen]` entries.
- Wait 30+ days on a test namespace (or force the janitor manually) — verify the sweep drops it on boot.

## 9. Risks and open questions

- **Summary-model latency on the spill path.** Each spill pass calls haiku once (batched). Network latency is a few hundred ms per pass, all during turn assembly, blocking the engine. Mitigations: (a) batch multiple eligible items into one call (already in D3); (b) the summary call is fire-and-forget with a timeout — if it exceeds the timeout, fall back to deterministic summaries and write the items immediately. Spill never blocks on the summarizer indefinitely.

- **Re-spill thrashing.** Covered in §6. One-turn grace period is the mitigation. Open question: is one turn enough? If a model fetches something, uses it over two consecutive turns, and it re-spills between those turns, the second use has to re-fetch. Monitoring item — adjust the grace period if traces show it matters.

- **Subagent fan-out namespace explosion.** `sessions_fan_out` with 20 siblings, each spilling aggressively, is 20× the write rate into the vector store. The hybrid TTL handles long-term cleanup, but peak per-turn cost is real. Flagged as a monitoring item. If it becomes a problem, the mitigation is to rate-limit child spills or batch-embed across siblings — not a Phase D concern, a Phase D.1 concern.

- **Cross-agent context leakage.** `context_fetch` refs are scoped to the requesting agent's session. A subagent trying to fetch a parent's ref should be rejected. §4.3 step 2 enforces this. Needs an explicit integration test.

- **Hard-cap UX at 10×.** The user sees a system error asking for `/clear`. Confirmed acceptable. Open sub-question: should the error also expose a `/compact` command that runs an LLM-driven compaction pass over the session RAG to give the user an escape hatch *before* wiping? **Flagged as optional follow-up, not Phase D core.** If compaction is added later, it is a new command + a new service method on `SessionSpill` and does not require re-opening the spec.

- **Embedding cost at scale.** Phase D assumes embedding is cheap enough to run on every spill. OpenRouter's `text-embedding-3-small` is, today. If the memory store is swapped to an embedder that is more expensive (e.g., voyage-3), the cost model changes. Mitigation: the cost is bounded by the spill rate, which is bounded by the 10× cap. A single session can generate at most ~10× the context window of embedded chunks before it is forced to clear. That is a hard upper bound on per-session embedding cost.

- **What counts as "live_tokens" when the bootstrap is huge.** Bootstrap (AGENTS.md + MEMORY.md + daily logs) is non-spillable but counts toward the 60% budget. If a workspace has a 400k-token MEMORY.md, the bootstrap alone is already past 40% of a 1M window and spill fires immediately on very short conversations. This is a configuration pathology, not a Phase D bug, but D8's docs pass should note that bootstrap files should be kept compact and that the daily-log rotation (existing, `memory_builder.rs`) is what keeps them that way.

## 10. Dependencies on prior phases

- **Phase A — Tool plugin architecture.** `context_fetch` is registered as a `ToolPlugin`. The `scope` parameter on `memory_search` / `memory_get` is added to plugin-authored tools. Phase A must be in place for D4 to ship.
- **Phase B — Orchestration skill.** §5.3 adds a paragraph to the existing `orchestration` skill. That skill must exist (created in Phase B) before D6 can ship.
- **Phase C — Engine / memory store / embedder plugins.** `MemoryStorePort` must be in its Phase-A-modernized `#[async_trait]` form. `MemoryStorePlugin` must exist so that the `scope` filter is implemented uniformly across backends (disk + Qdrant, and any future plugin). Phase C must ship and be exercised before D1 begins.

## 11. Guardrail

**Do not start Phase D until Phase A, Phase B, and Phase C are all merged and exercised for at least a week.** The spill pattern has many moving parts (eligibility filter, summary batch call, chunk/embed/write, placeholder rendering, grace period, namespace ownership) and is the most behaviorally sensitive change in the four-phase plan. It should land on a stable foundation, and D5 should be a single focused PR reviewed carefully.

## 12. What success looks like

After Phase D lands:

- No code in the engine or chat layer references `truncate_tool_result`, `assemble_recent_history`, `MAX_HISTORY_MESSAGES`, or `result_chars_limit`. Those symbols do not exist.
- A long CLI session can run for hours without losing information: every tool result and every message is either in-context or reachable via `context_fetch` / `memory_search`.
- The CLI transcript log shows `[previously-seen ...]` placeholders in place of the bulk of old tool outputs, with summaries that read naturally.
- `/clear` wipes the session RAG in one call; `/new` does the same and starts a fresh session ID; `tengu prune` wipes all sessions on the machine.
- The orchestration skill's "Context management" paragraph is visible in the main-agent system prompt on every turn, instructing the model to treat summaries as authoritative.
- A replay of a long real-workload trace shows lower total API cost than the pre-Phase-D baseline, because nothing is being re-sent as truncated-and-lossy tool results.
- The 10× hard cap can be tripped deliberately and produces a clean error with a clear recovery path (`/clear`).
- `MAX_TOOL_ROUNDS` still exists, untouched, still catches runaway tool loops.
