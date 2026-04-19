# Skill Eval Runner (LLM-judge)

**Status:** Design, pending approval
**Date:** 2026-04-19
**Depends on:** Phase A (tool plugin architecture, merged); Phase B (orchestration skill, PR #5 open). Runner consumes the existing tool-call stream exposed by `engine_builder.rs`.
**Blocks:** none — this is additive tooling.
**Related:** `skills/skill-eval/SKILL.md` (manual health-check skill; explicitly defers LLM-judge mode to future work at `SKILL.md:83-89`). This spec is that future work.

---

## 1. Problem

`skills/orchestration/evals/prompts.md` defines five prompts and their expected behaviours — but there is no way to run them. Today a human must paste each prompt into `tengu orchestrate`, eyeball the tool-call stream, and mentally compare against the "expected behaviour" column. That is:

- **Not repeatable.** Every contributor eyeballs differently; regressions slip in silently.
- **Not diffable.** No artefact survives between runs, so "what changed in row 2 between last Friday and today" is unanswerable.
- **Not enforceable.** CI cannot gate on skill behaviour; the only automated signal is the skill compiling into a valid prompt, which the existing `skill-eval` skill already catches at the frontmatter/drift level.

The `skill-eval` skill is deliberate about this gap: its Limitations section lists **LLM-judge mode**, **scheduled runs**, and **automatic remediation** as deferred until "there is real usage data to calibrate against" (`skill-eval/SKILL.md:83-89`). We now have that usage data — one skill (orchestration) with evals, one more (aura) actively being proposed — and a live need to test the skill-that-teaches-decomposition before any sandbox bets on it.

## 2. Goal

**Make `tengu eval <skill>` the one command that runs every prompt in that skill's `evals/`, scores each row pass/fail with an LLM judge, and emits a machine-readable report plus per-row transcripts.** Build this generically once, so any skill that drops `evals/prompts.md` + `evals/config.toml` becomes evaluable with zero additional runner work.

Concretely, v1 ships:

- `tengu eval` CLI subcommand, in-process, sequential-by-default, optional `--concurrency N`.
- Generic skill-agnostic discovery across the three-tier skill hierarchy.
- Two prompts formats: the existing markdown table (default, fully-live execution) and an opt-in YAML format with per-row tool stubs (hybrid execution — live by default, stubbed where declared).
- A fixed-by-default Opus 4.7 judge via OpenRouter, with `--judge-model` override and prompt caching for the judge system prompt.
- A stable `report.json` (schema_version 1) alongside per-row markdown transcripts.

**Rust LOC delta: ~+800 new (one new `src/adapters/eval_builder.rs` + one new CLI subcommand arm + one new spec in `config.toml` — nothing moved or deleted).** New skill asset: `skills/orchestration/evals/config.toml` (~12 lines) as the canonical example.

## 3. Non-goals

- **Claude Code engine support.** v1 supports `engine = "openrouter"` only. The tool-call tap lives inside the streaming tool loop in `engine_builder.rs`, which we own. Claude Code routes tool calls through the external CLI subprocess, which needs a different tap (through `mcp_bridge.rs`) and is a bigger build. v1 errors out with a clear message if a skill's `evals/config.toml` declares `engine = "claude_code"`.
- **Severity / rubric scoring.** Binary pass/fail + one-line rationale. LLM judges drift under numeric rubrics; the transcript artefact is where nuance lives.
- **GitHub Actions annotation format.** Plain table + JSON report in v1. CI integration is follow-up work.
- **Automatic remediation.** Report-only. No auto-editing of skills based on eval failures.
- **Cross-run regression dashboards.** Out of scope; `report.json` is the data source if a dashboard is later built.
- **Cost caps / spend guardrails.** Every row logs tokens in the report; operators set `max_tool_rounds` in `evals/config.toml` if runs get expensive.
- **Evaluating skills that need external services (Molecule Labs, Privy, Beach.science).** Supported in principle via YAML stubs, but out of v1 scope to actually wire up.

## 4. Design

### 4.1 Architecture & module layout

A new single module, `src/adapters/eval_builder.rs`, consistent with the repo's builder pattern (one file per subsystem). Wiring points:

- **CLI:** one new `Commands::Eval { … }` arm in `src/main.rs`, delegating to `eval_builder::run(args, config_root)`.
- **Engine:** reuses `engine_builder::build_engine(…)` for the agent-under-test and a second call for the judge (different model, no tools). No changes to `engine_builder.rs` beyond exposing a hook for the tool-call tap (existing stream events already carry this — likely zero change).
- **Tool executor:** wraps the existing `PluginToolExecutor` (`src/adapters/tool_plugin.rs`) in a new `StubbedExecutor` defined in `eval_builder.rs`. The stubbed executor consults a per-row `stubs` map before delegating.
- **Skill discovery:** reuses the three-tier walk that `skill-eval/SKILL.md` documents (`~/.tengu/skills/*/`, `.tengu/skills/*/`, `skills/*/`). A skill is "evaluable" iff it contains `evals/prompts.md` *or* `evals/prompts.yaml`, and either `evals/config.toml` exists or `--sandbox NAME` was passed.
- **No new ports, no new traits.** The runner is a consumer of existing infrastructure, not an extension of it.

### 4.2 CLI surface

```
tengu eval                              # discover + run every skill that has evals/
tengu eval <skill>...                   # one or more skills by name
tengu eval <skill> --sandbox NAME       # override: use sandboxes/NAME/config.toml
tengu eval <skill> --judge-model MODEL  # default: anthropic/claude-opus-4-7
tengu eval <skill> --concurrency N      # default: 1 (sequential)
tengu eval <skill> --format TABLE|JSON  # default: TABLE (JSON report always written to disk)
tengu eval <skill> --out DIR            # default: evals/runs/<ISO8601-ts>/
tengu eval <skill> --filter GLOB        # glob over row ids; default: all rows
tengu eval <skill> --keep-workspace     # keep per-row tmp workspaces after run (debugging)
```

**Exit codes:** 0 = all rows pass; 1 = at least one row failed; 2 = runner-level error (skill not found, config invalid, judge unreachable, engine not supported).

**TUI vs non-TUI:** not an interactive command — always prints a final summary table, never streams intermediate agent output to stdout (that would interleave with tool-call logging and make JSON format unparseable). Live progress is communicated via one line per row `[orchestration row 3/5] sequential-research-mint: running…` printed to stderr. Under `NO_COLOR=1` or non-TTY stdout, no ANSI codes.

### 4.3 Per-skill eval config (the `evals/config.toml` file)

Every evaluable skill ships `evals/config.toml`. This is a mostly-standard Tengu config, expanded into a fresh tmp workspace per run. One templated field, `{TMP_WORKSPACE}`, is substituted by the runner; everything else is verbatim TOML.

Canonical example — `skills/orchestration/evals/config.toml`:

```toml
runtime_profile = "cloud"

[memory]
enabled = true          # row 5 asserts on remember() calls

[orchestrator]
enabled = true          # registers sessions_spawn / sessions_fan_out
max_concurrent = 3

[agents.main]
engine = "openrouter"
model = "anthropic/claude-sonnet-4-6"
default = true
workspace = "{TMP_WORKSPACE}"
skill_packages = ["orchestration"]

[agents.main.limits]
max_tool_rounds = 10            # cap subagent loops under live mode
stream_event_timeout_secs = 60
```

Workspace handling:

- Fresh `$TMPDIR/tengu-eval-<skill>-<ts>/row-<id>/` per row.
- Torn down on row completion unless `--keep-workspace` is passed.
- `{TMP_WORKSPACE}` is the only expansion performed; if the skill config needs any other placeholder, that is a future-version add, not a v1 feature.

`--sandbox NAME` bypasses `evals/config.toml` entirely and loads `sandboxes/NAME/config.toml` as-is (no `{TMP_WORKSPACE}` expansion — the sandbox declares its own workspace).

### 4.4 Prompts format — two tiers

**Default: markdown** (`evals/prompts.md`). The format that `skills/orchestration/evals/prompts.md` already uses:

```markdown
# Orchestration skill evals

| Prompt | Expected behaviour |
|---|---|
| "research paper X then mint it as an IP token" | Sequential `sessions_spawn(researcher)` then `sessions_spawn(minter)`. |
...
```

Parsing rules:

- Exactly one markdown table with header row `| Prompt | Expected behaviour |` (case-insensitive match on the header cells).
- Row id is derived by kebab-casing the prompt (ASCII alphanumerics lowercased, everything else → `-`, consecutive dashes collapsed, leading/trailing dashes trimmed) and capped at 64 chars (`research paper x then mint it as an ip token` → `research-paper-x-then-mint-it-as-an-ip-token`, 44 chars, fits). Collisions are an error — rename the prompt or switch to YAML.
- Quoted prompts: leading/trailing double quotes are stripped.
- Backtick-wrapped identifiers in the expected column are retained verbatim (they carry meaning — `sessions_fan_out` is a tool name, not prose).
- No stubs, no per-row timeout overrides. Markdown rows always run fully live.

**Opt-in: YAML** (`evals/prompts.yaml`). When both files exist, YAML wins. Schema:

```yaml
- id: sequential-research-mint                 # required, kebab-case, unique within file
  prompt: "research paper X then mint it as an IP token"
  expected: "Sequential sessions_spawn(researcher) then sessions_spawn(minter)."
  timeout_secs: 120                            # optional, default 120
  stubs:                                       # optional
    - tool: http_request                       # tool name exact-match
      responses:                               # consumed in order; last entry repeats if exhausted
        - { status: 503, body: "Service Unavailable" }
        - { status: 200, body: '{"ok": true}' }
```

Only five fields are recognised: `id`, `prompt`, `expected`, `timeout_secs`, `stubs`. Unknown keys fail loudly (typo-prevention). Any extension to the schema bumps `schema_version` in the report.

### 4.5 Hybrid execution

The tool executor passed to the agent is `StubbedExecutor`, a thin wrapper around the existing `PluginToolExecutor`. Per-call flow:

1. If the current row is a YAML row with a matching `stubs[].tool` entry, pop the next response from its `responses` queue (last entry repeats if the queue is exhausted), shape it into the tool's expected `ToolResult` type, return.
2. Otherwise delegate to the wrapped `PluginToolExecutor`. Call executes for real.

Markdown rows have an empty stubs map, so every call delegates — fully live. YAML rows without `stubs:` behave the same. Only rows that declare `stubs:` see any divergence from live execution.

Stub scope is per-row; the map is rebuilt from the row spec at the start of each row's execution and discarded at the end. No cross-row leakage.

**Subagent taming under live mode.** Spawned subagents inherit the agent-under-test's `max_tool_rounds` (capped at 10 by the default `evals/config.toml`). They also inherit the row's `timeout_secs` — on timeout, the whole tree is killed. The judge scores the *parent agent's* decomposition pattern, not the subagent's internal work, so a subagent that times out internally still produces a usable observation log for the parent.

### 4.6 Per-row execution lifecycle

Sequential within a skill; opt-in parallel across rows via `--concurrency N`. Never parallel within a single row.

1. **Setup.** Allocate `$TMPDIR/tengu-eval-<skill>-<ts>/row-<id>/`. Load config TOML, expand `{TMP_WORKSPACE}`, build `Engine` + `PluginToolExecutor`. Wrap executor in `StubbedExecutor` with this row's stubs (empty for markdown rows and unstubbed YAML rows).
2. **Observation tap.** Subscribe to the engine's existing tool-call stream events. For each tool call: append `{seq, name, args_preview: truncate(args_json, 2048)}` to the row's `Vec<Observation>`. For each tool result: append a peer entry (result is *not* sent to the judge but *is* included in the transcript).
3. **Drive.** Send `Message::user(prompt)` into the engine under a `timeout_secs` hard cap. Engine runs to termination (final assistant text with no more tool calls) or timeout.
4. **Capture transcript.** System prompt summary (first 500 chars), user prompt, every assistant turn (text + tool calls + tool results), final assistant text, wall duration, timed_out flag.
5. **Judge.** Serialise the observation log (call pattern only, no results) + expected string + final assistant text into a single judge user turn. Invoke the judge engine. Parse the JSON verdict. On parse failure: verdict = `fail`, rationale = `"judge emitted malformed output: <first 200 chars>"`.
6. **Write artefact.** `<out-dir>/<skill>-<row-id>.md` with four sections: config summary, user prompt, full message log, judge input + verdict. Written regardless of verdict so passing rows are diffable too.
7. **Record row entry.** Append to the in-memory report; terminal line prints on row completion with verdict + rationale + transcript path on fail.
8. **Teardown.** Delete the tmp workspace unless `--keep-workspace`. Move to next row.

### 4.7 The judge

**Model.** Default `anthropic/claude-opus-4-7` via OpenRouter (already a dependency — no new SDK). Overridable via `--judge-model`. Guard against self-judging: a warning is printed to stderr if the judge model matches the agent-under-test's model, but the run proceeds.

**Prompt caching.** The judge system prompt is identical across every row, every skill, and every run. Mark it as a cache breakpoint on the first turn of every call. Expected hit rate ~100% after the first row of the first run.

**Judge system prompt (paraphrased here — authoritative version lives in `eval_builder.rs`):**
> You are evaluating whether an AI agent's tool-call sequence matches an expected behaviour. You will be shown the expected behaviour in natural language, the observed tool calls in order (tool name + truncated args), and the agent's final text reply. Decide whether the agent's behaviour matched the expected behaviour. Respond with strict JSON: `{"verdict": "pass" | "fail", "rationale": "<one sentence>"}`. Do not include any other output.

**Judge user turn (per row):**

```
Expected behaviour: <expected string from prompts file>

Observed tool calls (in order):
1. <tool_name>(<args_preview>)
2. <tool_name>(<args_preview>)
...
(each call truncated to 2 KB; tool results omitted — judge scores the CALL pattern.)

Final assistant text:
<truncated to 1 KB>

Did the agent's behaviour match the expected? Reply with JSON only.
```

**Prefill.** Assistant turn is prefilled with `{"verdict":` to force valid-JSON start. If the parser still fails, the row is marked `fail` with a diagnostic rationale — a malformed judge output is a judge-side regression worth catching.

**No severity, no rubric.** Binary pass/fail + one-line rationale only. The transcript artefact carries the rest.

### 4.8 Terminal output

Printed per-skill after that skill's rows all finish (no mid-skill interleaving under `--concurrency` — buffered and flushed atomically):

```
orchestration  (5 rows, 47s)
  ✓ sequential-research-mint      pass   sequential sessions_spawn observed (researcher→minter)
  ✓ parallel-price-fetch          pass   single sessions_fan_out with 3 children
  ✓ trivial-arithmetic            pass   direct text reply, no subagent tools
  ✗ fail-503-retry                fail   called sessions_spawn(retry) instead of retrying in-place
      → see evals/runs/2026-04-19T16-22-04Z/orchestration-fail-503-retry.md
  ✓ multi-step-deploy-fix         pass   4 sequential tool calls + 2 remember() writes with progress

4/5 passed (1 failed). Total wall: 47s. Total agent tokens: 38,412. Total judge tokens: 4,180.
```

Across multiple skills: one block per skill, blank-line separated, followed by a grand-total summary line. `--format json` suppresses the table and prints the JSON report to stdout instead.

### 4.9 Report schema (`report.json`)

Always written to `<out-dir>/report.json`, regardless of `--format`. Stable schema at `schema_version: 1`; additions are backward-compatible, removals bump the version.

```json
{
  "schema_version": 1,
  "started_at": "2026-04-19T16:22:04Z",
  "finished_at": "2026-04-19T16:22:51Z",
  "runner_version": "tengu 0.1.0 (commit ff6ad8e)",
  "judge_model": "anthropic/claude-opus-4-7",
  "concurrency": 1,
  "skills": [
    {
      "skill": "orchestration",
      "tier": "project",
      "config_source": "skill-local",
      "engine": "openrouter",
      "agent_model": "anthropic/claude-sonnet-4-6",
      "prompts_format": "markdown",
      "wall_ms": 47183,
      "rows": [
        {
          "id": "sequential-research-mint",
          "prompt": "research paper X then mint it as an IP token",
          "expected": "Sequential sessions_spawn(researcher) then sessions_spawn(minter).",
          "verdict": "pass",
          "rationale": "sequential sessions_spawn observed (researcher→minter)",
          "observed_tools": [
            {"seq": 1, "name": "sessions_spawn", "args_preview": "{\"role\":\"researcher\",\"prompt\":\"research paper X\"}"},
            {"seq": 2, "name": "sessions_spawn", "args_preview": "{\"role\":\"minter\",\"prompt\":\"mint IP token\"}"}
          ],
          "wall_ms": 11204,
          "agent_tokens": {"input": 4210, "output": 182},
          "judge_tokens": {"input": 714, "output": 48},
          "transcript_path": "evals/runs/2026-04-19T16-22-04Z/orchestration-sequential-research-mint.md",
          "timed_out": false,
          "stubs_used": false
        }
      ]
    }
  ],
  "summary": {
    "total_rows": 5,
    "passed": 4,
    "failed": 1,
    "timed_out": 0,
    "total_agent_tokens": 38412,
    "total_judge_tokens": 4180,
    "wall_ms": 47183
  }
}
```

Deliberate choices:

- `observed_tools[].args_preview` is a *string* (2 KB truncation already applied). Keeps the schema flat and avoids double-serialisation when args contain user-supplied text.
- No `observed_tools[].result`. Results are in the transcript; the judge only sees call patterns.
- `config_source` is `"skill-local"` / `"sandbox"` / `"override"` so cross-run diffs catch "someone ran with `--sandbox` and that's why the numbers moved".
- `prompts_format` is `"markdown"` or `"yaml"`.

### 4.10 Transcript artefact

`<out-dir>/<skill>-<row-id>.md`, one per row, four sections:

```markdown
# <skill> / <row-id>

## Config
engine=openrouter  model=anthropic/claude-sonnet-4-6  max_tool_rounds=10  workspace=/tmp/tengu-eval-orchestration-.../row-...

## User prompt
research paper X then mint it as an IP token

## Message log
[turn 1 — assistant]
Thinking about how to approach this…
→ tool call: sessions_spawn({"role":"researcher","prompt":"research paper X"})
← tool result: {"agent_id":"a-1234","status":"completed","text":"Found 3 candidate papers..."}
...
[turn N — assistant, final]
Done. The research is summarised and the IP token has been minted as token-id 0x…

## Judge
Input (tool call sequence): ...
Verdict: pass
Rationale: sequential sessions_spawn observed (researcher→minter)
```

### 4.11 Retention & gitignore

`evals/runs/` is added to `.gitignore`. Runs accumulate on disk; no auto-cleanup in v1. Follow-up work: `tengu eval --prune-runs` (or fold into `tengu prune`).

## 5. Data flow — one full run

```
$ tengu eval orchestration

 1. Discover: skills/orchestration/evals/ exists → evaluable.
 2. Parse: skills/orchestration/evals/prompts.md → 5 rows.
 3. Load: skills/orchestration/evals/config.toml → agent config.
 4. For each row (sequential):
    a. Allocate tmp workspace; expand {TMP_WORKSPACE}.
    b. Build engine + PluginToolExecutor; wrap in StubbedExecutor (empty stubs for markdown).
    c. Send user prompt. Engine drives tool loop to completion or timeout_secs.
    d. Tool-call tap collects Observation list.
    e. Serialise observation + expected + final text → judge prompt.
    f. Call judge engine (opus-4-7 via OpenRouter, cached system prompt).
    g. Parse JSON verdict. Write transcript.md. Record row entry.
    h. Teardown workspace.
 5. Print per-skill table to stdout.
 6. Write evals/runs/<ts>/report.json.
 7. Exit 0 (all pass) or 1 (any fail).
```

## 6. Testing

**Unit** (`tests/eval_*.rs`):

- Markdown parser: well-formed table → expected rows; missing header → error; duplicate row ids → error; quoted prompts handled.
- YAML parser: valid schema → rows; unknown key → error; stub without `responses` → error.
- `StubbedExecutor`: stubbed tool returns canned response and increments counter; non-stubbed tool delegates to inner executor; exhausted stub queue repeats last entry.
- Row id derivation: kebab-case, truncate, collision detection.
- Judge output parser: valid JSON → verdict; malformed → fail with diagnostic rationale; prefill `{"verdict":` handled.

**Integration** (behind `#[cfg(feature = "eval-integration")]`, opt-in because they hit OpenRouter):

- Full run against `skills/orchestration/evals/` with a pinned model and a deterministic seed if OpenRouter exposes one (else tolerate nondeterminism — flake tracked, not asserted).
- `--sandbox` override path: runner uses the sandbox config, writes `config_source: "sandbox"` in the report.
- `engine = "claude_code"` path: runner errors with exit code 2 and the documented message.

**Manual smoke checklist** (run once before merging):

- `tengu eval orchestration` on current main → all 5 rows pass (baseline; if any fail, that is a *skill* bug to file separately, not a runner bug).
- `tengu eval` (no skill arg) with only `orchestration` evaluable → exits 0, one-skill report.
- `tengu eval orchestration --concurrency 3` → rows interleave in stderr progress but table output is per-skill-atomic.
- `tengu eval orchestration --format json > out.json` → valid JSON, matches schema.
- `tengu eval nonexistent-skill` → exits 2 with clear error.
- Delete `skills/orchestration/evals/config.toml` → `tengu eval orchestration` exits 2 with clear error. `tengu eval orchestration --sandbox orchestration` (given a sandbox of that name) succeeds.

## 7. Risks & mitigations

- **Judge self-approval.** Mitigated by defaulting to a different model than the agent-under-test. A stderr warning fires if they match.
- **Judge drift across model releases.** The judge model is pinned in `report.json`; any bump is visible in diffs and will produce a one-time re-baseline.
- **Flaky live rows.** Accepted as an eval-subject bug, not a runner bug. The transcript artefact is the investigation tool. Skills with flaky evals migrate to YAML + stubs.
- **Runaway subagent trees.** Hard caps: `max_tool_rounds = 10` in the default config + `timeout_secs = 120` per row. Wall budget is therefore bounded.
- **Workspace leaks.** Tmp workspaces torn down unless `--keep-workspace`. If the runner crashes mid-row, `$TMPDIR/tengu-eval-*` remains — follow-up `tengu prune --eval-workspaces` if this becomes painful.
- **OpenRouter outage.** Runner retries the judge call once on 5xx, then surfaces a clear "judge unreachable — exit 2" error. Partial runs still write `report.json` with completed rows + `error` entry.

## 8. Open questions

- **Should the runner include a `--seed` flag for determinism?** OpenRouter passes seeds through to providers that support them; not all do. v1 omits this; revisit if flakiness becomes the dominant failure mode.
- **Should YAML rows support `forbidden_tools` / `required_tools` as deterministic assertions alongside the LLM judge?** Tempting (cheaper, more stable), but Q3-C picked LLM-judge as the single source of truth. Revisit if the judge proves too noisy on specific rows.
- **Should `config_source: "skill-local"` resolution walk the three tiers, or only the project tier?** v1 walks all three (matching `skill-eval`'s discovery). If this causes confusion — e.g. a `~/.tengu/skills/` copy shadowing the repo copy — collapse to project-only.

## 9. Future work (out of v1)

- Claude Code engine support (tap via `mcp_bridge.rs`).
- GitHub Actions annotation format (`--format gh`).
- Cross-run regression dashboard ("row 2 of orchestration regressed between ff6ad8e and HEAD").
- `forbidden_tools` / `required_tools` deterministic assertions layered on top of the judge.
- `tengu eval --prune-runs` and workspace cleanup.
- Scheduled runs (nightly eval against `main`).

## 10. Acceptance criteria

- `tengu eval orchestration` runs all 5 current rows and emits a table + `report.json` + 5 transcripts, exit code driven by row pass/fail.
- `skills/orchestration/evals/config.toml` is checked in as the canonical `evals/config.toml` example.
- `evals/runs/` is gitignored.
- The spec's unit + integration tests pass in CI.
- `MEMORY.md` is updated with: "eval runner — `tengu eval <skill>`; evals live at `skills/<skill>/evals/{prompts.md|prompts.yaml,config.toml}`; runner at `src/adapters/eval_builder.rs`; judge default Opus 4.7; report schema_version 1 at `evals/runs/<ts>/report.json`."
