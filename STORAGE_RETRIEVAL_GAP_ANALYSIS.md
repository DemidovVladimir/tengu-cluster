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
- In-memory chat history in CLI loop only:
  - `src/main.rs:126`
- Config schema for flow/store settings:
  - `crates/tengu-core/src/config/schema.rs:141`
  - `crates/tengu-core/src/config/schema.rs:219`
- In-memory KnowledgeStore with relevance scoring + hard budget API:
  - `crates/tengu-memory/src/lib.rs:89`
  - `crates/tengu-memory/src/lib.rs:144`

What does not exist yet:
- Persisted flow/session index and transcript writer
- Atomic flow-index writes + lock discipline
- Runtime prompt assembly buckets with enforced token budgets
- Compaction/pruning execution path
- History-limit and oversized tool-result safety path
- Retrieval integration into runtime loop

---

## Critical Gaps

### P0 (must-have before production)

1. Durable flow persistence (`index.json` + JSONL transcript append)
2. Atomic + lock-safe flow index updates
3. Runtime token-budget assembler (never load full transcript)
4. Compaction triggers (overflow and threshold)
5. Retrieval wired to runtime with hard `max_tokens` enforcement

### P1 (hardening after P0)

1. Retention/rotation policies for flow artifacts
2. Transcript path safety and corruption recovery tooling
3. History-turn limits per flow scope
4. Oversized tool-result trimming/clearing strategy
5. Retrieval telemetry: hit rate, dropped-by-budget, token footprint

---

## Recommended Build Order

1. Flow store foundation (`flows/index.json`, per-flow JSONL, lock + atomic writes)
2. Prompt-budget assembler (system/recent/retrieval/summary + reserved output)
3. Retrieval wiring (`query_with_budget`) into chat loop
4. Compaction path (manual first, then threshold/overflow auto path)
5. Retention + repair + diagnostics commands
