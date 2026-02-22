# Tengu Cluster - User Stories and Coverage Matrix

Date: 2026-02-21
Owner: Product + Architecture

## Purpose

This file defines high-value user scenarios and maps each one to implementation coverage.
It is the acceptance anchor for roadmap realism, not a marketing artifact.

Coverage status values:
- `Covered` - implemented and validated in current codebase
- `Partial` - foundational pieces exist, end-to-end scenario is not complete
- `Missing` - no meaningful implementation yet

## Story Index

| ID | Story | Coverage |
|---|---|---|
| US-001 | Multi-agent product team with flexible coordination hierarchy | Partial |
| US-002 | Mixed paid/free provider strategy per agent role | Partial |
| US-003 | Large-context models without prompt-budget waste | Partial |
| US-004 | Per-agent token/cost governance with deterministic degradation | Partial |
| US-005 | Workspace memory + retrieval that stays bounded and source-aware | Partial |
| US-006 | Tool execution with approvals, policy, and audit trails | Partial |
| US-007 | Hub runtime across multiple channels with lifecycle/health | Missing |
| US-008 | Portable runtime from Raspberry Pi to high-end GPU workstations | Partial |
| US-009 | Provider portability including Claude Code + hosted APIs | Partial |
| US-010 | Scenario-level acceptance validation before epic closure | Partial |
| US-011 | Capability governance for tools/skills/agent specs (user-led or orchestrator-led) | Partial |
| US-012 | Adapter-first + event-driven architecture for plug-and-play extensibility and auditable runtime | Partial |

---

## US-001

As a founder, I want a single-orchestrator setup with flexible dependent agents (hardware, software, marketing, product, frontend, backend, smart contract) so collaboration stays clear and centrally governed.

Acceptance targets:
1. Multi-agent runtime supports one orchestrator with flexible dependents in one workflow.
2. One orchestrator can govern/approve dependent-agent interactions and collect specialist outputs.
3. User receives one synthesized output with traceable sub-results from dependents.
4. Each dependent result is validated before downstream usage so invalid artifacts cannot silently cascade.

Current coverage:
- `Partial`: multiple agents can be configured, but runtime path is still single-agent chat-first.
- `Partial`: inter-agent handoff protocol is now defined as typed task/result envelopes, but explicit hierarchy/delegation execution loop is still missing.
- `Implemented baseline`: validation gate decisions exist for delegated handoffs (`/handoff pending`, `/handoff auto ...`, `/handoff accept|retry|rework|fail ...`) and dependent one-turn execution enters `review_required` before finalization.
- `Partial`: automated validator runner baseline is implemented (`/handoff auto` policy recheck + bounded Rust workspace checks), but check-suite breadth and dependency barriers for chained downstream artifact release are still pending.

Required tasks:
- `E6-T1`, `E6-T2`, `E6-T9`, `E6-T12`, `E6-T13`
- `E7-T1`, `E7-T5`
- `E8-T1`, `E8-T2`

---

## US-002

As an operator, I want to assign different provider/model tiers per role (for example OpenAI for orchestrator, Qwen local/free for hardware dependent, Claude-class for software dependent) so cost/performance is optimized by function.

Acceptance targets:
1. Per-agent engine/model selection is first-class and validated.
2. Runtime supports mixed providers in one cluster.
3. Fallback strategy is explicit when one provider fails.

Current coverage:
- `Partial`: Anthropic/OpenAI/Ollama/Claude Code implemented; runtime switching and multi-agent orchestration are pending.
- `Partial`: `config.webstudio.example.toml` includes a mixed-role topology template (including accountant with `huggingface` + `THUDM/GLM-4.7`), and Hugging Face backend baseline is implemented; selector/discovery/endpoint strategy follow-ups remain.

Required tasks:
- `E4-T5`, `E4-T7`, `E4-T10`, `E4-T11`, `E4-T12`
- `E4-T9` completed baseline
- `E6-T1`, `E7-T1`
- `E10-T1`

---

## US-003

As a user of large-context models, I want budget logic to preserve big input windows while reserving only realistic output space.

Acceptance targets:
1. Output reserve aligns to output cap, not naive context ratio.
2. Large-context regression tests prevent over-reserve regressions.
3. Per-agent overrides exist for context/output caps.

Current coverage:
- `Partial`: reserve alignment and overrides are implemented; tokenizer precision and model metadata registry are pending.

Required tasks:
- `E2-T7` (done baseline)
- `E4-T8` (done baseline)
- Follow-up: add tokenizer-accurate budgeting task under `E10`.

---

## US-004

As a budget-sensitive owner, I want deterministic per-agent budget behavior so the system degrades predictably instead of failing unpredictably.

Acceptance targets:
1. Hard caps enforced per turn and per flow.
2. Deterministic drop order under pressure.
3. Flow compaction remains restart-safe.

Current coverage:
- `Partial`: token controls are strong; cost limits are defined in schema but not enforced in runtime yet.

Required tasks:
- `E1` completed baseline
- `E2` completed baseline
- Follow-up: runtime cost enforcement task under `E10`.

---

## US-005

As an engineer, I want retrieval to stay token-bounded and cite sources so long sessions remain accurate and affordable.

Acceptance targets:
1. Retrieval is budget-capped and lens-aware.
2. Chunking and persistence avoid full re-index on restart.
3. Responses can trace source locations.

Current coverage:
- `Partial`: budget-capped retrieval is live; persistence/chunking/citations are pending.

Required tasks:
- `E3-T1` to `E3-T7`

---

## US-006

As a security-conscious user, I want tool calls to require policy checks and approvals with full audit logs.

Acceptance targets:
1. Tool loop executes safely with policy enforcement.
2. Approval flows are configurable.
3. Audit trail records tool input/output and decisions.

Current coverage:
- `Partial`: runtime can assemble tool-call events, execute a policy-checked `read_file` tool via registry, enforce config-driven approvals (`kit.approval_required`/`kit.approved`), and persist append-only JSONL audit records for policy/protocol/execution outcomes; broader toolset and delegated approval flows are pending.

Required tasks:
- `E8-T1`, `E8-T2` partial baseline
- `E8-T5` partial baseline

---

## US-007

As an operator, I want persistent hub mode for multiple channels with explicit lifecycle and health controls.

Acceptance targets:
1. `serve` runs as daemon with graceful lifecycle.
2. Channel adapters can be started/stopped and health-checked.
3. Streaming delivery works across supported channels.

Current coverage:
- `Missing`: CLI path is implemented; hub/channel lifecycle path is pending.

Required tasks:
- `E6-T1` to `E6-T8`

---

## US-008

As a deployment owner, I want one architecture that runs on Raspberry Pi and also scales on GPU desktops.

Acceptance targets:
1. Runtime profile detection and override are reliable.
2. Refiner/backend selection can adapt to hardware constraints.
3. Candle acceleration path supports CUDA/Metal/CPU fallback.

Current coverage:
- `Partial`: profile detection baseline exists; Candle runtime integration is pending.

Required tasks:
- `E9-T1` to `E9-T7`

---

## US-009

As a provider-agnostic user, I want portability across official APIs and Claude Code style runtime so subscriptions and local models can be mixed safely.

Acceptance targets:
1. Hosted providers (Anthropic/OpenAI/Google/HF) and Claude Code path are supportable.
2. Capabilities/limits are explicit and validated per backend.
3. Integration tests cover provider contracts and terminal stream guarantees.

Current coverage:
- `Partial`: Anthropic/OpenAI/Ollama/Claude Code/HuggingFace are implemented; Google coverage and provider integration tests are pending.

Required tasks:
- `E4-T3`, `E4-T7`, `E4-T10`, `E4-T11`, `E4-T12`
- `E4-T9` completed baseline
- `E5-T5` completed baseline

---

## US-010

As a product owner, I want scenario-level acceptance checks so epic completion means real-world readiness, not only unit test green status.

Acceptance targets:
1. Every epic maps to at least one user story acceptance check.
2. Release checklist includes scenario smoke tests.
3. Gaps are tracked as backlog tasks, not hidden assumptions.

Current coverage:
- `Partial`: epics/tasks exist; user-story coverage artifact is now introduced and needs enforcement.

Required tasks:
- `E10-T6`

---

## US-011

As a platform owner, I want strict capability governance so either the user directly controls tools/skills/agent specs, or one designated orchestrator controls subordinate agent capabilities within policy boundaries.

Acceptance targets:
1. User can define per-agent capability policy:
   - tool allow/deny
   - skills allow/deny
   - engine/model allowlist
   - sandbox mode
2. Orchestrator-control mode can be enabled so orchestrator can propose/assign subordinate capabilities.
3. Platform enforces hard guardrails so orchestrator decisions cannot exceed user-defined global boundaries.
4. Every capability change is audited with actor (`user` or `orchestrator`), reason, and timestamp.

Current coverage:
- `Implemented` strict cross-field config validation and fail-fast startup loading.
- `Implemented` governance-boundary validation for `kit`, `allowed_engines`, `skills`, sandbox mode, and routing/pipe consistency.
- `Implemented` runtime enforcement for `allowed_engines` and `kit` in active execution path:
  - engine/model allowlist is enforced in runtime engine construction
  - tool calls are checked against per-agent `kit` policy at start and before execution
- `Implemented` delegated actor gate for direct runtime capability requests:
  - in `mode=delegated`, only configured orchestrator can request direct capabilities in active runtime path
- `Implemented` capability-governance evaluator for handoff envelopes (`user` vs `delegated`) with bounded tool/skill/engine checks.
- `Partial` delegated control-plane baseline:
  - chat runtime supports `/assign`, `/assignments`, `/unassign`, `/assignments clear`, `/handoff ...`, `/stopall` for bounded delegated capability lifecycle
  - emitted handoff lifecycle events capture approved and queue-level accepted acknowledgements, plus `review_required` and terminal completed/failed/denied/revoked/expired states
  - assignment lifecycle outcomes are persisted as append-only JSONL audit records
  - non-expired approved assignments are replayed from audit log at startup for session continuity
  - delegated assignments are auto-expired by TTL, stale audit rows are pruned at startup, and closed assignments are reconciled out of active runtime state
  - validation-gate decisions are explicit (`accept`/`retry`/`rework`/`fail`) before delegated output is marked final
  - automated validation path exists via `/handoff auto` with bounded retry/fail outcomes and auditable report text
  - `/stopall` provides emergency delegated worker termination and revokes active assignments to avoid continued background token spend
  - topology bounds are enforced for delegated handoffs (`topology.mode`, orchestrator/dependent limits, `max_handoff_depth`)
- `Missing` skills loading/execution runtime path.
- `Partial` delegated orchestrator execution loop:
  - one-turn dependent execution baseline is active via event-driven handoff workers
  - full topology-aware multi-agent execution loop remains pending
  References:
  - schema fields: `crates/tengu-core/src/config/schema.rs`
  - policy helpers: `crates/tengu-core/src/config/policy.rs`
  - runtime execution gap (multi-agent path not yet active): `src/main.rs` + `src/runtime_engine.rs` + `src/runtime_bus.rs`

Required tasks:
- `E6-T9` (continue: full topology-aware orchestration loop)
- `E6-T12` (expand automated validator runner coverage beyond baseline checks)
- `E6-T13` (dependency barriers and deterministic failure propagation)

---

## US-012

As a platform engineer, I want runtime behavior to be adapter-first and event-driven so integrations stay plug-and-play while side-effects remain traceable and easy to review.

Acceptance targets:
1. Providers/channels/tools/refiners integrate only via shared adapter traits.
2. Runtime lifecycle activity is emitted as typed domain events.
3. Audit/metrics/policy reactions run as event subscribers instead of tightly coupled inline calls.
4. Event handling remains bounded and deterministic on single-core devices, and can scale with parallel subscribers on multi-core machines.

Current coverage:
- `Implemented` adapter boundaries via core traits (`Engine`, `Pipe`, `Refiner`, `Tool`).
- `Implemented` `DomainEvent`/`EventBus` core contracts in `tengu-core`.
- `Implemented` bounded in-process event bus with explicit overflow policies.
- `Implemented` runtime domain-event emission wiring for lifecycle hotspots.
- `Implemented` tool-audit subscriber persistence from tool lifecycle events.
- `Implemented` metrics/policy subscriber side-effects and event-bus lag diagnostics.
- `Implemented` profile-aware backpressure behavior and validation for minimal vs desktop/cloud runtime modes.
- `Partial` stream/event contract hardening is still tracked under schema evolution.

Required tasks:
- `E10-T2` (stream/event contract hardening)

---

## Scenario Focus: Product-Team Cluster (Reference Case)

Scenario (topology options):
1. Single-orchestrator (v1):
- One orchestrator: high-capacity hosted model
- Dependents: hardware/software/marketing/product
2. Single-orchestrator with grouped dependents (v1):
- Engineering + Product + Marketing dependent pools
- All cross-pool communication goes through orchestrator policy
3. Advanced multi-controller topology:
- Deferred until single-orchestrator path is stable and validated

Model mix example:
- Orchestrator: high-capacity hosted model
- Hardware specialist: cost-efficient/free model
- Software specialist: high-reliability coding model
- Marketing specialist: low-cost copy/research model
- Optional optimizer: local refiner path

What must be true before this scenario is considered production-ready:
1. Multi-agent hierarchy execution loop is implemented.
2. Provider mix works per agent in one runtime.
3. Reporting topology supports one orchestrator returning synthesized results to user.
4. Output reserve and context budgets are aligned per agent backend.
5. Retrieval and tool usage are policy-governed and auditable.
6. Hub/channel runtime supports operational lifecycle controls.
7. Capability governance supports both modes:
   - direct user control
   - delegated orchestrator control within user-defined hard boundaries.
