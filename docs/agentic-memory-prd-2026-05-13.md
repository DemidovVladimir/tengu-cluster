# Agentic Memory Plugin PRD (2026-05-13)

## 2026-05-15 Direction Change

| Old target | New target |
|---|---|
| Tengu memory replacement | Standalone portable memory plugin |
| Native Rust integration first | Host-neutral core + MCP first |
| Tengu as product host | Tengu as reference spike only |
| Planner-specific recall | Codex/Claude/Cowork adjustable project memory |

Canonical short doc: `docs/portable-agentic-memory-plugin-2026-05-15.md`.

## Correction

| Earlier doc got wrong | Correct model |
|---|---|
| Treated Open Brain and LLM Wiki as peer stores | Open Brain is live agent memory; LLM Wiki is compiled knowledge |
| Made wiki a database table first | Wiki is Markdown first; Postgres stores source/index metadata |
| Made implementation too broad | Start with capture, recall, promote, compile, lint |
| Scoped it as Tengu-only replacement | Build a portable hybrid memory plugin; Tengu is one adapter |

## Product

| Field | Decision |
|---|---|
| Name | `agentic_memory` |
| Goal | Portable self-improving memory plugin bundling Open Brain live memory + Karpathy LLM Wiki consolidation |
| Storage | Postgres + pgvector for live memory; Markdown files for compiled wiki; project policy patches as proposals |
| Access | Core service + MCP server + host adapters for Tengu, Codex, Claude Cowork, Claude Code |
| Policy | Agents may capture/recall/promote/compile/propose; humans approve behavior edits |

## Mental Model

```text
raw events/sources -> Open Brain memory -> promotion queue -> LLM Wiki compiler -> wiki/*.md
       ^                    |                        |                  |
       |                    v                        v                  v
 conversations       fast agent recall       reviewed claims      durable synthesis
 files/transcripts   graph/vector search     contradictions      human-readable memory
 host behavior        behavior proposals      adapter rules       project alignment
```

## Product Boundary

| Layer | Owns | Does not own |
|---|---|---|
| Core memory service | Postgres schema, raw store, recall, promotion, wiki compiler, lint, proposal engine | Host-specific UI |
| MCP surface | Tool calls any MCP-capable agent can use | Host process lifecycle |
| Host adapters | Codex/Claude/Tengu prompt injection, project files, behavior proposal application | Memory schema |
| Project policy | `.agentic-memory/AGENTS.md`, wiki pages, proposal queue | Silent edits to user config |

## Requirements

| ID | Requirement | Acceptance |
|---|---|---|
| R1 | Portable plugin core | Same memory can be used from Tengu, Codex, Claude Cowork, or Claude Code |
| R2 | Capture raw memory | Turns, files, URLs, transcripts stored with provenance |
| R3 | Recall fast context | Hybrid vector/full-text/metadata recall from Postgres |
| R4 | Promote stable knowledge | Repeated/high-value memories become promotion candidates |
| R5 | Compile wiki | LLM updates Markdown pages from promoted sources/claims |
| R6 | Lint memory | Detect stale, duplicate, orphaned, and contradictory records/pages |
| R7 | Adjust host behavior safely | Generate reviewable patches for AGENTS.md, skills, CLAUDE.md, Codex instructions, or project rules |
| R8 | Expose to any agent | MCP server is first-class; Tengu integration is optional/reference only |

## Non-Goals

| Non-goal | Reason |
|---|---|
| Multiple SQL providers | User wants Postgres only |
| Legacy vector compatibility | Replacement, not integration |
| Wiki as raw source of truth | Raw sources and event records are source of truth |
| Auto-behavior mutation | Too risky without review |
| Store every tool byte forever | Summarize/noise-filter tool output |

## Host Adapters

| Host | Adapter output |
|---|---|
| Codex | Reads wiki + recall block; proposes `AGENTS.md`/skill/doc patches |
| Claude Cowork | Reads via MCP + wiki files; proposes project-memory/instruction updates |
| Claude Code | Uses MCP stdio tool + local Markdown wiki |
| Tengu | Reference spike only; do not optimize product architecture around it |
| Any MCP client | `capture`, `recall`, `ingest_source`, `promote`, `compile_wiki`, `lint`, `propose_behavior` |

## Source Pattern

| Source | What to copy |
|---|---|
| Open Brain | User-owned Postgres/pgvector memory, MCP-accessible to agents |
| Open Brain server pattern | Structured memories, graph links, dedup/stale maintenance |
| Karpathy LLM Wiki | `raw/` immutable sources, `wiki/` LLM-owned Markdown, schema file, ingest/query/lint |
| Agentic memory papers | Separate fast encoding from slower consolidation |

## Success

| Metric | Target |
|---|---|
| Recall | Prior preference/decision appears in top 5 |
| Wiki quality | Answers use compiled wiki before raw chunks |
| Provenance | Every claim links to source/event ids |
| Safety | Behavior change has explicit proposal and evidence |
| Portability | Claude/ChatGPT/Codex/Tengu can read via MCP or files |

## References

| Reference | Use |
|---|---|
| [Open Brain System](https://openbrainsystem.com/) | Defines open brain as user-owned Postgres/pgvector memory exposed through MCP |
| [Open Brain server](https://github.com/Bobby-cell-commits/open-brain-server) | Example Open Brain-style MCP memory server |
| [LLM Wiki](https://llmwiki.app/) | Open-source implementation of Karpathy's LLM Wiki pattern |
| [Karpathy LLM Wiki gist](https://gist.github.com/karpathy/442a6bf555914893e9891c11519de94f) | Original raw/wiki/schema idea |
| [Proudfrog LLM Wiki workflow](https://proudfrog.com/en/insights/karpathy-llm-wiki-complete-workflow-guide) | Practical raw/wiki/schema and ingest/query/lint summary |
| [A-MEM paper](https://huggingface.co/papers/2502.12110) | Dynamic agentic organization of memory |
| [GAM paper](https://huggingface.co/papers/2604.12285) | Fast event memory separated from slower consolidation |
