# Tengu-cluster — Implementation Plan

> Companion to `REDESIGN.md`. Where REDESIGN says *what the target looks like*,
> this plan says *what order to land it in so nothing breaks between commits*.
> Every phase ends with a binary you can launch manually and exercise via TUI and
> Telegram.

---

## Deferred decisions

- **MemPalace integration (2026-04-24):** Considered adopting
  `/Users/vladimirdemidov/development/mempalace` as the memory/RAG layer.
  Deferred — Qdrant stays for now. The door is left open: MemPalace exposes a
  29-tool stdio-MCP server, so if it is adopted later it plugs in as an
  additional MCP server in `config.toml`, no rewrite of the Qdrant code
  required. Revisit after Phase 5.

## Guiding rules (apply to every phase)

1. **No flag day.** New subsystems land next to the old ones and are gated by a
   config flag until proven. Flipping the flag is how you switch; reverting is
   one line.
2. **Every phase is shippable.** `cargo build`, `cargo test`, `cargo clippy
   --all-targets -- -D warnings`, and `tests/scope_lint.rs` all pass at the end.
3. **Every phase ends with a manual smoke test on BOTH channels.** If either
   TUI or Telegram fails, the phase is not done.
4. **Rollback is always trivial until Phase 5.** Until then, the old roster
   still exists. If the new path misbehaves, set
   `[orchestrator] engine = "static"` and the old path runs.
5. **Bootstrap assets are committed files, not Vladimir's homework.** The
   minimum-viable `skills/orchestrator/SKILL.md`, `plan_schema.json`, and an
   example `agents/*.toml` all ship with the PR that enables them.
6. **Commit frequently.** Treat each phase as at least one commit per PR so
   work is never stranded in an uncommitted working tree. (Phase 0 was lost
   once to `git clean`; do not repeat that.)

---

## Feature flags introduced across the plan

Extend the EXISTING `[memory]` block (see `src/adapters/config.rs::MemoryConfig`
— already has enabled, embedding_model, backend, qdrant_url, qdrant_collection,
vector_size, persistent_store_chunk_*, etc.). Phase 0 adds three fields. Do
not create a parallel block.

```toml
[memory]
# ttl_days: 0 = never purge (default, MemPalace-style permanent memory).
# Set to a positive integer to enable startup sweep of older entries.
ttl_days            = 0
session_recent_n    = 10
cross_plan_top_k    = 5
# Existing default qdrant_url is http://localhost:6334 (NOT 6333).
# The storage-test sandbox already runs Qdrant on 6334; keep that.

# Extend EXISTING [orchestrator] block (OrchestratorConfig — already has agent,
# max_attempts_per_step, max_replans, route_explicit_agents):
[orchestrator]
# "static" uses legacy roster.rs/wiring.rs (default until Phase 5)
# "rag"    uses the new RAG planner + subprocess runner
engine              = "static"

# NEW top-level block:
[rag]
# Enables the startup indexer regardless of engine choice.
# Safe to turn on early — indexer is read-from-disk / write-to-Qdrant only.
enabled             = false
```

Flipping `orchestrator.engine` is the single cutover switch.

---

## Phase 0 — Baseline & safety net  (½ day)

### Goal
Establish a reproducible manual smoke test for both channels so every subsequent phase has a pass/fail reference. No behaviour changes.

### Tasks
1. Audit `cargo build`, `cargo test`, `cargo clippy` on main. Fix any existing warnings that would noise up later diffs.
2. Create `docs/manual-test-checklist.md` — a copy-pasteable script for the TUI and Telegram smoke tests.
3. Extend `[memory]` and `[orchestrator]` structs in `src/adapters/config.rs` with the new fields (inert). Add new `RagConfig` struct + `Config.rag` field. Mirror in `config.example.toml` as commented examples.
4. Run the manual smoke test on `main` and record the expected output so later phases can diff against it.
5. Confirm Qdrant is reachable (the codebase default is port **6334**, not 6333):
   ```bash
   curl http://localhost:6334/collections
   ```
   If not: `docker run -d -p 6333:6333 -p 6334:6334 qdrant/qdrant:latest`.
6. Remove the empty `config.toml/` directory at the repo root — an artifact from a failed `tengu init` that would conflict with a future `config.toml` file.

### Files touched
- `config.example.toml`
- `src/adapters/config.rs`
- `docs/manual-test-checklist.md` (new)
- `scripts/phase-0-checks.sh` (new — non-interactive pre-flight probes)

### Manual test (the baseline — run once, save the output)
```
# TUI
cargo run -- chat
  > hello
  > what agents do you have?
  > /quit

# Telegram
cargo run -- telegram
  (from your phone): /start
  (from your phone): hello
  (from your phone): what agents do you have?
```
Both should behave exactly as today. Paste each session into the "Regression baselines" section of `docs/manual-test-checklist.md`.

### Acceptance
- Both channels produce the recorded baseline output.
- Build clean, tests green.
- `bash scripts/phase-0-checks.sh` exits 0.

### Rollback
None needed — no behaviour change.

---

## Phase 1 — RAG facade + Qdrant collections (read-only)  (1–1½ days)

### Goal
`tengu_registry`, `tengu_messages`, and `tengu_outputs` exist in Qdrant, the indexer populates `tengu_registry` with compiled-in tools and MCP tools on startup, but NOTHING in the orchestrator uses it yet. Zero behaviour change.

### Tasks
1. Create `src/adapters/rag/` module with:
   - `mod.rs` — `RagStore` facade wrapping existing `memory/vector/qdrant.rs`.
   - `cleanup.rs` — TTL purge using `memory.ttl_days` (no-op when 0).
   - `indexer.rs` — content-hash dedup; skips re-embedding when hash matches.
   - `query.rs` — thin `search_registry` / `search_memory` wrappers.
2. Ensure collection creation is idempotent (create-if-missing). Handle the messages/outputs split described in REDESIGN §17.
3. Wire the startup indexer into `main.rs` for the `chat` and `telegram` subcommands — gated by `rag.enabled = true`.
4. Embed compiled-in tool descriptions and MCP tool descriptions (after `mcp/client.rs` calls `tools/list`).
5. Add `tengu registry list` CLI subcommand: prints `type | name | score=N/A`.
6. Add `tengu registry search <query>` CLI: prints top-10 with scores.
7. Unit tests:
   - `rag::indexer`: same content hash → no embed call.
   - `rag::cleanup`: entries older than ttl are deleted, newer kept; when `ttl_days=0`, noop.
   - `rag::query`: round-trip upsert + search returns the upserted item.

### Files touched
- NEW: `src/adapters/rag/{mod,indexer,query,cleanup}.rs`
- `src/adapters/mod.rs` (register module)
- `src/main.rs` (startup hook + two CLI subcommands)
- `src/adapters/memory/` (expose what the facade needs — no behaviour change)

### Manual test
```
# Flip the flag on
sed -i '' 's/enabled\s*=\s*false/enabled = true/' config.toml

cargo run -- chat
# (exit with /quit — we're just booting to trigger indexing)

cargo run -- registry list
# Expect: list of compiled-in tool descriptions + MCP tools

cargo run -- registry search "run a command"
# Expect: run_command / shell-ish tools score highest

# Repeat the Phase 0 smoke test on both channels.
cargo run -- chat
cargo run -- telegram
```
The baseline behaviour MUST match Phase 0 exactly. Only difference the user should notice: startup takes slightly longer (indexing).

### Acceptance
- Collections exist in Qdrant (visible in the Qdrant dashboard).
- `registry list` returns compiled-in + MCP tool entries.
- Phase 0 smoke tests still pass byte-for-byte on both channels.

### Rollback
`rag.enabled = false`. Indexer is skipped, nothing reads from Qdrant.

### Risks
- Embedding API cost on first boot: ~50 embeddings, cents. Log total count.
- Qdrant down → startup panics. Wrap indexer in `anyhow::Result` and log a warning rather than crashing; the old path doesn't need Qdrant.

---

## Phase 2 — Agent specs + skills on disk (indexed, unused)  (1 day)

### Goal
`agents/*.toml` and `skills/` directories exist, the indexer picks them up, and the RAG can answer "find me an agent for X." Planner still uses the static roster.

### Tasks
1. Create `src/adapters/agents/mod.rs` — loader + validator for `AgentSpec` (fields per REDESIGN §6).
2. Migrate the two existing `sandboxes/*/config.toml` files into `agents/*.toml` files that mirror their roles. Write the `description` field specifically for semantic search (what this agent is good at + what it is NOT good at).
3. Author a minimal `skills/orchestrator/SKILL.md` using the template in REDESIGN §10. Author `skills/orchestrator/plan_schema.json` from §9.
4. Extend `rag/indexer.rs` to scan:
   - `agents/*.toml` → `description` field → upsert into `tengu_registry`.
   - `skills/**/SKILL.md` → frontmatter `description` → upsert.
   - Three-tier skill search (managed / dotdir / root) — lowest to highest precedence; duplicate names shadow.
5. `tengu registry list --type agent` / `--type skill` filters added.
6. Integration test: `cargo test --test indexer` loads fixtures under `tests/fixtures/workspace/` and verifies the agents + N skills round-trip through `search_registry`.

### Files touched
- NEW: `src/adapters/agents/mod.rs`
- NEW: `agents/aura-orchestrator.toml`, `agents/storage.toml` (or similar, migrated from sandboxes)
- NEW: `skills/orchestrator/SKILL.md`
- NEW: `skills/orchestrator/plan_schema.json`
- `src/adapters/rag/indexer.rs` (new scanners)
- `tests/fixtures/workspace/` (new)

### Manual test
```
cargo run -- registry list --type agent
# Expect: migrated agents

cargo run -- registry list --type skill
# Expect: at least "orchestrator" + any existing skills

cargo run -- registry search "research the web"
# Expect: a research-capable agent is in top 3

# Phase 0 smoke test on both channels — still must match baseline.
cargo run -- chat
cargo run -- telegram
```

### Acceptance
- `registry list` shows agents and N skills.
- Semantic queries return sensible rankings.
- TUI + Telegram baseline smoke tests pass.
- Integration test passes.

### Rollback
Delete the `agents/` files and `skills/orchestrator/`. The indexer handles missing dirs gracefully. `rag.enabled = false` silences it entirely.

### Risks
- Three-tier shadowing bugs: write an explicit test with a skill present in tiers 1 + 3 and assert tier-3 wins.
- Embedding cost on larger `skills/` trees — cap at N = 500 for now, log if exceeded.

---

## Phase 3 — Runner subprocess + `compress_and_store` (standalone, unused)  (2 days)

### Goal
A runner subprocess exists, has a JSON IPC contract, and can be driven manually via a harness test. The orchestrator still does not use it.

### Tasks
1. Add `run-agent` subcommand to `main.rs`:
   - Refuses to run unless `TENGU_AGENT_IPC=1` is set (re-entry guard).
   - Reads stdin JSON, writes stdout JSON per REDESIGN §7.
   - `stderr` forwarded via a framed `StepProgress`-style protocol line prefix (e.g. `@@progress: ...`). The parent runner parses these lines; anything unprefixed is logged verbatim.
2. Implement `compress_and_store` compiled-in tool (`src/adapters/plugins/skill_lifecycle/compress_and_store.rs`):
   - Writes `{summary, session_id, step_id, created_at}` to `tengu_outputs`.
   - Sets an internal flag in the runner context.
3. Implement three-tier skill loader in a shared helper so the `run-agent` mode uses it. Hard-fail on missing skill.
4. Implement `src/adapters/runner.rs`:
   - `SubprocessRunner` impl of `WorkerHandle`.
   - Loads agent spec from `agents/<name>.toml`.
   - Computes `effective_tools = spec.tools ∩ ipc.tools ∪ {compress_and_store}`.
   - `Command::new(current_exe())` with the IPC env var, pipes stdin/stdout/stderr.
   - Times out via `spec.timeout_secs`.
5. Integration test:
   - Spawn `SubprocessRunner::run_step` with a goal like "say hello and store a summary".
   - Verify it exits with `status: "ok"`, a summary is in `tengu_outputs`, and an `OrchestratorEvent::StepProgress` was emitted.
6. Manual black-box test script: a `scripts/test-runner.sh` that pipes hand-written JSON into `TENGU_AGENT_IPC=1 cargo run -- run-agent` and prints the response. Commit this script.

### Files touched
- `src/main.rs` (new subcommand + env guard)
- NEW: `src/adapters/runner.rs`
- NEW: `src/adapters/plugins/skill_lifecycle/compress_and_store.rs`
- NEW: `scripts/test-runner.sh`
- `src/adapters/rag/` (write path for outputs)

### Manual test
```
# Black-box runner test — no orchestrator involved yet
./scripts/test-runner.sh
# Expect: stdout JSON with status: "ok", summary field populated.

cargo run -- registry search "hello world"
# Expect: the summary from the previous run is retrievable.

# Phase 0 smoke test — baseline behaviour still holds.
cargo run -- chat
cargo run -- telegram
```

### Acceptance
- `scripts/test-runner.sh` returns a clean `ok` response.
- Summary lands in `tengu_outputs`.
- TUI + Telegram baseline still matches.

### Rollback
The runner is dead code unless the orchestrator calls it. Nothing to revert.

### Risks
- Subprocess zombies if parent crashes mid-step. Use `tokio::process::Command` with `kill_on_drop(true)`.
- stderr interleaving garbling the TUI — covered by the `@@progress:` prefix.
- Env leakage (API keys to child): pass only an allow-list of env vars.

---

## Phase 4 — Dual-mode orchestrator (flag-gated cutover)  (2 days)

### Goal
Flip `[orchestrator] engine = "rag"` and the full flow works end-to-end via the new path. Keep `"static"` as the default so main stays safe. This is the phase where Vladimir does real manual testing.

### Tasks
1. Change `OrchestratorAgentPlanner` to branch on `config.orchestrator.engine`:
   - `static` → existing behaviour (roster.rs + wiring.rs).
   - `rag` → new behaviour:
     - `rag.search_registry(user_message, top_k=20)`.
     - Format into ranked roster markdown (exact format in REDESIGN §10).
     - Load `skills/orchestrator/SKILL.md` as system prompt.
     - LLM call → parse against `plan_schema.json`.
     - Retry on parse failure with previous-output + validator-error appended (max 3 retries, log each attempt).
2. In `rag` mode, the `DagExecutor` is constructed with `SubprocessRunner` as the `WorkerHandle`. In `static` mode it still uses the existing worker.
3. Emit `OrchestratorEvent::RagQueried` before every planner call.
4. Wire `tengu_messages` writes on user message receipt (both TUI and Telegram paths — one shared handler).
5. On replan (existing `replan.rs`), query `tengu_outputs` for top-K (from `cross_plan_top_k`) and inject as a context block. Query `tengu_messages` for last-N (from `session_recent_n`) of the same `session_id` by `created_at DESC` (NOT vector search) and prepend as a "recent dialogue" block.
6. Add `--engine rag` CLI override on `tengu chat` / `tengu telegram` so you can test without editing config.toml.
7. Add an E2E smoke test: `tests/e2e_rag_flow.rs` (use `cargo test --ignored`) that boots the harness with a stub embedding model + stub Qdrant (or a Qdrant test container), sends a message, and asserts the final response arrives via the event bus.

### Files touched
- `src/adapters/orchestrator/planner.rs`
- `src/adapters/orchestrator/replan.rs`
- `src/adapters/orchestrator/events.rs` (add `RagQueried`)
- `src/adapters/orchestrator/wiring.rs` — keep, now only used in static mode
- `src/adapters/tui/` and `src/adapters/telegram_builder.rs` — user-message write-through to `tengu_messages`
- `src/main.rs` (the `--engine` flag)
- NEW: `tests/e2e_rag_flow.rs`

### Manual test (this is the big one)

Run **each** of the sections below twice: once with `engine = "static"` (must match Phase 0 baseline), once with `engine = "rag"` (the new path).

#### TUI
```
[orchestrator] engine = "rag"
cargo run -- chat

> hello
# Expected: Direct response, fast, no plan.

> research what LLMs were released in April 2026 and summarise
# Expected: RagQueried event visible in debug log, plan with 1-2 steps,
# StepStarted/StepProgress/StepSucceeded events, final response.

> show me your plan for "write a bash script to list files"
# Expected: plan referencing the coder agent (or whichever is most relevant).
```

#### Telegram
Same three messages, from your phone. Verify messages arrive, status updates stream, final response is delivered.

#### Sanity probes
```
cargo run -- registry list --type message --session <id>

# Flip back
sed -i '' 's/engine = "rag"/engine = "static"/' config.toml
cargo run -- chat
# Expected: exact Phase 0 baseline behaviour returns.
```

### Acceptance
- Both `engine = "static"` and `engine = "rag"` run end-to-end cleanly on both channels.
- Switching the flag requires no restart-and-migrate incantation — just change config and run.
- At least one multi-step plan completes successfully under `engine = "rag"` on each channel.
- `RagQueried` events show sensible top-K entries.
- E2E test passes.

### Rollback
Set `engine = "static"`. Old path is unchanged. If a bug is found, fix it under the flag without touching the static path.

### Risks
- **Biggest risk**: planner produces invalid JSON → 3 retries → user sees an error. Mitigation: the retry-with-validator-error loop; if that isn't enough, fall back to `{"kind":"direct","response":"..."}` with a user-visible apology.
- Subprocess spawn + Qdrant write latency → visible lag. Measure; if >2s per step, investigate embedding caching.
- Replan loops (failed step → replan → failed step). Keep `max_replans` at 3.

---

## Phase 5 — Flip default + delete legacy  (½ day)

### Goal
`engine = "rag"` becomes the default; legacy roster path is deleted.

### Tasks
1. Change `config.toml` default to `engine = "rag"`.
2. Run the full manual checklist on both channels one more time.
3. Delete the legacy files confirmed present by reconnaissance (REDESIGN §12 after corrections):
   - `src/adapters/orchestrator/roster.rs`
   - `src/adapters/orchestrator/wiring.rs`
   - `src/adapters/eval_builder.rs` (move body to `tengu/ideas/eval/`)
   - `src/adapters/skill_lifecycle/evolve.rs` (move body to `tengu/ideas/auto-skill-research/`)
   - `sandboxes/aura/`, `sandboxes/storage-test/` (after Phase 2 migration completes)
4. Remove the `engine` flag — RAG is the only path. Remove the `static` branch from `planner.rs` and `main.rs`.
5. Update `README.md` with the new architecture summary + link to REDESIGN.md.
6. Compile, test, clippy.

### Files touched
- Delete the files/dirs above.
- `src/adapters/orchestrator/planner.rs` (drop static branch)
- `src/main.rs` (drop `--engine` flag)
- `config.toml`
- `README.md`

### Manual test
The full manual checklist — TUI + Telegram — must all pass with no flag manipulation possible.

### Acceptance
- No references to `roster.rs` / `wiring.rs` / `static` engine remain.
- TUI + Telegram full flows pass clean.
- `cargo build` / `cargo test` / `cargo clippy` clean.

### Rollback
`git revert` this PR. Because Phase 4 shipped the dual-mode path, reverting just restores the `static` engine option — nothing is structurally different.

---

## Phase 6 — Polish & deferred wins  (1–2 days, optional)

### Goal
The remaining items from REDESIGN.md that aren't required for a working v2.

### Tasks
1. Unknown agent fallback (REDESIGN §11 — the "C → B" flow). Write a skill scenario that always triggers low-score recall (describe a task no agent handles) and observe the orchestrator ask for clarification.
2. Surface `RagQueried` in the TUI (a collapsed panel showing top-K per turn). Helps debugging.
3. `tengu registry search <query>` — already in Phase 1; polish the output format.
4. Optional: background re-index on SIGHUP (not a file watcher — user-triggered re-index without restart).
5. Optional: hybrid BM25 + vector search for the registry. Qdrant supports it; may improve precision on short-description agents.
6. Optional: revisit MemPalace integration (see Deferred decisions above).

### Acceptance
Features work as described; no regressions against Phase 5 checklist.

---

## Estimated total effort

| Phase | Effort |
|-------|--------|
| 0 — baseline | ½ day |
| 1 — RAG facade | 1½ days |
| 2 — agents + skills on disk | 1 day |
| 3 — runner subprocess | 2 days |
| 4 — dual-mode cutover | 2 days |
| 5 — delete legacy | ½ day |
| 6 — polish | 1–2 days |
| **Total** | **8–9 days** of focused work |

The cutover risk lives entirely in Phase 4. Phases 0–3 are additive and safe. Phase 5 is just cleanup. Phase 6 is optional.

---

## What to do before starting Phase 0

1. Confirm Qdrant is running locally or that the team has a remote Qdrant reachable from your dev machine.
2. Confirm embedding API keys (OpenAI / Voyage / Ollama) are configured.
3. Decide who reviews each phase's PR — the plan's value comes from each increment being reviewed before the next starts.
4. Decide which two sandboxes (`aura`, `storage-test`) to migrate first as example `agents/*.toml` in Phase 2.

If any of those are unknown, Phase 0 is where to answer them.
