# Agentic Memory Implementation (2026-05-13)

## 2026-05-15 Direction Change

| Decision | Impact |
|---|---|
| Build standalone portable plugin | Do not continue optimizing Tengu-specific Rust integration |
| MCP is primary adapter | Codex/Claude/Cowork can all use the same memory core |
| Tengu work is a spike | Keep useful schema/tool lessons; do not make it the product boundary |
| Behavior adjustment is host-specific | Proposal engine emits patches for `AGENTS.md`, `CLAUDE.md`, skills, or project rules |

## One-Screen Plan

| Phase | Build | Done when |
|---|---|---|
| 1 | Standalone core | New package/repo owns schema, raw store, wiki, proposal queue |
| 2 | MCP server | Any agent can call capture/recall/ingest/promote/compile/lint/propose |
| 3 | Codex adapter | Recall/wiki context + reviewed `AGENTS.md`/skill proposals |
| 4 | Claude Cowork adapter | Recall/wiki context + reviewed project-instruction proposals |
| 5 | Claude Code adapter | MCP + `CLAUDE.md`/project-rule proposals |
| 6 | Tengu migration decision | Reuse or discard the Rust spike |

## Current Tengu Spike

| Item | Status |
|---|---|
| Use | Reference only; do not continue as product architecture |
| What to reuse | Postgres schema ideas, operation names, wiki compiler behavior, MCP contract |
| What to discard | Deep Tengu planner/runner coupling and Rust-only packaging |
| Risk | Tengu architecture makes this feel like a harness feature instead of a portable plugin |

## Portable Plugin Shape

| Component | Path / artifact | Role |
|---|---|---|
| Core package | New standalone package/repo | Store, recall, ingest, promote, compile, lint, proposal engine |
| DB | Postgres + pgvector | Open Brain live memory |
| Wiki | `.agentic-memory/wiki/` or host-selected root | Karpathy LLM Wiki compiled memory |
| Raw store | `.agentic-memory/raw/` | Immutable source snapshots |
| MCP server | Standalone plugin command | Portable tool surface for Codex/Claude/Cowork |
| Tengu spike | `channel_runtime`, `planner.rs`, `run-agent` capture | Reference only; not product target |
| Codex adapter | future `.codex-plugin` or MCP config | Inject recall/wiki and propose AGENTS/skill patches |
| Claude Cowork adapter | future MCP + project files | Inject recall/wiki and propose CLAUDE/project rule patches |

## Adapter Contract

| Hook | Input | Output |
|---|---|---|
| `context_before_turn` | host, project, user message, budget | recall block + wiki page refs |
| `capture_after_turn` | user/assistant/tool summary | `memory_events` rows + optional promotion candidates |
| `ingest_source` | file/url/transcript | raw file + chunks + embeddings |
| `compile_stable_memory` | promotions | cited Markdown wiki pages |
| `propose_behavior_update` | evidence + target host | reviewable patch, never auto-applied |
| `lint_memory` | project/user scope | duplicate/stale/conflict report |

## Data Layers

| Layer | Format | Owner |
|---|---|---|
| Raw events | Postgres rows | Plugin |
| Raw files/transcripts | Files under `.tengu/agentic-memory/raw/` + Postgres metadata | Plugin |
| Fast recall | Postgres FTS + pgvector | Plugin |
| Graph/claims | Postgres tables | Consolidator |
| Compiled wiki | Markdown under `.tengu/agentic-memory/wiki/` | LLM compiler |
| Schema | `.tengu/agentic-memory/AGENTS.md` | Human-reviewed |

## Minimal Schema

| Table | Columns |
|---|---|
| `memory_events` | id, session_id, agent, role, kind, content, summary, embedding, metadata, created_at |
| `memory_sources` | id, kind, uri, title, raw_path, hash, metadata, created_at |
| `memory_chunks` | id, source_id, event_id, content, embedding, metadata |
| `memory_claims` | id, claim, confidence, source_ids, status, created_at |
| `memory_links` | from_id, to_id, relation, reason |
| `memory_promotions` | id, target_kind, target_id, status, reason, evidence |

Planned graph tables: `memory_claims`, `memory_links`.

## Tool API

| Operation | Purpose |
|---|---|
| `capture` | Store conversation/event memory |
| `recall` | Retrieve token-budgeted context |
| `ingest_source` | Add file, URL, note, or transcript to raw memory |
| `promote` | Mark stable/high-value memory for wiki compilation |
| `compile_wiki` | Update Markdown wiki from promoted items |
| `lint` | Find duplicates, contradictions, stale pages, missing citations |
| `propose_behavior` | Produce reviewable skill/config patch from evidence (planned; not in the Tengu `agentic_memory` op enum yet) |

## Tengu Spike Touchpoints

| File/area | Change |
|---|---|
| `src/adapters/plugins/agentic_memory/` | New plugin |
| `src/adapters/plugins/mod.rs` | Export plugin |
| `channel_runtime::register_core_plugins` | Register once |
| `WORKSPACE_TOOLS_ALLOWLIST` | Add `agentic_memory` |
| `src/adapters/config.rs` | `[agentic_memory]` section (not landed — the DB comes from `TENGU_MEMORY_DATABASE_URL` only) |
| `src/adapters/egress.rs` | Embedder + `compile_wiki` LLM calls use `egress::policy().llm_api_client` — Tor by default (`[egress] network = "tor"`); Postgres at `TENGU_MEMORY_DATABASE_URL` is loopback / compose-internal (`postgres-memory` on `tor-front`), never proxied |
| `run-agent` env | `TENGU_SESSION_ID` + `TENGU_AGENT_NAME` (= the `[agents.<name>]` key) stamped onto `capture` rows when the LLM omits them |
| `planner.rs` recall/write paths | Spike only |
| `compress_and_store` path | Spike only |
| External hosts | Use MCP server first; add native plugin packaging only when host needs richer lifecycle hooks |

## Config Sketch (not implemented — `TENGU_MEMORY_DATABASE_URL` is the only knob today)

```toml
[agentic_memory]
enabled = true
database_url_env = "TENGU_MEMORY_DATABASE_URL"
raw_root = ".tengu/agentic-memory/raw"
wiki_root = ".tengu/agentic-memory/wiki"
schema_file = ".tengu/agentic-memory/AGENTS.md"
behavior_updates = "proposal_only"

[agentic_memory.recall]
top_k = 8
max_tokens = 1800
hybrid = true
```

## Implementation Rule

| Rule | Why |
|---|---|
| Postgres only | Avoid SQL spread across providers |
| Raw is immutable | Wiki remains auditable |
| Wiki is LLM-owned | Avoid human/model edit conflicts |
| Query wiki first for stable knowledge | Avoid re-synthesizing raw chunks |
| Use raw/Open Brain for fresh context | Avoid stale compiled answers |
| Adapter code stays thin | Core memory behavior must be portable |
| Behavior updates are proposals | Self-improvement needs reviewable evidence |
