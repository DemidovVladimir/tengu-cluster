# Archive

Dated plans, PRDs, reports and explorers whose work shipped. Read one for history only — the living docs are listed in `docs/index.html`; a file here is never updated. Rule (`CLAUDE.md` / `AGENTS.md` Output style, "Doc names"): a dated doc moves to `docs/archive/<yyyy-mm>/` (the month in its name, else the month it was added) once its work ships; a doc a lineage record cites never moves (`xlab-2026-10-01.md`, `soe-2026-10-08.md`, `xmarket-prd-2026-09-29.md`, `xmarket-feasibility-2026-09-30.md`, `forward-evidence-runbook-2026-10-08.md`, `p7`–`p10` — the P-series keeps `p6` beside them). Living docs were renamed undated on 2026-10-24 (e.g. `runtime-2026-09-30.md` → `runtime.md`); files here keep the old names in their links.

| File | What it was | Why archived |
|---|---|---|
| `2026-03/architecture.md` | the first architecture overview (harness-owned orchestration, PR #6–#8 doctrine) | superseded by docs/architecture.md (was architecture-2026-04-27.md) and the code map |
| `2026-03/claude-code-backend-plan.md` | plan for the Claude Code engine | shipped (engine `claude_code`); docs/engine-backends.md is current |
| `2026-03/claude-code-backend-prd.md` | PRD for the Claude Code engine | shipped; docs/engine-backends.md is current |
| `2026-04/context-cutting-flow-2026-04-27.html` | inner tool-loop deep-dive explorer | duplicate of docs/tutorial/tool-loop.html (O3) |
| `2026-04/context-cutting-flow-2026-04-27.svg` | picture of the same | companion of the archived explorer |
| `2026-04/context-management-2026-04-27.html` | interactive explorer of the context-management doc | hand-synced duplicate of the .md (O3) |
| `2026-04/harness-architecture.md` | harness-owned orchestration + memory architecture (PR #6–#8) | superseded by docs/architecture-2026-04-27.md and docs/code-map.md |
| `2026-04/IMPLEMENTATION_PLAN.md` | phase-by-phase roadmap of the v2 redesign (phases 0–7.2) and what shipped after | every phase shipped; open work lives in docs/SESSION_HANDOFF.md |
| `2026-04/manual-test-checklist.md` | manual smoke test run at the end of each phase | the phases are done; tests + the engine matrix cover it |
| `2026-04/orchestration-test-scenarios.md` | first live smoke scenarios of the orchestration (PRs #6–#10) | report of a finished run; it tells an agent to spend on OpenRouter (breaks the Model-spend rule) |
| `2026-04/orchestrator-pipeline.html` | interactive explorer of the same pipeline | duplicate of the tutorial (O3) |
| `2026-04/orchestrator-pipeline.md` | orchestration diagrams with file:line references | its line citations rotted; docs/architecture-2026-04-27.md + docs/tutorial/planner.html are current |
| `2026-04/skill-lifecycle-pipeline-diagrams.md` | skill distil / eval / evolve diagrams with file:line references | docs/skills.md + docs/tutorial/skill-lifecycle.html are current |
| `2026-04/skill-redesign-2026-04-29.md` | skill subsystem redesign reference | shipped; docs/skills.md is current |
| `2026-04/superpowers/` (9 files) | specs + plans of phase A (tool plugins), harness orchestration + memory, skill metrics + evolution, the first simplification PR | shipped; live rules in `docs/skills.md`, `docs/tools.md`, `docs/architecture-2026-04-27.md` |
| `2026-04/tengu-analysis.html` | interactive analysis page | duplicate of the tutorial and the code map (O3) |
| `2026-04/validation-checklist.md` | acceptance checklist for PRs #6–#13 | report of a finished check; same OpenRouter spend as above |
| `2026-05/feature-parity-radar-2026-05-08.html` | feature-parity radar chart vs Hermes / PI | docs/comparison-2026-04-26.md is current (O3) |
| `2026-05/memory-recall-debug-2026-05-11.html` | recall debugging explorer | duplicate of docs/tutorial/memory.html (O3) |
| `2026-09/hexagonal-plan-2026-09-23.md` | plan of the hexagonal rewrite (phases 0–6) | shipped; tests/layering_lint.rs + docs/code-map.md are current |
| `2026-09/xmarket-build-plan-2026-09-30.md` | how to execute the xmarket backlog: waves, gates, kickoff prompt | W1 shipped |
| `2026-09/xmarket-gaps-2026-09-29.md` | xmarket backlog: 185 items, milestones E0, M0–M8 | W1 shipped; the tracker docs/xmarket-tracker-2026-09-29.md keeps the state |
| `2026-10/changes-2026-09-29-to-10-02.html` | change log page for 2026-09-29 → 10-02 | report |
| `2026-10/code-findings-2026-10-07.md` | code findings sweep | report |
| `2026-10/control-loop-lab-baseline-2026-10-08.md` | control-loop-lab baseline run | report; the runbook docs/control-loop-lab-2026-10-08.md is current |
| `2026-10/studio-acceptance-2026-10-08.md` | Studio Play / Stop live acceptance (ST-30 / ST-31) | report (passed 2026-10-09) |
| `2026-10/studio-clean-room-2026-10-09.md` | Studio ST-90 clean-room run | report (passed 2026-10-09) |
| `2026-10/studio-editor-design-2026-10-08.md` | Studio editor design (ST-40) | built as the builder (docs/builder.md) |
| `2026-10/studio-trace-evidence-2026-10-08.md` | Studio trace evidence (ST-12 / Gate 2) | report |
| `2026-10/w1-inventory-2026-10-06.md` | W1 inventory for Operator Review #1 | report; the review is done (docs/w1-review-2026-10-06.md) |
| `2026-10/w1-p0-weekend-2026-10-06.md` | W1 P0 weekend report | report |
| `2026-10/w1-review-2026-10-06.md` | Operator Review #1 packet + the operator's verdict (APPROVE, 2026-10-08) | decision made; P6–P11 done |
| `2026-10/xlab-2026-10-02.html` | visual explainer of xlab as of 2026-10-02 | snapshot; docs/xlab-2026-10-01.md + the tutorial are current |
| `2026-10/xmarket-2026-10-02.html` | visual explainer of the xmarket desk as of 2026-10-02 | snapshot; the tracker + the tutorial are current |
