# Validation Checklist

End-to-end acceptance check for the work landed across PRs #6 → #13, updated 2026-09-18 for the current shape: planner = `RagPlanner` (file-backed `TENGU_PLANNER_REGISTRY.md`), worker = `SubprocessRunner` (one `tengu run-agent` child per step), single sandbox config (`sandboxes/<name>/config.toml`), Tor-by-default egress. Work top-to-bottom. Each section states **what to do**, **what to expect**, and **what a failure looks like**.

Runs in ~30 minutes of your time. Automated parts cost ~$0.50–$1.00 in OpenRouter API fees.

**2026-10-08:** § 11 adds the lineage registry, strategy ranking and SOE offline checks; § 9–10 agent lists follow the current sandboxes. **2026-10-02:** § 9 (xmarket paper desk) and § 10 (xlab history-first research) cover the `feature/xmarket` work — operator-runnable; `tengu` commands checked against `tengu <cmd> --help`, test filters against the test sources; § 10's numbers were re-run on a copy of `market.db` on 2026-10-02.

---

## 0. Prerequisites

```bash
cd /Users/vladimirdemidov/development/tengu-cluster
git log --oneline -5

export OPENROUTER_API_KEY=sk-or-...
export RUST_LOG=tengu=info,tengu::application::orchestrator=debug

# Egress is Tor by default: start the proxy, or put `[egress] network = "open"` in the config
make tor                              # Arti + lyrebird-rs on 127.0.0.1:9050 (needs ../lyrebird-rs)

cargo build --release --all-features 2>&1 | tail -3
```

Pass: `Finished 'release' profile` with only warnings. No errors.

If fail: something in the current tree doesn't compile — stop, diagnose.

---

## 1. Structural audit (grep, no runtime cost)

### 1.1 Memory subsystem files exist

```bash
ls src/application/memory/
ls src/adapters/outbound/memory/
```

**Expect:** `fencing.rs`, `injector.rs`, `manager.rs`, `mod.rs`, `writer.rs` + `outbound/memory/{builtin.rs,disk_vector.rs,embedder.rs,mod.rs}` (ports in `src/ports/memory.rs`). No `qdrant.rs` (removed Phase 6; Postgres `agentic_memory` lives in `src/adapters/outbound/tools/agentic_memory/`).

### 1.2 Orchestrator subsystem files exist

```bash
ls src/application/orchestrator/
```

**Expect:** `events.rs`, `executor.rs`, `mod.rs`, `planner.rs`, `replan.rs`, `retry.rs`, `shared_files.rs`, `wiring.rs` (the plan types live in `src/domain/plan.rs`). No `config.rs`, `roster.rs`, `telemetry.rs` (removed Phase 7.1). The worker lives outside the module: `src/adapters/outbound/subprocess_runner.rs` (`SubprocessRunner`).

### 1.3 Legacy files are gone

```bash
ls -d src/adapters/agent_builder.rs src/adapters/event_orchestrator.rs \
   src/adapters/task_builder.rs src/adapters/orchestrator.rs \
   src/adapters/memory_builder.rs src/adapters/qdrant_memory_store.rs \
   src/adapters/embedding.rs src/adapters/rag src/adapters/agents \
   src/application/orchestrator/roster.rs src/application/orchestrator/telemetry.rs \
   src/application/orchestrator/config.rs src/application/memory/vector/qdrant.rs \
   agents 2>&1 | rg -v "No such"
```

**Expect:** empty output — every listed file should return "No such file or directory".

### 1.4 Subagents plugin deleted

```bash
ls src/adapters/outbound/tools/subagents 2>&1
```

**Expect:** `No such file or directory`.

### 1.5 Memory plugin shape

```bash
ls src/adapters/outbound/tools/memory/
```

**Expect:** `ingest.rs`, `mod.rs`, `persistent_store.rs`, `search.rs`. No `remember.rs`.

### 1.6 Doctrine scrubbed

```bash
rg -n 'LLM = heart' CLAUDE.md docs/architecture-2026-04-27.md
```

**Expect:** hits. The doctrine is "LLM = heart, Open Brain + Karpathy LLM Wiki = brain, tools = hands" — enforced in `bootstrap::orchestrator::run_turn_with_system` (planner turn strips tools/memory/grounding).

### 1.7 No legacy types leak into production code

```bash
rg -n 'MemoryServiceHandle|EmbeddingPort|MemoryStorePort|DiskVectorMemoryStore|OpenRouterEmbeddingAdapter|QdrantMemoryStore|QdrantVectorStore|OrchestratorAgentPlanner|ChatWorker|AgentSpec' src/ -g '*.rs' | rg -v '//'
```

**Expect:** empty (or only matches inside doc comments that start with `//`). If there's a real import, something's wrong.

---

## 2. Unit tests (narrow filters, ≤30s each)

Per project rule: never run full `cargo test` blindly. Use these filters:

```bash
cargo test --bin tengu application::memory 2>&1 | tail -3
cargo test --bin tengu application::orchestrator 2>&1 | tail -3
cargo test --bin tengu adapters::outbound::tools::memory 2>&1 | tail -3
cargo test --bin tengu config 2>&1 | tail -3
cargo test --test scope_lint 2>&1 | tail -3
cargo test --test run_agent_ipc 2>&1 | tail -3
```

| Filter | Expected (2026-10-02, default features) |
|---|---|
| `application::memory` | 11 passed, 0 failed |
| `application::orchestrator` | 24 passed, 0 failed |
| `adapters::outbound::tools::memory` | 17 passed, 0 failed |
| `config` | 121 passed, 0 failed |
| `scope_lint` | 2 passed, 0 failed |
| `run_agent_ipc` | 7 passed, 0 failed |
| whole bin (`cargo test --bin tengu -- --list`) | 1,539 tests |

**If any fail:** don't continue — something regressed.

---

## 3. Orchestration live e2e (~$0.40, ~5 minutes)

The most important validation. Exercises planner + DAG executor + workers end-to-end against real LLMs.

### 3.1 Run with retention

```bash
./target/release/tengu eval orchestration-e2e --keep-runs 3
```

### 3.2 Expected result

```
orchestration-e2e  (9 rows, ~350s)
  ✓ simple_direct                    pass  ...
  ✓ two_step_sequential              pass  ...
  ✓ parallel_fan_out                 pass  ...
  ✓ failure_recovery                 pass  ...
  ✓ orchestrator_skip_for_trivial    pass  ...
  ✓ sequential_three_step            pass  ...
  ✓ parallel_two_way_with_synthesizer pass  ...
  ✓ parallel_four_way_stress         pass  ...
  ✓ diamond_dag                      pass  ...

9/9 passed (0 failed). Total wall: ~300-400s. Agent tokens: ~500k-800k.
```

### 3.3 Pass criteria

- **9/9 pass.** (8/9 is acceptable for first run — diamond_dag is the hardest; 7/9 or lower means regression.)
- Wall time 3–7 minutes.
- Token count in the 500k–900k range.

### 3.4 Fail modes + diagnosis

| Row fails with rationale | Likely cause |
|---|---|
| "Orchestrator errored out ..." | The planner LLM call itself failed (non-JSON output no longer errors — Phase 7.4 wraps it as `kind=direct` with a warn log). Planner prompt = `skills/orchestrator/SKILL.md` (`RagPlanner` loads it; `identity.instructions` is bypassed for the planning turn). |
| "Agent made direct HTTP calls" | Orchestrator didn't fire at all (eval path bypassed). Verify `[orchestrator]` block parses in the config (`tengu eval --sandbox <name>` overrides the skill-local `evals/config.toml`). |
| `step sN: agent "X" has no [agents.X] block` | Planner invented a name (`skills/orchestrator/SKILL.md` forbids this), or the worker `[agents.X]` lacks a `description` — only those reach `TENGU_PLANNER_REGISTRY.md` (`shared_files::routable_agents`); `SubprocessRunner::run_step` fails fast without spawning. |
| "step inputs not threaded" | `executor.rs` `render_step_inputs` bug — check the `<step-input from="...">` block format. |
| Row times out | Increase `timeout_secs` in `prompts.yaml` for that row; a single step is capped by the agent's `limits.step_timeout_secs` (default 600). |

Transcripts land in `evals/runs/<ts>/orchestration-e2e-<row_id>.md` for diagnosis.

---

## 4. Retention + `--no-persist` (~1 minute, free — uses cached eval if re-run, or a single cheap row)

### 4.1 Verify retention

Run the eval 12+ times quickly (or just observe over time after real use):

```bash
# Baseline: 0 dirs
ls evals/runs/ 2>&1 | wc -l

# Run eval with keep_runs=3 — the current run + up to 3 retained
./target/release/tengu eval orchestration-e2e --keep-runs 3

# Count should be ≤3
ls evals/runs/ 2>&1 | wc -l
```

**Pass:** ≤3 directories. The retention prune fires at startup.

**Fail:** count grows unbounded → `prune_old_run_dirs` isn't firing.

### 4.2 Verify `--no-persist`

```bash
# Note the current count
BEFORE=$(ls evals/runs/ 2>&1 | wc -l)

# Run with --no-persist
./target/release/tengu eval orchestration-e2e --filter simple_direct --no-persist

# Count should be identical
AFTER=$(ls evals/runs/ 2>&1 | wc -l)
echo "before=$BEFORE after=$AFTER"
```

**Pass:** `before == after`. No new directory created. Terminal still prints the row result + verdict.

**Fail:** directory count incremented → `--no-persist` isn't skipping the writes.

### 4.3 Verify `.gitignore` coverage

```bash
git check-ignore -v evals/runs/ 2>&1
git check-ignore -v skills/skill-creator/metrics/runs/ 2>&1
```

**Pass:** both print the matching `.gitignore` line.

**Fail:** git status includes `evals/runs/` or `metrics/runs/` → `.gitignore` rules missing.

---

## 5. TUI smoke — channel wiring (manual, ~10 minutes, ~$0.20)

The eval path has its own `EvalChatServiceFactory`. This tests the real TUI path through `RuntimeChatServiceFactory` + `snapshots_inputs_fn`.

### 5.1 Setup

One sandbox file holds everything (`docs/configuration.md`):

```bash
mkdir -p sandboxes/smoke && cp config.example.toml sandboxes/smoke/config.toml
# edit sandboxes/smoke/config.toml:
#   - uncomment [orchestrator] (agent = the planner agent, engine = "rag")
#   - one [agents.<name>] per worker (researcher, writer) WITH a `description` — only those are routable
#   - [memory] enabled = true
#   - `make tor` is running, or add [egress] network = "open"
./target/release/tengu doctor --sandbox smoke     # config parses, engines build, network/proxy status
```

### 5.2 Start the TUI

```bash
./target/release/tengu chat --sandbox smoke
```

**Expected log lines (`RUST_LOG=tengu=info`, from a separate terminal):**
- `orchestrator: engine=rag, planner=RagPlanner(file-registry), worker=SubprocessRunner`
- `TUI orchestrator constructed with per-turn snapshot factory`
- `DiskVectorStore loaded` with `entries=0` on first boot (`[memory] enabled = true`)

**If missing:** orchestrator didn't construct. Check `[orchestrator]` block (`agent` names an `[agents.*]` block, `engine = "rag"`) + default agent present.

### 5.3 Sanity prompts

Type these one at a time in the TUI and observe:

| Prompt | Expected |
|---|---|
| `hi` | Short greeting. Planner returns `kind=direct` → no `orch: plan created` bubble (only `orch: plan completed`). Reply in <3s. |
| `What is 2 + 2?` | Reply contains `4`. Planner fires but returns `kind=direct` — no `orch: plan created`, no `tengu run-agent` child spawned. |

### 5.4 Sequential smoke

```
Look up the HTTP status codes for 200 and 404, then write a two-sentence summary contrasting them.
```

**Expected event sequence** (TUI `orch:` system bubbles — `OrchestratorEvent` rendered in `tui/mod.rs:308`; `RUST_LOG=tengu=info` adds one `metrics` line per LLM call, children included):
1. `orch: plan created (2 steps)` — s2 depends on s1
2. `orch: ▶ s1 [researcher]` — a `tengu run-agent` child spawns
3. `orch: ✓ s1`
4. `orch: ▶ s2 [writer]`
5. `orch: ✓ s2`
6. `orch: plan completed`

Reply: polished paragraph referencing 200 and 404.

**Fail markers:** s2 starts before s1 succeeds. Reply doesn't reference the codes. Log shows only direct dispatch with no orchestrator events.

### 5.5 Parallel smoke

```
In parallel, give a one-line definition of tRPC and a one-line definition of GraphQL. Then write one paragraph contrasting them.
```

**Expected:** three steps — `s1:researcher` + `s2:researcher` both with `depends_on=[]`, `s3:writer depends_on=[s1,s2]`. `orch: ▶ s1` and `orch: ▶ s2` fire within ~100ms of each other (two `tengu run-agent` children in parallel).

**Pass:** both codes fire in parallel (observable in log timestamps), synthesizer pulls from both.

### 5.6 Memory survival (if you want to spend 2 more minutes)

```
Remember for future reference: my favorite HTTP status code is 418.
```
Reply should acknowledge + either call `memory_ingest` or write-through.

Exit TUI (Ctrl+C). Restart. Ask:

```
What's my favorite HTTP status code?
```

**Pass:** reply contains `418`.

**Fail:** reply says "I don't have that information" → `DiskVectorStore` isn't persisting (check `<workspace>/memory/vectors.bin` exists and has non-zero size between sessions), or nothing searched it: in orchestrated mode the planner turn skips disk-memory injection (`ChatOrchestratorPortImpl::run_orchestrator_turn_with_system`), so recall needs a worker step calling `memory_search`, or Postgres `agentic_memory` (`--features postgres_memory`, `[memory] within_session_output_top_k`).

---

## 6. Telegram smoke (optional, ~5 minutes)

Only if you have a Telegram bot token and want to verify that channel too. Skip if not.

Add to `sandboxes/smoke/config.toml` (the token is an env var, not a config key):
```toml
[telegram]
enabled = true
allowed_users = ["<your Telegram user id>"]   # or TENGU_TELEGRAM_ALLOWED_USERS — without either `tengu telegram` refuses to start
```

```bash
export TELEGRAM_BOT_TOKEN=...
./target/release/tengu telegram --sandbox smoke
```

Same log expectations as §5.2 but with `Telegram orchestrator constructed with per-message snapshot factory`.

Send the sequential prompt from §5.4 via Telegram. Verify the `/stop` command actually interrupts a mid-run plan: send a long multi-step prompt, wait for the first step-started status message, then send `/stop`. Expect `Stopped by user.` reply within 5 seconds.

---

## 7. Reporting

For each validation section, record:

| Section | Status | Notes |
|---|---|---|
| 1. Structural audit | pass / fail | |
| 2. Unit tests | pass / fail | test counts |
| 3. Orchestration eval | N/9 passed | which failed, rationale |
| 4. Retention | pass / fail | |
| 5. TUI smoke | pass / partial / fail | which sub-step failed |
| 6. Telegram (optional) | pass / skip | |
| 9. xmarket paper desk | pass / fail | `doctor --live` exit code, feeds live, ledger owner |
| 10. xlab research | pass / fail | research-arm n · mean vs the expected rows, gate cache hits |
| 11. Lineage · ranking · SOE | pass / fail | failing test names |

If anything fails:
- §1 or §2 fail → code regression. Don't proceed.
- §3 fails < 9/9 → prompt calibration or LLM-level issue. Paste failing row transcript.
- §4 fails → retention bug. Easy fix.
- §5 fails → channel wiring bug in `adapters/inbound/telegram.rs` or `tui/mod.rs`. Harder, needs a targeted PR.

---

## 8. What this validation does NOT cover

Known gaps (documented in `docs/harness-architecture.md` §9):

- **Concurrent multi-user sessions** — only one orchestrator session per test.
- **Postgres `agentic_memory`** — needs `--features postgres_memory` + `TENGU_MEMORY_DATABASE_URL`; not exercised here (ignored `postgres_*_smoke` tests cover it).
- **Tor egress** — sections above run with `make tor` or `[egress] network = "open"`; `tengu doctor --tor` is the only live exit check.
- **Eval stubs vs subprocess workers** — row `stubs` reach the planner turn (`EvalChatServiceFactory`) only; worker steps are real `tengu run-agent` children.
- **5+ step plans** — `prompts.yaml` tops out at 4 steps (diamond).
- **Mid-step LLM streaming cancellation** — `/stop` between steps is tested; mid-LLM-call cancel is not.
- **Live `local` engine legs** — run only on the operator's PC (`TENGU_MATRIX_LOCAL_BASE_URL`); never start a local model on the dev Mac.
- **Real money** — no live send anywhere (Solana `send`, Privy signing, M3b Hyperliquid orders): paper / simulate only, by operator rule.

If any of these matter for your use case, add a scenario in `docs/orchestration-test-scenarios.md` and a matching row in `prompts.yaml`.

---

## 9. xmarket — paper desk (`sandboxes/xmarket`, ~15 min, ≈ free)

Runbook: top of `sandboxes/xmarket/config.toml`. Docs: `docs/runtime-2026-09-30.md` § xmarket sandbox, `docs/xmarket-risk-paper-2026-09-30.md`. Run from the repo root (`--sandbox` resolves `sandboxes/<name>/` from the cwd; the repo `.env` sets `TENGU_HOME=~/.tengu`). At a terminal each command asks for the vault password on `/dev/tty`: Enter skips it (stdin `/dev/null` does not).

```bash
CARGO_TARGET_DIR=$HOME/.cache/tengu-xm.noindex/main CARGO_BUILD_JOBS=2 nice -n 10 cargo build --release   # default features
T=$HOME/.cache/tengu-xm.noindex/main/release/tengu
```

### 9.1 Offline (no network, scoped runs)

```bash
cargo test --bin tengu config::xmarket::tests::xmarket_sandbox_m0_stage
cargo test --bin tengu config::xmarket::tests::weekend_sandbox_replays_the_golden
cargo test --bin tengu config::risk::tests::every_sandbox_and_the_example_load
TENGU_CONFORMANCE_ONLY=paper_ cargo test --test bridge_conformance bridge_matches_in_process
TENGU_CONFORMANCE_ONLY=xm_ cargo test --test bridge_conformance bridge_matches_in_process
cargo test --test engine_matrix offline_local_xm
```

**Pass:** each `ok`. `TENGU_CONFORMANCE_ONLY` keeps the cases whose name contains the value (`paper_order*`, `paper_close`, `paper_positions`; `xm_exits`, `xm_weekend_fade`); each runs in-process and through a real `tengu mcp-bridge` and must agree.

### 9.2 Config + run + live health

```bash
"$T" doctor --sandbox xmarket </dev/null                 # exit 0: agents xm_architect, xm_executor (no planner); network open, allow api.hyperliquid.xyz
tmux new -s xm                                            # then, in the pane (foreground — never with &: the vault prompt stops a background job)
nice -n 10 "$T" run --sandbox xmarket
# second terminal:
"$T" run --sandbox xmarket </dev/null                    # exit 1: lease runtime:xmarket held
"$T" doctor --sandbox xmarket --live </dev/null          # exit 0 = heartbeat fresh + every required feed live
```

**Expect** (smoke 2026-10-01): `doctor --live` exit 0 at +2 min — 4 / 4 required feeds live (`hl_ctx`, `hl_book`, `xm_exits`, `risk_day`); 0 WARN / ERROR in `~/.tengu/logs/tengu.log`; HL weight ≤ 126 / min. Ctrl-C in the pane drains in ≤ 20 s, exit 0; `doctor --live` then exits 1 (`stopped`).

**Fail:** `doctor --live` exit 1 with `FAIL heartbeat missing: no run-xmarket.json` while the run is up → the doctor reads another `TENGU_HOME` than the run (run both from the repo root); `FAIL feed …` → the line carries the feed's state and `last_error`.

### 9.3 Ledger + kill switch

```bash
"$T" risk status --sandbox xmarket </dev/null            # read-only: account xmarket $100, owner sandbox xmarket, halts, positions, last verdicts
"$T" tool call --sandbox xmarket --agent xm_executor --tool risk_status </dev/null   # risk_state/1:xmarket — equity at mark, loss headroom
touch ~/.tengu/state/xmarket/KILL
"$T" risk status --sandbox xmarket </dev/null            # "kill-switch file …/KILL: PRESENT — every account is halted"
rm ~/.tengu/state/xmarket/KILL
"$T" risk resume --sandbox xmarket                       # at a terminal, if a halt is recorded: type the account name
```

**Expect:** before the first `tengu run`: `no ledger yet: <path>` (status never creates the ledger). `risk resume` is refused while `KILL` exists.

### 9.4 Engine matrix — xm set (live, costs tokens)

```bash
cargo test --features claude_code --test engine_matrix _xm -- --ignored --nocapture --test-threads 1
```

**Expect:** one `engine_matrix |` line per leg; gemini · haiku · claude_code green (W1 gate: 39 / 39 over 13 sets); `local_xm` skipped without `TENGU_MATRIX_LOCAL_BASE_URL` (operator's PC). Needs `OPENROUTER_API_KEY` (env or `.env`) and `claude` logged in.

### 9.5 Weekend sandbox (optional run)

Checks for `sandboxes/xmarket-weekend` use the FROZEN binary only (`~/.cache/tengu-xm.noindex/weekend/tengu-6fcb455`, sha256 `e2c3bb8f88f25d4a6a9275200b7b248ac8a052db16e344fb674da4e208320c72`) — a newer build must not touch `~/.tengu/state/xmarket-weekend/` before Mon 2026-10-05 10:00 New York. Commands + timeline: runbook at the top of `sandboxes/xmarket-weekend/config.toml`.

---

## 10. xlab — history-first research (`sandboxes/xlab`, ~5 min, free without a live gate)

Runbook: top of `sandboxes/xlab/config.toml`. Doc: `docs/xlab-2026-10-01.md` § 10 (commands), § 14 (results). Same `$T` as § 9; no LLM unless noted.

### 10.1 Offline (no network, scoped runs)

```bash
cargo test --bin tengu domain::backtest::checks           # 5 tests: time integrity (§ 39) + the rule W golden
cargo test --bin tengu config::backtest                   # 6 tests: [backtest] load rules, the strategy library checked at load
TENGU_CONFORMANCE_ONLY=market_history cargo test --test bridge_conformance bridge_matches_in_process
TENGU_CONFORMANCE_ONLY=backtest cargo test --test bridge_conformance bridge_matches_in_process
cargo test --test engine_matrix offline_local_xlab        # offline_local_xlab + _xlab_holdout + _xlab_rank
```

### 10.2 Config + data

```bash
"$T" doctor --sandbox xlab </dev/null                    # exit 0: xl_architect, xl_jev; network open, allow api.hyperliquid.xyz + api.geckoterminal.com
"$T" history coverage --sandbox xlab </dev/null          # instrument | kind | interval | first | last | rows | sources
```

**Expect** (data through 2026-10-01): 162 rows, 79 instruments (75 `hyperliquid:xyz:*` + `hyperliquid:BTC` / `ETH` / `SOL` / `HYPE`); 1h bars from `2026-03-07T08:00:00Z` (later for names listed later), funding from `2026-03-01T00:00:00Z`; `ctx` rows (`hl-archive:asset_ctxs`) for the four crypto perps, 2026-09-01 → 2026-09-30.

Extend (resumes; ≈ 1 h for the 75 names at xlab's HL budget — one HL-heavy xlab process at a time):

```bash
"$T" history backfill --sandbox xlab --instruments @crypto,@xyz_stocks --interval 1h --from 2026-03-01 --funding </dev/null
```

### 10.3 Rules backtest with a holdout split (≈ 1 s)

```bash
"$T" backtest --sandbox xlab --strategy weekend_fade --split time:2026-07-01T00:00:00Z </dev/null
"$T" backtest --sandbox xlab --strategy weekend_fade_liquid --split time:2026-07-01T00:00:00Z </dev/null
```

**Expect** on the 2026-10-01 data (re-run 2026-10-02; a later backfill adds weekends and moves these):

| Strategy | Research n · mean net bps (95 % CI) | In-sample n · mean | Holdout n · mean (CI) | Capped ($100 book) |
|---|---|---|---|---|
| `weekend_fade` | 1500 · +50.45 (+15.7 … +81.5) | 614 · +57.67 | 886 · +45.44 (−7.3 … +93.1) | 116 · +147.57 (+53.0 … +247.6); 1384 `max_gross_exposure_usd` refusals |
| `weekend_fade_liquid` | 875 · +58.26 (+15.8 … +98.9) | 374 · +39.55 | 501 · +72.22 (+6.3 … +130.9) | 116 · +119.64; `thin_entry` skips 625 |

Each run prints its run dir `~/.tengu/state/xlab/backtests/<run id>/` (`report.json`, `report.md`, `trades-<arm>.jsonl`, `candidates.jsonl`, `skips.json`). **Fail:** `market.db` missing → § 10.2 backfill (or add `--fetch`).

### 10.4 Jev gate — offline rerun (no key, no spend)

```bash
"$T" backtest --sandbox xlab --strategy weekend_fade --split time:2026-07-01T00:00:00Z --gate xl_gate --max-decisions 1500 --offline </dev/null
```

**Expect:** `decided 1500 · take 277 · skip 1223 · … · error 0`, then `cache 1500 hits · 0 misses · est $0.0000 · jev − rules +28.93 bps ci95=[-23.5,+81.6] · Brier 0.349`. `--max-decisions` defaults to 500 (the rest counted as `cut`). A cache miss under `--offline` is class `error`, never a call; drop `--offline` for a live first run (≈ $0.00004 per decision, `OPENROUTER_API_KEY`).

### 10.5 Engine matrix — xlab sets (live, costs tokens)

```bash
cargo test --features claude_code --test engine_matrix xlab -- --ignored --nocapture --test-threads 1
```

**Expect:** 12 legs (`openrouter_gemini_*`, `openrouter_haiku_*`, `claude_code_*`, `local_*` × `xlab`, `xlab_holdout`, `xlab_rank`). 2026-10-01 (before `xlab_rank`): haiku-4.5 and the Claude CLI pass both sets; gemini-2.5-flash-lite calls every tool but may misquote numbers — re-run; `local_*` skipped without `TENGU_MATRIX_LOCAL_BASE_URL`.

### 10.6 Architect turn (live, optional; `xl_architect` = `claude-opus-5-5` through the Claude CLI, the operator's subscription)

`"$T" chat --sandbox xlab` → "test a weekend follow placebo on the holdout split". **Expect:** `backtest` with a split shows the in-sample half only; one `"holdout": true` read adds a line to `~/.tengu/state/xlab/backtests/holdout-reads.jsonl`; the verdict is judged on the holdout line.

---

## 11. Lineage · strategy ranking · SOE (offline, no LLM, no network)

Docs: `docs/lineage-2026-10-06.md`, `docs/strategy-ranking-automation-2026-10-08.md`, `docs/soe-2026-10-08.md`, `docs/source-evidence-2026-10-08.md`. Each test runs the built binary with `TENGU_HOME` in a temp dir.

```bash
cargo test --test lineage_cli          # fixture registry: verify, seal (variant + ranking contract), report, trace; the repo lineage/: W1 frozen, every pin recomputes
cargo test --test strategy_ranking     # tengu ranking on a sealed test contract: publish, rerun no-op, resume, INCOMPLETE keeps latest, unsealed refused
cargo test --test soe_cli              # tengu soe: eval on the 16 fixture cases, check / portfolio / sensitivity, profile refusals
cargo test --test engine_matrix offline_local_sources   # source_evidence through run-agent on the local mock
"$T" lineage verify --pins </dev/null  # exit 0 on the repo registry
```

**Pass:** every test `ok`, `lineage verify --pins` exit 0.
