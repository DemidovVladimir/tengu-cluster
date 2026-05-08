# Tengu-cluster — Session Handoff (2026-04-27, end of day)

> Read this first. It captures the state at end of session 2026-04-27:
> what's built, what's verified, what's left, and how to pick up cleanly.
>
> **Pre-flight check before any work:** `git status --short` must be clean.
> If it isn't, the previous session left uncommitted changes — see
> `docs/comparison-2026-04-26.md` and the per-Phase entries below for what
> the diff should be.
>
> Companion docs:
> - `CLAUDE.md` (repo root) — required-reading + doctrine + gotchas. Read first.
> - `docs/architecture-2026-04-27.{md,svg,html}` — line-by-line walkthrough,
>   the picture, and the interactive file-map explorer.
> - `docs/context-management-2026-04-27.{md,svg,html}` — **canonical reference
>   for everything that shapes what an LLM sees** across 7 layers and
>   ~25 mechanisms. Read this BEFORE touching `prompt_budget.rs`,
>   `flow_builder.rs`, `engine_builder.rs::collect_engine_response`,
>   `chat_builder.rs::process_user_text`, or any of the `LimitsConfig` /
>   `MemoryConfig` defaults. The .html is interactive (search mechanisms,
>   walk a turn, symptom → cause lookup, sortable config table).
> - `docs/context-cutting-flow-2026-04-27.{svg,html}` — focused slice:
>   Layer 3 (the inner tool loop). Subset of the canonical doc; useful
>   when debugging something inside `collect_engine_response`.
> - `docs/compression-flow-2026-04-27.{md,svg}` — focused slice:
>   Layer 5 (`compress_and_store` step protocol). Useful when touching
>   subagent IPC, `tengu_outputs`, or replan-recall.
> - `docs/comparison-2026-04-26.{md,svg}` — Tengu vs Hermes Agent vs PI/Cowork.
> - `REDESIGN.md` — full design brief (the *what*).
> - `docs/IMPLEMENTATION_PLAN.md` — phase-by-phase plan (the *order*).
> - `docs/manual-test-checklist.md` — TUI + Telegram regression checklist.

## 2026-04-29 — Skill subsystem REDESIGN (Streams X/Y/Z)

**Plan source**: `docs/skill-redesign-2026-04-29.md`. Replaces the multi-agent / multi-tool patchwork with a Hermes-style unified design: ONE agent (`learning-agent`) + TWO unified tools (`view_skill` + `manage_skill`).

**Why**: `tengu skill evolve` works end-to-end (Apr 28), but in-chat lifecycle (`adjust yourself`, `evaluate`) was broken in 4 distinct places that couldn't all be patched cleanly. Each fix surfaced the next bug. Root cause: too many specialised agents (skill-author / skill-evaluator / skill-improver-inline / resource-finder) + too many bespoke tools (`skill_distill` / `apply_improver_proposal` / `skill_resource`) coordinating via JSON proposals shuttled between subprocesses. Hermes-agent solved the same problem with one tool + action-dispatch and one agent.

### Stream X — `view_skill` plugin (read-side)

`src/adapters/plugins/view_skill/mod.rs` (NEW, ~720 lines, 12 unit tests). Three actions:
- `list` — enumerate every skill (3 tiers, first-match wins) with `{name, description, tier, fixtures, resources_count, metrics_count, editable_by_learner}`.
- `read` — return one skill's frontmatter + body + resource list + metrics summary.
- `read_resource` — read a specific file under `skills/<name>/resources/`. Symlink-aware extract (canonicalise + prefix check). 1 MB cap.

Always-on (registered unconditionally; no `workspace_tools` opt-in needed). Coexists with the legacy `skill_resource` tool which stays for back-compat.

### Stream Y — `manage_skill` plugin (write-side)

`src/adapters/plugins/manage_skill/mod.rs` (NEW, ~750 lines, 17 unit tests). Six actions:
- `create` — atomic write of new SKILL.md + `resources/README.md` + `evals/prompts.yaml` stub. Defaults to `learner_facing: true`, `editable_by_learner: true`.
- `edit_body` — full body rewrite, frontmatter preserved.
- `patch` — fuzzy find-and-replace (v1: exact + whitespace-normalised; 6 hermes strategies marked TODO). 0-match → helpful error with file preview; >1 match without `replace_all` → ambiguous-match refusal. Post-patch frontmatter validation refuses breaking changes to SKILL.md.
- `add_resource` — atomic write under `resources/<path>`. Path validated. `overwrite: false` default.
- `remove_resource` — atomic delete.
- `delete` — refuses with active scratch worktree (`.tengu/worktrees/evolve-<name>-*`).

All writes: validate skill name, refuse on `editable_by_learner: false`, atomic temp+rename, append `skills/.audit.jsonl`, return `{action, skill, ..., loaded_in_current_conversation: false}`. Opt-in via `WORKSPACE_TOOLS_ALLOWLIST` + `tools = ["manage_skill"]` on the agent's TOML (NOT in `workspace_tools` field — that field is silently ignored on `agents/*.toml` per CLAUDE.md gotcha).

### Stream Z — collapse 4 agents → 1, rewrite lifecycle plans

- **NEW**: `agents/learning-agent.toml`. `tools = ["view_skill", "manage_skill", "http_request", "read_file", "list_directory"]`. `skills = ["skill-creator"]`. Engine: openrouter / opus-4-7 (better instruction-following for the multi-tool dance). Description hard-rules the agent into calling tools, not emitting JSON proposals as text.
- **DELETED**: `agents/skill-author.toml`, `agents/skill-evaluator.toml`, `agents/skill-improver-inline.toml`, `agents/resource-finder.toml`.
- **UPDATED**: `skills/orchestrator/SKILL.md` "Lifecycle verbs" table — every verb (`create skill`, `evaluate`, `adjust yourself`, `fix it`, `improve`) collapses to a 1-step plan dispatching to `learning-agent`. The agent decides at runtime which `view_skill` reads, which `http_request` fetches, and which `manage_skill` writes to fire.

### Integration glue

- `src/adapters/plugins/mod.rs` — registered `view_skill` and `manage_skill` modules.
- `src/adapters/channel_runtime.rs`:
  - `WORKSPACE_TOOLS_ALLOWLIST` extended to FIVE values: `shared_cache`, `persistent_store`, `skill_distill`, `apply_improver_proposal`, `manage_skill`.
  - `compute_base_tools` + `compute_bridge_tools` — `view_skill` advertised always; `manage_skill` advertised when in workspace_tools.
  - `register_core_plugins` — `ViewSkillPlugin` registered always; `ManageSkillPlugin` registered when in `allowed_names`.
- `CLAUDE.md` — gotcha #1 updated to FIVE values.

### Verification on host

```
cd ~/development/tengu-cluster
cargo build --release --features "qdrant claude_code"
cargo install --path . --features "qdrant claude_code" --locked
cargo test --features qdrant -- view_skill
cargo test --features qdrant -- manage_skill

# Force reindex so registry picks up the new agent + tools
TENGU_REGISTRY_FORCE_REINDEX=1 tengu registry reindex-all

# Smoke
tengu chat --sandbox aura
> adjust yourself for the spanish-teacher skill
```

### Expected smoke behaviour (the previous design failed at every stage)

| Step | What happens |
|---|---|
| Plan | 1-step: `[{agent: "learning-agent", goal: "Adjust skill spanish-teacher based on this dialog. Use view_skill to inspect, http_request to fetch, manage_skill to apply atomically."}]` |
| Agent enters | `view_skill(action="read", skill="spanish-teacher")` → sees frontmatter + body + resource list. |
| Agent gap-finds | If learner shows weakness in topic X, `http_request` fetches relevant pages. |
| Agent applies | One or more `manage_skill(action="add_resource", ...)` calls + optionally `manage_skill(action="patch", ...)` for body refinements. Each write is atomic + audit-logged. |
| Agent reports | Plain prose: "Updated spanish-teacher with N new resources at <paths>. Restart the chat to use the updated skill." |

### Open follow-ups (deferred)

- **`skill_distill` and `apply_improver_proposal` plugins still exist for back-compat.** Mark deprecated in a future cleanup; eventually delete and route their callers through `manage_skill`.
- **`skill_resource` plugin still exists** alongside `view_skill`. Same back-compat.
- **`tengu skill evolve` CLI is unchanged** — uses its own `skill-improver` agent + `ImproverProposal` JSON path, independent of the in-chat lifecycle.
- **Fuzzy patch v1 has only 2 strategies** (exact, whitespace-normalised). The other 6 from hermes (escape-norm, indent-flex, unicode-norm, block-anchor, context-aware, trimmed-boundary) are TODO comments in `manage_skill/mod.rs`.

---

## 2026-04-28 — Skill subsystem fourth wave (Streams M/N)

Plan source: `docs/skill-research-2026-04-28.md` Batch 9 closure + Stream B follow-up. Two parallel streams.

**Stream M — `ToolCtx::agent_config` threading**
- `src/adapters/tool_plugin.rs` — added `agent_config: Option<&'a AgentConfig>` to `ToolCtx`. `PluginToolExecutor` owns a `Option<AgentConfig>` clone and borrows it into every `execute()` ctx.
- `src/adapters/channel_runtime.rs` + `src/adapters/mcp_bridge.rs` — both pass their resolved `AgentConfig` clone into `PluginToolExecutor` at construction.
- `src/adapters/plugins/skill_lifecycle/distill.rs` — auto-seeded `evals/config.toml` now reads `engine` + `model` from `ctx.agent_config` when present; sonnet+openrouter fallback retained. New test `seeds_eval_config_engine_and_model_from_calling_agent`.
- 7 `ToolCtx { … }` literal sites updated (all visible to grep). Tests that don't exercise distill use `agent_config: None`.

**Stream N — `ImproverProposal::resource_additions`** (closes Batch 9 loop)
- `src/adapters/skill_lifecycle/evolve.rs` — `ProposalBody.resource_additions: Option<Vec<ResourceFile>>` (`#[serde(default)]`, additive). New `ResourceFile { path, content, overwrite }` struct. `validate_resource_path` (rejects `..`, absolute, root, prefix, non-utf8). `apply_proposal_resources` (atomic per-file write via `.resource.tmp-<nanos>` + rename, parent-dir creation, refuses overwrite without flag). `CycleOutcome.new_resource_additions` carries through cycle scoring. `Decision::Apply` writes resources after copying SKILL.md; partial failure → `tracing::warn!` and continue (matches the spec — user's `git diff` reveals what landed).
- `src/adapters/skill_lifecycle/approval_gate.rs` — `GateView.resource_additions: &'a [(String, usize)]`; render block lists `+ resources/<path>  (<bytes> bytes)` between body diff and rationale. New test `render_includes_resource_additions_when_present`.
- `agents/skill-improver-inline.toml` — `description` extended with the JSON schema example, the path-rules paragraph, and the "don't fabricate resources outside the resource-finder output" grounding rule.
- 5 new unit tests in `evolve.rs::tests` covering path validation, atomic write, overwrite refusal, and round-trip parsing of proposals with the new field.

**Verification on host**:
```
cargo build --release --features "qdrant claude_code"
cargo install --path . --features "qdrant claude_code" --locked
cargo test --features qdrant -- skill_lifecycle    # +6 new tests
cargo test --features qdrant -- distill            # +1 new test (config-threading)
```

**Open follow-ups (carried forward, mostly closed now)**:
- "adjust yourself" three-step plan still not smoke-tested e2e against real chat session.
- AbComparison metric kind (Batch 6 #19) deferred until a real evolve-cycle tie surfaces.
- `MetricKind::HumanReview` (Batch 5) deferred until a real skill needs it.
- `SkillReadinessStatus` (Batch 7) deferred — no in-tree skill declares prereqs today.

---

## 2026-04-28 — Skill subsystem third wave (Streams I/J/K)

Plan source: `docs/skill-research-2026-04-28.md` Batches 6 + 9. Three parallel streams.

**Stream I — `tengu skill seed`** (Batch 9 #31)
- `src/main.rs` — added `SkillAction::Seed` variant + dispatch arm + `skill_seed(...)` handler. Generates SKILL.md from template (with `editable_by_learner: true` and `learner_facing: true` defaults), recursively copies `resources_dir/` → `skills/<name>/resources/` (skipping dotfiles), emits stub `evals/prompts.yaml` (schema_version 1, single placeholder fixture). Atomic via `<tier>/.<name>.tmp-<nanos>/` + RAII `TmpDirGuard`. Audit-logged with `op: "seed"`.
- `docs/manual-test-checklist.md` — added SKL-15 / SKL-16 smoke entries.

**Stream J — three-tier registry reindex** (closes the managed-tier-invisible gap from previous handoff)
- `src/adapters/rag/indexer.rs` — `scan_skills` refactored to walk all three tiers (managed → workspace → project) with first-wins shadowing (managed beats workspace beats project). New `SkillScan { entries, managed_count, workspace_count, project_count }` struct + `scan_skills_with_counts(workspace, managed_root: Option<&Path>)` for testability. Original `scan_skills(workspace)` signature unchanged. Per-tier counts logged in `reindex_all_workspace`. 5 unit tests using TempDir + injected `managed_root`.
- `src/adapters/skill_builder.rs` — `FileSystemSkillSource::skill_directories` extended to prepend `~/.tengu/skills` (legacy Hermes-style path also picks up managed-tier skills now).

**Stream K — variance + parent tracking** (Batch 6 actions #17 and #18)
- `src/adapters/skill_lifecycle/storage.rs` — `MetricRollup` extended with `stddev / min / max: Option<f32>` (all `serde(default, skip_serializing_if)` for back-compat). `compute_rollups` populates them: sample stddev (n-1) when window ≥ 2, else `None`; min/max via `partial_cmp` over the windowed series. 4 new tests including explicit back-compat round-trip from old JSON shape.
- `src/adapters/skill_lifecycle/evolve.rs` — `CycleOutcome` extended with `parent_cycle_n: Option<u32>`. v1 driver always sets `None` (linear-from-baseline); reserved for a future branch-from-cycle flow. 1 test.
- `src/main.rs` — `SkillAction::Metrics` handler appends a per-metric summary line under the raw JSON dump: `m1: 0.75 ± 0.08 [0.60–0.90]  n=5` when variance is available, otherwise just `m1: 0.75  n=5`.

**Verification on host**:
```
cargo build --release --features "qdrant claude_code"
cargo install --path . --features "qdrant claude_code" --locked
cargo test --features qdrant -- skill_lifecycle
cargo test --features qdrant -- indexer

# New verbs
tengu skill seed german-teacher ~/Desktop/german-materials/    # Batch 9
tengu skill list                                               # should pick up managed-tier installs now
tengu skill metrics skill-creator --last 5                     # variance band visible after >=2 runs
```

**Open follow-ups** (carried forward):
- `tar` / `flate2` are not in `Cargo.toml`; install/export/seed shell out to system `tar`. macOS/Linux ship one. Windows would need a pivot.
- Auto-seeded `evals/config.toml` still hardcodes sonnet (`ToolCtx` doesn't carry agent spec yet).
- "adjust yourself" three-step plan documented in `skills/orchestrator/SKILL.md` but not yet smoke-tested e2e.
- AbComparison metric kind (Batch 6 action #19) deferred until a real evolve-cycle tie surfaces in production.

---

## 2026-04-28 — Skill subsystem second wave (Batches 2, 4, 7-Streams E/F/G/H)

Plan source: `docs/skill-research-2026-04-28.md`. After the first wave (Batches 1+3+slices of 8+9) shipped and `tengu skill evolve` was verified end-to-end, four more parallel streams landed.

**Batch 2 (CLI completeness, Stream E + F)**
- `src/main.rs` — refactored flat `Commands::SkillEvolve / SkillMetrics / SkillAcceptProposal` into nested `Commands::Skill { #[command(subcommand)] action: SkillAction }` with 8 variants. Five new verbs: `Remove`, `List`, `Doctor`, `Export`, `Install`. Dispatcher delegates to per-verb handlers (`skill_remove`, `skill_list`, `skill_doctor`, `skill_export`, `skill_install`).
- `src/adapters/skill_lifecycle/scanner.rs` (NEW) — tengu-scoped threat scanner. ~12 patterns across 5 categories (`destructive`, `exfiltration`, `injection`, `credentials`, `shell_metric`). Verdict mapping: `critical` → `Dangerous`, `high` → `Caution`, only `medium`/`low` → `Safe`. Shell-metric advisories never escalate alone.
- `src/adapters/skill_lifecycle/audit.rs` (NEW) — append-only `skills/.audit.jsonl` with `{ts, op, name, verdict, source, sha256}`. Helpers: `append`, `read_recent(limit)`, `now_ts()`.
- `src/adapters/skill_lifecycle/mod.rs` — registered `scanner`, `audit`.
- Install pipeline: quarantine → symlink-aware extract (`Path::canonicalize` + prefix-check) → frontmatter validate → scan (always print findings) → optional `--strict` gate → atomic move → audit. Default trusts any URL (per §0 decision 2).

**Batch 4 (description-trigger metric, Stream G)**
- `src/adapters/skill_lifecycle/metric_kinds/description_trigger.rs` (NEW) — Cowork `run_loop.py` pattern as a metric kind. 60/40 stratified train/test split (in-process LCG, no `rand` dep), 3× per query for stability with strict-majority voting, JudgeClient-driven. Scores against the test split only.
- `src/adapters/skill_lifecycle/metrics.rs` — added `DescriptionTrigger` variant + `name()`/`min_pass_rate()` arms + `validate_metrics` arm (checks `queries_file` exists, `runs_per_query >= 1`, `holdout ∈ [0,1]`).
- `src/adapters/skill_lifecycle/metric_kinds/mod.rs` — registered + re-exported `DescriptionTriggerKind`.
- `src/adapters/eval_builder.rs` — added dispatch arm + import.
- `skills/skill-creator/metrics/trigger_queries.yaml` (NEW) — 10 should-trigger + 10 near-miss should-not-trigger queries.
- `skills/skill-creator/SKILL.md` — added `triggers_correctly` metric to frontmatter (`min_pass_rate: 0.85`).

**editable_by_learner enforcement (Stream H)**
- `src/adapters/skill_lifecycle/evolve.rs` — new `is_editable_by_learner(skill_md_path) -> Result<bool>` helper. Default-allow when flag absent (back-compat for existing skills). `Decision::Apply` branch in `run_evolve` now checks the flag on the REAL workspace SKILL.md and refuses with `verdict = "refused-by-flag"` in `evolve_log.md` when explicitly false. 3 unit tests.
- `skills/skill-creator/SKILL.md` — flag set to `true` (stays editable).
- `skills/orchestrator/SKILL.md` — flag set to `false` (harness-critical, locked).

**CLI rename sweep (10 docs)**
- All references to `tengu skill-evolve` / `tengu skill-metrics` / `tengu skill-accept-proposal` rewritten to `tengu skill evolve` / `metrics` / `accept-proposal` across: `src/adapters/skill_lifecycle/evolve.rs`, `docs/skill-research-2026-04-28.md`, `docs/SESSION_HANDOFF.md`, `docs/skill-lifecycle.html`, `docs/skill-lifecycle-validation.md`, `docs/skill-lifecycle-pipeline-diagrams.md`, `docs/superpowers/specs/2026-04-20-skill-metrics-evolution-design.md`, `docs/skills.md`, `docs/configuration.md`, `docs/architecture.md`. The flat verb names no longer exist on the CLI.
- `skills/skill-eval/SKILL.md` — table of CLI commands updated by Stream E.

**Verification on host**:
```
cargo build --release --features "qdrant claude_code"
cargo install --path . --features "qdrant claude_code" --locked
cargo test --features qdrant -- skill_lifecycle    # all metric_kinds + scanner + audit + learner_state + evolve helpers
tengu skill evolve skill-creator --max-cycles 1 --sandbox aura     # repeat the previously-verified e2e
tengu skill list                                                   # NEW
tengu skill doctor                                                 # NEW (informational scan over installed skills)
```

**Open follow-ups (not landed, deferred)**:
- `tar` / `flate2` are not in `Cargo.toml`; install/export shell out to system `tar`. macOS/Linux ship one. Windows would need a pivot.
- `audit::append` is called from skill-lifecycle handlers but never from the existing `tengu skill evolve` path — acceptable (evolve has its own `evolve_log.md`); revisit if a unified audit is wanted.
- Auto-seeded `evals/config.toml` still hardcodes sonnet (`ToolCtx` doesn't carry agent spec yet).
- "adjust yourself" three-step plan documented in `skills/orchestrator/SKILL.md` but not yet smoke-tested e2e.

---

## 2026-04-28 — Skill subsystem multi-batch landing

Plan source: `docs/skill-research-2026-04-28.md`. Batches 1, 3 (G3), and large
slices of 8 + 9 landed via 4 parallel subagents + integration glue.

**Batch 1 (config polish, D1)**
- `sandboxes/aura/config.toml` — activated `[skill_lifecycle]` + defined `[agents.skill-improver]` and `[agents.fixture-runner]` from the `config.example.toml:184-213` template.
- `agents/researcher.toml` — dropped phantom `web-research` / `summarizer` from `skills` (now `[]`); silences reindex warns.
- `CLAUDE.md` — added `docs/skill-research-2026-04-28.md` row to the "REQUIRED updates" table; added new top-level "Output style" section for terse-by-default output.

**Batch 3 / G3 (auto-seed evals/config.toml)**
- `src/adapters/plugins/skill_lifecycle/distill.rs` — `SkillDistillTool::execute` now writes a fifth file `evals/config.toml` inside the same atomic-rename window. Generated config: `runtime_profile = "cloud"`, hardcoded sonnet model (TODO: thread calling agent's spec through `ToolCtx`), `workspace_tools` = BTreeSet-deduped union of tools observed in extracted fixtures, `[agents.<name>-agent.identity]` named `"<TitleCase> Eval Agent"`. Two new tests assert presence + tool propagation.

**Batch 8 (in-chat lifecycle verbs, partial)**
- `src/adapters/skill_lifecycle/metric_kinds/dialog_replay.rs` — new `MetricKind::DialogReplay { name, from_message_index, delegate_metric, expected_outcome?, min_pass_rate? }`. Synthesises a `FixtureContext` from the conversation slice + delegates scoring to a sibling `llm_judge` or `tool_assertion` metric on the same skill. Fail-soft on missing slice / unknown delegate / out-of-range index.
- `src/adapters/skill_lifecycle/metrics.rs` — added `DialogReplay` variant + `name()` / `min_pass_rate()` arms + `validate_metrics` arm. Extended `MetricRunCtx` with two new fields: `conversation: Option<&'a [Message]>` and `sibling_metrics: Option<&'a [MetricSpec]>`. All five existing `MetricRunCtx { … }` literal sites updated to set them to `None` (or `Some(ctx.skill_metrics)` in the eval_builder runner).
- `src/adapters/eval_builder.rs` — added `DialogReplay` arm to the dispatch match.
- `src/adapters/skill_lifecycle/metric_kinds/mod.rs` — registered `dialog_replay` mod + re-exported `DialogReplayKind`.
- `agents/skill-author.toml`, `agents/skill-evaluator.toml`, `agents/skill-improver-inline.toml` — new agent specs implementing the in-chat lifecycle verbs ("create skill", "evaluate", "fix it").
- `skills/orchestrator/SKILL.md` — added "Lifecycle verbs (in-chat skill management)" section mapping user phrases ("create skill from our dialog", "evaluate", "fix it", "adjust yourself", "rollback") to plan shapes.

**Batch 9 (learning-platform pieces, partial)**
- `src/adapters/skill_lifecycle/learner_state.rs` — new module owning `skills/<name>/state/<learner_id>.json`. `LearnerState { learner_id, topics_covered, topics_weak, mastery_scores: BTreeMap<String, f32>, last_session_ts }`. Public API: `state_path / load / save / list_learners`. Validates `learner_id` against `^[a-z0-9][a-z0-9_-]{0,63}$` (path-traversal guard). Atomic save via temp + rename. Cold-start load returns Default. 7 unit tests including concurrent atomic save.
- `src/adapters/skill_lifecycle/mod.rs` — registered `learner_state` mod.
- `agents/resource-finder.toml` — new agent spec using `http_request` to fetch 2–5 high-quality web resources for a learner gap. Returns JSON for downstream curation.
- `skills/skill-creator/SKILL.md` — documented two new optional frontmatter fields: `editable_by_learner: bool` and `learner_facing: bool`.

**Verification on host**:
```
cargo build --features qdrant
cargo test --features qdrant -- skill_lifecycle      # exercises learner_state + dialog_replay + storage
cargo test --features qdrant -- distill              # verifies G3 auto-seed
tengu skill evolve skill-creator --max-cycles 1 --sandbox aura
```

**Open follow-ups (NOT landed, deferred to Batch 9 follow-up)**:
- `editable_by_learner` flag is documented but NOT enforced by `evolve.rs` yet. The improver-inline agent will accept proposals regardless until the gate is wired.
- Calling agent's spec is not threaded through `ToolCtx`, so the `evals/config.toml` model is hardcoded to sonnet. Real fix: extend `ToolCtx` with a borrowed `&AgentConfig`.
- "adjust yourself" three-step plan (skill-evaluator → resource-finder → skill-improver-inline) is documented in `orchestrator/SKILL.md` but not yet smoke-tested end-to-end.
- No CLI yet for `tengu skill remove / list / install / export / doctor` (Batch 2 not started).

## 2026-04-28 — context/token metrics (landed)

Adds an observability layer for **context consumption per LLM and embedding
call** so the user can measure and optimize what gets sent to models.

**What landed:**

| Surface | What |
|---|---|
| New module `src/adapters/metrics.rs` | `MetricsRecord`, `MetricsKind { Planner, Subagent, Embedding }`, `MetricsLayer { name, chars, bytes }`, process-global `OnceLock<broadcast::Sender>`, `record()` helper that always logs + broadcasts, `AggregatorState` rollup (overall + by-agent + by-kind) used by the TUI. |
| New event variant | `OrchestratorEvent::MetricsRecorded { record }`. Emitted onto the orchestrator bus by a bridge task spawned in `build_orchestrator` that subscribes to the global metrics sink and re-broadcasts. |
| Trait extension | `ChatServiceFactory::run_turn_with_system_metered` and `OrchestratorChatPort::run_orchestrator_turn_with_system_metered` (both with default impls returning zeroed telemetry — back-compat preserved). `RuntimeChatServiceFactory::run_turn_inner` now returns `(String, TurnTelemetry)`; legacy methods unwrap and discard the telemetry. |
| Planner instrumentation | `RagPlanner::plan/replan` capture per-context-layer breakdown (`system`, `roster`, `cross_session`, `history`, `recall`, `failure`, `user_message`) BEFORE the LLM call, time the call, and emit one `MetricsRecord` with `kind=Planner` after parsing. Helper: `emit_planner_metrics`. |
| Subagent instrumentation | `run_agent_subprocess` records one `MetricsRecord` per `run_single_engine_turn` (deltas come from `StreamEvent::Usage`, already exposed by both engines). Records are packed into `AgentIpcOutput::{Ok,Failed}.metrics` (new field, `default + skip_serializing_if = Vec::is_empty` so the IPC payload stays byte-compatible). `SubprocessRunner::run_step` re-emits each record via `metrics::record(...)` on the parent. |
| Embedder instrumentation | `embed_batch_openrouter` reads `usage.total_tokens` from the OpenRouter response (was discarded), times the request, and emits one `MetricsRecord` with `kind=Embedding`. Falls back to a `chars/4` heuristic when the field is missing. Covers both reindex bulk calls and per-turn search query embeddings — every call site benefits. |
| TUI surface | New env var `TENGU_TUI_METRICS=1` (paralleling `TENGU_TUI_RAG_DEBUG`). When on, every `OrchestratorEvent::MetricsRecorded` is absorbed into a per-spawn `AggregatorState` and a compact one-liner is pushed as a System bubble: `metrics: tok in/out 4.5k/812 · last subagent (2.1k) · session 12.3k · researcher 8.7k`. Aggregator absorbs records even when the panel is OFF so flipping it on mid-session shows real numbers. |
| Tracing baseline (always on) | Every `metrics::record()` call emits a structured `tracing::info!` line with `kind=`, `agent=`, `model=`, `prompt_tokens=`, `completion_tokens=`, `latency_ms=`, `prompt_chars=`. Per-layer detail goes to the `debug` level so `info` stays narrow. Visible via `RUST_LOG=tengu=info` without any extra setup. |
| Eval mirror | `eval_builder.rs` records one Observation per `MetricsRecorded` event so eval rows can compare token consumption across runs. |

**Verification (run on host — sandbox can't hit Qdrant):**

```bash
cd ~/development/tengu-cluster
# Mixed-mode (planner=OpenRouter, subagents=Claude Code) is the verified
# pattern; researcher.toml declares `engine = "claude_code"` so the
# claude_code feature is required for any subagent dispatch test.
cargo build --release --features "qdrant claude_code"
cargo test --features "qdrant claude_code" -- metrics                    # unit tests on MetricsRecord/AggregatorState
cargo test --features "qdrant claude_code" -- ipc_output_with_metrics    # IPC round-trip with metrics

# RUST_LOG smoke — every LLM call should emit one info-level "metrics" line
RUST_LOG=tengu=info cargo run --release --features "qdrant claude_code" -- chat --sandbox aura
> what is the current BTC price in USD?
# Expect (in order):
#   - one or more `kind=embedding ... model=text-embedding-3-small` lines (registry search + reindex)
#   - one `kind=planner ... agent=planner/aura` line with prompt_tokens/completion_tokens
#   - one or more `kind=subagent ... agent=researcher` lines (one per inner-loop turn)

# TUI panel — opt-in
TENGU_TUI_METRICS=1 cargo run --release --features "qdrant claude_code" -- chat --sandbox aura
# Expect a System bubble after each LLM call:
#   metrics: tok in/out 4.5k/812 · last planner (1.2k) · session 5.6k · planner/aura 1.2k
```

**Doctrine impact:**
- Metrics are **observability**, never load-bearing. Every record path is fail-soft — a `tx.send` on the broadcast bus that has zero subscribers returns Err and we ignore it.
- The trait extension is **additive** with default impls returning zeros, so any third-party `ChatServiceFactory` impl (today there's only `RuntimeChatServiceFactory`) keeps working unmodified.
- The IPC payload extension is **backwards-compatible** — older child binaries that don't emit `metrics` produce JSON without that key, and serde's `#[serde(default)]` populates an empty vec.

**Open follow-ups:**
- The `AggregatorState` is per-TUI-spawn and lost on restart. Persisting rollups to a `metrics.jsonl` file would let `tengu metrics` print a "this week's spend" summary. Punted; one-line tracing baseline + bus subscriber covers the live use case.
- Telegram channel doesn't render the metrics surface yet (only the TUI does). Same env-var gating would extend it; deferred until someone needs it.

---

## End-of-session 2026-04-27 — TL;DR (latest first)

The system is **fully working end-to-end** with both OpenRouter and Claude Code
engines, mixed-mode (planner = OpenRouter, subagents = Claude Code), and
through Telegram. Verified by live smoke: `list files I have stored` returns a
real `persistent_store` list; `what is the BTC price?` returns a real
CoinGecko number; `hi` produces a clean Direct response from the planner with
no subagent dispatch.

What landed this session (Phases 7.1 → 7.7 — see per-phase sections below):

| Phase | What |
|---|---|
| 7.1 | Deleted legacy static path: `OrchestratorAgentPlanner`, `ChatWorker`, `engine="static"` branches, `roster.rs`. `engine="rag"` is the only orchestrator engine. |
| 7.2 | Subprocess inherits sandbox config via `Config.sandbox_name` + IPC. Fixes scope-deny in subagents. |
| 7.3 | `AgentSpec.engine` field — subagents honour `engine="claude_code"` declared in their TOML. |
| 7.4 | Claude Code subagent integration: bridge_tools wired, planner prose-fallback in `parse_verdict`. |
| 7.5 | Subprocess stderr passthrough + tool-list diagnostic logging. |
| 7.6 | MemoryManager in subprocess + CompressAndStorePlugin handler + bridge `WORKSPACE_TOOLS_ALLOWLIST` + env forwarding (TENGU_SESSION_ID, OPENROUTER_API_KEY) to MCP bridge subprocess. |
| 7.7 | **Consolidation refactor.** `register_core_plugins` extracted; sandbox-config loader unified; one `WORKSPACE_TOOLS_ALLOWLIST`; sync memory-init wraps async. Adding a tool is now a one-place edit. |

**Working, verified end-to-end:**
- Static-mode → deleted (Phase 7.1).
- RAG-mode planner (OpenRouter) emits clean JSON, routes correctly.
- RagPlanner persists messages to `tengu_messages` (Phase 6.4 full).
- Cross-session recall opt-in via `memory.cross_session_msg_top_k > 0` (Phase 6.4 read-back).
- Claude Code subagents call `mcp__tengu-tools__persistent_store` and other tengu tools through the MCP bridge (Phase 7.4–7.6).
- Workspace-fingerprint dedup skips reindex when nothing changed (Phase 6.2).
- TTL purge over `tengu_messages` + `tengu_outputs` runs on cold-start when `ttl_days > 0` (Phase 6.3).
- Sandbox-config inherits cleanly through the IPC boundary into subagent subprocesses (Phase 7.2).
- Telegram + Claude Code mixed-engine works.

**Known smells (not blocking, future polish):**
1. `model finished without calling compress_and_store` fires for Claude Code subagents — they answer correctly but don't reliably call the "I'm done" tool. Step output skips `tengu_outputs` write in those cases. Middle-ground protocol forgives it (final text becomes summary in IPC). Fix would be a stronger system-prompt nudge or harness-side enforcement.
2. `researcher.toml` references `web-research` and `summarizer` skills that don't exist on disk. The `skill body not found in any tier` warnings are cosmetic but the registry advertises phantom skills.
3. `sessio_id` sharing between RagPlanner and SubprocessRunner is still open — they each mint their own UUID.
4. The MCP bridge still has its own inline DiskVectorStore construction (Refactor #4 left it alone — different scope from main path; would benefit from a follow-up using `build_vector_stack_async` directly).

**Open items prioritized:**
- (Polish) `compress_and_store` reliability with Claude Code subagents.
- (Polish) Create or remove `web-research` / `summarizer` skill stubs.
- (Architectural) `session_id` unification across planner + runner.
- (Cleanup) Bridge memory-init using `build_vector_stack_async`.

---

## Last session (2026-04-26) — what landed

**Five commits in `main`:**

- `8c38210 chore: gitignore .claude/ (per-user IDE settings)`
- `25a1933 chore(warnings): #[allow(dead_code)] sweep — phase 7.2` — 16 pre-existing dead-code warnings annotated. Build is warning-clean.
- `c3fe7fd phase 6.1 full: OrchestratorEvent::RagQueried + bus plumbing` — RagPlanner emits `OrchestratorEvent::RagQueried { phase, query, hits }` on every plan/replan, on the same bus as `PlanCreated`/`StepStarted`. `Orchestrator::new` now takes the bus as an explicit constructor arg. `eval_builder` records the event for the judge; TUI swallows it (debug-panel render is the explicitly-deferred follow-up).
- `e248d82 registry recall: example_queries per agent + threshold realism` — three changes: (a) `agents/*.toml` gain an `example_queries` list; the indexer writes a SECOND vector per agent built from those examples; `search_registry` dedups by `(kind, name)` taking max score. (b) The 0.6 threshold in `SKILL.md` was unreachable with `text-embedding-3-small`; replaced with a ~0.15 floor and "use your judgement reading the description" guidance. (c) Researcher description rewritten to mention concrete domains (BTC/ETH/stocks/CoinGecko/weather/news) instead of generic "fact-finding".
- *(commit hash TBD)* `auto-reindex tengu_registry on first rag-mode chat turn` — `RagPlanner::rag()`'s `OnceCell` init now runs `reindex_all_workspace` from `current_dir()` exactly once per chat process, fail-soft. Eliminates the silent-stale-index footgun: edit `agents/*.toml` and the next `tengu chat` picks it up without a manual `tengu registry reindex-all`. `placeholder_tools` and `reindex_all_workspace` moved from `main.rs` to `rag/indexer.rs` so the CLI subcommand and the auto-reindex path share one implementation.

**Verified end-to-end this session (rag mode, sandbox=aura):**

- *"what is the current BTC price in USD?"* → planner routed to `researcher` → returned **"$78,034.10 USD"** (live CoinGecko fetch).
- Score lift on the same query against `researcher`: **0.21 → 0.355** (the example_queries vector is the matching one).
- Multi-turn coherence (BTC → "what about ETH?" → "on coingecko") was NOT re-verified after the SKILL.md tune. It was working before the tune; the tune shouldn't have regressed it, but it's worth a smoke run when picking up.

**Reminder:** the registry is auto-reindexed on the first user message of each chat process (Phase 6.x, landed 2026-04-26 — see `RagPlanner::auto_reindex_once`). Editing `agents/*.toml` and restarting `tengu chat` is now sufficient. Manual `tengu registry reindex-all` is still available for one-shot CLI reindexing without starting a chat.

---

## Phase 7.7 — single source of truth for plugin registration (landed 2026-04-27)

The Bug B / Bug C debacle from Phase 7.6 was caused by tool registration being duplicated between `channel_runtime::build_tool_executor` and `mcp_bridge::build_bridge_executor` — fixing one didn't fix the other. Refactor #1 of the cleanup audit consolidates them.

**What landed:**
- New `pub(crate) async fn register_core_plugins(registry, ctx, allowed_names, allowed_list, opts)` in `channel_runtime.rs` — registers the seven plugins shared by every executor: workspace, memory, cache (opt-in), skill-lifecycle (opt-in), compress_and_store (opt-in, qdrant), http, crypto.
- `permissive_scope` made `pub(crate)` so the bridge calls it instead of inlining its own copy.
- `build_tool_executor`: ~120 lines of plugin-registration boilerplate replaced by one call to `register_core_plugins` + the two in-process-only registrations (SkillPlugin, McpPlugin).
- `build_bridge_executor`: ~90 lines replaced by the same call + nothing extra.
- `CLAUDE.md` now has a "How to add a new tool" section pointing at the single edit-site (`register_core_plugins`).

**Net effect on the "add a tool" workflow:** before Phase 7.7, adding a workspace tool required ~4 edits across 2 files (with subtle drift opportunities); after, it's 1 edit in 1 function. The bridge inherits new tools automatically.

**Refactors #2–#5 also landed in the same session:**

- **#2 — single sandbox-config loader.** `run_agent_subprocess` Phase 7.2 reload now delegates to `load_sandbox_or` instead of inlining its own `Config::load` + fallback. The "comment explicitly warns of duplication" hazard is gone.
- **#3 + #5 (combined) — single `WORKSPACE_TOOLS_ALLOWLIST` constant.** Was duplicated as `VALID_WORKSPACE_TOOLS` (channel_runtime) and `SYNTHESIZED_WORKSPACE_TOOLS` (mcp_bridge). Now one `pub(crate) const` in `channel_runtime.rs` that both `agent_config_from_spec` and the bridge filter against. Adding a fourth opt-in workspace tool is now a one-line edit.
- **#4 — single memory-init source.** `build_vector_stack` (sync, ~60 lines of Qdrant/disk decision logic) and `build_memory_manager` (sync, ~30 lines of provider construction) are now thin block_on wrappers around their async siblings. The async versions are the canonical implementation; the sync versions exist only because TUI / Telegram callers haven't migrated to async yet.

**Net effect on "add a new tool/skill/agent/workspace_tool":**
- Tool: edit `register_core_plugins` (one place). Bridge inherits.
- Workspace-tool opt-in: edit `WORKSPACE_TOOLS_ALLOWLIST` constant (one place).
- Skill: drop a `SKILL.md` file (one place, auto-reindex).
- Agent: drop an `agents/<name>.toml` file (one place, auto-reindex).

---

## Phase 7.6 — Claude Code MCP-routed tool calls actually work (landed 2026-04-27)

Symptom: with Phase 7.4's bridge_tools wiring, Claude Code subagent saw `persistent_store` and `compress_and_store` in its MCP tool list, attempted to call them, and got `Tool 'X' is not available to this agent` from the executor. Root cause: tool DEFS were being advertised but no tool HANDLER was registered in the executor's registry.

**Bug A — `persistent_store` had no handler.** `build_subprocess_tool_executor` passed `memory_manager: &None` to `build_tool_executor`, so the `MemoryPlugin` couldn't register either `persistent_store` or `memory_ingest`. The tool def was in the LLM's view but the executor had no backing implementation. **Fix:** new async sibling `build_memory_manager_async` (mirrors `build_memory_manager` but doesn't need a `&Runtime` arg — usable from inside the subprocess's tokio context). `run_agent_subprocess` builds the manager from `parent_config.memory` when enabled, threads it through a new `memory_manager` parameter on `build_subprocess_tool_executor`.

**Bug B — `compress_and_store` had no handler.** The OpenRouter path detects `compress_and_store` calls out-of-band in the runner's tool loop, never going through the executor. Claude Code's MCP path goes directly through the executor → registry → fail. **Fix:** new `CompressAndStoreTool` + `CompressAndStorePlugin` in `plugins/skill_lifecycle/compress_and_store.rs`. Plugin reads `TENGU_SESSION_ID` env (set by `run_agent_subprocess` from `input.session_id`) at execute time. Both paths now write to `tengu_outputs` with identical semantics — OpenRouter via the out-of-band check (which short-circuits BEFORE the executor fires); Claude Code via the registered plugin handler.

**Diagnostic added in the same pass:** `subprocess tool stack built ... tools=[...]` log line showing the literal tool list, plus subprocess stderr now inherits to the parent terminal (was piped to oblivion). Both critical for debugging future Claude Code integration issues.

---

## Phase 7.4 — Claude Code subagent integration (landed 2026-04-27)

Two real bugs surfaced during the first Claude Code + Telegram e2e test:

**Bug 1 — Claude Code subagent saw no tengu tools.** `run_agent_subprocess` had `bridge_tools: None` in the `EngineContext`. Without this, the Claude Code CLI only had its built-in `Read/Write/Edit/Bash` tools — tengu plugin tools (`http_request`, `compress_and_store`, `persistent_store`, etc.) were invisible. Symptom: agent fetched BTC via `curl` (Bash) instead of `http_request`, then tried to call `compress_and_store` as a bash command and gave up. **Fix:** populate `EngineContext.bridge_tools` with the same `tools` list that's passed to `run_single_engine_turn` when `spec.engine == "claude_code"`. OpenRouter path leaves it None (function-calling API handles it).

**Bug 2 — Planner LLM returned prose, not JSON.** Claude Code CLI is an interactive agent by nature; even with `skills/orchestrator/SKILL.md` set as `--system-prompt` and instructing JSON-only output, it returned conversational text on a "What do you have in memory?" message. `parse_verdict` failed with `"expected value at line 1 column 1"` and the user saw `System error: orchestrator initial call failed`. **Fix (two parts):**
  - SKILL.md hardened with a "STRICT JSON ONLY" preamble plus explicit examples of forbidden output shapes.
  - `parse_verdict` got a Phase 7.4 fallback path: if no balanced JSON object exists in the response AT ALL, wrap the whole text as `Direct { response: <text> }` and log a warn line. Much better UX than a hard parse error — user still sees the model's reply, and the warn line surfaces the problem so it can be tuned.

The two fixes together make Claude Code + orchestrator viable. The planner can still emit raw JSON when it wants to (clean OpenRouter path); when it can't, the prose fallback prevents user-visible breakage.

---

## Phase 7.3 — subagent honours spec.engine (landed 2026-04-27)

Architectural gap surfaced when prepping a Telegram + Claude Code test: `run_agent_subprocess` hardcoded `build_openrouter_engine`, so an agent declaring `engine = "claude_code"` in its TOML was silently ignored on subagent dispatch — only the parent (orchestrator agent) honoured the engine field.

**Fix:** added `engine: String` field to `AgentSpec` (default `"openrouter"` for back-compat). `agent_config_from_spec` now propagates `spec.engine` to the synthesized `AgentConfig.engine`. `run_agent_subprocess` switched from `build_openrouter_engine` directly to `build_engine` with the synthesized config + `parent_config.claude_code.as_ref()`. With this, setting `engine = "claude_code"` in `agents/<name>.toml` and building with `--features claude_code` makes the subagent run through the Claude Code CLI engine.

Doc updated on `AgentSpec.model` to clarify the format depends on engine — OpenRouter slug (`anthropic/claude-sonnet-4-6`) for `openrouter`, bare model name (`claude-sonnet-4-6`) for `claude_code`.

---

## Phase 7.2 — subprocess sandbox-config inheritance (landed 2026-04-26)

Real production bug surfaced during e2e smoke after Phase 7.1 (full): when the orchestrator dispatched a step to `researcher` as a subprocess, the child process loaded the user's default config (`~/.tengu/config.toml`) — NOT the parent's `sandboxes/aura/config.toml`. Result: sandbox-specific scopes (`default_scopes.http_request.net_hosts = ["*"]`), secrets, and MCP servers all silently disappeared in the child, so `http_request` calls scope-denied even though the parent allowed them. User-visible symptom: `"unable to retrieve the current Bitcoin (BTC) price in USD due to repeated errors while accessing the API"` while the parent's logs showed a successful `✓ step-1`.

**Fix:** added `Config.sandbox_name` (`#[serde(skip)]`, runtime-resolved field), populated by `load_sandbox_or` after a sandbox config is loaded. `SubprocessRunner::new` now takes `sandbox_name: Option<String>`, threaded from `build_orchestrator(config, ...)` via `config.sandbox_name.clone()`. `AgentIpcInput` gained a new `sandbox_config: Option<String>` field (distinct from the legacy `sandbox` workspace-path field, which was dead code). `run_agent_subprocess` reads `input.sandbox_config` and re-resolves `sandboxes/<name>/config.toml` when it's present, falling back to the default user config when it's not. Mirror of `load_sandbox_or` exactly so the parent + child see the same config.

---

## Open security item — leaked Anthropic API key in git history

A commit prior to `e0029b8 "planned"` contained `<!-- sk-ant-api03-... -->` in `README.md`. That comment is gone from `HEAD` but still lives in the prior commit. **Revoke that key at console.anthropic.com regardless** — removing it from a working file does NOT unleak it; anyone with read access to the repo can still pull it from `git log -p`. A history rewrite (`git filter-repo`) is doable but invalidates every existing clone, so don't attempt it without explicit go-ahead.

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
| 6.1 (lite) — planner tracing | ✅ done | `tracing::info!` line per plan/replan with top-10 hits + scores |
| 6.5 — cross-plan recall in replan | ✅ done | `RagPlanner::replan` queries `tengu_outputs` for top-K and injects as context block |
| 6.4 (lite) — per-session history | ✅ done | In-memory ring buffer of last-N user messages on `RagPlanner`; injected as "Recent user messages" block before current turn so the planner sees prior context across multi-turn chats |
| 7.2 — dead-code annotation sweep | ✅ done (committed `25a1933`) | 16 `#[allow(dead_code)]` annotations on the static-mode orchestrator path + v1 MemoryProvider hierarchy. Build is warning-clean. |
| 6.1 (full) — RagQueried event + bus plumbing | ✅ done (committed `c3fe7fd`) | RagPlanner takes `Option<EventBus>`; `Orchestrator::new` takes explicit `bus`; eval_builder records the new variant, TUI swallows it |
| Registry recall — example_queries + threshold | ✅ done (committed `e248d82`) | Per-agent `example_queries` indexed as a 2nd vector; `search_registry` dedups by `(kind, name)` max-score; SKILL.md threshold dropped 0.6→~0.15 with LLM-judgement language; researcher description rewritten |
| Auto-reindex on first chat turn | ✅ done (commit hash TBD) | `RagPlanner::rag()` runs `reindex_all_workspace` once per chat process via OnceCell init; fail-soft; shared with CLI `tengu registry reindex-all` |

**Pre-existing v1 bugs fixed along the way:**
- TUI nested `rt.block_on` panic in memory-stats path (`src/adapters/tui/mod.rs:770`).
- `tengu chat` lacked `--sandbox` flag — added, parity with `tengu telegram`.
- `sandboxes/aura/config.toml`'s `[orchestrator]` block pointed at non-existent `"orchestrator"` agent — corrected (and removed entirely for daily-use static dispatch).
- `sandboxes/aura/config.toml` lacked `default_scopes.http_request` net allow-list, causing scope check failures on real HTTP calls.

---

## What remains — prioritised

All optional. Pick by friction in real use, not theoretical completeness.

### Polish (low risk, ~½ day each)

- ~~**6.1 (full) TUI debug panel**~~ — **Done (commit hash TBD).** TUI now renders `OrchestratorEvent::RagQueried` as a single compact System bubble — `rag-{phase} "{query}" → researcher(0.55) tool/http_request(0.32) ...` — gated on env var `TENGU_TUI_RAG_DEBUG=1` so default behaviour is unchanged (no clutter for normal use). Top-3 of the top-10 hits the planner sees are shown; query truncated to 60 chars; agent hits omit the `kind/` prefix to keep the line narrow. Logging path (`RUST_LOG=tengu=info`) is unchanged — both surfaces are powered by `emit_rag_query` in `planner.rs`. Run `TENGU_TUI_RAG_DEBUG=1 cargo run --release --features qdrant -- chat --sandbox aura` to see it.
- ~~**Per-example vectors (registry recall, v2)**~~ — **Done (commit hash TBD).** `index_agents` now writes one vector per `example_queries` entry instead of one joined block. Embedded text is the bare example string (tight cosine to user queries); stored snippet is the full agent description (so the planner LLM still reads full context after dedup-by-(kind,name) max-score). `query::search_registry` over-fetch bumped 4×→6× to accommodate up to ~16 examples per agent without starving tools/skills slots. **Side effect:** embedding-API calls per reindex jump roughly 6→29 for the current 3-agent set; reinforces the urgency of handoff item 6.2 (content-hash dedup) since auto-reindex hits this on every chat startup.
- ~~**6.2 — content-hash dedup in indexer**~~ — **Done (commit hash TBD), workspace-fingerprint variant.** Avoided the per-entry deterministic-ID redesign (which would have required widening `VectorStore::write` semantics) in favour of a simpler all-or-nothing approach: compute sha256 over (sorted) agent specs + skill entries + tool defs as a workspace fingerprint, persist to `<root>/.tengu/registry-fingerprint`. On reindex, compare new vs cached — match → skip clear+embed+upsert entirely; mismatch → full reindex + cache update. `RegistryReindexed` gained an `unchanged: bool` field; CLI and auto-reindex log paths handle both cases. `TENGU_REGISTRY_FORCE_REINDEX=1` bypasses the cache for recovery. **Tradeoff vs per-entry dedup:** any single edit (one TOML, one description) still triggers a full re-embed. The win is the warm-startup case — once the workspace settles, every `tengu chat` boot hits the cache and zero embedding-API calls fire. `.gitignore` updated to exclude the fingerprint file.
- ~~**6.3 — filter-based TTL purge**~~ — **Done (commit hash TBD).** Widened `VectorStore` trait with `delete_older_than(field, cutoff)` (default impl returns Ok(0) for backends that can't push the filter down — disk store keeps that default). Qdrant impl: scroll matching points by `Filter { must: [Range { lt: cutoff }] }` on `extra_rag_created_at`, then `delete_points` by id list, returning the precise count. `cleanup.rs::ttl_cleanup` purges both `tengu_messages` and `tengu_outputs` (registry deliberately excluded — deterministic from workspace, not time-decaying). Hooked into `RagPlanner::auto_reindex_once` so cold start runs the purge alongside the reindex; no-op when `memory.ttl_days == 0`. Fail-soft on Qdrant errors.

### Architectural completeness (medium effort)

- ~~**6.4 (full) — durable user-message persistence**~~ — **Done (commit hash TBD), partial scope.** `RagPlanner` now mints a `session_id` at construction (`TENGU_SESSION_ID` env override, else fresh UUID — mirrors `SubprocessRunner`) and writes every `plan()` user message to `tengu_messages` via `RagStore::store_memory` with `MemoryKind::Message`. Fail-soft on Qdrant/embedder errors. `replan()` does NOT write (same user_message in the same turn cycle would dup-row, same pattern as the lite buffer). Forward-compat: added `RagStore::search_messages` so a follow-up commit can wire cross-session recall into the planner prompt as a one-liner. **What this does NOT do (deferred):** (1) ~~read-back hydration on startup~~ — **shipped in 6.4 read-back, see below.** (2) session_id sharing with `SubprocessRunner` — the open question from the original handoff is still open; today the subprocess uses its OWN UUID.
- ~~**6.4 read-back hydration**~~ — **Done (commit hash TBD).** New config knob `memory.cross_session_msg_top_k` (default `0` = off, no behaviour change for existing users). When > 0, `RagPlanner::plan()` and `replan()` inject a `## Cross-session message recall` block of the top-K semantically-similar prior user messages from `tengu_messages` BEFORE the current turn. Order in plan(): semantic-read → durable-write → in-memory-push, so the just-written current message can never appear in its own recall block by construction. Read path filters exact-content duplicates as a safety net. Fail-soft on every error path (RagStore unavailable, embedder rate-limit, Qdrant unreachable). Try with `memory.cross_session_msg_top_k = 3` in your sandbox config + a multi-day usage pattern.
- ~~**6.6 — MCP tool indexing**~~ — **Done (commit hash TBD).** `placeholder_tools()` is gone. Replaced by `enumerate_builtin_tools()` (delegates to `channel_runtime::compute_bridge_tools(true, &["shared_cache","persistent_store","skill_distill"])` for the FULL roster regardless of per-agent capability flags) plus a new `enumerate_mcp_tools(servers)` async function that connects to each `Config.mcp_servers` entry, calls `tools/list`, and emits `{server}.{tool}` ToolDefs. Fail-soft per server. `reindex_all_workspace` now takes `mcp_servers: &[McpServerConfig]` as a first-class arg; `RagPlanner::new` and `auto_reindex_once` plumb it through so editing `mcp_servers` in sandbox config and restarting `tengu chat` auto-refreshes registry MCP entries. CLI subcommands (`registry reindex-tools` and `registry reindex-all`) updated; both now report MCP server count alongside the indexed totals.
- ~~**6.7 — C→B unknown-agent fallback (B half)**~~ — **Done (commit hash TBD).** New `AgentCompose` struct on `Step` with `base_agent`/`skills`/`tools`. Plumbed through `AgentIpcInput` → `run_agent_subprocess` so when `compose` is set: loads `agents/<base_agent>.toml`, overrides `skills`+`tools` for THIS run only (file on disk unchanged). `plan_schema.json` extended to allow the optional `compose` object. SKILL.md gains a "C → B fallback" section spelling out the two-turn dance — turn 1 emits a Direct asking the user to confirm the closest match, turn 2 emits a Plan with `compose` on confirmation. Per-step composition means parallel composed steps in one plan are mechanically supported (though not used by today's SKILL.md guidance, which keeps it to a single step). **Open**: persisting a composed agent ("save this as `<name>`") would require writing a new `agents/*.toml` and triggering reindex — deferred per REDESIGN §11.

### Cleanup (do last)

- ~~**7.1 — delete legacy**~~ — **Fully done (commit hash TBD).** Landed in two passes the same session, after the user confirmed e2e rag-mode smoke passed. Pass 1 deleted `roster.rs` from the compilation graph and inlined `render_roster` into `channel_runtime.rs`. Pass 2 (this one) deleted: (a) `OrchestratorAgentPlanner` struct + `Planner` impl + the `MemoryManager` import — replaced its associated `parse_verdict` helper with a free `pub(crate) fn parse_verdict` in the same file; (b) `ChatWorker` struct + `WorkerHandle` impl + four `ChatWorker`-coupled tests (only the `snapshots_inputs_fn` test survives in `wiring.rs::threading_tests`); (c) all static-mode branches in `build_orchestrator` — the function now constructs only `RagPlanner + SubprocessRunner`, gated on the `qdrant` cargo feature, and returns `None` (with a one-line warning) when `cfg.engine != "rag"` or qdrant is off. Inlined `render_roster` deleted along with its only caller. `roster.rs` is now an orphan deprecation stub on disk — finish the cleanup with `git rm src/adapters/orchestrator/roster.rs` as part of the commit. **Behaviour change:** orchestration without qdrant or with `engine != "rag"` is now disabled (returns None → channel falls back to direct default-agent dispatch); static-mode users must opt into rag.

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
- **`sandboxes/aura/config.toml` HAS `[orchestrator] engine = "rag"` set** as of 2026-04-26. Routing to `researcher` for off-pipeline questions (e.g. crypto prices) is the verified path. If you flip this back to static mode for some reason, expect Aura's LLM to punt with link-recommendations on questions like "what is the BTC price?" — the rag-mode delegation is what makes those work.
- **Aura's model is `anthropic/claude-sonnet-4-6`** — was haiku-4-5; haiku struggled with strict-format compliance and tool-use confidence. Keep on sonnet for any rag-mode work.
- **Aura's identity instructions were broadened** — added "for general user questions outside the DeSci pipeline... use the http_request tool freely." Without this, aura's heavy-DeSci identity skewed it toward "punt with recommendations" instead of using HTTP.
- **Registry score floor is `~0.15`, not `0.6`** (changed 2026-04-26 in `skills/orchestrator/SKILL.md`). `text-embedding-3-small` against short agent descriptions tops out around 0.30–0.40 even for clearly-relevant matches; `0.6` was unreachable and caused universal Direct fallback. The new SKILL.md asks the planner LLM to use its own judgement reading the description; the score is a noise floor, not a real gate. If you swap to a stronger embedding model later, revisit this number.
- **Auto-reindex runs on first chat turn** — `RagPlanner::rag()`'s lazy `OnceCell` init calls `reindex_all_workspace` from `current_dir()` exactly once per chat process, fail-soft. So `tengu chat --sandbox aura` after editing `agents/*.toml` picks up the change automatically. Side effect: a chat startup with `engine = "rag"` makes ~17 OpenAI embedding API calls (one per registry entry); cheap but worth knowing. Content-hash dedup (handoff item 6.2) would skip the redundant re-embeds when nothing changed.
- **Each agent gets up to TWO vectors in the registry now** (description + example_queries, when present). `search_registry` dedups by `(kind, name)` taking max score, so callers see one entry per agent. If you write a new search caller that walks raw `MemoryHit`s, mind the duplicate-name case.

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

**Step 0 — pre-flight (always run first):**

```bash
cd ~/development/tengu-cluster
git status --short                # if non-empty, commit the two pending patches first (commands at top of this file)
docker ps | grep qdrant           # Qdrant must be up on :6334
echo $OPENROUTER_API_KEY          # must be set
cargo build --features qdrant 2>&1 | grep -E "^warning:" | wc -l   # must be 0
```

Then a quick smoke to confirm the rag pipeline still works after committing:

```bash
RUST_LOG=tengu=info cargo run --release --features qdrant -- chat --sandbox aura
> what is the current BTC price in USD?     # turn 1 — fetches real number via researcher
> what about ETH?                           # turn 2 — must use turn 1's "price" context
> on coingecko                              # turn 3 — must use turns 1+2 to answer about ETH on coingecko
```

If turns 2/3 fall back to "what platforms?" / "what do you want?", multi-turn coherence has regressed and you are looking at a `RagPlanner` history-buffer issue.

**Step 1 — paste this into a fresh Claude session:**

> "I'm continuing the tengu-cluster v2 redesign. Read `docs/SESSION_HANDOFF.md` first — it has the full state including two patches that were verified working last session and committed to disk. Pre-flight already done; build is warning-clean; rag-mode smoke passes. Today I want to work on **<pick one>**:
> - **TUI debug panel** (the deferred half of 6.1 full) — render `OrchestratorEvent::RagQueried` as a live debug panel in the TUI showing top-K hits per planner call
> - **Per-example vectors** — push researcher's BTC-query score from 0.355 to 0.5+ by indexing each `example_queries` entry as its own vector
> - **6.4 (full) user-message persistence to `tengu_messages`** — durable cross-restart memory keyed by session_id
> - **6.6 real MCP tool indexing** — replace `placeholder_tools()` with enumerated compiled-in tools + MCP server `tools/list`
> - **7.1 delete legacy** — drop `roster.rs` + the `engine = "static"` branch + the `OrchestratorAgentPlanner`
> - **6.7 C→B unknown-agent fallback (B half)** — compose generic agent on user confirmation when all RAG hits below threshold
>
> Confirm understanding then propose the smallest commit-sized change."

That gives the new session the full context without re-litigating any closed decisions.

---

*Last updated 2026-04-26 after a session that landed phase 7.2 (dead-code sweep) and prepared two more uncommitted patches: phase 6.1 full (RagQueried event + bus plumbing) and the registry-recall fix (example_queries per agent + SKILL.md threshold realism + researcher description rewrite). Verified end-to-end: a "what is the BTC price?" query routes through `researcher` and returns a real CoinGecko number. The doctrine holds: behaviour is changeable by editing `.toml` and `SKILL.md` — no PR required.*
