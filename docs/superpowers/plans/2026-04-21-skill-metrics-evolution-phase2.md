# Skill Metrics + Evolution — Phase 2 Plan

> **Archived (2026-09-18)** — historical; current behaviour: see `README.md` / `docs/architecture-2026-04-27.md`.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan.

**Supersedes (partial):** `docs/superpowers/plans/2026-04-20-skill-metrics-evolution.md` Tasks 12-21. Tasks 1-11 already landed on `main` (commits `2dc6e6d`..`acae292`).

**Trigger for refresh:** `feature/harness-orchestration` merged to `main` via PRs #6/#7/#8, and the phase-b LLM-judge runner (`src/adapters/eval_builder.rs`, 1666 lines) came back with it. My original Task 12-13 plan assumed `eval_builder.rs` was deleted; it isn't. The real seam is `ChatServiceFactory::run_turn(agent, text) -> Result<String>` (`src/adapters/orchestrator/wiring.rs:30`), not a hypothetical free function.

**Goal:** Land typed-metric scoring + `skill_distill` tool + `tengu skill evolve` closed loop + dogfood fixtures, by **integrating with `eval_builder.rs`** rather than replacing it.

**Architecture:** `eval_builder.rs` keeps its row-based LLM-judge runner. When a skill's SKILL.md frontmatter declares a `metrics:` block, `eval_builder::run_skill` *also* scores each row against each declared metric kind via the `skill_lifecycle::metrics::MetricKind` trait (Tasks 2-6) and writes a rolling `metrics.json` + `history.jsonl` via `skill_lifecycle::storage` (Task 7). `tengu skill evolve` calls `eval_builder::run_skill` per cycle for baseline + rescore, uses `ChatServiceFactory::run_turn(improver_agent, user_msg)` to request proposals, picks the best cycle against the same rolling metrics, and applies atomically with user approval.

**Tech stack:** Rust stable, `tokio`, `serde`, `serde_yaml`, `anyhow`, `clap`, `chrono`, `similar` (new). `git worktree` shelled out.

---

## Prerequisites (verified present on `main`)

| File | Function |
|---|---|
| `src/adapters/orchestrator/wiring.rs` | `ChatServiceFactory` trait (the seam evolve uses for the improver agent) |
| `src/adapters/orchestrator/mod.rs` | `Orchestrator::handle(String) -> String` (not used directly by Phase 2) |
| `src/adapters/channel_runtime.rs` | `RuntimeChatServiceFactory`, `build_orchestrator`, `build_memory_manager` |
| `src/adapters/memory/{manager, injector, writer}.rs` | Memory subsystem (used by eval via factory) |
| `src/adapters/eval_builder.rs` | Row-runner + LLM judge (Phase 2 extends this) |
| `src/adapters/skill_lifecycle/metrics.rs` | `MetricKind` trait + `MetricSpec` + `validate_metrics` (Task 2) |
| `src/adapters/skill_lifecycle/metric_kinds/*.rs` | `ShellCheckKind`, `ToolAssertionKind`, `ScriptKind`, `LlmJudgeKind` (Tasks 3-6) |
| `src/adapters/skill_lifecycle/storage.rs` | `finalize_run`, `MetricsJson`, `history.jsonl` (Task 7) |
| `src/adapters/skill_lifecycle/fixtures.rs` | YAML read/write + transcript extraction (Task 8) |
| `src/adapters/plugins/skill_lifecycle/distill.rs` | `SkillDistillTool` (Task 10) |

**Known limitation carried forward from Task 9:** `PluginToolExecutor.conversation` is not populated from the live engine message buffer. `skill_distill` therefore receives an empty `ConversationView` when dispatched from a real conversation. A dedicated follow-up (post-Phase-2) adds interior mutability or a trait-level threading change. Tests bypass this by constructing `ConversationView::new(&messages)` directly.

---

## File changes map

```
src/adapters/
├── eval_builder.rs                    # MODIFIED: parse SKILL.md frontmatter metrics; score rows
│                                      #   against declared metric kinds; write metrics.json +
│                                      #   history.jsonl; extend RowResult with per-metric outcomes.
├── skill_lifecycle/
│   ├── scratch_worktree.rs            # NEW (Task β): git worktree helper + non-git fallback.
│   ├── evolve.rs                      # NEW (Task γ): cycle loop + pick_best + apply_proposal_to_skill_md.
│   ├── approval_gate.rs               # NEW (Task γ): terminal diff + y/n/d/o prompt.
│   ├── mod.rs                         # MODIFIED: uncomment scratch_worktree, evolve, approval_gate.
│   └── metrics.rs                     # MINOR MODIFY: make `MetricRunCtx::new(skill_dir, workspace)`
│                                      #   constructor if the fields aren't already exposed.

src/main.rs                            # MODIFIED: add Commands::SkillEvolve, SkillMetrics,
                                       #   SkillAcceptProposal; keep existing Commands::Eval.

skills/
├── skill-creator/
│   ├── SKILL.md                       # MODIFIED: Distillation section + metrics: block.
│   ├── metrics/
│   │   └── distill_quality.md         # NEW: LLM-judge rubric.
│   └── evals/
│       └── prompts.yaml               # NEW: 2 seed fixtures.
└── skill-eval/
    └── SKILL.md                       # MODIFIED: rewrite as CLI pointer.

Cargo.toml                             # MODIFIED: add similar = "2" for diff rendering.
```

---

## Task α: Integrate `metrics:` frontmatter into `eval_builder.rs`

**Files:**
- Modify: `src/adapters/eval_builder.rs`

**Goal:** Optional enrichment on top of the existing row-judge runner. Skills that declare `metrics:` in frontmatter get per-row-per-metric scoring AND the rolling `metrics.json` snapshot on disk. Skills without it continue to work unchanged.

### Step α.1: Add SKILL.md path to `SkillUnderTest`

In `eval_builder.rs`'s `SkillUnderTest` struct (around line 371), add:

```rust
pub struct SkillUnderTest {
    pub name: String,
    pub tier: SkillTier,
    pub evals_dir: PathBuf,
    pub prompts_path: PathBuf,
    pub prompts_format: String,
    pub config_path: PathBuf,
    pub skill_md_path: PathBuf,   // NEW
    pub skill_dir: PathBuf,       // NEW — parent of evals_dir; needed for metric rubric/script paths
}
```

In `discover_skills` (around line 446), populate both: `skill_md_path = entry.path().join("SKILL.md")`, `skill_dir = entry.path().to_path_buf()`.

### Step α.2: Load metrics from frontmatter

Add a module-level helper near `parse_yaml_prompts`:

```rust
use crate::adapters::skill_lifecycle::metrics::{validate_metrics, MetricSpec};

#[derive(Debug, Deserialize)]
struct SkillFrontmatter {
    #[serde(default)]
    metrics: Vec<MetricSpec>,
}

pub(crate) fn load_skill_metrics(skill_md_path: &Path, skill_dir: &Path) -> Result<Vec<MetricSpec>> {
    if !skill_md_path.exists() {
        return Ok(vec![]);
    }
    let body = std::fs::read_to_string(skill_md_path)
        .with_context(|| format!("read {}", skill_md_path.display()))?;
    let Some(rest) = body.strip_prefix("---\n") else {
        return Ok(vec![]);
    };
    let Some(end) = rest.find("\n---") else {
        return Ok(vec![]);
    };
    let fm_yaml = &rest[..end];
    let fm: SkillFrontmatter = serde_yaml::from_str(fm_yaml)
        .with_context(|| format!("parse frontmatter of {}", skill_md_path.display()))?;
    if !fm.metrics.is_empty() {
        validate_metrics(&fm.metrics, skill_dir)?;
    }
    Ok(fm.metrics)
}
```

### Step α.3: Extend `RowResult` with per-metric outcomes

Find `RowResult` in eval_builder.rs. Add field:

```rust
#[derive(Debug, Clone, serde::Serialize)]
pub struct RowResult {
    // existing fields ...
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub metric_outcomes: Vec<MetricOutcomeJson>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MetricOutcomeJson {
    pub metric: String,
    pub pass: bool,
    pub score: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}
```

### Step α.4: Score rows against metrics in `run_row`

Find `run_row` (around line 985 — the function that dispatches one row via the agent and judges it). After the judge verdict is assembled but before the `RowResult` is returned, invoke metric scoring when `skill_metrics` is non-empty. Pass `skill_metrics: &[MetricSpec]` and `skill_dir: &Path` into `RowCtx` (the struct at line 768) and thread them through.

Sketch of the scoring block to add at the end of `run_row`:

```rust
use crate::adapters::skill_lifecycle::metric_kinds::{
    LlmJudgeKind, ScriptKind, ShellCheckKind, ToolAssertionKind,
};
use crate::adapters::skill_lifecycle::metrics::{
    FixtureContext, MetricKind, MetricRunCtx, MetricSpec,
};
use crate::adapters::shell_executor::LocalShellExecutor;

let mut metric_outcomes = Vec::new();
if !ctx.skill_metrics.is_empty() {
    let shell = LocalShellExecutor::default();
    let fixture = FixtureContext {
        prompt: &prompt_text,
        expected_outcome: observation.expected_outcome.as_deref(),
        transcript: &assistant_text,
    };
    let run_ctx = MetricRunCtx {
        skill_dir: ctx.skill_dir,
        workspace: ctx.workspace,
        shell: &shell,
        tools: None,              // Task α does not wire tool_assertion through a live registry yet.
        judge: Some(Arc::clone(&ctx.judge_client)),
    };
    for spec in ctx.skill_metrics {
        let outcome = match spec {
            MetricSpec::ShellCheck { .. }   => ShellCheckKind.run(spec, &fixture, &run_ctx).await,
            MetricSpec::LlmJudge { .. }     => LlmJudgeKind.run(spec, &fixture, &run_ctx).await,
            MetricSpec::ToolAssertion { .. } => ToolAssertionKind.run(spec, &fixture, &run_ctx).await,
            MetricSpec::Script { .. }       => ScriptKind.run(spec, &fixture, &run_ctx).await,
        };
        let o = outcome.unwrap_or_else(|e| crate::adapters::skill_lifecycle::metrics::MetricOutcome {
            pass: false, score: 0.0,
            notes: Some(format!("metric runner error: {e}")),
            raw: serde_json::json!({}),
        });
        metric_outcomes.push(MetricOutcomeJson {
            metric: spec.name().to_string(),
            pass: o.pass, score: o.score, notes: o.notes,
        });
    }
}
```

### Step α.5: Build a `JudgeClient` adapter for the existing judge engine

The `LlmJudgeKind` expects an `Arc<dyn JudgeClient>`. Adapt the existing eval judge engine into the trait:

```rust
use crate::adapters::skill_lifecycle::metrics::JudgeClient;

struct EvalJudgeClient {
    engine: Arc<dyn crate::adapters::types::Engine>,
    default_model: String,
}

#[async_trait::async_trait]
impl JudgeClient for EvalJudgeClient {
    async fn judge(
        &self,
        system: &str,
        user: &str,
        prefill: &str,
        model: Option<&str>,
    ) -> Result<String> {
        // Single-turn completion; force-prefill assistant with `prefill`.
        // Reuse whatever single-turn helper `engine_builder` already exposes.
        // If none exists, synthesize with messages = [{role:system,...},{role:user,...},{role:assistant,prefill}].
        // Return only the assistant delta AFTER the prefill (strip the prefill prefix).
        let _ = (system, user, prefill, model);
        anyhow::bail!("EvalJudgeClient::judge: wire to engine.complete_once or equivalent in Task α.5 follow-up");
    }
}
```

**Wiring note:** If the existing `eval_builder::judge_row` has a ready-made "single completion" helper, reuse that. Otherwise, the simplest path is a tiny private helper in `engine_builder.rs` that sends `[system, user, prefilled-assistant]` messages and returns the accumulated assistant text minus the prefill. Flag as DONE_WITH_CONCERNS if the engine's completion API is not obviously a single-shot shape.

### Step α.6: Thread `skill_metrics` through `run_skill`

In `run_skill` (line 912), before dispatching rows, load:

```rust
let skill_metrics = load_skill_metrics(&skill.skill_md_path, &skill.skill_dir)?;
```

Pass `skill_metrics.as_slice()`, `&skill.skill_dir`, and the `Arc<dyn JudgeClient>` adapter into each `RowCtx` constructed in the per-row loop.

### Step α.7: Write `metrics.json` + `history.jsonl` after all rows complete

At the end of `run_skill`, if `skill_metrics` is non-empty:

```rust
use crate::adapters::skill_lifecycle::storage::{finalize_run, RunSample};

if !skill_metrics.is_empty() {
    let ts = started_at.format("%Y-%m-%dT%H-%M-%SZ").to_string();
    let samples: Vec<RunSample> = skill_report.rows.iter().map(|r| {
        let mut outcomes = std::collections::BTreeMap::new();
        for mo in &r.metric_outcomes {
            outcomes.insert(mo.metric.clone(), crate::adapters::skill_lifecycle::metrics::MetricOutcome {
                pass: mo.pass, score: mo.score, notes: mo.notes.clone(),
                raw: serde_json::json!({}),
            });
        }
        RunSample { fixture_id: r.id.clone(), outcomes }
    }).collect();
    let rolling_window = 10;   // TODO: read from [skill_lifecycle] config if present.
    finalize_run(&skill.skill_dir, &skill.name, &ts, &skill_metrics, &samples, rolling_window)
        .with_context(|| format!("finalize_run for {}", skill.name))?;
}
```

### Step α.8: Tests

Extend `eval_builder.rs`'s existing test block OR add a new inline `#[cfg(test)] mod metrics_integration_tests`. Cover:

1. **`load_skill_metrics` returns empty when frontmatter has no `metrics:` block** — backward compat.
2. **`load_skill_metrics` parses 4-kind sample** — happy path with one of each kind + validate_metrics hits filesystem for `rubric_file` / `path`.
3. **`load_skill_metrics` bails on malformed metric kind** — surfaces validate_metrics error.

End-to-end row-runner integration is harder to unit-test (requires a mock `ChatServiceFactory`), but `load_skill_metrics` is the narrow seam that actually parses the new contract. Full behavior is validated by the skill-creator eval in Task δ.

### Step α.9: Verify + commit

```bash
cargo check --bin tengu
cargo test --bin tengu skill_lifecycle:: -- --nocapture 2>&1 | tail -5
cargo test --bin tengu eval_builder:: -- --nocapture 2>&1 | tail -5
```

All should pass. Commit:

```bash
git add src/adapters/eval_builder.rs
git commit -m "feat(eval): score rows against SKILL.md metrics: frontmatter + write metrics.json"
```

---

## Task β: Scratch-worktree helper

**Files:**
- Create: `src/adapters/skill_lifecycle/scratch_worktree.rs`
- Modify: `src/adapters/skill_lifecycle/mod.rs` (uncomment the module declaration)

### β.1: Create scratch_worktree.rs

Use the exact code in `docs/superpowers/plans/2026-04-20-skill-metrics-evolution.md` Task 14 Step 1 — the `Scratch { path, is_worktree }` type + `create_scratch` + `remove_scratch` + `copy_dir_recursive` helpers + 2 tests (`non_git_falls_back_to_scratch_and_copies_skill`, `git_repo_uses_worktree_path`). That code is reviewed and well-formed; no API mismatch with the live codebase.

### β.2: Uncomment submodule

In `src/adapters/skill_lifecycle/mod.rs`, change:
```
// pub(crate) mod scratch_worktree; // Task 14
```
to:
```
pub(crate) mod scratch_worktree;
```

### β.3: Tests + commit

```bash
cargo test --bin tengu skill_lifecycle::scratch_worktree -- --nocapture
```

Expected: 2 passed.

```bash
git add src/adapters/skill_lifecycle/scratch_worktree.rs src/adapters/skill_lifecycle/mod.rs
git commit -m "feat(skill-lifecycle): scratch worktree helper + non-git fallback"
```

---

## Task γ: Evolve loop + approval gate + CLI

**Files:**
- Create: `src/adapters/skill_lifecycle/evolve.rs`
- Create: `src/adapters/skill_lifecycle/approval_gate.rs`
- Modify: `src/adapters/skill_lifecycle/mod.rs`
- Modify: `src/main.rs`
- Modify: `Cargo.toml` (add `similar = "2"`)

### γ.1: Add `similar` dep

```toml
[dependencies]
similar = "2"
```

### γ.2: Approval gate

Use the exact code from 2026-04-20 plan Task 16 Step 1 (`approval_gate.rs` with `Decision` enum, `GateView`, `render`, `read_decision`, and 3 tests). Unchanged. Uncomment `pub(crate) mod approval_gate;` in subsystem `mod.rs`.

### γ.3: Evolve core types + pure helpers

Create `src/adapters/skill_lifecycle/evolve.rs`. Use the Baseline / CycleOutcome / pick_target_metric / pick_best / ImproverProposal / ProposalBody / apply_proposal_to_skill_md / count_body_lines / append_evolve_log helpers from the 2026-04-20 plan Task 15 Step 1 and Task 16 Step 3 — ALL verbatim EXCEPT for the following changes:

**Change 1:** Remove the `ImproverDispatch` trait and its `OrchestratorImproverDispatch` impl. Replace with a direct dependency on `Arc<dyn ChatServiceFactory>`. The `run_evolve` signature becomes:

```rust
use crate::adapters::orchestrator::wiring::ChatServiceFactory;
use std::sync::Arc;

pub struct EvolveArgs<'a> {
    pub config: &'a crate::adapters::config::Config,
    pub workspace: &'a std::path::Path,
    pub skill: &'a str,
    pub max_cycles: Option<u32>,
    pub target_metric: Option<String>,
    pub base_branch: Option<String>,
    pub chat_factory: Arc<dyn ChatServiceFactory>,
}

pub async fn run_evolve(args: EvolveArgs<'_>) -> anyhow::Result<()> { /* ... */ }
```

**Change 2:** Baseline + cycle scoring now call `eval_builder::run_skill` (not a hand-rolled `EvalRun`). The function call shape:

```rust
use crate::adapters::eval_builder;

// Discover the skill via eval_builder's helpers (reuse, don't re-implement):
let roots = eval_builder::default_skill_roots();
let skills = eval_builder::discover_skills(&[args.skill.to_string()], &roots)?;
let skill_under_test = skills.into_iter().next().ok_or_else(|| {
    anyhow::anyhow!("skill '{}' not found or has no evals/", args.skill)
})?;

// Build the judge engine once.
let judge = eval_builder::build_judge(/* model override */).map_err(...)?;

// Run baseline (discards the returned SkillReport — we read metrics.json that
// was written by finalize_run during run_skill).
let out_dir = args.workspace.join(".tengu/evolve-out");
std::fs::create_dir_all(&out_dir)?;
let _baseline_report = eval_builder::run_skill(
    &skill_under_test, judge.as_ref(), &out_dir,
    None, 1, false, None,
).await?;

// Now read the metrics.json the runner just wrote.
let mj: crate::adapters::skill_lifecycle::storage::MetricsJson = serde_json::from_slice(
    &std::fs::read(args.workspace.join("skills").join(args.skill).join("metrics.json"))?,
)?;
let baseline_rollups = mj.metrics;
```

If `eval_builder` does not currently expose `build_judge` as a public helper, extract the judge-engine construction from `eval_builder::run` (lines 54-64) into a new `pub fn build_judge(model: Option<String>) -> Result<Arc<dyn Engine>>` and call it from both sites.

**Change 3:** Cycle scoring targets the scratch workspace. The key fact: `eval_builder::discover_skills` uses `default_skill_roots()` which is rooted at CWD. To evaluate inside the scratch worktree, temporarily override CWD via `std::env::set_current_dir(&scratch.path)` (wrap in a drop-guard that restores on exit), OR pass the scratch path as the first entry of a custom `roots: Vec<PathBuf>` argument — the latter requires extending `discover_skills` to accept roots, or writing a small custom discovery inline. Pick whichever is less invasive once you see the call site.

**Change 4:** `call_skill_improver` in the 2026-04-20 plan took an `ImproverDispatch` parameter. Replace with `args.chat_factory.run_turn(&sl.improver_agent, &user_msg).await`. The parsing of `ImproverProposal` from the returned JSON string is unchanged.

### γ.4: CLI dispatch

In `src/main.rs`, add three new `Commands`:

```rust
/// Bounded rewrite→rescore loop for a skill, with user approval gate.
SkillEvolve {
    skill: String,
    #[arg(long)] max_cycles: Option<u32>,
    #[arg(long)] target_metric: Option<String>,
    #[arg(long)] base_branch: Option<String>,
    #[arg(long)] sandbox: Option<String>,
},
/// Inspect rolling metrics for a skill.
SkillMetrics {
    skill: String,
    #[arg(long, default_value_t = 10)] last: u32,
},
/// Apply a saved evolve proposal (reserved for future auto-trigger work).
SkillAcceptProposal { path: PathBuf },
```

Dispatch block (match arms inside `main`'s existing `match cli.command`):

```rust
Some(Commands::SkillEvolve { skill, max_cycles, target_metric, base_branch, sandbox }) => {
    let config = load_sandbox_or(sandbox, config)?;
    let workspace = resolve_workspace_from_config(&config)?;
    // Build a live ChatServiceFactory the same way channel_runtime does for orchestrator.
    // Reuse helpers (build_memory_manager + RuntimeChatServiceFactory) — do NOT re-implement.
    let chat_factory = adapters::channel_runtime::build_cli_chat_factory(&config, &workspace).await?;
    let args = adapters::skill_lifecycle::evolve::EvolveArgs {
        config: &config, workspace: &workspace, skill: &skill,
        max_cycles, target_metric, base_branch, chat_factory,
    };
    adapters::skill_lifecycle::evolve::run_evolve(args).await?;
    Ok(())
}
Some(Commands::SkillMetrics { skill, last }) => {
    let workspace = resolve_workspace_from_config(&config)?;
    adapters::skill_lifecycle::cli_metrics::render(&workspace, &skill, last)
}
Some(Commands::SkillAcceptProposal { path }) => {
    eprintln!("accept-proposal is a placeholder in v1. Proposals are applied inline during `tengu skill evolve`. Path ignored: {}", path.display());
    Ok(())
}
```

**Wiring note on `build_cli_chat_factory`:** If `channel_runtime` does not already expose a CLI-friendly factory builder, add a thin `pub(crate) async fn build_cli_chat_factory(config, workspace) -> Result<Arc<dyn ChatServiceFactory>>` that mirrors the Telegram/TUI wiring but targets a single-default-agent snapshot. Look at how `telegram_builder` or `chat_builder` builds its `RuntimeChatServiceFactory` and extract the reusable bits.

**Wiring note on `cli_metrics::render`:** Trivial helper — reads `skills/<name>/metrics.json` + tails `history.jsonl`, prints a table. ~30 LOC. Put it in a new `src/adapters/skill_lifecycle/cli_metrics.rs` module (uncommented in `mod.rs`).

### γ.5: Ctrl-C handler

Scratch worktrees must be cleaned on SIGINT. Install a handler before running evolve:

```rust
if matches!(cli.command, Some(Commands::SkillEvolve { .. })) {
    ctrlc::set_handler(move || {
        eprintln!("\nInterrupted. Worktrees under .tengu/worktrees/ may need manual removal.");
        std::process::exit(130);
    }).ok();
}
```

Add `ctrlc = "3"` to Cargo.toml. Worktree auto-cleanup on signal is deferred; evolve_log records state for forensics.

### γ.6: Tests

Put unit tests for `pick_target_metric`, `pick_best`, `apply_proposal_to_skill_md`, and the approval-gate render directly in their respective files. All should pass without external dependencies. The run_evolve happy path is integration-gated behind a `skill-lifecycle-integration` feature flag — stub the feature flag in Cargo.toml but leave the integration test body as a `// TODO: end-to-end` comment. v1 ships with unit coverage.

### γ.7: Verify + commit (one commit for the whole task)

```bash
cargo check --bin tengu
cargo test --bin tengu skill_lifecycle:: -- --nocapture 2>&1 | tail -5
```

```bash
git add src/adapters/skill_lifecycle src/main.rs Cargo.toml
git commit -m "feat(skill-lifecycle): tengu skill evolve closed loop + approval gate + CLI"
```

---

## Task δ: Dogfood

**Files:**
- Modify: `skills/skill-creator/SKILL.md`
- Create: `skills/skill-creator/evals/prompts.yaml`
- Create: `skills/skill-creator/metrics/distill_quality.md`
- Modify: `skills/skill-eval/SKILL.md`

### δ.1: skill-creator SKILL.md

Update the frontmatter to include a `metrics:` block and append a Distillation section. Use the exact content from 2026-04-20 plan Task 19 Step 1 (Distillation section) and Task 21 Step 1 (metrics block).

### δ.2: skill-creator/metrics/distill_quality.md

Use the exact rubric content from 2026-04-20 plan Task 21 Step 2.

### δ.3: skill-creator/evals/prompts.yaml

Use the exact fixtures content from 2026-04-20 plan Task 21 Step 3 (f1 mint-ipnft distillation, f2 pipeline-ingest distillation).

### δ.4: skill-eval SKILL.md

Use the exact rewrite content from 2026-04-20 plan Task 20 Step 1 — the "This skill is documentation-only" pointer to the CLI.

### δ.5: Smoke test the new eval config

```bash
cargo build --bin tengu
./target/debug/tengu eval skill-creator 2>&1 | tail -20
```

Expected: eval_builder discovers skill-creator, parses its frontmatter metrics, runs 2 fixtures, writes `skills/skill-creator/metrics.json`. Does not require an OPENROUTER_API_KEY to smoke the fixture-load path (you can `--judge-model some-stub` or set OPENROUTER_API_KEY to a dummy and accept that the judge call itself will 401 — the point of this step is to verify the parser + storage wiring, not the judge).

### δ.6: Commit

```bash
git add skills/skill-creator skills/skill-eval
git commit -m "feat(skills): skill-creator eval fixtures + distill_quality rubric + skill-eval CLI pointer"
```

---

## Post-merge follow-up (explicit non-goals for this plan)

1. **Populate `PluginToolExecutor.conversation` from the live engine.** Task 9 added the field; nothing writes to it in real dispatch. Needs interior mutability or a trait-level threading change. Small dedicated PR.
2. **`tool_assertion` live dispatch.** Task α step 4 sets `tools: None` in the `MetricRunCtx`. `tool_assertion` therefore returns "tool registry unavailable" for any skill that tries to use it. Wiring the live `ToolRegistry` into metric scoring needs a plan entry once a real skill declares a `tool_assertion` metric.
3. **Auto-trigger on metric threshold drop.** Designed in the spec §3.2 as explicit non-goal; manual `tengu skill evolve` is the only trigger.
4. **Evolve parallelism.** Cycles stay sequential.
5. **Architecture diagram.** Per the "don't salami-slice" feedback: produce a single-page Mermaid diagram of the full lifecycle (distill → eval → evolve → apply) AFTER δ lands.

---

## Self-review (done inline during plan authoring)

1. **Spec coverage:** All requirements from `docs/superpowers/specs/2026-04-20-skill-metrics-evolution-design.md` sections 5, 6, 7, 8 are addressed — §5 skill_distill (already shipped in Task 10), §6 metrics (Tasks 2-7 shipped + Task α integration), §7 evolve (Tasks β, γ), §8 integration points (Task α.4).
2. **Placeholder scan:** The `EvalJudgeClient::judge` body is explicitly flagged as requiring a wiring decision ("flag as DONE_WITH_CONCERNS if …"). This is intentional — the wiring depends on what `engine_builder` currently exposes, which the implementer can check in ~5 minutes.
3. **Consistency:** `MetricRunCtx` fields match Tasks 4 + 6 (`tools`, `judge`). `EvolveArgs.chat_factory: Arc<dyn ChatServiceFactory>` matches the real seam at `orchestrator/wiring.rs:30`.
4. **Scope:** 4 consolidated tasks, each a single coherent commit. Matches the "don't salami-slice" feedback.

---

## Execution handoff

Execute via **superpowers:subagent-driven-development**. Four consolidated tasks, one subagent per task. Each task is a single commit.
