# Validation Checklist

End-to-end acceptance check for the work landed across PRs #6 → #13, updated 2026-09-18 for the current shape: planner = `RagPlanner` (file-backed `TENGU_PLANNER_REGISTRY.md`), worker = `SubprocessRunner` (one `tengu run-agent` child per step), single sandbox config (`sandboxes/<name>/config.toml`), Tor-by-default egress. Work top-to-bottom. Each section states **what to do**, **what to expect**, and **what a failure looks like**.

Runs in ~30 minutes of your time. Automated parts cost ~$0.50–$1.00 in OpenRouter API fees.

---

## 0. Prerequisites

```bash
cd /Users/vladimirdemidov/development/tengu-cluster
git log --oneline -5

export OPENROUTER_API_KEY=sk-or-...
export RUST_LOG=tengu=info,tengu::adapters::orchestrator=debug

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
ls src/application/memory/vector/
```

**Expect:** `builtin.rs`, `context_block.rs`, `fencing.rs`, `injector.rs`, `manager.rs`, `mod.rs`, `provider.rs`, `vector.rs`, `writer.rs` + `vector/{disk.rs,embedder.rs}`. No `qdrant.rs` (removed Phase 6; Postgres `agentic_memory` lives in `src/adapters/outbound/tools/agentic_memory/`).

### 1.2 Orchestrator subsystem files exist

```bash
ls src/application/orchestrator/
```

**Expect:** `events.rs`, `executor.rs`, `mod.rs`, `plan.rs`, `planner.rs`, `replan.rs`, `retry.rs`, `shared_files.rs`, `wiring.rs`. No `config.rs`, `roster.rs`, `telemetry.rs` (removed Phase 7.1). The worker lives outside the module: `src/adapters/outbound/subprocess_runner.rs` (`SubprocessRunner`).

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

| Filter | Expected (2026-09-18) |
|---|---|
| `application::memory` | 21 passed, 0 failed |
| `application::orchestrator` | 30 passed, 0 failed |
| `adapters::outbound::tools::memory` | 17 passed, 0 failed |
| `config` | 20 passed, 0 failed |
| `scope_lint` | 2 passed, 0 failed |
| `run_agent_ipc` | 4 passed, 0 failed |

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

**Expected event sequence** (TUI `orch:` system bubbles — `OrchestratorEvent` rendered in `tui/mod.rs:241`; `RUST_LOG=tengu=info` adds one `metrics` line per LLM call, children included):
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

If any of these matter for your use case, add a scenario in `docs/orchestration-test-scenarios.md` and a matching row in `prompts.yaml`.
