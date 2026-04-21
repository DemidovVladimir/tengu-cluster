# Skill Metrics + Evolution — Design

**Date:** 2026-04-20
**Status:** Draft pending user review
**Depends on:** `docs/superpowers/specs/2026-04-20-harness-orchestration-memory-design.md` (orchestrator skeleton must land first; memory tools must be stabilised)
**Scope:** adds a new harness-owned subsystem for skill distillation, metric measurement, and closed-loop skill evolution. Leaf work — blocks nothing.

---

## 1. Context

After the harness-orchestration pivot, skills are understood as narrow, declarative playbooks loaded at session start. The current gap: skills are authored once and drift silently. The user wants a full loop:

1. During a conversation, the user can ask the agent to **distill** the recent successful actions into a new skill, so the next session triggers the same behaviour without guidance.
2. Each skill carries **accuracy metrics** it can be measured against, declared by the skill author on the skill's own terms.
3. The user can ask the harness to **evolve** the skill against those metrics — the harness runs a bounded rewrite→rescore loop and presents the best proposal with a metric delta for user approval.

Prior work partially covered this: `tengu eval <skill>` existed on the abandoned `feature/phase-b-orchestration-collapse` branch, and Phase E designed a `tengu align` + `skill-improver` loop whose specs were deleted in `44f1c57` when the harness pivot happened. This design re-implements that functionality on the harness-orchestration chassis rather than cherry-picking the old code.

---

## 2. Doctrine fit (harness-orchestration invariants preserved)

Three principles from `2026-04-20-harness-orchestration-memory-design.md` §2 are load-bearing here:

- **Harness owns control flow.** Evolve's cycle count, best-cycle selection, regression tolerance, approval gate, scratch-worktree discipline — all Rust policy. The LLM writes rewrite proposals; the LLM does not decide when to stop, whether to commit, or which cycle won.
- **Agents are narrow LLM workers.** `skill-improver` and `fixture-runner` are `AgentConfig` entries, not Rust classes. Their system prompts are fixed; they see a single user message per invocation.
- **Orchestrator is "just an agent with one tool".** Eval and evolve both compose the orchestrator. Fixture replay is a plan with one step per fixture plus a synthesizer. Evolve cycles dispatch the skill-improver via the orchestrator. No parallel orchestration machinery.

Cache discipline (§3.3 of the harness spec) is preserved explicitly — see §6.1 of this spec for the proof table.

---

## 3. Scope

### 3.1 In scope for v1

- LLM-callable `skill_distill` tool. Writes `skills/<name>/{SKILL.md, evals/prompts.yaml, metrics/<scaffold>}`.
- Frontmatter `metrics:` block with four built-in kinds: `llm_judge`, `shell_check`, `tool_assertion`, `script`.
- `tengu eval <skill>` CLI — orchestrator-driven fixture replay; per-metric scoring; rolling `metrics.json` + per-run report; `history.jsonl` append-only.
- `tengu skill metrics <skill>` CLI — read-only inspection of rolling metrics.
- `tengu skill evolve <skill>` CLI — bounded rewrite→rescore loop via a `skill-improver` agent; approval gate with diff + delta; scratch-worktree isolation; manual rollback on reject.

### 3.2 Explicit non-goals for v1

- **Auto-trigger on metric threshold drop.** User explicitly scoped this as a manual verb; threshold automation can later wrap the manual path.
- **Per-invocation live metric capture** from production conversations. Out of scope — the eval path is the only metric source.
- **Hot-reload of distilled or evolved skills into the live conversation.** Forbidden by cache discipline. New/changed skills load on next session start.
- **Parallel evolve cycles.** Sequential only. Predictable LLM cost beats wall-clock savings here.
- **Cross-skill metric dependencies.**
- **Multi-skill evolve** (`tengu skill evolve a b c` rejected; one skill at a time).
- **LLM-driven approval.** The user is always the approver for evolve.
- **Automatic git commit on accept.** Evolve applies the diff; the user commits.
- **Streaming mid-run metric progress.** v1 prints a final table; mid-run output is log-only.
- **Property-based fuzzing on metric schemas** or **worktree stress testing.**
- **Metric migration tooling** when a skill's metric definition changes. v1 logs a warning and starts fresh history for the renamed metric.
- **Metric weighting / aggregate skill score.** Each metric stands alone.

---

## 4. Architecture

### 4.1 Directory layout

```
src/adapters/
├── orchestrator/                      # (from harness-orchestration spec)
├── memory/                            # (from harness-orchestration spec)
├── skill_lifecycle/                   # NEW — harness-owned lifecycle policy
│   ├── mod.rs                         # pub: run_eval, run_evolve, SkillLifecycleConfig
│   ├── distill.rs                     # SkillDistillService — file writer + fixture extractor
│   ├── metrics.rs                     # MetricKind dispatch + MetricResult + validate_metrics
│   ├── metric_kinds/
│   │   ├── llm_judge.rs               # rubric-driven judge LLM call
│   │   ├── shell_check.rs             # run command, match exit/stdout
│   │   ├── tool_assertion.rs          # dispatch a workspace tool, assert result
│   │   └── script.rs                  # execute skills/<n>/metrics/<name>.sh, parse JSON
│   ├── runner.rs                      # EvalRun: orchestrator-driven fixture replay
│   ├── evolve.rs                      # EvolveSession: bounded rewrite→rescore loop
│   ├── storage.rs                     # metrics.json (rolling) + metrics/runs/<ts>/
│   ├── fixtures.rs                    # evals/prompts.yaml read/write + transcript→fixture extraction
│   └── scratch_worktree.rs            # git worktree helpers for evolve cycles
│
└── plugins/
    └── skill_lifecycle/               # NEW — LLM-callable entry point
        ├── mod.rs                     # SkillLifecyclePlugin registering skill_distill
        └── distill.rs                 # SkillDistillTool (Tool trait impl)
```

### 4.2 CLI surface

```
tengu eval <skill> [--sandbox <name>] [--judge-model <m>] [--dry-run]
tengu skill metrics <skill> [--last N]
tengu skill evolve <skill> [--max-cycles N] [--target-metric <name>] [--base-branch <b>]
tengu skill accept-proposal <path>              # scaffolding for future auto-trigger path
```

`tengu eval` replaces the deleted phase-b runner; re-implementation on the harness, not cherry-pick. `tengu skill accept-proposal` is a read-only placeholder in v1 (no auto-trigger generates proposals); included so the surface is stable if threshold automation lands later.

### 4.3 Config additions (`tengu.toml`)

```toml
[skill_lifecycle]
improver_agent       = "skill-improver"     # required for tengu skill evolve
fixture_runner_agent = "fixture-runner"     # required for tengu eval
default_max_evolve_cycles = 3
per_run_dir          = "metrics"            # relative to each skill dir
default_rolling_window = 10                 # for metrics.json pass_rate

[agents.skill-improver]
engine = "openrouter"
model  = "anthropic/claude-opus-4-7"
workspace_tools = ["read_file", "list_directory"]   # read-only; harness applies diffs

[agents.skill-improver.identity]
instructions = """
You are a skill-improver. Given a skill that is under-performing on a specific
metric, you propose a REVISED skill body that should raise that metric's pass
rate without regressing other metrics. You emit ONE JSON object and nothing else:

{
  "proposal": {
    "body_markdown": "<full revised SKILL.md body, frontmatter NOT included>",
    "metrics": [<revised metrics array, OR omitted if metrics unchanged>],
    "rationale": "<1-3 sentences explaining what you changed and why>"
  }
}

Constraints:
- Preserve the skill's name and description (frontmatter) unchanged.
- Do not remove metrics. You may refine a metric's rubric or expectations.
- Do not add metrics without justifying them in the rationale.
- Do not introduce external runtime dependencies.
- Prefer small targeted revisions over wholesale rewrites.
"""

[agents.fixture-runner]
engine = "openrouter"
model  = "anthropic/claude-sonnet-4-6"
# workspace_tools inherited per-fixture (a fixture may declare extra tools).
```

Presence of `[skill_lifecycle]` and both agent entries activates the eval + evolve commands. Missing agents → command errors at dispatch with a clear config-error message (harness Tier 3 error class).

### 4.4 Agent opt-in for `skill_distill`

Agents opt in by listing `"skill_distill"` in `workspace_tools`, matching the pattern used for `shared_cache` / `persistent_store`. Any agent not listing it cannot call the tool. `ToolScope::check_fs_write(ctx.workspace.join("skills"))` enforces boundary on the tool's first line when `tier == "project"`.

For `tier = "workspace"` (`.tengu/skills/`), the scope check targets `ctx.workspace.join(".tengu/skills")`. For `tier = "managed"` (`~/.tengu/skills/`), the existing `check_fs_write` predicate is workspace-rooted and will reject the path; the plan adds a dedicated `check_fs_write_managed_skills` predicate (or equivalent) to handle this tier. Until that lands, `tier = "managed"` returns an explicit `UnsupportedTier` error (not a silent write). Default `tier = "project"` works today.

### 4.5 Dependency footprint

- Brings back `serde_yaml` + `glob` (previously used by the deleted eval runner).
- No `git2` — scratch-worktree shells out to `git worktree` via `ShellExecutionPort`. Matches project convention.

### 4.6 Relationship to existing skills

- `skills/skill-creator/SKILL.md` — kept, refreshed. Description unchanged; body gains a "Distillation" section teaching when to call `skill_distill`.
- `skills/skill-eval/SKILL.md` — kept as a pointer: its Phase-0-era "drift audit" role is subsumed by `tengu eval` + `tengu skill metrics`. Rewritten to direct agents to the CLI.

---

## 5. `skill_distill` tool

### 5.1 Schema

```json
skill_distill(
  name: string,                  // kebab-case, ^[a-z][a-z0-9-]{1,63}$
  description: string,           // "Use when..." triggering description (third person, ≤500 chars)
  body_markdown: string,         // full SKILL.md body (everything after frontmatter)
  metrics: Metric[],             // see §6 for Metric schema
  from_message_index: int,       // 0-based index into the calling agent's full message list.
                                 // Messages in slice [from_message_index .. current) are scanned;
                                 // fixtures built from adjacent (user, assistant) pairs within the slice.
  tier?: "project" | "workspace" | "managed",   // default "project"
  fixture_hints?: {
    include_user_messages?: boolean,    // default true
    expected_outcome?: string,           // narrative of success for LLM judges
    drop_tool_names?: string[]           // tool calls to exclude from fixtures
  }
)
```

### 5.2 Behaviour (deterministic; no LLM calls inside the tool)

1. **Scope check** on first line (`ctx.scope.check_fs_write(<tier_path>)`).
2. **Name validation.** Regex; reject collisions across all three tiers (no `--overwrite` in v1; collision = hard error).
3. **Metrics validation.** Dispatch on `kind`, check required params, `min_pass_rate ∈ [0,1]` if set. Single source of truth with the load-time validator (`skill_lifecycle/metrics.rs::validate_metrics`).
4. **Transcript extraction** via a new read-only `ToolCtx::conversation` handle (§5.6):
   - For each `(user_msg, assistant_msg)` pair in `[from_message_index..current)`, emit one fixture row.
   - `expected_tool_calls` = ordered list of `{tool_name, args_schema_only}`, where string args >32 chars are replaced with `<elided>` (schema-redaction).
   - Names in `fixture_hints.drop_tool_names` are skipped.
5. **Compose SKILL.md.** Frontmatter (`name`, `description`, YAML-serialized `metrics`) + newline + `body_markdown`.
6. **Write files** via temp-dir + atomic rename:
   ```
   skills/<name>/
   ├── SKILL.md
   ├── evals/prompts.yaml
   └── metrics/
       ├── <llm_judge_name>.md     # one per llm_judge metric
       └── <script_name>.sh        # stub echoing {"error":"unimplemented"} per script metric
   ```
   `metrics.json` does NOT appear here — it's created by the first `tengu eval` run.
7. **Return:**
   ```json
   {
     "path": "skills/<name>/",
     "tier": "project",
     "fixtures_created": 4,
     "metrics_declared": 2,
     "loaded_in_current_conversation": false
   }
   ```

### 5.3 Fixture YAML example

```yaml
# skills/mint-ipnft/evals/prompts.yaml
# Seeded from conversation <session-id>, messages [12..18].
# Hand-edit as needed; tengu eval re-reads this file fresh each run.
schema_version: 1
fixtures:
  - id: f1
    prompt: "Mint an IPNFT for the beach-science project on Sepolia"
    expected_tool_calls:
      - tool: http_request
        args_schema: {url: "<elided>", method: "POST"}
      - tool: sign_and_send_transaction
        args_schema: {contract: "<elided>", function: "mint", args: ["<elided>", "<elided>"]}
    expected_outcome: "IPNFT token ID returned, tx confirmed on Sepolia"
    metrics: [mint_confirmed, plan_quality]
```

### 5.4 Explicit non-behaviours

- Does not call an LLM.
- Does not register the new skill into the current conversation's tool/skill inventory.
- Does not commit to git.
- Does not create `metrics.json`.

### 5.5 Error surface (returned, never panics)

| Condition | Error |
|---|---|
| Name collision in tier | `SkillExists { name, tier, existing_path }` |
| Invalid metric kind / missing param | `InvalidMetric { name, reason }` |
| `from_message_index` out of range | `InvalidTranscriptRange { given, conversation_length }` |
| Write failure (permissions, disk) | `WriteFailed { path, io_error }` |

No partial writes: temp-dir + atomic rename, same pattern as `persistent_store`.

### 5.6 `ToolCtx` extension

Adds a read-only conversation view so the tool can do mechanical transcript extraction without re-synthesizing via an LLM call:

```rust
pub struct ToolCtx<'a> {
    // existing fields...
    pub conversation: ConversationView<'a>,  // NEW
}

pub struct ConversationView<'a> {
    messages: &'a [Message],
}
impl<'a> ConversationView<'a> {
    pub fn len(&self) -> usize { ... }
    pub fn slice(&self, from: usize, to: usize) -> Result<&[Message], OutOfRange> { ... }
}
```

Read-only; cannot mutate history, cannot observe other agents' conversations. Minor breaking change to `Tool::execute` signature — migration enumerated in the implementation plan.

### 5.7 Dogfooding

`skills/skill-creator/SKILL.md` gains a "Distillation" section with a worked example. `skill-creator` itself becomes the skill the orchestrator invokes when the user says "let's save this as a skill."

---

## 6. Metrics contract

### 6.1 Frontmatter `metrics:` schema

```yaml
---
name: mint-ipnft
description: Use when minting an IPNFT for a molecule project on testnet or mainnet.
metrics:
  - name: mint_confirmed                       # unique within the skill
    kind: shell_check
    cmd: "cast call $CONTRACT ownerOf $TOKEN_ID --rpc-url $RPC_URL"
    expect_stdout_matches: "^0x[0-9a-f]{40}$"
    expect_exit_code: 0                        # optional; defaults to 0
    min_pass_rate: 0.8

  - name: plan_quality
    kind: llm_judge
    rubric_file: metrics/plan_quality.md
    judge_model: anthropic/claude-opus-4-7     # optional; defaults from [skill_lifecycle]
    min_pass_rate: 0.7

  - name: tx_hash_recorded
    kind: tool_assertion
    tool: persistent_store
    action: get
    key: "last_mint_tx_hash"
    assert:
      value_matches: "^0x[0-9a-f]{64}$"
    min_pass_rate: 1.0

  - name: custom_gas_budget
    kind: script
    path: metrics/custom_gas_budget.sh         # emits JSON: {"pass": bool, "score": float, "notes"?: str}
    min_pass_rate: 0.9
---
```

### 6.2 Invariants (enforced by `validate_metrics`)

- The `metrics:` block itself is optional in frontmatter. A skill with no `metrics:` block loads normally and is inert to `tengu eval` (eval errors with `NoMetricsDeclared` — not a silent pass).
- If the block is present, it must be well-formed per the rules below.
- `name` unique within the skill.
- `kind` is one of the four built-ins; unknown kinds fail loud (load-time error, not silent drop).
- `min_pass_rate ∈ [0,1]` if set; absent means "report but do not gate evolve decisions."
- `shell_check` requires `cmd`; at least one of `expect_stdout_matches` | `expect_exit_code` must be set.
- `llm_judge` requires `rubric_file`; path must exist under the skill directory.
- `tool_assertion.tool` must be a registered workspace tool (checked against `ToolRegistry` at skill load).
- `script.path` must exist under the skill directory; executable bit not required (runner shells out to `sh <path>`).

### 6.3 Metric-kind dispatch

Shared trait:

```rust
#[async_trait]
trait MetricKind {
    async fn run(
        &self,
        spec: &MetricSpec,
        fixture: &FixtureContext,
        ctx: &MetricRunCtx,
    ) -> MetricOutcome;
}

pub struct MetricOutcome {
    pub pass: bool,
    pub score: f32,                     // 0.0..=1.0
    pub notes: Option<String>,          // 1-2 sentences, stored in per-run report
    pub raw: serde_json::Value,         // kind-specific details for debugging
}
```

| Kind | `run` behaviour | `score` derivation |
|---|---|---|
| `shell_check` | `ShellExecutionPort` spawn, 30s timeout, `$VAR` expansion. Match stdout regex + exit code. | 1.0 if all checks pass, else 0.0. |
| `llm_judge` | Load `rubric_file`, build `{rubric}\n\nFixture prompt:\n{fixture.prompt}\n\nAssistant transcript:\n{transcript}\n\nReturn JSON: {verdict, score, notes}`. Prefilled assistant `{"verdict":"` to force JSON. | Parsed `score`; `pass = verdict == "pass"`. |
| `tool_assertion` | Dispatch named tool via the runtime `ToolRegistry`. Evaluate `assert` block (`value_matches`, `value_equals`, `value_in`). | 1.0 if assertion holds, else 0.0. |
| `script` | `sh <path>` with `PROMPT`/`TRANSCRIPT`/`EXPECTED_OUTCOME` env vars populated from fixture; 60s timeout. Parse stdout JSON. | From JSON; malformed → `pass=false, score=0.0, notes="malformed script output"`. |

### 6.4 Storage layout

```
skills/<name>/
├── metrics.json                 # ROLLING SNAPSHOT (overwritten each run)
└── metrics/
    ├── <metric_name>.md         # llm_judge rubric(s)
    ├── <metric_name>.sh         # script metric(s)
    ├── runs/
    │   └── 2026-04-23T09-12-40Z/
    │       ├── report.json      # full per-run detail
    │       └── transcripts/
    │           ├── f1.md
    │           └── f2.md
    └── history.jsonl            # APPEND-ONLY, one line per run per metric
```

**`metrics.json`** (rolling snapshot, small, human-readable):

```json
{
  "schema_version": 1,
  "skill": "mint-ipnft",
  "last_run": "2026-04-23T09:12:40Z",
  "last_run_ref": "metrics/runs/2026-04-23T09-12-40Z",
  "metrics": {
    "mint_confirmed":  {"pass_rate": 0.75, "n": 4, "min_pass_rate": 0.8, "gated": true},
    "plan_quality":    {"pass_rate": 0.83, "n": 6, "min_pass_rate": 0.7, "gated": false}
  },
  "rolling_window": 10
}
```

`pass_rate` is computed over the last `rolling_window` runs from `history.jsonl` (default 10; overridable per-metric via `rolling_window` field in frontmatter). `gated = pass_rate < min_pass_rate` — this drives `tengu skill evolve`'s target-metric selection.

**`history.jsonl`** — one JSON line per (run, metric):

```jsonl
{"ts":"2026-04-22T14-03-11Z","metric":"mint_confirmed","pass_rate":0.5,"n":4,"ref":"metrics/runs/2026-04-22T14-03-11Z"}
{"ts":"2026-04-22T14-03-11Z","metric":"plan_quality","pass_rate":0.66,"n":6,"ref":"metrics/runs/2026-04-22T14-03-11Z"}
{"ts":"2026-04-23T09-12-40Z","metric":"mint_confirmed","pass_rate":1.0,"n":4,"ref":"metrics/runs/2026-04-23T09-12-40Z"}
```

Append-only, grep-friendly, diff-friendly for review. No database, no indexing.

**`runs/<ts>/report.json`** — full per-run detail: fixture-by-fixture, metric-by-metric, scores, notes, token counts, judge model, transcripts (inline for small runs, referenced for large).

### 6.5 `tengu eval <skill>` runner (orchestrator-driven)

```
tengu eval mint-ipnft
  ↓
 1. Load skills/mint-ipnft/evals/prompts.yaml → N fixtures
 2. Build synthetic user_msg: "Run fixtures f1..fN for skill mint-ipnft."
 3. Orchestrator emits plan: one step per fixture → fixture-runner agent,
    + one terminal synthesizer step that aggregates outputs.
 4. Each fixture-runner step runs the fixture prompt against the configured worker,
    captures the transcript, returns it as step output.
 5. Synthesizer step collects transcripts.
 6. Runner (Rust) iterates frontmatter.metrics[], invokes MetricKind.run(...),
    builds MetricOutcomes.
 7. Runner writes runs/<ts>/report.json, transcripts/fN.md, updates metrics.json,
    appends to history.jsonl.
 8. Exit code: 0 if all `gated` metrics pass, 1 if any `gated` fails, 2 on runner error.
```

Two consequences of composing the orchestrator:
- Retry/replan machinery transparently covers flaky fixture runs.
- Runner reuses `memory/injector.rs` so fixtures see the same memory context a real turn would.

### 6.6 Sandboxing side-effectful fixtures

Two v1 mitigations:

1. **Restricted `scopes` block on `fixture-runner`** — default denies `check_fs_write` outside workspace and `check_network` outside `testnet.*` / `localhost`. Overridable per-skill via optional `evals/scope_overrides.toml`. Concrete default allow-list picked in the implementation plan against real aura/molecule configs.
2. **`--dry-run` flag on `tengu eval`** — stubs side-effectful tools (`sign_and_send_transaction`, `http_request`) to return canned success responses. Metrics depending on real side effects are marked `skipped in dry-run` (not a pass, not a fail).

---

## 7. Evolution loop (`tengu skill evolve`)

### 7.1 Flow

```
tengu skill evolve mint-ipnft [--max-cycles 3] [--target-metric <name>] [--base-branch <b>]
  ↓
 1. BASELINE: run tengu eval mint-ipnft → baseline_report
    Identify target metric:
      - if --target-metric given: use it (error if not gated)
      - else: pick the gated metric with lowest pass_rate;
              if none gated, exit 0 with "nothing to evolve"
  ↓
 2. SCRATCH WORKTREE: git worktree add .tengu/worktrees/evolve-<skill>-<ts>
    from HEAD of current (or --base-branch) branch. Cycle writes stay inside it.
  ↓
 3. CYCLE LOOP (1..=max_cycles):
      a. Build skill-improver input (§7.3).
      b. Orchestrator dispatches one step → skill-improver agent.
         → expected output: {"proposal": {"body_markdown": ..., "metrics": [...]?, "rationale": ...}}
      c. Runner applies proposal to SKILL.md in the worktree (atomic).
      d. Runner executes tengu eval inside the worktree → cycle_report.
      e. Record (cycle_n, diff_summary, metric_delta, rationale) to evolve_log.
      f. Early exit if:
           - target metric hits 1.0, OR
           - all gated metrics pass AND target improved ≥ 0.15 absolute.
  ↓
 4. BEST-CYCLE SELECTION (§7.5).
  ↓
 5. APPROVAL GATE (§7.6).
  ↓
 6. ON APPROVE:
      - Copy SKILL.md from worktree → real skills/<name>/SKILL.md
      - Append proposal summary + metric delta to skills/<name>/metrics/evolve_log.md
      - Run tengu eval once more on the real path (sanity check vs cycle_report)
      - git worktree remove
      - No auto-commit; exit with "Changes applied. Run `git diff skills/<name>/` to review."
  ↓
 7. ON REJECT:
      - git worktree remove (discards cycle writes)
      - Append rejection note to evolve_log.md
      - "No changes applied. Baseline preserved."
```

### 7.2 Scratch-worktree discipline

- One worktree per invocation: `.tengu/worktrees/evolve-<skill>-<ISO8601>`.
- Created from HEAD of `--base-branch` (default: current).
- All cycle writes live inside the worktree. Cycle per-run reports live at `metrics/runs/evolve-<ts>/cycle-N/` so they never pollute real rolling history.
- Removed after apply-or-reject, always. Ctrl-C handler removes worktree before exit.
- `.tengu/worktrees/` added to `.gitignore` by the plan.
- **Non-git workspace fallback:** plain scratch directory at `.tengu/scratch/evolve-<skill>-<ts>/`; rollback is `rm -rf`. Emits a warning at start.

### 7.3 skill-improver user message (per invocation)

```
Skill: mint-ipnft
Current SKILL.md body:
<<<
{current_body}
>>>

Current metrics (frontmatter block):
<<<
{current_metrics_yaml}
>>>

Target metric: plan_quality
Target pass rate: 0.66 (baseline)  /  min_pass_rate: 0.70  → gated (failing)

Failing fixture transcripts (up to 3):
--- fixture f2 ---
{transcript_f2}
--- fixture f5 ---
{transcript_f5}
--- fixture f7 ---
{transcript_f7}

Other metrics and their baseline pass rates (keep these >= baseline - 0.05):
- mint_confirmed: 1.0
- tx_hash_recorded: 1.0

Previous attempts in this session (empty on cycle 1):
{previous_cycles_summary}

Produce your proposal.
```

On cycle 2+, `previous_cycles_summary` lists prior rationales + metric movements so skill-improver doesn't repeat a failed angle.

### 7.4 Applying a proposal (atomic)

`evolve.rs`:

1. Parse existing frontmatter + body.
2. Replace body with `proposal.body_markdown`.
3. If `proposal.metrics` provided, replace `frontmatter.metrics` with the new list; validate via `validate_metrics`. Invalid proposal → cycle fails, skill-improver re-invoked with the validation error as additional context (counts against `max_cycles`).
4. Temp-file + atomic rename.
5. No-op detection: empty body OR body identical to current AND metrics unchanged → cycle marked "no-op," counted as failure.

### 7.5 Best-cycle selection

Ordered rules:

1. Highest target-metric pass rate.
2. No regression > 0.05 absolute on any non-target gated metric (cycles violating this are dropped).
3. Fewer body-line additions (`max(0, lines(new) - lines(old))`, lower wins).
4. Earliest cycle wins (stability tie-break).

If rule 2 eliminates all cycles, no "best" — user sees `"evolve found N proposals but all regressed gated metrics. No changes applied."` Worktree preserved for 24 hours for inspection, auto-cleaned on next evolve.

### 7.6 Approval gate UX

```
Skill: mint-ipnft
Target metric: plan_quality (baseline 0.66 → proposed 0.83, delta +0.17)

Non-target gated metrics (all must stay >= baseline - 0.05):
  mint_confirmed:     1.00 → 1.00   ✓
  tx_hash_recorded:   1.00 → 1.00   ✓

SKILL.md changes (unified diff):
  @@ -14,6 +14,10 @@
   ## When to Use
  -- Use when minting a new IPNFT
  -- Use after a project proposal is approved
  +- Use when minting a new IPNFT on any supported chain
  +- Use after a project proposal is approved AND funding address confirmed
  +- Use only when the caller has a wallet with the agent scope
  +
  @@ -28,4 +32,6 @@
   ## Procedure
  -1. Call http_request to fetch the current gas price.
  +1. Call get_wallet_address to confirm scope.
  +2. Call http_request to fetch the current gas price.
  +3. ...

Rationale:
  Added an explicit scope-check step before sending. Failing fixtures f2, f5, f7
  all tried to sign without first calling get_wallet_address; the judge penalized
  the missing check.

[y] apply, [n] discard, [d] show details, [o] open worktree:
```

### 7.7 Rollback + audit trail

- **Reject:** worktree removed; nothing touched in real skill directory; one-line append to `skills/<name>/metrics/evolve_log.md`: timestamp, target metric, baseline/best deltas, "user rejected."
- **Accept:** changes applied; longer entry in `evolve_log.md` with rationale + diff reference + post-apply sanity-run result; no git commit (user owns branch + message convention).

---

## 8. Data flow + integration points

### 8.1 Request flows

| Trigger | Entry point | Path |
|---|---|---|
| Agent calls `skill_distill(...)` in a conversation | `plugins/skill_lifecycle/distill.rs::SkillDistillTool::execute` | → scope check → validate → extract fixtures → write files → return JSON |
| User runs `tengu eval <skill>` | `main.rs Commands::Eval` | → `skill_lifecycle::runner::run_eval` → compose `Orchestrator::handle` → metric dispatch → storage write → exit code |
| User runs `tengu skill evolve <skill>` | `main.rs Commands::SkillEvolve` | → `skill_lifecycle::evolve::run_evolve` → baseline eval → scratch worktree → N cycles (each = skill-improver invocation + cycle eval) → best-cycle selection → approval gate → apply/reject |
| User runs `tengu skill metrics <skill>` | `main.rs Commands::SkillMetrics` | → read `metrics.json` + tail of `history.jsonl` → render table |

### 8.2 Tool-registry interactions

- `skill_distill` registers via `SkillLifecyclePlugin` alongside existing plugins (workspace, http, crypto, cache, memory, skill, mcp). Per-agent opt-in via `workspace_tools`.
- `tool_assertion` metrics dispatch the named tool through the same `ToolRegistry` the runtime uses — no duplicate resolution path.
- Fixture-runner inherits workspace tools from its `AgentConfig`; fixtures may declare extra tools per-row (merged at fixture-dispatch time).

### 8.3 Memory-subsystem interactions

- Runner calls `memory/injector.rs::for_turn(mgr, agent="fixture-runner", query=fixture.prompt)` before each fixture step, matching production turn behaviour.
- Fixture-runner sync_turn writes are suppressed during eval runs (`EvalRun::suppress_memory_writes = true`) — we don't want eval replays to pollute daily logs. Evolve cycle runs also suppress writes.
- `skill-improver` uses `memory_search` only if its `workspace_tools` includes it (default config above grants `read_file` + `list_directory` only, keeping it read-only over the skill filesystem).

---

## 9. Error handling

### 9.1 Tier 1 — metric-kind run failure (per-fixture)

Any `MetricKind::run` failure becomes `MetricOutcome { pass: false, score: 0.0, notes: <error>, raw: <diagnostic> }`. Does not abort the run; the report captures the failure.

### 9.2 Tier 2 — fixture-runner step failure

Wrapped by the orchestrator `retry.rs`. Per-step `max_attempts` from orchestrator config (default 3). Exhaustion → orchestrator replan (tries a different fixture-runner invocation shape). Replan exhaustion → runner exit code 2 ("runner error").

### 9.3 Tier 3 — subsystem-level failure

| Condition | Behaviour |
|---|---|
| `[skill_lifecycle]` missing + user runs `tengu eval` | Error message pointing at config; exit 2. |
| Required agent missing | Error message; exit 2. |
| `skills/<name>/` missing | Error message; exit 2. |
| Skill has no `metrics:` block | `NoMetricsDeclared`; exit 2. |
| `evals/prompts.yaml` malformed | Error message citing line; exit 2. |
| skill-improver produces malformed JSON after 3 retries | Cycle marked failed; counted toward `max_cycles`. |
| All cycles regress non-target gated metrics | "No improvement; no changes applied." Exit 0 (completed without applying). |
| Worktree creation fails | Error; exit 2. Non-git fallback kicks in before erroring if enabled. |
| Ctrl-C during cycle | Signal handler removes worktree, then exits 130. |

---

## 10. Testing strategy

Project convention: cap individual test runs ≤30s; no blind full `cargo test`.

### 10.1 Unit tests (inline, default feature set)

- `skill_lifecycle/metrics.rs::validate_metrics` — each invariant violation case.
- `metric_kinds/shell_check.rs` — mocked `ShellExecutionPort`; exit/stdout combos; timeout; `$VAR` expansion.
- `metric_kinds/llm_judge.rs` — mocked LLM client; malformed JSON → 0/notes; pass/fail paths.
- `metric_kinds/tool_assertion.rs` — mocked `ToolRegistry`; each `assert` operator.
- `metric_kinds/script.rs` — real `sh` subshell with stub scripts; valid, malformed, missing-`pass` JSON shapes.
- `skill_lifecycle/storage.rs` — `metrics.json` round-trip; `history.jsonl` append idempotency; rolling-window computation.
- `skill_lifecycle/fixtures.rs` — transcript→fixture extraction; schema-redaction; `drop_tool_names`; empty-range.
- `plugins/skill_lifecycle/distill.rs` — each error case from §5.5; atomic-rename verified.
- `skill_lifecycle/evolve.rs` — best-cycle selection orderings; regression tolerance; no-op detection.

### 10.2 Integration tests (opt-in `skill-lifecycle-integration` feature)

- Distill a skill from a canned conversation; verify `SKILL.md` + `prompts.yaml` + metric scaffolds.
- Run `tengu eval` against a test skill with one of each metric kind; verify `metrics.json`, per-run report, `history.jsonl` append, exit code.
- Run `tengu skill evolve` with scripted skill-improver mock; verify best-cycle selection, approval prompt, worktree cleanup.
- Evolve with every cycle regressing → "no changes applied" path; worktree preserved.
- Ctrl-C mid-cycle → worktree removed.

### 10.3 Eval suite (dogfood)

- `skills/skill-creator/evals/prompts.yaml` — seeded fixtures exercising `skill_distill`. Metric: `llm_judge` scoring "does the distilled SKILL.md read like a coherent skill?"
- `skills/mint-ipnft/evals/prompts.yaml` (hypothetical; seeded from a real aura-orchestrator run) — used for manual smoke, not CI.

### 10.4 Deliberately skipped

- Property-based fuzzing on metric schemas.
- Multi-cycle evolve against live OpenRouter in CI (gated behind integration flag + env vars).
- Worktree stress tests (many parallel evolves).

---

## 11. Implementation sequencing (drives the plan)

1. `skill_lifecycle/metrics.rs` + `metric_kinds/` — pure logic, no orchestrator dependency.
2. `skill_lifecycle/storage.rs` + `fixtures.rs` — file I/O, unit-testable.
3. `skill_lifecycle/runner.rs` — depends on `orchestrator/` skeleton (wait until harness-orchestration §11 step 3 complete).
4. `plugins/skill_lifecycle/distill.rs` + `ToolCtx::conversation` extension.
5. CLI wiring: `tengu eval`, `tengu skill metrics`.
6. `skill_lifecycle/evolve.rs` + `scratch_worktree.rs` — full closed loop.
7. CLI wiring: `tengu skill evolve`, `tengu skill accept-proposal`.
8. Dogfood: `skills/skill-creator/SKILL.md` gains the "Distillation" section; `skills/skill-eval/SKILL.md` rewritten as a pointer to the CLI.
9. Evals on the subsystem itself: `skill-creator/evals/` + CI integration test.

---

## 12. Cache-discipline proof table

| Invariant (harness-orchestration §3.3) | Respected by |
|---|---|
| System prompts stable for conversation lifetime | `skill_distill` writes files only; return value includes `"loaded_in_current_conversation": false`. Evolve cycles run in a scratch worktree, never touching live conversations. |
| Tool schemas stable for conversation lifetime | Tool set fixed at conversation init from `AgentConfig.workspace_tools`. No mid-conversation registration in any flow. |
| Memory injected at API-call time, never persisted into history | Runner uses `memory/injector.rs::for_turn` per fixture step; memory writes suppressed during eval/evolve runs (see §8.3). |
| No skill body appended to system prompt mid-conversation | skill-improver receives the skill under review as a USER message, not injected into its own system prompt. |
| Past context never mutated | Evolve cycles live in scratch worktree; `history.jsonl` is append-only; baseline reports are immutable once written. |

---

## 13. Open items (to resolve in the plan, not the spec)

- **`ToolCtx::conversation` migration.** Minor breaking change to `Tool::execute` signature. Plan enumerates every call site and the migration ordering (adapter tools → plugins → tests).
- **fixture-runner sandboxing defaults.** §6.6 names the mitigations; concrete default allow-list (which network hosts, which filesystem prefixes) decided in the plan against real configs.
- **skill-improver prompt calibration.** §4.3's prompt + §7.3's user message are first drafts. Plan adds a calibration task: run evolve against two deliberately-broken skills, inspect proposals, tweak prompt until outputs are consistently coherent.
- **Ctrl-C signal handler placement.** Process-wide handler vs. `evolve.rs` drop guard — decided in plan.
- **`evals/scope_overrides.toml` precedence.** When both `AgentConfig.scopes` and `scope_overrides.toml` apply, which wins. Expected: override file is additive (grants only); denials always win. Confirmed in plan.

---

## 14. Non-goals summary (single reference point)

Auto-trigger on threshold drop · live per-invocation metrics · hot-reload of new skills into live conversations · parallel evolve cycles · cross-skill metric dependencies · multi-skill evolve · LLM-driven approval · auto-commit on accept · mid-run streaming · property fuzzing · metric migration tooling · aggregate skill score.

Each appears in the section where it would naturally belong, with one-line rationale. This section exists only for quick lookup.
