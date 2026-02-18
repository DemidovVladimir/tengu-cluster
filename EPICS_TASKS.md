# Tengu Cluster - Epics Backlog and Task Decomposition

This backlog is derived from:
- `TODO(epic-...)` markers in `src/` and `crates/`
- `PRD.md`
- `ARCHITECTURE.md`
- `STORAGE_RETRIEVAL_GAP_ANALYSIS.md`

Date: 2026-02-17

## Priority Waves

| Wave | Goal | Why |
|---|---|---|
| P0 | Reliability + token safety + runtime retrieval | Core business goal: predictable low cost and restart-safe flows |
| P1 | Provider/channel expansion + tool loop + observability | Enables real platform usage |
| P2 | Advanced retrieval/refiner quality + schema evolution | Scale quality/performance after core is stable |

## Progress Snapshot (2026-02-17)

Completed tasks:
1. `E1-T1`
2. `E1-T2`
3. `E1-T3`
4. `E1-T4`
5. `E1-T5`
6. `E1-T6`
7. `E1-T7`
8. `E2-T1`
9. `E2-T2`
10. `E2-T3`
11. `E2-T4`

Partially completed:
1. `E1-T9` (base flow-store health check added in `doctor`; repair checks still pending)
2. `E2-T5` (base prompt-bucket report exists in `/context`; full per-request telemetry still pending)

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
4. Implement Hugging Face backend (`hf-hub` + inference path).
5. Add runtime engine selection/switching.
6. Define feasibility/scope for optional `candle-local` backend.
7. Add provider integration tests (mocked + smoke).

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

Primary files:
`crates/tengu-core/src/config/mod.rs`, `crates/tengu-core/src/types/stream.rs`, `crates/tengu-core/src/types/message.rs`
