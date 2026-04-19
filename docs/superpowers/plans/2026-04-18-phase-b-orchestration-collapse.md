# Phase B — Orchestration Collapse Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move orchestration policy out of Rust into a documentation skill. Delete the legacy EventBus orchestrator, the upfront classifier, the Plan/DAG types, and their two callers (Telegram + CLI `tengu orchestrator`). Net Rust delta ≈ −2 450 LOC, 0 Rust added.

**Architecture:** The main agent runs inside `ChatRuntimeService::process_user_text` (already used by both Telegram and CLI's chat loop). The `orchestration` skill injects a playbook into the main agent's system prompt, teaching it when to call `sessions_spawn` / `sessions_fan_out`. No Rust code coordinates multi-agent work after this phase — the LLM does it with the primitives A7 already shipped.

**Tech Stack:** Rust (existing `chat_builder.rs::ChatRuntimeService` + Phase A `plugins/subagents/`), markdown skill authoring, `skill-creator` + `skill-eval` from Phase 0.

**Scope boundaries:**
- **IN:** Author `skills/orchestration/SKILL.md`; migrate Telegram + CLI callers off `event_orchestrator`; delete `event_orchestrator.rs` / `agent_builder.rs` / `task_builder.rs`; delete dead types; trim `OrchestratorConfig.planner_engine`.
- **OUT:** Per-agent scope derivation, `permissive_scope` replacement, async cascade through `build_tool_executor`, per-agent wallet allow-lists. Those `TODO(Phase B)` markers in `channel_runtime.rs` / `ports.rs` / `plugins/crypto/helpers.rs` / `plugins/http/request.rs` are Phase-A carry-over shims. They're mislabeled; this spec (`docs/superpowers/specs/2026-04-15-phase-b-orchestration-collapse-design.md`) does not cover them. A separate plan should schedule them — propose name: `Phase B-async`.

---

## File Structure

**Create:**
- `skills/orchestration/SKILL.md` — documentation skill, playbook body, ~200 lines
- `skills/orchestration/evals/prompts.md` — eval prompt set for `skill-eval`
- (Optional, B7) `skills/skill-cleaner/SKILL.md` + companion script

**Delete (B4):**
- `src/adapters/event_orchestrator.rs` (1 149 LOC)
- `src/adapters/agent_builder.rs` (130 LOC)
- `src/adapters/task_builder.rs` (420 LOC)

**Modify:**
- `src/adapters/telegram_builder.rs` — remove `TelegramTaskExecutor`, `handle_team`, classifier branch in `route_and_chat`. Expected shrink ~400 LOC.
- `src/adapters/orchestrator.rs` — replace plan-and-execute `else` branch with a single `ChatRuntimeService::process_user_text` dispatch. Expected shrink ~80 LOC.
- `src/adapters/types.rs` — delete `OrchestratorEvent`, `Plan`, `PlanTask`, `TaskStatus`, `TaskId`, `RoleDependencies`, `RouteDecision`, `AgentTaskExecutor`. Expected shrink ~200 LOC.
- `src/adapters/config.rs` — drop `OrchestratorConfig.planner_engine` field (keep `enabled` + `max_concurrent` — they're consumed by A7 subagents plugin). Expected shrink ~10 LOC; retain struct.
- `src/adapters/mod.rs` — drop `pub(crate) mod task_builder;` and `pub(crate) mod event_orchestrator;` declarations.

---

## Task B2: Author `orchestration` skill

**Files:**
- Create: `skills/orchestration/SKILL.md`
- Create: `skills/orchestration/evals/prompts.md`

- [ ] **Step 1: Create the skill directory and frontmatter**

```bash
mkdir -p skills/orchestration/evals
```

Write `skills/orchestration/SKILL.md` with exactly the body drafted in the spec §4.2. Start with the frontmatter:

```markdown
---
name: orchestration
description: Use whenever a user request has multiple steps, spans specialist agents, can be parallelized, or requires progress tracking. Teaches decomposition, delegation via sessions_spawn and sessions_fan_out, failure handling, and progress logging. Use this skill even when the user doesn't explicitly ask to "orchestrate" — any request that touches two or more subagents, or has sequential dependencies, triggers this skill.
---

# Orchestration Playbook

This skill teaches you (the main agent) how to decompose user requests, delegate work to subagents, and drive multi-step plans without a central coordinator. You are the orchestrator.
```

Then append all sections verbatim from `docs/superpowers/specs/2026-04-15-phase-b-orchestration-collapse-design.md` §4.2: *When to orchestrate*, *Read your roster first*, *Decomposition*, *Sequential delegation*, *Parallel delegation*, *Failure handling*, *Progress tracking*, *Steering a running subagent*, *When NOT to orchestrate*, *Primitive cheat sheet*.

- [ ] **Step 2: Verify the skill loads**

Run:
```bash
cargo run --features telegram,claude_code -- skills list 2>&1 | rg orchestration
```
Expected: one line containing `orchestration ... documentation`.

If it doesn't appear, check the skill discovery path logic in `src/adapters/skill_builder.rs` — the three-tier hierarchy is `~/.tengu/skills/` → `.tengu/skills/` → `skills/`. The repo-level `skills/` dir should pick it up without extra config.

- [ ] **Step 3: Author the eval prompt set**

Write `skills/orchestration/evals/prompts.md` with the five cases from the spec §7:

```markdown
# Orchestration skill evals

| Prompt | Expected behaviour |
|---|---|
| "research paper X then mint it as an IP token" | Sequential `sessions_spawn(researcher)` then `sessions_spawn(minter)`. |
| "fetch the latest prices for A, B, C" | Single `sessions_fan_out` with three independent requests. |
| "what's 2+2?" | Direct answer. No `sessions_spawn` call. |
| "my trade failed with HTTP 503" | Retry the same call. No decomposition. |
| "the deploy broke, check logs, restart, verify" | Sequential multi-step. At least one `memory_write` call to record progress. |
```

- [ ] **Step 4: Run the eval prompt set**

Automation for this step is specified in `docs/superpowers/specs/2026-04-19-eval-runner-design.md` and shipped on this branch: `tengu eval orchestration` replays the five prompts, scores each row pass/fail via an LLM judge, and writes per-row transcripts to `evals/runs/<ts>/`. Run it and record which prompts pass vs. fail.

If the skill under-triggers (doesn't fire on "research then mint"), edit the `description:` to be more pushy and re-run `tengu eval orchestration`. If it over-triggers on "what's 2+2?", add an explicit `DO NOT fire on single-step arithmetic questions` negative example in the *When NOT to orchestrate* section.

Until the eval runner lands, this step is the manual fallback: open `skills/skill-eval/SKILL.md`, follow its procedure on `skills/orchestration/`, paste each prompt into `tengu orchestrate --sandbox orchestration-eval`, and eyeball the tool-call stream.

- [ ] **Step 5: Commit**

```bash
git add skills/orchestration/
git commit -m "$(cat <<'EOF'
feat(phase-b): B2 — orchestration skill + eval prompt set

Documentation-only skill (no execution template). Frontmatter description
optimized for broad triggering; body teaches decomposition, sequential +
parallel delegation, failure handling, progress tracking. Zero Rust added.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task B3a: Telegram — drop the classifier branch

**Files:**
- Modify: `src/adapters/telegram_builder.rs:1128-1190` (`route_and_chat` classifier branch)
- Modify: `src/adapters/telegram_builder.rs:1019` (the `handle_team` call site in direct-task fast path)

**Goal:** After B3a, every Telegram inbound message goes through `parse_agent_routing` + `ChatRuntimeService::process_user_text`. No classifier; no planner; no `handle_team`.

- [ ] **Step 1: Write the failing parity test**

Add to `tests/telegram_routing.rs` (create if absent):

```rust
//! B3a parity: @role routing preserved without classifier.

use tengu::adapters::channel_runtime::parse_agent_routing;
use std::collections::HashMap;

#[test]
fn at_role_prefix_routes_to_named_agent() {
    let mut role_to_agent = HashMap::new();
    role_to_agent.insert("researcher".to_string(), "agent-1".to_string());
    role_to_agent.insert("minter".to_string(), "agent-2".to_string());

    let (role, text) = parse_agent_routing("@researcher: find paper X", Some(&role_to_agent));
    assert_eq!(role.as_deref(), Some("researcher"));
    assert_eq!(text, "find paper X");
}

#[test]
fn no_at_prefix_returns_none_role() {
    let role_to_agent = HashMap::new();
    let (role, text) = parse_agent_routing("plain user message", Some(&role_to_agent));
    assert_eq!(role, None);
    assert_eq!(text, "plain user message");
}
```

- [ ] **Step 2: Run the test, confirm it passes (parse_agent_routing already works)**

```bash
cargo test --features telegram --test telegram_routing -- --test-threads=1 2>&1 | tail -15
```
Expected: 2 passed. If the symbol isn't exposed, add `pub use channel_runtime::parse_agent_routing;` to the crate root.

- [ ] **Step 3: Remove the classifier branch from `route_and_chat`**

In `src/adapters/telegram_builder.rs`, replace the block starting at `// Multi-agent classifier routing.` (line ~1128) through the end of the classifier match (line ~1190) with a single fallback:

```rust
        // B3: no classifier — fall through to default agent when no @role prefix.
        // The main agent will call the orchestration skill and delegate via
        // sessions_spawn / sessions_fan_out when appropriate.
```

Leave the `routed_role.is_none() && self.is_multi_agent` check in place only if needed to pick the default agent; otherwise delete. The subsequent `// Resolve target agent.` block at line ~1192 already handles `routed_role = None` by falling back to `self.default_agent_id`.

- [ ] **Step 4: Remove the `handle_team` call site at line 1019**

Search for `handle_team(` in `telegram_builder.rs`. There should be 5 call sites after Step 3 (one in the approval flow at line 1019, plus any now-orphaned ones from Step 3's edits). Replace each with a direct call to the single-agent dispatch path (same code path that handles a `@role:` message but using the default agent).

If the logic is identical to the already-existing single-agent dispatch, extract a tiny helper:

```rust
/// Dispatch a user message to a single agent, regardless of routing source.
async fn dispatch_to_agent(
    &mut self,
    agent_id: &str,
    user_text: &str,
    media: Option<&Media>,
    sender_id: &str,
) {
    // move the existing single-agent chat body here
}
```

And have `route_and_chat` call `dispatch_to_agent` after resolving `target_agent_id`.

- [ ] **Step 5: `cargo check --features telegram` passes**

```bash
cargo check --features telegram 2>&1 | tail -10
```
Expected: no errors. Unused-import warnings are fine for now.

- [ ] **Step 6: Run all existing telegram tests**

```bash
cargo test --features telegram telegram -- --test-threads=1 2>&1 | tail -20
```
Expected: all previously-passing tests still pass.

- [ ] **Step 7: Commit**

```bash
git add src/adapters/telegram_builder.rs tests/telegram_routing.rs
git commit -m "$(cat <<'EOF'
feat(phase-b): B3a — Telegram route_and_chat dispatches directly

Removes upfront classify_request call and RouteDecision handling.
@role: prefix still routes via parse_agent_routing. Default messages
go to the default agent; multi-step reasoning now happens via the
orchestration skill + sessions_spawn primitives.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task B3b: Telegram — delete `handle_team` and `TelegramTaskExecutor`

**Files:**
- Modify: `src/adapters/telegram_builder.rs:467-510` (`TelegramTaskExecutor` struct + impl)
- Modify: `src/adapters/telegram_builder.rs:1523-1770` (`handle_team` method)
- Modify: `src/adapters/telegram_builder.rs:1662-1710` (executor construction inside `handle_team`)

- [ ] **Step 1: Delete `handle_team`**

Remove the entire method (`async fn handle_team`) from line ~1523 to ~1770. Any call site should already be gone after B3a Step 4.

- [ ] **Step 2: Delete `TelegramTaskExecutor`**

Remove the `struct TelegramTaskExecutor` + its `impl AgentTaskExecutor for TelegramTaskExecutor` block at lines 467–510.

- [ ] **Step 3: Delete orphaned helpers**

Grep the file for symbols that only `handle_team` used:

```bash
rg -n "bridge_base_tools|planner_engine" src/adapters/telegram_builder.rs
```

Delete fields / builders / parameter forwards that no longer have a consumer. If a field is referenced by exactly one remaining line and that line is dead code from `handle_team`, delete both.

- [ ] **Step 4: `cargo check --features telegram` passes**

```bash
cargo check --features telegram 2>&1 | tail -10
```
Expected: no errors. If a symbol like `OrchestratorConfig` or `OrchestratorEvent` is missing, that's B5 territory — do not patch here. Either comment-out the impossible branch and mark it `// removed in B5` or revert and do B5 first.

- [ ] **Step 5: Verify Telegram unit tests still pass**

```bash
cargo test --features telegram telegram -- --test-threads=1 2>&1 | tail -20
```

- [ ] **Step 6: Commit**

```bash
git add src/adapters/telegram_builder.rs
git commit -m "$(cat <<'EOF'
feat(phase-b): B3b — delete TelegramTaskExecutor + handle_team

Telegram no longer drives the legacy EventBus orchestrator. Multi-agent
coordination is the main agent's job via the orchestration skill.
Behaviour preserved: @role routing, /stop, keyboard approval, file
attachments, secret redaction, typing indicator, /agents, /wallet.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task B3c: CLI `tengu orchestrator` — drop plan-and-execute branch

**Files:**
- Modify: `src/adapters/orchestrator.rs:450-550` (the `else` branch running `prepare_plan` + `execute_plan`)
- Modify: `src/adapters/orchestrator.rs:14` (import of `event_orchestrator`)

The direct-dispatch branch (`role: task`, line 412) stays untouched — it doesn't use `event_orchestrator`.

- [ ] **Step 1: Replace the `else` branch**

Replace lines ~450–~550 (the `} else {` block that builds `plan_engine`, computes `role_deps`, calls `event_orchestrator::prepare_plan`, builds executors, calls `event_orchestrator::execute_plan`, and prints task outputs) with a single dispatch to the default agent's chat loop:

```rust
        } else {
            // B3c: no upfront planner. Route the raw input to the default agent.
            // The orchestration skill in its system prompt teaches it to call
            // sessions_spawn / sessions_fan_out when the request warrants.
            let Some(ref default_id) = planner_agent_id else {
                println!("  No default agent configured.");
                continue;
            };
            let runtime = agent_runtimes.get(default_id).unwrap();
            match execute_agent_task(runtime, &input, &secret_registry).await {
                Ok((output, _)) => {
                    println!();
                    println!("  {}", output);
                    println!();
                    task_counter += 1;
                    let task_id = format!("task-{}", task_counter);
                    task_history.record(task_id.clone(), input.clone(), "default".to_string());
                    task_history.complete(&task_id);
                }
                Err(e) => {
                    println!("  Failed: {}", e);
                }
            }
        }
```

- [ ] **Step 2: Drop the import of `event_orchestrator`**

Change line 14 from:
```rust
use crate::adapters::event_orchestrator::{self, OrchestratorConfig};
```
to:
```rust
use crate::adapters::config::OrchestratorConfig;
```

(Or whichever module `OrchestratorConfig` will live in after B6 — adjust in that task if the path changes.)

- [ ] **Step 3: Drop now-unused imports**

Run `cargo check` and let the compiler list unused imports (`RoleDependencies`, `AgentTaskExecutor`, `TaskStatus`, `AgentRuntimeExecutor` if it was local). Remove them one by one.

```bash
cargo check 2>&1 | rg "unused import"
```

- [ ] **Step 4: `cargo check` passes**

```bash
cargo check 2>&1 | tail -10
```
Expected: no errors.

- [ ] **Step 5: Smoke-run `tengu orchestrator` with a direct dispatch**

```bash
cargo run -- orchestrator 2>&1 <<<'/quit' | head -20
```
Expected: agent fleet prints, `>` prompt appears, `/quit` shuts down cleanly. No `event_orchestrator` log lines.

- [ ] **Step 6: Commit**

```bash
git add src/adapters/orchestrator.rs
git commit -m "$(cat <<'EOF'
feat(phase-b): B3c — CLI orchestrator dispatches to default agent

Drops the plan-and-execute branch that called event_orchestrator's
prepare_plan + execute_plan. Multi-step CLI runs now go through the
default agent's chat loop, which has the orchestration skill.
Direct 'role: task' fast-path unchanged.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task B4: Delete `event_orchestrator.rs`, `agent_builder.rs`, `task_builder.rs`

**Files:**
- Delete: `src/adapters/event_orchestrator.rs`
- Delete: `src/adapters/agent_builder.rs`
- Delete: `src/adapters/task_builder.rs`
- Modify: `src/adapters/mod.rs:14,20` (module declarations)

- [ ] **Step 1: Verify no external callers remain**

```bash
rg -n "event_orchestrator|agent_builder::|task_builder::|classify_request|run_orchestrator|prepare_plan|execute_plan" src/ tests/ | rg -v "^src/adapters/(event_orchestrator|agent_builder|task_builder)\.rs:"
```
Expected: no output. If there are matches, they're unmigrated callers — go back to B3.

- [ ] **Step 2: Delete the three files**

```bash
rm src/adapters/event_orchestrator.rs src/adapters/agent_builder.rs src/adapters/task_builder.rs
```

- [ ] **Step 3: Drop module declarations**

Edit `src/adapters/mod.rs` — remove these two lines:
```rust
pub(crate) mod task_builder;
pub(crate) mod event_orchestrator;
```

(Check for `pub(crate) mod agent_builder;` as well — grep confirmed it isn't listed at lines 14 or 20, but verify before committing.)

- [ ] **Step 4: `cargo check --all-features` passes**

```bash
cargo check --all-features 2>&1 | tail -20
```
Expected: no errors. There will be warnings from B5 territory (unused types in `types.rs`) — that's fine.

- [ ] **Step 5: Commit**

```bash
git add -u
git commit -m "$(cat <<'EOF'
feat(phase-b): B4 — delete legacy EventBus orchestrator

Removes event_orchestrator.rs (1149 LOC), agent_builder.rs (130 LOC),
task_builder.rs (420 LOC). ~1700 LOC net shrink. Callers all migrated
in B3a/B3b/B3c.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task B5: Delete dead types from `types.rs`

**Files:**
- Modify: `src/adapters/types.rs` — remove these top-level items:
  - `pub(crate) type TaskId = String;` (line ~309)
  - `pub(crate) enum OrchestratorEvent` (line ~341; ~95 LOC)
  - `pub(crate) trait AgentTaskExecutor` (line ~437)
  - `pub(crate) enum TaskStatus` (line ~527)
  - `pub(crate) struct Plan` (line ~565)
  - `pub(crate) enum RouteDecision` (line ~770)
  - `pub(crate) struct PlanTask` (line ~778)
  - `pub(crate) type RoleDependencies = ...` (grep for `RoleDependencies`)
  - Any companion `impl` blocks on the above

- [ ] **Step 1: Grep current call sites before deletion**

```bash
rg -n "OrchestratorEvent|AgentTaskExecutor|TaskStatus|\bPlan\b|PlanTask|RouteDecision|RoleDependencies|\bTaskId\b" src/ tests/
```
Capture the output. Every match outside `src/adapters/types.rs` is a caller that must also change.

- [ ] **Step 2: Delete each item, run `cargo check` after each**

Don't batch — delete one item, run `cargo check`, let the compiler tell you the next dependency. This is safer than "delete everything and hope."

```bash
cargo check --all-features 2>&1 | rg "^error" | head -20
```

For each remaining error, either the item is still in use (B3 left a straggler — fix that) or it's in a dead test that also should go.

- [ ] **Step 3: Delete any now-unused imports**

```bash
cargo check --all-features 2>&1 | rg "unused import" | head -20
```

- [ ] **Step 4: `cargo check --all-features` clean**

```bash
cargo check --all-features 2>&1 | tail -5
```
Expected: no errors, no warnings beyond pre-existing ones.

- [ ] **Step 5: Commit**

```bash
git add src/adapters/types.rs src/adapters/
git commit -m "$(cat <<'EOF'
feat(phase-b): B5 — delete legacy orchestration types

Removes OrchestratorEvent, Plan, PlanTask, TaskStatus, TaskId,
RoleDependencies, RouteDecision, AgentTaskExecutor. ~200 LOC.
Nothing references them after B4.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task B6: Config cleanup — drop `OrchestratorConfig.planner_engine`

**Files:**
- Modify: `src/adapters/config.rs:421-448` (the `OrchestratorConfig` struct + `Default` impl + helper)

**Important:** the struct stays. `enabled` and `max_concurrent` are consumed by the A7 subagents plugin. Only the `planner_engine` field goes — it was for the deleted upfront classifier.

- [ ] **Step 1: Remove the field from the struct**

Edit `src/adapters/config.rs`. In `pub struct OrchestratorConfig`, delete the `pub planner_engine: Option<String>,` field (and its preceding doc comment / `#[serde(default)]` attribute).

- [ ] **Step 2: Remove from `Default` impl**

Delete the corresponding line in `impl Default for OrchestratorConfig { fn default() { Self { ..., planner_engine: None, } } }`.

- [ ] **Step 3: Grep for any remaining reference**

```bash
rg -n "planner_engine" src/ tests/
```
Expected: no output. If telegram_builder.rs or orchestrator.rs still reference it, those were missed in B3 — fix there, not here.

- [ ] **Step 4: `cargo check --all-features` passes**

```bash
cargo check --all-features 2>&1 | tail -5
```

- [ ] **Step 5: Update example configs**

```bash
rg -n "planner_engine" config.example.toml .env.example agents/ 2>/dev/null
```
Remove any occurrences from shipped config samples so nobody copy-pastes a dead key.

- [ ] **Step 6: Commit**

```bash
git add src/adapters/config.rs config.example.toml
git commit -m "$(cat <<'EOF'
feat(phase-b): B6 — drop OrchestratorConfig.planner_engine

Field powered the deleted classify_request. enabled + max_concurrent
stay — A7 subagents plugin still consumes them.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task B7 (optional): `skill-cleaner`

**Not on the Phase B critical path.** Author after B6 merges. Follow `skills/skill-creator/SKILL.md` procedure.

**Files:**
- Create: `skills/skill-cleaner/SKILL.md` (~150 lines)
- Create: `skills/skill-cleaner/scripts/audit.sh` or `.py` (~100 lines)

**Behaviour:** walk the three-tier skill hierarchy, cross-reference against `memory/YYYY-MM-DD.md` daily logs, flag skills unused for >30 days, archive flagged skills to `archive/` (opt-in via `--archive` flag; default is report-only).

- [ ] **Step 1:** Sandbox-default is non-destructive. `--archive` is explicit.
- [ ] **Step 2:** Cross-reference against daily logs via `rg -l` on skill names.
- [ ] **Step 3:** Config-driven staleness threshold (default 30 days).
- [ ] **Step 4:** Produce a markdown report with columns: skill, last-use date, recommendation.
- [ ] **Step 5:** Commit.

---

## Verification gate (before merge to main)

- [ ] `cargo check --all-features` clean.
- [ ] `cargo test --all-features -- --test-threads=1` — no regression. Cap wall time at 30s per feedback memory.
- [ ] Manual smoke: `tengu telegram` — send `@researcher: hello`, expect routing to researcher agent. Send a plain message, expect default agent. Send `/stop` mid-run, expect clean cancellation.
- [ ] Manual smoke: `tengu orchestrator` — direct dispatch `role: task` still works; bare input runs through the default agent (which calls the orchestration skill as needed).
- [ ] Parity matrix from spec §5.1: keyboard approval, `/wallet`, file upload, secret redaction all still fire.
- [ ] `git diff main --stat` — net removal should be ≈ −2 400 LOC excluding skill content.
- [ ] Run the orchestration skill eval prompt set — five prompts, ≥4/5 match expected behaviour.

---

## Self-review (run after plan is saved; fix inline)

1. **Spec coverage:**
   - §4.2 orchestration skill → B2 ✓
   - §4.5 Telegram migration → B3a + B3b ✓
   - §4.6 type cleanup → B5 ✓
   - §5 migration plan B2–B6 → B2, B3a, B3b, B3c, B4, B5, B6 all present ✓
   - §4.3 `skill-creator` already in Phase 0 — no task needed ✓
   - §4.4 `skill-cleaner` → B7 (optional) ✓
   - §7 testing strategy → verification gate ✓
   - **Gap: spec did not mention CLI's `event_orchestrator` caller — plan adds B3c to cover it.** Documented at top.
   - **Gap: spec §4.6 said "no replacements needed" but `OrchestratorConfig` must survive (A7 uses `enabled` + `max_concurrent`) — plan narrows B6 to drop only `planner_engine`.** Documented at top.

2. **Placeholder scan:** no TBD / TODO / "add appropriate ..." / "similar to task N" found. All code blocks are complete.

3. **Type consistency:** `ChatRuntimeService::process_user_text` used consistently; `parse_agent_routing` (existing symbol) used consistently; `event_orchestrator` symbols all scheduled for deletion in the same task that removes their module. `OrchestratorConfig` survives B6 with two fields — consistent with A7's usage of `enabled` + `max_concurrent`.

4. **Scope drift check:** OUT-of-scope items (per-agent scopes, async cascade) flagged at the top and **not** added as tasks. They need a separate `Phase B-async` plan.
