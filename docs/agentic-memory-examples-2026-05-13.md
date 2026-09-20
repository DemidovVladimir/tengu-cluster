# Agentic Memory Examples + Sources (2026-05-13)

## 2026-05-15 Direction Change

| Previous examples | New interpretation |
|---|---|
| Tengu-native setup | Reference spike only |
| `tengu agentic-memory-server` | Example MCP shape, not final product command |
| Codex/Claude access | Primary target |
| Behavior proposal | Core feature, not future nice-to-have |

## Usual Combined Workflow

| Step | Open Brain role | LLM Wiki role |
|---|---|---|
| Capture | Store raw turn/source/chunk/embedding | None |
| Recall | Fast context retrieval for current task | Read compiled pages when stable knowledge is needed |
| Reflect | Extract claims, entities, links, duplicates | None |
| Promote | Mark stable/high-value claims | Compiler input |
| Compile | None | Update Markdown pages, links, index, contradiction notes |
| Lint | Detect stale/duplicate memories | Detect stale/orphan/contradictory wiki pages |

## Conversation Persistence

| Event | Storage |
|---|---|
| User says: "Keep docs concise." | `memory_events(kind=preference)` |
| Agent answers | `memory_events(kind=assistant_summary)` |
| Preference repeats | Claim confidence increases |
| Stable preference | Promotion candidate |
| Human approves behavior | Skill/TOML patch applied |

Example call:

```json
{
  "operation": "capture",
  "kind": "preference",
  "content": "User prefers concise, table-driven project docs.",
  "metadata": {
    "scope": "project",
    "confidence": "observed"
  }
}
```

## Add Info

| User says | Plugin action |
|---|---|
| "Remember: webhook direct responses must persist final text." | Capture event + claim |
| "Use it in reviews." | Link claim to project/review behavior |
| Later review | Recall claim with provenance |

## Add File

| Step | Action |
|---|---|
| 1 | Copy file to `raw/files/<hash>.md` |
| 2 | Store source metadata in Postgres |
| 3 | Chunk/embed for fast recall |
| 4 | Extract claims/entities |
| 5 | Promote important claims |
| 6 | Compile wiki pages from promoted claims |

Example call:

```json
{
  "operation": "ingest_source",
  "kind": "file",
  "path": "docs/webhooks-2026-05-11.md",
  "promote": true
}
```

## Add YouTube Transcript

| Step | Action |
|---|---|
| 1 | Fetch or accept transcript text |
| 2 | Store transcript under `raw/transcripts/` |
| 3 | Chunk/embed transcript |
| 4 | Extract claims and timestamps |
| 5 | Compile/update wiki topic pages |

Example call:

```json
{
  "operation": "ingest_source",
  "kind": "youtube_transcript",
  "url": "https://www.youtube.com/watch?v=...",
  "transcript_provider": "configured",
  "promote": true
}
```

## Agent Access

| Agent/client | Reads via |
|---|---|
| Tengu agents | Reference spike only |
| Claude Cowork / Claude Code | MCP server + Markdown wiki + behavior proposals |
| ChatGPT MCP clients | MCP server |
| Codex | MCP server + Markdown wiki + proposed `AGENTS.md`/skill patches |
| Humans | Markdown wiki, Git diff, SQL against `TENGU_MEMORY_DATABASE_URL` (no inspect CLI) |

The final product should expose a standalone MCP server command owned by the
portable plugin. The current `tengu agentic-memory-server` shape is only a
reference spike.

## Continuous Self-Improvement Loop

| Moment | Plugin action | Host effect |
|---|---|---|
| User repeats a preference | Increase claim confidence | Future recall includes preference |
| Project rule emerges | Promote claim | Wiki page updated with citations |
| Agent behavior mismatch appears | `propose_behavior` creates patch | Codex/Claude/Tengu shows reviewable diff |
| User approves patch | Host adapter applies it | `AGENTS.md`, skill, CLAUDE.md, or project rule changes |
| Lint finds contradiction | Report conflict + evidence | Human resolves or marks stale |

Example behavior proposal:

```json
{
  "operation": "propose_behavior",
  "target_host": "codex",
  "project": "tengu-cluster",
  "goal": "Keep docs concise and table-driven",
  "evidence": ["memory_event:preference:..."],
  "target_files": ["AGENTS.md", "docs/agentic-memory-prd-2026-05-13.md"]
}
```

## Codex / Claude Cowork Setup Shape

| Host | Minimal setup |
|---|---|
| Codex | Add the standalone plugin MCP server; expose wiki root as project context |
| Claude Cowork | Add MCP server command; point project memory/instructions at wiki + proposal queue |
| Claude Code | Add MCP server to project config; let tool return recall blocks and proposal diffs |
| Tengu | Reference spike; do not optimize the product around it |

## MVP Setup

| Step | Command/config |
|---|---|
| Start DB | `docker compose --profile postgres-memory up -d postgres-memory` (loopback / compose-internal; not routed through Tor) |
| Start plugin | Standalone MCP server command from the portable plugin |
| First write | `capture(kind="preference", content="...")` |
| Codex/Cowork | Add MCP server + wiki root to project context |
| Tengu | Optional reference spike: `cargo run --features postgres_memory -- chat --sandbox <name>` with `TENGU_MEMORY_DATABASE_URL`; embeddings go through `[egress]` (Tor by default, `network = "open"` for direct) |

## Source Summary

| Source | Key point |
|---|---|
| [Open Brain System](https://openbrainsystem.com/) | Open Brain is user-owned, machine-readable Postgres/pgvector memory exposed through MCP |
| [LLM Wiki](https://llmwiki.app/) | LLM Wiki has three layers: raw sources, LLM-owned wiki, schema |
| [Proudfrog workflow guide](https://proudfrog.com/en/insights/karpathy-llm-wiki-complete-workflow-guide) | Practical workflow is ingest/query/lint over `raw/`, `wiki/`, and schema |
| [Karpathy LLM Wiki gist](https://gist.github.com/karpathy/442a6bf555914893e9891c11519de94f) | Raw docs are compiled into Markdown wiki by an LLM |
| [A-MEM](https://huggingface.co/papers/2502.12110) | Agentic memory dynamically organizes experiences instead of only storing chunks |
| [GAM](https://huggingface.co/papers/2604.12285) | Separate fast event encoding from slower consolidation |
