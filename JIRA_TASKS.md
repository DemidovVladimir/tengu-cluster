# Jira-Style Task Backlog

Date: 2026-02-17
Source: `EPICS_TASKS.md`

## Status Snapshot (2026-02-17)

| Key | Status | Comment |
|---|---|---|
| E1-T1 | Done | Flow storage layout implemented in runtime |
| E1-T2 | Done | Flow index loader/writer implemented |
| E1-T3 | Done | Transcript append writer implemented |
| E1-T4 | Done | Atomic index writes implemented |
| E1-T5 | Done | Lock discipline implemented |
| E2-T1 | Done | Budget-safe recent-history assembler implemented |
| E2-T2 | Done | Hard runtime budget guards implemented |
| E2-T3 | Done | Retrieval bucket wired through `query_with_budget` in chat loop |
| E2-T4 | Done | Deterministic over-budget drop order implemented (history suffix + retrieval tail-drop) |
| E1-T9 | In Progress | Base flow-store check in doctor is done; repair checks still pending |
| E2-T5 | In Progress | Base `/context` bucket report exists; full per-request telemetry pending |

## Epics

| Key | Type | Summary | Priority |
|---|---|---|---|
| E1 | Epic | Flow Persistence and Session Resilience | P0 |
| E2 | Epic | Prompt Budgeting and Runtime Retrieval Wiring | P0 |
| E3 | Epic | Retrieval Quality and Index Persistence | P1 |
| E4 | Epic | Backend Provider Expansion | P1 |
| E5 | Epic | Backend Capability, Streaming, and Usage Telemetry | P1 |
| E6 | Epic | Channels Runtime, Policies, and Hub Mode | P1 |
| E7 | Epic | Routing Operations and Diagnostics | P1 |
| E8 | Epic | Tool Loop and Safety Controls | P1 |
| E9 | Epic | Refiner and Candle Acceleration | P1 |
| E10 | Epic | Contracts, Config Validation, and Schema Evolution | P2 |

## Epic Closure Rules

1. A task moves to `Done` only with code + docs update + validation evidence.
2. An epic moves to `Done` only when all acceptance criteria and validation checks in `EPICS_TASKS.md` are closed.
3. Evidence must include exact commands and outcomes (for example, `cargo test --workspace` pass).

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

## Epic E7 Tasks

| Key | Type | Description | Priority | Dependencies | Sprint |
|---|---|---|---|---|---|
| E7-T1 | Task | Add lock-safe runtime reload for routing bindings | P1 | E6-T1 | Sprint 4 |
| E7-T2 | Task | Add structured routing match traces | P1 | E7-T1 | Sprint 4 |
| E7-T3 | Task | Expose routing diagnostics via status/doctor | P1 | E7-T2 | Sprint 5 |
| E7-T4 | Task | Add route precedence/conflict resolution tests | P1 | E7-T1 | Sprint 5 |

## Epic E8 Tasks

| Key | Type | Description | Priority | Dependencies | Sprint |
|---|---|---|---|---|---|
| E8-T1 | Task | Implement tool-call lifecycle loop (`ToolCallStart/Delta/End`) | P1 | E5-T2 | Sprint 3 |
| E8-T2 | Task | Add tool registry and executor wiring | P1 | E8-T1 | Sprint 3 |
| E8-T3 | Task | Apply per-tool policy metadata + approvals | P1 | E8-T2 | Sprint 5 |
| E8-T4 | Task | Add allow/deny policy checks before tool execution | P1 | E8-T2 | Sprint 5 |
| E8-T5 | Task | Add audit trail for tool calls/results | P1 | E8-T2 | Sprint 5 |

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
