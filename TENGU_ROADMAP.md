# Tengu Implementation Roadmap — W1 → W2 → Evolution

**Repository:** `DemidovVladimir/tengu-cluster`  
**Purpose:** Operational execution plan for coding agents  
**Companion document:** `TENGU_HANDOFF.md`  
**Start condition:** The current weekend experiment has completed and its raw evidence has been preserved.  
**Default rule:** Never continue to the next gated phase merely because the previous phase produced code. Continue only after its validation gate passes.  
**Status (2026-10-08):** P0–P5 done (`docs/lineage-2026-10-06.md`, W1 frozen) · Operator Review #1 = APPROVE (`docs/w1-review-2026-10-06.md` § Verdict) · P6–P11 done (`docs/p{6,7,8,9,10}-*-2026-10-08.md`): no change beats rule W, no W2 candidate (G11 FAIL for stop 300), W1 kept · P12 / P13 not started; Operator Review #2 not reached · forward rule W evidence continues each weekend (weekend #2 sealed: `lineage/experiments/fwd.rule_w.2026-10-09.toml`, `docs/forward-evidence-runbook-2026-10-08.md`; M3 tally 1 / 12).

## 1. Agent Operating Rules

1. Read `TENGU_HANDOFF.md` and this file completely.
2. Inspect the actual repository before changing code.
3. Treat working code/tests as implementation reality and the handoff as current product intent.
4. Prefer `REUSE → EXTEND → NEW`; do not rebuild working X Market/X Lab functionality.
5. Preserve original experiment artifacts and point-in-time integrity.
6. Never silently repair, rewrite, or reinterpret historical evidence.
7. Never bypass deterministic risk or authorize new live-money execution.
8. Represent missing facts as `UNKNOWN` / `UNRESOLVED`; do not fabricate lineage.
9. Establish a recoverable repository checkpoint before persistent/schema changes.
10. Stop at every mandatory operator gate.

A sufficient agent prompt is:

> Read `TENGU_HANDOFF.md` and `TENGU_ROADMAP.md`. Follow the roadmap from the first eligible phase. Respect every validation and STOP condition. Do not skip gates.

## 2. Global Invariants

- **Evidence immutability:** migrations may reference/normalize old evidence but original evidence remains unchanged.
- **Point-in-time integrity:** future prices, news, labels, revisions, or outcomes cannot affect earlier decisions.
- **Risk boundary:** Architect, JEV, Experience, Evolution, and migration code cannot bypass deterministic risk.
- **Paper/live separation:** this roadmap does not authorize new real-money execution.
- **Generation isolation:** after W1 freeze, W2 work cannot silently change W1.
- **Failure is evidence:** failed strategies, rejected hypotheses, JEV/Architect failures, incidents, and NO ACTION remain queryable.
- **Smallest safe change:** material correctness fixes that alter experiment semantics require review before application.

## 3. Phase State Machine

```text
CURRENT WEEKEND EXPERIMENT
        |
        | HOLD — no roadmap implementation
        v
EXPERIMENT COMPLETES
        |
        v
P0 Preserve + Grade Forward Evidence
        | G0
        v
P1 Inventory + Verify + Freeze W1
        | G1
        v
P2 Experiment Registry
        | G2
        v
P3 Variant Registry
        | G3
        v
P4 Experience Foundation
        | G4
        v
P5 Capability Registry + Generation Isolation
        | G5
        v
========== OPERATOR REVIEW #1 ==========
        |
        | APPROVE
        v
P6 Decision Evaluation Foundation
        | G6
        +-------------------------+
        v                         v
P7 News/Information         P8 HIP-3 Oracle/
Research                    Microstructure Research
        | G7                      | G8
        +------------+------------+
                     v
P9 Cost/Liquidity Research
        | G9
        v
P10 W2 Candidate
        | G10
        v
P11 Historical + Holdout Evaluation
        |
   FAIL/INCONCLUSIVE -----> MODIFY / REJECT / MORE DATA
        |
       PASS
        v
P12 W2 Forward Paper Experiment
        | G12
        v
P13 W1 vs W2 Forward Review
        |
   REJECT / MODIFY / CONTINUE / PROMOTE
        v
========== OPERATOR REVIEW #2 ==========
        |
        | APPROVE
        v
P14 Experience Retrieval
        | G14
        v
P15 Decision Map Experiment
        | G15
        v
P16 Evolution Prototype
        | G16
        v
FUTURE GENERATION CYCLES
```

# 4. HOLD Until the Current Weekend Experiment Ends

**Do not start Phase 0–5 before the current experiment reaches its planned terminal point.**

Allowed: normal experiment operation, existing monitoring/recording, and genuinely necessary safety/correctness intervention.

Not allowed: refactoring X Market/X Lab, changing Rule W, candidate selection, risk limits, fill behavior, or retrofitting architecture into active decisions.

**Exit condition:** experiment terminal point reached and raw evidence required for grading safely retained.

# PHASE 0 — Preserve and Grade the Weekend Experiment

## Goal
Convert the completed run into immutable forward evidence before architecture work.

## Prerequisites
- experiment finished;
- no remaining experiment action depends on old runtime state;
- observations/logs/ledger/history accessible.

## Tasks
- Preserve anchors, candidates, books, contexts, risk verdicts, paper orders/fills, positions, funding, exits, shadow ledger, runtime/network/feed incidents, and reconstruction logs.
- Classify relevant evidence as `LIVE_RECORDED`, `BACKFILLED`, `MISSING`, or `NOT_APPLICABLE`.
- Backfill only where justified; retain source, backfill time, missing interval, and uncertainty.
- Grade selected positions, rejected candidates, shadow candidates, gross result, costs, funding, simulated slippage, net result, exits/stops, and relevant liquidity.
- Report development expectation, holdout expectation, and forward result separately.
- Preserve infrastructure incidents separately from strategy performance.

## Gate G0
PASS only if raw evidence is preserved, original rules remain unchanged, live/backfilled evidence is distinguishable, forward/historical evidence is separated, incidents are identifiable, and no historical output was overwritten.

## Failure path
If critical evidence cannot be reconstructed, accounting fails to reconcile, execution semantics are ambiguous, a material bug affected the run, or future information contaminated a decision: **STOP**. Do not “fix the result.” Produce a correctness report and classify the experiment as `VALID`, `VALID_WITH_LIMITATIONS`, `INVALID_FOR_STRATEGY_INFERENCE`, or `UNRESOLVED`.

## Output
`P0 Weekend Evidence & Grading Report`

# PHASE 1 — Inventory, Verify, Freeze W1

## Goal
Define W1 from actual code rather than assumptions.

## Prerequisite
G0 PASS, or operator acceptance of `VALID_WITH_LIMITATIONS`.

## Tasks
- Inventory runtime, X Market, weekend sandbox, X Lab, feeds, tools, strategy specs, stores, ledgers, risk, Architect, JEV, tests, and evidence directories.
- Map handoff requirements to `REUSE / EXTEND / NEW / DEFER / REMOVE`; explain every `NEW`.
- Execute verification for deterministic backtest, point-in-time behavior, typed strategy validation, holdout controls, JEV replay/cache behavior, paper accounting, risk-gate placement, error behavior, and regressions.
- Record known defects.
- Create a machine-readable W1 manifest referencing relevant capability/spec/policy/model/risk/schema/config versions.
- Freeze W1 after approved correctness fixes.

## Gate G1
PASS only if inventory/mapping exist, claimed invariants are tested or explicitly unresolved, W1 is machine-identifiable/reproducible within existing guarantees, and known defects are documented.

If freeze changes existing behavior unexpectedly: revert the freeze-related behavior change, preserve the discrepancy, diagnose, and rerun G1.

# PHASE 2 — Experiment Registry

## Goal
Make experiments structured/queryable without replacing original artifacts.

## Tasks
Represent at minimum: experiment ID, hypothesis, generation, creation time, strategy/spec reference, data/evidence windows, evidence class, cost model, decision policy, result references, uncertainty where known, and verdict.

Import important existing evidence where available, including weekend fade, liquid/top-4 variants, weekend follow, xyz overnight/funding tests, crypto move/funding tests, SOL/ETH spread, Saturday/weekday work, and the completed weekend forward experiment.

Prefer references to existing artifacts. Preserve no-go results.

## Gate G2
Select one surviving experiment, two rejected experiments, and the forward experiment. Each must reconstruct `hypothesis → configuration → evidence → result → verdict`. Missing historical metadata may be `UNKNOWN`; never infer it.

# PHASE 3 — Variant Registry

## Goal
Expose strategy search/variation and reduce hidden multiple-testing bias.

## Tasks
Represent family/parent, changed dimensions, reason where known, ordering/time where known, associated experiments, result, and verdict. Reconstruct Rule W families including all-names/liquid/top-4, Saturday/Sunday entry, stops/liquidity variants where evidence supports them. Provide preregistration for future variants.

## Gate G3
PASS if Rule W has navigable variant lineage, at least one rejected family is represented, future variants can be registered before outcome, and unknown historical counts/order remain explicit.

# PHASE 4 — Experience Foundation

## Goal
Create reusable evidence episodes without equating learning with P&L.

## Tasks
Experience Episode must represent/reference: context, hypothesis, information available at the time, alternatives, decision, action, outcome, decision quality, execution quality, lesson/status, experiment, generation, and evidence/source references.

Build:
1. Rule W end-to-end reference episode.
2. At least one rejected-strategy episode.
3. At least one operational/data-incident episode.

The representation must support profitable bad decisions and losing good decisions.

## Gate G4
Structured lineage queries must reconstruct:
- Rule W end-to-end;
- a failed idea and why it was rejected;
- an operational incident distinct from strategy failure.

# PHASE 5 — Capability Registry and Generation Isolation

## Goal
Identify decision/evolution-relevant capabilities and prove W1 cannot be silently altered by W2.

## Tasks
Classify relevant capabilities as `INTELLIGENCE`, `CAPITAL`, `RISK`, or `INFRASTRUCTURE`. Store identity/version/typed-contract reference/permission class/generation availability/lifecycle state where appropriate. Do not treat parameter variants as new capabilities automatically.

Link capabilities to W1. Introduce or simulate a W2-only capability/configuration and verify isolation.

## Gate G5
PASS only if the registry is minimal/meaningful, W1 capability set is queryable, W1 manifest/replay remain unchanged, W1 cannot access W2-only capability, and risk permissions remain intact.

# OPERATOR REVIEW #1 — MANDATORY STOP

Do not proceed to Phase 6 automatically.

## Required package
1. Weekend Evidence & Grading Report
2. Architecture Inventory
3. `REUSE / EXTEND / NEW / DEFER / REMOVE` Gap Analysis
4. Verification Report
5. W1 Manifest
6. Experiment Registry
7. Variant Registry
8. Experience representation
9. Rule W lineage
10. Rejected-strategy lineage
11. Operational-incident lineage
12. Capability Registry
13. Generation-isolation result
14. Unresolved questions
15. Proposed Phase 6–10 implementation changes

## Review questions
- Can W1 be reproduced?
- Can Rule W lineage be reconstructed without scattered manual report reading?
- Are failed hypotheses retained?
- Is forward evidence separate from historical evidence?
- Are live/backfilled data distinguishable?
- Was X Market/X Lab reused rather than duplicated?
- Is JEV still represented as unproven?
- Is deterministic risk unchanged?
- Can W2 be developed without mutating W1?

Outcomes: `APPROVE`, `APPROVE_WITH_FIXES`, `REWORK`, `ABORT_DIRECTION`.

# PHASE 6 — Decision Evaluation Foundation

**Start only after Operator Review #1 = APPROVE.**

Formalize comparable candidate-level evidence for deterministic decision, JEV decision/confidence, HOLD where meaningful, eventual outcome, and valid counterfactuals. Measure selection rate, net economics vs deterministic baseline, calibration/Brier where applicable, uncertainty, latency, and model/research cost.

## Gate G6
PASS only if the same candidate evidence can fairly evaluate deterministic baseline, JEV, and HOLD where meaningful. JEV remains `UNPROVEN` until evidence demonstrates value.

# PHASE 7 — Intelligence Experiment: News / Information

## Hypothesis
Weekend moves associated with meaningful real-world information may behave differently from thin-market noise.

## Tasks
Build point-in-time evidence for relevant filings, earnings/events, material headlines, corporate actions, splits, halts, and listing age. Initial classes: `NEWS`, `NOISE`, `UNCERTAIN`.

Test baseline Rule W versus policies such as fade NOISE, skip NEWS, skip UNCERTAIN; FOLLOW NEWS is a separate hypothesis. Register material variants.

## Gate G7
Research passes if provenance/timing are preserved, no future leakage exists, development/holdout remain separated, variants are registered, and economics include relevant costs.

A capability enters W2 only if evidence supports value beyond baseline without holdout tuning. Otherwise preserve negative/inconclusive Experience.

# PHASE 8 — HIP-3 Oracle / Microstructure Research

## Goal
Determine whether Rule W materially depends on oracle/market mechanics.

Investigate where evidence permits: oracle construction/update behavior, mark behavior, external inputs, closed-underlying behavior, instrument differences, historical mechanism changes, and relationship to Rule W outcomes.

Do not assume the conclusion. Unsupported findings remain `UNKNOWN`.

## Gate G8
Produce evidence-supported mechanics, explicit unknowns, testable dependency hypotheses, and new data-collection requirements.

**Critical condition:** if evidence shows Rule W depends on a mechanism that no longer exists, STOP Rule-W W2 promotion and reassess before Phase 10.

# PHASE 9 — Cost / Liquidity Research

## Goal
Test whether execution/liquidity information improves candidate selection.

Evaluate available spread, depth, estimated slippage, listing age, trading activity, and market age. Compare raw-signal ranking with expected-edge-after-cost ranking where justified. Preserve prior flat-cost experiments.

## Gate G9
Cost model must be reproducible; observed book evidence must be distinguishable from assumptions; old results remain intact; development/holdout discipline remains intact.

Only evidence-supported improvement is eligible for W2.

# PHASE 10 — Construct W2 Candidate

## Goal
Build the smallest meaningful candidate generation supported by evidence.

Default: at most one Intelligence improvement plus one Capital/strategy improvement. Do not bundle unrelated architecture work.

Create W2 Candidate manifest and structured W1→W2 diff across Intelligence, Capital, decision, risk, and data/config. Keep hard risk comparable unless separately operator-approved.

## Gate G10
Every W2 change must have experiment evidence; W1 remains immutable; W2 is reproducible; candidate remains small enough for causal comparison.

# PHASE 11 — Historical + Holdout W1/W2 Evaluation

Where data permits compare:
- A: W1 Intelligence + W1 Capital
- B: W2 Intelligence + W1 Capital
- C: W1 Intelligence + W2 Capital
- D: W2 Intelligence + W2 Capital

Preserve deterministic/JEV/HOLD arms where applicable. Measure net economics, uncertainty, tail behavior, avoided losses, missed opportunities, costs, decision quality, and complexity/research cost where measurable.

## Gate G11
**PASS to forward paper** only if improvement survives outside development tuning, integrity holds, hidden risk expansion does not explain it, and result is not merely one selected variant.

**FAIL:** reject/modify and preserve evidence.  
**INCONCLUSIVE:** collect more evidence/keep W1. Do not manufacture a candidate because the weekend is approaching.

# PHASE 12 — W2 Forward Paper Experiment

## Prerequisites
G11 PASS; experiment preregistered before outcome; W2 manifest/spec/decision/risk/candidate rules frozen.

During the experiment: no outcome-driven tuning. Record live observations, decisions, risk, fills, costs, incidents, and W1 shadow/counterfactual where valid.

After the experiment: grade before modifying W2.

## Gate G12
Classify `VALID`, `VALID_WITH_LIMITATIONS`, `INVALID_FOR_STRATEGY_INFERENCE`, or `UNRESOLVED`.

One winning window does not validate W2; one losing window does not automatically reject it.

# PHASE 13 — W1 vs W2 Forward Review

Compare historical/holdout expectation, W1 counterfactual, W2 actual, execution/information differences, avoided/missed trades, costs, incidents, decision quality, and realized outcome.

Possible decisions:
- `PROMOTE`
- `CONTINUE_FORWARD`
- `MODIFY`
- `REJECT`
- `INVALID`

A modification becomes a new registered variant/candidate, never a silent edit.

Promotion is based on evidence quality, uncertainty, risk, execution, costs, regressions, complexity, and attribution—not raw P&L alone.

# OPERATOR REVIEW #2 — EVOLUTION READINESS

Mandatory stop.

Do not automate Experience retrieval/Evolution until Tengu has demonstrated a trustworthy measured lifecycle:

`W1 → Experience → proposed improvement → W2 Candidate → historical test → holdout → forward test → comparison → promote/reject/continue`.

# PHASE 14 — Experience Retrieval

Start with the simplest adequate method. Do not introduce complex vector infrastructure without demonstrated need. Similarity should emphasize economic mechanism/context, event type, liquidity, volatility, regime, market state, and recency—not merely asset identity.

Output bounded Evidence Packets, not raw memory dumps.

## Gate G14
Known fixtures must retrieve relevant positive and negative analogues without future leakage. Poor retrieval must not be connected to decisions.

# PHASE 15 — Decision Map Experiment

Compare:
- A deterministic policy
- B context → JEV
- C Architect → compact Decision Map → JEV
- D JEV with Architect escalation

Measure economics, calibration, stability, latency, model cost, research cost, and error modes.

## Gate G15
Choose a more complex architecture only if evidence shows value. “Deterministic is better for this decision class” is a valid result.

# PHASE 16 — Evolution Prototype

Evolution consumes Experiment Registry, Variant Registry, Experience, capability/source metrics, historical/forward results, and failure attribution.

Allowed outputs: `KEEP`, `MODIFY`, `REMOVE`, `INVESTIGATE`, `ADD_CANDIDATE`.

It may not silently mutate frozen generations, expand hard risk, grant signing authority, promote from development-only evidence, or erase failures.

Candidate lifecycle:

`PROPOSE → REGISTER → HISTORICAL TEST → HOLDOUT → FORWARD/SHADOW → COMPARE → PROMOTE/REJECT`

## Gate G16
Run at least one complete candidate lifecycle without new live-money authority. PASS only if lineage remains reconstructable, generation isolation holds, failed candidates remain recorded, promotion obeys evidence gates, and operator-controlled risk remains intact.

# 5. Rollback / Stop Matrix

| Failure | Required action |
|---|---|
| Historical evidence overwritten | Stop; restore original evidence; audit affected phases |
| Point-in-time leakage | Invalidate affected experiments; fix; rerun uncontaminated evidence |
| Accounting mismatch | Stop strategy inference; reconcile first |
| W1 changes after freeze | Revert contamination; rerun generation-isolation tests |
| New framework duplicates X Market/X Lab | Stop; redesign toward reuse/extension |
| Risk bypass becomes possible | Stop immediately; restore boundary; review |
| Backfilled data indistinguishable from live | Fix provenance before using evidence |
| Missing historical metadata | Mark `UNKNOWN`; never fabricate |
| JEV adds no value | Preserve evidence; bypass/retire for that decision class |
| Intelligence experiment fails | Preserve negative Experience; exclude from W2 |
| Cost/liquidity experiment fails | Preserve negative Experience; exclude from W2 |
| Oracle research invalidates Rule W premise | Stop Rule-W promotion; reassess |
| W2 fails holdout | Reject/modify; no forward test |
| W2 evidence inconclusive | Keep W1; gather more evidence |
| Forward integrity invalid | Do not infer performance; fix process; rerun later |
| One valid W2 loss | Analyze; do not auto-reject solely from one outcome |
| One valid W2 win | Analyze; do not auto-promote solely from one outcome |
| Experience retrieval poor | Do not connect it to decisions |
| Complex decision layer adds no value | Prefer simpler architecture |
| Evolution cannot preserve lineage | Do not automate promotion |

# 6. Required Test Families

Progressively add/reuse tests for:

### Integrity
- future leakage;
- provenance;
- live vs backfilled distinction;
- immutable original artifacts.

### Reproducibility
- deterministic backtest repeatability;
- generation-manifest resolution;
- W1 replay after W2 changes.

### Isolation
- W1 cannot access W2-only capability;
- candidate cannot mutate frozen baseline;
- paper/live authority separation.

### Accounting
- orders/fills/positions reconcile;
- fees/funding/slippage consistency;
- missing/error data never silently becomes zero.

### Experiment
- development/holdout separation;
- variant registration;
- preregistration before forward outcome;
- negative result preservation.

### Experience
- Rule W lineage;
- rejected-strategy lineage;
- operational-incident lineage;
- lucky-bad and good-unlucky decisions representable.

### Decision
- deterministic baseline available;
- JEV compared rather than assumed;
- HOLD supported where meaningful;
- calibration measurable where applicable.

### Risk
- deterministic gate non-bypassable;
- unknown required risk state denies;
- generation work cannot silently expand authority.

# 7. Default Weekly Operating Pattern

This is a cadence, not a release deadline.

**Monday:** grade forward evidence, ingest Experience, review incidents, decide candidate status.

**Tuesday–Thursday:** historical research, variants, Intelligence/Capital experiments, candidate work.

**Thursday/Friday:** only if historical/holdout gates pass, freeze/preregister the next forward candidate. Otherwise keep W1 and continue research/data collection.

**Weekend / next eligible market window:** run frozen forward experiment without tuning.

**Following Monday:** grade before modification.

# 8. Definition of Progress

Progress is not more tools, models, code, trades, or architecture.

Progress means Tengu can increasingly prove:

- why a hypothesis exists;
- what was tried before it;
- what information was available at decision time;
- whether evidence was development, holdout, or forward;
- what alternatives would have done;
- which capability changed;
- whether improvement survived unseen data;
- whether the previous generation remains reproducible;
- whether a rejected generation's lessons remain available.

# 9. Monday Start Instruction

After the weekend experiment reaches its planned terminal point, give the coding agent only:

> Read `TENGU_HANDOFF.md` and `TENGU_ROADMAP.md`. Inspect the repository and confirm the weekend experiment has completed. Start at Phase 0 and follow every gate in `TENGU_ROADMAP.md`. Execute through Phase 5 only. Stop at Operator Review #1 and deliver the complete review package. Do not begin Phase 6+ without explicit approval.

If Phase 0 reveals an experiment-integrity problem, follow the Phase 0 failure path instead of continuing automatically.

# 10. Final Rule

**Never automate a lifecycle that Tengu cannot yet measure correctly by hand.**

First prove:

`W1 → evidence → Experience → candidate → W2 → historical evaluation → holdout → forward evaluation → comparison`.

Only after that lifecycle is trustworthy should Tengu automate Experience retrieval, Decision Maps, and Evolution.
