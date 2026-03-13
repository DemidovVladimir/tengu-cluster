# Memory Architecture Proposal (Validated)

Validated against the current Tengu codebase on 2026-03-13 and cross-checked against current official documentation for comparable systems.

> **Status (2026-03-13):** Phases 1-5 are implemented. Phase 6 (document indexing) remains deferred as a separate effort.
> - Phase 1: Inline inter-agent data passing (both CLI and Telegram orchestrators)
> - Phase 2: Per-workspace memory store path (disk and Qdrant collection scoping)
> - Phase 3: Metadata field on MemoryEntry (`HashMap<String, String>`, `#[serde(default)]`)
> - Phase 4: Orchestrator auto-summarize (topic overviews with `kind`/`source`/`goal`/`workspace_id` metadata)
> - Phase 5: RAG context before planning (filtered recall of `kind=topic_overview, source=orchestrator`)

This document replaces the fuzzier parts of the earlier proposal with a stricter split between three different problems:

1. **Inter-agent handoff fidelity**
2. **Project/topic memory across sessions**
3. **Document retrieval over workspace knowledge**

Those are related, but they should not share one mechanism.

## Executive Summary

The current Tengu memory design has two real gaps and one architectural conflation:

1. **Telegram multi-agent handoff is unreliable**
   Dependent agents are told where prior outputs live and asked to read them, which is a weak handoff pattern for LLMs.

2. **Persistent memory is global by default**
   Disk memory uses `~/.tengu/memory/`, so sandbox knowledge is not naturally isolated by workspace.

3. **`remember` is not a document knowledge base**
   It is suitable for short durable facts, summaries, and topic overviews. It is not the right primitive for large document corpora or file-level retrieval.

The right design is:

- **Handoff context**: inline prior task results directly in the prompt
- **Project memory**: store summarized durable knowledge per workspace
- **Document retrieval**: separate workspace document index and retrieval tool
- **Planner memory**: let the orchestrator recall prior topic overviews before planning

## What Is True In The Current Repo

### 1. Telegram multi-agent handoff is hallucination-prone

In the Telegram orchestrator, dependent tasks are given file paths to previous outcomes and told to use `read_file`.

Relevant code:
- [telegram_runtime.rs](/Users/vladimirdemidov/development/tengu-cluster/src/adapters/telegram_runtime.rs#L250)
- [telegram_runtime.rs](/Users/vladimirdemidov/development/tengu-cluster/src/adapters/telegram_runtime.rs#L267)

That is weaker than the CLI orchestrator, which embeds prior step output inline into the next task prompt.

Relevant code:
- [orchestrator.rs](/Users/vladimirdemidov/development/tengu-cluster/src/adapters/orchestrator.rs#L582)
- [orchestrator.rs](/Users/vladimirdemidov/development/tengu-cluster/src/adapters/orchestrator.rs#L63)

This is the highest-priority reliability problem.

### 2. Memory is global by default

The configured disk memory path defaults to `~/.tengu/memory/`.

Relevant code:
- [schema.rs](/Users/vladimirdemidov/development/tengu-cluster/crates/tengu-core/src/config/schema.rs#L376)
- [CONFIGURATION.md](/Users/vladimirdemidov/development/tengu-cluster/docs/CONFIGURATION.md#L383)
- [channel_runtime.rs](/Users/vladimirdemidov/development/tengu-cluster/src/adapters/channel_runtime.rs#L250)

That means DeSci and WebStudio can share the same disk memory store unless the user overrides config manually.

### 3. Orchestrated tasks do not get automatic memory recall

Normal chat turns use `ChatRuntimeService`, which recalls relevant memories before the engine call.

Relevant code:
- [chat_runtime.rs](/Users/vladimirdemidov/development/tengu-cluster/src/application/chat_runtime.rs#L142)

But orchestrated Telegram tasks use direct engine execution for concurrency and do not pass through that recall path.

Relevant code:
- [telegram_runtime.rs](/Users/vladimirdemidov/development/tengu-cluster/src/adapters/telegram_runtime.rs#L282)
- [telegram_runtime.rs](/Users/vladimirdemidov/development/tengu-cluster/src/adapters/telegram_runtime.rs#L616)

### 4. Memory entries do not currently carry metadata

`MemoryEntry` contains:
- `id`
- `content`
- `embedding`
- `agent_id`
- `created_at_epoch_s`

Relevant code:
- [memory.rs](/Users/vladimirdemidov/development/tengu-cluster/src/domain/memory.rs#L18)

That makes topic scoping, source tagging, and planner-specific recall harder than they should be.

## What Comparable Systems Actually Suggest

These comparisons should be used carefully. They support architectural direction, not one-to-one cloning.

### OpenClaw

OpenClaw clearly separates:
- agent-local searchable memory
- workspace-local markdown memory artifacts
- durable summarized notes

Useful takeaway for Tengu:
- **workspace/project isolation is a good default**
- **summaries are durable memory**
- **raw transcripts are not the same thing as curated memory**

Source:
- https://docs.openclaw.ai/concepts/memory

### CrewAI

CrewAI supports task context passing and persistent memory, but the relevant lesson is not “copy CrewAI’s storage backend.” The useful lesson is:

- task outputs and context should be forwarded explicitly
- long-term memory should store durable extracted knowledge, not entire unfiltered chats

Current docs describe a unified memory system and default storage via LanceDB rather than the older simplified descriptions people often repeat.

Sources:
- https://docs.crewai.com/concepts/tasks
- https://docs.crewai.com/en/concepts/memory

### Google ADK

Google ADK’s state model supports explicit state transfer. The useful lesson is:

- **verbatim shared state is lower-risk than “please go read this file”**

Source:
- https://google.github.io/adk-docs/sessions/state/

### AutoGen

AutoGen supports memory and explicit message/result passing, but the main lesson here is:

- planning and execution benefit from explicit carryover context

Source:
- https://microsoft.github.io/autogen/stable/user-guide/agentchat-user-guide/memory.html

### Hermes

Hermes is relevant mostly as a reminder that:

- cross-session memory should be summarized and searchable
- not every prior message belongs in the live context window

Source:
- https://hermes-agent.nousresearch.com/docs/user-guide/features/memory/

### Letta

Letta’s useful idea is conceptual:

- retrieval is a capability inside an agentic runtime, not a separate substitute for memory management

Source:
- https://docs.letta.com/guides/agents/memory

## The Correct Split: Three Memory Layers

### Layer 1: Handoff Context

Purpose:
- move accurate outputs from one agent to the next during one orchestrated run

This is **not** vector memory.

It should use:
- direct prompt embedding of prior task outputs
- truncation with clear boundaries
- optional file persistence for audit/debugging

This is the fix for current inter-agent hallucination.

### Layer 2: Project / Topic Memory

Purpose:
- store durable facts, decisions, IDs, summaries, and topic overviews for a workspace across sessions

This is what `remember` should support well.

It should store:
- research conclusions
- generated identifiers and URLs
- prior run summaries
- “topic overview” entries created by the orchestrator

It should not be used as:
- full document ingestion
- raw conversation dump by default

### Layer 3: Document Retrieval

Purpose:
- search the workspace corpus: PDFs, Markdown, specs, notes, generated outputs

This should be a separate subsystem from `remember`.

Why:
- documents are larger
- they need chunking
- retrieval should preserve file path provenance
- results should point back to file chunks, not pretend they are durable summarized memory

This is the right mechanism for “store documents and retrieve them later via embeddings.”

## Recommended Architecture

## Phase 1: Fix Inter-Agent Handoff

Port the CLI orchestrator handoff pattern to Telegram:

- store each completed task output in memory only if needed for later phases
- always store the output text in an in-memory map for the current run
- pass actual prior output inline to dependent agents
- keep outcome files for audit/debugging, but do not rely on agents to read them

Concrete shape:

- `HashMap<String, String>` for current-run task output text
- `build_task_prompt()` takes prior output text, not just paths
- shared truncation helper for both orchestrators

This is the highest-priority change.

## Phase 2: Scope Persistent Memory To Workspace By Default

When a sandbox agent has a workspace, default disk memory should live under:

- `<workspace>/memory/`

Fallback:

- if no workspace exists, use existing global path behavior

This gives isolation without requiring config churn for every sandbox.

Important nuance:
- if the backend is Qdrant, path scoping alone is not enough
- Qdrant should eventually scope by collection or payload filter as well

So Phase 2 should explicitly cover both backends:
- disk: separate directory
- qdrant: separate collection or required workspace metadata filter

## Phase 3: Add Memory Metadata

Add:

```rust
metadata: HashMap<String, String>
```

Recommended keys:
- `kind`: `fact`, `decision`, `outcome`, `topic_overview`
- `workspace_id`
- `topic`
- `source`: `user`, `agent`, `orchestrator`
- `run_id`

This is the foundation for better recall and planner memory.

## Phase 4: Add Orchestrator Topic Overviews

After a multi-agent run completes:

- compress the run into one durable topic summary
- store it in project memory with metadata

This should include:
- goal
- key outputs
- important file paths
- IDs / URLs / hashes
- unresolved follow-ups

This gives the orchestrator durable knowledge without replaying old conversations.

## Phase 5: Recall Topic Overviews Before Planning

Before classifying or planning a new request:

- embed the incoming request
- recall top relevant topic overviews from project memory
- inject only the compact summaries into the planner context

This is the right place for orchestrator memory.

It should not inject:
- raw full conversations
- full document bodies
- arbitrary unrelated memories

## Phase 6: Add Document Indexing As A Separate Capability

This is the missing piece in the earlier proposal.

Project memory and document retrieval should not be the same store.

Add a separate workspace knowledge index:
- chunk files
- store chunk embeddings
- store path + chunk metadata
- expose a dedicated retrieval tool, e.g. `search_project_knowledge`

This tool should return:
- path
- chunk excerpt
- score
- optional line/page metadata

Use cases:
- “find prior POI registration details”
- “search the paper corpus for assay method”
- “retrieve the section mentioning wallet policy”

## Conversation Handling Policy

The orchestrator should not store every raw message by default.

Instead:

- keep normal short-term chat state as it works today
- when useful, compress completed work into topic overviews
- optionally add explicit “conversation summary” entries only at session boundaries or compaction boundaries

That keeps memory durable without polluting recall with noise.

## Recommended Data Model

### 1. Handoff Cache

Ephemeral, per run:

```rust
HashMap<TaskId, TaskOutput>
```

Where `TaskOutput` holds:
- raw output text
- truncated prompt-safe form
- audit file path

### 2. Durable Project Memory

Backed by disk or Qdrant:

```rust
MemoryEntry {
    id,
    content,
    embedding,
    agent_id,
    created_at_epoch_s,
    metadata,
}
```

### 3. Document Index

Separate store or separate collection/table:

```rust
DocumentChunk {
    id,
    workspace_id,
    path,
    chunk_text,
    embedding,
    chunk_index,
    source_type,
    created_at_epoch_s,
}
```

## What To Build First

Priority order:

1. Inline Telegram handoff context
2. Workspace-scoped memory
3. Metadata on `MemoryEntry`
4. Orchestrator topic overviews
5. Planner recall from topic overviews
6. Separate document retrieval index

Why this order:
- 1 fixes the current reliability failure
- 2 prevents cross-sandbox contamination
- 3 unlocks useful filtering and summaries
- 4 and 5 make the orchestrator meaningfully stateful
- 6 solves document retrieval cleanly instead of overloading `remember`

## What To Avoid

- Do not use `remember` as the only storage mechanism for large documents.
- Do not inject raw entire conversations into planner context.
- Do not treat all retrieved memories as equally relevant across workspaces.
- Do not rely on “agent, please read this file” as the handoff boundary for dependent tasks.

## Validation Checklist

### Handoff

- dependent Telegram task prompts contain actual prior output text
- dependent agents no longer need `read_file` to understand prior task results

### Memory isolation

- DeSci and WebStudio create distinct memory locations by default
- Qdrant mode also isolates workspace memory logically

### Topic memory

- completed multi-agent runs create `topic_overview` entries
- a later related request recalls those entries before planning

### Document retrieval

- document search returns file-grounded chunks with provenance
- planner memory and document retrieval remain separate paths

## Sources

- OpenClaw memory: https://docs.openclaw.ai/concepts/memory
- CrewAI tasks: https://docs.crewai.com/concepts/tasks
- CrewAI memory: https://docs.crewai.com/en/concepts/memory
- Google ADK state: https://google.github.io/adk-docs/sessions/state/
- AutoGen memory: https://microsoft.github.io/autogen/stable/user-guide/agentchat-user-guide/memory.html
- Hermes memory: https://hermes-agent.nousresearch.com/docs/user-guide/features/memory/
- Letta memory: https://docs.letta.com/guides/agents/memory

