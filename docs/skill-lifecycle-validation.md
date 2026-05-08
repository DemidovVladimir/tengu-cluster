# Skill Lifecycle — Validation Guide

A practical, top-to-bottom walkthrough for validating skill distillation, evaluation, metrics, evolution, and retention.

Each section lists:
- **Prereqs** — what must be in place.
- **Commands** — verbatim shell commands.
- **Expected output** — what a successful run looks like.
- **Failure modes** — common problems + fixes.

Estimated token cost per live run is called out. Set your own ceilings before enabling.

---

## 0. Prerequisites

### 0.1 Binary built

```bash
cd /path/to/tengu-cluster
cargo build --bin tengu
```

Expected: `Finished dev [unoptimized + debuginfo] target(s)`. Any compile error → stop, fix before continuing.

### 0.2 OpenRouter API key

Live eval / evolve paths need `OPENROUTER_API_KEY` reachable by the binary. Two ways:

- **Env var** (simplest for one-off smokes):
  ```bash
  export OPENROUTER_API_KEY=sk-or-...
  ```
- **Secrets vault** (production):
  ```bash
  tengu secret init                    # one-time: creates ~/.tengu/secrets.vault
  tengu secret set OPENROUTER_API_KEY sk-or-...
  export TENGU_MASTER_PASSWORD=...     # skip the interactive prompt
  ```

Confirm: `./target/debug/tengu doctor` — should list your configured agents without errors.

### 0.3 Config with `[skill_lifecycle]`

Your `~/.tengu/config.toml` must include three things:

1. A default agent with `skill_distill` in `workspace_tools` (lets that agent author skills mid-conversation).
2. The `[skill_lifecycle]` block naming the improver + fixture-runner agents.
3. `[agents.skill-improver]` and `[agents.fixture-runner]` entries.

Minimum example:

```toml
runtime_profile = "auto"

[agents.main]
default = true
engine = "openrouter"
model = "anthropic/claude-sonnet-4-6"
workspace_tools = ["skill_distill"]

[agents.main.identity]
name = "Tengu"

[agents.main.limits]
max_tokens_per_flow = 100_000

[skill_lifecycle]
improver_agent       = "skill-improver"
fixture_runner_agent = "fixture-runner"
default_max_evolve_cycles = 3
default_rolling_window    = 10
max_per_run_reports       = 10     # per-skill metrics/runs/<ts>/ retention (0 disables)
worktree_stale_hours      = 24     # sweep leaked scratch worktrees older than this

[agents.skill-improver]
engine = "openrouter"
model  = "anthropic/claude-opus-4-7"
workspace_tools = []

[agents.skill-improver.identity]
name = "Skill Improver"
instructions = """
You are a skill-improver. Given a skill that is under-performing on a specific
metric, you propose a REVISED skill body that should raise that metric's pass
rate without regressing other metrics. Emit ONE JSON object and nothing else:
{"proposal":{"body_markdown":"...","metrics":[...]?,"rationale":"..."}}
Preserve name + description unchanged. Do not remove metrics.
"""

[agents.skill-improver.limits]
max_tokens_per_flow = 50_000

[agents.fixture-runner]
engine = "openrouter"
model  = "anthropic/claude-sonnet-4-6"
workspace_tools = []

[agents.fixture-runner.identity]
name = "Fixture Runner"
instructions = "You execute a single skill fixture and return the final assistant output."

[agents.fixture-runner.limits]
max_tokens_per_flow = 50_000
```

See `config.example.toml` in the repo root for the full commented reference.

Confirm: `./target/debug/tengu doctor` lists three agents (`main`, `skill-improver`, `fixture-runner`).

---

## 1. Unit test suite (no API key, no cost)

```bash
cargo test --bin tengu skill_lifecycle:: -- --nocapture 2>&1 | tail -5
cargo test --bin tengu eval_builder::    -- --nocapture 2>&1 | tail -5
cargo test --bin tengu plugins::         -- --nocapture 2>&1 | tail -5
```

Expected:

```
test result: ok. 48 passed; 0 failed ...      (skill_lifecycle)
test result: ok. 29 passed; 0 failed ...      (eval_builder)
test result: ok. 51 passed; 0 failed ...      (plugins)
```

Total: 128 tests. If any fail, investigate before running live smokes.

Specific test groups worth knowing:

| Module | What it covers |
|--------|----------------|
| `skill_lifecycle::config::tests` | `[skill_lifecycle]` TOML parsing + defaults |
| `skill_lifecycle::metrics::tests` | `validate_metrics` invariants |
| `skill_lifecycle::metric_kinds::shell_check::tests` | shell-check pass/fail/timeout |
| `skill_lifecycle::metric_kinds::llm_judge::tests` | judge JSON parse + prose-wrapped output |
| `skill_lifecycle::metric_kinds::tool_assertion::tests` | `value_matches`, `value_equals`, `value_in` |
| `skill_lifecycle::metric_kinds::script::tests` | shell-script metric JSON parse |
| `skill_lifecycle::storage::tests` | `metrics.json` round-trip, rolling window, retention pruning |
| `skill_lifecycle::fixtures::tests` | YAML round-trip + transcript→fixture extraction |
| `skill_lifecycle::scratch_worktree::tests` | git worktree + non-git fallback |
| `skill_lifecycle::evolve::tests` | `pick_target_metric`, `pick_best`, `apply_proposal_to_skill_md` |
| `skill_lifecycle::approval_gate::tests` | terminal diff + keystroke parsing |
| `plugins::skill_lifecycle::distill::tests` | `skill_distill` tool — atomic write, collision, invalid name |

---

## 2. CLI surface smoke (no API key, no cost)

```bash
./target/debug/tengu --help
```

Expected: `eval`, `skill` (with subcommands: `evolve`, `metrics`, `accept-proposal`, `remove`, `list`, `doctor`, `export`, `install`) all listed.

```bash
./target/debug/tengu eval --help
```

Expected flags:
- `--sandbox <NAME>` — load config from `sandboxes/<NAME>/config.toml`
- `--judge-model <M>` — override the judge model
- `--concurrency <N>` — parallel rows
- `--format table|json`
- `--out <DIR>` — override `evals/runs/<ts>/`
- `--filter <GLOB>` — subset rows by id
- `--keep-workspace` — keep tmp workspace dirs after run
- `--keep-runs <N>` — retain N most recent under `evals/runs/` (default 10)
- `--no-persist` — skip all file writes (rows still run, table still prints)
- `--max-runs <N>` — retain N most recent under `skills/<skill>/metrics/runs/` (default 10)

```bash
./target/debug/tengu skill evolve --help
./target/debug/tengu skill metrics --help
./target/debug/tengu skill accept-proposal --help
```

All should return without error.

---

## 3. `tengu doctor` (no API key needed, no cost)

```bash
./target/debug/tengu doctor
```

Expected output includes:
- Auto-detected runtime profile.
- Backend diagnostics for **each configured agent** — look for `main`, `skill-improver`, `fixture-runner`.

If `skill-improver` or `fixture-runner` is missing → your `~/.tengu/config.toml` is incomplete. Re-check section 0.3.

---

## 4. `tengu skill metrics` (no API key, no cost)

Read-only inspection. Works with or without a prior eval run.

```bash
./target/debug/tengu skill metrics skill-creator
```

**Expected when no prior run exists:**

```
No metrics.json yet for skill 'skill-creator'. Run `tengu eval skill-creator` first.
```

**Expected after an eval run:**

```json
{
  "schema_version": 1,
  "skill": "skill-creator",
  "last_run": "2026-04-21T13-51-02Z",
  "last_run_ref": "metrics/runs/2026-04-21T13-51-02Z",
  "rolling_window": 10,
  "metrics": {
    "distill_quality": {
      "pass_rate": 1.0,
      "n": 2,
      "min_pass_rate": 0.7,
      "gated": false
    }
  }
}

-- history (last 10) --
{"ts":"2026-04-21T13-51-02Z","metric":"distill_quality","pass_rate":1.0,"n":2,"ref":"metrics/runs/2026-04-21T13-51-02Z"}
```

The `gated` field is the key signal:
- `gated: true` → `pass_rate < min_pass_rate` → this metric is a candidate target for `tengu skill evolve`.
- `gated: false` → above threshold → evolve won't touch it unless explicitly `--target-metric`'d.

---

## 5. `tengu eval --no-persist` (cheap, ~$0.05, no files written)

Run the pipeline without touching the filesystem. Useful to verify that an eval flow works before committing to persisted output.

```bash
./target/debug/tengu eval skill-creator --no-persist
```

Expected:
- Logs show 3 OpenRouter calls per row (agent dispatch + row-judge + metric llm_judge).
- Terminal table prints pass/fail per row + aggregate.
- **No new files anywhere** — not `evals/runs/`, not `skills/skill-creator/metrics.json`.

Verify:

```bash
ls evals/runs/ 2>&1 | head
ls skills/skill-creator/metrics.json 2>&1
```

Both should say "No such file or directory".

---

## 6. Full `tengu eval` (live, ~$0.10)

```bash
./target/debug/tengu eval skill-creator
```

Expected behaviour:
1. Logs: `[skill-creator row f1] running…` / `[skill-creator row f2] running…`
2. Three OpenRouter calls per row (agent, row-judge, metric-judge).
3. **`finish_reason=tool_calls`** appears in logs — this means the agent is actually *invoking* `skill_distill`, not just describing it.
4. Terminal table:

   ```
   skill-creator  (2 rows, ~70s)
     ✓ f1    pass    The agent invoked skill_distill with a coherent body…
     ✓ f2    pass    Agent called skill_distill with a clear three-step Procedure…

   2/2 passed (0 failed). Total wall: ~70s. Agent tokens: ~7600. Judge tokens: ~1500.
   ```

5. New files on disk (post-run):

   ```
   evals/runs/<ts>/
       report.json
       skill-creator-f1.md
       skill-creator-f2.md
   skills/skill-creator/
       metrics.json
       metrics/runs/<ts>/
           report.json
       metrics/history.jsonl          (one line per metric per run)
   ```

6. Exit code: `0` if all gated metrics pass, `1` if any gated fails, `2` on runner error.

### Verify the metric dispatch is real

Check one of the transcripts:

```bash
less evals/runs/<most-recent>/skill-creator-f1.md
```

Look for a `# Tools` section that **includes `skill_distill`**. If it doesn't — your config doesn't have `workspace_tools = ["skill_distill"]` on the eval agent, or the `SkillLifecyclePlugin` registration regressed. The eval config is at `skills/skill-creator/evals/config.toml` — check that its `skill-creator-agent` has `workspace_tools = ["skill_distill"]`.

Check that the metric rubric actually ran:

```bash
jq '.fixtures[0].outcomes.distill_quality' skills/skill-creator/metrics/runs/<most-recent>/report.json
```

Expected: a JSON object with `pass`, `score`, `notes` — the judge's substantive verdict.

### Failure modes

| Symptom | Cause | Fix |
|---------|-------|-----|
| "`[skill_lifecycle]` config missing" | No `[skill_lifecycle]` block in config | Add per section 0.3 |
| "agent 'X' is not a valid workspace_tool" | Validator allowlist doesn't accept `skill_distill` | Update `src/adapters/config.rs` — should include `"skill_distill"` in the valid list (commit `4b0abcd` or later) |
| "yaml prompts parse failed" | Fixtures file is wrong schema | `skills/<name>/evals/prompts.yaml` must be a flat list of `{id, prompt, expected, ...}`, not `{schema_version, fixtures: [...]}` |
| Every row fails with "agent only described skill_distill in text" | `SkillLifecyclePlugin` not registered → tool not advertised to LLM | Verify commit `f16d2ef` or later is in the tree — check `src/adapters/channel_runtime.rs` for `SkillLifecyclePlugin` registration block |
| llm_judge fails with "model does not support assistant prefill" | Old prefill-based judge prompt | Verify commit `6d88028` or later — `LlmJudgeKind::run` should build the prompt with `""` prefill |

---

## 7. Full `tengu skill evolve` (live, ~$0.30 per cycle; `--max-cycles 1` recommended for first smoke)

Requires a **gated** metric — `distill_quality` will be gated if you haven't yet achieved `pass_rate >= 0.7` rolling. The easiest way to prove this end-to-end: run eval once (step 6) to seed `metrics.json`, then evolve.

### 7.1 Discard path (safe, non-mutating)

```bash
echo "n" | ./target/debug/tengu skill evolve skill-creator --max-cycles 1
```

Expected behaviour:
1. **Startup sweep** — logs mention removing any stale worktrees from `.tengu/worktrees/` older than 24h.
2. **Baseline eval** — one full `tengu eval` pass internally.
3. **Target metric selection** — logs show `distill_quality` picked as target (assuming it's gated).
4. **Scratch worktree created** at `.tengu/worktrees/evolve-skill-creator-<ts>/` (visible in process state; cleaned on exit).
5. **Improver dispatch** — one call to `claude-opus-4-7`, returning a JSON proposal with `body_markdown`, `rationale`, optionally `metrics`.
6. **Cycle rescore** — full eval inside the scratch worktree.
7. **Best-cycle selection** — if no regression > 0.05 on non-target metrics, the cycle is a valid candidate.
8. **Approval gate** renders:

   ```
   Skill: skill-creator
   Target metric: distill_quality (baseline 0.50 → proposed 1.00, delta +0.50)

   Non-target gated metrics (must stay >= baseline - 0.05):
     (none)

   SKILL.md changes (unified diff):
     @@ ... @@
     +- **New section: Metrics**
     -...
     +...

   Rationale:
     Strengthens distillation quality by …

   [y] apply, [n] discard, [d] show details, [o] open worktree:
   ```

9. Your piped `n` → `"No changes applied. Baseline preserved."`
10. Scratch worktree removed. `skills/skill-creator/SKILL.md` untouched.
11. One line appended to `skills/skill-creator/metrics/evolve_log.md`:

    ```
    - 2026-04-21T20-15-30Z | target=distill_quality | baseline=0.50 → best=1.00 | verdict=rejected | rationale=...
    ```

### 7.2 Apply path (mutating — use with care)

```bash
echo "y" | ./target/debug/tengu skill evolve skill-creator --max-cycles 1
```

Difference from 7.1:
- `skills/skill-creator/SKILL.md` gets overwritten with the proposed body.
- A sanity re-eval runs on the real path to confirm the reported delta.
- `evolve_log.md` records `verdict=accepted`.

**To revert** after smoke-testing:

```bash
git checkout skills/skill-creator/SKILL.md
```

### 7.3 All-cycles-regress path

Hard to trigger deliberately — happens when every improver proposal makes some non-target gated metric worse than `baseline - 0.05`. If it does:

Expected:
```
evolve found 3 proposals but all regressed gated metrics. No changes applied.
Worktree preserved for inspection: .tengu/worktrees/evolve-skill-creator-<ts>/
```

Worktree is **not** removed — you can cd in and inspect what the improver produced across cycles. It will be auto-cleaned on the next evolve run (24h stale sweep).

### 7.4 Multi-cycle

```bash
echo "n" | ./target/debug/tengu skill evolve skill-creator --max-cycles 3
```

Expected: logs show `cycle 1`, `cycle 2`, `cycle 3`. On cycle 2+, the improver's user message includes a `Previous attempts in this session:` section listing prior rationales + metric movements — so it doesn't repeat a failed angle.

Early exits: if any cycle hits `target_metric pass_rate = 1.0`, or if all gated metrics pass AND target improved ≥ 0.15, the loop exits early before completing all N cycles.

### Failure modes

| Symptom | Cause | Fix |
|---------|-------|-----|
| "no gated metrics failing; nothing to evolve" | All metrics above threshold | This is a success, not a bug. Lower `min_pass_rate` or break the skill deliberately to smoke evolve. |
| "skill-improver returned malformed JSON" | Model didn't follow the JSON-only instruction | Check `[agents.skill-improver].identity.instructions` — it must tell the model to emit one JSON object only. |
| "git worktree add failed" | Workspace is a git repo but something else is wrong (permissions, disk, corrupt index) | Check `git status` manually in workspace root. The non-git fallback kicks in automatically for non-git repos. |
| Hangs forever at the prompt | Stdin isn't interactive (e.g. run inside a subshell that captured stdin) | Pipe your decision: `echo "n" \| tengu skill evolve …` |

---

## 8. Retention + sweep validation (no API, no cost)

### 8.1 Verify `max_per_run_reports` caps per-skill run dirs

Preconditions: run eval at least twice to have ≥2 run dirs.

```bash
# Count how many per-run dirs exist
ls skills/skill-creator/metrics/runs/ | wc -l

# Run eval with a tight cap
./target/debug/tengu eval skill-creator --max-runs 3

# After the run, you should have ≤3 (not more)
ls skills/skill-creator/metrics/runs/ | wc -l
```

Expected: `≤ 3`. Older timestamps are deleted.

`history.jsonl` is **not** pruned — it's the permanent rolling history, cheap (~200B/line):

```bash
wc -l skills/skill-creator/metrics/history.jsonl
```

Grows monotonically. Bounded at *read-time* by `rolling_window` in config.

### 8.2 Verify `keep_runs` caps `evals/runs/`

Pre-existing feature (also runnable via retention cap):

```bash
./target/debug/tengu eval skill-creator --keep-runs 3
ls evals/runs/ | wc -l     # ≤ 3
```

### 8.3 Verify worktree stale sweep

Manually plant a stale worktree dir to exercise the sweep:

```bash
mkdir -p .tengu/worktrees/evolve-fake-old
touch -d '30 hours ago' .tengu/worktrees/evolve-fake-old

./target/debug/tengu skill evolve skill-creator --max-cycles 1 --dry-run 2>&1 | head
# (then Ctrl-C after startup to avoid a real eval run, OR pipe 'n')

# The stale dir should be gone:
ls .tengu/worktrees/ 2>&1
```

Expected: `evolve-fake-old` removed at startup. Log line: `sweep_stale_worktrees: removed stale worktree`.

To disable the sweep entirely (for debugging leaked worktrees):

```toml
[skill_lifecycle]
worktree_stale_hours = 0
```

---

## 9. Distillation — from a live conversation

The `skill_distill` tool is advertised to any agent that lists it in `workspace_tools`. Exercising it requires a conversation, not the eval runner.

### 9.1 Interactive TUI

```bash
./target/debug/tengu chat
```

In the chat:
1. Do some workflow — e.g. "fetch JSON from https://api.example.com/users and save the array length to persistent_store under users_count".
2. After success, say: "Let's save this as a skill called fetch-user-count."
3. The agent should call `skill_distill` — you'll see a tool activity indicator.
4. Check:

   ```bash
   ls skills/fetch-user-count/
   ```

   Expected: `SKILL.md`, `evals/prompts.yaml`, `metrics/` (with any rubric scaffolds).

   **Invariant:** the new skill does NOT load into the current conversation — it becomes available on the next session. Cache discipline requires a stable tool inventory per conversation.

### 9.2 Verify what got written

```bash
cat skills/fetch-user-count/SKILL.md
cat skills/fetch-user-count/evals/prompts.yaml
```

Expected:
- Frontmatter with `name`, `description`, `metrics:` block.
- Body with Overview / When to Use / Procedure / Common Mistakes.
- `prompts.yaml` seeded with the conversation slice (args schema-redacted — long strings replaced with `"<elided>"`).

### 9.3 Known limitation

`PluginToolExecutor` threads the live message slice into `ConversationView` via the `ToolExecutor::execute` trait (commit `e243468`). If you see `from_message_index` errors or an empty fixtures.yaml, the engine dispatch path may not be passing `messages` correctly — file a bug.

---

## 10. Quick reference: the full pipeline in one session

```bash
# Build
cargo build --bin tengu

# Prereqs
export OPENROUTER_API_KEY=sk-or-...

# Unit tests (no cost)
cargo test --bin tengu skill_lifecycle:: --quiet
cargo test --bin tengu eval_builder:: --quiet
cargo test --bin tengu plugins:: --quiet

# Config sanity
./target/debug/tengu doctor

# Dry-run eval (no files written, ~$0.05)
./target/debug/tengu eval skill-creator --no-persist

# Full eval (~$0.10)
./target/debug/tengu eval skill-creator

# Inspect
./target/debug/tengu skill metrics skill-creator

# Evolve discard path (~$0.30)
echo "n" | ./target/debug/tengu skill evolve skill-creator --max-cycles 1

# Evolve apply path (~$0.30 + whatever sanity re-eval costs)
echo "y" | ./target/debug/tengu skill evolve skill-creator --max-cycles 1
# Revert if needed:
git checkout skills/skill-creator/SKILL.md

# Retention verify
./target/debug/tengu eval skill-creator --max-runs 3
ls skills/skill-creator/metrics/runs/ | wc -l   # should be ≤ 3
```

Total cost for a full validation pass: **~$1.00**, ~5 minutes wall time.

---

## Related

- Design spec: `docs/superpowers/specs/2026-04-20-skill-metrics-evolution-design.md` (§15 Implementation addendum reconciles spec vs. shipped).
- Plan (phase 1): `docs/superpowers/plans/2026-04-20-skill-metrics-evolution.md` (Tasks 1-11, landed on main).
- Plan (phase 2): `docs/superpowers/plans/2026-04-21-skill-metrics-evolution-phase2.md` (Tasks α/β/γ/δ consolidation).
- `docs/skills.md` — skill authoring + Metrics & Evolution section.
- `docs/configuration.md` — `[skill_lifecycle]` config reference.
- `docs/architecture.md` — Skill Lifecycle key-abstraction entry.
