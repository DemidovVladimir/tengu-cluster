# Skill Subsystem — Research & Plan (2026-04-28)

Tight reference. Tables over prose.

---

## Target use case — personalized learning agent

Concrete scenario driving every batch below:

1. Teacher seeds `german-teacher` skill with resources (books, YouTube, drills).
2. User runs `tengu chat --sandbox learner` and works with the agent.
3. User types `adjust yourself` periodically.
4. Agent reflects on the dialog → identifies weak topics (e.g., dative) → patches skill to focus there → optionally fetches new resources.
5. Skill drifts toward this user's actual needs across sessions.

| Implication | Gap | Where |
|---|---|---|
| Skills accumulate per-user state | G12 | Batch 9 |
| One verb combines evaluate + fix | G13 | Batch 9 (extends Batch 8) |
| Skills grow resources from web | G14 | Batch 9 |
| Teacher / learner roles diverge | G15 | Batch 9 |
| Long-horizon memory ("two weeks ago you missed X") | G16 | Existing `memory.cross_session_msg_top_k`; verify sufficient |

**MVP cut for the learning agent**:

| Tier | Items |
|---|---|
| Blocker | Batch 1 (config), G3 (auto-seed eval config), Batch 8 (in-chat verbs), G12 (per-user state), G13 (`adjust yourself`) |
| Nice | G14 (resource fetcher), Batch 4 (description-trigger eval), Batch 6 (variance) |
| Defer until 2nd learner | G15 (multi-tenancy), Batch 7 (readiness), Batch 5 (qualitative review) |

**Architecture decisions** (closed 2026-04-28):

| # | Decision |
|---|---|
| A1 | Per-user state → **sidecar JSON** at `skills/<name>/state/<learner>.json`. Body shared, state per-user, no reindex cost. |
| A2 | Resource fetcher → **new `resource-finder` agent** (TOML). `skill-improver-inline` dispatches it via the orchestrator. **Superseded 2026-04-29**: folded into the single `[agents.learning-agent]` block (`sandboxes/aura/config.toml`) — `docs/skill-redesign-2026-04-29.md`. |
| A3 | Teacher/learner permissions → **frontmatter flag** `editable_by_learner: bool`. Per-skill declarative. |

---

## Decisions (closed 2026-04-28)

| # | Question | Decision |
|---|---|---|
| 1 | Scope of `tengu skill remove` | Project tier only. `--tier {workspace,managed}` opts in. |
| 2 | `tengu skill install` source policy | Trust any URL. No allow-list. |
| 3 | `Script` / `ShellCheck` metric kinds | Keep, tighten guidance. No deprecation. |
| 4 | Auto-trigger evolve on metric drop | Stays a non-goal. |

**Tension from #2 + #3**: remote URL can ship arbitrary shell at eval time. Mitigation: scan + inform, never block by default. `--strict` opts into gating. Symlink-aware extraction is the one non-negotiable gate (filesystem-traversal RCE is out of scope of the user's risk acceptance).

---

## What exists today

| Capability | Status | Code |
|---|---|---|
| `skill_distill` LLM tool (the create-skill metaskill) | Shipped | `src/adapters/plugins/skill_lifecycle/distill.rs` |
| `tengu eval <skill>` | Shipped | `src/adapters/eval_builder.rs::run` |
| `tengu skill evolve <skill>` (bounded rewrite→rescore) | Shipped | `src/adapters/skill_lifecycle/evolve.rs` |
| `tengu skill metrics <skill>` | Shipped | `src/main.rs` (`SkillAction::Metrics`) |
| `tengu skill accept-proposal` | Placeholder | `src/main.rs` (`SkillAction::AcceptProposal`) |
| 6 `MetricKind` impls: `shell_check / llm_judge / tool_assertion / script / dialog_replay / description_trigger` | Shipped | `src/adapters/skill_lifecycle/metric_kinds/` |
| Rolling `metrics.json` + `metrics/history.jsonl` + per-run reports | Shipped | `src/adapters/skill_lifecycle/storage.rs` |
| Approval gate (diff + delta + y/n/d/o) | Shipped | `src/adapters/skill_lifecycle/approval_gate.rs` |
| Scratch git-worktree with stale-sweep | Shipped | `src/adapters/skill_lifecycle/scratch_worktree.rs` |
| Three-tier skill scanner (managed → workspace → project, first wins) | Shipped | `src/adapters/skill_builder.rs::skill_directories` (loader) · `src/adapters/plugins/view_skill/mod.rs` (in-chat) · `src/adapters/orchestrator/shared_files.rs::scan_skill_summaries` (planner registry) |
| Planner registry regenerated on every planner turn (no reindex step) | Shipped | `shared_files::ensure_planner_registry` |
| Cache-discipline invariant (`loaded_in_current_conversation: false`) | Shipped | `distill.rs:217` |
| `[skill_lifecycle]` config block enabled in a sandbox | Shipped | `sandboxes/aura/config.toml` (+ `[agents.skill-improver]`); commented sample in `config.example.toml` |
| `tengu skill remove / list / install / export / doctor / seed` | Shipped | `src/main.rs` (`SkillAction::*`) |
| Description-triggering eval | Shipped | `src/adapters/skill_lifecycle/metric_kinds/description_trigger.rs` |
| Human qualitative review | **Missing** | — |
| `evals/config.toml` seeded by `skill_distill` | Shipped (G3) | `src/adapters/plugins/skill_lifecycle/distill.rs` |
| Threat scanner on install / doctor | Shipped | `src/adapters/skill_lifecycle/scanner.rs` |
| In-chat lifecycle verbs → single `learning-agent` with `view_skill` / `manage_skill` | Shipped | `skills/orchestrator/SKILL.md` "Lifecycle verbs" · `[agents.learning-agent]` in `sandboxes/aura/config.toml` · `src/adapters/plugins/{view_skill,manage_skill}/` |
| Per-learner sidecar state (A1) | Shipped | `src/adapters/skill_lifecycle/learner_state.rs` |
| `editable_by_learner` frontmatter flag (A3) | Shipped | `skill_lifecycle/evolve.rs::is_editable_by_learner` |

---

## Gaps (priority order)

| # | Gap | Impact | Doctrine fit | Batch |
|---|---|---|---|---|
| G1 | `[skill_lifecycle]` not enabled in any sandbox — **closed** (`sandboxes/aura`) | Blocks `tengu skill evolve` end-to-end | D1 (TOML only) | Batch 1 |
| G2 | Phantom skill refs in `[agents.researcher].skill_packages` — **closed** | Cosmetic warns | D1 | Batch 1 |
| G3 | `skill_distill` doesn't seed `evals/config.toml` — **closed** | Distilled skills can't `tengu eval` | D2 | Batch 3 |
| G4 | No `tengu skill remove / list / install / export / doctor` — **closed** | Operator must `rm -rf` | D2 | Batch 2 |
| G5 | No description-triggering eval — **closed** (`description_trigger`) | Hardest-to-debug skill failure | D2 | Batch 4 |
| G6 | No human qualitative review | Subjective skills can't be gated | D2 | Batch 5 |
| G7 | No variance in `MetricRollup` (only pass_rate) — **closed** (`stddev/min/max`) | Hides flaky metrics | D2 | Batch 6 |
| G8 | Conditional skill activation not enforced at planner-selection | Skill chosen, then fails on prereq | D2 | Batch 7 |
| G9 | No in-chat trigger phrase for `skill_distill` ("create skill from our dialog") — **closed** (`learning-agent` + `manage_skill`) | Tool exists but only fires by LLM discretion | D1 (SKILL.md only) | Batch 8 |
| G10 | No in-chat trigger for evaluate-current-dialog ("evaluate") — **closed** | CLI-only — operator must drop to terminal mid-flow | D2 | Batch 8 |
| G11 | No reflective eval — `tengu eval` requires a pre-existing `evals/prompts.yaml`, can't use the current dialog — **closed** (`dialog_replay`) | Forces user to pre-author fixtures | D2 | Batch 8 |
| G12 | No per-user / per-learner skill state — **closed** (`learner_state.rs`) | Two learners using `german-teacher` overwrite each other's progress | D2 (sidecar) or D3 (clones) | Batch 9 |
| G13 | No single-verb `adjust yourself` (combines evaluate + fix + resource enrichment) — **closed** | User has to chain "evaluate" → "fix it" manually | D1 (planner SKILL.md) | Batch 9 |
| G14 | No resource enrichment — improver can't add YouTube links / book refs to a skill — **closed** (`learning-agent` calls `http_request` + `manage_skill(add_resource)`) | Learning skills go stale on resources | D2 | Batch 9 |
| G15 | No teacher / learner role distinction — **closed** (`editable_by_learner`) | Anyone can edit the skill in any sandbox | D1 (frontmatter flag) | Batch 9 |
| G16 | Long-horizon memory across sessions ("you struggled with dative 2 weeks ago") | Today `memory.cross_session_msg_top_k` is generic recall; doesn't index by topic | D2 | Batch 9 (verify scope first) |

D1 = pure TOML/SKILL.md edit. D2 = additive Rust edit. D3 = new abstraction (none in the plan).

---

## Reference comparison: hermes-agent vs Cowork skill-creator

> Both are complete shipped systems. Tengu is half-built, designed against both.

| Capability | Cowork skill-creator | Hermes skills_hub | Tengu today |
|---|---|---|---|
| Authoring loop | ✓ draft → eval → review → improve | — | partial (`skill_distill`) |
| Description-trigger eval (60/40 train/test) | ✓ `run_loop.py` | — | — |
| Quantitative metrics (mean/stddev/delta) | ✓ `benchmark.json` | — | partial (pass_rate only) |
| Qualitative review (browser viewer) | ✓ `generate_review.py` | — | — |
| Blind A/B comparator | ✓ `comparator.md` | — | — |
| Iteration history (`v0 → vN` with parent) | ✓ `history.json` | — | partial (`metrics/runs/<ts>/`) |
| Skill packaging | ✓ `package_skill.py` → `.skill` | partial (`publish` → GH PR) | — |
| Marketplace install | — | ✓ 7 hub registries | — |
| Threat scanner | — | ✓ 80+ patterns × 9 categories | — |
| Trust-matrix install policy | — | ✓ 4×3 matrix | — |
| Symlink-aware extraction | — | ✓ `Path.is_relative_to()` | — |
| Quarantine → scan → confirm | — | ✓ | — |
| Update with origin-hash detection | — | ✓ `.bundled_manifest` | — |
| Conditional activation | — | ✓ `SkillReadinessStatus` | partial (frontmatter exists, not enforced) |
| Persistent audit log | — | partial (no log) | — |
| Atomic writes | ✓ | ✓ | ✓ (`distill.rs:196`) |
| Three-tier loading (metadata → body → linked) | ✓ | ✓ | ✓ |
| Cache discipline | ✓ | ✓ | ✓ |
| Rolling pass rates | — | — | ✓ |
| Bounded auto-evolve loop | — | — | ✓ |
| LLM-proposes diff + approval gate | — | — | ✓ |
| `skill_distill` (in-conversation create) | — | — | ✓ |

**Honest split**: Cowork = authoring; Hermes = distribution + management. Tengu uniquely owns the deploy + measure + auto-evolve middle.

---

## Per-gap source mapping

| Tengu gap | Better reference | What to take | What to skip |
|---|---|---|---|
| `tengu skill remove` | Hermes `uninstall` | Walk + atomic delete + audit line | Cross-tier removal default |
| `tengu skill list / doctor` | Hermes `list / inspect / check / audit` | Walk format, per-skill verdict | Trust-level UI columns |
| `tengu skill install` pipeline | Hermes `install` | Quarantine → extract → validate → atomic move | 4×3 trust gate |
| `tengu skill export` bundle shape | Cowork `package_skill.py` | Tar.gz of `SKILL.md + evals/ + metrics/*.md` | Browser packaging UI |
| Auto-seed `evals/config.toml` | (principle, not code) | Derive from calling agent | — |
| Description-triggering eval | Cowork `run_loop.py` | 60/40 split, 3× per query, `best_description` from test score | Python implementation |
| Human qualitative review | Cowork `generate_review.py` | Open transcripts → freeform feedback → JSON read-back | Browser; tengu uses `$EDITOR` |
| Threat scanner | Hermes `skills_guard.py` | `ThreatPattern` struct + `safe/caution/dangerous` labels | 4×3 matrix, 80-pattern wholesale |
| Symlink-aware extract | Hermes | `Path::canonicalize` + prefix check | — |
| Variance in `MetricRollup` | Cowork `benchmark.json` | `{mean, stddev, min, max}` shape | — |
| Conditional activation | Hermes `SkillReadinessStatus` | Surface readiness to planner pre-selection | `fallback_for_toolsets` (no toolsets in tengu) |
| Blind A/B as `pick_best` tiebreaker | Cowork `comparator.md` | Comparator agent + rubric scores | — |

---

## Plan of attack

### Batch 1 — config polish (D1, <1h)

| # | Action | File |
|---|---|---|
| 1 | Activate `[skill_lifecycle]` + define `[agents.skill-improver]` (`fixture_runner_agent` is optional/unused) | `sandboxes/aura/config.toml` |
| 2 | Drop phantom `web-research` / `summarizer` from researcher | `[agents.researcher]` in `sandboxes/aura/config.toml` |
| 3 | Add this doc to "audit-for-staleness" list | `CLAUDE.md` |

### Batch 2 — CLI completeness (D2, ~1 day)

New modules first:

| Module | Purpose |
|---|---|
| `src/adapters/skill_lifecycle/scanner.rs` | `ThreatPattern { id, severity, category, regex, description }` + `Finding` + `ScanResult { verdict ∈ safe/caution/dangerous }`. ~15 patterns: shell-injection in `Script`/`ShellCheck` cmd strings, env-var exfil, `rm -rf $HOME`, `curl … \| sh`. No matrix, no policy. |
| `src/adapters/skill_lifecycle/audit.rs` | Append `{ts, op, name, verdict, source, sha256}` to `skills/.audit.jsonl` |

Then five subcommands:

| # | Command | Behaviour |
|---|---|---|
| 4 | `tengu skill remove <name> [--tier T] [--yes]` | Default project-tier. Refuse on active scratch worktree. Audit log. |
| 5 | `tengu skill list [--tier T]` | Walk three tiers, parse frontmatter, read `metrics.json`, print table. Mark skills with `Script`/`ShellCheck` metrics. |
| 6 | `tengu skill doctor` | Cross-check `[agents.*].skill_packages` of the active config vs filesystem. Report orphans, phantoms, missing rubric files. Exit non-zero on phantoms (`--no-fail` to suppress). Run scanner over installed skills (informational). |
| 7 | `tengu skill export <name> [--out <path>]` | Tar.gz of `SKILL.md + evals/prompts.yaml + metrics/*.md`. Include `*.sh` only if `Script` metrics declared. |
| 8 | `tengu skill install <source> [--tier T] [--strict] [--yes]` | Quarantine → symlink-aware extract → validate frontmatter → scan (always print findings) → optional `--strict` gate → atomic move → audit. |

### Batch 3 — distill ergonomics (D2, ~½ day)

| # | Action |
|---|---|
| 9 | Extend `SkillDistillTool::execute` to seed `evals/config.toml` from calling agent's config |
| 10 | Update `creates_skill_with_fixtures_and_scaffolds` test to assert seeded config parses |
| 11 | Document the seeded config in `skills/skill-creator/SKILL.md` |

### Batch 4 — description-triggering metric (D2, ~1 day)

Reference: Cowork `run_loop.py`.

| # | Action |
|---|---|
| 12 | New `MetricKind::DescriptionTrigger` reading `metrics/trigger_queries.yaml`, calling judge LLM 3× per query, scoring against `should_trigger` |
| 13 | Add variant to `MetricSpec` + `validate_metrics`. Update `metrics-evolution-design.md` as addendum. |
| 14 | Add `metrics/trigger_queries.yaml` to `skills/skill-creator/` (10 should-trigger + 10 near-miss should-not-trigger). Wire into frontmatter with `min_pass_rate: 0.85`. |

### Batch 5 — human review (deferred, ~½ day)

Reference: Cowork `generate_review.py + viewer.html`. Tengu uses `$EDITOR`.

| # | Action |
|---|---|
| 15 | `MetricKind::HumanReview` reading `metrics/reviews/<ts>.yaml` thumbs counts |
| 16 | `tengu skill review <name>` opens latest run's transcripts in `$EDITOR`, writes YAML |

### Batch 6 — measurement upgrades (deferred, ~1 day)

Surfaced by §"reference comparison".

| # | Action |
|---|---|
| 17 | Extend `MetricRollup` with `stddev / min / max: Option<f32>`. Backwards-compat (Option). |
| 18 | Add `parent_cycle_n: Option<u32>` to `CycleOutcome`. Trivial. |
| 19 | `MetricKind::AbComparison` as a `pick_best` tiebreaker when target+gated tie. |

### Batch 7 — conditional activation (deferred, ~½ day)

Reference: Hermes `SkillReadinessStatus`.

| # | Action |
|---|---|
| 20 | `compute_readiness(spec) → Readiness` helper read at registry-index time. Emit as registry roster tag. Planner SKILL.md gains "skip `SetupNeeded` skills unless explicit" rule. |

### Batch 8 — in-chat skill lifecycle verbs (high impact, ~1 day)

Goal: user types `create skill from our dialog` or `evaluate` in chat; harness routes to the right action. No CLI hop.

**Trigger model**: the planner SKILL.md gains a "lifecycle verbs" block that maps user phrases to plan steps. Phrases stay loose (LLM judgement); the *steps* are tight.

| # | Action |
|---|---|
| 21 | Extend `skills/orchestrator/SKILL.md` with a "Lifecycle verbs" section: phrases like *"create / save / distill skill from this dialog"*, *"evaluate this skill / dialog"*, *"improve / fix the skill we just used"* map to specific plan shapes. |
| 22 | Plan shape: `{kind: "plan", steps: [{agent: "skill-author", goal: "distill skill from messages [N..current]"}]}`. New agent `skill-author` (`[agents.skill-author]` block) gets `skill_distill` in `tools` + identity instructions to call `skill_distill` with `from_message_index = N`. Already-built tool, new agent wrapper. |
| 23 | Reflective eval: new `MetricKind::DialogReplay { against_message_index: usize }` that uses the calling conversation slice as a single fixture, scores it through existing `llm_judge` / `tool_assertion` kinds. Lives in `metric_kinds/dialog_replay.rs`. |
| 24 | New agent `skill-evaluator` (`[agents.skill-evaluator]` block): given a skill name + a message-index range, runs the existing `eval_builder::run_skill` against an ephemeral `prompts.yaml` derived from the dialog slice, returns a structured pass/fail summary. |
| 25 | Plan shape for `evaluate`: `{steps: [{agent: "skill-evaluator", goal: "evaluate <skill> against messages [N..current]"}]}`. Output: pass/fail per metric, what went wrong, suggested fix. No file written unless the user follows up with *"apply the fix"*. |
| 26 | "Apply the fix" plan shape: dispatches to existing `tengu skill evolve` driver via a new `skill-improver-inline` agent that takes the eval output as input and emits an `ImproverProposal` on the spot. Reuses `apply_proposal_to_skill_md`. |

**Phrase → plan flow**:

| User says | Plan kind | Agent | Tool/op |
|---|---|---|---|
| "create skill from our dialog [as <name>]" | plan | `skill-author` | `skill_distill` |
| "evaluate" / "evaluate this skill" / "evaluate our dialog" | plan | `skill-evaluator` | `MetricKind::DialogReplay` over current slice |
| "fix it" / "apply the fix" / "improve the skill" | plan | `skill-improver-inline` | `apply_proposal_to_skill_md` on the named skill |
| "rollback" | direct | — | `git checkout skills/<name>/SKILL.md` (one shell-out) |

**Shipped shape (2026-04-29)**: rows 22–26 collapsed into ONE `[agents.learning-agent]` block in `sandboxes/aura/config.toml` (`view_skill` + `manage_skill`); phrase → plan table lives in `skills/orchestrator/SKILL.md` "Lifecycle verbs" — `docs/skill-redesign-2026-04-29.md`.

**Cache discipline still holds**: distilled / improved skills don't activate in the current conversation. The user gets a confirmation message ("Saved to `skills/<name>/`. Available next session.") not a hot-swap.

**Why this isn't just a slash-command**: a slash-command (`/skill create …`) would bypass the planner. Routing through the orchestrator means the planner can disambiguate ("which skill?", "which message range?") and refuse when the dialog is too short / not a workflow.

### Batch 9 — learning-platform pieces (~2 days)

| # | Action | File / shape |
|---|---|---|
| 27 | **Sidecar state writer/reader** (per A1). Schema: `{learner_id, topics_covered: [...], topics_weak: [...], mastery_scores: {topic: 0..1}, last_session_ts}`. Module: `skill_lifecycle/learner_state.rs`. Atomic write via temp + rename, same as `distill.rs:196`. | `skills/<name>/state/<learner>.json` |
| 28 | **`resource-finder` agent** (per A2). New `[agents.resource-finder]` block with `tools = ["http_request"]`. Identity: "Given a topic + learner gap description, find 2–5 high-quality web resources, return JSON `[{url, title, why_relevant, length}]`". Superseded — `learning-agent` calls `http_request` itself. | `[agents.learning-agent]` in `sandboxes/aura/config.toml` |
| 29 | **Frontmatter flags** (per A3). Extend SKILL.md schema with `editable_by_learner: bool` (default `false`). `skill_lifecycle/evolve.rs` checks the flag; refuses to apply diffs from learner-mode chats unless set. | `skills/skill-creator/SKILL.md` doc + `skill_lifecycle/metrics.rs` parser |
| 30 | **`adjust yourself` planner block.** Extend `skills/orchestrator/SKILL.md` with the trigger. Plan shape: 3 sequential steps — `skill-evaluator` (DialogReplay + load state) → `resource-finder` (only if eval reports gaps) → `skill-improver-inline` (proposal updates SKILL.md + state JSON + cites new resources). One approval gate at the end. | `skills/orchestrator/SKILL.md` |
| 31 | **`tengu skill seed <name> <resources_dir>`** CLI for teacher onboarding. Drops `SKILL.md` template + populates `skills/<name>/resources/` from a directory. Atomic. | `Commands::SkillSeed` in `main.rs` |
| 32 | **Verify long-horizon memory (G16)**. Smoke test: 3-session learner, dative weakness in session 1, generic chat in session 2, `adjust yourself` in session 3 must surface dative weakness via `memory.cross_session_msg_top_k`. If insufficient → add `MemoryKind::TopicMastery` (D2, half-day add). | Punt the new MemoryKind unless smoke fails. |

**Walkthrough — concrete flow for the German teacher use case**:

| Turn | User | System |
|---|---|---|
| Setup | Teacher: `tengu skill seed german-teacher ./materials/` | Skill `german-teacher` written with `resources/` populated |
| Day 1 | Learner: opens `tengu chat --sandbox learner`, picks German agent, drills | Conversation accumulates in `agentic_memory user events` with `session_id` |
| Day 1 end | Learner: `adjust yourself` | Plan: `skill-evaluator` → `skill-improver-inline`. Eval finds: "weak on dative". Proposal: SKILL.md adds dative focus, `state/<learner>.json` records weak topic. Approval gate. User accepts. |
| Day 2 | Learner: opens chat, agent (using updated skill) starts on dative drills | Skill body reflects Day 1's learnings |
| Week 1 | Learner: `adjust yourself` | Eval reads `state/<learner>.json`, finds dative now passing, proposes moving to genitive + fetches 2 new YouTube links into `resources/genitive.md` |

---

## Test strategy

### Unit tests per gap

| Item | Test |
|---|---|
| `tengu skill remove` | `removes_project_tier_by_default`, `removes_managed_when_flag_set`, `refuses_with_active_evolve_worktree` |
| `tengu skill list` | `lists_present_skills_with_metrics_health`, `marks_script_or_shell_check_metrics`, `handles_metrics_json_absent` |
| `tengu skill doctor` | `detects_phantom_refs_in_skill_packages`, `detects_orphan_skills`, `detects_missing_rubric_files`, `reports_script_metrics_informational` |
| `tengu skill install` | `validates_skill_md_after_extract`, `rejects_symlink_escape`, `prints_findings_always`, `gates_only_with_strict`, `commits_with_audit_line` |
| `tengu skill export` | `bundles_skill_md_evals_metric_rubrics`, `omits_metrics_runs_history`, `includes_sh_when_script_metrics_declared` |
| `evals/config.toml` seeding | `creates_runnable_eval_config_after_distill` |
| `MetricKind::DescriptionTrigger` | `passes_when_judge_agrees_should_trigger`, `fails_when_judge_disagrees`, `stable_across_3_runs` |
| `MetricKind::HumanReview` | `pass_rate_from_thumbs_count` |
| `MetricRollup` variance | `compute_rollups_emits_stddev_when_window_n_gt_1` |
| `MetricKind::DialogReplay` | `scores_current_slice_via_llm_judge`, `scores_current_slice_via_tool_assertion`, `rejects_when_slice_empty` |
| Lifecycle-verb routing | `planner_routes_create_skill_phrase_to_skill_author`, `planner_routes_evaluate_to_skill_evaluator`, `planner_refuses_create_when_dialog_lt_2_messages` |

### Manual smoke (add to `docs/manual-test-checklist.md`)

```
SKL-1  list shows all 11 in-tree skills, marks Script/ShellCheck
SKL-2  remove eth-balance-check cleans project-tier; managed/workspace untouched without --tier
SKL-3  doctor flags `[agents.*].skill_packages` phantoms in the active config (exit non-zero); reports script metrics (informational)
SKL-4  install <local tarball>; default proceeds with findings printed; --strict refuses
SKL-5  install of tarball with `rm -rf $HOME` Script metric prints literal cmd in finding
SKL-6  export skill-creator | tar tz lists SKILL.md + evals/prompts.yaml + metrics/distill_quality.md, NOT metrics/runs/, NOT metrics.json
SKL-7  distilled skill has runnable evals/config.toml — tengu eval <new-skill> works without manual edits
SKL-8  trigger-eval gates on a should-not-trigger query
SKL-9  in chat: "create skill from our dialog as foo" → planner dispatches skill-author → skill_distill writes skills/foo/. Confirmation message in chat.
SKL-10 in chat: "evaluate" after using skill foo → skill-evaluator runs DialogReplay → returns per-metric pass/fail + what went wrong
SKL-11 in chat: "fix it" after evaluate → skill-improver-inline emits proposal → approval gate → SKILL.md updated. "rollback" reverts via git checkout.
SKL-12 teacher: tengu skill seed german-teacher ./materials/ → skill written with resources/ populated.
SKL-13 learner: chat session, "adjust yourself" → eval+improver+resource-fetcher run sequentially. Approval gate shows body diff + state JSON diff + new resources/<topic>.md.
SKL-14 second learner against same skill: state/<learner-2>.json is independent of state/<learner-1>.json. No cross-talk.
```

### Regression nets

Any change to `skill_lifecycle/`:

```
cargo test --bin tengu skill_lifecycle::
tengu eval skill-creator
tengu eval orchestration-e2e
```

If `pick_best` / `pick_target_metric` / `apply_proposal_to_skill_md` change, also:

```
tengu skill evolve skill-creator --max-cycles 1   # inspect the gate render
```

---

## Doctrine notes

| Concern | Note |
|---|---|
| No-scripts policy | `Script` + `ShellCheck` metric kinds keep their shell-out for now (decision #3). Cowork's Python pipeline scripts (`run_loop.py` etc.) are NOT ported — patterns re-expressed as Rust metric kinds. |
| TOML/SKILL.md preferred over Rust | All new metric kinds extend the existing `MetricSpec` enum + frontmatter. Configuration via TOML where possible. |
| Cache discipline | New skills don't hot-load. `skill_distill` / `manage_skill` return `loaded_in_current_conversation: false`. Planner registry regenerated on the next planner turn. |
| Composition over wholesale | Per-skill `metrics:` frontmatter. Per-cycle proposals replace whole body (`evolve.rs:115`), not surgical patches. |
| Fail-soft vs hard | Memory ops fail-soft. Plan-shape errors hard. Scanner findings printed, not gating (decision #2). |

---

## Code pointers

| Concern | File |
|---|---|
| `skill_distill` tool | `src/adapters/plugins/skill_lifecycle/distill.rs` |
| Plugin registration | `src/adapters/channel_runtime.rs` (`register_core_plugins`, `WORKSPACE_TOOLS_ALLOWLIST`) |
| Metric types + dispatch | `src/adapters/skill_lifecycle/metrics.rs` |
| Metric kinds | `src/adapters/skill_lifecycle/metric_kinds/{shell_check,llm_judge,tool_assertion,script,dialog_replay,description_trigger}.rs` |
| Rolling storage + retention | `src/adapters/skill_lifecycle/storage.rs` |
| Fixture YAML + extraction | `src/adapters/skill_lifecycle/fixtures.rs` |
| Eval runner | `src/adapters/eval_builder.rs` |
| Evolve driver | `src/adapters/skill_lifecycle/evolve.rs` |
| Approval gate | `src/adapters/skill_lifecycle/approval_gate.rs` |
| Scratch worktree | `src/adapters/skill_lifecycle/scratch_worktree.rs` |
| CLI dispatch | `src/main.rs` (`Commands::Eval`, `Commands::Skill { SkillAction::Evolve \| Metrics \| AcceptProposal \| Remove \| List \| Doctor \| Export \| Install \| Seed }`) |
| Threat scanner | `src/adapters/skill_lifecycle/scanner.rs` |
| Audit log (`skills/.audit.jsonl`) | `src/adapters/skill_lifecycle/audit.rs` |
| Per-learner sidecar state | `src/adapters/skill_lifecycle/learner_state.rs` |
| In-chat read / write tools | `src/adapters/plugins/view_skill/mod.rs` · `src/adapters/plugins/manage_skill/mod.rs` |
| Skill scan for the planner registry | `src/adapters/orchestrator/shared_files.rs::scan_skill_summaries` |
| In-process three-tier loader (`skill_packages`) | `src/adapters/skill_builder.rs::skill_directories` |
| Cowork reference | `/var/folders/.../skills/skill-creator/{SKILL.md,scripts/,agents/,eval-viewer/}` |
| Hermes reference | `/Users/vladimirdemidov/development/hermes-agent/{hermes_cli,tools}/skills_hub.py` + `tools/skills_guard.py` |
| Design spec | `docs/superpowers/specs/2026-04-20-skill-metrics-evolution-design.md` |
| Pipeline diagrams | `docs/skill-lifecycle-pipeline-diagrams.md` |
