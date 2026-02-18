# Storage & Retrieval Gap Analysis (Tengu vs OpenClaw)

## Scope

Comparison focus:
- Session/flow storage durability and safety
- Prompt-context token control paths
- Retrieval quality/cost controls

Compared codebases:
- Tengu codebase: this repository (`tengu-cluster`)
- OpenClaw baseline: `/Users/vladimirdemidov/development/open_claw/openclaw`

---

## OpenClaw Baseline (Relevant Capabilities)

Storage and session lifecycle:
- Session store maintenance with pruning/capping/rotation:
  - `/Users/vladimirdemidov/development/open_claw/openclaw/src/config/sessions/store.ts:301`
  - `/Users/vladimirdemidov/development/open_claw/openclaw/src/config/sessions/store.ts:371`
  - `/Users/vladimirdemidov/development/open_claw/openclaw/src/config/sessions/store.ts:413`
- Atomic-ish store write path + lock discipline:
  - `/Users/vladimirdemidov/development/open_claw/openclaw/src/config/sessions/store.ts:476`
  - `/Users/vladimirdemidov/development/open_claw/openclaw/src/config/sessions/store.ts:585`
  - `/Users/vladimirdemidov/development/open_claw/openclaw/src/config/sessions/store.ts:712`
- Transcript path validation and containment checks:
  - `/Users/vladimirdemidov/development/open_claw/openclaw/src/config/sessions/paths.ts:57`
  - `/Users/vladimirdemidov/development/open_claw/openclaw/src/config/sessions/paths.ts:115`

Token safety and context control:
- Compaction trigger model and reserve policy docs:
  - `/Users/vladimirdemidov/development/open_claw/openclaw/docs/reference/session-management-compaction.md:174`
  - `/Users/vladimirdemidov/development/open_claw/openclaw/docs/reference/session-management-compaction.md:192`
- Pre-compaction memory flush thresholds:
  - `/Users/vladimirdemidov/development/open_claw/openclaw/src/auto-reply/reply/memory-flush.ts:113`
- TTL-aware session pruning of tool results:
  - `/Users/vladimirdemidov/development/open_claw/openclaw/docs/concepts/session-pruning.md:11`
- Oversized tool-result truncation safeguards:
  - `/Users/vladimirdemidov/development/open_claw/openclaw/src/agents/pi-embedded-runner/tool-result-truncation.ts:138`

History control:
- Per-session-key history limit policy:
  - `/Users/vladimirdemidov/development/open_claw/openclaw/src/agents/pi-embedded-runner/history.ts:15`
  - `/Users/vladimirdemidov/development/open_claw/openclaw/src/agents/pi-embedded-runner/history.ts:43`

---

## Current Tengu State

What exists:
- Persisted flow/session index + JSONL transcript append with lock-safe and atomic writes:
  - `src/flow_store.rs`
- Runtime prompt budget assembly (system/history/retrieval/output reserve):
  - `src/main.rs`
- Runtime retrieval wiring with hard token-capped query path:
  - `src/main.rs`
  - `crates/tengu-memory/src/lib.rs`
- Scope-aware history turn limits enforced in runtime:
  - `src/main.rs`
  - `crates/tengu-core/src/config/schema.rs`
- Runtime compaction trigger path (threshold + overflow) with summary insertion:
  - `src/main.rs`
- `doctor` flow-store integrity diagnostics (missing/unsafe/corrupt transcript checks):
  - `src/main.rs`
  - `src/flow_store.rs`

What does not exist yet:
- Oversized tool-result safety/truncation path
- Retention/rotation jobs for archived flow artifacts
- Automated corruption repair workflow (detection now exists in `doctor`)
- Persisted retrieval index and incremental refresh path

---

## Critical Gaps

### P0 (must-have before production)

1. Oversized tool-result safety/truncation path

### P1 (hardening after P0)

1. Retention/rotation policies for flow artifacts
2. Transcript corruption repair tooling (path safety checks are implemented in diagnostics)
3. Durable compaction artifact lifecycle (archival/retention/repair hooks)
4. Retrieval telemetry: hit rate, dropped-by-budget, token footprint
5. Persisted retrieval index + incremental refresh

---

## Recommended Build Order

1. Flow store foundation (`flows/index.json`, per-flow JSONL, lock + atomic writes)
2. Prompt-budget assembler (system/recent/retrieval/summary + reserved output)
3. Retrieval wiring (`query_with_budget`) into chat loop
4. Oversized tool-result safety path
5. Retention + repair + diagnostics commands
