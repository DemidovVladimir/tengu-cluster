# Memory Architecture Redesign Proposal

> **Status (2026-03-15):** This is a historical document. All problems described below have been resolved. Phases 1-5 are implemented. See [MEMORY_ARCHITECTURE_VALIDATED.md](MEMORY_ARCHITECTURE_VALIDATED.md) for the validated design with implementation status.

## The Problems (Resolved)

### 1. Inter-Agent Hallucination (Critical)

When multi-agent tasks run (DeSci sandbox, WebStudio), the dependent agent **fabricates data** instead of reading the actual output of the prior agent.

**Root cause in Telegram orchestrator** (`telegram_runtime.rs:250-281`):
```
build_task_prompt() gives Agent B a file path to Agent A's outcome:
  "- **task-1** -> outcome at `.tengu-tasks/task-1.md`"
  "Use read_file on these outcome files to see what was done..."
```

The LLM often ignores the `read_file` instruction and hallucinates the content. This is a **known pattern** across frameworks -- giving an LLM a pointer to data and hoping it will dereference it is unreliable.

**The CLI orchestrator already solved this** (`orchestrator.rs:582-611`): `build_step_context()` embeds the actual output text inline (truncated to `MAX_STEP_CONTEXT_CHARS = 3000`). Agent B sees the real data in its prompt, no file read needed.

**Evidence from research**: CrewAI's `context=[task_a, task_b]` injects actual TaskOutput content. Google ADK's `output_key` pattern gives agents verbatim data via shared state. Both have **low hallucination risk** for data transfer. Tengu's Telegram orchestrator is the only path that relies on the agent choosing to read a file.

### 2. Global Memory Pollution

All sandboxes share `~/.tengu/memory/vectors.bin`. DeSci memories pollute WebStudio recall. A `remember` call from the DeSci research agent gets returned when the WebStudio frontend agent recalls "layout patterns."

**What others do**: OpenClaw uses per-project memory (`memory/` in workspace dir). Each agent gets its own SQLite index at `~/.openclaw/memory/<agentId>.sqlite`. Strict isolation.

### 3. No Topic Tracking / No Orchestrator Memory

The orchestrator has no memory of prior goals. If a user runs "research IP-NFT minting" on Monday and "mint an IP-NFT" on Tuesday, the orchestrator plans from scratch -- no recall that research was already done.

**What others do**: CrewAI stores long-term task results in SQLite. OpenClaw writes daily notes + curated MEMORY.md. AutoGen maintains full conversation history. All enable cross-session recall.

### 4. No Conversation Compression

Full dialog history is passed around, consuming tokens. When the 80% budget warning fires, there's no automatic persistence of important context before truncation.

**What others do**: OpenClaw's pre-compaction flush triggers a silent agentic turn to write durable memories before context is compacted. Hermes uses LLM summarization + FTS5 for cross-session recall.

---

## Research Summary

### Three Dominant Patterns (from 8 frameworks)

| Pattern | Frameworks | Hallucination Risk | Tengu Fit |
|---------|-----------|-------------------|-----------|
| **Shared mutable state** | LangGraph, Google ADK | Low (verbatim) | Poor -- Tengu is file-based, not state-dict |
| **Task output forwarding** | CrewAI, AutoGen | Low-Medium (structured) | **Best fit** -- already have task results |
| **Session isolation + message passing** | OpenClaw, Hermes | Medium (lossy text) | Current approach for single-agent |

### Key Insights

1. **CrewAI's cognitive scoring** -- same knowledge, different recall lenses per agent role. Planning agent weights importance; execution agent weights recency.
2. **OpenClaw's hybrid search** -- vector + BM25 catches exact tokens (IDs, hashes) that pure embedding search misses.
3. **OpenClaw's temporal decay** -- `score * e^(-lambda * age_days)`, half-life 30 days. Evergreen entries exempt.
4. **Intrinsic Memory Agents paper** (arxiv 2508.08997) -- heterogeneous role-aligned memory outperforms shared homogeneous memory for role consistency.
5. **Letta's insight** -- RAG should be a *tool within* an agentic framework, not a standalone pipeline. Avoids "context pollution" from irrelevant retrieved chunks.

---

## Proposed Changes

### Phase 1: Inline Inter-Agent Data Passing (Highest Priority)

**What**: Port the CLI orchestrator's inline embedding strategy to the Telegram orchestrator. Stop relying on agents to `read_file` dependency outcomes.

**Current flow (Telegram)**:
```
Agent A completes -> output written to .tengu-tasks/task-1.md
Agent B gets prompt: "outcome at `.tengu-tasks/task-1.md`, use read_file"
Agent B: [often skips read_file, hallucinates content]
```

**Proposed flow**:
```
Agent A completes -> output stored in memory + written to file (audit)
Agent B gets prompt with actual output inline:
  "## Completed dependency: task-1 (researcher)
   <actual truncated output, max 3000 chars>"
Agent B: [sees real data, acts on it]
```

**Changes**:
1. `outcome_paths: HashMap<String, String>` -> `HashMap<String, (String, String)>` mapping `task_id -> (file_path, output_text)`
2. `build_task_prompt(dep_outcomes: &[(String, String)])` -> `dep_outcomes: &[(String, String, String)]` where third element is actual output content (truncated to 3000 chars, char-boundary-safe)
3. Embed dependency output inline in the prompt instead of just file paths
4. Keep outcome file write for auditability, but the prompt no longer depends on agent reading it
5. Extract shared truncation helper to `channel_runtime.rs`: `truncate_output(text: &str, max_chars: usize) -> String`

**Files**:
- `src/adapters/telegram_runtime.rs` -- `build_task_prompt()`, batch result collection in `orchestrate_team_goal()`
- `src/adapters/channel_runtime.rs` -- shared `truncate_output()` helper
- `src/adapters/orchestrator.rs` -- refactor to use shared truncation helper

**Risk**: Low. The CLI orchestrator has been doing this successfully. We're just porting the same pattern.

---

### Phase 2: Per-Sandbox Memory Store Path

**What**: When a sandbox has a workspace, store memory in `<workspace>/memory/` instead of global `~/.tengu/memory/`. This prevents cross-sandbox pollution.

**Current**:
```
DeSci sandbox -> ~/.tengu/memory/vectors.bin
WebStudio sandbox -> ~/.tengu/memory/vectors.bin  (same file!)
TUI single-agent -> ~/.tengu/memory/vectors.bin
```

**Proposed**:
```
DeSci sandbox -> ~/desci-workspace/memory/vectors.bin
WebStudio sandbox -> ~/webstudio-workspace/memory/vectors.bin
TUI single-agent -> ~/.tengu/memory/vectors.bin  (unchanged default)
```

**Changes**:
1. Add `resolve_memory_store_path()` to `channel_runtime.rs`:
   ```rust
   pub(crate) fn resolve_memory_store_path(
       memory_config: &MemoryConfig,
       workspace: Option<&Path>,
   ) -> PathBuf {
       if let Some(ws) = workspace {
           ws.join("memory")
       } else {
           // existing tilde expansion of memory_config.store_path
       }
   }
   ```
2. `build_memory_handle()` gains `workspace: Option<&Path>` parameter
3. Callers pass first agent's workspace (sandbox) or None (single-agent TUI)

**Files**:
- `src/adapters/channel_runtime.rs` -- add `resolve_memory_store_path()`, modify `build_memory_handle()`
- `src/adapters/orchestrator.rs` -- pass workspace to `build_memory_handle()`
- `src/adapters/telegram_runtime.rs` -- pass workspace to `build_memory_handle()`
- `src/adapters/tui/mod.rs` -- pass workspace (or None for fallback)

**Backward compat**: Single-agent TUI with no workspace -> `~/.tengu/memory/` (unchanged). No config schema changes.

**Risk**: Low. Only changes where vectors.bin is created, not how it works.

---

### Phase 3: Metadata Field on MemoryEntry

**What**: Add `metadata: HashMap<String, String>` to `MemoryEntry` for topic tagging, source tracking, and type classification. This is the foundation for phases 4 and 5.

**Current `MemoryEntry`**:
```rust
struct MemoryEntry {
    id: String,
    content: String,
    embedding: Vec<f32>,
    agent_id: String,
    created_at_epoch_s: u64,
}
```

**Proposed**:
```rust
struct MemoryEntry {
    id: String,
    content: String,
    embedding: Vec<f32>,
    agent_id: String,
    created_at_epoch_s: u64,
    #[serde(default)]
    metadata: HashMap<String, String>,  // NEW
}
```

**Standard metadata keys** (convention, not enforced):
- `type`: `fact` | `outcome` | `topic_overview`
- `goal`: originating goal text (for topic overviews)
- `source`: `orchestrator` | `agent` | `user`

**Changes**:
1. Add field to `MemoryEntry` in `src/domain/memory.rs`
2. Add `remember_with_metadata()` method to `MemoryService`
3. Include metadata in Qdrant payload (qdrant_memory_store.rs)
4. Update `memory_tool_defs()` to accept optional metadata in `remember` tool

**Backward compat**: `#[serde(default)]` means existing `vectors.bin` files deserialize with empty metadata. No migration needed.

**Risk**: Very low. Additive change.

---

### Phase 4: Orchestrator Auto-Summarize (Depends on Phase 3)

**What**: After multi-agent task completion, the orchestrator compresses all step results into a topic overview and stores it in memory. This gives the system long-term knowledge of what was accomplished.

**Flow**:
```
Goal: "Research IP-NFT minting on Base chain"
  -> Agent A: researches -> produces output
  -> Agent B: writes code -> produces output
  -> Orchestrator: combines outputs -> creates topic_overview memory entry:
     "Researched IP-NFT minting on Base chain. Key findings: ..."
     metadata: {type: topic_overview, goal: "Research IP-NFT...", source: orchestrator}
```

**Changes** (~30 lines per orchestrator):
1. After all batches complete, build combined text from step results (truncated)
2. If `memory_handle` is available, call `remember_with_metadata()` with topic overview
3. Content: compressed summary of key outcomes, file paths, IDs generated

**Files**:
- `src/adapters/telegram_runtime.rs` -- after batch loop in `orchestrate_team_goal()`
- `src/adapters/orchestrator.rs` -- after plan-and-execute loop

**Risk**: Low. Additive. Worst case: a failed memory write logs a warning and continues.

---

### Phase 5: RAG Context Before Planning (Depends on Phases 3+4)

**What**: Before planning, the orchestrator recalls relevant prior topic overviews and injects them as context. This prevents re-doing work and gives the planner awareness of what's been accomplished.

**Flow**:
```
User: "Mint an IP-NFT on Base chain"
  -> Orchestrator recalls: "topic_overview: Researched IP-NFT minting on Base..."
  -> Planner gets enriched goal:
     "## Relevant Prior Work
      - Previously researched IP-NFT minting on Base chain. Key findings: ...
      ## Current Goal
      Mint an IP-NFT on Base chain"
  -> Plan is better because it knows research is done
```

**Changes** (~15 lines per orchestrator):
1. If `memory_handle` available, create `MemoryService`, call `recall(goal, top_k=3, max_tokens=600)`
2. If results exist, prepend to goal text
3. Modified goal text flows into existing `generate_plan()` -- no planner changes needed

**Files**:
- `src/adapters/telegram_runtime.rs` -- before `generate_plan()` in `orchestrate_team_goal()`
- `src/adapters/orchestrator.rs` -- before `generate_plan()` in plan-and-execute path

**Risk**: Very low. If recall fails, original goal is used as-is.

---

## Implementation Order

```
Phase 1 (inline data passing)  ──┐
Phase 2 (per-sandbox memory)   ──┼── independent, can be done in any order
Phase 3 (metadata on entries)  ──┘
Phase 4 (auto-summarize)       ──── depends on Phase 3
Phase 5 (RAG before planning)  ──── depends on Phase 3, benefits from Phase 4
```

Phase 1 is the highest priority -- it directly fixes the hallucination problem that's making multi-agent tasks unreliable.

---

## What This Does NOT Include (Future Work)

These are patterns observed in the research that could be valuable later but are out of scope for this iteration:

1. **Hybrid search (vector + BM25)** -- OpenClaw and ZeroClaw use this. Would help catch exact IDs/hashes that pure cosine similarity misses. Currently we only have cosine search.

2. **Temporal decay** -- OpenClaw's `score * e^(-lambda * age_days)` with 30-day half-life. Would help relevance ranking but requires scoring changes in both disk and Qdrant backends.

3. **Pre-compaction memory flush** -- OpenClaw's pattern of auto-persisting important context before the context window truncates. Tengu already has the 80% warning; could fire a silent `remember` at that point.

4. **Role-aligned memory lenses** -- CrewAI's pattern where different agent roles weight recall differently (planner weights importance, executor weights recency). Requires metadata + scoring changes.

5. **MMR re-ranking** -- Maximal Marginal Relevance to prevent redundant recall results. Simple to add but needs scoring infrastructure first.

6. **Structured task outputs** -- CrewAI's `TaskOutput` with raw text + JSON + Pydantic model. Would add schema validation at agent handoff boundaries. Significant change to the engine response model.

---

## Verification Plan

1. `cargo build` after each phase
2. `cargo test` -- existing tests in `task_planner`, `memory_service`, `memory_store`, `domain::memory` must pass
3. **Phase 1**: Run DeSci sandbox via Telegram, verify Agent B's prompt contains Agent A's actual output (check logs)
4. **Phase 2**: Run DeSci sandbox, verify `~/desci-workspace/memory/vectors.bin` is created (not `~/.tengu/memory/`)
5. **Phase 3**: Call `remember` tool, verify metadata serializes/deserializes in vectors.bin
6. **Phase 4**: Complete a multi-agent task, verify topic_overview memory entry exists
7. **Phase 5**: Start a new conversation in same sandbox, verify prior topic overview appears in planner context
