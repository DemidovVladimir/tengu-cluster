# Tengu-cluster — Session Handoff (2026-04-25)

> Read this first. It captures the state of the redesign as of session end:
> what's built, what's verified, what's left, and how to pick up cleanly.
>
> **Pre-flight check before any work:** `git status --short` must be clean.
> If it isn't, the previous session ended with uncommitted changes — most
> likely the **Phase 6.4 lite** patch (per-session history buffer in
> RagPlanner) and possibly a docs commit. Build + verify those first
> (see "Recent bug + partial fix" below for the regression-test prompt
> sequence) before starting new work.
>
> Companion docs:
> - `REDESIGN.md` — full design brief (the *what*).
> - `docs/architecture-v2.md` — doctrine reference (the *why*).
> - `docs/IMPLEMENTATION_PLAN.md` — phase-by-phase plan (the *order*).
> - `docs/manual-test-checklist.md` — TUI + Telegram regression checklist.

---

## TL;DR

The v2 redesign described in `REDESIGN.md` is **functionally complete and end-to-end verified**. Every doctrine principle (LLM = heart, RAG = brain, Tools/MCP = hands) is operational. The system has two working modes:

| Mode | How to enable | Status |
|---|---|---|
| `engine = "static"` (default) or no `[orchestrator]` block | Existing v1 path; default | Working |
| `engine = "rag"` | Set in `[orchestrator]` block | **Working** — verified by fetching real BTC price end-to-end via RagPlanner → SubprocessRunner → real Sonnet LLM → `http_request` → CoinGecko → `compress_and_store`. |

The static path is preserved as a safety net. Flipping `engine = "rag"` activates the new pipeline with no other config changes required (assuming Qdrant is up and `OPENROUTER_API_KEY` is set).

---

## Verified-working end-to-end behaviour

**Static mode** (no `[orchestrator]` block in sandbox config, or `engine = "static"`):
- TUI + Telegram dispatch user message directly to default agent.
- Real LLM, real tools, real conversation. Confirmed with crypto price queries returning live numbers ("$86.50 USD" for Solana, etc.).

**RAG mode** (`[orchestrator] engine = "rag"`):
- User message → `RagPlanner` queries `tengu_registry` → ranked roster of agents/skills/tools.
- Planner LLM (uses `skills/orchestrator/SKILL.md` as system prompt, no tools, no memory, no grounding nudge) emits valid plan JSON.
- `DagExecutor` dispatches each step to `SubprocessRunner`.
- `SubprocessRunner` spawns `tengu run-agent` as a child process with `TENGU_AGENT_IPC=1`.
- Child loads `agents/<name>.toml`, three-tier skill loader merges skill bodies, builds `PluginToolExecutor` over `(spec.tools ∩ ipc.tools) ∪ {compress_and_store}`.
- Multi-turn LLM mini-loop runs until model calls `compress_and_store` or hits `max_turns`.
- `compress_and_store` summary written to `tengu_outputs` (Qdrant), exits cleanly via JSON IPC.
- Verified: *"what is the current BTC price in USD?"* → routed to `researcher` (semantic match against the roster), child fetched from CoinGecko, returned **"$77,685 USD"**.

---

## Phases shipped (in chronological session order)

| Phase | Status | Summary |
|---|---|---|
| 0 — baseline + config scaffold | ✅ done | `[memory]`, `[orchestrator]`, `[rag]` config blocks; manual-test-checklist; phase-0-checks.sh; REDESIGN/IMPLEMENTATION_PLAN/architecture-v2 docs |
| 1 — RAG facade | ✅ done | `src/adapters/rag/` over existing `QdrantVectorStore`; 3 collections (`tengu_registry` / `tengu_messages` / `tengu_outputs`); `tengu registry list \| search \| reindex-tools` |
| 2 — agent specs + orchestrator skill | ✅ done | `agents/{aura,researcher,storage}.toml`; `skills/orchestrator/{SKILL.md,plan_schema.json}`; three-tier skill scanner; `tengu registry reindex-all` |
| 3 — subprocess runner + IPC | ✅ done | `runner.rs` SubprocessRunner; `compress_and_store` tool def + write helper; `tengu run-agent` subcommand (`TENGU_AGENT_IPC=1` guard); `scripts/test-runner.sh` |
| 4 — dual-mode orchestrator | ✅ done | `RagPlanner`; `build_orchestrator` branches on `engine = "static" \| "rag"` |
| 4b — SubprocessRunner cutover | ✅ done | `impl WorkerHandle for SubprocessRunner`; worker construction branches alongside planner |
| 4c — orchestrator skill as planner prompt | ✅ done | Additive `run_*_with_system` on `ChatServiceFactory` / `OrchestratorChatPort`; RagPlanner loads SKILL.md; tools/memory/grounding stripped on planner path |
| 5a — real LLM in subprocess | ✅ done | run-agent loads AgentSpec, builds engine, runs one turn (no tools); base+suffix prompt assembly |
| 5b — multi-turn tool dispatch | ✅ done | `run_single_engine_turn` exposed; `agent_config_from_spec` + `build_subprocess_tool_executor`; multi-turn loop with out-of-band `compress_and_store` |
| 5c — middle-ground protocol enforcement | ✅ done | `Failed` only when no `compress_and_store` AND no text emitted; otherwise `Ok` (with tracing warn if no-compress-and-store path) |
| 6.1 (lite) — planner tracing | ✅ done | `tracing::info!` line per plan/replan with top-10 hits + scores. Full `OrchestratorEvent::RagQueried` deferred until event bus plumbing |
| 6.5 — cross-plan recall in replan | ✅ done | `RagPlanner::replan` queries `tengu_outputs` for top-K and injects as context block |
| 6.4 (lite) — per-session history | ✅ done | In-memory ring buffer of last-N user messages on `RagPlanner`; injected as "Recent user messages" block before current turn so the planner sees prior context across multi-turn chats |

**Pre-existing v1 bugs fixed along the way:**
- TUI nested `rt.block_on` panic in memory-stats path (`src/adapters/tui/mod.rs:770`).
- `tengu chat` lacked `--sandbox` flag — added, parity with `tengu telegram`.
- `sandboxes/aura/config.toml`'s `[orchestrator]` block pointed at non-existent `"orchestrator"` agent — corrected (and removed entirely for daily-use static dispatch).
- `sandboxes/aura/config.toml` lacked `default_scopes.http_request` net allow-list, causing scope check failures on real HTTP calls.

---

## What remains — prioritised

All optional. Pick by friction in real use, not theoretical completeness.

### Polish (low risk, ~½ day each)

- **6.1 full** — Add `OrchestratorEvent::RagQueried { query, results }` variant; plumb event bus into `RagPlanner::new`; emit on every plan/replan; render in TUI as a debug panel. The lite (tracing-only) version is already shipped.
- **6.2 — content-hash dedup in indexer** — Currently `tengu registry reindex-all` clears + re-embeds every entry. Add sha256 of description in payload extras; skip re-embed when unchanged. Saves embedding API spend on every restart.
- **6.3 — filter-based TTL purge** — Default `ttl_days = 0` (never purge). When set > 0, `cleanup.rs::ttl_cleanup` currently logs a TODO; it needs filter-based delete via direct Qdrant client (the `VectorStore` trait doesn't expose it).
- **7.2 — pre-existing v1 dead-code warnings** — ~17 dead-code warnings in `orchestrator/plan.rs`, `memory/builtin.rs`, `engine_builder.rs`, `types.rs::EngineContext`, etc. Predate this redesign. Land `#[allow(dead_code)]` or remove.

### Architectural completeness (medium effort)

- **6.4 (full) — durable user-message persistence** — Lite version (in-memory ring buffer in RagPlanner) ships today; survives within one channel session. Full version writes each turn to `tengu_messages` keyed by `session_id`, survives restart, and lets the planner (and future agents) recall across sessions. Simplest path: in `RagPlanner::plan`, before the LLM call, write `user_message` via `RagStore::store_memory` with `MemoryKind::Message`; mint a `session_id` at RagPlanner construction (UUID, mirrors `SubprocessRunner`). Open question: how to share `session_id` across RagPlanner + SubprocessRunner so the conversation is unified across the entire orchestration.
- **6.6 — MCP tool indexing** — Today `placeholder_tools()` in `main.rs` injects 6 hardcoded ToolDefs into `tengu_registry`. Should enumerate real compiled-in tools (via `compute_base_tools`) plus MCP server tools (via `mcp/client.rs::tools/list`). Touches the registry CLI subcommand.
- **6.7 — C→B unknown-agent fallback** — REDESIGN §11. When all RAG hits score below 0.6, the SKILL.md tells the planner to ask user. That works; the **B** half (compose generic agent on user confirmation, run for one turn) isn't implemented. Multi-turn UX, deserves its own session.

### Cleanup (do last)

- **7.1 — delete legacy** — Drop `src/adapters/orchestrator/roster.rs`, the `engine = "static"` branch in `OrchestratorAgentPlanner` and `build_orchestrator`, the `ChatWorker` static-mode branch. Make `engine = "rag"` the only path. Requires confidence that rag mode is bulletproof for every sandbox you care about — runs the full manual checklist on each sandbox + Telegram + cancel/replan flows.

---

## File map of new modules

```
src/adapters/
├── agents/mod.rs                                  ← AgentSpec loader (Phase 2)
├── rag/
│   ├── mod.rs                                     ← RagStore facade + types (Phase 1)
│   ├── indexer.rs                                 ← startup_index_*, scan_skills (Phase 1+2)
│   ├── query.rs                                   ← search_registry, search_memory (Phase 1)
│   └── cleanup.rs                                 ← TTL purge (Phase 1, 6.3 stub)
├── runner.rs                                      ← SubprocessRunner + IPC types (Phase 3+4b)
├── orchestrator/planner.rs                        ← RagPlanner + load_orchestrator_skill_body
└── plugins/skill_lifecycle/compress_and_store.rs  ← Tool def + write_summary helper

agents/                                            ← v2 agent specs
├── aura.toml
├── researcher.toml
└── storage.toml

skills/orchestrator/                               ← Phase 2
├── SKILL.md                                       ← Used as planner system prompt (Phase 4c)
└── plan_schema.json                               ← JSON schema for planner output

scripts/
├── phase-0-checks.sh                              ← Pre-flight pre-build sanity probes
└── test-runner.sh                                 ← Phase 3 black-box IPC test
```

**Existing files materially modified:**
- `src/adapters/config.rs` — `MemoryConfig` +3 fields (`ttl_days`, `session_recent_n`, `cross_plan_top_k`); `OrchestratorConfig` +`engine`; new `RagConfig`; `Config.rag` field.
- `src/adapters/mod.rs` — `pub mod rag` (qdrant-gated); `pub mod agents`; `pub mod runner`.
- `src/adapters/orchestrator/wiring.rs` — additive `run_*_with_system` on `ChatServiceFactory` / `OrchestratorChatPort`; `ChatOrchestratorPortImpl` impl.
- `src/adapters/channel_runtime.rs` — `build_orchestrator` branches on `engine`; new `agent_config_from_spec` + `build_subprocess_tool_executor`; `RuntimeChatServiceFactory` `run_turn_with_system` override that strips tools/memory/grounding when `system_override.is_some()`.
- `src/adapters/chat_builder.rs` — added `suppress_grounding_nudge: bool` to `ChatRuntimeService` (default `false` everywhere except planner path).
- `src/adapters/engine_builder.rs` — `run_single_engine_turn` made `pub(crate)` so subprocess can reuse it.
- `src/adapters/orchestrator/planner.rs` — `RagPlanner` (new struct); `parse_verdict` made `pub(crate)`; planner tracing helper.
- `src/adapters/tui/mod.rs` — `rt.block_on(stats)` → `.await` (panic fix); `Commands::Chat` now accepts `--sandbox`.
- `src/main.rs` — `Commands::Registry` + `Commands::RunAgent` subcommands; `run_agent_subprocess` body (Phase 5b multi-turn loop with `compress_and_store` handling); `load_skill_body_three_tier`, `load_config_or_default_unconditional`.

---

## Recent bug + partial fix worth carrying forward

**Multi-turn context loss in rag mode (fixed with caveats — Phase 6.4 lite).**

In rag mode, the orchestrator drove each turn statelessly: the planner saw only the current `user_message` with no thread of prior turns. Subagents inherited the same blindness. Observed end of session 2026-04-25:

```
> what is the current BTC price in USD?
[fetches via CoinGecko, returns ~$77,682]

> Just search for "TRUMP" on those platforms for the latest price.
[planner: "what platforms?"]

> coingecko
[planner: "what do you want me to do with coingecko?"]
```

Each turn arrived at the planner orphaned. Static mode didn't have this — `ChatLoopState` accumulates history per channel — but rag mode mints a fresh planner state per turn.

**Fix landed (Phase 6.4 lite):** `RagPlanner` now keeps an in-memory `Vec<String>` ring buffer of recent user messages (capped at 2× `memory.session_recent_n`). `plan()` pushes the current message and injects the last N as a `## Recent user messages this session` block before the current turn. `replan()` reads (without double-pushing). The planner LLM can now synthesize self-contained step goals using prior context.

**Caveats — read before debugging:**
- Buffer is in-memory, lost on restart.
- One `RagPlanner` instance shared by multiple Telegram chats interleaves their histories — possible cross-talk if you ever run a multi-tenant Telegram deployment.
- Buffer holds user messages only, not assistant responses. Usually sufficient because the planner can pick the right step goal from user-side context alone, but a model might occasionally need both.

**Phase 6.4 (full) — still on the deferred list** — durable persistence to `tengu_messages` keyed by `session_id`, surviving restart and supporting cross-session recall. The lite version is intentionally a stop-gap that fixes the user-visible bug without trait-touching plumbing.

**Regression test for any future change to the planner:**

```bash
cargo run --release --features qdrant -- chat --sandbox aura
> what is the current BTC price in USD?     # turn 1 — fetches real number
> what about ETH?                           # turn 2 — must use turn 1's "price" context
> on coingecko                              # turn 3 — must use turns 1+2 to answer about ETH on coingecko
```

Turn 2 and 3 must succeed without needing repeated context. If they fall back to "what platforms?" / "what do you want?", multi-turn coherence has regressed and you are looking at a `RagPlanner` history-buffer issue (possibly: empty buffer from re-construction, off-by-one in `format_history`, or skipped injection in `plan()`).

---

## Active gotchas worth remembering

- **`workspace_tools` is a narrow allow-list of THREE values** — `shared_cache`, `persistent_store`, `skill_distill`. Anything else fails config validation. The "agent's full tool list" is `compute_base_tools(uses_tools, has_memory, workspace_tools)` which always includes http+crypto+workspace plus the three opt-ins.
- **`compress_and_store` is appended IMPLICITLY** — never list it in `agents/*.toml::tools`. The runner appends it itself for every subagent invocation.
- **Planner LLM call strips tools / memory / grounding** — `run_turn_with_system` in `channel_runtime.rs` sets `tools = []`, `tool_executor = None`, `memory_manager = None`, `suppress_grounding_nudge = true` when `system_override.is_some()`. This is intentional (REDESIGN §10) so the model has only one job: emit plan JSON.
- **`sandboxes/aura/config.toml` has NO `[orchestrator]` block in current state** — daily-use static dispatch. Re-add `[orchestrator] agent = "aura", engine = "rag"` for rag-mode testing.
- **Aura's model is `anthropic/claude-sonnet-4-6`** — was haiku-4-5; haiku struggled with strict-format compliance and tool-use confidence. Keep on sonnet for any rag-mode work.
- **Aura's identity instructions were broadened** — added "for general user questions outside the DeSci pipeline... use the http_request tool freely." Without this, aura's heavy-DeSci identity skewed it toward "punt with recommendations" instead of using HTTP.

---

## How to verify state in a fresh session

```bash
cd ~/development/tengu-cluster
git log --oneline -20    # last ~20 commits should show phase-0..phase-6.5

# Pre-flight
docker ps | grep qdrant  # ensure Qdrant is up on :6334 (or 6333)
echo $OPENROUTER_API_KEY # set
bash scripts/phase-0-checks.sh

# Build
cargo build --release --features qdrant

# Static-mode smoke (no orchestrator block in sandbox)
cargo run --release --features qdrant -- chat --sandbox aura
> what is the current BTC price in USD?
# Expected: real number from CoinGecko

# RAG-mode smoke (re-add [orchestrator] block in sandboxes/aura/config.toml first)
cargo run --release --features qdrant -- chat --sandbox aura
> what is the current BTC price in USD?
# Expected logs:
#   "orchestrator engine = rag (Phase 4 ...)"
#   "orchestrator worker = SubprocessRunner (Phase 4b ...)"
# Expected events:
#   orch: plan created (1 step)
#   orch: ▶ s1 [researcher]
#   orch: ✓ s1
# Expected output: real BTC price in USD.

# Multi-turn regression test (Phase 6.4 lite — must pass without
# regression after any planner change):
#   > what is the current BTC price in USD?
#   > what about ETH?
#   > on coingecko
# Turns 2 and 3 MUST succeed without asking for clarification.
# If the planner replies "what platforms?" or "what do you want?",
# the per-session history buffer in RagPlanner has regressed.

# Optional debug
RUST_LOG=tengu=info cargo run --release --features qdrant -- chat --sandbox aura
# RagPlanner emits one info-level line per plan call summarising RAG hits.
```

---

## How to start a productive next session

Open a new Claude session and paste this prompt:

> "I'm continuing the tengu-cluster v2 redesign. Read `docs/SESSION_HANDOFF.md` first for the full state. The redesign is functionally complete; what remains is optional polish (see the 'What remains' section). Today I want to work on **<pick one>**:
> - 6.4 user-message persistence to tengu_messages
> - 6.6 real MCP tool indexing in tengu_registry (replacing placeholder_tools)
> - 7.1 delete legacy roster.rs/wiring.rs static path
> - 7.2 sweep pre-existing dead-code warnings
> - 6.7 C→B unknown-agent fallback (B half)
>
> Confirm understanding then propose the smallest commit-sized change."

That gives the new session the full context without re-litigating any closed decisions.

---

*This handoff was written 2026-04-25 at the close of a marathon session that landed the v2 redesign end-to-end. All decisions in `REDESIGN.md §20` were honoured. The doctrine holds: behaviour is changeable by editing `.toml` and `SKILL.md` — no PR required.*
