# Tengu Cluster - Epics Backlog and Task Decomposition

This backlog is derived from:
- `TODO(epic-...)` markers in `src/` and `crates/`
- `PRD.md`
- `ARCHITECTURE.md`
- `STORAGE_RETRIEVAL_GAP_ANALYSIS.md`
- `USER_STORIES.md`

Date: 2026-02-22

## Priority Waves

| Wave | Goal | Why |
|---|---|---|
| P0 | Reliability + token safety + runtime retrieval + event-bus foundation | Core business goal: predictable low cost, restart-safe flows, and low-risk architecture evolution |
| P1 | Provider/channel expansion + tool loop + observability | Enables real platform usage |
| P2 | Advanced retrieval/refiner quality + schema evolution | Scale quality/performance after core is stable |

## Cross-Epic Architecture Guardrail

All runtime-facing epics must preserve:
1. Adapter-first integration boundaries via `tengu-core` traits (`Engine`, `Pipe`, `Refiner`, `Tool`).
2. Typed event contracts for lifecycle behavior (`StreamEvent` now, `DomainEvent` bus migration for side-effects).
3. Clear current-vs-target documentation when migration is partial (especially `E11` tasks).

## Progress Snapshot (2026-02-21)

Completed tasks:
1. `E1-T1`
2. `E1-T2`
3. `E1-T3`
4. `E1-T4`
5. `E1-T5`
6. `E1-T6`
7. `E1-T7`
8. `E1-T8`
9. `E1-T9`
10. `E2-T1`
11. `E2-T2`
12. `E2-T3`
13. `E2-T4`
14. `E2-T5`
15. `E2-T6`
16. `E2-T7`
17. `E5-T1`
18. `E5-T2`
19. `E5-T3`
20. `E5-T4`
21. `E5-T5`
22. `E4-T1`
23. `E4-T2`
24. `E4-T4`
25. `E4-T8`
26. `E4-T9`
27. `E10-T1`
28. `E10-T7`
29. `E11-T1`
30. `E11-T2`
31. `E11-T3`
32. `E11-T4`
33. `E11-T5`
34. `E11-T6`
35. `E8-T3`
36. `E8-T4`
37. `E8-T6`
38. `E8-T7`
39. `E6-T14`

Partially completed:
1. `E6` control-plane baseline is live in chat runtime (`/assign`, `/assignments`, `/unassign`, `/assignments clear`, `/stopall`) with bounded delegated checks plus assignment audit replay/cleanup, emergency worker-stop semantics, and topology-aware handoff bounds (`topology.mode`, orchestrator/dependent allow-list, `max_handoff_depth`) enforced at assignment and execution time; full multi-agent serve/hub lifecycle remains pending.
2. Runtime orchestration internals were split into focused modules (`src/runtime_bus.rs`, `src/runtime_engine.rs`, `src/runtime_commands.rs`, `src/runtime_prompt.rs`) to reduce `src/main.rs` complexity.

## Epic Status Snapshot

| Epic | Status | Notes |
|---|---|---|
| `E1` | Done | All P0 storage/session resilience tasks are complete. |
| `E2` | Done | Prompt budgeting + retrieval wiring + regressions are complete, including output-reserve alignment to engine output caps. |
| `E3` | Planned | Retrieval persistence/ranking/telemetry backlog. |
| `E4` | In Progress | Anthropic + OpenAI + Claude Code + Hugging Face backends are implemented with overrideable context/output defaults; Google backend and runtime switching are pending. |
| `E5` | Done | Capability contract, Ollama streaming, usage accounting, backend diagnostics, and stream fixtures are complete. |
| `E6` | In Progress | Chat runtime now has delegated orchestrator control-plane baseline (`/assign`, `/assignments`, `/unassign`, `/assignments clear`, `/stopall`) with user-boundary checks, handoff events (including queue-level `Accepted` acknowledgements plus one-turn dependent `Completed`/`Failed` execution results), persisted assignment audit lifecycle events (approved/completed/failed/denied/revoked/expired), startup replay of non-expired approved assignments, runtime TTL cleanup, startup audit-log retention pruning, terminal assignment reconciliation, and emergency delegated worker-stop behavior. Topology-aware delegated handoff bounds are enforced (`topology` orchestrator/dependent/depth policy). Multi-pipe lifecycle and full multi-agent execution loop remain pending. |
| `E7` | Planned | Routing reload/diagnostics pending, including role/capability-aware agent graph routing. |
| `E8` | Done | Runtime assembles tool-call events, executes registry tools (`read_file`) with policy/path guards, enforces config-driven approvals, applies pre-execution allow/deny gates, persists tool audit JSONL events (with compact argument/result previews), defines typed inter-agent handoff task/result envelopes, and enforces capability governance in runtime (`user` vs `delegated` actor gate) plus tool/skill/engine handoff policy checks. |
| `E9` | Planned | Candle/refiner acceleration backlog pending. |
| `E10` | In Progress | Cross-field and governance-boundary validation are implemented; stream/schema migration and release gating remain. |
| `E11` | Done | Adapter contracts, stream events, bounded bus, runtime emitters, audit/metrics/policy subscribers, and profile/backpressure validation are complete. |

## Epic E1: Flow Persistence and Session Resilience (P0)

Mapped TODO tags:
`epic-flow-persistence`, `epic-history-limits`, `epic-compaction`, `epic-runtime-budgets`, `epic-doctor`

Scope:
1. Design flow storage layout: `flows/index.json`, `flows/<flow_key>/transcript-*.jsonl`, compaction artifacts.
2. Implement flow index loader/writer with startup recovery path.
3. Implement transcript append writer with rotation policy.
4. Add atomic index write discipline (temp file + rename).
5. Add lock discipline for concurrent index updates.
6. Implement history turn limits by flow scope.
7. Implement compaction trigger path (threshold + overflow).
8. Implement oversized tool-result guard/truncation strategy.
9. Extend `tengu doctor` with storage integrity checks and repair guidance.

Acceptance criteria:
1. Flow recovery works by `flow_key` after restart.
2. Prompt assembly never reads unbounded full transcript.
3. Index remains valid after interrupted writes.
4. Storage diagnostics report corruption with actionable next steps.

Validation strategy:
1. Restart/restore test for persisted flow history.
2. Atomic write robustness test (append stress keeps valid JSON index).
3. Lock discipline concurrency test (parallel writers serialize without corruption).
4. Session rotation check via `/reset`.
5. Doctor checks for flow store (readability, permissions, recovery hints).

Primary files:
`src/main.rs`, `crates/tengu-core/src/config/schema.rs`, `crates/tengu-core/src/lib.rs`

## Epic E2: Prompt Budgeting and Runtime Retrieval Wiring (P0)

Mapped TODO tags:
`epic-prompt-budgeting`, `epic-runtime-retrieval`, `epic-runtime-budgets`

Scope:
1. Implement bucketed prompt assembler: system, recent, retrieval, summaries, reserved output.
2. Wire runtime enforcement for limits (`max_context_tokens`, reserves, hard caps).
3. Integrate `KnowledgeStore::query_with_budget` into chat loop.
4. Define deterministic drop order for over-budget conditions.
5. Add per-request bucket budget metrics.
6. Add regression tests for budget overflow scenarios.
7. Align reserved output budget to engine output caps for large-context providers.

Acceptance criteria:
1. Runtime enforces configured caps for every request.
2. Retrieval context is token-bounded and lens-aware in live loop.
3. Over-budget behavior degrades predictably and deterministically.

Validation strategy:
1. Budget-guard tests for small/medium/large context windows.
2. Verify reserved output margin remains available.
3. Verify newest messages are prioritized under hard budgets.
4. Verify retrieval bucket never exceeds configured cap.
5. Regression suite: `cargo test --workspace`.

Primary files:
`src/main.rs`, `crates/tengu-core/src/config/schema.rs`, `crates/tengu-optimizer/src/noop/mod.rs`

## Epic E3: Retrieval Quality and Index Persistence (P1)

Mapped TODO tags:
`epic-retrieval-persistence`, `epic-retrieval-chunking`, `epic-retrieval-citations`, `epic-retrieval-telemetry`, `epic-retrieval-ranking`, `epic-retrieval-packing`

Scope:
1. Add persisted metadata/index for knowledge to avoid full re-index on startup.
2. Implement chunk-level indexing for large files.
3. Add citation metadata (path + line/offset) to retrieval output.
4. Improve ranking from heuristics to lexical baseline (TF-IDF/BM25).
5. Evaluate improved packing strategy under token budget.
6. Add retrieval metrics: hit-rate, dropped-by-budget, avg selected tokens.
7. Add latency and quality-drift benchmarks.

Primary files:
`crates/tengu-memory/src/lib.rs`, `crates/tengu-optimizer/src/rules/mod.rs`

## Epic E4: Backend Provider Expansion (P1)

Mapped TODO tags:
`epic-multi-engine`, `epic-backend-anthropic`, `epic-backend-openai`, `epic-backend-google`, `epic-backend-huggingface`, `epic-backend-candle-local`

Scope:
1. Implement typed REST backend for Anthropic.
2. Implement typed REST backend for OpenAI.
3. Implement typed REST backend for Google Gemini.
4. Implement Hugging Face backend via Inference Providers OpenAI-compatible API path (`/v1/chat/completions`) with `HF_TOKEN` auth.
5. Add runtime engine selection/switching.
6. Define feasibility/scope for optional `candle-local` backend.
7. Add provider integration tests (mocked + smoke).
8. Enforce model-aware context/output fallbacks plus config overrides on all provider backends.
9. Implement Claude Code backend path via subprocess/auth profile.
10. Add Hugging Face model selector policy support (`:fastest`, `:cheapest`, `:preferred`, explicit provider-id suffix such as `:sambanova`) and document runtime semantics.
11. Add Hugging Face model discovery path (`GET /v1/models`) with fallback static list for offline/credential-missing scenarios.
12. Add Hugging Face endpoint strategy support (router default + optional dedicated endpoint override).

Primary files:
`src/main.rs`, `crates/tengu-backends/src/lib.rs`, `crates/tengu-backends/src/ollama/mod.rs`

## Epic E5: Backend Capability, Streaming, and Usage Telemetry (P1)

Mapped TODO tags:
`epic-backend-capabilities`, `epic-backend-ollama-streaming`, `epic-backend-usage-accounting`, `epic-backend-telemetry`

Scope:
1. Add backend capability discovery contract (context window, tools, streaming).
2. Implement true Ollama streaming on `StreamEvent::TextDelta`.
3. Standardize usage accounting across providers.
4. Add backend diagnostics metadata for runtime/status.
5. Add fixtures for stream ordering and terminal states.

Primary files:
`crates/tengu-backends/src/ollama/mod.rs`, `crates/tengu-core/src/lib.rs`

## Epic E6: Channels Runtime, Policies, and Hub Mode (P1)

Mapped TODO tags:
`epic-hub-runtime`, `epic-channel-lifecycle`, `epic-channel-streaming`, `epic-channel-cli-media`, `epic-pipe-policies`, `epic-channel-telegram`, `epic-channel-discord`, `epic-channel-webchat`, `epic-channel-ack`

Scope:
1. Implement long-running hub/serve runtime with multi-pipe orchestration.
2. Add channel lifecycle task management (start/stop/health).
3. Add inbound access policy middleware.
4. Implement Telegram adapter (`teloxide`).
5. Implement Discord adapter (`serenity`/`twilight`).
6. Implement WebChat adapter.
7. Add streaming output support for channel responders.
8. Add optional delivery ack contract where supported.
9. Implement topology-aware multi-agent execution loop (single-orchestrator with flexible dependents).
10. Add orchestrator control plane for dependent capability assignment within user-defined hard boundaries.
11. Add delegated-result validation gate contract (`accept`/`retry`/`rework`/`fail`) before dependent outputs can be reused.
12. Implement orchestrator validation policy runner (role-aware checks: compile/test/lint/schema/tool checks) with bounded retries/fallback.
13. Add dependency-aware execution barriers so dependent tasks consume only validated upstream artifacts and receive deterministic failure propagation.
14. Add emergency delegated execution stop (`/stopall`) to cancel delegated workers and revoke active assignments when user needs immediate token-spend cutoff.

Primary files:
`src/main.rs`, `crates/tengu-channels/src/lib.rs`, `crates/tengu-channels/src/cli/mod.rs`, `crates/tengu-core/src/lib.rs`

## Epic E7: Routing Operations and Diagnostics (P1)

Mapped TODO tags:
`epic-routing-hot-reload`, `epic-routing-observability`

Scope:
1. Add lock-safe runtime reload for routing bindings.
2. Add structured match traces (what matched and why).
3. Expose routing diagnostics via status/doctor.
4. Add route resolution tests for precedence and conflicts.
5. Add role/capability-aware routing for orchestrator/dependent execution graph.

Primary files:
`crates/tengu-core/src/routing/mod.rs`, `src/main.rs`

## Epic E8: Tool Loop and Safety Controls (P1)

Mapped TODO tags:
`epic-tool-loop`, `epic-tool-policy`, `epic-tool-audit`, `epic-tool-security`

Scope:
1. Implement runtime tool-call lifecycle (`ToolCallStart/Delta/End`).
2. Add tool registry and executor wiring.
3. Apply per-tool policy metadata and approvals.
4. Add allow/deny checks before tool execution.
5. Add audit trail for tool calls/results.
6. Define and implement inter-agent handoff contract (task/result envelopes).
7. Enforce capability policies for tools/skills/engines at runtime.

Primary files:
`src/main.rs`, `crates/tengu-core/src/types/stream.rs`, `crates/tengu-core/src/types/message.rs`

## Epic E9: Refiner and Candle Acceleration (P1)

Mapped TODO tags:
`epic-refiner-candle`, `epic-refiner-candle-gpu`, `epic-refiner-remote`, `epic-refiner-selection`, `epic-refiner-observability`

Scope:
1. Implement `CandleRefiner` behind feature flag `candle`.
2. Add CUDA/Metal selection with CPU fallback.
3. Add remote refiner client mode.
4. Add profile-based runtime refiner selection with config override.
5. Add benchmark suite for compression/summarization quality.
6. Improve hardware capability probing for GPU backends.
7. Add latency/quality telemetry hooks for refiner.

Primary files:
`crates/tengu-optimizer/src/lib.rs`, `crates/tengu-core/src/config/profile.rs`

## Epic E10: Contracts, Config Validation, and Schema Evolution (P2)

Mapped TODO tags:
`epic-config-validation`, `epic-stream-contract`, `epic-message-schema`, `epic-media-storage`

Scope:
1. Implement cross-field config validation with clear errors.
2. Define strict stream event contract and terminal guarantees.
3. Extend message schema for richer structured content.
4. Add externalized media storage references for large payloads.
5. Add compatibility tests for schema migration.
6. Add user-story acceptance matrix and release checklist as a required quality gate.
7. Add config/runtime validation for capability governance boundaries.

Primary files:
`crates/tengu-core/src/config/mod.rs`, `crates/tengu-core/src/types/stream.rs`, `crates/tengu-core/src/types/message.rs`, `USER_STORIES.md`

## Epic E11: Internal Event Bus and Runtime Decoupling (P0)

Mapped TODO tags:
`epic-event-bus-core`, `epic-event-bus-runtime`, `epic-event-bus-subscribers`, `epic-event-bus-backpressure`, `epic-event-bus-observability`

Scope:
1. Define typed `DomainEvent` contract for runtime lifecycle (inbound turn, engine turn, tool lifecycle, flow events, policy outcomes).
2. Add `EventBus` abstraction and in-process implementation (`tokio` fanout) with bounded queues.
3. Publish runtime events from existing chat path without breaking current behavior.
4. Move audit/metrics/policy side-effects to event subscribers incrementally.
5. Add explicit backpressure/drop-policy behavior for minimal single-core deployments.
6. Add optional parallel subscriber execution profile for multi-core hosts.
7. Add diagnostics for subscriber lag, dropped events, and queue saturation.

Acceptance criteria:
1. Current runtime behavior remains stable while side-effects are served by subscribers.
2. Event delivery and terminal state handling are deterministic under capacity pressure.
3. Minimal profile remains bounded (no unbounded queues, no deadlocks).
4. Desktop/cloud profiles can enable higher subscriber parallelism safely.

Validation strategy:
1. Unit tests for event ordering, overflow handling, and terminal guarantees.
2. Integration tests proving audit subscriber receives full tool lifecycle payloads.
3. Profile tests for minimal (single worker) and desktop (parallel workers) runtime modes.
4. Regression suite: `cargo test --workspace`.

Primary files:
`src/main.rs`, `src/tool_audit.rs`, `crates/tengu-core/src/lib.rs`, `crates/tengu-core/src/types/stream.rs`
