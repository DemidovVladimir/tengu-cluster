# Tengu — Codex Project Handoff

**Repository:** `DemidovVladimir/tengu-cluster`  
**Document purpose:** Current product/architecture context and implementation instructions for coding agents.  
**Status:** Active source of product intent.  
**Current target:** Establish measurable W1 → W2 evolution using the existing X Market and X Lab systems.

---

# 0. Instructions to the Coding Agent

Read this document before making architectural changes.

Then inspect the repository itself.

## Sources of truth

Use this priority:

1. Existing working code and tests for implementation reality.
2. This document for current product intent and architectural direction.
3. X Market / X Lab documentation for experiment history and existing behavior.
4. Older PRDs/design documents only as historical context.

If an older document conflicts with this handoff, do not silently follow the older document.

Document the conflict.

## Critical rule

**Do not rebuild working X Market or X Lab functionality merely to match a cleaner theoretical architecture.**

Prefer:

REUSE

→ EXTEND

→ GENERALIZE

over:

REWRITE.

---

# 1. What Tengu Is

Tengu is intended to become a self-evolving capital intelligence system.

It is not intended to be merely:

- a crypto trading bot;
- a stock trading bot;
- an LP bot;
- an arbitrage bot;
- a news trading bot.

The long-term objective is:

> Given the information currently available, historical experience, available economic capabilities, available capital, uncertainty and deterministic risk constraints, determine what sequence of actions currently offers the best risk-adjusted expected economic outcome.

**DO NOTHING / HOLD must always be a valid outcome.**

---

# 2. Fundamental Doctrine

The architectural doctrine is:

**Reasoning models understand.**

**JEV makes bounded typed decisions where it proves useful.**

**Deterministic software calculates.**

**Deterministic risk controls authority.**

**Tengu tools act.**

No reasoning model receives unrestricted control of money.

---

# 3. Long-Term Economic Universe

Tengu must not assume:

economic action = BUY or SELL.

Potential Capital capabilities include:

- spot;
- perpetuals;
- liquidity provision;
- concentrated LP;
- hedging;
- delta-neutral combinations;
- funding;
- basis;
- arbitrage;
- lending;
- borrowing;
- staking;
- yield;
- cash/stable allocation;
- future economic primitives.

The capability universe is intentionally extensible.

---

# 4. Existing System

This project is NOT greenfield.

Two important systems already exist:

## X Market

A forward/paper trading environment.

Existing functionality includes:

- live Hyperliquid observations;
- market contexts;
- order books;
- typed rows;
- observation stores;
- historical day stores;
- paper ledger;
- positions;
- funding;
- runtime state;
- feed health;
- risk logs;
- network-call logs;
- deterministic risk gate;
- realistic paper fills;
- paper strategy execution;
- weekend Rule W experiment.

Conceptually:

**X Market = Forward Experiment Environment.**

---

# 5. X Lab

X Lab is the historical research environment.

Existing functionality includes:

- historical data warehouse;
- Hyperliquid candle/funding history;
- deterministic Rust backtester;
- strategy specifications represented as typed data;
- development/research arm;
- capped arm;
- JEV arm;
- cost modelling;
- funding;
- risk caps;
- bootstrap statistics;
- run evidence directories;
- holdout discipline;
- point-in-time integrity;
- JEV cached replay;
- regression tests for previously discovered research errors.

Conceptually:

**X Lab = Historical Experiment Environment.**

---

# 6. Existing Research Results Matter

Do not treat previous research as disposable.

Multiple economic hypotheses have already been tested.

Most failed.

That negative evidence is valuable.

The strongest currently surviving family is:

**Rule W — weekend stock-perpetual overshoot fade.**

The system must preserve both successful and failed hypotheses as Experience.

---

# 7. Rule W

Rule W currently serves as the first reference strategy for the integrated Tengu architecture.

Conceptually:

Friday 20:00 New York:

record anchor.

Weekend:

stock underlying is closed while the corresponding HIP-3 stock perp continues trading.

Sunday 18:00:

measure weekend movement.

Fade sufficiently large movement.

Monday 09:00:

exit before regular US market open.

Current paper implementation operates under deterministic risk limits.

Rule W is important primarily because it already has:

- historical research;
- negative controls;
- holdout;
- strategy variants;
- JEV comparison;
- paper execution;
- forward experiment;
- real recorded books;
- explicit risk policy.

Therefore it provides the first end-to-end Experience lineage.

Tengu is NOT intended to become a Rule-W-only system.

---

# 8. Current Weekend Forward Experiment

At the time this architecture was defined, a preregistered weekend paper experiment was running.

The experiment includes:

- Friday anchor;
- Sunday entry;
- Monday exit;
- capped $100 paper portfolio;
- larger shadow paper ledger;
- live books;
- market context recording;
- execution-cost observations;
- risk decisions.

Known infrastructure incidents were recorded separately from strategy performance.

Do not alter historical experiment evidence retroactively.

If this experiment has already completed when reading this document:

**import and grade the completed result rather than recreating it.**

---

# 9. Current Architectural Insight

The project originally considered building:

M0 experimental foundation

→ M1 capability system

→ M2 synthetic world

from scratch.

Do NOT do this.

Review of X Market and X Lab showed that much of that infrastructure already exists.

The missing architecture is primarily:

**Experiment lineage**

+

**Variant lineage**

+

**Experience**

+

**Generation identity**

+

**Capability identity**

+

**Evolution measurement.**

---

# 10. Target Architecture

Conceptually:

WORLD

→ OBSERVATION

→ INTELLIGENCE

→ EXPERIENCE RETRIEVAL

→ ARCHITECT

→ CONTEXTUAL DECISION SPACE

→ DECISION POLICY

→ DETERMINISTIC RISK

→ EXECUTION

→ OUTCOME

→ EXPERIENCE

→ EVOLUTION

→ NEXT GENERATION.

X Market and X Lab remain major components of this architecture.

---

# 11. Three Time Scales

Do not conflate these.

## Runtime Loop

Seconds/minutes.

OBSERVE

→ DECIDE

→ RISK

→ EXECUTE

→ MONITOR.

## Adaptive / Research Loop

Minutes/hours/days.

INVESTIGATE

→ RETRIEVE HISTORY

→ TEST

→ COMPOSE CONTEXT

→ UPDATE HYPOTHESIS.

## Evolution Loop

Initially roughly weekly.

REVIEW EXPERIENCE

→ PROPOSE CHANGE

→ TEST CANDIDATE

→ COMPARE GENERATIONS

→ PROMOTE / REJECT.

Weekly software evolution does NOT mean weekly market awareness.

---

# 12. Intelligence Plane

The Intelligence Plane represents:

> How does Tengu know what is happening?

Potential capabilities include:

- price;
- order book;
- funding;
- OI;
- volume;
- volatility;
- liquidity;
- filings;
- news;
- X/social;
- on-chain;
- protocol information;
- historical retrieval;
- event classification;
- statistical analysis.

Intelligence capabilities themselves must eventually be measurable and evolvable.

---

# 13. Capital Plane

The Capital Plane represents:

> What can Tengu economically do?

Potential capabilities include:

TRADE

LP

HEDGE

LEND

BORROW

STAKE

ARBITRAGE

HOLD

and future primitives.

---

# 14. Intelligence Context / Intelligence Map

Do not immediately build a giant universal map.

For a specific situation Architect should eventually compose a bounded context answering:

> What information matters here?

For Rule W this might include:

- weekend movement;
- liquidity;
- spread;
- depth;
- listing age;
- filing activity;
- company news;
- related markets;
- historical analogues;
- oracle state.

This representation may mature into an Intelligence Map.

---

# 15. Capital Context / Capital Map

For the same situation:

> What economically meaningful actions are currently available?

Example:

FADE LONG

FADE SHORT

FOLLOW

HEDGE

HOLD.

This representation may mature into a Capital Map.

---

# 16. Important Open Question: JEV

Do NOT design the entire system around the assumption that JEV adds value.

Existing X Lab evidence already indicates that JEV has not yet demonstrated statistically convincing improvement over Rule W.

Therefore:

**JEV VALUE = UNPROVEN**

for the current decision class.

JEV remains a useful architectural candidate, not a sacred component.

---

# 17. Decision Architecture Must Be Experimental

Eventually compare:

## A

Deterministic strategy.

## B

JEV.

## C

Architect → bounded Decision Map → JEV.

## D

JEV with Architect escalation.

## E

HOLD / NO ACTION where applicable.

Compare them under identical historical/forward conditions.

Measure:

- economics;
- calibration;
- latency;
- stability;
- inference/research cost.

---

# 18. Architect

Architect is the sophisticated reasoning component.

Potential responsibilities:

- research;
- hypothesis generation;
- criticism;
- historical comparison;
- information selection;
- experiment design;
- strategy composition;
- candidate-generation proposals.

Architect must not directly move capital.

Architect quality must itself become measurable.

---

# 19. Risk

Hard financial authority remains deterministic.

Risk must remain non-bypassable.

Architect cannot override it.

JEV cannot override it.

Experience cannot override it.

Evolution cannot override it.

Unknown required risk information should fail closed.

Changes that expand hard risk authority require explicit operator approval.

---

# 20. Experiment Registry

Experiment becomes a first-class object.

Every important experiment should be traceable to:

- experiment identity;
- hypothesis;
- creation time;
- generation;
- strategy/spec;
- data window;
- development window;
- holdout window;
- forward window;
- cost model;
- capabilities used;
- decision policy;
- result;
- uncertainty;
- verdict.

Reuse existing run evidence rather than duplicating raw data.

---

# 21. Variant Registry

This is important.

Record materially different variants tried under the same strategy/hypothesis family.

Examples:

Sunday entry;

Saturday entry;

top-4;

all names;

liquidity filter;

listing-age filter;

different stops;

different signal thresholds.

Reason:

If 100 variants are attempted and the best one is shown, the statistical result cannot be interpreted as though only one hypothesis was tested.

The system must know how much searching occurred.

---

# 22. Experience

Experience is:

**CONTEXT + HYPOTHESIS + INFORMATION + DECISION + ACTION + OUTCOME + QUALITY.**

Experience should generally reference existing evidence instead of copying large raw datasets.

---

# 23. Experience Episode

Minimum conceptual contents:

## Context

What situation existed?

## Hypothesis

What was believed?

## Evidence

What was known at the time?

## Alternatives

What could have been done?

## Decision

What was selected?

## Action

What actually executed?

## Outcome

What happened economically and operationally?

## Quality

Was the decision supported by information available at that time?

## Lesson

What should potentially affect future behavior?

---

# 24. Negative Experience Is First-Class

Preserve:

- failed strategies;
- rejected hypotheses;
- no-go research;
- JEV mistakes;
- Architect mistakes;
- malformed research;
- NO ACTION;
- execution failures;
- infrastructure incidents;
- missing data.

Do not train Evolution only on winners.

---

# 25. Rule W Acceptance Lineage

The system should eventually answer programmatically:

1. Where did Rule W originate?

2. Which hypotheses preceded it?

3. Which hypotheses failed?

4. Which historical experiments tested Rule W?

5. Which variants were attempted?

6. Which data was development data?

7. Which data was holdout?

8. What did holdout show?

9. What costs were assumed?

10. What did JEV do on the same candidates?

11. Did JEV demonstrate improvement?

12. Which forward experiment followed?

13. Which X Market run executed it?

14. Which risk policy governed it?

15. What happened during execution?

16. Which infrastructure incidents occurred?

17. Which data was live vs backfilled?

18. What was the forward result?

19. Which Experience Episode resulted?

20. Which generation owned each relevant component?

If this requires manually reading several Markdown reports, the Experience layer is not complete.

---

# 26. Forward Evidence Classes

Results must identify evidence class.

Use conceptually:

DEVELOPMENT

HOLDOUT

FORWARD_PAPER

LIVE_MICRO

LIVE_PRODUCTION.

Do not mix these categories.

---

# 27. Information Timing

Preserve where possible:

event_time;

publication_time;

source_observed_time;

Tengu_received_time;

analysis_time;

decision_time;

execution_time.

Information arriving after the decision must never appear as evidence available before the decision.

---

# 28. Source Provenance

External observations must preserve provenance.

Derived Tengu conclusions must be distinguishable from external observations.

Ten reposts of one article are not ten independent confirmations.

---

# 29. Point-in-Time Integrity

At simulated time T:

only information available at or before T may be visible.

Future:

prices;

bars;

news;

labels;

revisions;

outcomes

must not leak backward.

Any leakage invalidates the experiment.

Preserve X Lab's existing time-integrity protections.

---

# 30. Strategy as Data

Preserve the existing X Lab principle:

**strategy should preferably be represented as typed data/specification rather than new executable code.**

Architect may propose new strategy specifications using existing primitives.

Create new executable capability only when the required primitive genuinely does not exist.

---

# 31. Capability Registry

Create a registry only for capabilities relevant to:

- decisions;
- experiments;
- generation comparison;
- economic execution.

Do not register every helper function.

Primary classes:

INTELLIGENCE

CAPITAL

RISK

INFRASTRUCTURE.

Each relevant capability should eventually have:

- identity;
- version;
- typed contract;
- permission class;
- generation availability;
- lifecycle status.

---

# 32. Capability Lifecycle

Use conceptually:

DISCOVERED

→ CANDIDATE

→ HISTORICALLY_TESTED

→ FORWARD_PAPER

→ APPROVED

→ ACTIVE

→ DEMOTED

→ RETIRED.

A capability does not gain real-money authority merely because historical results look good.

---

# 33. Generation

Generation is a frozen measurable configuration of Tengu.

Example:

W1

W2-CANDIDATE

W2.

Generation manifest should reference relevant:

- Intelligence capabilities;
- Capital capabilities;
- strategy specs;
- decision policy;
- research policy;
- model configuration;
- risk-policy version;
- relevant schema/configuration versions.

---

# 34. Generation Immutability

Once W1 is frozen:

W2 changes must not silently alter W1.

Historical replay of W1 must remain reproducible.

This is a critical invariant.

---

# 35. Experience Retrieval

Do not build sophisticated memory infrastructure prematurely.

Once enough Experience exists, prototype retrieval using the simplest adequate method.

Given current context, retrieve comparable episodes based on:

- economic mechanism;
- market state;
- event structure;
- regime;
- liquidity;
- volatility;
- recency.

Asset identity alone must not dominate similarity.

---

# 36. Evidence Packet

Future decision systems should receive bounded evidence.

Example:

Comparable episodes: 24

Positive: 17

Negative: 5

Inconclusive: 2

Median outcome: X

Worst outcome: Y

Important failure conditions:

- meaningful weekend news;
- low liquidity;
- recently listed instrument.

Regime similarity: Z

Confidence: C.

Do not dump unrestricted Experience Memory into JEV.

---

# 37. Decision Quality vs Outcome

Never learn:

profit = good decision

loss = bad decision.

Store separately:

- expected decision quality;
- execution quality;
- realized outcome.

Support:

lucky bad decision;

good unlucky decision.

---

# 38. Economic Accounting

Compare strategies using net economics.

Include where applicable:

- fees;
- spread;
- slippage;
- funding;
- gas;
- borrow cost;
- incentives;
- hedge cost;
- rebalancing.

Gross theoretical edge is insufficient.

---

# 39. Counterfactuals

Where practical compare reasonable alternatives available at the time.

For Rule W:

plain Rule W;

JEV-filtered Rule W;

HOLD;

news-filtered Rule W;

alternative entry;

liquidity-aware Rule W.

Counterfactuals help separate luck from decision quality.

---

# 40. HIP-3 Oracle Research

This is a specific research requirement, not a solved assumption.

Investigate:

- how the relevant HIP-3 oracle is constructed;
- how frequently it updates;
- what happens while the underlying stock is closed;
- how mark price behaves;
- what external price inputs are used;
- whether behavior differs across instruments;
- whether oracle mechanics changed during the historical sample.

Reason:

Rule W may partially depend on the microstructure/oracle mechanics of these markets.

If those mechanics change, the historical edge may disappear.

Do not assume the explanation before testing it.

---

# 41. Permanent Data Collection

Continue recording information that cannot reliably be reconstructed later.

Particularly valuable:

- order books;
- real spread/depth;
- source arrival times;
- execution conditions;
- temporary failures;
- live oracle/mark state where available.

Do not assume historical APIs can reconstruct all live conditions later.

---

# 42. First Recommended W2 Intelligence Experiment

Hypothesis:

> Weekend stock-perp movements associated with meaningful new information behave differently from movements driven mainly by thin-market noise.

Build historical point-in-time labels where possible:

NEWS

NOISE

UNCERTAIN.

Potential policies:

fade NOISE;

skip NEWS;

skip UNCERTAIN.

FOLLOW NEWS may be tested separately.

Do NOT assume it is profitable.

Compare against baseline Rule W.

---

# 43. First Recommended W2 Capital/Strategy Experiment

Improve candidate selection using real economic costs.

Potential features:

- spread;
- book depth;
- estimated slippage;
- listing age;
- trading activity;
- liquidity.

Test:

raw-move ranking

vs

expected-edge-after-cost ranking.

Do not promote based only on development results.

---

# 44. W2 Candidate

Keep W2 small.

Recommended:

## Intelligence change

Weekend news/information classification.

## Capital/strategy change

Cost/liquidity-aware Rule W selection.

Avoid unrelated large architectural changes in the same candidate.

---

# 45. W1 vs W2 Evaluation

Where data permits compare:

## A

W1 Intelligence + W1 Capital.

## B

W2 Intelligence + W1 Capital.

## C

W1 Intelligence + W2 Capital.

## D

W2 Intelligence + W2 Capital.

This separates improvement from better information vs better action.

---

# 46. Evolution

Only after generation comparison works.

Evolution consumes:

- Experiment Registry;
- Variant Registry;
- Experience;
- capability metrics;
- source metrics;
- historical results;
- forward results;
- failure attribution.

Evolution may propose:

KEEP

MODIFY

REMOVE

INVESTIGATE

ADD CANDIDATE.

It may not automatically expand real-money authority.

---

# 47. Synthetic Testing Philosophy

Do NOT build a second synthetic trading platform unless necessary.

Use small deterministic adversarial fixtures inside existing testing infrastructure.

Required future fixtures:

- future leakage;
- duplicate sources;
- late accurate source;
- fast noisy source;
- lucky bad decision;
- good unlucky decision;
- regime shift;
- unavailable capability;
- generation isolation;
- accounting corruption;
- NO ACTION.

Synthetic tests validate invariants.

Real history and forward experiments validate economics.

---

# 48. Current Implementation Scope

The immediate coding iteration is intentionally smaller than the complete architecture.

Implement only the foundation necessary to make W1 measurable.

## Execute now

Phase 0 through Phase 5 below.

## Design but do not automatically execute

Phase 6 onward.

Stop for operator review after Phase 5.

---

# 49. PHASE 0 — Finish Current Forward Experiment

If the weekend experiment is still active:

do not alter its strategy.

Complete:

- entry recording;
- execution recording;
- exit;
- fees;
- funding;
- slippage;
- P&L;
- shadow results;
- incidents;
- backfill.

If it has already completed:

grade and import it.

Backfilled observations must be distinguishable from live-recorded observations.

Import recorded weekend books into X Lab where useful for realistic cost evaluation.

Do not silently overwrite previous flat-cost results.

---

# 50. PHASE 1 — Inventory and Freeze W1

Inspect actual repository code.

Inventory:

- runtime;
- X Market;
- weekend sandbox;
- X Lab;
- feeds;
- tools;
- strategy kinds;
- stores;
- ledgers;
- risk;
- Architect;
- JEV;
- tests.

For each requirement classify:

REUSE

EXTEND

NEW

DEFER

REMOVE.

Then verify existing claims through tests.

Create a machine-readable:

**W1 Manifest.**

Freeze W1 after necessary correctness fixes.

Candidate changes must not mutate W1.

---

# 51. PHASE 2 — Experiment Registry

Generalize existing run/evidence identity.

Implement structured experiment metadata sufficient to connect:

HYPOTHESIS

→ STRATEGY

→ DATA

→ RESULT

→ VERDICT.

Import existing important X Lab experiments.

At minimum include:

- weekend_fade;
- weekend_fade_liquid;
- weekend_fade_top4;
- weekend_follow;
- xyz_overnight_follow;
- xyz_funding_carry;
- crypto_move_fade;
- crypto_funding_carry;
- sol_eth_spread;
- relevant Saturday/weekday investigations.

Negative results must remain queryable.

---

# 52. PHASE 3 — Variant Registry

Add explicit strategy/hypothesis lineage.

Record:

parent hypothesis;

derived variant;

changed dimensions;

reason for variant;

creation time;

experiment;

result.

Make it possible to ask:

> How many related variants were attempted before this result?

New forward variants should be registered before their outcomes are known.

---

# 53. PHASE 4 — Experience

Implement Experience Episode representation.

Prefer references to existing evidence.

Do not duplicate entire historical datasets.

Create:

## Reference Positive/Surviving Episode

Rule W complete lineage.

## Reference Negative Episode

At least one rejected strategy.

## Operational Episode

At least one infrastructure/data incident.

The system must distinguish economic failure from infrastructure failure.

---

# 54. PHASE 5 — Capability Registry

Inventory only decision/evolution-relevant capabilities.

Classify:

INTELLIGENCE

CAPITAL

RISK

INFRASTRUCTURE.

Add:

identity;

version;

typed contract reference;

permission class;

generation availability.

Do not confuse:

strategy variant

with

new capability.

Create automated verification that:

W1 cannot access a W2-only capability.

---

# 55. STOP CONDITION

After Phase 5:

STOP IMPLEMENTATION.

Produce a review package.

Do not automatically implement Intelligence Map, Capital Map, autonomous Evolution or new live execution.

---

# 56. Required Review Package

Deliver:

## Architecture Inventory

What actually exists.

## Gap Analysis

For requirements:

REUSE / EXTEND / NEW / DEFER / REMOVE.

## W1 Manifest

Exact measurable baseline.

## Experiment Registry

Including failures.

## Variant Registry

Including historical variants where reconstructable.

## Experience Representation

And imported episodes.

## Rule W Lineage

Historical → holdout → JEV → forward.

## Failed Strategy Lineage

At least one.

## Capability Registry

Decision/evolution-relevant capabilities only.

## Verification Report

Tests and invariants.

## Unresolved Questions

Anything that cannot be established from code/data.

## Phase 6–10 Proposal

A concrete implementation proposal, but do not execute it without review.

---

# 57. Required Acceptance Test — Rule W

The system must answer from structured evidence rather than manually reading Markdown:

1. What hypothesis produced Rule W?

2. Which alternatives were rejected?

3. Which historical runs tested it?

4. Which variants exist?

5. How many related variants were tried?

6. Which data was development?

7. Which data was holdout?

8. What did holdout show?

9. What costs were modelled?

10. Which capabilities produced the evidence?

11. What did JEV decide?

12. How did JEV compare with deterministic Rule W?

13. What was the uncertainty?

14. Which forward test was preregistered?

15. Which run executed it?

16. Which risk policy governed it?

17. Which incidents occurred?

18. Which data was backfilled?

19. What was the forward outcome?

20. Which Experience Episode represents it?

21. Which generation owned the relevant components?

---

# 58. Required Acceptance Test — Rejected Idea

Select at least one no-go strategy.

Reconstruct:

HYPOTHESIS

→ EXPERIMENT

→ EVIDENCE

→ RESULT

→ VERDICT

→ EXPERIENCE.

Negative knowledge must survive.

---

# 59. Required Acceptance Test — Generation Isolation

Create or simulate a W2-only change.

Verify:

W1 replay unchanged.

W1 manifest unchanged.

W1 cannot invoke W2-only capability.

---

# 60. PHASE 6 — Decision Evaluation

**Do not implement automatically.**

Proposed next work:

formalize deterministic vs JEV comparisons;

preserve candidate-level counterfactuals;

measure calibration;

test Architect escalation.

---

# 61. PHASE 7 — Information Experiment

**Do not implement automatically.**

Proposed next work:

historical weekend filings/news;

point-in-time event labels;

NEWS / NOISE / UNCERTAIN;

compare information-aware policy with Rule W.

---

# 62. PHASE 8 — Oracle/Microstructure Research

**Do not implement automatically.**

Investigate HIP-3 oracle/mark behavior and Rule-W dependence on market mechanics.

---

# 63. PHASE 9 — Cost/Liquidity Experiment

**Do not implement automatically.**

Use recorded books and liquidity features to test cost-aware candidate selection.

---

# 64. PHASE 10 — W2 Candidate

**Do not implement automatically.**

Construct small W2 Candidate.

Prefer:

one Intelligence improvement

+

one Capital/strategy improvement.

Compare W1/W2 using controlled experiments.

---

# 65. Later — Experience Retrieval

Only after Experience exists.

Retrieve historical analogues.

Start simple.

Do not prematurely introduce complex vector infrastructure.

Evaluate retrieval quality before connecting it to decisions.

---

# 66. Later — Decision Map Experiment

Test whether JEV should receive:

Intelligence Context + Capital Context directly

or

a compact Architect-produced Decision Map.

Compare against deterministic baseline.

Do not choose architecture by intuition.

---

# 67. Later — Evolution Loop

Only after W1/W2 comparison is trustworthy.

Evolution should propose candidates based on evidence.

It should not directly mutate the running production generation.

Candidate lifecycle:

PROPOSE

→ TEST

→ HOLDOUT

→ FORWARD/SHADOW

→ COMPARE

→ PROMOTE / REJECT.

---

# 68. Research Cost

Eventually record:

- model calls;
- API calls;
- MCP calls;
- latency;
- information gained;
- decisions changed.

More research is not automatically better.

**ENOUGH INFORMATION** must be a valid action.

---

# 69. Tool Explosion

More capabilities are not automatically better.

Eventually measure:

- usage;
- reliability;
- redundancy;
- latency;
- cost;
- informational contribution;
- economic contribution.

Evolution must be able to retire tools.

---

# 70. Regime Drift

Historical evidence should eventually include:

- recency;
- regime similarity;
- sample size;
- uncertainty.

Do not treat an edge discovered in one market regime as permanent.

---

# 71. Causal Attribution

When performance changes, attempt to determine why.

Possible categories:

INFORMATION

ANALYSIS

MEMORY

COMPOSITION

DECISION

RISK

EXECUTION

INFRASTRUCTURE

RANDOM OUTCOME.

Do not explain every P&L change as model intelligence.

---

# 72. Primary Product Metric

Do NOT use:

“Did Tengu make money this week?”

as the primary success criterion.

Use:

> Across independent evaluation periods, does the next generation demonstrate better information acquisition, decision quality and risk-adjusted economic outcomes without hidden increases in risk?

---

# 73. Core Product Principle

Tengu must evolve both:

**HOW IT SEES**

and

**WHAT IT CAN DO.**

Experience connects those two planes.

Architect reasons and composes.

Decision policies choose within bounded contexts.

Risk controls authority.

Execution acts.

Evidence determines whether a change survives.

---

# 74. Immediate Coding-Agent Instruction

Start by inspecting the repository.

Do not begin by implementing abstractions from this document.

Map this document onto the existing implementation.

Reuse existing systems wherever possible.

Protect the current/completed weekend experiment.

Then execute only:

**Phase 0 → Phase 5.**

Run all existing tests plus new acceptance tests.

Produce the review package.

**STOP.**

Do not continue into Phase 6+ without operator review.

The goal of this iteration is not to create autonomous self-evolution.

The goal is to make the current Tengu system capable of proving whether its next generation is actually better than the current one.