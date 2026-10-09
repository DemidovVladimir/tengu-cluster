# SESSION_HANDOFF.md — Tengu-Cluster running state log

> Restored 2026-05-14. The file was dropped from the tree during the
> agentic-memory rework branch; `CLAUDE.md` / `AGENTS.md` still list it as
> required reading, so it is back. Keep the top section current.

---

## 2026-10-09: sealed keys — branch `feature/sealed-keys` (`docs/sealed-keys-2026-10-09.md`)

| Item | State |
|---|---|
| Design (simplified + hardened 2026-10-09) | provider keys = Worker secrets the operator sets in Cloudflare; `wrangler.toml` `ROUTES` (route → upstream, secret name, exactly one key position `header` / `path` / `query`, optional `allow` path prefixes; anything else 403) + `CLIENTS` (fingerprint → label, session hours, routes; empty = none, unknown name = 500); secret `SESSION_KEY`; sessions route-scoped (signed `routes:` line, granted = asked ∩ `CLIENTS`, each call needs the route in the session and in `CLIENTS`). Gone: blobs, HPKE, `KeyVault` Durable Object, KV allow-list, `tengu keys seal`, `[keys] pubkey` / `sealed_dir` |
| Built | `crates/tengu-seal` (`route` / `session` / `ssh` / `target` — 20 tests, `shipped_wrangler_toml_parses` among them) · `cloudflare/seal-worker` (Rust Worker: `/healthz`, `/session`, `/whoami`, `/<route>/…`, `Tengu-Route`; reply masking: header values, text / JSON / XML ≤ 2 MiB on bytes, larger streams on (`masked` = -1), event streams / binary pass, HEAD + 101 / 204 / 205 / 304 body-less; one JSON log line; wasm 663 KB) · `[keys]` + `[keys.env]` route values + `strip`; `@session` only `OPENROUTER_API_KEY` next to an `OPENROUTER_BASE_URL` route; proxy canonicalized to its origin · `tengu keys setup` (prints the config's routes) / `status` (`granted …`) / `check <route>` (asks for that route) · `install` at `load_sandbox_or` and in `run-agent` / `mcp-bridge` children (reuse the session, strip again; fail closed: unsets `[keys.env]`) · `registry_after_keys` (extends the registry, never rebuilds) · Telegram refuses to start without a session when `[keys.env]` routes it · RPC / Telegram session header · shipped routes `openrouter` (allow 4 paths), `telegram`, `solana-rpc`, `evm-rpc` · sandbox `sealed-check` + guard test `config::keys::tests::sealed_check_sandbox_is_harmless` + runbook `docs/sealed-check-2026-10-09.md` + `docs/sealed-check-evidence/` · tutorial `sealed-keys.html` · operator walkthrough `docs/cloudflare-secrets-setup.html` |
| Verified | before the hardening, local `wrangler dev`, fake secrets, through httpbin: bearer 200, header template 200, wrong creds 401, echoed key masked, missing secret 500, dead upstream 502, path escape never forwarded, unknown route 400 · hardened, local `wrangler dev`, real OpenRouter key in the local Worker only, `sealed-check`: Jev `convert` `0xff` → `255`, tool error, dry run, maps, `tengu run` + `doctor --live`, Studio Play / Send / Stop, N1 fail closed, key-exfiltration requests refused, `allow` refused `v1/keys`, a 6.5 MB reply streamed |
| Open (operator) | Secretive keys `tengu-attended` / `tengu-unattended`; `tengu keys setup` → `CLIENTS` with explicit `routes`; `npx wrangler login` + `npx wrangler deploy`; rotate each provider key, then `npx wrangler secret put` it + `SESSION_KEY` (`docs/cloudflare-secrets-setup.html`); `[keys] strip` the moved local names; then P0 numbers (CPU p95, Tor reachability of workers.dev) and opt-in per sandbox (`allow_hosts` sandboxes must list the Worker host; lping Solana scopes need it in `net_hosts`); `sealed-check` against the deployed Worker |
| Open (code) | sessions are not refreshed in-process — a long run must restart before `session_hours` (unattended key: up to 168 h) · a session holder can still USE a route's key (e.g. call any Telegram Bot API method as the bot), never READ it |
| Not moved | wallet signing (separate "go"), local Postgres, Claude Code login |

## 2026-10-09: lease fail-stop (review `handoff_review.md` P1 + P2) — branch `fix/lease-loss-fail-stop`

| Finding | Fixed | Test |
|---|---|---|
| P1 `tengu soe cycle` kept writing after losing `runtime:<s>` / `state:<root>` | `ports::runtime::Ownership` (`HeldLeases`: the keeper's and the writers' one view — sticky loss, a renewal granted with another `acquired_at_ms` = lost); the cycle renews before each resumed line, the claim, each phase marker, each stage (raced by the loss), the freeze, each state-log line; lost ⇒ `lease_lost`, nothing more written (no `failed.json`); `under_leases` drops the cycle and fails `lease_lost`, never its result; the `soe_cycle` job writes under `Runtime::ownership` | `application::soe::tests::a_lost_lease_stops_the_cycle_writing` · `application::soe::job::tests::a_lost_lease_fails_the_job_writing_nothing` · `cli::soe::weekly::tests::a_lost_lease_fails_the_command_not_the_cycle_result` · `application::runtime::tests::a_lapsed_lease_retaken_since_is_lost_for_good` |
| P2 ranking published after its last renewal | renewed before the dated ranking, `latest`, the terminal and the FAILED manifest; a lapse re-taken since is a loss | `application::ranking::tests::a_lease_lost_before_publish_publishes_nothing` |

## 2026-10-08/09: strategy ranking, SOE O0–O4, Studio — on `main` @ `f7ac9a5636e2493e3839e896cba9951a2d41aefb`

All squash-merged (#39–#45); this session's branches deleted, their tips kept as `archive/<branch>` tags (older branches: operator decision). Every commit, PR, branch, tag and open gate: `handoff_for_check.md` (repo root). Checked against the code 2026-10-09.

| PR | Squash on `main` | What |
|---|---|---|
| #39 | `10bdbb53a27390f6a67fd73826740befe05ababe` | xlab-w2 `keep_runs = 0`: unbound, it shares `xlab`'s state dir with the W1-cited run dirs and would have pruned them. #40 then set `keep_runs = 200` — `[strategy_ranking]` names the registry, so retention keeps every cited run (`sandboxes/xlab-w2/config.toml` `[backtest]`, `src/config/backtest.rs` module table) |
| #40 | `117b2595b0864066502d156aca42c7180494d67a` | strategy ranking SR-1..SR-8: lineage kind `ranking`, unsealed contracts `rank.xlab-w2.daily.v1` / `rank.xlab-w2.weekend.v1`, ranker, coordinator, `tengu ranking run \| show`, tool `strategy_ranking`, xlab-w2 feeds — § Strategy ranking below |
| #41 | `bc9bc12a038654ac4941c714abcfa777c99af631` | SOE O0–O2: `domain/soe/`, `domain/source/`, `sources.db`, SEC EDGAR + EU TED, `tengu soe` / `tengu sources`, tool `source_evidence`, `sandboxes/soe` — § SOE below |
| #42 | `e84e886c6d3d4dc985e408991fe66d1773140c01` | docs sweep A: README, architecture, context, research, memory / skills docs and 22 tutorial pages match the code |
| #43 | `7cae230f61eddcc014b94521564b15a7b3ac817f` | SOE O3 / O4: weekly cycle, replay + grading, Review #2 packet, tools `soe_view` / `soe_propose` / `soe_challenge`, feed `kind = "job"` (`soe_cycle`), generation SOE-G0 |
| #44 | `f7ac9a5636e2493e3839e896cba9951a2d41aefb` | Tengu Studio + control-loop-lab ST-00..ST-40: workflow graph, trace store, `tengu studio` / `tengu trace`, `web/studio/`, Play / Stop — § Studio below |
| #45 | (this PR) | docs sweep B (agent guides, handoff, hub, code map, runtime / tool docs, 15 tutorial pages, feature docs), ST-90 clean room PASS, `docs/studio-evidence/`, `handoff_for_check.md` |

### Operator decisions open

| Gate | The operator | Blocks | Doc |
|---|---|---|---|
| G-WKND | stops weekend #2 Mon 2026-10-12; snapshot `lineage/evidence/w1-2026-10-12.toml`, grade, regrade | the main-checkout pull (rule below) | `docs/forward-evidence-runbook-2026-10-08.md` |
| G-SR1 | seals both ranking contracts as drafted (`tengu lineage seal ranking:<id>`) or changes them first | every ranking — refused while unsealed (`tengu lineage verify` warns `ranking_unsealed`) | `docs/strategy-ranking-automation-2026-10-08.md` § Operator decisions |
| G-O0 | signs the private SOE profile (`tengu soe init` writes the UNSIGNED template `<TENGU_HOME>/state/soe/operator.toml`: four money values `REQUIRED`, add `[[capabilities]]`, sign); approves each source row's terms + hash and enables it; approves committing sanitized planning docs | a source `enabled = true`; the first live cycle | `docs/soe-2026-10-08.md` § 1, § 15.1 |
| SOE-G0 lock | after G-O0 + G-WKND: appends SOE-G0's `[[frozen]]` row to `lineage/locks.toml` | nothing in code (a CANDIDATE runs) | same, § 15.1 #8 |
| Operator Review #2 (SOE G-R2; not `TENGU_ROADMAP.md`'s trading Review #2 after P13 — not reached) | 4 frozen live weekly cycles, each graded; a replay of ≥ 8 older dated cases with a counted holdout read; `tengu soe review` packet | STOP: nothing from O5–O8 | same, §§ 15.2–15.4 |
| Live engine-matrix legs | sets `xlab_rank` (`strategy_ranking`), `sources` (`source_evidence`), `soe` (`soe_view` · `soe_propose` · `soe_challenge`): only the offline local legs ran; openrouter + claude_code live legs not run, local on the operator's PC; also `tengu run --sandbox xlab-w2` live | each tool's definition of done (CLAUDE.md "How to add a new tool" step 4) | `tests/engine_matrix.rs` module table |
| Operator Review #3 | decides on building the Studio editor (ST-40 = design only) | any edit / save code or route | `docs/studio-editor-design-2026-10-08.md` |

### Weekend no-pull rule (main checkout)

| When | Do |
|---|---|
| Until the weekend #2 stop (Mon 2026-10-12) | the main checkout stays at `10bdbb53a27390f6a67fd73826740befe05ababe`: the frozen `~/.cache/tengu-xm.noindex/weekend/tengu-acdef66` runs from it, and its W1 load refuses origin/main's `lineage/rankings/` (verified 2026-10-08, runbook row "Before the stop"). Work in worktrees only |
| After the stop, before `git pull` | `git checkout -- docs/forward-evidence-runbook-2026-10-08.md` (the local edit = upstream's blob `aa7ccff6f4cd980d64d2f6e6a40a47dde5676c0b`, nothing lost) · `rm TENGU_STUDIO_PLAN.md docs/strategy-ranking-automation-2026-10-08.md` (untracked older drafts; tracked upstream since #44 / #40) · `rm handoff_for_check.md` if present (not on disk, not in git on 2026-10-09) · then `git pull` |
| Stays untracked | the private research docs of cleanup row C1 |

### Studio (#44) — tracker `TENGU_STUDIO_PLAN.md` § 8

| Item | State |
|---|---|
| ST-00…ST-03 | lab `sandboxes/control-loop-lab` + runbook `docs/control-loop-lab-2026-10-08.md` + real-Jev baseline; Gate 1 waived (operator instruction 2026-10-08) |
| ST-10 | `tengu studio graph --sandbox <s> [--map <file>]`: `domain/workflow.rs` + `application/studio/graph.rs` + `bootstrap/studio.rs`; `Config::source_sha256` = `config_hash`; goldens `tests/fixtures/studio/` (`TENGU_REGEN_GOLDEN=1`) |
| ST-11 | `domain/trace.rs` envelope + `ports/trace.rs` + JSONL store `adapters/outbound/trace_store.rs` (`<TENGU_HOME>/logs/trace/<sandbox>/<run_id>.jsonl`); `tengu run` / `tengu decide` record (`run.opened` first); `decisions.jsonl` lines + `runtime_id` / `run_id`; `tengu trace runs\|show`; `decide` / `doctor` / `studio` log to stderr |
| ST-12 | every lab step is an event: `runtime.*` (`bootstrap/runtime.rs` `start_recorded` / `shutdown`), `loop.*` (`LoopDispatch::with_trace`, counters), `feed.*` (`FeedEnv::trace`), `observation.read` / `jev.*` (legal set) / `action.*` (`DecisionLoop::with_trace`, payload = the computed `StepOutcome`), `tool.*` (`application/trace_exec.rs` `TracedExecutor`), `trigger.*` (`tengu decide`; a loop that cannot be built is `trigger.* failed`, not an empty run); parents via `trace_exec::cause`; coverage table `docs/runtime-2026-09-30.md` § Trace + tutorial `studio.html` |
| Gate 2 | waived (operator instruction 2026-10-08) — evidence `docs/studio-trace-evidence-2026-10-08.md` (real-Jev decide act / tool-error / uncertain map + `tengu run` with ticks, SIGINT, restart, SIGINT; `tengu trace show` reconstructs each run) |
| Review ST-10..12 | adversarial pass: payload object keys scrubbed too + the graph's attrs get the same `domain::trace::scrub_value` (an unregistered `${VAR}` URL never leaves); an `http_request` non-2xx text is `tool.failed` (`reduce::http_ok`, the loop's rule); a tool feed run stopped before every call closes with `feed.skipped`; `bound_payload` stays ≤ the bound when the dropped names alone would not fit; a decide's `run.opened` names no node; one dry-run rule `DecisionLoopConfig::logs_only` (loop, validation, graph); `trace show` reads once, `--follow` keeps one Ctrl-C listener. Unchanged: ids, seq order, `decisions.jsonl` (additive keys only), scopes, layering, frozen paths |
| ST-20 | `tengu studio --sandbox <s> [--port] [--bind]` (`--features studio` in #44, with CI step `cargo test --features studio --bin tengu studio`; in the default build since 2026-10-09 — row "Default build"): `adapters/inbound/studio/` (axum, GET only: `/`, `/assets/*`, `/api/v1/{meta, graph[?map=], health, runs, runs/:id/events, runs/:id/stream, live/stream}`) · guard: loopback bind only, per-process token (URL fragment `#t=`; header / `?token=`), Host 421, Origin / Sec-Fetch-Site 403 · SSE per client bounded (256 live; backlog waits) → `event: lagged` + close, resume by `Last-Event-ID` · live stream = the heartbeat holder's run across restarts (`application/studio/stream.rs`) · health = `bootstrap::runtime::read_live` (shared with `doctor --live`) · `jev.*` pin `legal` + `legal_actions` (`EventDraft::keep`: dropped last by the 4096-byte bound) · Rust-only exception for `web/studio/` (CLAUDE.md + AGENTS.md) + `tests/language_policy.rs`; `web/studio/` = a read-only shell (ST-21 draws the graph). Live proof: `docs/studio-trace-evidence-2026-10-08.md` § ST-20 |
| ST-21 / ST-22 | the page `web/studio/` (vanilla JS, no build, no CDN) draws only what Rust computed: tones `domain::trace::Status::tone` (ok green · failed / refused red · pending / running / stale / missing / escalated / dropped amber · skipped plain · grey only from the step's legal set or a map) served in `/api/v1/meta`; the board fold `application/studio/board.rs` (`/api/v1/runs/<id>/board[?upto=]`: node tone = latest event's, lit edge only from `action.*` / `tool.*`, grey legal set, header facts; `/events` adds each event's `view`: tone, facets inherited down the parent chain, edges); the inspector `/api/v1/nodes/<id>` = the validated config section (`application/studio/inspect.rs`, never TOML text) + evidence files; `Node::facets` (graph goldens regenerated, additive); `RunState` live · closed · open on `/runs`; a `--map` decide run drawn on its kept map's graph. Live = SSE doorbell + the same `/events` pages ⇒ live = replay. Place in the URL fragment (token dropped from the address bar after the first read). Live proof: `docs/studio-trace-evidence-2026-10-08.md` § ST-21 / ST-22 (headless Chrome, temp profile) |
| Gate 3 | waived (operator instruction 2026-10-08) — not reviewed by the operator |
| ST-30 / ST-31 | Play / Attach / graceful Stop through the `tengu run` start: `inbound/run.rs` split into `start_session` → `RunSession` (`stopper`, `loops`, `wait_and_shutdown`; `tengu run` unchanged); `[studio] control` (`config/studio.rs`: default false, `control-loop-lab` true, `tengu studio --allow-control`, never for `[generation]`-bound or hardened — a load error there); rules `application/studio/control.rs`; `adapters/inbound/studio/control.rs` `Controller` (Play on its own task, a held lease = typed `bootstrap::runtime::LeaseHeld` ⇒ 409 + attached read-only, Stop = `Stopper::stop("studio stop")`, refused for a runtime Studio did not start, event = a named scenario into the owned `LoopDispatch`, Studio SIGINT drains first); routes `GET /api/v1/control`, `POST /api/v1/control/{play,stop,event}`; guard: a POST needs the token header + own `Origin` + `Sec-Fetch-Site: same-origin` (403), Host 421, JSON ≤ 1 MiB (413 / 415 / 400), no CORS; Studio's own recording `kind = "studio"` (`studio.started` · `studio.control` request → ok / refused / failed · `studio.runtime` · `studio.stopped`); page: Play / Stop / scenario select drawn only from `/api/v1/control`. Live proof (real Jev): `docs/studio-acceptance-2026-10-08.md` |
| Gate 4 | waived (operator instruction 2026-10-08) — not reviewed by the operator |
| ST-40 + docs | editor = design only `docs/studio-editor-design-2026-10-08.md` (palette from Rust, connections = the graph's edges → one TOML key each, `Config::load` validation, Save-as-new + TOML diff, W1 / hardened view-only, maps via `ExecutionMap::apply`; no code, no route); operator doc `docs/studio-2026-10-08.md` (quick start, routes, security, Play / Stop, troubleshooting); CLAUDE.md + AGENTS.md "Beyond chat" row + gotcha; code map `[studio]` row + recipe |
| Review ST-20..ST-40 | adversarial pass (`7a69f79e51c46b54cfdaa0123da5b18c00610d7e`): the CSRF proofs guard every request but GET / HEAD on any path (was: `/api/` only — the guard leaned on the router not normalising paths); `scenario` / `loop` names 1–128 bytes (400; a 1 MiB name was kept in the served view and trace); the control verdict carries its `tone`, `studio.js` compares no status / tone (test); the Studio server logs to `tengu.log` + stderr like `tengu run`; acceptance doc no longer cites the folded WIP commit as its base. Live (`docs/studio-acceptance-2026-10-08.md` § Review): guards incl. traversal + bogus `Last-Event-ID`, a live drain of an in-flight event (`finished 1`), secrets sweep 0 matches, `kill -9` → no orphan, lease taken over after its TTL |
| Screenshots | `docs/studio-evidence/01-idle.png` (no runtime, health not live) · `02-running.png` (after Play: live run, a scenario event queued) · `03-replay.png` (the stopped run replayed at seq 60 of 117) — `control-loop-lab`, 2026-10-09 08:01–08:05 UTC |
| Next | ST-90 clean room PASSED 2026-10-09 (fresh clone of `f7ac9a5636e2493e3839e896cba9951a2d41aefb`: release build, lab A0–A15 + G1–G8 + Studio flow, `cargo test --workspace` 2130 / `--features studio` 2176 passed; 8 doc fixes applied — `docs/studio-clean-room-2026-10-09.md`); next: Operator Review #3 decides on building the editor |
| Integration with #39–#43 (Fri 2026-10-09, before the squash) | #39–#43 (xlab-w2 `keep_runs`, strategy ranking, SOE O0–O4, docs sweep) merged into `feature/studio`: a `kind = "job"` feed (`soe_cycle`) starts through the same `start_session` under `tengu run` and Studio's Play; its runs trace like a tool run (`feed.fired` `kind = "job"` → `feed.completed` · `retrying` · `failed`, correlation `feed:<name>:<slot ms>`); the workflow graph draws a job feed owned by the `[soe] architect` (whose store holds its `feed/1` row); an `[soe]` refusal carries `LeaseHeld` like the others; `sources` / `ranking` log to stderr via `stdout_is_data` |
| Default build (2026-10-09, branch `feature/studio-default-trace-orch`) | `studio` joins Cargo `default` (`openrouter`, `telegram`, `studio`): `cargo build` and the Docker image carry the server (the image build now copies `web/`, the page `include_str!` needs it); CI: `cargo test --workspace` runs the Studio tests, the old opt-in Studio step became the no-studio check `cargo test --no-default-features --features openrouter,telegram --bin tengu studio` (the stub that names the flag) |
| Webhooks + orchestration traced (2026-10-09, same branch) | `tengu webhooks` records (`RunKind::Webhooks`, `run.closed`); every request to a configured endpoint = `trigger.webhook` root → its loop / plan events → `webhook.responded` (code; never body or headers) under `tengu webhooks`, `tengu run`, Play; `OrchestratorEvent` → `plan.*` / `step.*` / `metrics.recorded` (`application/orchestrator/trace.rs`, written synchronously by `EventBus::send`) for chat / telegram / eval (with `[orchestrator]`), webhook agent endpoints, escalations; a step's `run-agent` child records its `tool.*` (`AgentIpcInput.trace` → `AgentIpcOutput.trace` → parent `replay_child`); graph: `planner` —delegates→ routable agents —calls→ their tools (lping golden 62 → 100 nodes, 88 → 129 edges); live check: `docs/runtime-2026-09-30.md` § Trace (fixture `tests/fixtures/webhooks/trace.toml`, no model reached). Review 2026-10-09: CI gains `cargo test --features webhooks --bin tengu webhooks` (no CI step compiled the webhook tests); a late subagent `metrics.recorded` keeps its step after `plan.completed`; `tengu telegram` opens its run only once the session is built (a refused start left an open run); an eval row writes `run.closed` after its drain window; test `executor::tests::traced_bus_runs_each_step_under_its_step_started`; re-run live: no secret, body or header in 5 runs. Open: a `claude_code` step's bridged tool calls are not recorded; a `run-agent` child that ends without its IPC output (engine error mid-loop, timeout kill) loses its tool events; a step aborted with its turn has no outcome event |
| Gotchas | lab runs export `TENGU_HOME="$HOME/tengu-lab/home"` first (parent `.env` = `~/.tengu`); `--sandbox` is cwd-relative (run from the repo / worktree root); a tick's feed and loop events share the session `tick:<slot ms>` = one correlation; the Studio server is in the default build since 2026-10-09 (feature `studio`; a `--no-default-features` build without it refuses, naming the flag; `tengu studio graph` works in every build); Studio control lives in the Studio process — a killed Studio takes its runtime along, the lease frees after 30 s |

### Strategy ranking (#40) — built, unsealed (Thu 2026-10-08)

| Item | State |
|---|---|
| Doc | `docs/strategy-ranking-automation-2026-10-08.md` (audit A1–A12 as built, deviations, SR-0–SR-8 states, operator decisions) · tutorial `docs/tutorial/strategy-ranking.html` |
| Built | record kind `ranking` (`lineage/rankings/`, `tengu lineage seal ranking:<id>`, Warn `ranking_unsealed`) · cohort identity in `report.json` (`generation`, `instruments_sha256`, `costs_sha256`) · `[strategy_ranking]` + retention of cited runs in unbound sandboxes · pure ranker `domain/backtest/ranking.rs` · coordinator `application/ranking/` (seal check, DST-correct date + cutoff, lease `ranking:<id>`, manifest resume, freshness, backtests, publish dated + `latest`) · `tengu ranking run \| show` · opt-in tool `strategy_ranking` (every engine; capability `intel.strategy_ranking` CANDIDATE) · xlab-w2: contracts listed, private `xl_ranker`, feeds `history_refresh` / `strategy_ranking_daily` / `strategy_ranking_weekend`, `keep_runs = 200` |
| Commits | `caacdc0be446ce9d7628beacbc31ef0992855c1f`, `42584f85ca739b7ee6a0df408726e05488d5deec`, `5a4682e8f0c7fce262f94895e2fe9b134f71e4ae`, `7c7a7cdc5df01f1b117abbc8b0c3a30dcff5b78d`, `6573e24f44da1eb35ff2f7930348cbd884fa1239`, `6a99fe212062036833b8a4bb28e3a6457573262d` + the SR-8 docs — pre-squash branch commits (PR #40's history; on no branch since the merge), squash `117b2595b0864066502d156aca42c7180494d67a` |
| Tests | domain ranker + contract + validate unit tests; `application::ranking::tests` (lease, resume, publish order, stale, DST, `a_forward_grade_changes_only_later_dates`); `tests/strategy_ranking.rs` on the binary; conformance case `strategy_ranking_hl`; engine-matrix set `xlab_rank` (offline local leg) |
| G-WKND (merge) | merged to `main` anyway (#40); the frozen weekend binary's W1 load refuses the unknown `lineage/rankings/` dir, so the main checkout is not pulled until weekend #2 stops (§ Weekend no-pull rule) |
| G-SR1 (operator) | both contracts committed **unsealed**: seal as drafted or change first (strategy set, rating order, cohort fields — `instruments_sha256` + `costs_sha256` split xlab-w2 into several cohorts — cutoffs, the weekend contract's `days`). Until then every ranking is refused; nothing ran on `~/.tengu/state/xlab` |
| Open | INCOMPLETE is terminal (rerun = delete the date dir) · a run cut by shutdown keeps its lease ≤ 15 min · the W1 `xlab` sandbox shares the state dir and can prune xlab-w2 runs no `latest.json` cites · live matrix legs `*_xlab_rank` and `tengu run --sandbox xlab-w2` not run |

### SOE O0–O4 (#41, #43) — built and tested offline, waits on G-O0

| Item | State |
|---|---|
| Docs | `docs/soe-2026-10-08.md` (contract, sources, threat model, cycle; operator path § 15) · `docs/source-evidence-2026-10-08.md` (O2) · tutorials `soe.html`, `source-evidence.html` · `docs/lineage-2026-10-06.md` § 7 (SOE-G0) · `docs/runtime-2026-09-30.md` § SOE sandbox |
| O0 / O1 | 16 synthetic eval cases (`tests/fixtures/soe/cases/`) · `domain/soe/` (values, records, economics, PRD § 7.2 hard gates, capability fit, ranking, `HOLD` week) · profile loader `config/soe.rs` · `tengu soe init \| check \| portfolio \| sensitivity \| eval` |
| O2 | `domain/source/` (records, two-clock as-of view, packet `source_asof/1`) · `[sources]` (`config/sources.rs`) · append-only `<TENGU_HOME>/state/<state>/sources.db` (`adapters/outbound/sources/store.rs`) · SEC EDGAR + EU TED (`sources/{sec,ted}.rs`) · `tengu sources list \| fetch \| import \| asof \| purge \| terms \| disable \| enable` · read-only tool `source_evidence` |
| O3 / O4 | `application/soe/` (cycle, freeze, submit, replay, grade, review, `soe_cycle` job) · `[soe]` closed-world load rules · `FsCycleStore` (`adapters/outbound/soe/store.rs`) · feed `kind = "job"` · stage tools `soe_view` / `soe_propose` / `soe_challenge` (`tools/soe/`) · skills `soe-architect`, `soe-critic` · `tengu soe cycle \| replay \| grade \| resolve \| review \| verify \| show` |
| Sandbox | `sandboxes/soe`: bound to SOE-G0 (CANDIDATE, hash-only public record, no `[[frozen]]` row yet) · closed world, no write / contact / spend / publish tool · every `[sources.registry.*]` row `enabled = false` · feed `[feeds.soe_week]` (`kind = "job"`) · state `~/.tengu/state/soe/`, workspace `~/soe-ws` (neither exists yet) |
| Capabilities | `intel.source_evidence`, `intel.soe_view`, `intel.soe_propose`, `intel.soe_challenge` (+ `intel.strategy_ranking`) — all `CANDIDATE` |
| Tests | bridge conformance `source_evidence:*`, `soe_*`; engine-matrix sets `sources`, `soe` (offline local legs `offline_local_sources`, `offline_local_soe`) |
| Not yet (G-O0) | a signed profile, an enabled source, a cycle against a live model, the live engine-matrix legs — `docs/soe-2026-10-08.md` § 15 |
| Private | the planning set `docs/software-opportunity-{prd,roadmap,compatibility}-2026-10-04.md` stays local (private figures; cleanup row C1) |

## Operator Review #1 = APPROVE (Thu 2026-10-08)

| Item | Answer |
|---|---|
| Verdict | `APPROVE` — package accepted, § 16 Phase 6–10 as proposed (`docs/w1-review-2026-10-06.md` § Verdict) |
| U8 | PRs #25 + #26 merged |
| U9 | `sandboxes/xmarket` desk stays unbound |
| U10 | forward run `VALID_WITH_LIMITATIONS` and Saturday prereg `UNRESOLVED` accepted |
| Next | Phase 6 decision evaluation: first D1 (`data_asof` in reports), D2 (count CLI holdout reads), D3 (never prune a registered run), D11 (late funding from HL history) — `TENGU_ROADMAP.md` § Phase 6, gate G6 |
| P6 fixes (2026-10-08) | D2 `tengu backtest --split` counts its holdout reads (`via = "cli"`) · D3 retention never prunes a run the registry cites (`GenerationScope::cited_runs`) · D1 every report records `data_through_ms`, `--data-through` reruns on the same data (the warehouse-growth problem; named apart from the per-candidate `data_asof_ms`) · W1 pins re-verified: 37 OK, 0 errors |
| D11 → Phase 9 (operator 2026-10-08) | late funding from HL `fundingHistory` needs a new opt-in tool (`hl_ctx` schema is W1-pinned, exec tools have no network); ≈ $0.015 a weekend — built with the P9 cost model |
| P6 core (done 2026-10-08) | `tengu evidence evaluate <run dir>` (`domain/backtest/evaluation.rs`): rules · Jev · HOLD on the same candidates, per candidate + per trade, calibration, latency, cost, verdict. W1 gate run: per trade +28.93 [−23.5, +81.6] (reproduced), per candidate −35.79 [−61.9, −6.6], jev − hold +14.66 [+3.0, +27.5] ⇒ Jev UNPROVEN; G6 PASS — `docs/p6-decision-evaluation-2026-10-08.md` |
| P8 oracle / microstructure (2026-10-08) | closed-session oracle = EMA to impact prices, measured τ ≈ 27 min (docs 30); the Sun 20:00 ET snap undoes ~nothing (8.5 vs 60.4 bps); rule W holds after the 2026-04-30 τ change (+44.60, CI [+7.4, +79.7]) ⇒ no STOP; G8 PASS — `docs/p8-hip3-oracle-2026-10-08.md` |
| P7 news labels (2026-10-08) | `tengu history events` (SEC EDGAR, 844 filings, 63 / 75 names; publication = the index page's acceptance time — the JSON clock is off by the NY offset for some filers) + `weekend_window` `labels` knob; NEWS 62 · UNCERTAIN 273 · NOISE 1165; skip NEWS +45.60, NOISE only +50.59 vs +45.84 ⇒ INCONCLUSIVE, no W2 change — `docs/p7-news-labels-2026-10-08.md` |
| P9 cost / liquidity (2026-10-08) | cost model v2 in `xlab-w2` (observed fees: 9.0 for BMNR / MSTR / PURRDAT; measured $100 spread, mean 4.13 bps / side); knobs `stop_loss_bps`, `rank_by = "net_of_cost"`; top 4 close stop 300 +163.14 vs +142.65 (CIs overlap) ⇒ INCONCLUSIVE, the stop is W2's one capital candidate — `docs/p9-cost-liquidity-2026-10-08.md` |
| P10 / P11 (2026-10-08) | no W2 candidate: nothing beats rule W; the close stop 300 (development +163.14 vs +142.65) failed the forward weekend (−40.57 vs +131.04) ⇒ G11 FAIL, keep W1, no P12 run — `docs/p10-w2-candidate-2026-10-08.md` |
| Weekend #2 (prereg 2026-10-08) | `fwd.rule_w.2026-10-09` sealed 2026-10-08T12:56:28Z (before the 2026-10-10T00:00:00Z anchor); frozen binary `~/.cache/tengu-xm.noindex/weekend/tengu-acdef66` (sha256 `fdcf2c270dd94347b2786d2b1a0b6dda162d91c1a4f02dc37408ce497555b445`); operator start ≤ Fri 19:30 ET — `docs/forward-evidence-runbook-2026-10-08.md` |
| Next | forward evidence: the W1 desk + recorder every weekend (operator: `tengu run --sandbox xmarket-weekend` Fri ≤ 19:30 ET; M3 go needs 12 weekends, 1 so far), counterfactual grading of stops / labels / net-of-cost each Monday; D11 open |

## Resume here (Tue 2026-10-06) — `TENGU_ROADMAP.md` P0–P5 done, STOP at Operator Review #1

Branch `feature/w1-lineage` (from `main` `2aa79717f5cfb1d8a8211672c50ecaed18b568a7`). Review package: `docs/w1-review-2026-10-06.md`. **No Phase 6+ until the operator answers APPROVE / APPROVE_WITH_FIXES / REWORK / ABORT_DIRECTION.**

| Phase | Landed | Doc |
|---|---|---|
| P0 | vault `~/.tengu/state/evidence/w1-2026-10-06/` (486 files, read-only, record `lineage/evidence/w1-2026-10-06.toml`); `tengu evidence snapshot / verify / coverage / grade / regrade`; forward run `VALID_WITH_LIMITATIONS` (capped +1.370036060021 USD, +141.82 bps, n 4; shadow +6.213796012 USD, +8.43 bps, n 74; all reconciliation checks PASS); Saturday prereg graded from the books (+54.66 bps as registered, INCONCLUSIVE, validity UNRESOLVED) | `docs/w1-p0-weekend-2026-10-06.md` |
| P1 | inventory, gap analysis, 11 known defects; 1,514 unit tests + offline suites green; 8 of 10 recorded xlab runs + the Jev gate replay byte-identical; W1 manifest `lineage/generations/W1.toml` (37 pins) frozen in `lineage/locks.toml`; tags `w1-forward-2026-10-02` (`6fcb455bae5553e2d51390e4cedd334779ca0d9c`) + `w1-forward-config-2026-10-02` (local, not pushed) | `docs/w1-inventory-2026-10-06.md` |
| P2–P4 | `lineage/` registry (14 families, 41 variants, 30 experiments, 3 episodes, 8 incidents) + `tengu lineage verify / show / trace / family / attempts / report / capabilities / generation / seal` | `docs/lineage-2026-10-06.md` |
| P5 | 17 capabilities; `[generation] id = "W1"` in `sandboxes/xlab` + `xmarket-weekend` — refused at load: a non-W1 opt-in tool, an unlisted sandbox, a drifted pin; `backtest` refuses a kind outside the generation | same, § 4 |
| Review fixes | branch `feature/w1-review-fixes`: the W1 lock now covers its capability records (row recomputed: `a64c505f5ac99e3ed25f63cca31513de273771c9a5467287eda06007f2e74a43`); `sandbox_unbound`; live `-wal` refused (readers + snapshot); forward seal = window start; `holdout_seen_before` / `window_unknown`; grade checks `funding_qty` / `funding_hours` (vault: 12 / 12 PASS both accounts); regrade look-ahead flag + `mkt_ctx/1` universe; Docker mounts `SANDBOX=<s>` at its own path; minors #11 #14 #16 #18 #19 #21–#23; #12 #13 #15 #17 #20 #24 = known limits | `docs/lineage-2026-10-06.md` § 6 |

| Open | Note |
|---|---|
| PRs #25 / #26 | merged 2026-10-08 (`aa73e42e2628987f6fe6e5b5b7da3482fec37ab2`, `746efca44e115f8888862599b0d5c366ccea9df1`); branches kept — lineage records cite their commits (`e5daae83febea00e549ce9b8cac83bddce12a0a2` also tagged `w1-forward-config-2026-10-02`) |
| Docker | a bound sandbox loads `lineage/` (`COPY lineage` added); `make up SANDBOX=<s>` mounts at `/opt/tengu/sandboxes/<s>/config.toml` (`TENGU_CONTAINER_CONFIG`) so `../../lineage` resolves |
| Known defects D1–D11 | `docs/w1-inventory-2026-10-06.md` § 5 — proposed for P6 |
## Visual tutorial `docs/tutorial/` (2026-10-07, on `main` @ `9f0e98f96704eca960575d6c453c5dabb5487d65`)

| What | State |
|---|---|
| Site | `docs/tutorial/index.html` + 32 feature pages (2026-10-09; 25 at launch), one animated page per feature, written from the code; static, no build (`AUTHORING.md`: page anatomy, components, deploy = copy the folder) |
| Sync rule | every code change updates the pages whose `sources` cover it (`docs/tutorial/sources.toml`) — CLAUDE.md / AGENTS.md "REQUIRED updates"; `cargo test --test tutorial_map`; Claude Code `PostToolUse` hook in `.claude/settings.json` (`.gitignore` now `/.claude/*` + `!/.claude/settings.json`) |
| Not covered | lineage / evidence (only on `feature/w1-lineage`): when it merges, `tutorial_map` fails on the unmapped files → add pages `lineage`, `evidence` — done with #32; 2026-10-09: 32 feature pages (+ `trace`) (`docs/tutorial/sources.toml`), incl. `strategy-ranking`, `studio`, `soe`, `source-evidence` |
| Found on the way | `docs/code-findings-2026-10-07.md`: 32 bugs + 18 stale docs fixed 2026-10-07 (PR #31, each with a test; pages re-checked), 3 rows not a bug, 17 gaps open, 1 operator decision open (reduce-only exit with a stale book: the gate waives `book_age`, the paper fill still rejects `stale_book`) — #38 closed 4 gaps 2026-10-08: 13 open, the decision still open |

## Local data to clean up later (operator, 2026-10-07)

Data that lives ONLY on the operator's Mac (not in git, the repo is public). Kept on purpose for now. **An agent deletes a group only after the operator says that group is done** — never on its own initiative, never as "cleanup while here". Back up first if the operator asks. When a row is deleted, remove it here in the same commit.

| # | Path | Size (2026-10-09) | What | Delete when | How |
|---|---|---|---|---|---|
| A1 | `~/.tengu/state/evidence/w1-2026-10-06/` | 2.2 GB | W1 weekend evidence vault (read-only); its tree hash is pinned by `lineage/evidence/w1-2026-10-06.toml` (on `main` since #32). Weekend #2's vault `w1-2026-10-12/` joins it after the Monday snapshot | W1 retired (Review #1 APPROVE + P10 kept W1: not yet) | `chmod -R u+w` then `rm -rf`; `tengu evidence verify` then fails for that record — expected |
| A2 | `~/.tengu/state/xmarket-weekend/` | 1.2 GB | weekend paper runs: `ledger.db`, `history/` day files, `run-logs/`, `runtime.db` (the vaults' source); weekend #2 writes here 2026-10-09 → 10-12 | with A1, never before weekend #2 is vaulted + graded | `rm -rf` |
| A3 | `~/.cache/tengu-xm.noindex/weekend/` | 96 MB | frozen weekend binaries `tengu-6fcb455`, `tengu-desk-2026-10-02` (commit = tag `w1-forward-2026-10-02`, on origin), `tengu-acdef66` (weekend #2, `acdef66fc45252cba181a351cbc237ed6886ad57`) | with A1; `tengu-acdef66` not before weekend #2 is graded | `rm -rf` |
| B1 | `~/.tengu/state/xlab/` | 535 MB | xlab warehouse `market.db` (+ `-wal` / `-shm`), `backtests/` run dirs, `holdout-reads.jsonl`; shared with `xlab-w2` (its `strategy-rankings/` appear once G-SR1 seals a contract — none yet) | xlab research finished | `rm -rf` (backfill can rebuild `market.db`; run dirs and holdout reads cannot) |
| B2 | `~/.tengu/state/xmarket/` | 646 MB | xmarket paper ledger + recorder history | xmarket work finished | `rm -rf` |
| B3 | `~/.tengu/state/research/` | 8.3 GB | `market-scan-2026-10-05/`, `qnt/`, `sliding/` research data | research finished | `rm -rf` |
| B4 | `~/.cache/tengu-xm.noindex/tengu-xlab-f554db0` | 107 MB | xlab binary (one file) | with B1 | `rm` |
| C1 | main checkout `docs/crypto-opportunity-research-2026-10-05.md`, `docs/crypto-opportunity-deep-dive-2026-10-06.{md,html}`, `docs/software-opportunity-{prd,roadmap,compatibility}-2026-10-04.md` | ~160 KB | private, untracked (never committed — public repo): the two crypto research docs + the three SOE planning docs (private figures; committing sanitized versions is a G-O0 item). Not C1: the untracked `TENGU_STUDIO_PLAN.md` + `docs/strategy-ranking-automation-2026-10-08.md` — tracked upstream, deleted before the post-weekend pull (§ Weekend no-pull rule) | operator decides: move somewhere private, or drop | `rm` the six files |
| D1 | `~/.tengu/logs/` | 75 MB | `decisions.jsonl`, `egress.jsonl`, `risk.jsonl`, `tengu.log`, `maps/` (execution maps by sha256); `trace/<sandbox>/` appears once a post-#44 binary runs `tengu run` / `decide` on `~/.tengu` | no longer needed for audit | truncate or `rm` the files (the dir is recreated) |
| D2 | `~/lping-workspace/.tengu/`, `~/.tengu/state/solana-writes.db` | 128 KB, 28 KB | lping observation cache, Solana write lease store | lping work finished | `rm -rf` / `rm` |
| E4 | `~/.cache/tengu-xm.noindex/seed/` | 6.2 GB | clone source for agent target dirs (`agents/<label>`, deleted after each agent), refreshed 2026-10-09 | any time (refresh before the next parallel build) | `rm -rf` |
| E5 | `~/.cache/tengu-xm.noindex/agents/{sealed-tengu,seal-worker,sealed-crate}/`, `~/development/tengu-sealed/` (worktree) | ~13 GB | sealed-keys build caches and the `feature/sealed-keys` worktree (the throwaway test ssh key was deleted after the local runs) | after the sealed-keys PR merges | `rm -rf` the dirs; `git worktree remove ~/development/tengu-sealed` |
| F1 | `~/tengu-lab/` | 464 KB | control-loop-lab + Studio: workspace `control-loop-lab/{in,out}` (the lab agent's only fs roots, empty); the lab `TENGU_HOME` `home/` (312 KB: `logs/trace/<sandbox>/<run_id>.jsonl`, `logs/decisions.jsonl`, `logs/maps/`, `state/`) and the Studio validation home `home-validation-2026-10-09/` (152 KB: `logs/` + `state/`; its trace `cbd8cd05-30fb-4177-aa99-0b978f0e83e5` backs `docs/studio-evidence/`) | Studio / lab work finished (after Operator Review #3) | `rm -rf ~/tengu-lab` (only ever lab data — `docs/control-loop-lab-2026-10-08.md` reset steps) |
| F2 | `~/.tengu/state/soe/` (+ workspace `~/soe-ws`) | none yet | SOE state root: signed `operator.toml`, `sources.db`, `cycles/`, `replays/`, `reviews/`, `eval/` — private (never in git); created by `tengu soe init` / the first `tengu sources fetch` | never without the operator: it holds the signed profile and the graded cycles of Operator Review #2 | back up first; `rm -rf` |

## Third-party science-platform integrations removed (2026-10-07, operator)

| What | Note |
|---|---|
| Removed | the science-pipeline sandbox, its three skills (pipeline orchestrator, science-social posting, paid lab mutations), their env vars in `.env.example`, every code / test / doc mention |
| Consequence | `[skill_lifecycle]` and the `learning-agent` / `skill-improver` agents have no shipped sandbox (commented sample in `config.example.toml`); `skills/orchestrator/SKILL.md` "Lifecycle verbs" still route to `learning-agent` — define it in a sandbox before using them. Examples now use `lping` (orchestrated, webhooks), `xlab` (claude_code), `storage-test` (Telegram) |
| Not removed | git history (public repo; a rewrite cannot reach PR refs or forks) — the leaked keys of those integrations are revoked |

---

## lping execution chains (Tue 2026-10-06) — PR #27 (`lp_swap_plan` + step A), then `feature/execution-map` (step B)

| Change | Note |
|---|---|
| Step A: bound slots | `{event = "/x"}`, `{from, path}`, `{observation, path}` bind ONE value (no question to Jev; unresolved ⇒ action illegal); `{event = "/list/*", value}` lists from the event — `config/decision_loop.rs` (`SlotMode`), `application/decision_loop/slots.rs` |
| Step A: dry-run goes on | a dry-run write no longer ends the event (`run_event`); xlab `xl_gate` (terminal-only) and jev-exec (all `read_only`) unaffected; lping `lp_watch` takes one more step (hold) |
| Step A: `hedge_decide` `data.order` | `PerpsOrder` = the action as `jup_perps_order` args (close on an entire-position decrease; decrease / close cap ≥ the side's current notional) |
| Step A: lping `hedge_exec` / `lp_exec` | simulate chains, every arg bound; verified live with Jev (keyless): hedge order simulated (1 tx, 93 733 CU, forced target in a scratch config — the real wallet has no LP, so BUG-011 grace holds), LP swap (1 tx) and open (1 tx, 210 003 CU) simulated |
| Step B: `sequence` + execution maps (`feature/execution-map`) | loops offer one step at a time (`sequence`, `?` optional, failure halts); `tengu decide --map` runs an Architect's JSON map that only narrows a loop (`config/execution_map.rs`, skill `execution-map`, audit `trigger = "map:<sha256>"`); `lp_swap_plan` `data.deposit` (feasible only) feeds `dlmm_open_position`; lping exec loops sequenced. Verified live (Jev, keyless simulations) |
| Next (step C) | money rails before any `send`: Solana `call_id` idempotency, cross-venue `[risk]` on Solana writes, approval gate; a private map tool for hardened sandboxes (no shell there); dedicated wallet |
| `lp_swap_plan` | pure port of the bot's `planSwapForDeposit`: permanent + rent SOL reserve, refundable position rent, hedge-collateral USDC reserve; one `jupiter_swap` route (token units) or a typed block (`invalid_input`, `insufficient_total_value` / `_sol` / `_usdc`) |
| lping | 11 observe/plan tools + 5 `simulate \| send` Solana writes on private `lp_executor`; `lp_watch` / `hedge_watch` stay `dry_run`, `hedge_exec` / `lp_exec` simulate; Raydium LP + Hyperliquid live writes remain explicit gaps |

---

## Merged to `main` (Fri 2026-10-02)

| PR | Squash commit on `main` | Original branch (kept on origin) |
|---|---|---|
| #20 xmarket W1 + gate (68 commits) | `131af134b57191850c249008a50b2ec05a59ed53` | `feature/xmarket` |
| #21 xlab (23 commits, rebased onto #20) | `b7dc915149cc025cc9466748905ab18ca46e075a` | `feature/xlab` |

The `feature/xmarket` / "not pushed" mentions below are history. Older local-only work is on origin as `backup/*` branches.

**xlab models (operator 2026-10-02: no expensive OpenRouter models):** `xl_architect` = `claude_code` `claude-opus-5-5`, `xl_jev` = `claude_code` `claude-sonnet-5-5` (never called), both `builtin_tools_profile = "none"`; Jev is the only OpenRouter model. Needs a build with `--features claude_code` and `claude` logged in. Checked live: `tengu doctor --sandbox xlab --engines` both ok; an Opus `tengu tool turn` called `backtest` (run `20261002T183833Z-weekend_fade`, n 614, in-sample +57.67 bps).

**xmarket the same way (2026-10-02, tracker § 7 # 21):** no planner (`[orchestrator]` and `[agents.xm]` removed; `xm` only ever routed to `xm_architect`), `xm_architect` = default chat agent on `claude-opus-5-5`, `xm_executor` = `claude-sonnet-5-5` (never called), both `"none"`. No OpenRouter model in xmarket until the Jev loops arrive. M5 `jev-architect-escalator` needs an escalation target again.

---

## Docs refresh (Fri 2026-10-02) — state below is unchanged

| Topic | State |
|---|---|
| Start | `docs/index.html` (docs hub, open in a browser) → every explorer + markdown doc, one search |
| New pages | `docs/xmarket-2026-10-02.html` (paper desk, W1 items, risk + paper, runtime, rule W, parity + hardening, W1 gate) · `docs/xlab-2026-10-02.html` (history first, data, specs, engine rules, results, Architect, review) · `docs/changes-2026-09-29-to-10-02.html` (the 90 commits of `main..HEAD`: phases, charts, files, tracker items) |
| Refreshed | README, `docs/configuration.md` (schema reference), `docs/skills.md`, `docs/tools.md`, `config.example.toml` (comments), `docs/architecture-2026-04-27.{md,svg,html}` (+ `tengu run` / `tengu backtest` flows, Stores tab), `docs/architecture.md`, `docs/harness-architecture.md`, comparison, IMPLEMENTATION_PLAN, tracker § 0, build plan, validation + manual-test checklists, CLAUDE.md / AGENTS.md |
| Corrected | doctrine #4 (planner reply parsed tolerantly, no planner retry; a step runs `max_attempts_per_step` times, then `max_replans`) · session_id gotcha (chat / Telegram once per process, eval once per row, webhooks per request) · metrics: only successful planner calls, `run-agent` step turns, embeddings, wiki compiler and Jev emit a `MetricsRecord` — in-process chat turns emit none · the vault prompt reads `/dev/tty`: Enter skips it, `</dev/null` does not · `doctor --live` is the healthcheck for a `tengu run` container (the shipped image: `tengu telegram` + plain `doctor`) · vector store `<workspace>/memory/vectors.bin` · `openrouter` feature = empty marker · `xm_weekend_fade` in the exec-tool lists · xlab: placebo −58.0 (was −58.1), 26 of 75 xyz names reach 2026-03-07 (the rest from listing) · short commit hashes → full |
| Checked | pages in headless Chrome: no console errors, every tab renders, no overflow at 390 px; every 40-hex hash in the docs is a commit; `code_map` (html regenerated), `layering_lint`, `scope_lint`, `run_agent_ipc`, config load over every sandbox + the example, `cargo fmt --check`; 1,539 unit tests in the default-feature bin |

---

## Resume here (Thu 2026-10-01 evening — xlab: history first)

Operator 2026-10-01: "why do I have to wait 2 days" — answer from history, never block on live recording (CLAUDE.md gotcha "History first"). Built the same day on `feature/xmarket` (merged to `main` as #21 on 2026-10-02): sandbox `xlab` for the PRD v0.5 harness, doc `docs/xlab-2026-10-01.md` (§ 14 = first results).

| Topic | State |
|---|---|
| Commits | `b19c91539e6862c0cbe832c7306774db4c6b2252` prep · `18221e8049073c66d59f1f58a0a2fb3fce490999` Jev on a clock + decision cache · `d1154b1031b453694f67f2cc45eda722bf441866` sandbox + skill · `e7cd0633bb30cc34886026fb41759248fd14e83d` market.db + backfill · `9645fc0f6482ff962ddb9c45a5aa0b1ed76a0e12` engine · `6b7a0e0b4262f5a4616e1b3bd6dc751a47592e01` `market_history` · `90242ef64a0a8063b0c135921dc2ec396be8f5a3` gate arm · `d46639b2de669ff285bc6d2471cb52fddda41608` `tengu backtest` · `b8bb07559f5cdbe8937375beec193df549e627c9` CLAUDE / AGENTS / tracker · `02f0c6117f552542ea8b8b4dfa1490e68488528a` `backtest` tool · `2750f788746c2de8049530b55a3148296cdca115` gate wiring + splits + entry liquidity + capped ranking · `f7baf25d57a14c74099307e43f44aef007afb9c7` Architect tools · · `e6b0075c9239fc571a1d6c8c748511b9947bfdc5` split CIs + skill call shapes · `9e24ffc2f2b2df126d2d26340a1311cc2f6a08c9` docs · `db96cb25c0ffa1efa13f8d7c388e407dfb107d24` review fixes (engine) · `dba04d59922ba1922304a41351b1b4431efe9e75` review fixes (tools) · `998641179a4c2c25e33ea81832e3994a1ba8601f` text bound · `1c600de5117cf153f8a1e0642cd8297afe8b8929` matrix fixture regenerated · `0f93d3295f4c5b7e077d6d3a0d903eb4350d926f` + `f554db01379f5bfc82cdf020934710084118c0c0` holdout read prints out-of-sample figures only · `4b3270af4731adac26e6c51dde092f5cafebb577` |
| Data | `~/.tengu/state/xlab/market.db`: 79 instruments (75 xyz + BTC / ETH / SOL / HYPE), 1h bars from 2026-03-07, funding from 2026-03-01, Sept HL-archive ctx (crypto); re-run `tengu history backfill --sandbox xlab --instruments @crypto,@xyz_stocks --interval 1h --from 2026-03-01 --funding` to extend (resumes) |
| Results | rule W +50.4 bps (n 1,500, CI +15.7 … +81.5; t 2.9 clustered by weekend); liquid-entry variant holdout +72.2 (CI +6.3 … +130.9); placebo −58.0; every other library strategy no-go; Jev gate on W: +28.9 vs rules, CI spans 0, p(take) uninformative (Brier 0.35) — `docs/xlab-2026-10-01.md` § 14 (figures after the engine review fixes; the earlier ones beside them) |
| Engine review fixes (`xlab-fix-engine`) | P&L on simple returns (a linear perp; ln overstated shorts); a split inside a bar drops that bar (KIOXIA 1d); capped admissions never read whether a later bar exists (an unfilled exit holds its slot); total-loss halt + drawdown once per exit instant; research arm censors plans the data cannot finish; `daily_window` DST-gap anchors skipped; paired CI only with ≥ 2 periods per arm and ≥ 99 % coverage; t clustered by period; Sharpe by the active-period rate; `half_spread` refuses unknown keys; `[backtest] max_candidates` (50 000) and `keep_runs` (100); `checks.rs` worlds at 15m (DST) / 1h / 4h / 1d / splits, moves incl. deletes and cuts, arms checked too |
| Weekend run | optional now: it adds only executable xyz weekend books (no archive has them). The frozen-binary rules below still hold if it runs; the throwaway sampler (pid in `~/.tengu/state/xmarket/research/weekend-2026-10-02/sampler.pid`) is a sleeping shell — `kill <pid>` to drop it |
| Next | `docs/xlab-2026-10-01.md` § 12: HL-archive L2 import (crypto executable books), info layer on history (EDGAR / news as `event_window` events), `ask_architect` escalation replay, capability lifecycle store, forward paper of a validated spec; local engine legs of `market_history` / `backtest` on the operator's PC |
| Review fixes (`xlab-fix-tools`) | the `backtest` tool hides a split's holdout until `"holdout": true` and counts every read (`~/.tengu/state/xlab/backtests/holdout-reads.jsonl`); `run_id` + `view` reads a stored run's periods / instruments / trades / notes by id (no more unreadable run-dir path); `market_history` applies `[backtest.splits]`; a tool run is capped at 50 000 candidates; xlab HL budget 600 / min, burst 200; skill `xlab-research` steps 2 / 4 / 5 rewritten — `docs/xlab-2026-10-01.md` § 8, § 11. Open: a sandbox-level holdout every tool enforces; reconcile the tool's 50 000 cap with the engine fix's `[backtest]` cap |
| Validation (final) | full unit suite 1,511 green; `cargo check --all-features --all-targets` clean (2 old warnings); conformance 68+ cases, offline engine matrix, lints green. Adversarial review: 17 confirmed findings (5 major), all fixed and re-verified fixed by 17 independent checks. Live legs (xlab + xlab_holdout): haiku-4.5 + Claude CLI 4 / 4; gemini-2.5-flash-lite calls every tool fine but misquotes numbers (1 / 3, 0 / 3); local: the operator's PC. Live Architect (Sonnet): final turn tunes in-sample, reads the holdout once, verdict "not robust: holdout CI crosses 0" (`docs/xlab-2026-10-01.md` § 14) |

---

## Previous: Resume here (Thu 2026-10-01 ~09:00 ET — W1 done: 34 items + the W1 gate passed; weekend run Fri, then operator decisions + W2)

| Topic | State |
|---|---|
| **Weekend run (operator action)** | Start `tengu run --sandbox xmarket-weekend` **≤ Fri 2026-10-02 19:30 New York = Sat 01:30 on this Mac (Europe/Berlin)**: tmux pane, foreground (the vault prompt suspends a `&` job), the frozen W1-gate binary `~/.cache/tengu-xm.noindex/weekend/tengu-6fcb455` started under `caffeinate -i -s` (runbook). Keep the Mac on AC power (`-s` holds only on AC), lid open, online until Mon 2026-10-05 10:00 New York (16:00 Berlin). Runbook + timeline: top of `sandboxes/xmarket-weekend/config.toml`; soak results: `docs/runtime-2026-09-30.md` § Weekend run. The throwaway sampler (pid in `~/.tengu/state/xmarket/research/weekend-2026-10-02/sampler.pid`) records the same weekend independently |
| Operator rules | 2026-10-01: workflows / parallel agents allowed for speed — keep ≤ 3 at once, each build in its own `~/.cache/tengu-xm.noindex/agents/<label>` (cloned, then `cargo clean -p tengu-cluster`), deleted after; before (2026-09-30): **one agent at a time, low priority** (the claude process runs `renice 15` + `taskpolicy -b`; every build `CARGO_BUILD_JOBS=2`, `CARGO_TARGET_DIR` under `~/.cache/tengu-xm.noindex/` — Spotlight skips `.noindex`); **never start a local model on this Mac** (Ollama server stopped; the live `local` engine check runs later on the operator's Windows gaming PC over the LAN) |
| Why | 6 parallel agents + gemma4 (10 GB) + Spotlight indexing ~30 GB of build output made the Mac lag badly |
| Merged since wave A | `x-bridge-conformance-test` 7f02717d6533558d5ba90d675dbf9a4eae680691 (hidden `tengu tool list|call`; 46 cases now), `risk-paper-fill-engine` 78593839a260664456fb3b5e1ae58a2a3628afae, `hl-ctx-tool` ea0b05b9503c0fae58d0171adf90ca1a155b4db2, `hl-book-tool` 1e6c3721a619c883900fcee2df0304641b04b60b (`tools/hyperliquid/book.rs::fresh_book` = the paper engine's live book read, wired by `risk-gate-enforcement`), `risk-gate-domain` 81e00651ff24058eaccd4f12f6a2e76f08748a14, `risk-paper-ledger-store` b71d1be57099e5914e4b8c32ccbe1e708bc562ee (`ports/paper.rs`: `place(PlaceRequest, Decide)` = gate + fill + write in one `BEGIN IMMEDIATE`), `risk-kill-switch` ea69033b92635bfbe14790a7f9a2fea6cecad363 (`tengu risk status|halt|resume`, `risk_status`; 48 conformance cases), `rt-scheduler` 24a7dd359faea06bfdcadb2366a236980b0d4192 (`[feeds.<n>]` tool / tick feeds, windows + DST-safe `at` ticks; `tengu run` refuses a feed whose tool its agent cannot run), `x-engine-matrix-smoke` 0ad160d2d28ee6156f2082ad39fc6695fa21eb6a (live 2026-09-30: gemini-2.5-flash-lite, claude-haiku-4.5 on OpenRouter and claude-haiku-4-5 on the Claude CLI × workspace / hyperliquid / xm tool sets — 9/9 green, ≈ $0.06; `tengu doctor --engines`; the local leg runs only on the operator's Windows PC via `TENGU_MATRIX_LOCAL_BASE_URL`), `risk-gate-enforcement` 5dd0a96cfbc5ed7402f98694d449ec23f27ac3d7 (exec tools refuse unless the caller is a private agent; bridge call ids `mcp:<uuid>:<id>`), `risk-paper-tools` 4209d6423668102eb3049763cdb6c6b568c5557f (`paper_order` / `paper_close` / `paper_positions`; hidden `tengu tool turn`; live matrix xm set green), `risk-audit-verdicts` 1cd924d92f3e984b6d24b1a0da948345b4a8144e (`logs/risk.jsonl` mirror, `StepOutcome::Refused`), `x-exit-rules` 845cf25efee3bd92aa00768e50a98c52d68e4f5c (`[risk.exits]`, exec tool `xm_exits` via a `kind = "tool"` feed) + ebd3d17f4091fd5c62ac176d8ccccb3c973efd18 (docs), `x-shared-workspace-and-state-layout` 5e03e86bb08dc0b76c597d3c6b959a647fc212c9 (one xmarket workspace, `[risk]` needs `[xmarket]`, state layout, prune spares `state/`), `x-weekend-fade-strategy` fbb3246a171bbfedf13f50ba98153e90ce20d357 (rule W as exec tool `xm_weekend_fade`: capped + shadow ledgers, 60 s idempotent steps, golden replay of 2026-09-26 bit-exact), `x-weekend-sandbox` bc510871c69db05b61a99ba29cfe6787b906e91f (`sandboxes/xmarket-weekend`: floor profile, one private agent, 5 required feeds, recorder; golden replay through the sandbox config; 30-min live soak green — 0 WARN / ERROR, HL weight ≤ 190 / min of 1200, SIGTERM drain 0.32 s), `ops-sandbox-config` a27f9b5560387d20daa91e7fbb5859747e3e886c (`sandboxes/xmarket` M0 stage: planner `xm`, routable `xm_architect`, private `xm_executor`; HL ctx + books recorded, exits, daily risk roll; 5-min smoke green), `x-engine-parity-audit` 1f52df0e711904d62a14a279ecba6b430511a8f8 (E0 closed: every catalog tool + a shell skill + an `[[mcp_servers]]` proxy pass lint + conformance (57 cases) + live matrix on gemini-2.5-flash-lite / claude-haiku-4.5 / Claude CLI, 12 / 12 sets each; leads 1–12 fixed — parent-session env stripped, explicit bridge workspace grant, OpenRouter failed-turn retry + `native_finish_reason`, bridge `compress_and_store`, no secrets in `--mcp-config`, shell skills bridged, chat honours `tools`, chat call ids namespaced, Privy under the egress ceiling; open: the live `local` column on the operator's PC) |
| Wave B worktrees | all merged and removed · local-leg commands for the Windows PC: `~/.tengu/state/xmarket/research/parity_audit_leads.md` |
| W1 gate (passed 2026-10-01) | Three read-only adversarial reviews — weekend path (→ 6fcb455bae5553e2d51390e4cedd334779ca0d9c, frozen weekend binary + 30-min soak 2 green), money safety (→ 0fd620b5fc5f368ba75538621b8b9fb1718156e9 access, 270f23e4f144503ca4e7564cdb6ef1118855d5f3 ledger, batch 2), engine parity / doctrine (→ batch 2) — then batch 2 in parallel worktrees: 8e35c28de6880fb53ac8e5d06d97ddbeaf33e21e, 78f6f6f0357d474587d80c863fc539892a203b94, b0e3e31a353e55fe7ba700a4740d60313af3c743, e50483c8f6271188090550ccbc35343469700438, 7e8cfb081be0d4d126dd2797940b71bf1422daca, b389d066d5aede8115c27896397888d4ce7fcd3a (tracker W1 notes). Final checks on 11900f78894867e3427ca828d10ec80b4d3d4e4b: fmt, lints, conformance, run_agent_ipc, mcp_bridge_external, offline matrix, 1328 unit tests, `cargo check --all-features --all-targets`; live engine matrix on 11900f78894867e3427ca828d10ec80b4d3d4e4b: 39 / 39 legs green on gemini / haiku / Claude CLI (one gemini wording flake passed on a rerun; `local` + Postgres legs skipped) |
| Operator decisions (open) | Tracker W1 note "operator decisions": `editor_shell` built-ins with the full env; weekend capped book limits; Docker owner name |
| Next | Fri ≤ 19:30 ET the weekend run (row above; the FROZEN binary — never run a newer one on `~/.tengu/state/xmarket-weekend/` before Mon 10:00 ET: it would add ledger columns mid-run) → Mon: analyse the weekend (fade rows, both ledgers, the sampler) → operator decisions → W2 (inputs: VPS + SSH alias, dedicated OpenRouter key) · the live `local` legs when the Windows PC is ready |
| Weekend replay inputs (outside the repo) | `~/.tengu/state/xmarket/research/replay-2026-09-26/`: `universe.txt` (75 xyz single stocks = HL `stocks` minus 17 ETFs, STRC preferred, OURA pre-IPO; delisted IBIDEN out), `weekend_2026-09-26_candles.json` (anchor / entry / exit 5m candles), `weekend_2026-09-26_golden.json` (74 names ex KIOXIA: mean net +95.5 bps, 53 positive, capped CRCL / SMSN / MINIMAX / MSTR). Also there: all 128 xyz markets' 5m candles (`c5m_all_xyz_2026-09-25_to_28.tgz`), `perpCategories.json`, annotations summary; if lost, re-fetch 5m candles for 2026-09-25 18:00 → 09-28 16:00 UTC (HL keeps ~17 days of 5m bars) |

## Earlier (2026-09-30, W1 in progress): xmarket wave A merged on `feature/xmarket`

| State | Detail |
|---|---|
| Branch | `feature/xmarket` (local then; merged to `main` as #20 on 2026-10-02); one commit per tracker item; progress + deviations: tracker § 0 "Where to begin" + "W1 notes" |
| Landed (wave A, 16 items) | E0: `x-bridge-parity`, `x-claude-code-hardening`, `x-tool-schema-lint`, `x-local-model-fit` (+ `risk-exec-idempotency-ids`) · M0: `risk-config-schema`, `ops-audit-atomic-write`, `hl-market-schema`, `rt-backoff-budget`, `hl-info-client`, `risk-calc-costs`, `risk-paper-ledger-domain`, `rt-daemon`, `rt-health` · M1: `kg-calendars`, `ops-history-recorder` |
| Running (wave B) | `hl-ctx-tool`, `hl-book-tool`, `risk-gate-domain`, `risk-paper-ledger-store`, `risk-kill-switch`, `risk-paper-fill-engine`, `rt-scheduler`, `x-bridge-conformance-test`, `x-engine-matrix-smoke` |
| Next | wave C: `risk-gate-enforcement`, `risk-paper-tools`, `risk-audit-verdicts`, `x-exit-rules`, `x-engine-parity-audit`, `ops-sandbox-config`, `x-shared-workspace-and-state-layout`; wave D: `x-weekend-fade-strategy`, `x-weekend-sandbox` + 30-min soak by Fri 2026-10-02 18:00 ET |
| New commands | `tengu run [--sandbox <s>]` (lease, heartbeat, drain), `tengu doctor --sandbox <s> --live`, `tengu history range|asof <key>` — `docs/runtime-2026-09-30.md` |
| Verified live | Claude CLI 2.1.285 merges the `--mcp-config` env (secrets reach the bridge by inheritance); without `--strict-mcp-config` it loaded 28 operator MCP servers (144 tools), with it none; all 38 tool schemas accepted by gemini-2.5-flash-lite, claude-haiku-4.5, gpt-4o-mini (OpenRouter) and parsed by Ollama 0.24; a haiku turn through the bridge returned a vault secret as `[REDACTED]` |
| How waves run | `Workflow` with `isolation: "worktree"` (worktrees start from `main` — agents `git merge --ff-only <base>` first); each agent builds in its own `CARGO_TARGET_DIR` cloned (APFS `cp -Rc`) from `~/.cache/tengu-xm/seed` — a shared `target/` let a stale test binary from a deleted worktree run (`tests/code_map.rs` "NotFound"); coordinator cherry-picks one item at a time, adds code-map rows + regenerates the html, ticks the tracker, runs the item gate |
| Weekend data | throwaway sampler pid in `~/.tengu/state/xmarket/research/weekend-2026-10-02/sampler.pid` (records Fri 19:30 ET → Mon 10:00 ET); 5m candles of the 2026-09-26 weekend for all 128 xyz markets saved outside the repo for the replay fixture |

## Next session (set 2026-09-30): build xmarket — start with the build plan

| Read | Why |
|---|---|
| `docs/xmarket-build-plan-2026-09-30.md` | **First.** The operator's mandate (build the full PRD scope; every tool works 100 % under `openrouter`, `local` and `claude_code`; a separate `xmarket-weekend` sandbox; validate and fix until it runs smoothly), "Before the first item", waves W1–W9 with gates (W1 = E0 engine parity + the weekend-run slice, deadline **Fri 2026-10-02 18:00 ET**), the engine validation matrix, operator inputs, and the kickoff prompt to paste |
| `docs/xmarket-tracker-2026-09-29.md` **§ 0 Start here** | Rules for every task (R1–R13), definition of done; § 1 milestones (E0 first); § 5 backlog (185 items) |
| `docs/xmarket-prd-2026-09-29.md` (addendum at the top) | Operator decisions: build everything, engine parity for every tool, paper first, $100 budget + `[risk]` caps, 24/7 `tengu run` on the operator's VPS, network `open` (switchable), no legal gates |
| `docs/xmarket-feasibility-2026-09-30.md` | Evidence, attached as a warning: verdict re-scope (cross-venue convergence fails after costs); the operator kept the full plan. Holdout: weekend fade passes on 53 new names, post-earnings rule not confirmed. Weekend order books are being recorded Fri 2026-10-02 → Mon 10-05 in `~/.tengu/state/xmarket/research/weekend-2026-10-02/` (throwaway sampler, pid in `sampler.pid`) — analyse them on Monday |
| `docs/xmarket-gaps-2026-09-29.md` | Per-item detail — read the entry before starting an item |

The xmarket section further down (2026-09-29 / 2026-09-30 rows) lists what is decided and what the operator still has to provide.

---

## TL;DR — current state (2026-09-29): Solana write tools (phase 6b)

Branch `feature/decision-loop` (not merged). Doc: **`docs/typed-observations-2026-09-24.md` § Write tools**. Plan: `/Users/vladimirdemidov/.claude/plans/enchanted-waddling-reef.md` (reviewed: 5 high findings folded in).

| Area | Change |
|---|---|
| Tools | 5 opt-in rows (`tools/solana/write_{tokens,swap,dlmm,perps}.rs`): `solana_close_token_accounts`, `jupiter_swap` (Ultra), `dlmm_open_position`, `dlmm_close_position`, `jup_perps_order`; `mode` = simulate (default, keyless) \| send; result `write/1` (`domain/solana_write.rs`) |
| Wire format | `domain/solana_tx.rs` (legacy compile + serialize, legacy / v0 parse, System / SPL / ATA / ComputeBudget ix), `domain/lp/{dlmm_ix,perps_ix}.rs` — byte-for-byte goldens from the bot's own libraries (`tests/fixtures/solana/tx/golden.json`) |
| Signer | `ports/solana_signer.rs`; `outbound/solana/signer.rs` = `ed25519-dalek` over `[solana] signer_key_file` (0600, no-echo errors). Send only with the tool scope's `wallets = ["<pubkey>"]` on one non-routable agent |
| Signing sandbox | `config/solana.rs`: no `claude_code`, no `[[mcp_servers]]`, no scope with `shell_bins` (runtime: permissive fallback runs no shell — `AgentConfig::no_shell_fallback`), key outside every fs root; grant rules; read_only write actions must simulate |
| Send pipeline | `outbound/solana/send.rs` + `writes_store.rs` (`<TENGU_HOME>/state/solana-writes.db`): lease per wallet, pending record before submit (resolved first next time), one-attempt `sendTransaction` (JSON-RPC error = not sent), confirm / expire by `lastValidBlockHeight`, write fence; `Submitter` seam (RPC or Ultra `/execute`) |
| State | fence pins `lp_snapshot` and makes older snapshots `stale_input` in the decide tools; `merge_lp_state` (CAS × 3): close ⇒ `reentry`, perps ⇒ `last_hedge_action` (request-aware cooldown); open keeper requests keep wSOL open / refuse SOL-leg orders |
| Verified live (keyless) | mainnet simulations: Ultra 0.01 SOL→USDC (48 640 CU), DLMM open 20 bins (163 105 CU), DLMM close of a real 46-bin position (356 418 CU), perps short increase (98 233 CU) — `cargo test --bin tengu -- --ignored live_` |
| lping | new `[agents.lp_executor]` (no `description`) runs both loops; write tools simulate-only (no signer, no grant); `lp_watch.open_position` uses the real schema, `mode = "simulate"`; `http_request` `env_reads` tightened to `SOLANA_RPC_URL`; "Signing" how-to at the end of the config |

| Open | Detail |
|---|---|
| First live send | not done: needs a DEDICATED wallet (never the bot's `F3YvPiLdniRPGpeKrbeGWR2zg2wPpzVuvqBA5BBJBQ5S` — the lease cannot see the TS bot) + the operator's go; order: close token accounts → ~$1 swap → tiny DLMM open + close → minimum perps order |
| Human approval | `TelegramConfig.tool_approvals` / `approve_only` are parsed but never read — no approval gate exists |
| Token-2022 pools | refused by the DLMM write tools (transfer-hook slices not built) |
| Privy signer | second `SolanaSigner` impl possible later |
| Loop write actions | `hedge_watch` has no write actions yet; `lp_watch.open_position` stays `dry_run` + `mode = "simulate"` |

## Previous TL;DR (2026-09-24): typed observations + Solana LP read tools

Branch `feature/decision-loop` (not merged). Subsystem doc: **`docs/typed-observations-2026-09-24.md`**. Loop doc: `docs/decision-loop-plan-2026-09-24.md`; sandbox: `docs/lping-2026-09-24.md`.

| Area | Change |
|---|---|
| Typed observations | `ToolOutput.observation` + `ToolExecutor::execute_typed` (default wraps `execute`); `domain/observation.rs` envelope (features ≤ 32 scalars, line 1 ≤ 200 with full ids, `Field<T>` / `ObsStatus` — failed reads never 0); `ports/observation.rs` + `outbound/observations.rs` (`<workspace>/.tengu/observations.db`, slot-monotonic, `Error` rows never stored, 7-day purge); `application/observe.rs::observe()` |
| Decision loop | `world` + `world_max_age_secs`, action `requires`, `FromObservation` slots, typed results + `HistoryEntry.obs`; loop tools wrapped in `SanitizedToolExecutor` (`build_decision_loop(.., secrets)`) |
| Review (2026-09-25) | 6-lens adversarial review: 31 findings, 11 refuted, 20 confirmed (5 medium, 15 low; none reachable with money — dry-run, no signer) and all fixed with regression tests: empty discovery bounded by caller max age (TTL 10 s), explicit-`positions` snapshots never stored under the canonical key, share > bin supply ⇒ incomplete, oracle reuse ≤ 10 s, `lp_state` compare-and-swap commits + unreadable ⇒ block, position/discovery read errors ⇒ `invalid_read`, grace clock per bot, in-cycle storm/freeze via `lp_knobs`, current-event `FromHistory`, JSON-RPC error text scrubbed, ids never cut in slot descriptions, base58 length pre-check. Open: cross-tool slot mixing of cached `acct/1` rows (share ≤ supply catches only part); architecture `.svg` has no decision-loop panel |
| Solana tools | 11 opt-in observe/plan tools (`tools/solana/`): `sol_price`, `dlmm_pools`, `dlmm_pool`, `dlmm_positions`, `jup_perps`, `solana_wallet`, `solana_tx`, `lp_snapshot`, `lp_swap_plan`, `hedge_decide`, `lp_decide`; plus 5 live-capable writes (`simulate \| send`). `lp_swap_plan` ports the bot's reserve/rent/collateral-aware deposit swap planner. Pure: `domain/solana.rs`, `domain/lp/{dlmm,perps,wallet,market,gates,hedge,snapshot}.rs`. IO: `outbound/solana/{rpc,accounts,http_json,plan}.rs` |
| Policy port | `domain/lp/hedge.rs` = bot hedge controller, 1027/1027 production vectors (`cargo test --bin tengu lp::hedge`); `gates.rs` = re-entry, storm, trend/regime confirm, composition, wallet 50/50, 70-bin range cap |
| Crates | `base64 = "0.22"` direct; `curve25519-dalek ~4.1` dev-dependency only (off-curve cross-check). No bs58 / solana-sdk / anchor / borsh |
| lping | 11 observe/plan + 5 live-capable atomic write tools on private `lp_executor`; `lp_watch` and `hedge_watch` remain `dry_run` until a dedicated signer/wallet grant exists. `lp_swap_plan` supplies the deterministic pre-deposit route. Raydium LP + Hyperliquid writes are still gaps. |
| Egress | new hosts: `api.mainnet-beta.solana.com` (or `$SOLANA_RPC_URL`'s host), `lite-api.jup.ag`, `dlmm.datapi.meteora.ag`, `hermes.pyth.network`; RPC URL never rendered (host only) — `docs/egress-2026-09-16.md` |

| Open | Detail |
|---|---|
| Phase 5 — push feed | Yellowstone gRPC → `acct/1:<pubkey>` rows (slot-monotonic put) + heartbeat row `stream/1:<name>` so unchanged accounts count as fresh; subscription set = union of `lp_snapshot` `data.watch`; trigger via `DecisionLoop::handle_event`; `egress::grpc_channel` |
| ~~Phase 6b — writes~~ | LANDED 2026-09-29 — see the TL;DR above |
| Pyth 401 | Hermes (and the benchmarks mirror) answer 401 → Pyth only with `pyth_feed_id` (`auth_required` error, Partial row); default oracle is Jupiter-only (`degraded = true` unless a pool cross-check is given) |
| ATA-only balances | `solana_wallet.balances` and `lp_snapshot.wallet_balances` count the mint's ATA only; tokens in other accounts appear only in `token_accounts` rows |
| Extended positions | > 70 bins decode fully (SDK-verified on a 164-bin position) but raise `ExtendedPosition`; a missing bin array ⇒ `complete = false` (amounts are a floor, row Partial); farming rewards + Token-2022 transfer fees not modelled; new ranges capped at 70 bins |
| Two-read slot skew | `plan::read_pool` = 2 GMAs (2nd pinned ≥ 1st slot) + reused `acct/1` rows up to their max age → `LpSnapshot.slot` (min) ≠ `slot_max` is possible; a lagging public node answers `-32016` (retried once) |
| Hedge knobs `trend_confirm_ms` + `no_lp_grace_ms` | LANDED: clamp-regime confirm for `lp_input = "midpoint"` and bot BUG-011 grace (counts from the first no-LP read, `LpControllerState.no_lp_since_ms`; no re-entry wait ⇒ action `none` "no-LP grace"; `0` = off). `hedge_decide` optional `lp_knobs` computes storm / imbalance freeze in-cycle (hedge_watch passes them) |
| Non-USDC-quote storm | `lp_decide` storm samples come from the USD `price_oracle` row; for a pool whose quote is not USDC they are dropped → `move_5m_pct = None`, storm never fires |
| Cache | no cross-process single-flight (WAL prevents corruption, not duplicate RPC on a miss); `acct/1` rows hold base64 data (bin array ≈ 13.5 KB), only the 7-day purge bounds growth |
| `sol_price` keys | pool-aware: `price_oracle/1:<mint>` vs `price_oracle/1:<mint>:<pool>` — a `world` alias must name the key the loop's `sol_price` call writes |

### 2026-09-29 — `sandboxes/jev-exec` (Claude architect → Jev executor)

| Change | Detail |
|---|---|
| Sandbox | `sandboxes/jev-exec/config.toml`: architect (in-process `claude_code`, subscription) whose only usable tool is `run_command` → `tengu decide --loop executor`; executor = Jev loop over `http_request` / `list_directory`. Verified live end to end — `docs/decision-loop-plan-2026-09-24.md` § Jev as an architect's hands |
| `tengu decide` | prints `history` (args + reduced result per step) — `DecisionLoop::history()` |
| TUI fix | direct (no-orchestrator) turns passed `bridge_tools: None` and the normal rebuild never set them → an in-process `claude_code` agent in `tengu chat` had NO tengu tools (only `/skill` commands did). Both paths now pass the bridge tools |
| Jev decision feed | audit lines gain `ok` + `output`; `tengu chat` on a config with `[decision_loops]` shows each decision of those loops live (`jev executor #1 · crypto_price (1.00) → executed` + slots, args, output). `view::hide_thinking` removed the LAST bubble, not the spinner — any mid-turn System bubble (feed, orchestrator events) was lost; now removes the indicator by index |
| Vault prompt fix | a `tengu` started by a tool (`run_command` → `tengu decide`) re-prompted `Master password:` on `/dev/tty` while the TUI owned it → "Engine stream timed out — no data for 120s". The first `tengu` to open the vault now sets `TENGU_SECRETS_LOADED` (loaded key names, set even on failure; forwarded to the Claude Code bridge); descendants inherit the secrets, register them for redaction, never prompt |

| Open | Detail |
|---|---|
| Plugin MCP leak | `claude -p` also loads the user's global Claude Code plugin MCP servers into every `claude_code` agent (outside tengu scopes/egress). Candidate fix: `--strict-mcp-config` in `engines/claude_code.rs` |
| Hand-off paths | `--sandbox` is cwd-relative and `TENGU_CONFIG` is not forwarded to the bridge → jev-exec hardcodes `~/development/tengu-cluster`. A `decide` tool or `--sandbox` resolution from `$TENGU_HOME` would remove it |
| Jev args | slots are enumerated only; a `FromEvent` slot source would let the architect pass values |

### 2026-09-29 — `xmarket` PRD + gap tracker (planning only, no code)

| Doc | What |
|---|---|
| `docs/xmarket-prd-2026-09-29.md` | Operator PRD, verbatim: event-driven cross-market trading intelligence (news / X / EDGAR + Hyperliquid HIP-3 + Robinhood Chain → Jev → risk gate → paper) |
| `docs/xmarket-tracker-2026-09-29.md` | The backlog: 179 items in M0–M8 + M3b (live pilot), 20 conventions, accounts + operator setup, decisions, risks, verified facts |
| `docs/xmarket-gaps-2026-09-29.md` | Per-item research notes (files, API shapes, no-Rust options, evidence) — look up by id |

| Open | Detail |
|---|---|
| M0 not started | thin paper slice running 24/7 on the operator's Hetzner / Hostinger VPS (Docker): bridge parity first, then `tengu run` + HL reads + EDGAR 8-K → one Jev loop → risk gate ($100 budget) → paper fill → exit rules → audit (40 items); M3 is a go / no-go edge check; M3b = live pilot on a $100 Hyperliquid sub-account after an M3 go |
| Operator decisions (2026-09-30) | Operator in Kazakhstan, no legal or regulatory gate in the plan (venue choice is the operator's; only effect: Kazakh connections cannot reach Coinbase, OKX, … → deploy outside); `network = "open"`, switchable later (convention 17); one OpenRouter key for Jev + LLM at $40 / day. `SEC_USER_AGENT` set in `.env` and verified 2026-09-30. Budget $100, paper first then real (M3b), server = operator's Hetzner or Hostinger VPS. Pending operator action: create the xmarket OpenRouter key; pick the server + SSH alias (tracker § 6, operator setup) |
| Doc contradiction | OpenRouter over Tor: `docs/egress-2026-09-16.md` says reachable, `docs/lping-2026-09-24.md` says blocked — `hl-tor-probe` (M1) settles it |
| Claude Code rule (2026-09-30) | Every tool must work under `engine = "claude_code"`, no exceptions — CLAUDE.md / AGENTS.md step 4 + gotcha, `docs/tools.md` step 5, tracker convention 20. The bridge is not at parity today (default config + `main` agent, empty `SecretRegistry`, `no_shell = false`, no `TENGU_CONFIG` / `--strict-mcp-config`): M0 items `x-bridge-parity`, `x-claude-code-hardening`, `x-bridge-conformance-test` |
| ~~Stale gotcha~~ fixed 2026-09-30 | CLAUDE.md / AGENTS.md said only one sandbox runs `open`; now they list lping, jev-exec, unlimited (and the planned xmarket) |

---

## Previous TL;DR (2026-09-23): hexagonal layout

Branch `refactor/hexagonal` (not merged). Plan + per-phase status: `docs/hexagonal-plan-2026-09-23.md`. Start any "where is / how do I" question at **`docs/code-map.md`** (+ interactive `docs/code-map.html`).

| Area | Change |
|---|---|
| Layout | `src/{domain,ports,config,application,bootstrap}` + `src/adapters/{inbound,outbound}`; `main.rs` is 14 lines. Old `channel_runtime.rs` / `engine_builder.rs` / `types.rs` / `tool_plugin.rs` / `plugins/` / `*_builder.rs` are gone — every doc path was rewritten |
| Enforcement | `tests/layering_lint.rs` (layer rules, 0 exceptions), `tests/code_map.rs` (code map lists every file, graph block current — regen `TENGU_REGEN_CODE_MAP=1 cargo test --test code_map`), `tests/scope_lint.rs` (now actually scans `outbound/tools` + `mcp_client`) |
| Tools | One `ToolEntry` row in `adapters/outbound/tools/mod.rs::catalog()`; opt-in names in `domain/tools.rs::WORKSPACE_TOOLS`. Guide: `docs/tools.md` |
| New ports | `Embedding`, `MemoryService`, `RecallStore` (`ports/memory.rs`), `ToolDirectory` (`ports/tool.rs`); `SecretRegistry` → `domain/secrets.rs` |
| MCP | `[[mcp_servers]]` tools are `{server}__{tool}` and reach plan-step subagents (both engines) + in-process TUI/Telegram agents (see rows below) |

| Open | Detail |
|---|---|
| ~~No local-model engine~~ added 2026-09-23 | `engine = "local"` (+ optional `[agents.<n>.local] base_url`, `api_key_env`; defaults Unsloth `http://127.0.0.1:8888` / `UNSLOTH_API_KEY`). Own engine `engines/local.rs` (`LocalEngine`), direct connection, not part of `[egress]`; keyless = no auth header. Tests: `engines::local::tests::*` (keyless + tool call, bearer, unreachable server), `local_engine_parses_with_and_without_block`. Live: `run-agent` on Ollama `gemma4:latest`, Tor down → `status=ok`. Not tried against a real Unsloth install (not installed here). |
| ~~`[[mcp_servers]]` tools invisible to plan-step subagents~~ fixed 2026-09-23 | `build_subprocess_tool_executor` now advertises the executor's MCP tools (through `tools`); Claude Code engine passes servers to the bridge (`TENGU_BRIDGE_MCP_SERVERS` + forwarded `$VAR`s), bridge registers `McpPlugin`. Names `{server}.{tool}` → `{server}__{tool}` (providers reject `.`). Tests: `mcp_client` fake-server test, `subprocess_executor_advertises_mcp_server_tools`, `claude_code` bridge-config test, `tests/mcp_bridge_external.rs` (real `tengu mcp-bridge` ↔ `tests/fixtures/fake_mcp_server.sh`). Not tested against a live LLM. |
| ~~In-process Claude Code agents (TUI/Telegram) don't see `[[mcp_servers]]` tools~~ fixed 2026-09-23 | `bootstrap::tools::with_mcp_bridge_tools` lists the servers once at agent setup and appends `{server}__{tool}` to the bridge list; `ChatRuntimeService.mcp_servers` / `ChatTurnInputs.mcp_servers` reach `EngineContext`. Test: `in_process_claude_code_agent_gets_mcp_tools_and_servers`. |
| ~~Flaky test `learner_state::tests::save_is_atomic_concurrent`~~ fixed 2026-09-23 | Real bug: temp names were `.<name>.tmp-<nanos>`; macOS clocks tick in µs, so concurrent writers collided and the losing `rename` failed. Now a UUID suffix (`evolve.rs::unique_suffix`) in `learner_state::save`, `manage_skill`, `skill_distill`, evolve apply, `tengu skill install`. Regression test `concurrent_saves_never_collide` (8×100; failed 3/3 before). |
| ~~Webhook agents on `engine = "claude_code"` get no bridge tools~~ fixed 2026-09-23 | `adapters/inbound/webhooks.rs` passed `bridge_tools: None` (no tengu or MCP tools). Now `bridge_inputs` hands the executor's tool list (catalog + skills + `{server}__{tool}`) and `[[mcp_servers]]` to Claude Code engines. Test `claude_code_webhook_agent_gets_bridge_tools_and_mcp_servers`. |

---

## Previous TL;DR (2026-09-18)

Tor-by-default egress, one config per sandbox, deploy/tor = Arti + lyrebird-rs.
Everything below is **uncommitted on `main`** (on top of the 2026-09-12 /
2026-09-16 work, also uncommitted). `cargo check --all-features`, `cargo fmt
--check`, scoped tests, `tests/run_agent_ipc.rs`, `tests/scope_lint.rs` pass.

| Area | Change |
|---|---|
| `[egress]` default = Tor | `EgressConfig.network = "tor" \| "open"` (default `tor`). `resolved()`: tor → `proxy` = `TENGU_TOR_PROXY` or `socks5h://127.0.0.1:9050`, `route_llm_api = true`; open → direct. Explicit values win; `route_llm_api` is now `Option<bool>`. Children receive the *resolved* config via `TENGU_EGRESS`. `warn_if_proxy_unreachable` (parent only, after sandbox resolution). `tengu doctor` prints `network`. |
| Claude Code + Telegram over Tor | `claude_code_profile` no longer refuses `route_llm_api`; the CLI child gets `HTTPS_PROXY`/`HTTP_PROXY` = HTTP CONNECT form of the proxy (`EgressPolicy::http_connect_proxy`, `claude_cli_env`; Arti serves CONNECT on the SOCKS port). `TelegramPipe::build_bot` builds teloxide's reqwest 0.11 client itself (`reqwest011` alias, 2026-09-19). |
| One agent schema | `agents/*.toml` + `src/adapters/agents/` (`AgentSpec`) **deleted**. `AgentConfig` gained `description` (presence = planner-routable), `example_queries`, `tools`, alias `skills` → `skill_packages`; `LimitsConfig.step_timeout_secs` (default 600). `shared_files::routable_agents` feeds `render_registry`; `RagPlanner::new(.., agents, ..)`; `SubprocessRunner::new(sandbox, session, agents)` — fail-fast on unknown agent, per-step `max_tool_rounds` / `step_timeout_secs` (the old spec `max_turns`/`timeout_secs` were never wired — parent always sent 20 / 180s). `run_agent_subprocess` loads the parent config first, then `[agents.<name>]`; `bootstrap::tools::subagent_config` replaces `agent_config_from_spec`; subagents now run with their real `limits` and per-agent `claude_code` profile. `skill_doctor(&config, ..)`. Also fixed: `tools = [...]` on `[agents.*]` used to be silently dropped (no field). |
| Decision loops (2026-09-24) | Branch `feature/decision-loop`. `[decision_loops.<name>]` = Jev (`~typesafe/jev-latest`, OpenRouter `/api/alpha/decisions`) picks action + arg slots, existing tools execute via the loop agent's executor. Files: `domain/decision.rs`, `ports/decision.rs`, `config/decision_loop.rs`, `application/decision_loop/`, `outbound/decisions.rs`, `bootstrap/decision.rs`, `cli/decide.rs`; webhooks gained `loop` + `auth_header_env` (Helius). `MetricsKind::Decision`; audit `<TENGU_HOME>/logs/decisions.jsonl`. Verified live (`tengu decide`, webhook 401/202/escalation). Open: history lost on restart, gRPC feed (phase 5), Solana LP tools + signing (phase 6). Plan: `docs/decision-loop-plan-2026-09-24.md` |
| lping sandbox (2026-09-24) | New `sandboxes/lping/config.toml` placeholder: `lping` planner + `crypto_researcher` (OpenRouter, `http_request`), `network = "open"`, webhook `solana_events` disabled. Plan + open gaps (webhook `Authorization`-header auth for Helius, stream consumer, Jev decisions gate, Solana tools): `docs/lping-2026-09-24.md` |
| Sandboxes | `storage-test`: description/tools, `model = "claude-sonnet-4-6"`. `unlimited`: explicit `[egress] network = "tor"`. `config.example.toml` documents `[egress]` + a subagent example. |
| deploy/tor | `deploy/snowflake/` (Go lyrebird + socat, fixed-IP subnet) and `refresh-snowflake-bridges.sh` deleted. `deploy/tor/Dockerfile` = Arti 2.6.0 (`--features http-connect`) + lyrebird-rs built from the BuildKit named context `lyrebird-rs` (`../lyrebird-rs`, `LYREBIRD_RS_SRC` = dir or git URL). `arti.toml`: managed transport (`path = /usr/local/bin/lyrebird`, `run_on_startup = true`, protocols obfs4 + snowflake), Tor Browser bridge lines (7 obfs4 + 2 snowflake from lyrebird-rs `tools/arti-e2e/bridges-*.txt`), `tor-state` volume. `compose.yml`: one `tor` service on `127.0.0.1:9050`, healthcheck = `check.torproject.org` `IsTor:true`. `docker-compose.tor.yml` includes it and sets `TENGU_TOR_PROXY=socks5h://tor:9050`. |
| Makefile | `NETWORK=tor` (default) / `open` selects the compose file set for `up`/`up-memory`/`down`/`logs`/`status`/`doctor`/`clean`/`build`. `tor`, `tor-down`, `tor-logs`, `tor-bridges` (calls lyrebird-rs `bridges.sh`). Removed `up-tor`, `down-tor`, `tor-native`, `tor-native-down`. `LYREBIRD_RS_DIR` (default `../lyrebird-rs`) exported as `LYREBIRD_RS_SRC`. |
| Docker sandbox (2026-09-19) | `make up` had no way to pick a sandbox (always `./config.toml`). Now `SANDBOX=<name>` → `TENGU_CONFIG_FILE=sandboxes/<name>/config.toml`, bind-mounted by compose at `/opt/tengu/config.toml`; `NETWORK` defaults to that file's `[egress] network` (explicit `NETWORK=` still overrides); `check-config` guard on `up`/`up-memory` (a missing file used to make Docker create a `config.toml/` directory); `down --remove-orphans`; `make down-all` (both compose file sets + standalone `make tor`); `make chat` = `run --rm tengu chat` with the same wiring (README's `docker compose run tengu chat --sandbox <name>` could never work: missing `./config.toml` becomes a directory). Open: `install.sh` / `cloud-init.yml` take no sandbox; image has no Claude Code CLI (`claude_code` sandboxes can't run in Docker); `~` in sandbox paths = `/root` (not persisted). |
| Telegram over Tor (2026-09-19) | Docker `make up SANDBOX=unlimited` crash-looped: `GetMe` timed out. teloxide 0.10 defaults are 5s connect / 17s total and it does not extend the total for the 10s long poll; `api.telegram.org` over Tor measured 11–15s (sometimes >10s to connect). `TelegramPipe::build_bot` now builds the reqwest 0.11 client itself (`reqwest011` alias in `Cargo.toml`, `telegram` feature; lockfile gains only the dep edge) — proxy + 30s connect / 60s total; open network keeps teloxide defaults. `TELOXIDE_PROXY` no longer used. `sandboxes/unlimited` gained `[telegram] allowed_users` (empty rejected every message). |
| unlimited: LLM off Tor (2026-09-20) | OpenRouter is Cloudflare-fronted -- 403 "Just a moment..." on Tor exit IPs. `sandboxes/unlimited` set `route_llm_api = false` (network stays `tor`): LLM API direct, `http_request`/shell still proxied. Works for NATIVE `chat`/`telegram`. Does NOT work for Docker `make up SANDBOX=unlimited`: the container is on the internal `tor-front` net, so "direct" has no route to OpenRouter (`doctor` `IsTor=true`/`llm api: direct` only tests the tool client, not an LLM call). Docker + this sandbox would need `network = "open"` or a second internet-capable network on the tengu service. |
| Docker/installer | `Dockerfile` no longer copies `agents/`; `.dockerignore` no longer excludes `sandboxes/` (the previous `COPY sandboxes` could not have worked). `install.sh`: `TENGU_NETWORK`, `LYREBIRD_RS_SRC`/`LYREBIRD_RS_REPO`, clones lyrebird-rs, drives `make up NETWORK=…`; `cloud-init.yml` clones lyrebird-rs to `/opt/lyrebird-rs`, systemd uses `make up`/`make down`. |
| `http_request` | Hop-0 egress + `net_hosts` gate moved from `PreparedRequest::from_args` into `execute` (audit record on denial unchanged); `tests/scope_lint.rs` passes again. |
| Repo cleanup | Root `index/features/howto/memory/context.html` deleted (`docs/*.html` are the maintained pages); `Adaptive_AI_Learning_Marketplace_PRD.md` + `tengu/ideas/*` → `docs/ideas/`; `docs/configs/tui-memory-smoke.toml` deleted; empty `memory/`, `scripts/`, `.worktrees/` removed; `.githooks/pre-commit` now `cargo fmt --all --check` (it pointed at a script that did not exist). |
| Docs | README, `docs/configuration.md`, `docs/egress-2026-09-16.md`, `CLAUDE.md`/`AGENTS.md`, `config.example.toml` rewritten by hand; every other doc/diagram/skill/inline comment audited and fixed in place by the 2026-09-18 workflow (11 auditors, 65 files). `docs/architecture-v2.md` deleted (condensed duplicate of `REDESIGN.md`); superseded banners on `docs/architecture.md`, `docs/harness-architecture.md`, comparison/radar pages, every `docs/superpowers/*`. |

### Review findings applied (2026-09-18 workflow: 4 lenses × 3 skeptics, 22 raised, 20 confirmed)

| Finding | Fix |
|---|---|
| `-c/--config` never reached `run-agent` children (they resolve `$TENGU_CONFIG`) | `main` pins `TENGU_CONFIG` to the resolved path right after clap; verified with a live child run |
| `AgentConfig` lost the strict schema `AgentSpec` had | `#[serde(deny_unknown_fields)]` on `AgentConfig` + tests (`max_turns`/`timeout_secs`/`sandbox` rejected, `skills` alias, blank `description`, `step_timeout_secs = 0`) — all three sandboxes, `~/.tengu/config.toml` and `config.example.toml` still load |
| Non-routable blocks (no `description`) could still be dispatched | `run_step` and the child both filter on `description.is_some()`; error lists routable agents |
| Child used raw `toml::from_str` (no env substitution / validation) and swallowed parse errors | `Config::load` with an `error!` log; built-in defaults only when the file is absent |
| Child used `workspace = "~/…"` unexpanded | `expand_tilde` before scopes/memory/engine |
| `tengu skill doctor` could not see sandbox agents | `skill doctor --sandbox <name>` |
| Telegram `set_var(TELOXIDE_PROXY)` per message from tokio workers | `Bot` built once in `TelegramPipe::new` (`build_bot`), cloned per send |
| `http_connect_proxy` broke IPv6 proxy hosts | authority built from `host_str()` (brackets kept) + test |
| Startup probe checked only the first resolved address | tries every address (reqwest semantics) |
| MCP stdio / shells got `NO_PROXY=""` while MCP http exempted loopback | one `LOOPBACK_NO_PROXY` for all proxied children |
| Standalone `tengu mcp-bridge` ignored the operator's `[egress]` (forced Tor) | loads `$TENGU_CONFIG` / `<TENGU_HOME>/config.toml` `[egress]` when `TENGU_EGRESS` is absent |
| `make doctor` always passed `--tor` (fails under `NETWORK=open`) | `--tor` only under `NETWORK=tor` |
| `make tor` + `make up` both published 127.0.0.1:9050 | `deploy/tor/compose.internal.yml` (`ports: !reset []`) merged via the include path list in `docker-compose.tor.yml` |
| Relative `LYREBIRD_RS_SRC` resolved against `deploy/tor/`; `tor-bridges` ignored it | `LYREBIRD_RS_DIR` is the single knob (abspath unless `://`), exported as `LYREBIRD_RS_SRC` |
| cloud-init unit `TimeoutStartSec=120` < Tor bootstrap; ufw/bind comments implied 7080 is reachable under Tor | `TimeoutStartSec=900`; comments scoped to `NETWORK=open` |
| rustfmt failing on the hoisted `http_request` gate; scope_lint regex needs `scope.check_` on one line | formatted; gate kept on one line |
| Deleted `AgentSpec` tests had no successors; new paths untested | `config::` (3), `shared_files::registry_lists_routable_agents_only`, `channel_runtime::subagent_config_merges_workspace_tool_optins_from_tools`, `runner::run_step_fails_fast_on_unknown_agent`, `egress::` (+3) |
| Stale help text / comments (`agents/*.toml`, `agent_config_from_spec`, `tengu_outputs`, `RagStore`, Qdrant, `tengu.toml`, missing spec paths) | swept across `src/` (inline-comment auditor + follow-up); `skills/orchestrator/plan_schema.json` wording; `skills/orchestration-e2e/evals/config.toml` workers gained `description` so the eval can route |

### Verified (2026-09-18)

| Check | Result |
|---|---|
| `make tor` (Arti + lyrebird-rs image, obfs4 managed transport) | container healthy; Arti log `[pt lyrebird] connected`, `guard [… via obfs4 …] is usable` |
| `curl --socks5-hostname 127.0.0.1:9050 https://check.torproject.org/api/ip` | `IsTor:true` |
| `curl -x http://127.0.0.1:9050 …` (HTTP CONNECT — the Claude CLI / teloxide path) | `IsTor:true` |
| `tengu doctor --sandbox unlimited --tor` and `tengu doctor --tor` on a config with no `[egress]` | `network: tor`, `llm api: via proxy`, `tor: IsTor=true`, exit 0 |
| OpenRouter `GET /api/v1/models` and `api.telegram.org` through the proxy | HTTP 200 / 302 |
| `docker compose … config` for `deploy/tor/compose.yml`, base, base + tor override | resolves; `lyrebird-rs` context = `/Users/…/lyrebird-rs`, `TENGU_TOR_PROXY` set on tengu |
| CI gate (final) | `cargo fmt --all --check` clean; `cargo test --all-features` = 375 unit + 4 `run_agent_ipc` + 2 `scope_lint`, 0 failed; `cargo clippy --all-features` = same 40 pre-existing warnings as before this session, none new |
| Strict schema | `tengu doctor --sandbox {storage-test,unlimited}`, `tengu status` on `~/.tengu/config.toml`, `config.example.toml` all load under `deny_unknown_fields` |
| `--config` propagation | child spawned with only `TENGU_CONFIG` (as pinned by the parent) resolves `[agents.foo]` from that file and builds its engine |
| `make -n doctor` / `NETWORK=open` / `LYREBIRD_RS_DIR=../foo` / `LYREBIRD_RS_DIR=https://…` | expand as intended; `bash -n deploy/install.sh` ok |
| Docker | `tengu-cluster:latest` builds (bakes `sandboxes/` + `skills/`, no `agents/`); `tengu-tor:latest` builds; merged compose config: `tor` has no host port inside the project, `tengu` no ports, `TENGU_TOR_PROXY` set |

### Open after this pass

| Item | Note |
|---|---|
| lyrebird-rs on GitHub | **Closed 2026-09-18** — pushed to `DemidovVladimir/lyrebird-rs` at `082ea0254fd057c617fdad86158129d0ec78aaec`; a fresh clone matches the local checkout, and `LYREBIRD_RS_DIR=https://github.com/DemidovVladimir/lyrebird-rs.git` builds `tengu-tor` from the git context (every lyrebird layer a content cache hit against the local build). `install.sh` / `cloud-init.yml` clones now work. |
| Claude Code CLI proxying is env-based | `HTTPS_PROXY` (advisory); network-enforced only under Docker `make up`. |
| Docker `make up` end-to-end | both images build and the merged compose config is right; a full Telegram session over Tor was not exercised in this pass. |
| `sandboxes/unlimited` model | The working tree changed `moonshotai/kimi-k3` → `qwen/qwen3.8-27b` (+ identity name) **before** this session (already modified at session start); the 900 s timeout / 32k cap comments still mention kimi-k3. Confirm or revert that hunk before committing. |
| `skills/orchestration-e2e/evals/config.toml` | Pre-existing: `workspace_tools` lists `http_request` / `memory_search` / `memory_ingest`, which `Config::validate` rejects (`tengu status` on it fails with 5 issues); `tengu eval` loads it through its own path. Also `skill_distill` seeds `evals/config.toml` with the calling agent's engine while `eval_builder::load_eval_config` refuses `claude_code` (auditor finding, not fixed). |
| Skill tier precedence | `shared_files::scan_skill_summaries` (registry) is project-first while `skill_builder::skill_directories` / `view_skill` are managed-first (auditor finding, not fixed). |
| `adapters::outbound::engines::build_planner_engine` | `#[allow(dead_code)]`, only consumer of `[claude_code] timeout_secs`; delete or wire (auditor finding). |
| Local Docker leftovers | `tengu-snowflake:latest` image from the deleted stack is still in the local Docker cache (`docker rmi tengu-snowflake:latest`). |
| Secrets in git history | unchanged from 2026-09-12 — rotate + purge. |

---

## Previous state (2026-09-12)

Audit-and-fix pass over the uncommitted agentic-memory tree (two Workflow
runs: 6 finders + 6 skeptics, then 4 fix clusters + 1 verifier). Everything
below is **uncommitted on `main`**; every feature combo compiles, scoped
tests pass, `cargo fmt --check` is clean.

| Area | Change |
|---|---|
| Scopes | `[default_scopes]` / `[agents.*.scopes]` are now **enforced** (were parsed, never used). `Config::fold_default_scopes` at load; `resolve_tool_scopes` in `build_tool_executor` + MCP bridge via `TENGU_BRIDGE_SCOPES` (`ClaudeCodeEngine::with_scopes`); children get their own workspace in `fs_roots` (`grant_workspace_root`). `check_env_read` honours `"*"`. |
| agentic_memory | `execute` gates on `env_reads` for `TENGU_MEMORY_DATABASE_URL` (scope_lint passes); wrong-dim embeddings fail-soft on event insert + hybrid recall; `capture` defaults `session_id` / `agent` from `TENGU_SESSION_ID` / `TENGU_AGENT_NAME`; `pg_trgm` dropped from `ensure_schema`; DDL runs once per process (`OnceCell`). |
| Plan hand-off | `AgentIpcInput.plan_state` (per-session, `shared_files::set_active_plan`) is the source of truth; `TENGU_PLAN.md` is a debug artifact + fallback. Fixes the cross-session race for webhooks / Telegram. |
| Planner registry | Write failure of `TENGU_PLANNER_REGISTRY.md` is fail-soft (roster kept in memory). TOOLS section lists MCP server tools again (`<server>__<tool>` since 2026-09-23, enumerated once per `RagPlanner`). |
| Config | `OrchestratorConfig.engine` defaults to `"rag"` and is validated; dead `MemoryConfig` qdrant/backend/vector_size/embedding_provider/ttl_days fields + `[rag]` removed; `AgentConfig.requires` removed; `AgentSpec` is `deny_unknown_fields` + engine validated; `skill_lifecycle.fixture_runner_agent` optional; `TENGU_CONFIG` env honoured (`--config` > `$TENGU_CONFIG` > `<TENGU_HOME>/config.toml`). |
| CLI | `tengu doctor` exits non-zero on engine build failure (Docker HEALTHCHECK). `run-agent` exports `TENGU_AGENT_NAME`. |
| Dead code | `channel_runtime::build_vector_stack`, `VectorStore::delete_older_than`, `src/adapters/rag/**`, `src/application/memory/vector/qdrant.rs`, `__deltest.tmp` removed. `regex-lite` dropped; `rust-version = "1.78"`. |
| Tests | `tests/run_agent_ipc.rs` (Rust) replaces `scripts/test-runner.sh`; `tests/memory_search_tool.rs` (grep-test) and `scripts/phase-0-checks.sh` deleted. CI: single `quality.yml` (fmt, check --all-features, clippy advisory, test). |
| Run docs | README rewritten (real CLI table, features, mixed engines, Postgres memory, webhooks); Makefile `up-memory`/`native-memory` replace qdrant targets; Dockerfile bakes `agents/` + `sandboxes/`, exposes 7080 (webhooks); compose/installer/cloud-init use `postgres-memory` + real GitHub URL; `docs/configuration.md` has the env-var table; `config.example.toml` matches the real schema; root HTML pages de-Qdrant'd (`rag.html` deleted). |
| `unlimited` sandbox (2026-09-13) | `sandboxes/unlimited/config.toml` — direct-agent bench on OpenRouter DeepSeek (`unlimited`=v4-flash default, `pro`=v4-pro, `r1`=r1-0528; Telegram `@pro:` routing). Verified through `tengu run-agent` on all three models — recipe + results in `sandboxes/unlimited/BENCH.md`. |
| `tengu prune --hard` (2026-09-14) | `prune.rs::plan_prune` now takes `PruneOptions` (was 3 positional args). New `--hard` flag: with `--sandbox`, **empties the workspace root entirely** — enumerates every direct child (`.tengu/`, `memory/`, and any arbitrary agent-created dir like `image_payload/`) so the allow-list gap no longer leaves generated folders behind; keeps the root dir itself so it's reusable. Soft prune unchanged (allow-list + `scaffold.project.directories`). Never touches `sandboxes/<name>/config.toml`; does not clear Postgres `agentic_memory` (that's `make clean`). 2 unit tests in `prune.rs`; `docs/configuration.md` Reset section updated. |
| OpenRouter body-read timeout + output cap → config (2026-09-16) | Two `[limits]` knobs now drive the OpenRouter engine (were hardcoded / dead). **`request_timeout_secs`** (default 600) is the reqwest total timeout, body read included — `stream: false` means the body arrives only when generation ends, so slow reasoning models (`kimi-k3`) previously hit the 120s wall as `error decoding response body`. **`max_output_tokens_per_turn`** is now actually sent: set → `max_tokens: <n>`, unset → field **omitted** (model/provider default; no more synthetic `context÷8` send-ceiling). The `context÷8` formula survives only as a budget-reservation estimate when unset (`OpenRouterEngine::max_output_tokens_per_turn`, prompt-budget/TUI only). Body-read errors print the full cause via anyhow `{:#}`. Wiring: `build_openrouter_engine_with_limits(model, ctx, timeout, cap_opt)`; `build_engine` passes `agent_config.limits.{request_timeout_secs,max_output_tokens_per_turn}`; planner/eval use `config::default_request_timeout_secs()` + `None`. Tests: `engine_builder::tests::{body_read_failure_surfaces_source_chain, max_tokens_omitted_when_unset_sent_when_configured, budget_reservation_reflects_configured_cap}`. Note: `engine.run().await` sits outside the `stream_event_timeout_secs` idle loop, so `request_timeout_secs` is the only limit on a non-streaming call, and cancel isn't checked while it waits. |
| `[egress]` — Tor / host allowlist / audit (2026-09-16; **superseded by the 2026-09-18 section above**: Tor is now the default, `deploy/snowflake` + `tor-native`/`up-tor` are gone) | New `src/adapters/outbound/egress.rs`, process-wide policy (`install`; `TENGU_EGRESS` to `run-agent` + `mcp-bridge`). `proxy` (socks5h, reqwest `socks` feature) on the tool client (http_request, crypto), MCP-http, and — with `route_llm_api` — OpenRouter/embeddings/wiki compiler. `allow_hosts`/`deny_hosts`/`https_only` ceiling on every `http_request` hop (redirects now followed manually; credentials dropped cross-origin — previously reqwest auto-followed redirects past `net_hosts`). `run_command`/shell skills: URL-literal guard + proxy env; `shell_network = "isolated"` wraps `sh` in macOS `sandbox-exec` (only the proxy port). Claude Code `editor_shell` → `editor` under a proxy; `route_llm_api` refuses `claude_code`. JSONL audit per hop / network-looking shell command. `build_tool_executor` lost its `shared_http_client` arg (all callers passed `None`). `OpenRouterEngine::new` returns `Result`. `tengu doctor [--sandbox] [--tor]`. Docker: `deploy/tor/compose.yml` = **Arti 2.6.0** (SOCKS + HTTP CONNECT on 9050, internal networks only) **→ Snowflake** (`deploy/snowflake/`, lyrebird 0.8.1 pinned by tag+commit, `socat`-exposed unmanaged PT at 10.213.47.2:9150; `[bridges] enabled = true`, official Tor Browser 15.0.23 lines). `make tor-native` (adds `tor-port` → 127.0.0.1:9050) / `make up-tor` (`docker-compose.tor.yml` includes the stack; tengu on internal `tor-front`) / `make tor-bridges` (signed-bundle refresh). Arti deliberately not embedded (see egress doc). Upkeep: bump Arti + lyrebird pins and bridge lines with Tor Browser releases. Verified against real Tor — `docs/egress-2026-09-16.md`. Open: Linux `isolated` shell (use Docker override); `http_request` sends no `User-Agent`. |
| Secrets | `.env.example` values blanked. **The old values are still in git history (`02666555090c48e2f46ad446698815cb413f9560`, `d9037e36c03746870587c802ea210596babfad70`) — rotate third-party API keys of removed integrations (revoked 2026-10-07), then purge history.** |

### Decisions left to the maintainer

| Item | Options |
|---|---|
| `[hub]` config + `HubConfig` | Nothing listens on it (only `tengu status` prints it). Remove the struct, or keep as a placeholder. Container ports now point at the webhook listener. |
| `build_tool_executor` is sync and drives MCP connect via `futures::executor::block_on`; the TUI calls it outside a tokio context | Latent panic if a sandbox sets `mcp_servers` and runs `tengu chat`. Make it `async` (all other callers already are). |
| `OrchestratorChatPort::run_orchestrator_turn` + `memory/injector.rs` + `MemoryProvider::prefetch` | Dead path (only `run_orchestrator_turn_with_system` is live). Delete in a follow-up. |
| Postgres connection per memory call | DDL is now once per process; connections are still per call. Pool if it shows up in latency. |

---

## Previous state (2026-05-14)

Runtime memory has been **replaced**: Qdrant-RAG → **Open Brain** (Postgres +
pgvector) + **Karpathy LLM Wiki** (compiled Markdown). New work is uncommitted
on `main`. Planner routing also moved off Qdrant — it is now file-backed
(`TENGU_PLANNER_REGISTRY.md` + `TENGU_PLAN.md`). All six implementation-doc
phases have landed, including the Phase 6 cleanup that fully removed the
legacy Qdrant `rag/` module, the `qdrant` cargo feature, and the
`tengu registry` / `tengu memory inspect` CLIs. Smaller gaps remain (below).

**Compile-verified 2026-09-12** (all feature combos, scoped tests). Postgres
smoke tests and the end-to-end turn were NOT run in that pass — see the
verification block.

---

## What landed (agentic-memory rework — uncommitted)

| Area | Change |
|---|---|
| Plugin | `src/adapters/outbound/tools/agentic_memory/mod.rs` — Postgres store + `agentic_memory` tool (`capture`/`recall`/`ingest_source`/`promote`/`compile_wiki`/`lint`) + free fns for planner/runner recall |
| Schema | `memory_events`, `memory_sources`, `memory_chunks`, `memory_promotions` (created under an advisory lock by `ensure_schema`); `vector` extension (`pg_trgm` dropped 2026-09-12 — nothing used it); FTS + HNSW indexes |
| Planner | `orchestrator/planner.rs` — `RagPlanner` de-Qdrant'd: registry loaded from `TENGU_PLANNER_REGISTRY.md`; recall lanes (cross-session / within-session / cross-plan) read Postgres `agentic_memory` under `postgres_memory` |
| Shared files | `orchestrator/shared_files.rs` — generates `TENGU_PLANNER_REGISTRY.md`, writes/reads `TENGU_PLAN.md` (plan state → subagent prompt) |
| Runner | `main.rs` — subagent summaries captured to Postgres (`try_persist_agentic_step_summary`) on both the `compress_and_store` path and the no-call backstop; subagent prompt loads `TENGU_PLAN.md` |
| Wiring | `bootstrap/` — `agentic_memory` registered in `register_catalog`, added to `WORKSPACE_TOOLS` + `compute_base_tools`/`advertised_defs`; `build_orchestrator` no longer gated on `qdrant` |
| Config / build | `config.rs` `valid_workspace_tools` += `agentic_memory`; `Cargo.toml` `tokio-postgres` + `postgres_memory` feature; `docker-compose.yml` `postgres-memory` profile (pgvector/pg16) |
| Docs | `docs/agentic-memory-{prd,implementation,examples}-2026-05-13.md`; architecture / comparison / context-management docs rewritten for the new model |

## What landed (this session — 2026-05-14, finish-code + Phases 4/5/6 + doc reconciliation)

| Area | Change |
|---|---|
| `.gitignore` | Ignore `TENGU_PLANNER_REGISTRY.md`, `TENGU_PLAN.md`, `/.codex/`, `**/.tengu/agentic-memory/raw/` (the compiled `wiki/` stays tracked — human-reviewable per PRD) |
| `agentic_memory/mod.rs` (chunks + recall) | `ingest_source` now embeds chunks (`memory_chunks.embedding` populated, fail-soft text-only fallback); agent-facing `recall` is now hybrid (pgvector → FTS) instead of FTS-only; tool schema gained `session_id`/`agent`/`role`/`reason`/`evidence` properties (the dispatch code already read them); new `env_embedder` / `embed_query` helpers; `insert_chunks` / `PostgresMemoryStore::recall` signatures gained params (one call site + one ignored smoke test updated) |
| `agentic_memory/mod.rs` (Phase 4 wiki compiler) | `compile_wiki` rewritten: runs an LLM over promoted memories → cited Markdown page (`[mem:<kind>/<id>]` inline + a deterministic `## Sources` footer). New module-scope helpers `compile_wiki_prompt` / `render_wiki_page_llm` / `wiki_compiler_model` / `chat_complete` (a minimal direct-OpenRouter chat call, the chat-side sibling of `Embedder`). Fail-soft: no `OPENROUTER_API_KEY` or an API error falls back to the old deterministic bullet dump. Model via `TENGU_WIKI_COMPILER_MODEL` env (default `anthropic/claude-sonnet-4-6`). |
| `metrics.rs` | New `MetricsKind::WikiCompiler` variant (+ `as_str` arm) so the wiki-compiler LLM call emits a `MetricsRecord` like every other LLM/embedding call. |
| `mcp_bridge.rs` + `main.rs` (Phase 5 MCP server) | New `tengu agentic-memory-server` subcommand — a standalone MCP stdio server exposing **only** `agentic_memory` to non-Tengu agents. Refactor: extracted `serve_mcp_stdio` (shared by `run_mcp_bridge` + the new `run_agentic_memory_mcp_server`); `handle_initialize` gained a `server_name` param (`tengu-tools` vs `tengu-agentic-memory`). CLI: new `Commands::AgenticMemoryServer` variant + early-return (stderr-only tracing, JSON-clean stdout) + feature-gated match arms, mirroring `McpBridge` / `RunAgent`. Operator doc: `docs/mcp-bridge.md`. |
| Phase 6 — full Qdrant removal (~10 files) | `adapters/inbound/webhooks.rs` persist repointed from `rag::RagStore` → `agentic_memory::write_step_summary_with_embedding` (the landmine: `webhooks` had an undeclared `qdrant` dep, so `--features webhooks` alone never compiled). `compress_and_store.rs` gutted to just `definition()` (Qdrant plugin/handler/`write_summary` gone). `src/adapters/rag/` orphaned — `pub mod rag` removed from `adapters/mod.rs`. `tengu registry` + `tengu memory inspect` CLIs deleted (`Commands` variants, `RegistryAction`/`MemoryAction` enums, all four command fns, dispatch arms, `try_persist_step_summary` + its call sites). `memory/vector.rs` qdrant `VectorStore` impl orphaned; `bootstrap/` `build_vector_stack_async` is disk-only and `resolve_qdrant_collection` removed. `Cargo.toml`: `qdrant` feature + `qdrant-client` dep gone. `docker-compose.yml`: qdrant service/profile/volume gone. `.env.example` / `config.example.toml`: qdrant blocks replaced with Postgres-memory equivalents. |
| `CLAUDE.md` | Was stale (pre-rework: "RAG = brain", Qdrant, auto-reindex). Rewritten to match `AGENTS.md` substance + new required-reading entry for the agentic-memory docs + corrected stuck-recipe |
| `AGENTS.md` | Fixed find-replace corruption — a blanket `Claude`→`Codex` had mangled code identifiers (`engine = "Codex"`, `Codex-sonnet-4-6`, `--features Codex`). Restored to `claude_code` / `claude-sonnet-4-6` / `--features claude_code`. `CLAUDE.md` + `AGENTS.md` are now twins |
| `docs/SESSION_HANDOFF.md` | Restored (this file) |

---

## Open items

### Phase 4–6 (per `docs/agentic-memory-implementation-2026-05-13.md`)

| Item | State |
|---|---|
| Phase 4 — wiki compiler | **Landed** (2026-05-14). `compile_wiki` LLM-synthesises a cited Markdown page from promoted memories; deterministic fallback when no LLM is available. Not yet exercised end-to-end against a real LLM — see verification block. |
| Phase 5 — MCP surface | **Landed** (2026-05-14). `tengu agentic-memory-server` is a standalone MCP stdio server exposing only `agentic_memory`; non-Tengu agents (ChatGPT/Codex/Claude) wire it into their MCP client config. Not yet exercised against a real external client — see verification block. |
| Phase 6 — migration cleanup | **Landed** (2026-05-14). Full Qdrant removal: `rag/` module orphaned, `qdrant` feature + `qdrant-client` dep gone, `compress_and_store` Qdrant plugin gone, `tengu registry` / `tengu memory inspect` CLIs gone, qdrant `VectorStore` orphaned, webhook persist migrated to `agentic_memory`. **Not compiled** — see verification block. Orphaned files (`src/adapters/rag/*.rs`, `src/application/memory/vector/qdrant.rs`) are still physically present — `git rm` them (the sandbox couldn't unlink). |

### Smaller gaps

| Item | Note |
|---|---|
| `lint` is shallow | Returns counts only — no duplicate / contradiction / stale-page / missing-citation detection (PRD R6). |
| `propose_behavior` | In the implementation-doc Tool API table; not in the operation enum. Deliberately deferred per the PRD "Correction" (MVP = capture/recall/ingest/promote/compile/lint). |
| `[agentic_memory]` config section | Implementation doc "Config Sketch" (enabled / raw_root / wiki_root / recall knobs) is **not** implemented. Plugin uses `TENGU_MEMORY_DATABASE_URL` env + hardcoded `RAW_ROOT`/`WIKI_ROOT` consts. |
| Graph tables | `memory_claims` / `memory_links` are "planned" in the doc; `ensure_schema` does not create them. Promotion → claim/link flow not built. |
| `capture` session scoping | **Closed 2026-09-12** — `capture` falls back to `TENGU_SESSION_ID` / `TENGU_AGENT_NAME` exported by the `run-agent` child. |
| Embedding dim coupling | Schema hardcodes `vector(1536)`; embedder pinned to `DEFAULT_EMBEDDING_MODEL` (`text-embedding-3-small`). Since 2026-09-12 a non-1536 vector warns and degrades to text-only on every path (event insert, hybrid recall, chunks). |
| Chunk embedding throughput | `insert_chunks` embeds chunks sequentially (one API call each). Batching is a follow-up for large sources. |
| Postgres inspect CLI | `tengu memory inspect` was removed with the Qdrant path. No Postgres-native "did the write land?" diagnostic yet — query the DB directly or run the `postgres_*_smoke` tests. |
| Orphaned files | **Closed 2026-09-12** — `git rm`'d. |
| Schema file | `.tengu/agentic-memory/AGENTS.md` (data-layers table) is not created or read by anything yet. |

---

## Phase 6 — completed (compile-verify still required)

Full Qdrant removal landed across ~10 files (see the "what landed" table). Key
notes for whoever runs the compiler next:

- **`rag/` is cleanly isolated** — it was already fully behind
  `#[cfg(feature = "qdrant")]`, so nothing outside it referenced `RagStore`
  except `webhook_builder`, `compress_and_store`, and `main.rs`'s CLI — all
  handled. A post-removal grep for `crate::adapters::rag` / `QdrantVectorStore`
  / `qdrant_client` in compiled code is clean.
- **The landmine, fixed** — `webhook_builder::persist_webhook_output` used
  `rag::RagStore` directly while gated only on `webhooks`, so
  `cargo build --features webhooks` never actually compiled. It now writes via
  `agentic_memory` under `#[cfg(feature = "postgres_memory")]`, with a
  graceful no-op (warn) when that feature is off.
- **Vestigial `MemoryConfig` qdrant fields and the orphaned `rag/` +
  `vector/qdrant.rs` files** — both removed 2026-09-12 (compiler-verified).

---

## Verify before declaring done (run on a machine with cargo + Docker)

```sh
# 1. Build — every feature combo must compile (Phase 6 removed the `qdrant`
#    feature entirely; `webhooks` was the latent-break combo).
cargo build                                        # default (openrouter, telegram)
cargo build --features postgres_memory             # new memory path
cargo build --features webhooks,postgres_memory    # the Phase 6 landmine combo
cargo build --features claude_code,postgres_memory # mixed-engine

# 2. Unit tests (no DB needed)
cargo test --features postgres_memory agentic_memory

# 3. Postgres + pgvector smoke (DB needed)
docker compose --profile postgres-memory up -d postgres-memory
export TENGU_MEMORY_DATABASE_URL=postgres://tengu:tengu@localhost:5432/tengu_memory
cargo test --features postgres_memory postgres_capture_and_recall_smoke -- --ignored
cargo test --features postgres_memory postgres_vector_recall_smoke      -- --ignored

# 4. End-to-end — trace one turn, confirm recall block appears
#    sandbox config needs [memory] within_session_output_top_k = 3 (or similar)
RUST_LOG=tengu=info cargo run --release --features postgres_memory -- chat --sandbox lping

# 5. Wiki compiler (Phase 4) — via an agent opted into tools = ["agentic_memory"]:
#    capture + promote a memory, then run compile_wiki, then inspect the page.
#    agentic_memory(operation="capture", kind="preference", content="...")
#    agentic_memory(operation="promote", target_kind="event", target_id="<id>")
#    agentic_memory(operation="compile_wiki", title="...")
#    -> expect .tengu/agentic-memory/wiki/<slug>.md with inline [mem:...] cites
#       + a ## Sources footer. Tool result reports mode=llm (or mode=fallback
#       if OPENROUTER_API_KEY is unset — that path must still write a page).

# 6. Standalone MCP server (Phase 5) — handshake + tools/list over stdio
printf '%s\n%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
  | TENGU_MEMORY_DATABASE_URL=postgres://tengu:tengu@localhost:5432/tengu_memory \
    cargo run --features postgres_memory -- agentic-memory-server
#    -> id=1: result.serverInfo.name == "tengu-agentic-memory"
#    -> id=2: result.tools[0].name == "agentic_memory"
#    (server exits cleanly when stdin closes; build a release binary for real use)
```

Watch for: `agentic_memory: ...` warn lines (embed/connection/LLM fail-soft),
`persisted user message to agentic_memory`, `agentic_memory: backstop wrote
final_text summary to Postgres`, and one `metrics` line with
`kind=wiki_compiler` per `compile_wiki` call. After ingesting a source,
confirm `memory_chunks.embedding` is non-null for at least one row.

---

## Active gotchas

The compiled gotcha list lives in `CLAUDE.md` / `AGENTS.md` ("Key gotchas").
Rework-specific call-outs:

- **`TENGU_PLANNER_REGISTRY.md` / `TENGU_PLAN.md` are generated** — regenerated
  every planner turn / accepted plan. Now gitignored. Don't hand-edit; don't
  commit.
- **`postgres_memory` is off by default** — without it, the planner recall
  lanes compile to `String::new()` and `agentic_memory` is not registered.
  The harness still runs (file registry + in-memory history); it just has no
  durable cross-session memory.
- **`agentic_memory` module is fully feature-gated** — everything in
  `src/adapters/outbound/tools/agentic_memory/` only compiles under `postgres_memory`.
- **`tengu agentic-memory-server` reuses the `mcp-bridge` machinery** — it is
  NOT a Claude-specific protocol; `mcp_bridge.rs` implements standard MCP
  (JSON-RPC 2.0 stdio), it was just originally built for the `claude_code`
  engine. `run_mcp_bridge` and `run_agentic_memory_mcp_server` share
  `serve_mcp_stdio`; the bridge path is byte-identical post-refactor.
- **MCP clients replace the env, not extend it** — when an external client
  spawns `tengu agentic-memory-server`, only the keys in its `env` block are
  visible. Forward `OPENROUTER_API_KEY` alongside `TENGU_MEMORY_DATABASE_URL`
  or `recall`/`ingest_source`/`compile_wiki` silently run in their degraded
  fail-soft modes. Same trap `adapters/outbound/engines/claude_code.rs` documents for the bridge.
