# SESSION_HANDOFF.md — Tengu-Cluster running state log

> Restored 2026-05-14. The file was dropped from the tree during the
> agentic-memory rework branch; `CLAUDE.md` / `AGENTS.md` still list it as
> required reading, so it is back. Keep the top section current.

---

## TL;DR — current state (2026-09-12)

Audit-and-fix pass over the uncommitted agentic-memory tree (two Workflow
runs: 6 finders + 6 skeptics, then 4 fix clusters + 1 verifier). Everything
below is **uncommitted on `main`**; every feature combo compiles, scoped
tests pass, `cargo fmt --check` is clean.

| Area | Change |
|---|---|
| Scopes | `[default_scopes]` / `[agents.*.scopes]` are now **enforced** (were parsed, never used). `Config::fold_default_scopes` at load; `resolve_tool_scopes` in `build_tool_executor` + MCP bridge via `TENGU_BRIDGE_SCOPES` (`ClaudeCodeEngine::with_scopes`); children get their own workspace in `fs_roots` (`grant_workspace_root`). `check_env_read` honours `"*"`. `sandboxes/aura/config.toml` gained `fs_roots` for `http_request`. |
| agentic_memory | `execute` gates on `env_reads` for `TENGU_MEMORY_DATABASE_URL` (scope_lint passes); wrong-dim embeddings fail-soft on event insert + hybrid recall; `capture` defaults `session_id` / `agent` from `TENGU_SESSION_ID` / `TENGU_AGENT_NAME`; `pg_trgm` dropped from `ensure_schema`; DDL runs once per process (`OnceCell`). |
| Plan hand-off | `AgentIpcInput.plan_state` (per-session, `shared_files::set_active_plan`) is the source of truth; `TENGU_PLAN.md` is a debug artifact + fallback. Fixes the cross-session race for webhooks / Telegram. |
| Planner registry | Write failure of `TENGU_PLANNER_REGISTRY.md` is fail-soft (roster kept in memory). TOOLS section lists MCP server tools again (`<server>.<tool>`, enumerated once per `RagPlanner`). |
| Config | `OrchestratorConfig.engine` defaults to `"rag"` and is validated; dead `MemoryConfig` qdrant/backend/vector_size/embedding_provider/ttl_days fields + `[rag]` removed; `AgentConfig.requires` removed; `AgentSpec` is `deny_unknown_fields` + engine validated; `skill_lifecycle.fixture_runner_agent` optional; `TENGU_CONFIG` env honoured (`--config` > `$TENGU_CONFIG` > `<TENGU_HOME>/config.toml`). |
| CLI | `tengu doctor` exits non-zero on engine build failure (Docker HEALTHCHECK). `run-agent` exports `TENGU_AGENT_NAME`. |
| Dead code | `channel_runtime::build_vector_stack`, `VectorStore::delete_older_than`, `src/adapters/rag/**`, `src/adapters/memory/vector/qdrant.rs`, `__deltest.tmp` removed. `regex-lite` dropped; `rust-version = "1.78"`. |
| Tests | `tests/run_agent_ipc.rs` (Rust) replaces `scripts/test-runner.sh`; `tests/memory_search_tool.rs` (grep-test) and `scripts/phase-0-checks.sh` deleted. CI: single `quality.yml` (fmt, check --all-features, clippy advisory, test). |
| Run docs | README rewritten (real CLI table, features, mixed engines, Postgres memory, webhooks); Makefile `up-memory`/`native-memory` replace qdrant targets; Dockerfile bakes `agents/` + `sandboxes/`, exposes 7080 (webhooks); compose/installer/cloud-init use `postgres-memory` + real GitHub URL; `docs/configuration.md` has the env-var table; `config.example.toml` matches the real schema; root HTML pages de-Qdrant'd (`rag.html` deleted). |
| `unlimited` sandbox (2026-09-13) | `sandboxes/unlimited/config.toml` — direct-agent bench on OpenRouter DeepSeek (`unlimited`=v4-flash default, `pro`=v4-pro, `r1`=r1-0528; Telegram `@pro:` routing). Verified through `tengu run-agent` on all three models — recipe + results in `sandboxes/unlimited/BENCH.md`. |
| OpenRouter body-read timeout (2026-09-16) | `OpenRouterEngine` reqwest total timeout 120s → 600s (`stream: false` → the body arrives only when generation ends; slow reasoning models like `kimi-k3` at 16k `max_tokens` hit `error decoding response body`). The body-read error now prints the full cause via anyhow `{:#}` (e.g. `…: error decoding response body: <cause>`). Test: `engine_builder::tests::body_read_failure_surfaces_source_chain`. Note: `engine.run().await` sits outside the `stream_event_timeout_secs` idle loop, so the reqwest timeout is the only limit on a non-streaming call, and cancel isn't checked while it waits. |
| Secrets | `.env.example` values blanked. **The old values are still in git history (`0266655`, `d9037e3`) — rotate Molecule, Beach, Alchemy and the Qdrant Cloud JWT, then purge history.** |

### Decisions left to the maintainer

| Item | Options |
|---|---|
| Planner agent on `claude_code` (`sandboxes/aura/config.toml` `[agents.aura]`) | Doctrine says OpenRouter for the planner; the sandbox keeps `claude_code` for the subscription. Comment now states the trade-off; value unchanged. |
| `[hub]` config + `HubConfig` | Nothing listens on it (only `tengu status` prints it). Remove the struct, or keep as a placeholder. Container ports now point at the webhook listener. |
| `allowed_users = ['848344935']` in aura sandbox | Personal Telegram id in a tracked file; `${TELEGRAM_ALLOWED_USER}` substitution is available. |
| `Adaptive_AI_Learning_Marketplace_PRD.md` at repo root | Unrelated PRD; move to `tengu/ideas/` or delete. |
| Root HTML pages still name `tengu_outputs` / `tengu_messages` collections in prose | Rewrite to `agentic_memory` or delete the pages in favour of `docs/*.html`. |
| `sandboxes/storage-test/config.toml` `model = "sonnet"` | Alias accepted by the CLI; strict form is `claude-sonnet-4-6`. |
| `build_tool_executor` is sync and drives MCP connect via `futures::executor::block_on`; the TUI calls it outside a tokio context | Latent panic if a sandbox sets `mcp_servers` and runs `tengu chat`. Make it `async` (all other callers already are). |
| `OrchestratorChatPort::run_orchestrator_turn` + `memory/injector.rs` + `MemoryProvider::prefetch` | Dead path (only `run_orchestrator_turn_with_system` is live). Delete in a follow-up. |
| Postgres connection per memory call | DDL is now once per process; connections are still per call. Pool if it shows up in latency. |

---

## Previous state (2026-05-14)

Runtime memory has been **replaced**: Qdrant-RAG → **Open Brain** (Postgres +
pgvector) + **Karpathy LLM Wiki** (compiled Markdown). New work is uncommitted
on `main`. Planner routing also moved off Qdrant — it is now file-backed
(`TENGU_PLANNER_REGISTRY.md` + `TENGU_PLAN.md`). All six implementation-doc
phases have landed, including the Phase 6 cleanup that fully removed the
legacy Qdrant `rag/` module, the `qdrant` cargo feature, and the
`tengu registry` / `tengu memory inspect` CLIs. Smaller gaps remain (below).

**Compile-verified 2026-09-12** (all feature combos, scoped tests). Postgres
smoke tests and the end-to-end turn were NOT run in that pass — see the
verification block.

---

## What landed (agentic-memory rework — uncommitted)

| Area | Change |
|---|---|
| Plugin | `src/adapters/plugins/agentic_memory/mod.rs` — Postgres store + `agentic_memory` tool (`capture`/`recall`/`ingest_source`/`promote`/`compile_wiki`/`lint`) + free fns for planner/runner recall |
| Schema | `memory_events`, `memory_sources`, `memory_chunks`, `memory_promotions` (created under an advisory lock by `ensure_schema`); `vector` extension (`pg_trgm` dropped 2026-09-12 — nothing used it); FTS + HNSW indexes |
| Planner | `orchestrator/planner.rs` — `RagPlanner` de-Qdrant'd: registry loaded from `TENGU_PLANNER_REGISTRY.md`; recall lanes (cross-session / within-session / cross-plan) read Postgres `agentic_memory` under `postgres_memory` |
| Shared files | `orchestrator/shared_files.rs` — generates `TENGU_PLANNER_REGISTRY.md`, writes/reads `TENGU_PLAN.md` (plan state → subagent prompt) |
| Runner | `main.rs` — subagent summaries captured to Postgres (`try_persist_agentic_step_summary`) on both the `compress_and_store` path and the no-call backstop; subagent prompt loads `TENGU_PLAN.md` |
| Wiring | `channel_runtime.rs` — `agentic_memory` registered in `register_core_plugins`, added to `WORKSPACE_TOOLS_ALLOWLIST` + `compute_base_tools`/`compute_bridge_tools`; `build_orchestrator` no longer gated on `qdrant` |
| Config / build | `config.rs` `valid_workspace_tools` += `agentic_memory`; `Cargo.toml` `tokio-postgres` + `postgres_memory` feature; `docker-compose.yml` `postgres-memory` profile (pgvector/pg16) |
| Docs | `docs/agentic-memory-{prd,implementation,examples}-2026-05-13.md`; architecture / comparison / context-management docs rewritten for the new model |

## What landed (this session — 2026-05-14, finish-code + Phases 4/5/6 + doc reconciliation)

| Area | Change |
|---|---|
| `.gitignore` | Ignore `TENGU_PLANNER_REGISTRY.md`, `TENGU_PLAN.md`, `/.codex/`, `**/.tengu/agentic-memory/raw/` (the compiled `wiki/` stays tracked — human-reviewable per PRD) |
| `agentic_memory/mod.rs` (chunks + recall) | `ingest_source` now embeds chunks (`memory_chunks.embedding` populated, fail-soft text-only fallback); agent-facing `recall` is now hybrid (pgvector → FTS) instead of FTS-only; tool schema gained `session_id`/`agent`/`role`/`reason`/`evidence` properties (the dispatch code already read them); new `env_embedder` / `embed_query` helpers; `insert_chunks` / `PostgresMemoryStore::recall` signatures gained params (one call site + one ignored smoke test updated) |
| `agentic_memory/mod.rs` (Phase 4 wiki compiler) | `compile_wiki` rewritten: runs an LLM over promoted memories → cited Markdown page (`[mem:<kind>/<id>]` inline + a deterministic `## Sources` footer). New module-scope helpers `compile_wiki_prompt` / `render_wiki_page_llm` / `wiki_compiler_model` / `chat_complete` (a minimal direct-OpenRouter chat call, the chat-side sibling of `Embedder`). Fail-soft: no `OPENROUTER_API_KEY` or an API error falls back to the old deterministic bullet dump. Model via `TENGU_WIKI_COMPILER_MODEL` env (default `anthropic/claude-sonnet-4-6`). |
| `metrics.rs` | New `MetricsKind::WikiCompiler` variant (+ `as_str` arm) so the wiki-compiler LLM call emits a `MetricsRecord` like every other LLM/embedding call. |
| `mcp_bridge.rs` + `main.rs` (Phase 5 MCP server) | New `tengu agentic-memory-server` subcommand — a standalone MCP stdio server exposing **only** `agentic_memory` to non-Tengu agents. Refactor: extracted `serve_mcp_stdio` (shared by `run_mcp_bridge` + the new `run_agentic_memory_mcp_server`); `handle_initialize` gained a `server_name` param (`tengu-tools` vs `tengu-agentic-memory`). CLI: new `Commands::AgenticMemoryServer` variant + early-return (stderr-only tracing, JSON-clean stdout) + feature-gated match arms, mirroring `McpBridge` / `RunAgent`. Operator doc: `docs/mcp-bridge.md`. |
| Phase 6 — full Qdrant removal (~10 files) | `webhook_builder.rs` persist repointed from `rag::RagStore` → `agentic_memory::write_step_summary_with_embedding` (the landmine: `webhooks` had an undeclared `qdrant` dep, so `--features webhooks` alone never compiled). `compress_and_store.rs` gutted to just `definition()` (Qdrant plugin/handler/`write_summary` gone). `src/adapters/rag/` orphaned — `pub mod rag` removed from `adapters/mod.rs`. `tengu registry` + `tengu memory inspect` CLIs deleted (`Commands` variants, `RegistryAction`/`MemoryAction` enums, all four command fns, dispatch arms, `try_persist_step_summary` + its call sites). `memory/vector.rs` qdrant `VectorStore` impl orphaned; `channel_runtime.rs` `build_vector_stack_async` is disk-only and `resolve_qdrant_collection` removed. `Cargo.toml`: `qdrant` feature + `qdrant-client` dep gone. `docker-compose.yml`: qdrant service/profile/volume gone. `.env.example` / `config.example.toml`: qdrant blocks replaced with Postgres-memory equivalents. |
| `CLAUDE.md` | Was stale (pre-rework: "RAG = brain", Qdrant, auto-reindex). Rewritten to match `AGENTS.md` substance + new required-reading entry for the agentic-memory docs + corrected stuck-recipe |
| `AGENTS.md` | Fixed find-replace corruption — a blanket `Claude`→`Codex` had mangled code identifiers (`engine = "Codex"`, `Codex-sonnet-4-6`, `--features Codex`). Restored to `claude_code` / `claude-sonnet-4-6` / `--features claude_code`. `CLAUDE.md` + `AGENTS.md` are now twins |
| `docs/SESSION_HANDOFF.md` | Restored (this file) |

---

## Open items

### Phase 4–6 (per `docs/agentic-memory-implementation-2026-05-13.md`)

| Item | State |
|---|---|
| Phase 4 — wiki compiler | **Landed** (2026-05-14). `compile_wiki` LLM-synthesises a cited Markdown page from promoted memories; deterministic fallback when no LLM is available. Not yet exercised end-to-end against a real LLM — see verification block. |
| Phase 5 — MCP surface | **Landed** (2026-05-14). `tengu agentic-memory-server` is a standalone MCP stdio server exposing only `agentic_memory`; non-Tengu agents (ChatGPT/Codex/Claude) wire it into their MCP client config. Not yet exercised against a real external client — see verification block. |
| Phase 6 — migration cleanup | **Landed** (2026-05-14). Full Qdrant removal: `rag/` module orphaned, `qdrant` feature + `qdrant-client` dep gone, `compress_and_store` Qdrant plugin gone, `tengu registry` / `tengu memory inspect` CLIs gone, qdrant `VectorStore` orphaned, webhook persist migrated to `agentic_memory`. **Not compiled** — see verification block. Orphaned files (`src/adapters/rag/*.rs`, `src/adapters/memory/vector/qdrant.rs`) are still physically present — `git rm` them (the sandbox couldn't unlink). |

### Smaller gaps

| Item | Note |
|---|---|
| `lint` is shallow | Returns counts only — no duplicate / contradiction / stale-page / missing-citation detection (PRD R6). |
| `propose_behavior` | In the implementation-doc Tool API table; not in the operation enum. Deliberately deferred per the PRD "Correction" (MVP = capture/recall/ingest/promote/compile/lint). |
| `[agentic_memory]` config section | Implementation doc "Config Sketch" (enabled / raw_root / wiki_root / recall knobs) is **not** implemented. Plugin uses `TENGU_MEMORY_DATABASE_URL` env + hardcoded `RAW_ROOT`/`WIKI_ROOT` consts. |
| Graph tables | `memory_claims` / `memory_links` are "planned" in the doc; `ensure_schema` does not create them. Promotion → claim/link flow not built. |
| `capture` session scoping | **Closed 2026-09-12** — `capture` falls back to `TENGU_SESSION_ID` / `TENGU_AGENT_NAME` exported by the `run-agent` child. |
| Embedding dim coupling | Schema hardcodes `vector(1536)`; embedder pinned to `DEFAULT_EMBEDDING_MODEL` (`text-embedding-3-small`). Since 2026-09-12 a non-1536 vector warns and degrades to text-only on every path (event insert, hybrid recall, chunks). |
| Chunk embedding throughput | `insert_chunks` embeds chunks sequentially (one API call each). Batching is a follow-up for large sources. |
| Postgres inspect CLI | `tengu memory inspect` was removed with the Qdrant path. No Postgres-native "did the write land?" diagnostic yet — query the DB directly or run the `postgres_*_smoke` tests. |
| Orphaned files | **Closed 2026-09-12** — `git rm`'d. |
| Schema file | `.tengu/agentic-memory/AGENTS.md` (data-layers table) is not created or read by anything yet. |

---

## Phase 6 — completed (compile-verify still required)

Full Qdrant removal landed across ~10 files (see the "what landed" table). Key
notes for whoever runs the compiler next:

- **`rag/` is cleanly isolated** — it was already fully behind
  `#[cfg(feature = "qdrant")]`, so nothing outside it referenced `RagStore`
  except `webhook_builder`, `compress_and_store`, and `main.rs`'s CLI — all
  handled. A post-removal grep for `crate::adapters::rag` / `QdrantVectorStore`
  / `qdrant_client` in compiled code is clean.
- **The landmine, fixed** — `webhook_builder::persist_webhook_output` used
  `rag::RagStore` directly while gated only on `webhooks`, so
  `cargo build --features webhooks` never actually compiled. It now writes via
  `agentic_memory` under `#[cfg(feature = "postgres_memory")]`, with a
  graceful no-op (warn) when that feature is off.
- **Vestigial `MemoryConfig` qdrant fields and the orphaned `rag/` +
  `vector/qdrant.rs` files** — both removed 2026-09-12 (compiler-verified).

---

## Verify before declaring done (run on a machine with cargo + Docker)

```sh
# 1. Build — every feature combo must compile (Phase 6 removed the `qdrant`
#    feature entirely; `webhooks` was the latent-break combo).
cargo build                                        # default (openrouter, telegram)
cargo build --features postgres_memory             # new memory path
cargo build --features webhooks,postgres_memory    # the Phase 6 landmine combo
cargo build --features claude_code,postgres_memory # mixed-engine

# 2. Unit tests (no DB needed)
cargo test --features postgres_memory agentic_memory

# 3. Postgres + pgvector smoke (DB needed)
docker compose --profile postgres-memory up -d postgres-memory
export TENGU_MEMORY_DATABASE_URL=postgres://tengu:tengu@localhost:5432/tengu_memory
cargo test --features postgres_memory postgres_capture_and_recall_smoke -- --ignored
cargo test --features postgres_memory postgres_vector_recall_smoke      -- --ignored

# 4. End-to-end — trace one turn, confirm recall block appears
#    sandbox config needs [memory] within_session_output_top_k = 3 (or similar)
RUST_LOG=tengu=info cargo run --release --features postgres_memory -- chat --sandbox aura

# 5. Wiki compiler (Phase 4) — via an agent opted into tools = ["agentic_memory"]:
#    capture + promote a memory, then run compile_wiki, then inspect the page.
#    agentic_memory(operation="capture", kind="preference", content="...")
#    agentic_memory(operation="promote", target_kind="event", target_id="<id>")
#    agentic_memory(operation="compile_wiki", title="...")
#    -> expect .tengu/agentic-memory/wiki/<slug>.md with inline [mem:...] cites
#       + a ## Sources footer. Tool result reports mode=llm (or mode=fallback
#       if OPENROUTER_API_KEY is unset — that path must still write a page).

# 6. Standalone MCP server (Phase 5) — handshake + tools/list over stdio
printf '%s\n%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
  | TENGU_MEMORY_DATABASE_URL=postgres://tengu:tengu@localhost:5432/tengu_memory \
    cargo run --features postgres_memory -- agentic-memory-server
#    -> id=1: result.serverInfo.name == "tengu-agentic-memory"
#    -> id=2: result.tools[0].name == "agentic_memory"
#    (server exits cleanly when stdin closes; build a release binary for real use)
```

Watch for: `agentic_memory: ...` warn lines (embed/connection/LLM fail-soft),
`persisted user message to agentic_memory`, `agentic_memory: backstop wrote
final_text summary to Postgres`, and one `metrics` line with
`kind=wiki_compiler` per `compile_wiki` call. After ingesting a source,
confirm `memory_chunks.embedding` is non-null for at least one row.

---

## Active gotchas

The compiled gotcha list lives in `CLAUDE.md` / `AGENTS.md` ("Key gotchas").
Rework-specific call-outs:

- **`TENGU_PLANNER_REGISTRY.md` / `TENGU_PLAN.md` are generated** — regenerated
  every planner turn / accepted plan. Now gitignored. Don't hand-edit; don't
  commit.
- **`postgres_memory` is off by default** — without it, the planner recall
  lanes compile to `String::new()` and `agentic_memory` is not registered.
  The harness still runs (file registry + in-memory history); it just has no
  durable cross-session memory.
- **`agentic_memory` module is fully feature-gated** — everything in
  `src/adapters/plugins/agentic_memory/` only compiles under `postgres_memory`.
- **`tengu agentic-memory-server` reuses the `mcp-bridge` machinery** — it is
  NOT a Claude-specific protocol; `mcp_bridge.rs` implements standard MCP
  (JSON-RPC 2.0 stdio), it was just originally built for the `claude_code`
  engine. `run_mcp_bridge` and `run_agentic_memory_mcp_server` share
  `serve_mcp_stdio`; the bridge path is byte-identical post-refactor.
- **MCP clients replace the env, not extend it** — when an external client
  spawns `tengu agentic-memory-server`, only the keys in its `env` block are
  visible. Forward `OPENROUTER_API_KEY` alongside `TENGU_MEMORY_DATABASE_URL`
  or `recall`/`ingest_source`/`compile_wiki` silently run in their degraded
  fail-soft modes. Same trap `claude_code_engine.rs` documents for the bridge.
