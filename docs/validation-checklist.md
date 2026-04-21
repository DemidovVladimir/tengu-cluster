# Validation Checklist

End-to-end acceptance check for the work landed across PRs #6 → #12, plus the retention + TUI-memory-config follow-up. Work top-to-bottom. Each section states **what to do**, **what to expect**, and **what a failure looks like**.

Runs in ~30 minutes of your time. Automated parts cost ~$0.50–$1.00 in OpenRouter API fees.

---

## 0. Prerequisites

```bash
# Current main at time of writing: f14bfc6 (#12).
# PR #13 adds retention + --no-persist + TUI memory config.
cd /Users/vladimirdemidov/development/tengu-cluster
git log --oneline -5

export OPENROUTER_API_KEY=sk-or-...
export RUST_LOG=tengu_cluster=info,tengu_cluster::adapters::orchestrator=debug

cargo build --release --all-features 2>&1 | tail -3
```

Pass: `Finished 'release' profile` with only warnings. No errors.

If fail: something in the current tree doesn't compile — stop, diagnose.

---

## 1. Structural audit (grep, no runtime cost)

### 1.1 Memory subsystem files exist

```bash
ls src/adapters/memory/
ls src/adapters/memory/vector/
```

**Expect:** `builtin.rs`, `context_block.rs`, `fencing.rs`, `injector.rs`, `manager.rs`, `mod.rs`, `provider.rs`, `vector.rs`, `writer.rs` + `vector/{disk.rs,qdrant.rs,embedder.rs}`.

### 1.2 Orchestrator subsystem files exist

```bash
ls src/adapters/orchestrator/
```

**Expect:** `config.rs`, `events.rs`, `executor.rs`, `mod.rs`, `plan.rs`, `planner.rs`, `replan.rs`, `retry.rs`, `roster.rs`, `telemetry.rs`, `wiring.rs`.

### 1.3 Legacy files are gone

```bash
ls src/adapters/agent_builder.rs src/adapters/event_orchestrator.rs \
   src/adapters/task_builder.rs src/adapters/orchestrator.rs \
   src/adapters/memory_builder.rs src/adapters/qdrant_memory_store.rs \
   src/adapters/embedding.rs 2>&1 | grep -v "No such"
```

**Expect:** empty output — every listed file should return "No such file or directory".

### 1.4 Subagents plugin deleted

```bash
ls src/adapters/plugins/subagents 2>&1
```

**Expect:** `No such file or directory`.

### 1.5 Memory plugin shape

```bash
ls src/adapters/plugins/memory/
```

**Expect:** `ingest.rs`, `mod.rs`, `persistent_store.rs`, `search.rs`. No `remember.rs`.

### 1.6 Doctrine scrubbed

```bash
grep -r 'heart.*brain\|brain.*heart' docs/architecture.md src/ 2>&1
```

**Expect:** empty. The old "heart / brain / sensors" doctrine is gone.

### 1.7 No legacy types leak into production code

```bash
grep -rn 'MemoryServiceHandle\|EmbeddingPort\|MemoryStorePort\|DiskVectorMemoryStore\|OpenRouterEmbeddingAdapter\|QdrantMemoryStore' src/ --include='*.rs' | grep -v '//' | grep -v '^Binary'
```

**Expect:** empty (or only matches inside doc comments that start with `//`). If there's a real import, something's wrong.

---

## 2. Unit tests (narrow filters, ≤30s each)

Per project rule: never run full `cargo test` blindly. Use these filters:

```bash
cargo test --bin tengu adapters::memory 2>&1 | tail -3
cargo test --bin tengu adapters::orchestrator 2>&1 | tail -3
cargo test --bin tengu adapters::plugins::memory 2>&1 | tail -3
cargo test --bin tengu adapters::config 2>&1 | tail -3
cargo test --test scope_lint 2>&1 | tail -3
```

| Filter | Expected |
|---|---|
| `adapters::memory` | 21 passed, 0 failed |
| `adapters::orchestrator` | 31 passed, 0 failed |
| `adapters::plugins::memory` | ~17 passed, 0 failed |
| `adapters::config` | 12 passed, 0 failed |
| `scope_lint` | 2 passed, 0 failed |

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
| "Orchestrator errored out, expected value at line 1 column 1" | Planner prompt produced non-JSON. Check `skills/orchestration-e2e/evals/config.toml` orchestrator prompt. |
| "Agent made direct HTTP calls" | Orchestrator didn't fire at all (eval path bypassed). Verify `[orchestrator]` block parses in the config. |
| "unknown agent: X" | Planner invented an agent name. Prompt should forbid this — check §rules in the orchestrator prompt. |
| "step inputs not threaded" | `executor.rs` `render_step_inputs` bug — check the `<step-input from="...">` block format. |
| Row times out | Increase `timeout_secs` in `prompts.yaml` for that row. |

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

```bash
mkdir -p ~/tengu-tui-smoke && cd ~/tengu-tui-smoke
cp /Users/vladimirdemidov/development/tengu-cluster/docs/configs/tui-memory-smoke.toml ./tengu.toml
sed -i '' "s|\$WORKSPACE|$PWD|g" tengu.toml
```

### 5.2 Start the TUI

```bash
/Users/vladimirdemidov/development/tengu-cluster/target/release/tengu chat
```

**Expected log lines (grep from a separate terminal):**
- `TUI orchestrator constructed with per-turn snapshot factory`
- `DiskVectorStore loaded` with `entries=0` on first boot

**If missing:** orchestrator didn't construct. Check `[orchestrator]` block + default agent present.

### 5.3 Sanity prompts

Type these one at a time in the TUI and observe:

| Prompt | Expected |
|---|---|
| `hi` | Short greeting. No orchestrator event lines in logs. Reply in <3s. |
| `What is 2 + 2?` | Reply contains `4`. Planner fires (`orchestrator:plan_created` in log) but returns `kind=direct`. |

### 5.4 Sequential smoke

```
Look up the HTTP status codes for 200 and 404, then write a two-sentence summary contrasting them.
```

**Expected log sequence:**
1. `orchestrator:plan_created` — two steps, s2 depends on s1
2. `orchestrator:step_started s1:researcher`
3. `orchestrator:step_succeeded s1`
4. `orchestrator:step_started s2:writer`
5. `orchestrator:step_succeeded s2`
6. `orchestrator:plan_completed`

Reply: polished paragraph referencing 200 and 404.

**Fail markers:** s2 starts before s1 succeeds. Reply doesn't reference the codes. Log shows only direct dispatch with no orchestrator events.

### 5.5 Parallel smoke

```
In parallel, give a one-line definition of tRPC and a one-line definition of GraphQL. Then write one paragraph contrasting them.
```

**Expected:** three steps — `s1:researcher` + `s2:researcher` both with `depends_on=[]`, `s3:writer depends_on=[s1,s2]`. `step_started s1` and `step_started s2` fire within ~100ms of each other.

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

**Fail:** reply says "I don't have that information" → `DiskVectorStore` isn't persisting, or `MemoryInjector::for_turn` isn't running. Check `<workspace>/memory/vectors.bin` exists and has non-zero size between sessions.

---

## 6. Telegram smoke (optional, ~5 minutes)

Only if you have a Telegram bot token and want to verify that channel too. Skip if not.

Add to `tengu.toml`:
```toml
[telegram]
enabled = true
token = "YOUR_BOT_TOKEN"
```

Run `tengu telegram` instead of `tengu chat`. Same log expectations as §5.2 but with `Telegram orchestrator constructed with per-message snapshot factory`.

Send the sequential prompt from §5.4 via Telegram. Verify the `/stop` command actually interrupts a mid-run plan: send a long multi-step prompt, wait for the first `orchestrator:step_started`, then send `/stop`. Expect `Stopped by user.` reply within 5 seconds.

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
- §5 fails → channel wiring bug in `telegram_builder.rs` or `tui/mod.rs`. Harder, needs a targeted PR.

---

## 8. What this validation does NOT cover

Known gaps (documented in `docs/harness-architecture.md` §9):

- **Concurrent multi-user sessions** — only one orchestrator session per test.
- **Qdrant backend** — all tests use disk. Qdrant port exists but not validated against a live server.
- **5+ step plans** — `prompts.yaml` tops out at 4 steps (diamond).
- **Mid-step LLM streaming cancellation** — `/stop` between steps is tested; mid-LLM-call cancel is not.

If any of these matter for your use case, add a scenario in `docs/orchestration-test-scenarios.md` and a matching row in `prompts.yaml`.
