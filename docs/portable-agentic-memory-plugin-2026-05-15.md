# Portable Agentic Memory Plugin (2026-05-15)

## Decision

| Topic | Decision |
|---|---|
| Product | Standalone hybrid memory plugin, not a Tengu-cluster feature |
| Brain model | Open Brain live memory + Karpathy LLM Wiki compiled memory |
| Primary hosts | Codex, Claude Cowork, Claude Code, any MCP-capable agent |
| Tengu status | Reference spike only; likely not the target architecture |
| Safety | Self-improvement creates reviewed proposals, never silent edits |

## Shape

| Layer | Owns | Storage |
|---|---|---|
| Open Brain | Live events, preferences, decisions, files, transcripts, embeddings, graph links | Postgres + pgvector |
| LLM Wiki | Stable compiled knowledge, project/user profiles, operating rules | Markdown wiki + schema |
| Proposal engine | Suggested host behavior changes with evidence | Patch queue / Git diff |
| Host adapter | Inject recall/wiki context and apply approved proposals | Codex/Claude/Cowork-specific files |
| MCP surface | Portable tool API for any agent | stdio/http MCP server |

## Core Loop

| Step | Action | Result |
|---|---|---|
| 1 | Capture conversation/source/tool summary | Raw event with provenance |
| 2 | Recall before work | Context block + wiki page refs |
| 3 | Reflect/promote | Stable claims and preferences selected |
| 4 | Compile wiki | Durable Markdown memory updated |
| 5 | Propose behavior | Reviewable patch for host instructions |
| 6 | Human approves | Host behavior becomes better aligned |

## Tool API

| Tool | Purpose |
|---|---|
| `capture` | Store turn, preference, decision, source, or summary |
| `recall` | Return budgeted Open Brain + wiki context |
| `ingest_source` | Add file, URL, note, transcript |
| `promote` | Mark memory as stable enough for wiki |
| `compile_wiki` | Update Markdown wiki from promoted evidence |
| `lint` | Find duplicates, stale facts, contradictions, orphan pages |
| `propose_behavior` | Generate patch for Codex/Claude/Cowork project behavior |
| `apply_approved` | Apply only explicitly approved proposal |

## Host Adapters

| Host | Reads | Proposed writes |
|---|---|---|
| Codex | MCP recall + Markdown wiki | `AGENTS.md`, skills, local plugin config |
| Claude Cowork | MCP recall + project wiki | project instructions, memory files, workflow rules |
| Claude Code | MCP recall + wiki | `CLAUDE.md`, project rules |
| Generic MCP agent | MCP tools | proposal artifact only |

## Implementation Direction

| Phase | Build |
|---|---|
| 1 | New standalone repo/package for memory core + MCP server |
| 2 | Postgres schema + raw/wiki filesystem layout |
| 3 | MCP tools with host-neutral JSON contracts |
| 4 | Codex adapter: recall injection + proposal patches |
| 5 | Claude Cowork/Claude Code adapter |
| 6 | Optional migration from Tengu spike |

## Non-Goals

| Non-goal | Reason |
|---|---|
| Deep Tengu integration | Rust/orchestrator architecture is not the product target |
| Silent self-modification | Too risky; proposals need approval |
| Multiple SQL backends | Postgres only |
| Wiki-only memory | Raw events remain source of truth |
