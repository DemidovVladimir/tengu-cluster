# Event Bus Migration Plan

Date: 2026-02-20
Related epic: `E11` (`EPICS_TASKS.md`, `JIRA_TASKS.md`)

## Implementation Status (2026-02-20)

1. ✅ `E11-T1`: `DomainEvent` v1 schema + `EventBus` trait implemented in `crates/tengu-core/src/events.rs`.
2. ✅ `E11-T2`: bounded in-process bus implemented with explicit overflow policies and tests.
3. ✅ `E11-T3`: chat runtime emitters added for domain lifecycle events.
4. ✅ `E11-T4`: tool audit writes migrated to event subscriber.
5. ✅ `E11-T5`: metrics/policy subscribers and lag/saturation diagnostics implemented.
6. ✅ `E11-T6`: profile-aware event-bus tuning and backpressure validation tests implemented for minimal vs desktop/cloud runtime profiles.

## Goal

Introduce an internal domain event bus to decouple runtime side-effects (audit/metrics/policy reactions) from direct orchestration code while preserving current behavior and deployment portability.

## Non-Negotiables

1. Adapter-first boundaries stay unchanged (`Engine`, `Pipe`, `Refiner`, `Tool`).
2. Event handling must be bounded and deterministic on minimal single-core devices.
3. Multi-core hosts can enable parallel subscribers for higher throughput.
4. Migration is incremental; no big-bang runtime rewrite.
5. New runtime side-effects should not add permanent inline coupling in runtime orchestration modules (`src/main.rs`, `src/runtime_engine.rs`, `src/runtime_commands.rs`); they must target subscriber paths or be explicitly marked as temporary.

## Domain Event v1 (Initial)

1. `InboundTurnReceived`
2. `FlowResolved`
3. `PromptAssembled`
4. `EngineTurnStarted`
5. `EngineTurnCompleted`
6. `EngineTurnFailed`
7. `ToolCallStarted`
8. `ToolCallCompleted`
9. `ToolCallDenied`
10. `FlowCompacted`

## Migration Phases

### Phase 1: Contracts and Bus Core

1. ✅ Define `DomainEvent` enum and payload structs (`E11-T1`).
2. ✅ Define `EventBus` trait (`publish`, `subscribe`) with clear delivery semantics (`E11-T1`).
3. ✅ Implement bounded in-process bus with explicit overflow policy (`E11-T2`).

Exit criteria:
1. Unit tests prove publish/subscribe behavior and overflow policy.
2. No unbounded queue paths.

### Phase 2: Runtime Producers

1. ✅ Emit v1 events from runtime hotspots in `src/main.rs` and `src/runtime_engine.rs` (`E11-T3`).
2. ✅ Keep existing direct side-effects active for parity during transition.

Exit criteria:
1. Existing chat/tool behavior unchanged.
2. Regression suite remains green.

### Phase 3: Subscriber Migration

1. ✅ Move tool audit writing to event subscriber (`E11-T4`).
2. ✅ Add metrics/policy subscribers (`E11-T5`).
3. ✅ Remove redundant inline side-effects for migrated audit path.

Exit criteria:
1. Audit and metrics are subscriber-driven.
2. Runtime path is slimmer with no behavior regression.

### Phase 4: Profile Validation

1. ✅ Validate minimal profile: single worker, bounded queue, deterministic drop behavior (`E11-T6`).
2. ✅ Validate desktop/cloud profile: parallel subscribers and lag diagnostics (`E11-T6`).

Exit criteria:
1. ✅ Minimal profile remains stable under stress.
2. ✅ Multi-core profile shows safe throughput gains.

## Validation Commands

```bash
cargo fmt --all
cargo test --workspace
scripts/check_rust_file_descriptions.sh
```

## Traceability Matrix

| Plan Phase | Jira Task(s) |
|---|---|
| Phase 1 | `E11-T1`, `E11-T2` |
| Phase 2 | `E11-T3` |
| Phase 3 | `E11-T4`, `E11-T5` |
| Phase 4 | `E11-T6` |
