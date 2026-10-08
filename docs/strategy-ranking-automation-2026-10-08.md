# Scheduled strategy ranking — audit, plan and as-built state (2026-10-08)

> **State (branch `feature/strategy-ranking`):** SR-0 to SR-8 are built and tested. Both contracts are committed **unsealed**, so the publisher refuses every real ranking until the operator seals one (SR-1 stop, § Operator decisions). Nothing has run on `~/.tengu/state/xlab`.
> **Merge:** after weekend #2's Monday snapshot (Mon 2026-10-12, gate G-WKND). The frozen weekend binary refuses the new `lineage/rankings/` dir.
> **Tutorial:** [`tutorial/strategy-ranking.html`](tutorial/strategy-ranking.html).

## Decision

| Question | Decision (as built) |
|---|---|
| Separate ranking sandbox? | **No.** The ranker runs in [`sandboxes/xlab-w2`](../sandboxes/xlab-w2/config.toml), the W2 research sandbox. It stays **unbound**: P10 built no W2 generation ([`p10-w2-candidate-2026-10-08.md`](p10-w2-candidate-2026-10-08.md)). |
| Separate ranking component? | **Yes**, in deterministic Rust: the pure ranker [`domain/backtest/ranking.rs`](../src/domain/backtest/ranking.rs) and the coordinator [`application/ranking/`](../src/application/ranking/mod.rs). No LLM computes or reorders a ranking. |
| Modify `xlab` / `xmarket-weekend`? | **No.** Both are W1-frozen and untouched. |
| Scheduler? | `[feeds]` at-ticks under `tengu run --sandbox xlab-w2` (`kind = "tool"`). The deterministic cron fallback is the CLI `tengu ranking run`. A webhook is **not** a fallback: `POST /webhooks/:name` runs an LLM orchestrator turn or queues a decision-loop event ([`webhooks.rs`](../src/adapters/inbound/webhooks.rs) :1-28, route :297). There is no `/v1/message` route. |
| Generated document location? | The state dir, one folder per contract: `<state>/strategy-rankings/<contract id>/<YYYY-MM-DD>/{manifest.json, ranking.json, ranking.md}` + `<contract id>/latest.{json,md}`. |
| Jev? | None. Rules arms only; every row says `evaluation = NOT_GATED` (§ Deviations #1). |

## Implementation audit (as built)

| # | Capability | State | Evidence | Remaining gap |
|---|---|---:|---|---|
| A1 | Daily / weekday / weekend clock, tz, DST | ✅ | feed `at` / `tz` ([`config/feeds.rs`](../src/config/feeds.rs)), `parse_at` / `next_fire` ([`domain/schedule.rs`](../src/domain/schedule.rs) :234, :139); a ranking date's cutoff `ranking_date` ([`application/ranking/mod.rs`](../src/application/ranking/mod.rs) :186), test `the_ranking_date_and_cutoff_follow_new_york_across_both_dst_switches` | — |
| A2 | External cron entrypoint | ✅ | `tengu ranking run \| show` ([`cli/ranking.rs`](../src/adapters/inbound/cli/ranking.rs)): no LLM, no network; exit 1 on INCOMPLETE or a refusal | — |
| A3 | Refresh + backtest primitives | ✅ | `market_history` `fetch = true` (feed `history_refresh`); `resolve` / `prepare` / `evaluate` ([`application/backtest/mod.rs`](../src/application/backtest/mod.rs) :247, :441, :604), `write_run_dir` ([`run_dir.rs`](../src/application/backtest/run_dir.rs) :245) | — |
| A4 | Per-strategy structured evidence | ✅ | run dir; `report.json` records its cohort identity `generation`, `instruments_sha256`, `costs_sha256` ([`domain/backtest/report.rs`](../src/domain/backtest/report.rs) :101-113, `Resolved::identity` in `application/backtest/mod.rs` :123) | reports written before 2026-10-08 lack it ⇒ `cohort_unknown` |
| A5 | Report metrics | ✅ | `Summary` ([`domain/backtest/stats.rs`](../src/domain/backtest/stats.rs) :151) | — |
| A6 | Same-run rules / Jev / HOLD evaluation | ✅, not wired | `tengu evidence evaluate` (b0ca497df3bc427ded2af78aa71e57b76d67f66d) reads gated runs only | a ranking runs no gate: `NOT_GATED` |
| A7 | `data_through_ms`, holdout audit, retention | ✅ | D1–D3 (b2494707e71d05166fe75df2e4338f0aa7949941). Retention `cited_runs` = the bound generation's registry ∪ the `[strategy_ranking]` registry ∪ every `run:` in `<state>/strategy-rankings/*/latest.json` (`application/backtest/mod.rs` :532, :558). Covers an unbound sandbox too | only `latest.json` is scanned: an older dated ranking's runs can be pruned (its `ranking.json` keeps their numbers) |
| A8 | Schedule on `xlab` | out of scope | W1 frozen | intentional |
| A9 | Multi-strategy fan-out | ✅ | one tool call runs the whole chain inside the coordinator (`run_ranking`, `application/ranking/mod.rs` :281); feeds stay `kind = "tool"` | — |
| A10 | Comparable-run selection | ✅ | `select` ([`domain/backtest/ranking.rs`](../src/domain/backtest/ranking.rs) :545): one run per strategy and cohort, every other run listed with a reason | — |
| A11 | Deterministic rating + ascending sort | ✅ | `rank` (:748): lexicographic rating tuple, ties by strategy id | — |
| A12 | Rolling ranked document | ✅ | [`application/ranking/store.rs`](../src/application/ranking/store.rs): manifest, dated files, atomic `latest` | — |

**Completeness:** the end-to-end feature exists and is tested on fixtures. It publishes nothing real until a contract is sealed.

## Contract as built

| Area | As built |
|---|---|
| Record | `lineage/rankings/<id>.toml` ([`domain/lineage/ranking.rs`](../src/domain/lineage/ranking.rs)): `preregistered = true`, `sandbox`, `strategies`, `evidence_class = DEVELOPMENT`, `arm` (`research` · `capped`), `tz`, `cutoff`, `days`, `from`, `cohort`, `on_missing`, `[freshness]`, `[eligibility]`, `[rating]`. Every table denies unknown fields; a shape rule is `invalid_field`. |
| Seal | `tengu lineage seal ranking:<id>` appends `[[sealed]]` to `lineage/locks.toml`. Unsealed = Warn `ranking_unsealed`; changed after the seal = Error `seal_mismatch`. The publisher reloads the registry on every run and refuses `contract_unsealed` / `contract_changed`. |
| Cohort | 9 fields in both contracts: `generation`, `evidence_class`, `arm`, `instruments_sha256`, `costs_sha256`, `interval`, `from_ms`, `to_ms`, `data_through_ms`. Runs rank together only when all are equal; each cohort gets its own table. The window (`from`, cutoff) is always checked. |
| Selection | Registered run identity, never a path: each strategy's `report.json` read back and hashed, its row named `run:<state>/<run id>`. One run per strategy and cohort (the newest run id). First reason wins, in this order: `not_in_contract`, `failed:<stage>` / `stale` / `missing`, `holdout_present`, `arm_missing`, `cohort_unknown:<field>`, `cohort_mismatch:<field>`, `superseded_run`, `excluded_status`, `insufficient_trades` / `insufficient_periods` / `funding_incomplete`, `metric_missing:<key>` / `non_finite:<key>`. |
| Rating | `evidence_tier` → `verdict` → `ci95_lo_bps` → `mean_net_bps` → `-best2_periods_share` → `-max_drawdown_bps`; numbers as `round(x / 0.01)`; tie `strategy_asc`. `evidence_tier` = the highest result class of PASS experiments of the variants carrying the run's `spec_sha256` (a PENDING forward lifts nothing); `verdict` = their weakest status, `UNREGISTERED` when none. |
| Order | Ascending: rank 1 = the weakest of its cohort. |
| Eligibility | `min_trades = 20`, `min_periods = 8`, `max_funding_incomplete = 0`, `exclude_status = ["SUPERSEDED"]`. A missing rated figure is never read as 0. |
| Daily job | `rank.xlab-w2.daily.v1`: 15 strategies (the 19-strategy library minus the four labelled ones: SEC events have no refresh feed). Cutoff 00:00 New York: date D ranks decisions in [2026-03-01, D 00:00) with data through D 00:00, i.e. through the end of D − 1. Feed `strategy_ranking_daily` daily 06:00, after `history_refresh` at 05:00. |
| Weekend job | Its own contract `rank.xlab-w2.weekend.v1`: the 8 `weekend_window` strategies, `days = ["Mon"]`, cutoff Monday 12:00 New York (the 09:30 exits' bars have closed). Feed `strategy_ranking_weekend` Mon 13:00, after the Mon 12:05 refresh. |
| No holdout | No split, ever: a ranking never reads or writes `holdout-reads.jsonl` (test `the_daily_run_never_writes_a_holdout_read`). |
| Forward grading | The standing is read from the registry at rank time. A forward grade landed later lifts the tier only in later dates; published files are never rewritten (test `a_forward_grade_changes_only_later_dates`). |
| Outputs | Contract-level path (§ Decision). Publish order: dated `ranking.json`, dated `ranking.md`, then on COMPLETE `latest.md`, `latest.json` (the commit point), then the manifest's final status. An older `--date` never moves `latest` back. |
| Provenance | `ranking.json` (`strategy_ranking/1`): contract + sha256, sandbox, date, tz, `from_ms`, `cutoff_ms`, arm, evidence class, evaluation, rating order, quantum, tie-break, `generated_at_ms`, status; per cohort its key; per row: run locator, `report_sha256`, `spec_sha256`, variants, tier, verdict, n, periods, mean, median, CI, hit rate, Sharpe, t, max drawdown, best-2 share, mean ex best 5, `data_through_ms`, rating tuple. `content_sha256` (in the manifest) leaves out run ids, report hashes and the generation time, so a rerun on the same data hashes the same. |
| Failure | `on_missing = INCOMPLETE`: a listed strategy that failed, went stale or is missing makes the date INCOMPLETE. The dated files are written, `latest` is kept, the CLI exits 1 and the tool returns `ranking_incomplete: …`. |
| LLM role | None in the chain. The tool's text is the compact ranking (ids and run locators whole), which an agent may quote. |

## Coordinator rules (`run_ranking`)

| Step | Rule |
|---|---|
| Refusals (nothing written) | `no_strategy_ranking`, `contract_not_listed`, `contract_unknown`, `contract_unsealed`, `contract_changed`, `contract_sandbox`, `not_a_ranking_day`, `cutoff_not_reached`, `before_from`, `ranking_busy`. Each error message starts with its code. |
| Date | `--date`, else the newest local date whose cutoff has passed and whose weekday is in `days`. |
| Published date | A COMPLETE or INCOMPLETE manifest: the published ranking comes back and nothing runs. INCOMPLETE is terminal: only deleting `<date>/` reruns it. |
| Lease | `ranking:<contract id>` in the state dir's `runtime.db`, TTL 15 min, renewed before each strategy (`ranking_lease_lost` if another holder took it). Holder `cli:<pid>` or the tool call id. |
| Manifest | `strategy_ranking_manifest/1`: `RUNNING` → `COMPLETE` · `INCOMPLETE`; `FAILED` when the coordinator itself failed. A `RUNNING` / `FAILED` manifest of the same contract sha256 resumes: a `DONE` strategy whose `report.json` still hashes the same is reused. |
| Freshness | Per instrument, the newest stored bar must close at or after cutoff − `max_lag_bars` × interval, else the strategy is `STALE` and does not run. The coordinator never fetches: refresh is the separate feed. |
| Strategy errors | `stage` = `backtest` · `freshness` · `evaluate`; the error text shows the state dir as `<state>` (a copied state ranks to the same `content_sha256`). |

## Deviations from the plan

| # | Plan said | As built | Why |
|---|---|---|---|
| 1 | Evaluate completed runs; provenance includes evaluation status | Every row `evaluation = NOT_GATED`: rules arms only, no Jev gate, `tengu evidence evaluate` not wired | W1 Jev is UNPROVEN (P6); Jev calls cost money and need the network; `evaluate` needs a gated run |
| 2 | Outputs `<state>/strategy-rankings/YYYY-MM-DD/` | `<state>/strategy-rankings/<contract id>/<YYYY-MM-DD>/` + `<contract id>/latest.*` | xlab-w2 runs two contracts in one state dir; the shared `xlab` state could hold more |
| 3 | SR-6: create and bind a W2 generation | xlab-w2 stays unbound; retention reads the `[strategy_ranking]` registry | P10 built no W2 candidate |
| 4 | Signed webhook as the cron fallback | `tengu ranking run` | A webhook is an LLM turn or a loop event, not a deterministic stage |
| 5 | Weekend job: refresh → run → evaluate → publish | A contract of its own plus a Monday refresh tick | The daily contract publishes one ranking per date, so a second feed of it would be a no-op |
| 6 | The coordinator refreshes history | `history_refresh` is a separate feed; the coordinator never touches the network | Feeds chain no stages; a stale instrument is visible as `STALE` |
| 7 | — | INCOMPLETE is terminal and is a tool error (`ranking_incomplete`) | Never rank partial input as complete; under `tengu run` the feed shows `down` and `doctor --live` names it |
| 8 | — | A third list, `dropped` (`not_in_contract`, `superseded_run`), beside ineligible and failed; each rejection has a stable `reason` + a `detail` | Runs that never competed are not ineligible |

## Work plan and tasks (state)

| ID | Task | State | Done by |
|---|---|---|---|
| SR-0 | Phase 6 D1–D3 + evaluation | ✅ | b2494707e71d05166fe75df2e4338f0aa7949941, b0ca497df3bc427ded2af78aa71e57b76d67f66d |
| SR-1 | Preregister the ranking contract | 🟡 drafted, **unsealed** | record kind `ranking`, `seal ranking:<id>`, `ranking_unsealed`: caacdc0be446ce9d7628beacbc31ef0992855c1f; contracts `lineage/rankings/rank.xlab-w2.{daily,weekend}.v1.toml`. The seal is the operator's |
| SR-2 | Deterministic run selection | ✅ | report identity caacdc0be446ce9d7628beacbc31ef0992855c1f; `select` 42584f85ca739b7ee6a0df408726e05488d5deec |
| SR-3 | Pure ranker | ✅ | 42584f85ca739b7ee6a0df408726e05488d5deec; `domain::backtest::ranking::tests` (order, ties, quantum, tiers, cohorts, missing / NaN metrics, content hash) |
| SR-4 | Publisher surface, engine parity | ✅ | CLI 5a4682e8f0c7fce262f94895e2fe9b134f71e4ae; tool `strategy_ranking` + capability `intel.strategy_ranking` 6573e24f44da1eb35ff2f7930348cbd884fa1239 (conformance case `strategy_ranking_hl`, engine-matrix set `xlab_rank`, offline local leg) |
| SR-5 | One-run coordination | ✅ | 5a4682e8f0c7fce262f94895e2fe9b134f71e4ae; `application::ranking::tests` (lease, resume, publish order, stale, DST, forward grade) |
| SR-6 | W2 sandbox + feeds | ✅ unbound | 6a99fe212062036833b8a4bb28e3a6457573262d: `[strategy_ranking]`, private `xl_ranker`, feeds `history_refresh` / `strategy_ranking_daily` / `strategy_ranking_weekend`, `keep_runs = 200`; test `config::feeds::tests::xlab_w2_feeds_validate` |
| SR-7 | Operational acceptance tests | ✅ | 6573e24f44da1eb35ff2f7930348cbd884fa1239: [`tests/strategy_ranking.rs`](../tests/strategy_ranking.rs) on the binary (no-op rerun, restart, INCOMPLETE keeps `latest`, no holdout read, unsealed / edited contract refused) |
| SR-8 | Docs + tutorial | ✅ | this doc, `tutorial/strategy-ranking.html`, lineage / xlab / runtime / tools / code-map / handoff / tracker rows |

## Operator decisions (gate G-SR1, before the first seal)

| # | Question | Default | Note |
|---|---|---|---|
| 1 | Seal `ranking:rank.xlab-w2.daily.v1` as drafted? | go | The rating order, eligibility and freshness above; 15 strategies; cutoff 00:00 New York |
| 2 | Seal `ranking:rank.xlab-w2.weekend.v1` as drafted? | go | 8 weekend strategies, Mondays, cutoff 12:00 New York |
| 3 | Keep `instruments_sha256` + `costs_sha256` in `cohort`? | keep | Strategies with other universes, `exclude` lists, costs or intervals never share a table: the xyz, crypto and `sol_eth_spread` strategies land in separate cohorts |
| 4 | Start `tengu run --sandbox xlab-w2`? | after 1–2 | Before a seal every ranking feed errors `contract_unsealed` and shows `down` |
| 5 | Changing a contract later | new id (`…v2`) | An edit after the seal is `seal_mismatch`; the publisher refuses `contract_changed` |

## Open issues

| Issue | Effect |
|---|---|
| INCOMPLETE is terminal | A transient failure (a missed 05:00 refresh ⇒ STALE) leaves that date INCOMPLETE; rerun by deleting `<state>/strategy-rankings/<contract>/<date>/` |
| A run cut by shutdown keeps its lease | The next holder gets `ranking_busy` until the 15-min TTL ends, then resumes the manifest |
| Shared `xlab` state dir | The W1-bound `xlab` sandbox (`keep_runs` 100) can prune xlab-w2 ranking runs that no `latest.json` cites; each dated `ranking.json` keeps the numbers |
| Not run live | The engine-matrix legs `*_xlab_rank` (API keys, a Claude login, the operator's PC) and `tengu run --sandbox xlab-w2` (its `xl_gate` loop needs `OPENROUTER_API_KEY` to build) |

## Sequence

`SR-0 → SR-1 (drafted) → SR-2/SR-3 → SR-4 → SR-5 → SR-6 → SR-7/SR-8` ✅ → **G-WKND** merge → **G-SR1** operator seal → `tengu run --sandbox xlab-w2`.

The SR-1 stop holds: the rating and comparability policy shape research conclusions, so the operator seals the contract before any ranked outcome is observed. Everything above ran on test fixtures only.
