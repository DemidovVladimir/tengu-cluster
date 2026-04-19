# Phase E — Self-Alignment Loop

**Status:** Design, pending approval
**Date:** 2026-04-16
**Depends on:** Phase A (subagent spawning, for fixture replay) and Phase B (orchestration skill exists and can be edited by the loop). Does not depend on Phase C or D.
**Blocks:** Nothing. Can ship any time after Phase B; parallel with Phase C/D is fine.

---

## 1. Problem

Phase 0 installed `skill-creator` (create) and `skill-eval` (measure) as the two halves of a skill evolution loop — but the loop is not yet closed:

- **`skill-eval` only does structural checks today** — gating (binaries/env/OS), drift (stale references), dead-skill detection. It does not run a skill against a prompt, observe behaviour, or produce a numeric score.
- **None of the seven installed skills declare evaluation metrics.** Frontmatter varies wildly: some use `homepage`, some use `metadata.openclaw`, some inline auth fields. No common schema for "what does success look like for this skill."
- **No feedback is captured from usage.** When a user says "don't do X" or silently retries, that signal is lost. Corrections never feed back into skill refinement.
- **No meta-skill closes the loop.** There is no `skill-improver` that reads eval results + traces and proposes `SKILL.md` edits. `skill-creator` authors from scratch; nothing revises based on measured drift.

The stated vision is: *the more time the user spends with tengu, the more aligned the system gets.* That requires a measurement → proposal → apply loop. Today we have measurement stubs and nothing else.

## 2. Goal

**0. Implements the "skills are logic" principle of `docs/architecture.md` recursively** — the alignment loop itself is a skill (`skill-improver`), a CLI primitive (`tengu align`), and a metric schema in frontmatter, not a new Rust subsystem. The no-compromise corollary applies: if this phase is tempted to add Rust code that encodes *what makes a skill "good"*, that code is a skill, not Rust. `skill-improver` makes that judgment; Rust only orchestrates the replay and diff.

Close the evolution loop so skills improve over time against objective, deterministic metrics:

1. **Every skill declares its success criteria in frontmatter.** A new `eval:` block names expected triggers, expected non-triggers, fixture patterns, and numeric thresholds.
2. **Every skill carries an `evals/*.jsonl` fixture set** that describes sample prompts and expected behaviour (skill fired? which tools called? outputs contain what?).
3. **`tengu align` runs all fixtures** in sandboxed subagents, records real traces, computes metrics, and writes a dated report.
4. **`skill-improver` meta-skill reads a failing report + the skill body + the failing traces**, and writes a proposed `SKILL.md` edit to `proposals/<skill>-<date>.md` for the user to review and apply.
5. **A light `/feedback` primitive captures user-side signal** — appended to `memory/feedback.md`, read by `skill-improver` as additional ground truth beyond fixture outcomes.

**Expected Rust LOC delta:** ~+250 (fixture-replay runner, `tengu align` CLI, `/feedback` command). No new subsystems — the whole loop sits on top of Phase A's subagent spawning and Phase B's orchestration skill.

## 3. Non-goals

- **LLM-judge mode.** Phase E's scoring is deterministic and structural (did the expected skill fire, did the expected tools run, does the output contain expected substrings). LLM-judged binary pass/fail on loose prompt tests is a separate, complementary mechanism — specified in `2026-04-19-eval-runner-design.md` (`tengu eval <skill>`) and shipped ahead of Phase E. The eval runner handles the "did this skill teach the right decomposition" question on markdown/YAML prompts; Phase E's `tengu align` handles the "do these JSONL fixtures still score above threshold" question. Neither subsumes the other — a fully-aligned repo runs both.
- **Auto-apply.** `skill-improver` proposes patches; it never commits to `SKILL.md` directly. User review is the gate. A tiered auto-apply mode (low-risk wording fixes auto-applied) is a follow-up, not Phase E.
- **Scheduled/daemon runs.** `tengu align` is on-demand. Schedule via existing cron / `loop` mechanisms if wanted; no background daemon in the binary.
- **Cross-skill regression tests.** Each skill's fixtures test only that skill. Interaction between skills (e.g., orchestration + molecule-x402) is out of scope — too brittle for a first cut.
- **Live production trace analysis.** `align` runs controlled fixtures against a sandboxed subagent, not scraped real-user traces. `/feedback` captures real-user signal, but fixtures stay synthetic.
- **Non-skill policies.** Only skills get fixtures. Agent configs, engine behaviour, channel behaviour are not evaluated here.

## 4. Architecture

### 4.1 Metric schema — frontmatter addition

Skills grow an optional `eval:` block:

```yaml
---
name: aura-orchestrator
description: End-to-end DeSci molecule — POI registration, IP-NFT minting, ...
homepage: https://testnet.molecule.xyz
eval:
  triggers:
    - "mint IP-NFT for /tmp/paper.pdf"
    - "register POI then mint and publish"
  non_triggers:
    - "what's the price of ETH?"
    - "how do I list my files?"
  fixtures: "./evals/*.jsonl"
  thresholds:
    trigger_precision:  0.90    # of N invocations, fraction that fired correctly
    trigger_recall:     0.85    # of N triggerable prompts, fraction that fired
    tool_call_match:    0.80    # of expected tool calls, fraction observed with matching args
    outcome_contains:   0.80    # of expected outcome strings, fraction present
---
```

`eval:` is optional during migration. Skills without it are silently skipped by `tengu align` with a "no metrics declared" note in the report. A lint warning (dormant until E5 lands) encourages authors to add the block.

### 4.2 Fixture format — `evals/*.jsonl`

Newline-delimited JSON, one fixture per line:

```jsonl
{"id":"mint-basic", "prompt":"mint an IP-NFT for /tmp/paper.pdf", "expected_skill":"aura-orchestrator", "expected_tool_calls":[{"name":"http_request","args_pattern":{"url":"*/poi/register*","method":"POST"}}, {"name":"sign_and_send_transaction"}], "expected_contains":["tx hash"]}
{"id":"mint-with-announce", "prompt":"mint /tmp/paper.pdf and announce on X", "expected_skill":"aura-orchestrator", "expected_tool_calls":[{"name":"http_request","args_pattern":{"url":"*/poi/register*"}}, {"name":"sign_and_send_transaction"}, {"name":"http_request","args_pattern":{"url":"*/announce*"}}], "expected_contains":["posted","success"]}
{"id":"not-trigger-price", "prompt":"what's the price of ETH?", "expected_skill":null}
```

Fields:

| Field | Type | Required | Purpose |
|---|---|---|---|
| `id` | string | yes | stable ID for reporting |
| `prompt` | string | yes | user input |
| `expected_skill` | string or null | yes | which skill should fire (null = none of the evaluated skills) |
| `expected_tool_calls` | array | no | ordered sublist of tool-call shapes |
| `expected_contains` | array of strings | no | substrings that must appear in the final assistant output |

Matching semantics:

- **Skill trigger.** The runner inspects the trace's `SkillCatalog::fetch_body` calls and/or tool calls from that skill's manifest. A skill "fires" if the subagent reads its `SKILL.md` body during the turn.
- **Tool-call order.** `expected_tool_calls` is an ordered sublist (not a strict prefix). Additional tool calls in between are allowed.
- **Args pattern.** Glob match on string values; objects matched structurally. A missing key in the pattern means "don't care."
- **Outcome contains.** Case-insensitive substring match, all required unless threshold says otherwise.

### 4.3 `tengu align` CLI

New subcommand, thin wrapper around a new `alignment_runner.rs`:

```
$ tengu align                              # run all skills' fixtures
$ tengu align --skill molecule-x402        # one skill
$ tengu align --fixtures "mint-*"          # fixture id glob
$ tengu align --report-only                # skip skill-improver invocation
$ tengu align --improve                    # explicitly run skill-improver on failures
```

Flow per fixture:

1. Open a sandboxed session via `channel_runtime::open_session(workspace=tempdir, agent=default)`.
2. Send the fixture's `prompt` as user input.
3. Capture the full trace: tool calls, skill catalog reads, final assistant output.
4. Score against the fixture:
   - Did the expected skill fire? (trigger)
   - Do the expected tool calls appear in order with matching args? (tool_call_match)
   - Does the output contain the expected substrings? (outcome_contains)
5. Aggregate per skill. Compute per-threshold pass/fail.
6. Write `reports/YYYY-MM-DD-HHMMSS-align.md` (human-readable) and append one line per skill to `metrics/<skill>.jsonl` (machine-readable rolling series so trends are visible).
7. If `--improve` is set (explicit opt-in) and any skill fails a threshold, spawn `skill-improver` for that skill.

**Sandboxing:**

- Sandboxed sessions get a fresh ephemeral workspace (tempdir) with a known fixture environment — only env vars in `[alignment].sandbox_env_allowlist` are inherited; everything else is stripped.
- Real HTTP/crypto calls are allowed by default. Authors write fixtures that tolerate variance (assert `"tx hash"` substring, not the exact value) and target testnets / mock endpoints for destructive tools.
- V1 keeps sandboxing simple. A future `--mock-network` mode is a follow-up, not Phase E.

### 4.4 `skill-improver` meta-skill

Authored using `skill-creator` (meta-meta: the skill that writes skills is the skill that reads a failing skill and writes a proposal). Lives at `skills/skill-improver/SKILL.md`.

Input it receives (assembled by `tengu align --improve` into a single prompt):

- The failing align report entry — which skill, which fixtures failed, which thresholds.
- The current `SKILL.md` body for the subject skill.
- The trace JSON for each failing fixture.
- Optional: recent entries in `memory/feedback.md` mentioning this skill.

Its output: a file at `proposals/<skill>-<date>.md` with:

```markdown
# Proposal: <skill> — <date>

## Diagnosis
<1–3 sentences>

## Failing fixtures
- <id>: <threshold> = <observed>, expected ≥ <target>

## Suggested edit
<unified-diff-style block against SKILL.md>

## Expected after
<threshold>: ≥ <estimate>

## Rationale
<1 sentence>
```

It does **not** edit the skill directly. The user reviews and applies manually (or via a follow-up tiered-auto-apply phase).

Rules of thumb embedded in the skill body:

- `trigger_precision` low → description over-triggers → tighten with explicit non-examples
- `trigger_recall` low → description under-triggers → add example phrases, lower specificity barrier
- `tool_call_match` low → body's playbook is wrong → rewrite the relevant section, citing the fixture that failed
- `outcome_contains` low → body's final-output guidance is wrong → add explicit "say X when done" instruction

### 4.5 Feedback capture — `/feedback` primitive

Thin channel-level command (works in CLI and Telegram):

```
/feedback @aura-orchestrator don't post to X without asking
```

Writes one entry to `memory/feedback.md`:

```markdown
## 2026-04-16 14:22
- **Skill:** aura-orchestrator
- **Message:** don't post to X without asking
- **Session:** session:01HXYZ...
```

`@<skill>` prefix is parsed out; if absent the entry is general. `skill-improver` reads this file during its proposal pass as qualitative context alongside the quantitative metrics.

`/feedback` does **not** trigger `skill-improver` on its own — feedback accumulates until the user runs `tengu align --improve`.

### 4.6 Directory layout after Phase E

```
skills/
  aura-orchestrator/
    SKILL.md                # gains `eval:` block
    evals/
      mint-basic.jsonl
      non-triggers.jsonl
  skill-creator/
    SKILL.md                # updated to document eval: block authoring
  skill-eval/
    SKILL.md                # unchanged semantics; internally delegates to the runner
  skill-improver/           # new
    SKILL.md
    evals/
      improver-basic.jsonl  # self-calibrating — the improver has its own fixtures
  # ... all other skills gain evals/

reports/                    # new — human-readable align reports
  2026-04-16-align.md
metrics/                    # new — rolling machine-readable metrics (one JSONL per skill)
  aura-orchestrator.jsonl
  molecule-x402.jsonl
proposals/                  # new — skill-improver output
  aura-orchestrator-2026-04-16.md
memory/
  feedback.md               # new — /feedback sink
```

### 4.7 Rust surface additions

One new file plus three small integration points:

- **`src/adapters/alignment_runner.rs`** (~180 LOC). `AlignRunner::run(skill_filter, fixture_filter, improve: bool)` walks the skill tree, loads fixtures, spawns a sandboxed session per fixture via `channel_runtime::open_session`, scores traces, writes reports/metrics, optionally invokes `skill-improver`.
- **`src/main.rs`** (+30 LOC). `tengu align` subcommand dispatches to `AlignRunner`.
- **`src/adapters/channel_runtime.rs`** (+20 LOC) + **`src/adapters/telegram_builder.rs`** (+15 LOC) + **`src/adapters/chat_builder.rs`** (+15 LOC). `/feedback` command handler writes to `memory/feedback.md`; one entry point per channel adapter.
- **`src/adapters/skill_builder.rs`** (+20 LOC). Parse optional `eval:` block from frontmatter into an `EvalConfig` struct (stored on each skill; unused by the runtime, consumed only by `alignment_runner`).

No new traits, no new async machinery. A single optional `[alignment]` block in `config.toml` exposes `align_model_override`, `sandbox_env_allowlist`, `report_dir` — all with sensible defaults.

**Total Rust footprint:** ~280 LOC added. Per-PR LOC numbers in §5 include markdown content (skill bodies, docs) alongside Rust; the Rust-only portion stays ~300.

## 5. Migration plan

| # | PR | What lands | LOC Δ | Risk |
|---|---|---|---|---|
| E1 | Metric schema documented. `skill-creator` body updated to teach `eval:` frontmatter and `evals/*.jsonl` convention. `eval:` parsing in `skill_builder.rs`. | `skill-creator/SKILL.md`, `skill_builder.rs` | +40 | none |
| E2 | `skill-eval` extended with fixture-replay mode. New section in its `SKILL.md`; internally delegates to the same runner as `tengu align`. No runtime change — it is still an on-demand skill. | `skill-eval/SKILL.md`, `alignment_runner.rs` (partial) | +150 | low |
| E3 | `tengu align` CLI + `alignment_runner.rs` complete. Sandbox session spawning via `channel_runtime::open_session`. Report writer, metrics writer, optional `--improve`. | `main.rs`, `alignment_runner.rs`, `channel_runtime.rs` | +200 | med (subagent lifecycle edge cases) |
| E4 | `skills/skill-improver/` authored via `skill-creator`. Proposes patches; writes `proposals/*.md`. Self-hosting fixtures. | new skill dir | 0 Rust | low (content quality) |
| E5 | Backfill `evals/` for the 7 existing skills. Start with deterministic ones (`aura-orchestrator`, `molecule-x402`, `privy-agentic-wallets`, `telegram-rag-ingest`). Best-effort for `beach-science` and the meta-skills (`skill-creator`, `skill-eval`) — trigger-only fixtures where outcomes are subjective. Calibrate thresholds by running align against the current skill and setting targets 5–10 points below observed. | `skills/*/evals/`, `skills/*/SKILL.md` frontmatter | +authoring, 0 Rust | med (authoring cost) |
| E6 | `/feedback` channel command. Parses `@<skill>` prefix; appends to `memory/feedback.md`. Works in CLI and Telegram. | `channel_runtime.rs`, `telegram_builder.rs`, `chat_builder.rs` | +50 | low |

**Cumulative Rust LOC delta:** ~+250. Everything else is content.

**Ordering:** E1 → E2 → E3 ships as a self-contained block (runner is useless without schema; `skill-eval` wraps the runner once it exists). E4 and E5 can ship in parallel after E3. E6 is independent.

## 6. Error handling and edge cases

- **Fixture runs a tool that needs real credentials.** Sandbox inherits only env vars in `[alignment].sandbox_env_allowlist`. Fixtures needing keys the user hasn't granted are skipped with a "no credentials" note in the report. No hard failure.
- **Sandboxed subagent calls a destructive tool.** Authors write fixtures against testnets or mock endpoints; V1 does not stub crypto/HTTP. A follow-up `--mock-network` mode handles the harder cases.
- **Sandbox session times out or loops.** `tengu align` applies `MAX_TOOL_ROUNDS` + a wall-clock fixture timeout (default 2 minutes per fixture, configurable). Timeouts are marked failed with a "timeout" reason.
- **A skill's `eval:` block is malformed.** `skill_builder.rs` parses with `serde(default)`; malformed fields produce a warning but do not prevent the skill from loading at runtime. `align` reports it and skips the skill's fixtures.
- **`skill-improver` produces a non-applicable patch.** User review is the gate. The proposal file is inert — it does not modify the skill. A rejected proposal is just a deleted file.
- **Fixture ID collision across skills.** IDs are scoped per skill. Report rows are keyed by `(skill, fixture_id)`.
- **`MAX_TOOL_ROUNDS` is hit during a fixture.** Fixture fails with a "tool rounds exhausted" reason; does not crash the runner.

## 7. Testing strategy

- **Unit tests**
  - `eval:` block parsing round-trip.
  - Fixture JSONL parsing (malformed lines produce errors).
  - Tool-call pattern matching (args glob, ordered sublist).
  - Outcome-contains case-insensitive matching.
  - Threshold computation (edge cases: zero fixtures, all pass, all fail).

- **Integration tests**
  - Smoke test: one synthetic skill with one fixture; `tengu align` runs it end-to-end; report written; metrics file appended.
  - Sandbox isolation: fixture running `write_file` writes to the sandbox tempdir, not the user's workspace.
  - `/feedback` command writes the expected format to `memory/feedback.md`.

- **Content tests** (authored alongside E5)
  - Each backfilled skill has at least one positive fixture (`expected_skill = self`) and one negative (`expected_skill = null`).
  - Each skill's thresholds are calibrated against observed baseline.

- **Manual QA**
  - Run `tengu align`, inspect a report, deliberately break a skill (e.g., remove a key phrase from the description), re-run align, confirm the regression is caught, run `skill-improver`, apply the proposal, re-run align, confirm metrics recovered.

## 8. Risks and mitigations

| Risk | Mitigation |
|---|---|
| Fixture authoring cost is high; users skip writing them | E1 makes `eval:` optional; schema is friendly; `skill-creator` teaches a minimal first fixture pattern (one trigger, one non-trigger) as a starting point. |
| Fixtures become brittle — a tiny skill change breaks 10 fixtures | Args pattern is glob, not exact. `expected_tool_calls` is an ordered sublist, not a prefix. `outcome_contains` is substring, not exact match. The format favours structural assertions over string equality. |
| Real HTTP/crypto calls from sandboxed fixtures cost money or fire side effects | Authors target testnets and mock endpoints. Allowlist env-var inheritance makes accidental credential leaks unlikely. `--mock-network` is a documented follow-up. |
| `skill-improver`'s proposals are low quality | Proposals are inert until user applies them. Quality is the user's call. Repeat offenders can be manually patched; the meta-skill itself improves through its own eval loop. |
| `tengu align` is slow (many real LLM calls) | Per-skill and per-fixture filters. Rolling metrics let trends be observed without running the whole suite. CI integration is an opt-in, not a default. |
| Drift from real usage — fixtures pass, but real users still hit bad paths | `/feedback` is the backstop. `skill-improver` reads `memory/feedback.md` as a qualitative signal alongside the quantitative metrics. |
| Subjective skills (e.g., `beach-science` — "write a good research post") are hard to score | Trigger-only fixtures are valid. Outcome assertions are optional. Subjective skills declare no `outcome_contains` and still participate in trigger precision/recall. |

## 9. Open questions

- **Should `tengu align` run `skill-improver` automatically on failure?** Landed on explicit `--improve` opt-in to keep the default a pure measurement pass. Revisit once we have a week of real reports.
- **Does `skill-improver` need its own fixtures?** Yes — E5 includes a seed fixture set ("given this failing trace, produce a non-empty proposal with a diff block and a diagnosis"). Self-calibrating.
- **Should proposals ever auto-apply for pure frontmatter changes?** Possible follow-up (tiered auto-apply). Not in Phase E.
- **Does `/feedback` need a multi-line mode?** Probably yes, but V1 treats everything after `/feedback` as one line and ignores embedded newlines. Expand if real usage demands it.

## 10. Dependencies on prior phases

- **Phase A.** `channel_runtime::open_session` must support opening a fresh sandboxed session programmatically. Post-B3 this is already how Telegram/CLI enter the runtime, so the runner uses the same entry point. Sandboxed sessions are "just sessions with a tempdir workspace and a restricted env."
- **Phase B.** `skill-improver` edits the `orchestration` skill the same way it edits any other skill; B having moved orchestration into a skill is exactly what makes this loop coherent. Without B, the orchestration behaviour is still in Rust and the loop cannot touch it.
- **Not dependent on Phase C.** Engine/store/embedder plugins are not needed for alignment. If align runs before C, it uses whichever engine/store is configured.
- **Not dependent on Phase D.** Session RAG is not needed for fixture replay; fixtures are expected to be short.

## 11. Out of scope reminder

- LLM-judge scoring.
- Auto-apply (tiered or otherwise).
- Scheduled align runs inside the binary.
- Cross-skill interaction tests.
- Engine / channel / store changes.
- Live-trace analysis beyond `/feedback`.

---

## 12. Implied deltas to phases A / B / C / D

Phase E is additive, but getting the full vision — lighter, easier, up to date — calls for five small adjustments to the existing specs. Each is a single-paragraph edit in the corresponding spec; none changes scope meaningfully.

### 12.1 Doctrine wording (updates `docs/architecture.md`)

Today the doctrine reads: "Context is the brain / Skills are the logic." Tighten to reflect the intended mental model: **context and skills together are the brain** — skills feed policy into context, Rust assembles context, the LLM runs it. Two-line rewording, no semantic change to the corollary or the Tool Access Control section.

### 12.2 Phase B — promote `skill-cleaner` to critical path

Phase B7 (`skill-cleaner`) is currently optional and parallelizable. Promote it to the Phase B critical path so the skill surface stays small as the alignment loop grows. A system that self-improves must also self-prune; otherwise proposals accumulate against dead skills.

### 12.3 Phase C — drop `ChannelPlugin` from scope

Phase C currently covers `EnginePlugin` + `ChannelPlugin` + `MemoryStorePlugin` + `EmbedderPlugin`. `ChannelPlugin` is speculative — Slack/Discord/Mail are not requested by any user. Dropping it saves ~300 LOC of trait plumbing and documentation. If a third channel is actually requested, reintroduce `ChannelPlugin` in a follow-up phase, using post-B3 CLI and Telegram as the reference shape.

### 12.4 Phase D — migrate more policy into the orchestration skill

Phase D currently adds one paragraph to `skills/orchestration/SKILL.md` covering `context_fetch`. Broaden that paragraph: when to summarize inline vs rely on a `[previously-seen]` block, how aggressive to be about re-fetching, when to fan out multiple fetches. Leave only the 60% / 10× thresholds + chunk size in Rust (pure substrate knobs). This keeps the no-compromise corollary intact even for context-management policy.

### 12.5 Phase D — don't pin model dates in substrate config

Phase D's `summary_model = "claude-haiku-4-5-20251001"` pins a specific snapshot. Parameterize as `summary_model = "claude-haiku-4-5"` (floating) and document the choice in the `[alignment]` docs pass. Same for any other pinned models across the specs. "Up to date" means substrate moves with the model family, not against a date.

---

## 13. What success looks like

After Phase E lands:

- Every skill in `skills/` has an `eval:` block and an `evals/` directory.
- `tengu align` runs clean on a fresh clone; the baseline report shows all thresholds passing.
- Deliberately breaking a skill's description is caught by `tengu align` on the next run. `--improve` produces an applicable `proposals/<skill>-<date>.md`. Applying it restores the baseline.
- `/feedback @<skill> <message>` is a habit; `memory/feedback.md` grows organically.
- The doctrine wording reflects the heart / brain / senses / logic framing in the user's terms.
- Phase A → B → E → C → D is the execution order, with deltas 12.1–12.5 folded into the affected specs.
- Total Rust LOC across A + B + C + D + E: **~−5 000 net** (A −2 400, B −2 450, C −800 (trimmed), D ≈ 0, E +250).
