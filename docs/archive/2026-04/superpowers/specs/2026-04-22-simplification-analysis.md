# Simplification Analysis — Dependency Graph, Duplication, and Refactor Plan

> **Archived (2026-09-18)** — historical; current behaviour: see `README.md` / `docs/architecture-2026-04-27.md`.

**Date:** 2026-04-22
**Scope:** `src/` at `main` (commit `6864a48`)
**Deliverable mode:** Analysis + ranked refactor plan. **No code changes in this pass.**

## 0. Method

Two-phase audit, per `docs/superpowers/specs/` brainstorming flow:

1. **Mechanical pass** — `cargo tree`, `rg`-based symbol sweeps, file-size breakdown, module/subsystem map. Produces the graph and the low-effort dead-code candidates.
2. **Judgment pass** — five parallel `Explore` agents, one per subsystem, scoped to explicit file lists and explicit rules (skills-are-capabilities, flat structure, plugin-based tools, no-placeholder-code). Each produced a structured findings list with file:line cites.

All agent findings that recommend deletion or behavioral changes were spot-verified before inclusion here. Claims that failed verification are listed in §8 to avoid re-raising them.

---

## 1. Executive summary

- The codebase is **27,068 LOC of Rust across 92 files**, dominated by six files that hold 45% of total LOC: `eval_builder.rs` (2619), `telegram_builder.rs` (2015), `skill_builder.rs` (1304), `config.rs` (1222), `channel_runtime.rs` (1105), `engine_builder.rs` (932).
- Architecture matches the stated rules: flat `src/adapters/` + `src/main.rs`, plugin-based tools under `plugins/*/`, harness/plugin split for skills and memory is intentional.
- **No major structural duplication** was found — the hexagonal/layered scaffolding the repo deliberately avoids is absent.
- **Significant tactical duplication** exists: ~300–500 LOC of copy-pasted helpers (truncation, char-boundary scans, JSON arg extraction, plugin schema declarations).
- Two empty placeholder files live in `orchestrator/`. One blanket `#![allow(dead_code)]` gate sits at the top of `skill_lifecycle/mod.rs` masking whatever changes later.
- Three files (`persistent_store.rs`, `skill_builder.rs`, `eval_builder.rs`) are large enough to warrant a structural pass — but that work is mid-sized and should not be bundled with the cleanup PR.

---

## 2. Dependency graph (subsystem-level)

```
                                     ┌──────────────┐
                                     │   main.rs    │ (entry, CLI)
                                     └──────┬───────┘
                                            │
                        ┌───────────────────┼────────────────┐
                        ▼                   ▼                ▼
                  ┌──────────┐      ┌────────────┐     ┌──────────┐
                  │   tui    │      │  telegram  │     │   eval   │
                  │  (801)   │      │  (2015)    │     │  (2619)  │
                  └─────┬────┘      └─────┬──────┘     └─────┬────┘
                        └──────────┬──────┴─────────────┬────┘
                                   ▼                    ▼
                     ┌─────────────────────────────────────┐
                     │     channel_runtime (1105)          │◄── shared build_tool_executor
                     │     chat_builder   (473)            │    for every channel
                     │     flow_builder   (270)            │
                     └──────┬───────────────┬──────────────┘
                            │               │
                ┌───────────▼──┐     ┌──────▼──────────┐
                │  orchestr.   │     │  engine_builder │ (932)
                │  (10 files)  │     │  claude_code_   │ (657, feat-gated)
                │  planner,    │     │   engine        │
                │  executor,   │     └──────┬──────────┘
                │  replan, …   │            │
                └───────┬──────┘            │
                        │                   │
                        ▼                   ▼
                  ┌──────────────────────────────┐
                  │      tool_plugin (319)       │  ← Tool trait + ToolRegistry
                  │      ports (421)             │  ← ToolScope + traits
                  │      types (349), config(1222)│
                  └───────────┬──────────────────┘
                              │
           ┌──────────────────┼────────────────────────┐
           ▼                  ▼                        ▼
    ┌────────────┐     ┌────────────┐          ┌────────────┐
    │  plugins/  │     │  memory/   │          │  skill_    │
    │  workspace │     │  (harness) │          │  lifecycle │
    │  http      │     │            │          │  (harness) │
    │  crypto    │     └──────┬─────┘          └──────┬─────┘
    │  cache     │            │                       │
    │  mcp       │            ▼                       ▼
    │  memory ◄──┼──────┐  vector/                 plugins/
    │  skill     │      │  (disk|qdrant|embedder)  skill_lifecycle/
    │  skill_lc ◄┼──────┘                           (distill tool)
    └────────────┘
```

Key correlations:

- **One-way dependency from channels → orchestrator → engine → plugins.** No reverse edges.
- **Harness/plugin split for memory and skill_lifecycle is clean**: harness owns mutation state, plugins own LLM-callable surface. Both sides reach into `tool_plugin::Tool` via the plugin registry.
- **`channel_runtime.rs` is the hub**: all three channels (`tui`, `telegram`, `eval`) wire through it to build the `ToolExecutor`. The 7 duplicated `build_*_tool_executor` blocks all live in this file and differ only in channel bookkeeping.
- **MCP is genuinely two-sided**: `mcp_bridge.rs` (outbound, exposes Tengu tools to external Claude Code) vs. `plugins/mcp/` (inbound, proxies external MCP tools into Tengu). JSON-RPC types live on both sides; the two copies differ intentionally (`id: Option<Value>` vs `id: u64`) but the repetition is still a maintenance surface.

**External deps (`cargo tree --depth 1`): 38 direct crates, all currently used.** No obvious bloat. `cursive`, `pdf-extract`, `calamine`, `zip`, `alloy`, `teloxide`, `rusqlite` each serve a documented purpose.

---

## 3. Duplication findings (ranked by confidence)

### 3.1 Verified high-confidence duplications

| # | Pattern | Locations | LOC saved | Risk |
|---|---------|-----------|-----------|------|
| D1 | Char-boundary-safe string truncation | `engine_builder.rs:900` `channel_runtime.rs:599,693` `tool_builder.rs:157` `mcp_bridge.rs:307` `eval_builder.rs:1021` | ~40 | L |
| D2 | Plugin `execute()` JSON arg extraction — `get(k).and_then(as_str).ok_or_else(...)` pattern | 8+ sites across `plugins/workspace/*`, `plugins/http/request.rs`, `plugins/crypto/*`, `plugins/cache/shared_cache.rs` | ~80 | M |
| D3 | Plugin `tools/memory` JSON schema duplicated between `plugins/memory/mod.rs:40–150` and each tool's `.new()` (ingest.rs, search.rs, persistent_store.rs) | `plugins/memory/mod.rs:32–150` vs `ingest.rs:44–80`, `search.rs:37–72`, `persistent_store.rs:211–244` | ~120 | M |
| D4 | 7 identical `futures::executor::block_on(registry.register_plugin(...))` wrappers with identical "make async once chain is fully async" TODOs | `channel_runtime.rs:127–232` | ~60 | M |
| D5 | `SanitizedToolExecutor<'a>` (borrowed) + `OwnedSanitizedToolExecutor` (owned) — same redaction logic, two lifetime variants | `engine_builder.rs:538–592` | ~25 | M |

**D1 is the cheapest win.** Single helper (`fn truncate_text(s: &str, max: usize, suffix: &str) -> String`) in `adapters/token.rs` (already exists, 11 LOC) swaps in at 6 call sites.

### 3.2 Agent-reported, not yet verified

- **`skill_builder.rs:428–489` manually parses YAML** instead of delegating to `serde_yaml` already in `Cargo.toml`. Claimed ~150 LOC reducible. Requires verification that the manual path isn't handling a serde_yaml gap.
- **`persistent_store.rs:430–441` reimplements vector search** instead of delegating to `MemorySearchTool`. Needs verification that `persistent_store` isn't filtering in a way `search.rs` can't.
- **`skill_lifecycle/distill.rs:233–265 validate_metrics_structural()`** duplicates `skill_lifecycle/metrics.rs:122 validate_metrics()` modulo file-existence check. Likely extractable helper, ~20 LOC.
- **`eval_builder.rs:1366–1389` (direct dispatch) vs `1703–1726` (orchestrator dispatch)** duplicate executor + observer setup. Claim: ~60 LOC reclaimable.

---

## 4. Verified dead code / orphaned declarations

| # | Finding | Location | Action | Risk |
|---|---------|----------|--------|------|
| Z1 | `orchestrator/config.rs` is a 1-line file: `//! Placeholder — implemented in Task 4.N.` | `src/adapters/orchestrator/config.rs:1` | Delete file, remove `pub mod config;` from `orchestrator/mod.rs:13` | L |
| Z2 | `orchestrator/telemetry.rs` is a 1-line file: `//! Placeholder — implemented in Task 4.N.` | `src/adapters/orchestrator/telemetry.rs:1` | Delete file, remove `pub mod telemetry;` from `orchestrator/mod.rs:21` | L |
| Z3 | `MemoryProvider::on_pre_compress()` defined in `memory/provider.rs:46` has zero call sites in `src/`; no impls override it (default stub returns empty string) | `memory/provider.rs:46` | Remove the trait method | L |
| Z4 | Stale `sessions_spawn` / `sessions_fan_out` fixtures and test stubs — `plugins/subagents/` was removed but fixtures reference the old tool names | `eval_builder.rs:2080–2110`, `2413–2420`, `2431–2432` | Rewrite fixtures to reference a currently-registered tool, or delete the tests | L |
| Z5 | Blanket `#![allow(dead_code)]` at top of `skill_lifecycle/mod.rs` hides any future dead code in the whole subsystem | `skill_lifecycle/mod.rs:7` | Remove the allow; let compiler flag real issues, suppress narrowly | L |

**Note on Z4:** The fixtures are only used in unit tests (no live eval run references them), but they create misleading documentation artifacts. Rewriting is preferred over deletion so the test coverage survives.

---

## 5. Simplification candidates (structural)

Ranked by `impact × confidence / blast_radius`. Each item is a *follow-up*, not part of the cleanup PR.

### S1 — `persistent_store.rs` (810 LOC) split by operation
Five distinct responsibilities bundled into one file: text extraction, chunking, manifest I/O, four operation handlers (store/search/list/delete), tests. Extract operation handlers to sibling files (`plugins/memory/persistent_store/{store,search,list,delete}.rs`) — flat structure is preserved (same dir), `Tool` interface unchanged. **Est. delta: neutral LOC, but each operation becomes readable in isolation.** Risk: **M** — call-site-free, but touches the single largest plugin file.

### S2 — `eval_builder.rs` (2619 LOC) split by responsibility
Three cohesive chunks: (a) core `run / run_skill / run_row` — ~1400 LOC; (b) eval config loading — ~50 LOC → `eval_config.rs`; (c) test/fixture stubs (NoopActivity, NoopRuntimeToolExecutor, EvalJudgeClient adapter) — ~150 LOC → `eval_fixture.rs`. Plus: reclaim ~60 LOC by extracting shared executor+observer setup between direct and orchestrator dispatch paths (D2 in §3.1). **Est. delta: −~200 LOC net, improved readability.** Risk: **L–M** — no API change, no behavior change.

### S3 — `skill_builder.rs` (1304 LOC) YAML parsing collapse
Replace `try_parse_frontmatter()` (408–512) manual YAML parsing with `serde_yaml` delegate. Needs a verification pass first to confirm the manual parser isn't handling an edge case serde_yaml misses. **Est. delta: −~150 LOC if the manual path is truly unnecessary.** Risk: **M** — YAML round-trip semantics must survive.

### S4 — `skill_lifecycle/evolve.rs` (620 LOC) frontmatter-mutation passes
Current flow parses, mutates metrics, serializes, then re-parses. Consolidate into one parse-mutate-serialize helper. **Est. delta: −~30 LOC, 2 parse cycles saved per evolve session.** Risk: **M** — core evolve loop.

### S5 — `flow_builder.rs:136–152` three separate scope-default match functions
Collapse `default_compaction_threshold_ratio_for_scope`, `default_compaction_keep_turns_for_scope`, `default_compaction_summary_max_tokens` into one struct-returning function. **Est. delta: −~20 LOC.** Risk: **L**.

### S6 — `MemoryConfig` (config.rs:544–575) legacy vector DB fields gated by `backend`
The four legacy vector DB-specific fields (`qdrant_url`, `qdrant_api_key`, `qdrant_collection`, `vector_size`) are live but only read when `backend = "qdrant"`. Consider making them an `Option<QdrantConfig>` sub-struct so they're not implied as always-applicable. **Est. delta: −~10 LOC, config surface clearer.** Risk: **M** — config schema change, user configs would need migration.

Items *NOT* in this list (intentionally):

- Single-impl traits (`WorkerHandle`, `Planner`, `OrchestratorChatPort`, `ToolActivityPort`, `SkillSourcePort`, `ShellExecutionPort`) — kept for testability / extensibility. Collapsing them gains nothing and breaks test injection.
- Vector backend split (`disk.rs` vs `qdrant.rs`) — correct abstraction, no shared logic to extract.
- Harness/plugin split for memory + skill_lifecycle — intentional, per `mod.rs` comments.

---

## 6. Recommended first PR — "Cleanup + tactical dedup" (APPROVED 2026-04-22)

A single bundled PR. No new abstractions beyond a shared truncation helper + the unified redaction wrapper. No public API changes. All items are low-risk and mutually independent.

**Contents (D1, D2, D3, D4, D5, Z1, Z2, Z3, Z4, Z5):**

1. Add `fn truncate_text(s: &str, max: usize, suffix: &str) -> String` to `adapters/token.rs`. Swap 6 call sites. (**D1**, ~−35 LOC)
2. Add `tool_utils::{require_str, require_i64, require_bool}` helpers. Swap ~15 plugin call sites. (**D2**, ~−60 LOC)
3. Unify `plugins/memory/` tool schema so `mod.rs` is the single source; each tool's `.new()` references it. (**D3**, ~−100 LOC)
4. Extract `register_plugin_safe(...)` helper in `channel_runtime.rs`; collapse 7 block_on blocks. (**D4**, ~−40 LOC)
5. Collapse `SanitizedToolExecutor<'a>` + `OwnedSanitizedToolExecutor` into one redaction wrapper covering both borrowed and owned registry cases. (**D5**, ~−25 LOC; engine-boundary touch — exercise TUI + eval + telegram paths once before merging)
6. Delete `orchestrator/config.rs`, `orchestrator/telemetry.rs`; remove imports. (**Z1, Z2**)
7. Remove unused `MemoryProvider::on_pre_compress()`. (**Z3**)
8. Rewrite stale `sessions_spawn` / `sessions_fan_out` eval fixtures to use a currently-registered tool. (**Z4**)
9. Remove `#![allow(dead_code)]` from `skill_lifecycle/mod.rs`; replace with narrower per-item `#[allow(dead_code)]` if anything actually flags. (**Z5**)

**Est. delta:** −230 to −325 LOC (item estimates: D1 ~35, D2 ~60, D3 ~100, D4 ~40, D5 ~25, Z-items ~10). No behavioral change. No public API change. Tests must compile and pass without modification.

**Deliberately excluded from this PR:**
- S1–S6 structural refactors — each deserves its own PR. S2 is the approved next one (see §7).
- §3.2 unverified agent findings — need spot-verification before inclusion in a second dedup PR.

---

## 7. Proposed sequence after cleanup PR (APPROVED 2026-04-22)

1. **Cleanup PR (this spec → writing-plans).** §6 contents, single branch.
2. **S2 — `eval_builder.rs` split.** Next PR after cleanup merges. Separate branch, own writing-plans pass. Promoted from step 3 per user decision.
3. Verify each §3.2 agent finding; fold surviving ones into a "secondary dedup" PR.
4. S1 (persistent_store split) — standalone PR.
5. S3 (skill_builder YAML) — only after verifying manual parser can be safely dropped.
6. S4, S5 — bundled or standalone.
7. S6 — only if there's a user-facing config migration window.

Each step fits the "small changes, verify" memory rule: compile, pass `tests/scope_lint.rs` + `tests/memory_search_tool.rs`, hand-exercise the relevant channel (`tengu chat`, `tengu eval`, `tengu telegram`) before moving on. "Don't salami-slice" rule: no parallel branches; each PR merges before the next plan kicks off.

---

## 8. Agent claims that did *not* survive verification

Recorded here so they don't resurface:

- **"`memory.backend` field is always `disk`, remove it"** — false. `channel_runtime.rs:405` matches on `backend.as_str()` and branches to `QdrantVectorStore` when `"qdrant"` is set and the feature is enabled. Field is live.
- **"40+ default-valued fields should consolidate to a builder"** — low value, high churn. Config fields use `#[serde(default = "...")]` per-field which is idiomatic. Not recommended.
- **"Port traits with single impls should collapse"** — intentionally kept for test-injection of `NoopActivity`, fake shell executors, etc. Collapsing breaks tests.

---

## 9. Decisions (2026-04-22)

- **Q1** — §6 cleanup-PR contents approved as-is.
- **Q2** — S2 (eval_builder split) promoted to "next PR after cleanup" — separate branch, separate plan.
- **Q3** — D5 folded into cleanup PR (same branch). §6 updated accordingly.

Writing-plans now proceeds for §6 as scoped above.
