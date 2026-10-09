# Tengu Control Loop Lab + Studio — implementation plan

**Date:** 2026-10-08  
**Status:** merged to main 2026-10-09 as #44 (squash `f7ac9a5636e2493e3839e896cba9951a2d41aefb`) — ST-00 … ST-03, ST-10, ST-11, ST-12 landed 2026-10-08, ST-20, ST-21, ST-22, ST-30, ST-31, ST-40 (design only) + the operator docs 2026-10-09 (§ 8); live browser validation 2026-10-09 (`docs/studio-2026-10-08.md` § Live validation); Gates 1, 2, 3 and 4 waived; ST-90 clean room passed 2026-10-09 (`docs/studio-clean-room-2026-10-09.md`); Operator Review #3 (editor) open; the operator's 2026-10-08 instruction waives the numbered gates (recorded per gate in § 8). The § 8 Commit column names `feature/studio` branch commits — squashed into #44, not in main's history  
**Audience:** the coding agent that will implement the work and the operator who will review it

> This file is the delivery contract and progress tracker. The coding agent must update the status/evidence columns as work lands. Code existing in the working tree is not proof; only a commit plus the named verification is ✅.

## 0. Outcome and product decision

| Question | Decision |
|---|---|
| What must be proved first? | One small, safe sandbox must visibly demonstrate scheduling, data/event reaction, Jev choice, tool execution, failures, restart and audit. |
| Primary UI | **Local browser UI**: a graph, inspector and timeline need more space and interaction than a terminal. |
| Role of the terminal | Remains authoritative for build, automation, CI, `run`, `decide`, `doctor`, logs and recovery. Do not build a second animated terminal dashboard. |
| First UI release | Read-only topology + live/replayed execution. It may observe a CLI-started runtime. |
| Play / Stop | Add only after the read-only trace is trustworthy; reuse the existing runtime supervisor, leases and graceful drain. |
| Drag and drop | Later phase, after real use. It creates a **new draft sandbox** through Rust validation; it never becomes a second workflow engine. |
| Jev's place | Jev is the typed next-action controller (“cerebellum”), not an unrestricted central brain. The current event/run is the centre of the UI. |
| Source of truth | Existing validated sandbox TOML, tool catalog, Execution Map and runtime evidence. The browser never owns business behaviour. |

## 1. Why this work comes now

The repository already has the machinery, but it is hard to verify as one system:

| Existing foundation | Current surface |
|---|---|
| Architect → Jev → tools experiment | `sandboxes/jev-exec/config.toml` |
| Scheduled feeds, decision loops, simulation and typed observations | `sandboxes/lping/config.toml` |
| Execution Maps that can only narrow a loop | `src/config/execution_map.rs` · `tengu decide --map` |
| Decision evidence | `<TENGU_HOME>/logs/decisions.jsonl` |
| Runtime health | heartbeat JSON + `loop/1` / `feed/1` observations + `tengu doctor --live` |
| Planner/subagent events and metrics | `OrchestratorEvent` broadcast bus |
| Static visual explanations | `docs/tutorial/decision-loop.html` · `runtime.html` · `sandboxes.html` |

Missing: one reference run, one topology model, one correlated execution trace, and one UI that shows those facts together.

## 2. Scope

### First delivery

1. A new safe reference sandbox, provisionally `control-loop-lab`.
2. A repeatable CLI acceptance run against the real Jev endpoint.
3. A versioned, redacted execution-event/read model.
4. `tengu studio --sandbox control-loop-lab`: local read-only browser UI.
5. Live attach and historical replay of a completed run.
6. After Operator Review #2: bounded Play / Stop for the lab runtime.

### Explicitly deferred

| Deferred item | Unlock condition |
|---|---|
| Editing arbitrary existing TOML | Import/save-as-new behavior and comment preservation are designed and reviewed. |
| Drag-and-drop workflow authoring | The operator has used the read-only Studio on real runs and approves the node/edge vocabulary. |
| Remote control over a network | Authentication, TLS and threat model are separate work. MVP binds to loopback. |
| Real wallet/order actions | Never part of the lab. A production sandbox remains governed by its existing risk/signing rules. |
| A new generic workflow runtime | Not needed. Studio must project and control the current Tengu runtime. |

## 3. Reference sandbox contract

`control-loop-lab` must be understandable without reading Rust.

| Property | Requirement |
|---|---|
| Safety | Dedicated workspace; no signer, wallet grant, paper/live order, shell action or unrestricted filesystem scope. |
| Default action mode | Read-only or local `dry_run` / `simulate`. Any real side effect is confined to a named file under the lab workspace. |
| Trigger | Manual JSON event plus a short scheduled tick suitable for a demo. Do not wait until the next morning/weekend to verify it. |
| Data modes | Reproducible fixture/scenario input is mandatory; optional public live-data input may be added separately. |
| Decision | Real Jev call with the configured `act_at`; log the model actually returned. Fake engines are test-only. |
| Actions | At minimum: `hold`, one safe executed action, one error/refusal path and one below-confidence path. |
| Evidence | Every step has sandbox, runtime/run, session, event, decision, action and tool-call correlation IDs. |
| Restart | Lease, heartbeat, graceful stop and a second start must be visible and verified. |
| Runbook | One quick-start path, one CLI-only path, expected outputs, troubleshooting and cleanup. |

Required scenarios:

| Scenario | Expected visible result |
|---|---|
| `normal` | Jev chooses `hold`; no tool side effect. |
| `act` | Jev chooses the safe action; arguments and result are visible. |
| `uncertain` | Confidence is below `act_at`; escalation/stop is explicit. |
| `tool-error` | Failure, retry/backoff decision and health effect are explicit. |
| scheduled tick | The feed fires repeatedly; queue/in-flight/completed counters move. |
| restart | Old run stops cleanly; new runtime ID starts; evidence is not mixed. |

**Implementation constraint:** first attempt the lab with existing feeds, loop configuration and tools. If a new tool is genuinely required, stop and propose the smallest reusable Rust tool before adding it; follow the complete catalog/scope/schema/engine-matrix recipe.

## 4. Studio UX contract

### Page layout

| Region | Content |
|---|---|
| Header | Sandbox, config hash, runtime state, run ID, model, Live/Replay, health. |
| Graph | Trigger/feed → event/observation → loop → Jev choice → action/tool → result; risk/scope gates surround executable nodes. |
| Inspector | Selected node's validated config, current input, legal actions, answers/probabilities, arguments, reduced/redacted result and duration. |
| Timeline | Ordered correlated events with filter by session, loop, agent, model, tool, status and call ID. |
| Evidence drawer | Links/paths for heartbeat, decision audit, observation key and generated run artifact. |
| Controls | Phase 1: attach/replay only. Phase 2: Play, graceful Stop and scenario selection when explicitly allowed. |

### Visual semantics

| Object | Meaning |
|---|---|
| Analysis map | What Jev saw: goal, event, fresh world and current-event history. |
| Execution map | Allowed/narrowed actions, sequence, caps, threshold and dry-run state. |
| Highlighted edge | The action actually selected, never an animation guessed by the frontend. |
| Grey node | Configured but not legal for this step. |
| Amber node | Waiting, stale, retrying or below confidence. |
| Red node | Failed, refused by a deterministic gate or unavailable. |
| Green node | Executed successfully or terminally completed. |

No “thinking” animation may be shown unless a real backend event supports it.

## 5. Technical architecture

```text
validated Config + tool catalog + optional Execution Map
                         │
                         ▼
                  WorkflowGraph (read model)

runtime / feeds / Jev / tools / risk / orchestrator
                         │
                         ▼
              redacted ExecutionEvent stream
                         │
                 store + bounded live bus
                         │
              snapshot/replay API + SSE
                         │
                         ▼
                  local browser Studio
```

### Required boundaries

| Layer | Responsibility |
|---|---|
| `domain` | Plain versioned `WorkflowGraph`, `ExecutionEvent`, IDs, statuses and pure ordering/redaction-safe summaries. No HTTP, config parsing or persistence. |
| `ports` | Trace sink/store/reader and runtime-control abstractions only where a real boundary exists. |
| `application` | Build the graph from already validated config/catalog data; correlate events; start/attach/replay use cases. |
| outbound adapters | Durable trace storage if required; reuse existing audit/observation/runtime stores rather than copying business rules. |
| inbound adapters | CLI command and local HTTP/SSE endpoints; request validation and browser assets. |
| bootstrap | Composition only: connect the existing runtime, event sink, trace reader and Studio server. |
| web assets | Rendering and interaction only. No risk, scope, schedule, workflow or decision policy in JavaScript/TypeScript. |

### `ExecutionEvent` minimum envelope

The exact Rust names may change after the Phase 0 audit, but the contract must carry:

`schema_version`, `event_id`, `ts_ms`, `sandbox`, `config_hash`, `runtime_id`, `run_id`, `session_id`, `correlation_id`, optional `parent_event_id` / `call_id`, `component`, `kind`, `node_id`, `status`, optional `duration_ms`, and a bounded redacted payload or artifact reference.

Minimum event families:

- runtime starting/running/stopping/stopped;
- feed scheduled/fired/retrying/dropped;
- loop queued/started/completed/failed;
- Jev request completed/failed with choices and confidence;
- action selected/rejected/escalated;
- tool started/completed/failed/refused;
- observation read/fresh/stale/missing;
- deterministic risk/scope refusal;
- planner/subagent step events when present.

Requirements:

- IDs are stable across live streaming and replay.
- One event is written once; reconnecting SSE clients resume from an event ID.
- Live transport is bounded and may lag; durable evidence remains replayable.
- Redaction happens before persistence and before broadcast.
- Large/raw outputs stay in existing artifacts; events carry bounded summaries and references.
- Existing audits remain compatible during migration; do not silently replace them.

### Browser implementation

| Decision | Rule |
|---|---|
| Backend | Rust only; reuse the repository's existing HTTP stack where possible. |
| Live transport | Prefer SSE for server → browser events; ordinary HTTP for snapshots and later controls. Use WebSocket only if a demonstrated need appears. |
| Frontend | Isolated HTML/CSS and minimal JavaScript/TypeScript are allowed **only for Studio web pages**. No Node backend. Avoid a build chain for the first read-only release. |
| Assets | Bundle locally; no CDN, analytics, telemetry or runtime dependency on the public internet. |
| Bind | `127.0.0.1` by default. Non-loopback bind is refused unless explicitly enabled and protected. |
| Mutation | Read-only by default. Control endpoints require explicit CLI enablement and an anti-CSRF/session token. |

Before adding non-Rust web source, the coding agent must update both `AGENTS.md` and `CLAUDE.md` with the same narrowly scoped Studio-web exception. This user-approved exception does **not** permit JavaScript/TypeScript/Python for tools, adapters, runtime behavior, tests, fixture generators or probes.

## 6. Play / Stop contract

Play/Stop is Phase 4, not a prerequisite for the read-only UI.

| Action | Contract |
|---|---|
| Play | Starts the existing application runtime for the selected sandbox; it must use the same config validation, composition, lease and stores as `tengu run`. No duplicate mini-runtime. |
| Attach | If a runtime already owns the lease, Studio attaches read-only instead of starting a second copy. |
| Stop | Requests the existing bounded graceful drain; shows stopping → stopped and preserves evidence. No `kill -9`. |
| Scope | Enabled for `control-loop-lab` by default only. Other sandboxes require an explicit `--allow-control`. |
| Failure | Start/stop errors become trace events and visible UI states; never pretend the click succeeded. |

## 7. Future visual editor contract

**STOP: design/spike only until Operator Review #3.** Design (ST-40): `docs/studio-editor-design-2026-10-08.md` — palette, connection → TOML key table, validate → preview → Save-as-new flow, open questions with default picks. Nothing of it is built.

| Rule | Required behavior |
|---|---|
| Palette | Derived from Rust config/tool schemas and the live catalog, never a second hand-maintained list. |
| Connections | Represent existing Tengu concepts: feed target, world read, action tool, slot binding, sequence, escalation and agent ownership. |
| Validation | The Rust backend runs the same `Config::load`/validation logic; frontend validation is advisory only. |
| Save | “Save as new sandbox draft”; show generated TOML and diff before writing. Do not rewrite an existing hand-authored or running sandbox in v1. |
| Safety | No arbitrary-code/shell node; tools, scopes, wallets, modes and caps remain explicit. |
| Frozen generations | W1-bound sandboxes are view-only. A changed pinned design requires a new sandbox/generation. |
| Execution Map | A UI-produced map may only narrow the base loop exactly as `ExecutionMap::apply` permits. |

## 8. Delivery plan and review gates

Status: `☐ not started` · `🟡 working tree only` · `✅ committed + verified` · `⛔ blocked` · `⏭ waived` (gate only: waived by the operator's 2026-10-08 instruction, never reviewed).

| ID | Status | Task | Required evidence | Commit |
|---|---:|---|---|---|
| ST-00 | ✅ | Audit current runtime, loop, audits, event buses and active work; identify reuse vs gaps. | One terse table added below; no implementation yet. — Done: § 8 "Phase 0 audit result" (7 concerns, file:line at `10bdbb53a27390f6a67fd73826740befe05ababe`) + gap line. | `52cad0cf2badb4ec9a11457e862775dc242575a6` |
| ST-01 | ✅ | Write acceptance matrix and exact commands for the lab. | Operator can predict every expected result before code changes. — Done: `docs/control-loop-lab-2026-10-08.md` (setup, quick start, env names, matrix A0–A15, troubleshooting, guarded cleanup); § 9 commands updated. | `52cad0cf2badb4ec9a11457e862775dc242575a6` |
| ST-02 | ✅ config + guard test; live proof in ST-03 | Add `control-loop-lab` with reproducible scenarios and optional live mode. | CLI run shows all required scenarios; no real external write. — `sandboxes/control-loop-lab/{config.toml, scenarios/{normal,act,tool-error}.json, scenarios/{uncertain,act-dry}.map.json}`; no new tool; `config::decision_loop::tests::control_loop_lab_has_no_dangerous_surface` + `config::risk::tests::every_sandbox_and_the_example_load` pass. Optional live mode deferred (not needed for Gate 1). `uncertain.map.json` also turns dry-run on: a Jev confidence of exactly 1.0 passes `act_at = 1.0` (application/decision_loop/mod.rs:394, gate is `<`) and would otherwise write. | `52cad0cf2badb4ec9a11457e862775dc242575a6` |
| ST-03 | ✅ | Record a baseline real-Jev CLI run. | Sanitized transcript, audit excerpts, health before/during/after, actual model and cost/latency. — Done: `docs/control-loop-lab-baseline-2026-10-08.md`. A0–A15 pass; A1–A4 3/3 each; Jev build `typesafe/jev-1.13-20260917`; 32 calls cost $0.000799134 at 267–679 ms; two runs (holders `Vladimirs-MacBook-Pro-2.local:74815:83519fb5-3839-45a3-9724-78322e317268`, `Vladimirs-MacBook-Pro-2.local:76105:f9ecbbab-7cea-4648-8025-cd26aefb5801`); `~/.tengu` untouched (sha256 proof). The run corrected 9 runbook details, listed in § Corrections of that doc: a parent `.env` is loaded, `decide` writes its logs to stdout, a tick's `t` is per process, cleanup leaves the observation store. | `7062b4c6bbe302b78f76d13b7c33b5af5aa6896a` |
| **Gate 1** | ⏭ waived | **Operator reviews the lab before trace/UI work.** | Explicit approval. — waived by the operator's 2026-10-08 instruction — evidence: `docs/control-loop-lab-baseline-2026-10-08.md` (ST-03 commit `7062b4c6bbe302b78f76d13b7c33b5af5aa6896a`). Not reviewed by the operator. | `7062b4c6bbe302b78f76d13b7c33b5af5aa6896a` |
| ST-10 | ✅ | Add the pure workflow graph/read model. | Golden graph for the lab and at least one existing complex sandbox. — Done: `domain/workflow.rs` + `application/studio/graph.rs` (`build_graph`, execution map via `ExecutionMap::apply`: dropped actions `narrowed_out`, `map_changes`) + `bootstrap/studio.rs` + `Config::source_sha256` (`toml_digest`) + `tengu studio graph --sandbox <s> [--map <file>]`; goldens `tests/fixtures/studio/graph-control-loop-lab.json` (16 nodes, 17 edges) and `graph-lping.json` (4 loops, webhook trigger, requires / binds / caps / sequence); tests `domain::workflow::tests::{normalize_is_order_independent, node_ids_keep_full_names}` · `application::studio::graph::tests::{golden_control_loop_lab, golden_lping, map_narrows_and_greys_dropped_actions, map_never_widens}` · `bootstrap::studio::tests::graph_attrs_are_redacted` pass; live `studio graph` on the lab = golden (`docs/control-loop-lab-baseline-2026-10-08.md` § ST-10 / ST-11 smoke). Deviation: tool nodes carry `in_catalog`, not tool descriptions (keeps goldens stable across catalog text edits). | `dfec6877cabf2d9e1e533ca0ff714a3a4bd1f506` · review `c9eeabc1c94e28d0e9819bd94e5126cf095b7c40` |
| ST-11 | ✅ | Add versioned correlated execution events and bounded live/durable delivery. | Ordering, reconnect, lag, restart, redaction and compatibility tests. — Done: `domain/trace.rs` (every § 5 field + `seq` + `artifact`) · `ports/trace.rs` · `adapters/outbound/trace_store.rs` (`<TENGU_HOME>/logs/trace/<sandbox>/<run_id>.jsonl`, `run.opened` first, redaction before write, payload ≤ 4096 B by whole fields, follow 250 ms into a bounded channel) · `NoopTrace` · `bootstrap/trace.rs` · `tengu run` / `tengu decide` record; `decisions.jsonl` + `runtime_id` / `run_id` · `tengu trace runs\|show [--after] [--follow]`; ST-03 open issue fixed (`decide` / `doctor` log to stderr). Tests: ordering `domain::trace::tests::ordering_by_seq`, `trace_store::tests::concurrent_emit_keeps_seq_order` · reconnect `follow_resumes_after_seq`, `ids_stable_across_write_and_replay`, `one_event_written_once` · lag `follow_lagging_reader_loses_nothing` · restart `restart_starts_new_run_file` · redaction `redacts_secrets_and_urls_before_write`, `bound_payload_drops_fields_never_cuts_ids` · compatibility `decision_loop::tests::audit_lines_carry_runtime_and_run_ids`, `envelope_round_trips_and_tolerates_unknown_fields`, `cli::tests::data_commands_log_to_stderr`. Live: real-Jev decide (`run_id` `0c6f502e-c8db-48f4-bf4f-9843c0836fc1`) + 26 s `tengu run` (`run_id` `648f7ea2-4a6f-4c6d-a920-8cbe6922b3da`, holder `Vladimirs-MacBook-Pro-2.local:96248:d657d420-589f-40a7-8f35-0891e6b16e79` on both tick lines). Deviations: `TraceReader` bound to one sandbox at construction; the SSE bus / per-client lag event is ST-20 (here: the file is the durable bus, `follow` backpressures); `tengu webhooks` records no trace yet. | `dfec6877cabf2d9e1e533ca0ff714a3a4bd1f506` · review `c9eeabc1c94e28d0e9819bd94e5126cf095b7c40` |
| ST-12 | ✅ | Instrument the lab path end-to-end without duplicating business rules. | Every required UI transition points to a real persisted event. — Done: `application/trace_exec.rs` (per-task `Cause` = `parent_event_id`; `TracedExecutor` `tool.started → completed / failed` + `duration_ms`, inside `AttributedExecutor`, outside `SanitizedToolExecutor`) · `runtime.starting / running / start_failed / stopping / stopped` (`bootstrap/runtime.rs`, `start` → `start_recorded`) · `loop.queued / started / completed / failed / dropped / refused` + `LoopStats` (`LoopDispatch::with_trace`) · `feed.fired / completed / retrying / failed / tick_sent / dropped / skipped` (`FeedEnv::trace`) · `observation.read` (`World::reads`), `jev.completed` (answers + the per-step legal set + questions) / `jev.failed`, `action.selected / completed / refused / escalated / rejected` carrying the computed `StepOutcome` (`DecisionLoop::with_trace`) · `trigger.decide / map → completed / failed` (`cli/decide.rs`; a loop that cannot be built is now `trigger.* failed`, fixing ST-11's empty decide run). Coverage table (every § 4 visual → event kind + field): `docs/runtime-2026-09-30.md` § Trace + tutorial `studio.html` § "What each Studio visual reads". Tests: `trace_exec::tests::{tool_events_carry_call_id_and_duration, typed_error_rows_are_failures}` · `loops::tests::trace_events_follow_ticket_lifecycle` · `feeds::tests::{tick_trace_links_loop_event, fatal_tool_failure_traces_failed_not_retrying}` · `decision_loop::tests::{trace_covers_every_outcome, trace_legal_set_matches_questions, escalation_event_has_confidence_and_act_at}` · `bootstrap::runtime::tests::lifecycle_events_on_start_and_shutdown` pass. Not traced yet: planner / subagent steps, typed scope refusals (G5: a `tool.failed` with the text), `tengu webhooks`. Review (adversarial, ST-10..ST-12): 8 fixes — payload keys + graph URLs scrubbed (`domain::trace::scrub_value`), non-2xx `http_request` = `tool.failed` (`reduce::http_ok`), stopped tool-feed run closed by `feed.skipped`, `bound_payload` bound kept, decide `run.opened` names no node, one dry-run rule `DecisionLoopConfig::logs_only`, `trace show` single read + one Ctrl-C listener (`docs/SESSION_HANDOFF.md` row). | `d6438a4294a74d1be4b3aa65d5722b7d29c3d5eb` · review `c9eeabc1c94e28d0e9819bd94e5126cf095b7c40` |
| **Gate 2** | ⏭ waived | **Operator reviews raw trace/replay before UI controls.** | CLI/API replay reconstructs the run. — waived by the operator's 2026-10-08 instruction — evidence: `docs/studio-trace-evidence-2026-10-08.md` (commit `d6438a4294a74d1be4b3aa65d5722b7d29c3d5eb`): real-Jev decide act / tool-error / `uncertain.map.json` + `tengu run` 62 s (5 ticks, 3 probe failures, SIGINT) + restart 26 s (SIGINT); `tengu trace show` reconstructs all 5 runs (seq 1…n, `event_id` = `<run_id>:<seq>`, 90 / 116 parents inside their run, replay twice = same bytes, `--after 5` = the tail, live `--follow` = replay byte for byte), runtime ids differ per restart (`Vladimirs-MacBook-Pro-2.local:27798:bfd39ea7-b4cb-4260-9618-3e5a0a88f482` vs `Vladimirs-MacBook-Pro-2.local:28618:9860a615-b02d-44c4-bd82-1b0ce7399a51`), key value 0 matches in the trace dir. Lab cleaned with the runbook block. Not reviewed by the operator. | `d6438a4294a74d1be4b3aa65d5722b7d29c3d5eb` |
| ST-20 | ✅ | Add `tengu studio` local server and read-only APIs/SSE. | Loopback/security tests; attach to running lab. — Done: `tengu studio --sandbox <s> [--port] [--bind]` behind the non-default feature `studio` (axum) · `adapters/inbound/studio/{mod,api,guard,sse,assets}.rs` (GET only: `/`, `/assets/*`, `/api/v1/{meta, graph[?map=], health, runs, runs/:run_id/events, runs/:run_id/stream, live/stream}`) · `application/studio/stream.rs` (backlog waits; live overflow past 256 ⇒ `event: lagged` + close; resume by `Last-Event-ID` / `?after=`; live = the heartbeat holder's run across restarts) · `bootstrap/studio.rs` `StudioContext` · health = `bootstrap::runtime::read_live` (shared with `doctor --live`) · Rust-only exception for `web/studio/` (CLAUDE.md + AGENTS.md, commit `1d8223fd4ed2c0384c20db27bee45625fb71c636`) + `tests/language_policy.rs` · CI step `cargo test --features studio --bin tengu studio` · review open issue fixed: `jev.*` pin `legal` + `legal_actions` (`EventDraft::keep`; `trace_store::tests::bounding_keeps_the_legal_set`, `domain::trace::tests::bound_payload_keeps_pinned_fields`). Tests (real `127.0.0.1:0` listener + reqwest): `studio::tests::{refuses_non_loopback_bind, rejects_foreign_host_header, rejects_cross_origin, token_guards_every_api_route, get_routes_only_without_control, graph_endpoint_matches_golden, events_page_is_ordered, sse_resumes_from_last_event_id, sse_lagged_client_resumes_without_gap, attaches_to_running_lab_by_heartbeat_holder, health_without_a_runtime_is_not_live, streams_are_capped_and_end_at_shutdown}` · `stream::tests` (5) · `cli::studio::tests` · `language_policy::{non_rust_only_in_allowed_dirs, policy_table, studio_web_is_local_only}`. Live: one Studio attached to a real `tengu run` of the lab, SIGINT, restart, SIGINT — `attached` → `restarted`, 67 events in order, `/health` = `doctor --live`, API page = `tengu trace show` (`docs/studio-trace-evidence-2026-10-08.md` § ST-20). Deviations: the token is required on every `/api/` GET too (design: non-GET only); no `--run` flag (the page picks runs, ST-22); status → colour not served yet (ST-21). | `1d8223fd4ed2c0384c20db27bee45625fb71c636` · `fe5115bc31c0bb381ccb4ea80998cba459ccbf2a` |
| ST-21 | ✅ | Add graph, inspector, filters and timeline. | Browser evidence matches the same run's audit and health counters. — Done: `web/studio/{index.html,studio.css,studio.js}` (vanilla, no build, no CDN; SVG at the Rust `layer` / `order`; header sandbox · config hash · runtime state · run id · runtime id · model · Live/Replay · health; inspector; timeline filters session · loop · agent · model · tool · status · call id; evidence drawer; light/dark tokens; one column below 1100 px) draws only Rust results: tones `domain::trace::Status::tone` → `Tone` (served in `/api/v1/meta`; decision: `skipped` = plain dashed — logged, not run; `dropped` = amber), the board fold `application/studio/board.rs` (`GET /api/v1/runs/:run_id/board[?upto=]`: node tone = its latest event's, lit edge only from `action.*` / `tool.*` and only a graph edge, grey = the loop's latest `jev.*` legal set or a map's `narrowed_out`, header facts), each event's `view` on `/events` (tone, `Node::facets` inherited down the parent chain, edges), `GET /api/v1/nodes/:node_id[?map=]` (`application/studio/inspect.rs`: the validated config section, never TOML text, + edges + evidence files), health tones. Tests: `studio::tests::node_detail_is_from_validated_config`, `bootstrap::studio::tests::node_detail_is_redacted`, `board::tests::{colours_and_edges_from_events_only, grey_is_the_latest_legal_set, fold_is_deterministic}`, `inspect::tests::slices_come_from_validated_structs`, `domain::trace::tests::every_status_has_one_tone`, goldens regenerated (facets, additive). Live (real Jev, headless Chrome on a temp profile): run `95ff067e-f446-410e-ab2c-7cd42af74cf9` at 99 s — board `completed 6` = heartbeat `completed 6` = `doctor --live` `done 6` = 6 `decisions.jsonl` lines of the run (`docs/studio-trace-evidence-2026-10-08.md` § ST-21 / ST-22). Deviations: browser check by headless Chrome screenshots + DOM dumps (the Chrome extension was not connected; no Playwright); phone width checked at 500 px (headless Chrome's smallest window). Review: the page no longer greys a `narrowed_out` node itself nor maps a control verdict ok → green / else → red (the board and the verdict's `tone` decide; `studio::tests::page_compares_no_status_or_tone`). Live validation 2026-10-09 (real Jev, run `cbd8cd05-30fb-4177-aa99-0b978f0e83e5`, `typesafe/jev-1.13-20260917`, 117 events, replay identical): screenshots `docs/studio-evidence/01-idle.png` (static graph before a runtime), `02-running.png` (live tones, lit edges, loop `completed 9` = health `done 9`), `03-replay.png` (closed run at `seq` 60 of 117) — `docs/studio-2026-10-08.md` § Live validation. | `a0ecdb5490975eeb61865047186f73dbb18a0d9c` · review `7a69f79e51c46b54cfdaa0123da5b18c00610d7e` |
| ST-22 | ✅ | Add historical run selection and replay. | Reload/reconnect produces identical ordered nodes/events. — Done: Runs table (state live · closed · open = `domain::trace::RunState`, `config changed` badge from `config_current`), Replay with a `seq` slider (`board?upto=`), place in the URL fragment (mode, run, seq, node, event); live = the SSE stream as a doorbell + the same `/events` pages, so live and replay are one list; a `tengu decide --map` run drawn on its kept map's graph (`StudioContext::run_graph`); a run of another config is drawn on the current graph with its unknown nodes listed (MVP: its own graph is not rebuilt). Tests: `studio::tests::{replay_equals_live_sequence, reload_returns_identical_order, runs_are_not_mixed_across_restarts}`, `domain::trace::tests::run_state_by_closing_event`. Live: `/events` pages of 7 = one page of 1000 = `tengu trace show` (73 events), `/board` read 3× byte-identical, restart run `a0fd15c8-1779-4398-a691-ef4b4196d8eb` its own ids and counters. | `a0ecdb5490975eeb61865047186f73dbb18a0d9c` |
| **Gate 3** | ⏭ waived | **Operator uses the read-only Studio and approves controls.** | Explicit approval. — waived by the operator's 2026-10-08 instruction — evidence: `docs/studio-trace-evidence-2026-10-08.md` § ST-21 / ST-22 (commit `a0ecdb5490975eeb61865047186f73dbb18a0d9c`). Not reviewed by the operator. | `a0ecdb5490975eeb61865047186f73dbb18a0d9c` |
| ST-30 | ✅ | Add Play/Attach/Graceful Stop through the existing runtime. | Second runner refused; attach works; graceful drain visible. — Done: `adapters/inbound/run.rs` split into `start_session` → `RunSession` (`stopper`, `loops`, `wait_and_shutdown`; `tengu run` unchanged) · typed lease refusal `bootstrap::runtime::LeaseHeld` · `[studio] control` (`config/studio.rs`: default false, `control-loop-lab` true, `tengu studio --allow-control`, never for `[generation]`-bound or hardened — `control = true` there fails the load) · rules `application/studio/control.rs` (phase + heartbeat → idle · attached · starting · running · stopping · stopped · failed, each action ok or why not) · `adapters/inbound/studio/control.rs` `Controller`: Play = `start_session` in the Studio process (same leases, recording, loops, feeds, heartbeat; a held lease ⇒ 409 + attached read-only), Stop = `Stopper::stop("studio stop")` (the SIGINT drain; refused for a runtime Studio did not start), event = a named scenario (`scenarios/<name>.json`) into the owned `LoopDispatch`, session `studio-<scenario>-<uuid>`; start / stop failures + every request = `studio.control` / `studio.runtime` events in Studio's own recording (`RunKind::Studio`, `studio.stopped` closes it) · routes `GET /api/v1/control`, `POST /api/v1/control/{play,stop,event}` · page: Play / Stop / scenario select drawn only from `/api/v1/control` (hidden when off). Tests: `adapters::inbound::studio::control::tests::{play_uses_runtime_lease, second_play_refused, play_attaches_when_cli_holds_lease, stop_drains_and_releases_lease, stop_refused_for_attached_runtime, start_failure_is_a_trace_event, play_survives_a_dropped_request, read_only_refuses_every_action}` · `config::studio::tests::{control_off_by_default, refused_for_generation_bound_and_hardened}` · `application::studio::control::tests::{rule_table, only_a_live_foreign_heartbeat_attaches, scenarios_and_their_loop, refusals_say_why}` · `studio::tests::{control_routes_play_event_stop, get_routes_only_without_control}` · `bootstrap::runtime::tests::a_second_instance_is_refused_until_the_first_stops` (typed `LeaseHeld`). Live (real Jev, release `--features studio`): `docs/studio-acceptance-2026-10-08.md` — Play 200 (holder `Vladimirs-MacBook-Pro-2.local:31302:64331316-a1f3-4a4f-a2c0-948380f5f8a8`, run `93668d55-372f-45a3-9b45-cfec64055960`), CLI `tengu run` exit 1 naming that holder, 2 ticks, events act (write_marker ok) + tool-error (read_probe ok false), Stop 200 (`lease_released: true`, heartbeat `stopped` / `studio stop`), CLI run ⇒ `attached` + Play / Stop / event 409, Studio SIGINT drained its second runtime (`studio SIGINT`) and exited 0. Deviations: Stop answers after the drain (200; 202 after the grace + 5 s) rather than at once; Play takes an optional `scenario`; event accepts a scenario name only (no event body from the browser); Studio records its own run only with control on; Play runs on its own task (a dropped request never half-starts a runtime); `control_off_by_default` / `refused_for_generation_bound_and_hardened` live in `config::studio::tests`. Review (adversarial, ST-20..ST-40): a live drain of an in-flight event (Stop at its `loop.started`: `runtime.stopping` before its `jev.completed`, the write ran, `loop.completed`, drain `finished 1`) and the Studio server logs like `tengu run` (`tengu.log` + stderr) — `docs/studio-acceptance-2026-10-08.md` § Review R3. Live validation 2026-10-09 in the browser (real Jev): Play → Send `act` (`write_marker` ok) → Send `tool-error` (`read_probe` failed, then `hold` escalated at 0.69 < `act_at` 0.8) → Stop (`studio stop`, `lease_released: true`); run `cbd8cd05-30fb-4177-aa99-0b978f0e83e5` (117 events), Studio's recording `d9bcce3d-7f95-4116-af42-26b0b812c1ee` (12 events) — `docs/studio-evidence/02-running.png`, `03-replay.png`, `docs/studio-2026-10-08.md` § Live validation. | `b1d51fc05801a1bb29e654b21d94e3d55a45c26b` · review `7a69f79e51c46b54cfdaa0123da5b18c00610d7e` |
| ST-31 | ✅ | Harden lifecycle and local control security. | CSRF/token, loopback, crash/restart and no-orphan tests. — Done: `guard.rs`: a change request (any non-GET on `/api/`) needs the `X-Studio-Token` header (never `?token=`), `Origin` = this server and `Sec-Fetch-Site: same-origin` — else 403; `Host` not this loopback server = 421 first (DNS rebinding); constant-time token compare; JSON body only (415), ≤ 1 MiB (`DefaultBodyLimit`, 413), unknown fields 400; no CORS header, preflight 403; loopback bind (ST-20). Lifecycle: Studio SIGINT / SIGTERM = `Controller::shutdown` (no new Play, waits a starting Play, the Stop drain, `studio.stopped`), a second signal exits 130; the runtime lives in the Studio process (no child, no orphan; a killed Studio's lease frees after the 30 s TTL). Tests: `studio::tests::{post_without_token_is_403, post_cross_origin_is_403, dns_rebinding_host_is_421, control_body_is_capped_and_strict}` · `control::tests::{sigint_drains_owned_runtime, no_orphan_after_owner_crash_lease_expires, start_failure_is_a_trace_event}`. Live guards (the release binary): no token / token in query / foreign Origin / no Origin / no Sec-Fetch-Site 403, rebinding Host 421, text/plain 415, 1 100 000-byte body 413, event body 400, preflight 403 — nothing started (`docs/studio-acceptance-2026-10-08.md` § Guards). Review (adversarial, ST-20..ST-40): the CSRF proofs now guard every request but GET / HEAD on any path (`studio::tests::guard_holds_for_odd_path_spellings`, raw sockets: `//api/`, `/./api/`, `/%61pi/`, `/%2Fapi/`, `/assets/../api/`, `/API/`), `scenario` / `loop` names 1–128 bytes (400, never echoed), the control verdict carries its `tone` and `studio.js` compares no status or tone (`page_compares_no_status_or_tone`); live on the release binary: guards incl. traversal in `/assets`, run / node ids, maps and a bogus `Last-Event-ID`, a secrets sweep of every served JSON (0 key matches), and a `kill -9` of a Studio running its runtime (no tengu process left, CLI refused for the lease's 27 s, taken over 29 s after the kill) — `docs/studio-acceptance-2026-10-08.md` § Review. | `b1d51fc05801a1bb29e654b21d94e3d55a45c26b` · review `7a69f79e51c46b54cfdaa0123da5b18c00610d7e` |
| **Gate 4** | ⏭ waived | **Operator decides whether the editor is still valuable.** | Node/edge vocabulary is based on observed use, not guesses. — waived by the operator's 2026-10-08 instruction — evidence: `docs/studio-acceptance-2026-10-08.md` (commit `b1d51fc05801a1bb29e654b21d94e3d55a45c26b`). Not reviewed by the operator. | `b1d51fc05801a1bb29e654b21d94e3d55a45c26b` |
| ST-40 | ✅ design only | Editor design/spike: palette, connect, validate, TOML preview, Save As. | No production save/edit until separately approved. — Done: `docs/studio-editor-design-2026-10-08.md` (one screen): palette from Rust only (`NodeKind` + `layer`, each agent's `agent_base_tools` → catalog `ToolDef` schemas, a Rust `palette()` over `DecisionLoopConfig` / `ActionConfig` / `SlotConfig` / `SeqStep` / `FeedConfig` / `RuntimeConfig` kept honest by a `deny_unknown_fields` round-trip test; no shell / code / signer / wallet / `[risk]` node); connections = the existing `domain/workflow.rs` edges, each mapped to the one TOML key it writes; validate = `Config::load` on the draft at its final path (hardening + `[generation]` read the path); preview = TOML + `similar` diff + `build_graph`; Save as new only (`create_new`, never in place or a running sandbox); W1-bound / hardened view-only; maps only via `ExecutionMap::apply`, never written into TOML; open questions with default picks (`toml_edit` for comments, drafts in `sandboxes/<name>/`, `--allow-edit`). No code, route or branch: `rg 'allow-edit\|/api/v1/drafts' src web` finds nothing. Tutorial `studio.html` § "The editor, as designed". Deviation: no throwaway branch (coordinator's step text) — the spike is the design over seams that already run (`/api/v1/graph?map=`, `/api/v1/nodes/:id`, `Config::load`). Gate 4 waived ⇒ the vocabulary is the read model's, not observed use (said in the doc). | `f56b1b45325c3a94a002569337f53512c41d335e` |
| ST-90 | ✅ committed + verified | Final docs, tutorial, runbook and clean-room acceptance. | Fresh checkout follows quick start; all required repository checks pass. — Docs: `docs/studio-2026-10-08.md`, tutorial `studio.html`, `docs/runtime-2026-09-30.md`, code map, CLAUDE.md + AGENTS.md, § 9 (docs sweep B, 2026-10-09). Clean room 2026-10-09: fresh clone of origin/main `f7ac9a5636e2493e3839e896cba9951a2d41aefb`, `cargo build --release --features studio` exit 0, lab A0–A15 + G1–G8 + Studio Play / events / Stop all as documented (real Jev `typesafe/jev-1.13-20260917`, 39 calls), `cargo test --workspace` 2130 passed / 0 failed, `--features studio` 2176 passed / 0 failed, fmt / layering / scope / tutorial_map / code_map / language_policy / lineage_cli green, `~/.tengu` untouched; 8 doc fixes applied — `docs/studio-clean-room-2026-10-09.md`. Live browser evidence: `docs/studio-evidence/` (headless Chrome captures of a real-Jev run). | docs sweep B PR (this row) |

### Phase 0 audit result

Read-only audit at `10bdbb53a27390f6a67fd73826740befe05ababe` (2026-10-08). Paths are `src/`-relative unless named; every ref was read at that commit.

| Concern | Reuse exactly | Extend | Do not duplicate |
|---|---|---|---|
| Runtime lifecycle | `bootstrap::runtime::start` (bootstrap/runtime.rs:284-306) · `Runtime::begin/launch/shutdown` (499-571, 617-655: `stopping` beat → drain ≤ `shutdown_grace_secs` → `stopped` beat → leases released) · `Supervisor` / `Stopper` first stop wins (application/runtime/mod.rs:55-83, 114-152) · `OwnerLeases::take` refusal naming the holder (bootstrap/runtime.rs:118-140, 157-185) · `ACQUIRE_SQL` (adapters/outbound/runtime_store.rs:26-32) · holder `<host>:<pid>:<uuid>` (bootstrap/runtime.rs:490-493) = `runtime_id` · `Signals` (adapters/inbound/run.rs:139-179) | split `run_runtime` (inbound/run.rs:20-87) into `start_session` + `drive`; Studio Stop = `stopper.stop("studio stop", false)`, the SIGINT path (run.rs:50-52) · trace-sink param on `start` / `begin` · `runtime.*` events beside the heartbeat writes (bootstrap/runtime.rs:633-646) | a second supervisor, mini-runtime or child `tengu run` · a stop file / table / `kill -9` · a new lease (`ACQUIRE_SQL` already refuses a second runner) |
| Feed/loop health | `Heartbeat`, `LoopHealth` (`loop/1`), `FeedHealth` (`feed/1`) (domain/runtime.rs:79-123, 183-206) · `read_heartbeat` (runtime_store.rs:55-65) · `live_verdict` = header health (domain/runtime.rs:396-479) · `HealthBoard::beat` (application/runtime/health.rs:126-175) · `LoopDispatch::stats` (loops.rs:157-162) · `FeedHealth::on_*` + `is_stale` (domain/runtime.rs:236-285) | `loop.queued/started/completed/failed/dropped/refused` from `enqueue` + `Ticket` (loops.rs:192-250, 308-357; `Ticket` gains `session_id`) · `feed.fired/completed/retrying/failed/dropped` from `FeedRunner::settle` / `run_tick` (feeds.rs:259-399) | counters, stale rules, doctor verdict — Studio reads `run-<s>.json` + rows, never recomputes · a new health table |
| Decision audit | `DecisionLoop::audit` (application/decision_loop/mod.rs:618-662) + `append_line`, one `write_all` (670-679) · call id `{loop}:{session_id}:{t}` (544-549) · `render_audit` (714-777) · TUI tail (adapters/inbound/tui/mod.rs:47-98) · `StepOutcome` (domain/decision.rs:121-144) · `JevClient` 1 retry + breaker 3 / 30 s (adapters/outbound/decisions.rs:9-13, 148-200) | additive `runtime_id`, `run_id` on `AuditLog` (mod.rs:95-104; the shape test checks presence only, 1781-1802) · `jev.*` / `action.*` / `tool.*` events beside `self.audit(…)` (309-339, 393-401, 436-480) with the per-step legal set (243-266) | legality / confidence / caps logic (mod.rs:350-501): events carry the computed `StepOutcome` · `decisions.jsonl` stays (TUI + evidence read it) |
| Orchestrator events | `OrchestratorEvent` + `EventBus` broadcast, cap 256 (application/orchestrator/events.rs:19-87) · bus built in `build_orchestrator` (bootstrap/orchestrator.rs:350-396) | later: a subscriber mapping `Step*` / `PlanCompleted` → `ExecutionEvent` with the surface's session id (events carry none: events.rs:22-55) — not needed for the lab (no `[orchestrator]`, `escalate = false`) | a second planner-event enum · any change to `OrchestratorEvent` (TUI, eval consume it) |
| Metrics | `MetricsRecord` / `MetricsKind::Decision` (domain/metrics.rs:43-103) per Jev call (decision_loop/mod.rs:587-609) · global sink (application/metrics.rs:21-64) · Jev `usage` incl. `cost` on the audit line (domain/decision.rs:79-86; mod.rs:647) | copy `latency_ms` + `usage` into the `jev.completed` payload | a Studio token counter · the global sink as Studio transport (per-process broadcast, not durable) |
| Config/tool schemas | `Config::load` (config/mod.rs:1159-1183) · `DecisionLoopConfig` / `ActionConfig` / `SlotConfig` + `validation_errors` (config/decision_loop.rs:55-250, 254-) · `FeedConfig` (config/feeds.rs:5-91) · `ExecutionMap::apply` (config/execution_map.rs:93-188) + kept maps `<TENGU_HOME>/logs/maps/<sha256>.json` (adapters/inbound/cli/decide.rs:110-119) · `ToolDef` (domain/message.rs:66) · `agent_base_tools` (bootstrap/tools.rs:430-447) · `catalog()` / `advertised_defs` (adapters/outbound/tools/mod.rs:83, 333) · `toml_digest` (domain/lineage/pins.rs:26-28) = `config_hash` | `Config.source_sha256` (`#[serde(skip)]`) = `toml_digest(&raw)` in `Config::load` (raw at config/mod.rs:1160) · `[studio] control` only in ST-30 | a hand-maintained node / palette list in JS: graph vocabulary only from these structs + the catalog |
| Browser HTTP stack | axum 0.7 (Cargo.toml:70) as in webhooks (adapters/inbound/webhooks.rs:68-75, 295-298) · `axum::serve(…).with_graceful_shutdown` (inbound/run.rs:113-115) · SSE `axum::response::sse` (axum's default `tokio` feature) · `futures`, `tokio-stream`, `uuid`, `sha2` already deps · loopback default precedent (config/mod.rs:814-816) | feature `studio = ["axum"]`, not default (pattern `webhooks = ["axum", "hmac"]`, Cargo.toml:84) · assets via `include_str!` · reqwest API tests on a real `127.0.0.1:0` listener | a second server crate (hyper / warp / actix) · WebSocket · Node / npm / CDN · Studio routes in the webhook router (HMAC auth model differs) |

Gaps the later steps close: no `runtime_id` / `run_id` / `config_hash` on audit lines (mod.rs:633-655) · no event envelope or parent links · no tool start / duration (mod.rs:453-480) · a step's tool failure is invisible in `loop/1` (loops.rs:333-339) · per-step legal set not persisted (mod.rs:243-266) · `tengu decide` runs outside the runtime lease (decide.rs:72-82) · Stop = OS signal only (run.rs:50-58) · `tengu prune` deletes `<TENGU_HOME>/logs` (adapters/outbound/prune.rs:79) ⇒ the lab runs under its own `TENGU_HOME`.

## 9. Target runbook

The coding agent must make these commands true or update this section with the final equivalent. State 2026-10-09: every command below ran as written (ST-03, ST-11, ST-20 … ST-31 evidence docs; the Studio quick start again on 2026-10-09, `docs/studio-2026-10-08.md` last line). Run from the repo / worktree root (`--sandbox` is cwd-relative); `target/release/tengu` = `$CARGO_TARGET_DIR/release/tengu` when that is set. Operator doc: `docs/studio-2026-10-08.md`.

### CLI-only proof

```bash
export TENGU_HOME="$HOME/tengu-lab/home"     # FIRST — never ~/.tengu (the weekend run owns it)
cargo build --release                        # or --features studio: one binary serves both proofs
mkdir -p "$HOME/tengu-lab/control-loop-lab/in" "$HOME/tengu-lab/control-loop-lab/out" "$TENGU_HOME"

# One controlled event through the real Jev loop.
echo '{"scenario":"act"}' \
  | target/release/tengu decide --sandbox control-loop-lab --loop demo --event -

# Long-running scheduler and health.
target/release/tengu run --sandbox control-loop-lab
target/release/tengu doctor --sandbox control-loop-lab --live

# Read model + trace (ST-10 / ST-11; logs on stderr, stdout = JSON).
target/release/tengu studio graph --sandbox control-loop-lab [--map sandboxes/control-loop-lab/scenarios/uncertain.map.json]
target/release/tengu trace runs --sandbox control-loop-lab
target/release/tengu trace show --sandbox control-loop-lab --run <run_id> [--after <seq>] [--follow]
```

Required env (names only): `TENGU_HOME` (the lab home above), `OPENROUTER_API_KEY` (Jev); optional `OPENROUTER_BASE_URL`. Export `TENGU_HOME` before anything else: tengu also loads the nearest `.env` above the cwd, and a worktree's parent `.env` sets `~/.tengu`. Every scenario, expected output, troubleshooting row and the guarded cleanup: `docs/control-loop-lab-2026-10-08.md` (acceptance matrix A0–A15). The recorded real-Jev run of all of it: `docs/control-loop-lab-baseline-2026-10-08.md`.

### Studio proof

```bash
cargo build --release --features studio      # the server is behind the non-default `studio` feature
target/release/tengu studio --sandbox control-loop-lab [--port <n>] [--bind 127.0.0.1|::1] [--allow-control]
# stdout: Studio: http://127.0.0.1:<port>/#t=<64 hex token>   (open exactly this URL; a non-loopback --bind is refused)
```

Expected operator flow:

1. Open the printed local URL (the page keeps the token for the tab and drops it from the address bar).
2. See the validated static graph before a runtime exists (`Live`, "waiting: no heartbeat").
3. Attach to a CLI-started run and watch real events colour the graph: each node = its latest event's tone, a lit edge only from an `action.*` / `tool.*` event, grey = not in the step's legal set (all computed in Rust: `/api/v1/runs/<id>/board`).
4. Select an event (timeline) or a node (graph) and compare its Jev answer + probabilities, tool args/result, `call_id`, `decision_id` and evidence paths with `decisions.jsonl` / `tengu trace show`.
5. Switch to Replay (Runs table); drag the `seq` slider; refresh the page and obtain the same run, position, selection and timeline (URL fragment).
6. With control on (`[studio] control = true` — the lab — or `--allow-control`; never a `[generation]`-bound or hardened sandbox): press Play (the `tengu run` start in the Studio process; a CLI `tengu run` is then refused by the lease), pick a scenario and Send event, observe, then Stop (the graceful drain). While a CLI `tengu run` holds the lease, Studio shows `attached` and refuses every action. curl: `POST /api/v1/control/{play,stop,event}` with `X-Studio-Token`, `Origin: <the printed base URL>`, `Sec-Fetch-Site: same-origin`, `Content-Type: application/json` (`docs/studio-acceptance-2026-10-08.md`).

### Cleanup

Cleanup must be explicit and limited to the lab's named workspace/state. Never use a broad recursive target, never touch another sandbox, and preserve the recorded acceptance artifact unless the operator asks to remove it. The block: `docs/control-loop-lab-2026-10-08.md` § Cleanup (guarded by `$TENGU_HOME` = the lab home; removes `out/marker.txt`, the lab observation store, `$TENGU_HOME/logs` and `$TENGU_HOME/state` — trace files and Studio's own recordings included). Then `pgrep -fl 'tengu (run|studio)'` must print nothing.

## 10. Coding rules — mandatory

The coding agent must read and follow `AGENTS.md` before touching code. In particular:

| Rule | Application to this work |
|---|---|
| Rust owns behavior | Runtime, graph construction, trace contract, validation, persistence, APIs, controls, tools and adapters are Rust. |
| Web-only language exception | HTML/CSS/JS/TS may exist only in the isolated Studio frontend. No Python. Keep `AGENTS.md` and `CLAUDE.md` synchronized. |
| Hexagonal architecture | Domain stays pure; ports describe boundaries; use cases live in application; HTTP/store/runtime integrations are adapters; bootstrap only wires. Layering lint must remain green. |
| Config over code | Lab behavior belongs in sandbox TOML and existing tool definitions wherever possible. Do not hard-code a demo workflow in the UI/server. |
| One runtime | Studio reuses `tengu run` semantics, validation, leases, dispatcher, health and shutdown. |
| Safety ceiling | UI/Execution Map may narrow scopes, actions, caps and modes; never widen them silently. |
| W1 freeze | Do not edit `sandboxes/xlab`, `sandboxes/xmarket-weekend`, W1 manifests or pinned W1 capability records. |
| Existing work | Begin with `git status`; preserve every unrelated change. Use a separate worktree/branch while other agents are active. |
| Secrets | Redact before events, storage, logs, API and browser. Never expose environment values or signer material. |
| Egress | Any new network path uses the canonical egress layer and updates its audit/docs. Browser assets are local. |
| Tools | A new tool follows catalog, opt-in, scope, schema, bridge and live-engine-matrix requirements. Avoid a demo-only tool unless approved. |
| Documentation | Every `src/` change updates mapped tutorial pages and their checked date; update code map/runtime/decision-loop/tool docs whenever their claims change. |
| AGENTS twin | A doctrine/top-level/required-reading or language-policy change updates both `AGENTS.md` and `CLAUDE.md`. |

Required reading before implementation:

1. `AGENTS.md` and `CLAUDE.md`.
2. `docs/code-map.md` and `docs/architecture-2026-04-27.md`.
3. Top of `docs/SESSION_HANDOFF.md` and current `git status`.
4. `docs/runtime-2026-09-30.md`.
5. `docs/decision-loop-plan-2026-09-24.md`.
6. `docs/typed-observations-2026-09-24.md`.
7. `docs/egress-2026-09-16.md` and `docs/webhooks-2026-05-11.md`.
8. `docs/tutorial/AUTHORING.md` plus the existing decision-loop/runtime/sandbox tutorial pages.

## 11. Verification and definition of done

### Required automated checks

- `cargo fmt --check` and targeted Clippy if used by the repository workflow;
- domain/application unit tests for graph, event identity, ordering, redaction and state transitions;
- API tests for snapshots, SSE resume/lag and controls;
- integration tests for lease conflict, graceful stop, restart and replay;
- existing layering, scope, config, schema, engine-matrix and tutorial-map tests affected by the change;
- full relevant `cargo test` suite before handoff.

Do not introduce Playwright/Python fixture generators for MVP. Test the backend/API in Rust; record browser acceptance separately with screenshots or a short capture.

### Product definition of done

| Requirement | Proof |
|---|---|
| A new operator can run the lab | Fresh-checkout runbook transcript. |
| Real Jev made the choices | Returned model/decision IDs and redacted decision evidence. |
| The system reacted to time and data | Scheduled and manual scenarios in one replayable run. |
| The graph is truthful | Every highlight references a persisted event and node ID. |
| Failures are understandable | Tool error, uncertainty, stale/missing data and retry states are visually distinct. |
| Nothing dangerous happened | No wallet/signer/order; confined scopes; audit shows no unexpected egress/write. |
| CLI remains sufficient | The same run is diagnosable with `decide`, `run`, `doctor` and evidence files. |
| Existing behavior did not regress | Required repository test matrix passes. |

## 12. Coding-agent handoff format

At each gate, report only:

| Field | Required content |
|---|---|
| Delivered | Task IDs and commit(s). |
| Changed | Files grouped by domain/ports/application/adapters/bootstrap/web/docs. |
| Proved | Exact commands and concise outcomes. |
| Visual evidence | Screenshot/capture plus run/session/correlation ID. |
| Deviations | Any contract change and why it is necessary. |
| Risks/open items | Concrete blockers only. |
| Next gate | What operator approval is required. |

The agent must stop at every numbered gate. Passing tests does not authorize the next gated phase.

