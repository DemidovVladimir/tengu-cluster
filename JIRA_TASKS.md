# Jira-Style Task Backlog

Date: 2026-02-21
Source: `EPICS_TASKS.md`

## Status Snapshot (2026-02-21)

| Key | Status | Comment |
|---|---|---|
| E1-T1 | Done | Flow storage layout implemented in runtime |
| E1-T2 | Done | Flow index loader/writer implemented |
| E1-T3 | Done | Transcript append writer implemented |
| E1-T4 | Done | Atomic index writes implemented |
| E1-T5 | Done | Lock discipline implemented |
| E1-T6 | Done | Scope-aware history turn limits enforced in runtime (`flow.max_history_turns` + defaults) |
| E1-T7 | Done | Runtime compaction trigger path implemented (threshold + overflow with summary compaction) |
| E1-T8 | Done | Core tool-result token guard/truncation utility implemented with regression tests |
| E2-T1 | Done | Budget-safe recent-history assembler implemented |
| E2-T2 | Done | Hard runtime budget guards implemented |
| E2-T3 | Done | Retrieval bucket wired through `query_with_budget` in chat loop |
| E2-T4 | Done | Deterministic over-budget drop order implemented (history suffix + retrieval tail-drop) |
| E1-T9 | Done | `doctor` now runs flow index + transcript integrity diagnostics with actionable findings |
| E2-T5 | Done | Per-request prompt budget telemetry now emitted by runtime (bucket-level tracing + `/context` snapshot) |
| E2-T6 | Done | Added budget overflow regression tests for small windows, reserves, and bucket hard-caps |
| E2-T7 | Done | Prompt reserve now aligns to engine output cap (not raw context fraction), avoiding over-reserve on large-context models |
| E5-T1 | Done | Engine capability discovery contract added in `tengu-core`; surfaced in runtime engine/banner output |
| E5-T2 | Done | Ollama backend now emits true streamed `TextDelta` events with terminal usage/done frames |
| E5-T3 | Done | Usage accounting standardized as cumulative turn snapshots with single-apply runtime aggregation |
| E5-T4 | Done | Backend diagnostics metadata contract added and surfaced in `status`/`doctor`/`/engine` |
| E5-T5 | Done | Added stream ordering/terminal-state fixtures for success and parse-error paths in Ollama backend tests |
| E4-T1 | Done | Anthropic backend implemented via typed REST and wired into runtime engine selection |
| E4-T2 | Done | OpenAI backend implemented via typed REST and wired into runtime engine selection |
| E4-T9 | Done | Claude Code backend implemented via subprocess (`claude --print`) and wired into runtime engine selection |
| E4-T8 | Done | Added provider context/output fallback strategy + per-agent overrides (`context_window_override`, `max_output_tokens_per_turn`) for implemented backends |
| E10-T1 | Done | Added cross-field config validation with actionable aggregated errors and fail-fast startup loading |
| E10-T7 | Done | Added governance boundary validation for `kit`/`allowed_engines`/`skills`/sandbox + routing/pipe consistency |
| E6-T10 | In Progress | Added delegated orchestrator control-plane baseline in chat runtime (`/assign`, `/assignments`) with user-boundary checks, handoff lifecycle events, and persisted assignment audit JSONL; full multi-agent execution loop integration remains pending under `E6-T9` |
| E8-T1 | In Progress | Runtime now consumes `ToolCallStart/Delta/End` events with pending-call assembly and validation guards |
| E8-T2 | In Progress | Added runtime tool registry/executor wiring with built-in `read_file` and workspace path-safety checks |
| E8-T3 | Done | Added per-tool policy metadata (`risk_level`, `requires_approval`) and config-driven approval gates (`kit.approval_required`, `kit.approved`) before tool execution |
| E8-T4 | Done | Enforced allow/deny policy before execution (defense-in-depth gate in `execute_tool_call`) with denial event path and regression tests |
| E8-T6 | Done | Added typed inter-agent handoff task/result envelopes with schema validation and domain-event payload integration for auditable dispatch/result lifecycle |
| E8-T5 | In Progress | Added append-only tool audit trail (`state/audit/tool_calls.jsonl`) persisted from tool lifecycle domain-event subscriber |
| E8-T7 | Done | Runtime now enforces capability-governance actor gate (`user` vs `delegated`) for direct capability requests, plus existing engine/tool/skill/handoff policy checks; delegated multi-agent execution loop remains tracked under `E6-T10` |
| E11-T1 | Done | `DomainEvent` v1 schema + `EventBus` trait added in `tengu-core::events` |
| E11-T2 | Done | `InProcessEventBus` implemented with `DropNewest`/`DropOldest`/`BlockProducer` and overflow behavior tests |
| E11-T3 | Done | Chat runtime now emits `DomainEvent` lifecycle events for inbound/flow/prompt/engine/tool/compaction hotspots |
| E11-T4 | Done | Tool audit persistence migrated to event subscriber fed by tool lifecycle `DomainEvent`s |
| E11-T5 | Done | Added metrics + policy subscribers and periodic lag/saturation diagnostics from event bus counters |
| E11-T6 | Done | Added profile-aware event-bus runtime tuning and validation tests for minimal (`DropNewest`) vs desktop/cloud (`DropOldest`) backpressure behavior |

## Completed Start Task (2026-02-20)

- `E5-T5` — Add stream ordering + terminal-state fixtures.
- `E4-T1` — Implement Anthropic backend via typed REST.
- `E4-T2` — Implement OpenAI backend via typed REST.
- `E4-T9` — Implement Claude Code backend via subprocess/auth profile.
- `E4-T8` — Add provider capability/output fallback + per-agent override contract.
- `E2-T7` — Align runtime output reserve with engine output cap.
- `E10-T1` — Implement cross-field config validation.
- `E10-T7` — Add config/runtime validation for capability governance boundaries.
- `E11-T1` — Define runtime `DomainEvent` schema + `EventBus` abstraction.
- `E11-T2` — Implement bounded in-process event bus + overflow policy tests.
- `E11-T3` — Emit runtime domain events from chat path without behavior regressions.
- `E11-T4` — Migrate tool audit writes to subscriber handler (event-driven side-effect).
- `E11-T5` — Add metrics/policy subscribers and lag/saturation diagnostics.
- `E11-T6` — Validate minimal single-core and multi-core profiles for queue/backpressure behavior.
- `E8-T3` — Apply per-tool policy metadata + approvals.
- `E8-T4` — Add allow/deny policy checks before tool execution.
- `E8-T6` — Define and implement inter-agent handoff contract (task/result envelopes).
- `E8-T7` — Enforce capability policies for tools/skills/engine usage at runtime (user + delegated orchestrator modes).

## Next Start (2026-02-21)

- `E6-T10` — Continue orchestrator control plane: persist delegated assignments and connect execution loop with `E6-T9`.

## Epics

| Key | Type | Summary | Priority | Status |
|---|---|---|---|---|
| E1 | Epic | Flow Persistence and Session Resilience | P0 | Done |
| E2 | Epic | Prompt Budgeting and Runtime Retrieval Wiring | P0 | Done |
| E3 | Epic | Retrieval Quality and Index Persistence | P1 | Planned |
| E4 | Epic | Backend Provider Expansion | P1 | In Progress |
| E5 | Epic | Backend Capability, Streaming, and Usage Telemetry | P1 | Done |
| E6 | Epic | Channels Runtime, Policies, and Hub Mode | P1 | In Progress |
| E7 | Epic | Routing Operations and Diagnostics | P1 | Planned |
| E8 | Epic | Tool Loop and Safety Controls | P1 | In Progress |
| E9 | Epic | Refiner and Candle Acceleration | P1 | Planned |
| E10 | Epic | Contracts, Config Validation, and Schema Evolution | P2 | In Progress |
| E11 | Epic | Internal Event Bus and Runtime Decoupling | P0 | Done |

## Epic Closure Rules

1. A task moves to `Done` only with code + docs update + validation evidence.
2. An epic moves to `Done` only when all acceptance criteria and validation checks in `EPICS_TASKS.md` are closed.
3. Evidence must include exact commands and outcomes (for example, `cargo test --workspace` pass).
4. Architecture conformance must be shown for runtime changes:
   - adapter boundary preserved (`Engine`/`Pipe`/`Refiner`/`Tool`)
   - event contract preserved (`StreamEvent`/`DomainEvent`)
   - no new long-lived inline side-effects without `E11` migration task linkage

## Epic E1 Tasks

| Key | Type | Description | Priority | Dependencies | Sprint |
|---|---|---|---|---|---|
| E1-T1 | Task | Design flow storage layout (`flows/index.json`, transcripts, compacted artifacts) | P0 | - | Sprint 1 |
| E1-T2 | Task | Implement flow index loader/writer with startup recovery | P0 | E1-T1 | Sprint 1 |
| E1-T3 | Task | Implement transcript append writer with rotation policy | P0 | E1-T1 | Sprint 1 |
| E1-T4 | Task | Add atomic write discipline for index updates | P0 | E1-T2 | Sprint 1 |
| E1-T5 | Task | Add lock discipline for concurrent index updates | P0 | E1-T2 | Sprint 1 |
| E1-T6 | Task | Implement history turn limits by flow scope | P0 | E1-T2 | Sprint 2 |
| E1-T7 | Task | Implement compaction trigger path (threshold + overflow) | P0 | E1-T3,E1-T6 | Sprint 2 |
| E1-T8 | Task | Implement oversized tool-result guard/truncation strategy | P0 | E1-T6 | Sprint 2 |
| E1-T9 | Task | Extend `tengu doctor` with storage integrity checks | P0 | E1-T2,E1-T3 | Sprint 2 |

## Epic E2 Tasks

| Key | Type | Description | Priority | Dependencies | Sprint |
|---|---|---|---|---|---|
| E2-T1 | Task | Implement bucketed prompt assembler | P0 | E1-T1 | Sprint 1 |
| E2-T2 | Task | Wire hard budget enforcement from config into runtime | P0 | E2-T1 | Sprint 1 |
| E2-T3 | Task | Integrate `KnowledgeStore::query_with_budget` into chat loop | P0 | E2-T1 | Sprint 2 |
| E2-T4 | Task | Define deterministic drop order under over-budget conditions | P0 | E2-T2,E2-T3 | Sprint 2 |
| E2-T5 | Task | Add per-request budget metrics by bucket | P0 | E2-T2 | Sprint 2 |
| E2-T6 | Task | Add regression tests for budget overflow scenarios | P0 | E2-T4 | Sprint 2 |
| E2-T7 | Task | Align output reserve with engine output cap and add large-context regression tests | P0 | E2-T2,E5-T1 | Sprint 3 |

## Epic E3 Tasks

| Key | Type | Description | Priority | Dependencies | Sprint |
|---|---|---|---|---|---|
| E3-T1 | Task | Persist knowledge metadata/index to avoid full re-index | P1 | E2-T3 | Post-MVP |
| E3-T2 | Task | Add chunk-level indexing for large files | P1 | E3-T1 | Post-MVP |
| E3-T3 | Task | Add citation metadata (path + line/offset) to retrieval results | P1 | E3-T2 | Post-MVP |
| E3-T4 | Task | Improve ranking to lexical baseline (TF-IDF/BM25) | P1 | E3-T1 | Post-MVP |
| E3-T5 | Task | Evaluate improved budget packing strategy | P1 | E3-T4 | Post-MVP |
| E3-T6 | Task | Add retrieval telemetry (hit-rate, dropped-by-budget, token footprint) | P1 | E3-T1 | Post-MVP |
| E3-T7 | Task | Add retrieval latency/quality benchmarks | P1 | E3-T4,E3-T6 | Post-MVP |

## Epic E4 Tasks

| Key | Type | Description | Priority | Dependencies | Sprint |
|---|---|---|---|---|---|
| E4-T1 | Task | Implement Anthropic backend via typed REST | P1 | E5-T1 | Sprint 3 |
| E4-T2 | Task | Implement OpenAI backend via typed REST | P1 | E5-T1 | Sprint 3 |
| E4-T3 | Task | Implement Google Gemini backend via typed REST | P1 | E5-T1 | Sprint 5 |
| E4-T4 | Task | Implement Hugging Face backend (`hf-hub` + inference path) | P1 | E5-T1 | Sprint 5 |
| E4-T5 | Task | Add runtime engine selection/switching | P1 | E4-T1,E4-T2 | Sprint 3 |
| E4-T6 | Task | Define feasibility/scope for `candle-local` backend | P1 | E9-T1 | Post-MVP |
| E4-T7 | Task | Add provider integration tests (mocked + smoke) | P1 | E4-T1,E4-T2,E4-T3,E4-T4 | Sprint 5 |
| E4-T8 | Task | Add model-aware context/output defaults + config overrides for all provider backends | P1 | E4-T1,E4-T2 | Sprint 3 |
| E4-T9 | Task | Implement Claude Code backend via subprocess with profile-based auth | P1 | E5-T1 | Sprint 4 |

## Epic E5 Tasks

| Key | Type | Description | Priority | Dependencies | Sprint |
|---|---|---|---|---|---|
| E5-T1 | Task | Add backend capability discovery contract | P1 | - | Sprint 2 |
| E5-T2 | Task | Implement true Ollama streaming (`TextDelta`) | P1 | E5-T1 | Sprint 2 |
| E5-T3 | Task | Standardize usage accounting across providers | P1 | E5-T1 | Sprint 3 |
| E5-T4 | Task | Add backend diagnostics metadata for runtime/status | P1 | E5-T1 | Sprint 3 |
| E5-T5 | Task | Add stream ordering + terminal-state fixtures | P1 | E5-T2 | Sprint 3 |

## Epic E6 Tasks

| Key | Type | Description | Priority | Dependencies | Sprint |
|---|---|---|---|---|---|
| E6-T1 | Task | Implement hub/serve runtime with multi-pipe orchestration | P1 | E1-T2 | Sprint 4 |
| E6-T2 | Task | Add channel lifecycle management (start/stop/health) | P1 | E6-T1 | Sprint 4 |
| E6-T3 | Task | Add inbound access policy middleware | P1 | E6-T1 | Sprint 4 |
| E6-T4 | Task | Implement Telegram adapter (`teloxide`) | P1 | E6-T2,E6-T3 | Sprint 4 |
| E6-T5 | Task | Implement Discord adapter (`serenity`/`twilight`) | P1 | E6-T2,E6-T3 | Post-MVP |
| E6-T6 | Task | Implement WebChat adapter | P1 | E6-T2,E6-T3 | Post-MVP |
| E6-T7 | Task | Add streaming output support for channel responders | P1 | E5-T2,E6-T2 | Sprint 5 |
| E6-T8 | Task | Add optional channel delivery ack contract | P1 | E6-T2 | Sprint 5 |
| E6-T9 | Task | Implement topology-aware multi-agent execution loop (single-orchestrator with flexible dependents) | P1 | E6-T1,E7-T1,E8-T2 | Sprint 5 |
| E6-T10 | Task | Add orchestrator control plane for dependent capability assignment with user-boundary checks | P1 | E6-T9,E8-T7,E10-T7 | Sprint 5 |

## Epic E7 Tasks

| Key | Type | Description | Priority | Dependencies | Sprint |
|---|---|---|---|---|---|
| E7-T1 | Task | Add lock-safe runtime reload for routing bindings | P1 | E6-T1 | Sprint 4 |
| E7-T2 | Task | Add structured routing match traces | P1 | E7-T1 | Sprint 4 |
| E7-T3 | Task | Expose routing diagnostics via status/doctor | P1 | E7-T2 | Sprint 5 |
| E7-T4 | Task | Add route precedence/conflict resolution tests | P1 | E7-T1 | Sprint 5 |
| E7-T5 | Task | Add role/capability-aware routing for orchestrator/dependent execution graph | P1 | E7-T1,E6-T9 | Sprint 5 |

## Epic E8 Tasks

| Key | Type | Description | Priority | Dependencies | Sprint |
|---|---|---|---|---|---|
| E8-T1 | Task | Implement tool-call lifecycle loop (`ToolCallStart/Delta/End`) | P1 | E5-T2 | Sprint 3 |
| E8-T2 | Task | Add tool registry and executor wiring | P1 | E8-T1 | Sprint 3 |
| E8-T3 | Task | Apply per-tool policy metadata + approvals | P1 | E8-T2 | Sprint 5 |
| E8-T4 | Task | Add allow/deny policy checks before tool execution | P1 | E8-T2 | Sprint 5 |
| E8-T5 | Task | Add audit trail for tool calls/results | P1 | E8-T2 | Sprint 5 |
| E8-T6 | Task | Define and implement inter-agent handoff contract (task/result envelopes) | P1 | E8-T1,E8-T2,E6-T9 | Sprint 5 |
| E8-T7 | Task | Enforce capability policies for tools/skills/engine usage at runtime (user + delegated orchestrator modes) | P1 | E8-T2,E10-T7 | Sprint 5 |

## Epic E9 Tasks

| Key | Type | Description | Priority | Dependencies | Sprint |
|---|---|---|---|---|---|
| E9-T1 | Task | Implement `CandleRefiner` behind feature `candle` | P1 | - | Post-MVP |
| E9-T2 | Task | Add CUDA/Metal selection + CPU fallback | P1 | E9-T1 | Post-MVP |
| E9-T3 | Task | Add remote refiner client mode | P1 | E9-T1 | Post-MVP |
| E9-T4 | Task | Add profile-based refiner selection + override | P1 | E9-T1,E9-T2 | Post-MVP |
| E9-T5 | Task | Add refiner quality benchmark suite | P1 | E9-T1 | Post-MVP |
| E9-T6 | Task | Improve GPU capability probing for profile detection | P1 | E9-T2 | Post-MVP |
| E9-T7 | Task | Add refiner latency/quality telemetry hooks | P1 | E9-T1,E9-T5 | Post-MVP |

## Epic E10 Tasks

| Key | Type | Description | Priority | Dependencies | Sprint |
|---|---|---|---|---|---|
| E10-T1 | Task | Implement cross-field config validation | P2 | E1-T2,E2-T2 | Post-MVP |
| E10-T2 | Task | Define strict stream contract and terminal guarantees | P2 | E5-T5 | Post-MVP |
| E10-T3 | Task | Extend message schema for richer structured content | P2 | E10-T1 | Post-MVP |
| E10-T4 | Task | Add externalized media storage references | P2 | E10-T3 | Post-MVP |
| E10-T5 | Task | Add schema migration compatibility tests | P2 | E10-T1,E10-T3 | Post-MVP |
| E10-T6 | Task | Add user-story acceptance matrix and scenario-driven release checklist | P2 | E10-T1 | Sprint 4 |
| E10-T7 | Task | Add config/runtime validation for capability governance (`kit`, `skills`, `allowed_engines`, sandbox boundaries) | P2 | E10-T1 | Sprint 4 |

## Epic E11 Tasks

| Key | Type | Description | Priority | Dependencies | Sprint |
|---|---|---|---|---|---|
| E11-T1 | Task | Define `DomainEvent` schema + `EventBus` trait for runtime lifecycle events | P0 | E5-T5,E10-T2 | Sprint 4 |
| E11-T2 | Task | Implement bounded in-process event bus with explicit overflow policy | P0 | E11-T1 | Sprint 4 |
| E11-T3 | Task | Emit domain events from chat runtime path (`src/main.rs`) without behavior regressions | P0 | E11-T2 | Sprint 4 |
| E11-T4 | Task | Migrate tool audit writes to subscriber handler (event-driven side-effect) | P0 | E11-T3,E8-T5 | Sprint 4 |
| E11-T5 | Task | Add metrics/policy subscribers and lag/saturation diagnostics | P0 | E11-T3 | Sprint 5 |
| E11-T6 | Task | Validate minimal single-core and multi-core profiles for queue/backpressure behavior | P0 | E11-T2,E11-T5 | Sprint 5 |
