# xmarket — build plan (2026-09-30)

How the next sessions build everything in the PRD. **What** to build: [`xmarket-tracker-2026-09-29.md`](xmarket-tracker-2026-09-29.md) (185 items, milestones E0, M0–M8, M3b; rules R1–R13 and the definition of done in its § 0). **Why**: [`xmarket-prd-2026-09-29.md`](xmarket-prd-2026-09-29.md) + its operator addendum. **Evidence**: [`xmarket-feasibility-2026-09-30.md`](xmarket-feasibility-2026-09-30.md) (attached as a warning). Per-item detail: [`xmarket-gaps-2026-09-29.md`](xmarket-gaps-2026-09-29.md).

## Status (2026-10-08)

| Topic | State |
|---|---|
| Since 2026-10-02 | W1 + xlab merged to main 2026-10-02 (#20 `131af134b57191850c249008a50b2ec05a59ed53`, #21 `b7dc915149cc025cc9466748905ab18ca46e075a`); weekend #1 ran and was graded ([`w1-p0-weekend-2026-10-06.md`](w1-p0-weekend-2026-10-06.md)); `TENGU_ROADMAP.md` P0–P11 done — W1 frozen in `lineage/`, no change beats rule W, W1 kept ([`p10-w2-candidate-2026-10-08.md`](p10-w2-candidate-2026-10-08.md)); strategy rankings merged, contracts unsealed (#40); weekend #2 sealed ([`forward-evidence-runbook-2026-10-08.md`](forward-evidence-runbook-2026-10-08.md)) |
| W1 | ✅ done 2026-10-01 on `feature/xmarket` (merged 2026-10-02, row above): 34 items + the gate — three read-only adversarial reviews (weekend path, money safety, engine parity / doctrine), every confirmed finding fixed (`6fcb455bae5553e2d51390e4cedd334779ca0d9c`, `0fd620b5fc5f368ba75538621b8b9fb1718156e9`, `270f23e4f144503ca4e7564cdb6ef1118855d5f3`, batch 2) or recorded as an operator decision. Tracker § 0 + W1 notes; commits per item: `docs/IMPLEMENTATION_PLAN.md` § X |
| Sequencing change | **History first** (operator rule 2026-10-01): sandbox `xlab` ([`xlab-2026-10-01.md`](xlab-2026-10-01.md)) pulled the M7 replay work forward — `ops-clock-port` ✅; `ops-replay-harness`, `ops-report-decisions`, `x-hl-historical-backfill` 🟡. Each later wave is judged by replay on backfilled history before it runs live; live recording only for data with no historical source |
| Evidence on history | xlab § 14: rule W +50.4 bps (n 1,500, CI +15.7 … +81.5), liquid entries' holdout +72.2 (CI +6.3 … +130.9); every other library strategy no-go after costs; Jev gate: no evidence yet that it adds value |
| Weekend run | #1 done 2026-10-05; #2 2026-10-09 → 10-12 on the frozen `tengu-acdef66` (runbook above); the frozen-binary rules hold (tracker § 0 step 2a) |
| Next | forward rule W evidence each weekend (M3 needs 12, 1 so far); the operator decisions (tracker W1 notes: 3 open, 2 moot since 2026-10-07) → W2 (§ Waves; inputs: VPS + SSH alias, dedicated OpenRouter key; prompt in § Kickoff) |
| Machine load | operator 2026-10-01 (supersedes "one agent at a time"): workflows / parallel agents allowed, ≤ 3 at once, each build in its own `CARGO_TARGET_DIR` under `~/.cache/tengu-xm.noindex/`; never a local model on this Mac |
| Engine matrix | 18 tool sets (W1: 13, + `xlab`, `xlab_holdout`, `xlab_rank`, `sources`, `soe`; `tests/engine_matrix.rs` `Set::ALL`); `xlab_rank` / `sources` / `soe` live legs not run yet; W1-gate live run 39 / 39 on `11900f78894867e3427ca828d10ec80b4d3d4e4b`; xlab sets: haiku-4.5 + Claude CLI green, gemini-2.5-flash-lite misquotes numbers (xlab § 14); `local` legs on the operator's PC |

## Mandate (operator, 2026-09-30)

| # | Requirement |
|---|---|
| 1 | Build the full PRD scope — every tracker item, including the parts the feasibility study found no edge for |
| 2 | Every tool — existing and new — works 100 % under `engine = "openrouter"`, `"local"` and `"claude_code"` (subscription). No exceptions |
| 3 | A separate sandbox for the weekend investigation run (`sandboxes/xmarket-weekend`) |
| 4 | Everything tested and validated; self-validate and fix until it runs smoothly. Use workflows, subagents and `/loop` as needed; take the time needed |
| 5 | Docs adjusted in the same commits; the system is ready to run |
| 6 | Real money moves only with the operator's explicit go (M3b sends, deposits); everything else runs on paper, simulate or testnet |

## Before the first item (✅ done 2026-09-30)

| Step | Command / action |
|---|---|
| 1 | `git switch -c feature/xmarket` from `main`, then commit the uncommitted planning docs as the first commit (`CLAUDE.md`, `AGENTS.md`, `docs/SESSION_HANDOFF.md`, `docs/tools.md`, `docs/mcp-bridge.md`, `docs/engine-backends.md`, `docs/egress-2026-09-16.md`, `docs/xmarket-*.md`) — explicit paths, never `git add -A` (done: `47fc090de426205df91425df789849466e962399`) |
| 2 | Baseline: `cargo check --all-features`; `cargo test --test layering_lint --test scope_lint --test code_map`; one scoped unit run (`cargo test --bin tengu decision_loop`). Record failures that pre-exist — they are not yours to hide |
| 3 | Engines ready: `OPENROUTER_API_KEY` in `.env`; Ollama with `gemma4:latest` on the operator's Windows PC (operator rule 2026-09-30: never on this Mac); `claude --version` works and is logged in (subscription). `cargo build --release --features claude_code,webhooks` |
| 4 | Leave the throwaway weekend sampler running (`~/.tengu/state/xmarket/research/weekend-2026-10-02/`, pid in `sampler.pid`) — it is the fallback for the weekend data |

## Operating model

| Topic | Rule |
|---|---|
| Unit of work | One tracker item = one commit (W1: on `feature/xmarket`, merged as #20; since then a feature branch off main, one PR, squash-merged) whose message starts with the item id; code, tests and docs together; tick the item (✅ + short hash) in the tracker in the same commit |
| Machine load (operator 2026-09-30, relaxed 2026-10-01) | On the operator's Mac: low priority (`renice` + `taskpolicy -b`, `CARGO_BUILD_JOBS=2`, build dirs under a `.noindex` folder), no local models. Since 2026-10-01 workflows / parallel agents are allowed: ≤ 3 at once, each build in its own `CARGO_TARGET_DIR` under `~/.cache/tengu-xm.noindex/` (was: one agent at a time) |
| Parallelism | Workflows with `isolation: "worktree"` for items that touch disjoint files. Shared files — `src/adapters/outbound/tools/mod.rs` (catalog), `src/domain/tools.rs`, `src/config/mod.rs`, `docs/code-map.{md,html}`, the tracker — are edited only by the coordinator when merging. Merge one worktree at a time and run the item gate after each merge |
| Order | Follow the waves below; inside a wave follow the tracker's M-table order unless the "After:" lines in the gaps doc allow parallel work |
| Loop | For long waves run `/loop` (self-paced): each iteration takes the next ☐ item of the current wave, implements it, runs the item gate, commits, ticks. The loop stops at a wave gate for the full validation |
| Tests | Scoped runs only, ≤ 30 s each (`cargo test --bin tengu <filter>`); `cargo check --all-features` and the lints at wave gates; no network in unit tests (replay fixtures committed as JSON, produced outside the repo) |
| Subagent prompts | Name the item id, the gaps-doc entry, the files it owns, the tests to run; forbid `git add -A`, pushes, and edits to shared files |
| When stuck | Fix forward. If an item's design in the gaps doc is wrong, correct the doc in the same commit and note it in the tracker. Ask the operator only for the inputs in § Operator inputs |
| Context | Keep the tracker, `docs/SESSION_HANDOFF.md` and memory current after every wave, so a new session can resume from docs alone |

## Waves

| Wave | Goal | Items | Gate |
|---|---|---|---|
| W1 ✅ 2026-10-01 | Engine parity foundation + the weekend investigation run | E0 (all 7) · M0: `ops-sandbox-config`, `x-shared-workspace-and-state-layout`, `rt-daemon`, `rt-backoff-budget`, `rt-scheduler` (incl. clock-time ticks in `America/New_York`), `hl-info-client`, `hl-market-schema`, `hl-ctx-tool`, `hl-book-tool`, `risk-config-schema`, `risk-calc-costs`, `risk-paper-ledger-domain`, `risk-gate-domain`, `risk-paper-ledger-store`, `risk-paper-fill-engine`, `risk-exec-idempotency-ids`, `risk-gate-enforcement`, `risk-paper-tools`, `risk-kill-switch`, `risk-audit-verdicts`, `x-exit-rules`, `ops-audit-atomic-write`, `rt-health`, `x-weekend-fade-strategy`, `x-weekend-sandbox` · M1: `kg-calendars`, `ops-history-recorder` | Wave gate green and a 30-minute live soak of `tengu run --sandbox xmarket-weekend` **by Fri 2026-10-02 18:00 ET** → it runs Fri 19:30 ET → Mon 10:00 ET. If not green by then, do not start it; the throwaway sampler covers the data and the sandbox runs the next weekend |
| W2 | Rest of M0 | `info-fetch`, `info-parsers`, `info-edgar`, `jev-event-key`, `jev-event-templating`, `risk-calc-tools`, `jev-xmarket-loops-toml`, `ops-deploy-compose`, `x-m0-e2e-test`, `ops-openrouter-budget-key`, the four M0 docs items | M0 exit check (tracker § 1) + deploy to the operator's VPS |
| W3 | M1 + M2 | all M1 and M2 items | M1 and M2 exit checks; the VPS runs 24 h unattended |
| W4 | M3 edge check | all M3 items + analysis of the recorded weekends | Verdict recorded in the tracker (go / re-scope / stop) |
| W5 | M3b live pilot | all M3b items — build + testnet; mainnet send only after an M3 "go" and the operator's funded sub-account | M3b exit check on testnet; one $10 mainnet IOC only with the operator present |
| W6 | M4 information layer | all M4 items | M4 exit check (needs the X token) |
| W7 | M5 event ↔ asset, Jev, slow path | all M5 items | M5 exit check |
| W8 | M6 + M7 | all M6 and M7 items | M6 and M7 exit checks; ablation report |
| W9 | M8 extensions | all M8 items | M8 exit check |

History first (operator rule 2026-10-01): a wave's question is answered on backfilled history wherever history exists, before any live wait — xlab (`tengu backtest`, the Jev gate arm; [`xlab-2026-10-01.md`](xlab-2026-10-01.md) § 6–7) already covers part of M7 (`ops-clock-port` ✅; `ops-replay-harness`, `ops-report-decisions`, `x-hl-historical-backfill` 🟡). Live recording only for data with no historical source (e.g. executable xyz weekend books).

## The weekend investigation sandbox (`sandboxes/xmarket-weekend`)

| Part | Spec |
|---|---|
| State (2026-10-08) | weekend #1 ran 2026-10-02 → 10-05 on `tengu-6fcb455` and was graded with `tengu evidence` ([`w1-p0-weekend-2026-10-06.md`](w1-p0-weekend-2026-10-06.md)); weekend #2 runs `tengu-acdef66` ([`forward-evidence-runbook-2026-10-08.md`](forward-evidence-runbook-2026-10-08.md)); the sandbox is bound to the frozen W1 (`[generation]`) |
| State (2026-10-02) | Built (`bc510871c69db05b61a99ba29cfe6787b906e91f`), golden replay bit-exact, 30-min soak green; W1-gate fixes `6fcb455bae5553e2d51390e4cedd334779ca0d9c`, frozen as `~/.cache/tengu-xm.noindex/weekend/tengu-6fcb455`, soak 2 green. The run is optional now (history first: xlab measures rule W over ~30 weekends); it adds only executable xyz weekend books |
| Purpose | First out-of-time evidence for the weekend fade (W) at executable prices; a real run of the new runtime |
| Process | `tengu run --sandbox xmarket-weekend` on this Mac, started under `caffeinate -i -s` (awake while the run lives; runbook in the sandbox file), state in `<TENGU_HOME>/state/xmarket-weekend/` |
| Feeds | `hl_ctx` for every listed xyz market every 60 s; `hl_book` for every listed xyz single-stock perp every 5 min (every 60 s from Sun 17:00 to 19:00 ET and Mon 08:30 to 09:30 ET); recorder on for both |
| Strategy | `x-weekend-fade-strategy`: at Sun 18:00 ET (last closed day before a trading day) compute s = ln(HL at 18:00 / HL at Fri 20:00 ET) per name; paper-fade every eligible name in a shadow ledger (no cap, depth-walk fills) and the top 4 with \|s\| ≥ 50 bps at $25 each in the `[risk]`-capped ledger; exit both at Mon 09:00 ET |
| Cost profile | Floor: no LLM, no X, Jev off; network `open` |
| Acceptance | Offline: replay of the 2026-09-26 → 09-28 weekend from recorded candles gives the same signals and fills as the feasibility scripts. Live: a 30-minute soak on Thursday or Friday with `tengu doctor --live` green, recorder rows growing, no errors in the audit |
| Monday | Compare shadow and capped ledgers with the throwaway sampler's books; add the result to the feasibility report — since 2026-10-06: vault snapshot + `tengu evidence grade` / `regrade` + a graded lineage record (runbook § Monday grading) |

## Engine parity — validation matrix

| Engine | How tools run | Offline check | Live smoke |
|---|---|---|---|
| `openrouter` | in-process `PluginToolExecutor`; OpenAI-style function schemas | `x-tool-schema-lint`: every catalog schema in the subset OpenRouter's providers accept (names `^[a-zA-Z0-9_-]{1,64}$`, no `$ref` / `oneOf` at the top level, bounded description length) | `x-engine-matrix-smoke` with `google/gemini-2.5-flash-lite` and `anthropic/claude-haiku-4.5`: a scripted turn that calls each tool of the set under test and reads the result |
| `local` | in-process; OpenAI-compatible server (Ollama, llama.cpp, vLLM, Unsloth) | same lint, plus `x-local-model-fit`: tool results fit `limits.context_window` (compact rendering, bounded `data`); offline mock legs `offline_local_*` | Ollama `gemma4:latest` on the operator's Windows PC over the LAN (`TENGU_MATRIX_LOCAL_BASE_URL`; never on this Mac) |
| `claude_code` | Claude CLI → `tengu mcp-bridge` (stdio MCP) | `x-bridge-conformance-test`: same result through the bridge as in-process (config, scopes, stores, secrets + redaction, `no_shell`, call id) | `claude -p` on the subscription with `builtin_tools_profile = "none"` and `--strict-mcp-config`, same scripted turn |

A tool is done only when all three rows pass for it (R1). `x-engine-parity-audit` runs the matrix over every existing catalog tool first and fixes what fails — done 2026-10-01 (`1f52df0e711904d62a14a279ecba6b430511a8f8`): every catalog tool, a shell skill and an `[[mcp_servers]]` proxy pass on `openrouter` and `claude_code`; the live `local` column waits for the operator's PC.

## Gates and self-validation

| Level | Checks |
|---|---|
| Item | Tracker § 0 definition of done: unit tests, bridge conformance + schema lint for tools, lints, docs, tick |
| Wave | `cargo fmt --check`; `cargo check --all-features`; `cargo test --test layering_lint --test scope_lint --test code_map --test run_agent_ipc --test mcp_bridge_external`; the scoped unit suites the wave touched; engine matrix live smoke for every tool the wave added; the milestone's offline e2e test; a live soak of the sandbox (≥ 30 min, `tengu doctor --live` green) |
| Milestone | The milestone exit check (tracker § 1), then an adversarial review workflow — 2–3 reviewers with distinct lenses (correctness vs code, engine parity + doctrine, ops / money safety) — and fix every confirmed finding before moving on |
| Fix loop | Any red check: fix, re-run the same gate, repeat until green; record anything waived (and why) in the tracker |

## Operator inputs

| Input | Needed for | When |
|---|---|---|
| Mac on, lid open, online Fri 2026-10-02 19:30 ET → Mon 2026-10-05 10:00 ET (Sat 04:30 → Mon 19:00 UTC+5) | weekend run + sampler — optional since 2026-10-01 (history first); done. Each later weekend: the runbook's times | W1 |
| Ollama with `gemma4:latest` on the operator's **Windows gaming PC** (reached over the LAN; never on the dev Mac), `OLLAMA_HOST=0.0.0.0`, `OLLAMA_CONTEXT_LENGTH=16384`, its address; Claude CLI logged in | engine matrix (`local` column live check) | when the operator says go |
| Dedicated xmarket OpenRouter key, $40 / day limit | 24/7 runs | before W2 deploy (the current key is fine for development) |
| VPS choice (Hetzner or Hostinger) + an SSH alias; Docker installed | `ops-deploy-compose` | W2 |
| Alpaca key (reference equities) | `rh-ref-equities` | W3 |
| X bearer token + spending limit | `info-x-ingest` | W6 |
| Hyperliquid sub-account funded with $100 + API wallet (testnet first) | M3b | W5, after an M3 "go" |

## Kickoff prompt for the next session (W2)

The W1 prompt (used 2026-09-30) is in this file at `47fc090de426205df91425df789849466e962399`.

```text
Implement xmarket wave W2. Read, in order: CLAUDE.md (required reading #10, #11, #13),
docs/SESSION_HANDOFF.md (top), docs/xmarket-build-plan-2026-09-30.md (Status, § Waves),
docs/xmarket-tracker-2026-09-29.md § 0 and the W1 notes (the open operator decisions),
and docs/xlab-2026-10-01.md § 12–14. Confirm the operator decisions first. History
first: judge each W2 piece on xlab history before it runs live. W2 = info-fetch,
info-parsers, info-edgar, jev-event-key, jev-event-templating, risk-calc-tools,
jev-xmarket-loops-toml, ops-deploy-compose, x-m0-e2e-test, ops-openrouter-budget-key,
rt-docs, hl-docs, x-info-docs, risk-docs; gate = the M0 exit check (tracker § 1) +
the deploy to the operator's VPS. Every tool works under openrouter, local and
claude_code (R1). One commit per item on a feature branch off main, explicit paths;
≤ 3 parallel agents, no local model on this Mac. Stop only for the operator inputs
listed in the build plan.
```
