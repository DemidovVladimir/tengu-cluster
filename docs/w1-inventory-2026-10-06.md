# W1 — inventory, gap analysis, verification, known defects (2026-10-06)

`TENGU_ROADMAP.md` Phase 1 (G1). W1 is defined from the code, not from intent. Review package: [`w1-review-2026-10-06.md`](w1-review-2026-10-06.md) · design: [`lineage-2026-10-06.md`](lineage-2026-10-06.md).

## 1. What W1 is (as run vs as merged)

| Part | As run (forward, 2026-10-02 → 05) | As merged (`main`) |
|---|---|---|
| Code | binary `tengu-6fcb455` from `6fcb455bae5553e2d51390e4cedd334779ca0d9c` (sha256 `e2c3bb8f88f25d4a6a9275200b7b248ac8a052db16e344fb674da4e208320c72`; only on `origin/feature/xmarket` — now also tag `w1-forward-2026-10-02`) | `2aa79717f5cfb1d8a8211672c50ecaed18b568a7` (#20–#24) — 16 later W1-gate fix commits in the xm / paper / runtime code |
| Config | `sandboxes/xmarket-weekend/config.toml` at `e5daae83febea00e549ce9b8cac83bddce12a0a2` (tag `w1-forward-config-2026-10-02`) | same rule, risk, feeds; recorder `["*"]` only on the weekend branch (PR #25) |
| Research | xlab runs of 2026-10-01 (binary from the `feature/xlab` branch, commit not stamped) | `sandboxes/xlab/config.toml` (9 strategies; +2 weeknight specs on PR #26) |
| Desk recorder | `tengu-desk-2026-10-02` (sha256 `13ee5f988dc100e140d3cd73da233aca4a7ef14581151ac67ce5be65c786e8a0`, commit UNKNOWN) | `sandboxes/xmarket` (not part of W1's decision path) |

## 2. Architecture inventory

| Subsystem | Where | W1 role |
|---|---|---|
| Runtime `tengu run` | `adapters/inbound/run.rs`, `bootstrap/runtime.rs`, `application/runtime/{mod,feeds,loops,health}.rs`, `domain/schedule.rs` | feeds on a UTC grid, leases, heartbeat, `doctor --live` |
| Recorder | `config/recorder.rs`, `outbound/{observations,history_sqlite}.rs` | live day files `<state>/history/<YYYYMMDD>.db` (`source = 'live'`) |
| Rule W (forward) | `domain/xm/weekend_fade.rs` (signal, selection, golden replay), tool `outbound/tools/xm/weekend_fade.rs`, exits `domain/xm/exits.rs` + tool `xm_exits` | Sun 18:00 NY fade top 4 \|s\| ≥ 50 bps at $25 (capped) + every name at $100 (shadow), exit Mon 09:00 NY |
| Paper ledger | `ports/paper.rs`, `outbound/paper_store.rs`, `domain/xm/{ledger,paper,cost}.rs`, `application/paper.rs` | fills by depth walk of the live book after simulated latency; fees per HL tier / deployer scale; hourly funding |
| Risk gate | `domain/xm/{risk,risk_state}.rs`, `outbound/tools/xm/exec_common.rs`, `config/risk.rs` | inside every exec tool; fails closed on any missing input; KILL file; shadow skips budget rules only |
| Warehouse | `outbound/market_data.rs`, `outbound/backfill/`, CLI `history` | `market.db` (1h / 1m bars, funding, ctx) — backfill only |
| Backtest engine | `domain/backtest/` (pure), `application/backtest/{mod,run_dir,gate}.rs`, CLI `backtest` | six kinds, research + capped arms, costs, funding, splits, bootstrap CIs, time-integrity checks |
| Holdout discipline | `outbound/tools/xlab/holdout.rs` | tool hides the holdout; `holdout: true` reads counted in `holdout-reads.jsonl` |
| Jev | `[decision_loops.xl_gate]` (pin `typesafe/jev-1.13-20260917`), `application/decision_loop/`, `outbound/decision_cache.rs`, `bootstrap/decision.rs` | gate arm replayed on a `SimClock`, cache by full sha256 → offline reruns identical |
| Architect | `xl_architect` (Claude CLI, `claude-opus-5-5`, profile `none`) + skill `xlab-research`, tools `market_history`, `backtest` | proposes specs (data), reads holdouts once |
| Tests | 1,514 unit tests; `tests/{layering_lint,scope_lint,code_map,run_agent_ipc,bridge_conformance,mcp_bridge_external,engine_matrix}.rs` | § 4 |
| Evidence on disk | `~/.tengu/state/{xmarket-weekend,xmarket,xlab}/`, `~/.tengu/logs/`, workspace `observations.db` files | preserved in the vault `w1-2026-10-06` (P0) |

## 3. Gap analysis — handoff requirement → REUSE / EXTEND / NEW / DEFER / REMOVE

| Requirement (handoff §) | Class | Decision |
|---|---|---|
| Forward experiment environment (§ 4) | REUSE | xmarket-weekend + `tengu run` + ledger + gate, unchanged |
| Historical environment, strategy as data (§ 5, § 30) | REUSE | xlab engine, specs, run dirs |
| Point-in-time integrity (§ 29) | REUSE + EXTEND | `domain/backtest/checks.rs` unchanged; episodes refuse information timed after the decision (`future_leakage`) |
| Deterministic risk (§ 19) | REUSE | no change; generation binding can only narrow what a sandbox loads |
| JEV as an unproven candidate (§ 16) | REUSE | gate arm + cache; W1 marks the arm `UNPROVEN` |
| Evidence preservation, immutability (roadmap § 2) | NEW | `tengu evidence snapshot / verify`: no immutable copy existed and three paths delete evidence (§ 5 D3, D4, the 90-day recorder sweep) |
| Live vs backfilled (§ 26, § 49) | EXTEND | `Provenance` enum over the existing markers (`obs_history.source`, `market.db` `fetched_at_ms`); `tengu evidence coverage` |
| Forward grading, weekly (roadmap § 7) | NEW | `tengu evidence grade` (ledger + reconciliation) and `regrade` (recorded books, reuses rule W + the book walk): Monday grading was ad-hoc SQL with no saved output |
| Experiment registry (§ 20) | NEW | `lineage/experiments/` + `tengu lineage`; references run ids and the vault, copies no data |
| Variant registry, search count (§ 21) | NEW | `lineage/families/`, `variants/`; attempts = run dirs by `spec_sha256` + `holdout-reads.jsonl` |
| Experience episodes (§ 22–24, § 37) | NEW | `lineage/episodes/`: decision, execution, outcome quality stored apart |
| Generation manifest + immutability (§ 33–34) | NEW | `generations/W1.toml` pins + `locks.toml`; `[generation]` checked at every config load |
| Capability registry (§ 31–32) | NEW (minimal) | binds existing tools and strategy kinds; helpers stay unregistered |
| Information timing (§ 27), source provenance (§ 28) | EXTEND → DEFER | `available_at` per episode item now; publication / received times with the news layer (P7) |
| Counterfactuals (§ 39) | EXTEND | the shadow ledger is the rejected candidates' counterfactual; episode alternatives; systematic arms in P6 |
| Calibration (§ 17) | REUSE → DEFER | gate arm Brier exists; formal comparison P6 |
| HIP-3 oracle research (§ 40) | DEFER | P8 |
| Recorded books into xlab (§ 49) | DEFER | P9; the vault keeps every recorded book |
| Experience retrieval, evidence packets (§ 35–36) | DEFER | P14 (operator review #2) |
| Research cost, tool metrics (§ 68–69) | DEFER | `MetricsRecord` + gate cost exist; aggregation later |
| Synthetic fixtures (§ 47) | NEW (part) | future leakage, lucky-bad, good-unlucky, generation isolation, unavailable capability, NO ACTION, accounting corruption: test fixtures now; duplicate / late / noisy sources, regime shift: P7 |
| M0 foundation / M1 capability system / M2 synthetic world from scratch (§ 9) | REMOVE | not built — X Market and X Lab already are that infrastructure |

## 4. Verification (G1)

| Claim | Evidence | Result |
|---|---|---|
| Unit suite on W1 code | `cargo test --bin tengu` on `653bff0` | 1,514 passed · 0 failed · 28 ignored (live / network legs) |
| Lints + offline integration | `layering_lint`, `scope_lint`, `code_map`, `run_agent_ipc` (7), `bridge_conformance` (3), `mcp_bridge_external` (3), `engine_matrix` offline (5) + `every_catalog_tool_has_a_live_leg` | all pass |
| Deterministic backtest | the 2026-10-01 library re-run in an isolated `TENGU_HOME` (clones of `market.db`, `decision-cache.db`) | 8 of 10 runs byte-identical (report, trades, candidates, skips); the 2 funding-carry runs differ — data, not code (D1) |
| Jev replay / cache | `20261001T182905Z-weekend_fade` re-run `--offline --max-decisions 1500` | identical report and trades; 1,500 cache hits; decisions differ only in `latency_ms` |
| Point-in-time | `checks.rs`: `decisions_at_or_before_t_ignore_what_comes_after`, `arms_at_or_before_t_…`, `every_kind_reads_nothing_after_its_decision`, `a_bar_is_observable_only_at_its_close` | pass |
| Typed strategy validation | `spec.rs` tests, `strategies_are_checked_at_load` | pass |
| Holdout controls | `holdout.rs` tests (`a_split_hides_the_holdout_until_it_is_read_and_reads_are_counted`) | pass — CLI reads uncounted (D2) |
| Paper accounting | `ledger.rs`, `paper.rs` (`fixture_fills_match_hand_computed_goldens`), P0 ledger reconciliation | pass (P0 report) |
| Risk-gate placement | `risk.rs` (`never_allows_an_entry_with_a_missing_input`), `exec_common.rs` (`the_kill_switch_is_probed_again_inside_the_transaction`) | pass |
| Regressions | `golden_replay_2026_09_26`, `golden_weekend_window_equals_the_rule_w_replay` | pass |

## 5. Known defects (recorded, not fixed — W1 is frozen)

| # | Defect | Effect | Proposed |
|---|---|---|---|
| D1 | A run's identity (`spec_sha256` + window) does not pin the warehouse; exits may read bars past `to_ms` | reruns change when `market.db` grows (2 funding-carry runs) | record `data_asof` in `report.json`; bound exits by it (P6) |
| D2 | `tengu backtest --split` prints the holdout without counting the read | weeknight holdout reads (2026-10-05) not in `holdout-reads.jsonl` | count CLI reads too (P6) |
| D3 | `[backtest] keep_runs = 100` prunes the oldest run dirs | registry-referenced runs can disappear from `state/` | vault (done) + never prune a run the registry references (P6) |
| D4 | `observations.db` purges rows > 7 days on open | `xm_weekend/1:2026-10-02` gone after ~2026-10-12 | vault (done); day files keep it (recorder `"*"`) |
| D5 | No build stamp: a binary carries no commit | the desk binary's commit is UNKNOWN; W1 records commits externally | stamp the commit at build (P10) |
| D6 | The forward binary predates 16 W1-gate fix commits | W1 "as run" ≠ W1 "as merged" in xm / paper / runtime code | W1 manifest names both; W2 forward runs `main` |
| D7 | During the 2026-10-02 sleep the weekend `hl_ctx` slot 23:17 never ran and was not warned (23:16 was); `dropped` = 2 for the run counts only warned skips (cause not diagnosed) | silent data gaps | diagnose; count every skipped slot (P6) |
| D8 | xlab's flat xyz cost (0.9 bps taker) vs observed ~9 bps on `xyz:MSTR` / `xyz:PURRDAT` | development figures optimistic for those names | per-name fees from recorded `mkt_instrument/1` (P9) |
| D9 | xlab's time holdout (from 2026-07-01) was already seen by the feasibility study and its names holdout | rule W's xlab holdout is CONTAMINATED; the clean out-of-sample evidence is forward weekends only | registry marks it; future holdouts preregistered |
| D10 | `spec_sha256` includes `name` and `@universe` by name | a rename is a new identity; a universe edit is not | variants keep both hash and resolved universe (P6) |
| D11 | Paper funding is booked at the rate observed at booking time (HL per-hour history is not read); a late booking after a gap takes a later observation | 2026-10-05 08:00 hour booked at 08:04:35Z, capped +$0.015 (`inc.2026-10-05.funding-booked-late`) | settle from HL `fundingHistory` when an hour is booked late (P6) |
