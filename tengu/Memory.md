---
tags:
  - core
  - memory
  - architecture
  - rag
aliases:
  - Memory Architecture
  - Vector Memory
  - RAG
---

# Memory

Tengu has **permanent vector memory** — [[Agents]] remember across sessions, enabling continuity in a 24/7 multi-agent system. The memory architecture was validated on 2026-03-13 and cross-checked against official documentation for comparable systems.

## Implementation Status

| Phase | Description | Status |
|-------|-------------|--------|
| Phase 1 | Inline inter-agent data passing (both CLI and Telegram orchestrators) | Implemented |
| Phase 2 | Per-workspace memory store path (disk and Qdrant collection scoping) | Implemented |
| Phase 3 | Metadata field on MemoryEntry (`HashMap<String, String>`, `#[serde(default)]`) | Implemented |
| Phase 4 | Orchestrator auto-summarize (topic overviews with `kind`/`source`/`goal`/`workspace_id` metadata) | Implemented |
| Phase 5 | RAG context before planning (filtered recall of `kind=topic_overview, source=orchestrator`) | Implemented |
| Phase 6 | Document indexing (separate chunked retrieval subsystem) | Deferred |

## Executive Summary

The memory design addresses two real gaps and one architectural conflation:

1. **Telegram multi-agent handoff was unreliable** — dependent agents were told where prior outputs live and asked to read them, which is a weak handoff pattern for LLMs. Fixed by inline data passing (Phase 1).

2. **Persistent memory was global by default** — disk memory used `~/.tengu/memory/`, so sandbox knowledge was not naturally isolated by workspace. Fixed by per-workspace scoping (Phase 2).

3. **`remember` is not a document knowledge base** — it is suitable for short durable facts, summaries, and topic overviews. It is not the right primitive for large document corpora or file-level retrieval. Document retrieval is a separate future subsystem (Phase 6).

The correct design is:

- **Handoff context**: inline prior task results directly in the prompt
- **Project memory**: store summarized durable knowledge per workspace
- **Document retrieval**: separate workspace document index and retrieval tool
- **Planner memory**: let the [[Orchestrator]] recall prior topic overviews before planning

---

## The Three Memory Layers

### Layer 1: Handoff Context

**Purpose**: Move accurate outputs from one [[Agents|agent]] to the next during one orchestrated run.

This is **not** vector memory. It uses:

- Direct prompt embedding of prior task outputs
- Truncation with clear boundaries (up to 3000 chars, char-boundary-safe)
- Optional file persistence for audit/debugging (outcome files written to `.tengu-tasks/`)

This is the fix for inter-agent hallucination. The CLI [[Orchestrator]] already used this pattern via `build_step_context()` in `orchestrator.rs`, which embeds actual output text inline (truncated to `MAX_STEP_CONTEXT_CHARS = 3000`). Agent B sees the real data in its prompt, no file read needed.

**Why the old approach failed**: In the Telegram orchestrator, dependent tasks were given file paths to previous outcomes and told to use `read_file`. The LLM often ignored the `read_file` instruction and hallucinated the content. This is a known pattern across frameworks — giving an LLM a pointer to data and hoping it will dereference it is unreliable.

**Evidence from research**: CrewAI's `context=[task_a, task_b]` injects actual TaskOutput content. Google ADK's `output_key` pattern gives agents verbatim data via shared state. Both have low hallucination risk for data transfer.

**Implementation details**:

- `outcome_paths: HashMap<String, String>` was changed to `HashMap<String, (String, String)>` mapping `task_id -> (file_path, output_text)`
- `build_task_prompt()` takes prior output text, not just paths — dependency output is embedded inline in the prompt
- Shared `truncate_output()` helper extracted to `channel_runtime.rs`, used by both orchestrators
- Outcome files still written for auditability, but the prompt no longer depends on agents reading them

**Data model** — ephemeral, per run:

```rust
HashMap<TaskId, TaskOutput>
```

Where `TaskOutput` holds:
- raw output text
- truncated prompt-safe form
- audit file path

**Files**:
- `src/adapters/telegram_runtime.rs` — `build_task_prompt()`, batch result collection in `orchestrate_team_goal()`
- `src/adapters/channel_runtime.rs` — shared `truncate_output()` helper
- `src/adapters/orchestrator.rs` — uses shared truncation helper

### Layer 2: Project / Topic Memory

**Purpose**: Store durable facts, decisions, IDs, summaries, and topic overviews for a workspace across sessions.

This is what `remember` supports well. It stores:

- Research conclusions
- Generated identifiers and URLs
- Prior run summaries
- "Topic overview" entries created by the [[Orchestrator]]

It should **not** be used as:

- Full document ingestion
- Raw conversation dump by default

**Data model** — backed by disk or Qdrant:

```rust
MemoryEntry {
    id: String,
    content: String,
    embedding: Vec<f32>,
    agent_id: String,
    created_at_epoch_s: u64,
    #[serde(default)]
    metadata: HashMap<String, String>,
}
```

**Standard metadata keys** (convention, not enforced):

| Key | Values | Purpose |
|-----|--------|---------|
| `kind` | `fact`, `decision`, `outcome`, `topic_overview` | Type classification |
| `workspace_id` | workspace name | Workspace scoping |
| `topic` | free text | Topic tagging |
| `source` | `user`, `agent`, `orchestrator` | Origin tracking |
| `run_id` | UUID | Run correlation |
| `goal` | originating goal text | For topic overviews |

**Backward compat**: `#[serde(default)]` means existing `vectors.bin` files deserialize with empty metadata. No migration needed.

### Layer 3: Document Retrieval (Future — Phase 6)

**Purpose**: Search the workspace corpus — PDFs, Markdown, specs, notes, generated outputs.

This is a separate subsystem from `remember`. Project memory and document retrieval should not be the same store.

**Why separate**:

- Documents are larger
- They need chunking
- Retrieval should preserve file path provenance
- Results should point back to file chunks, not pretend they are durable summarized memory

**Proposed data model** — separate store or separate collection/table:

```rust
DocumentChunk {
    id: String,
    workspace_id: String,
    path: String,
    chunk_text: String,
    embedding: Vec<f32>,
    chunk_index: usize,
    source_type: String,
    created_at_epoch_s: u64,
}
```

**Proposed tool**: `search_project_knowledge` returning:
- path
- chunk excerpt
- score
- optional line/page metadata

**Use cases**:
- "find prior POI registration details"
- "search the paper corpus for assay method"
- "retrieve the section mentioning wallet policy"

---

## Backends

| Backend | Storage | Search | Feature Flag |
|---------|---------|--------|-------------|
| **Disk** (default) | `<workspace>/memory/` bincode files | Brute-force cosine | Always available |
| **Qdrant** | gRPC collection `tengu-memory-<ws>` | ANN cosine | `--features qdrant` |

## Per-Workspace Isolation

Each workspace gets its own memory store, preventing cross-sandbox pollution (e.g., DeSci memories do not pollute WebStudio recall):

- **Disk**: `<workspace>/memory/vectors.bin`
- **Qdrant**: collection `tengu-memory-<workspace_name>`, metadata stored as `meta_*` payload keys

**Fallback**: If no workspace exists (e.g., single-agent TUI), the existing global path `~/.tengu/memory/` is used unchanged.

**Resolution**: `resolve_memory_store_path()` in `channel_runtime.rs`:

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

Qdrant collection scoping uses `resolve_qdrant_collection()` which returns `tengu-memory-<ws_name>`.

Initialized via `build_memory_handle(workspace: Option<&Path>)` in [[Channels|channel_runtime.rs]] — shared across all adapters (TUI, Telegram, CLI orchestrator).

## Memory [[Tools]]

Tools registered by the memory subsystem (defined in `memory_tool_executor.rs` via `memory_tool_defs()`):

- **`remember`** — store content with optional metadata (`kind`, `topic`, `source`, etc.)
- **`recall`** — vector search for similar entries

`recall_filtered()` post-filters by required metadata key-value pairs (fetches 3x `top_k` to compensate for filtered-out results).

The `remember_with_metadata()` method on `MemoryService` enables Phase 3+ functionality — storing entries with arbitrary metadata tags.

## Embedding

Default: OpenRouter `text-embedding-3-small` (1536 dimensions).

Configured via `EmbeddingPort` in [[Architecture|application ports]] (`src/application/ports.rs`). Adapter implementation in `src/adapters/embedding.rs`.

## [[Orchestrator]] Integration

The [[Orchestrator]] integrates with memory at three levels:

### Auto-Summarize (Phase 4)

After a multi-agent run completes, the orchestrator compresses all step results into a topic overview and stores it in memory. This gives the system long-term knowledge of what was accomplished.

Flow:
```
Goal: "Research IP-NFT minting on Base chain"
  -> Agent A: researches -> produces output
  -> Agent B: writes code -> produces output
  -> Orchestrator: combines outputs -> creates topic_overview memory entry:
     "Researched IP-NFT minting on Base chain. Key findings: ..."
     metadata: {kind: topic_overview, goal: "Research IP-NFT...", source: orchestrator}
```

The topic overview includes:
- Goal
- Key outputs
- Important file paths
- IDs / URLs / hashes
- Unresolved follow-ups

**Files**:
- `src/adapters/telegram_runtime.rs` — after batch loop in `orchestrate_team_goal()`
- `src/adapters/orchestrator.rs` — after plan-and-execute loop

### RAG Planner Recall (Phase 5)

Before planning a new request:

1. Embed the incoming request
2. Recall top relevant topic overviews from project memory (filtered to `kind=topic_overview, source=orchestrator`)
3. Inject only the compact summaries into the planner context

Flow:
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

It does **not** inject:
- Raw full conversations
- Full document bodies
- Arbitrary unrelated memories

**Files**:
- `src/adapters/telegram_runtime.rs` — before `generate_plan()` in `orchestrate_team_goal()`
- `src/adapters/orchestrator.rs` — before `generate_plan()` in plan-and-execute path

### Conversation Handling Policy

The [[Orchestrator]] does not store every raw message by default. Instead:

- Keep normal short-term chat state as it works today
- When useful, compress completed work into topic overviews
- Optionally add explicit "conversation summary" entries only at session boundaries or compaction boundaries

That keeps memory durable without polluting recall with noise.

---

## Research Comparison

These comparisons support architectural direction, not one-to-one cloning.

### Three Dominant Patterns (from 8 frameworks)

| Pattern | Frameworks | Hallucination Risk | Tengu Fit |
|---------|-----------|-------------------|-----------|
| **Shared mutable state** | LangGraph, Google ADK | Low (verbatim) | Poor — Tengu is file-based, not state-dict |
| **Task output forwarding** | CrewAI, AutoGen | Low-Medium (structured) | **Best fit** — already have task results |
| **Session isolation + message passing** | OpenClaw, Hermes | Medium (lossy text) | Current approach for single-agent |

### OpenClaw

OpenClaw clearly separates:
- agent-local searchable memory
- workspace-local markdown memory artifacts
- durable summarized notes

Useful takeaways for Tengu:
- **Workspace/project isolation is a good default**
- **Summaries are durable memory**
- **Raw transcripts are not the same thing as curated memory**

Each agent gets its own SQLite index at `~/.openclaw/memory/<agentId>.sqlite`. Strict isolation.

OpenClaw also uses:
- **Hybrid search (vector + BM25)** — catches exact tokens (IDs, hashes) that pure embedding search misses
- **Temporal decay** — `score * e^(-lambda * age_days)`, half-life 30 days. Evergreen entries exempt
- **Pre-compaction memory flush** — auto-persists important context before the context window truncates (silent agentic turn to write durable memories)

Source: https://docs.openclaw.ai/concepts/memory

### CrewAI

CrewAI supports task context passing and persistent memory. The relevant lessons:

- Task outputs and context should be forwarded explicitly
- Long-term memory should store durable extracted knowledge, not entire unfiltered chats
- Current docs describe a unified memory system and default storage via LanceDB

CrewAI also features:
- **Cognitive scoring** — same knowledge, different recall lenses per agent role. Planning agent weights importance; execution agent weights recency

Sources:
- https://docs.crewai.com/concepts/tasks
- https://docs.crewai.com/en/concepts/memory

### Google ADK

Google ADK's state model supports explicit state transfer. The useful lesson:

- **Verbatim shared state is lower-risk than "please go read this file"**

Source: https://google.github.io/adk-docs/sessions/state/

### AutoGen

AutoGen supports memory and explicit message/result passing. The main lesson:

- Planning and execution benefit from explicit carryover context

AutoGen maintains full conversation history, enabling cross-session recall.

Source: https://microsoft.github.io/autogen/stable/user-guide/agentchat-user-guide/memory.html

### Hermes

Hermes is relevant as a reminder that:

- Cross-session memory should be summarized and searchable
- Not every prior message belongs in the live context window

Hermes uses LLM summarization + FTS5 for cross-session recall.

Source: https://hermes-agent.nousresearch.com/docs/user-guide/features/memory/

### Letta

Letta's useful idea is conceptual:

- Retrieval is a capability inside an agentic runtime, not a separate substitute for memory management
- RAG should be a *tool within* an agentic framework, not a standalone pipeline — avoids "context pollution" from irrelevant retrieved chunks

Source: https://docs.letta.com/guides/agents/memory

### Key Insights from Research

1. **CrewAI's cognitive scoring** — same knowledge, different recall lenses per agent role. Planning agent weights importance; execution agent weights recency.
2. **OpenClaw's hybrid search** — vector + BM25 catches exact tokens (IDs, hashes) that pure embedding search misses.
3. **OpenClaw's temporal decay** — `score * e^(-lambda * age_days)`, half-life 30 days. Evergreen entries exempt.
4. **Intrinsic Memory Agents paper** (arxiv 2508.08997) — heterogeneous role-aligned memory outperforms shared homogeneous memory for role consistency.
5. **Letta's insight** — RAG should be a *tool within* an agentic framework, not a standalone pipeline. Avoids "context pollution" from irrelevant retrieved chunks.

---

## What To Avoid

- Do not use `remember` as the only storage mechanism for large documents.
- Do not inject raw entire conversations into planner context.
- Do not treat all retrieved memories as equally relevant across workspaces.
- Do not rely on "agent, please read this file" as the handoff boundary for dependent tasks.
- Do not store every raw message by default — compress completed work into topic overviews instead.

---

## Future Work

These are patterns observed in the research that could be valuable later but are out of scope for the current iteration:

1. **Document indexing (Phase 6)** — Separate workspace knowledge index with chunked files, chunk embeddings, path + chunk metadata, and a dedicated `search_project_knowledge` retrieval tool. This is the missing piece that would handle large document corpora cleanly without overloading `remember`.

2. **Hybrid search (vector + BM25)** — OpenClaw and ZeroClaw use this. Would help catch exact IDs/hashes that pure cosine similarity misses. Currently Tengu only has cosine search.

3. **Temporal decay** — OpenClaw's `score * e^(-lambda * age_days)` with 30-day half-life. Would help relevance ranking but requires scoring changes in both disk and Qdrant backends.

4. **Pre-compaction memory flush** — OpenClaw's pattern of auto-persisting important context before the context window truncates. Tengu already has the 80% warning; could fire a silent `remember` at that point.

5. **Role-aligned memory lenses** — CrewAI's pattern where different agent roles weight recall differently (planner weights importance, executor weights recency). Requires metadata + scoring changes.

6. **MMR re-ranking** — Maximal Marginal Relevance to prevent redundant recall results. Simple to add but needs scoring infrastructure first.

7. **Structured task outputs** — CrewAI's `TaskOutput` with raw text + JSON + Pydantic model. Would add schema validation at agent handoff boundaries. Significant change to the engine response model.

---

## Validation Checklist

### Handoff

- Dependent Telegram task prompts contain actual prior output text
- Dependent agents no longer need `read_file` to understand prior task results

### Memory Isolation

- DeSci and WebStudio create distinct memory locations by default
- Qdrant mode also isolates workspace memory logically

### Topic Memory

- Completed multi-agent runs create `topic_overview` entries
- A later related request recalls those entries before planning

### Document Retrieval (Future)

- Document search returns file-grounded chunks with provenance
- Planner memory and document retrieval remain separate paths

---

## Key Files

| File | Purpose |
|------|---------|
| `src/domain/memory.rs` | Domain types: `MemoryEntry`, `MemorySearchResult`, `cosine_similarity`, `budget_memories` |
| `src/application/memory_service.rs` | Application service: `remember()`, `remember_with_metadata()`, `recall()`, `recall_filtered()`, `forget()` |
| `src/application/ports.rs` | Ports: `EmbeddingPort` and `MemoryStorePort` (async `Pin<Box<Future>>`) |
| `src/adapters/memory_store.rs` | Disk adapter: brute-force cosine, bincode persistence |
| `src/adapters/qdrant_memory_store.rs` | Qdrant adapter: gRPC/tonic, ANN cosine, feature-gated `--features qdrant` |
| `src/adapters/embedding.rs` | Embedding adapter: OpenRouter API, `text-embedding-3-small`, 1536 dims |
| `src/adapters/memory_tool_executor.rs` | Memory tool definitions: `memory_tool_defs()` |
| `src/adapters/channel_runtime.rs` | Shared logic: `build_memory_handle()`, `resolve_memory_store_path()`, `resolve_qdrant_collection()`, `truncate_output()` |

## Related

- [[Orchestrator]] — auto-summarize and recall
- [[Agents]] — who uses memory
- [[Tools]] — memory tools (remember, recall)
- [[Configuration]] — memory backend config
- [[Architecture]] — hexagonal architecture with ports and adapters
- [[Channels]] — channel adapters share memory init logic
