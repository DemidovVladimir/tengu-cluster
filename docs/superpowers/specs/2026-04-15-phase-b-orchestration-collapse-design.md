# Phase B — Orchestration Collapse (skill-driven)

**Status:** Design, pending approval
**Date:** 2026-04-15
**Depends on:** Phase A (tool plugin architecture) must be complete and landed
**Blocks:** Phase C (engine/channel/store plugins)
**Supersedes:** the earlier Rust-plugin version of Phase B — the `TasksPlugin` + `PlanPlugin` approach is dropped in favour of moving orchestration into a skill.

---

## 1. Problem

`tengu-cluster` has **two parallel orchestration architectures** in the same codebase:

**Path 1 — legacy EventBus (Telegram channel only):**
- `event_orchestrator.rs` (1 149 LOC) — central event loop, `Plan` state machine, DAG dispatch, retry + cascade-skip, Tier 1/2 data routing, global token budgeting, structured-output regex extraction.
- `agent_builder.rs` (130 LOC) — per-agent worker loop with mpsc inbox and `TaskAssignment` → `TaskCompletion` / `TaskError` event translation.
- `task_builder.rs` (420 LOC) — upfront LLM request classifier (`classify_request`), plan construction, DAG validation.
- `OrchestratorEvent`, `Plan`, `PlanTask`, `TaskStatus`, `RoleDependencies`, `RouteDecision`, `AgentTaskExecutor` types in `types.rs` (~200 LOC).
- Wired through `telegram_builder.rs::TelegramTaskExecutor`.

**Path 2 — LLM-driven subagent spawning (CLI and everywhere else):**
- `orchestrator.rs` (629 LOC) — boots agents, builds the tool registry, runs the main agent's chat loop.
- `subagent_builder.rs` — `AgentRuntime`, `SubagentRegistry`, `sessions_spawn` / `sessions_fan_out` / `subagents` tools. The main agent gets the team roster in its system prompt and decides when to delegate.

Path 2 is strictly simpler, proven on the CLI, and matches how every modern LLM harness (Claude Code, Cursor, Anthropic's Managed Agents) has converged. Path 1 exists only because Telegram was built first and nobody has migrated it. The duplication burns ~1.9 k LOC of control-plane code that re-implements what LLM tool-calling already does, produces two sets of types, and silently diverges — a bug fixed on one path will not appear on the other.

## 2. Goal

**0. Implements the "logic" principle of `docs/architecture.md`** (see `2026-04-15-phase-0-doctrine-design.md`): all orchestration policy moves out of Rust and into `skills/orchestration/`. The `skill-creator` + `skill-eval` meta-loop (installed in Phase 0) is what makes this evolvable — the user can re-author or measure the orchestration skill without filing a Rust PR. The no-compromise corollary applies: if this phase is tempted to add Rust code that encodes a retry, delegation, or decomposition strategy, that code is a skill, not Rust.

**Move orchestration out of Rust and into a skill.** The Rust core keeps only the primitives (`sessions_spawn`, `sessions_fan_out`, `subagents`, `memory_write`, `memory_get`, etc.) and becomes a thin substrate for skill-driven behaviour. The *policy* for how to decompose requests, delegate to subagents, handle failures, and track progress moves into `skills/orchestration/SKILL.md`, which is loaded through the existing `SkillCatalog` (Phase A §4.6) and injected into every main-agent system prompt.

**Install meta-tooling so orchestration evolves without code changes.** Copy `skill-creator` from `anthropics/skills` (verbatim, per the project's "skills are portable, never modified by the platform" rule) and use it to author the `orchestration` skill and a new `skill-cleaner` skill. `skill-creator` itself covers create / modify / eval / benchmark, so the meta-skill set collapses to two skills, not four.

**Expected Rust LOC delta: ~−2 450 (B3 −400 + B4 −1 700 + B5 −200 + B6 −150). New Rust code: ≈ 0.** The playbook is prose, not code.

## 3. Non-goals

- **New Rust features.** Phase B is a deletion + skill authoring exercise. Any new capability that tempts us toward adding Rust code is out of scope.
- **Hard DAG enforcement.** Confirmed with the user: no current workflow needs a state-machine guarantee that task B cannot start until task A completes. If such a workflow appears later, a narrow Rust `plan_tool` can be added for *that* workflow — not pre-built speculatively.
- **Telegram adapter rewrite.** The adapter stays; only its orchestration path changes. Multi-agent routing (`@role: message`), inline-keyboard approvals, `/stop` cancellation, `/wallet`, file attachments, secret redaction, typing indicator, `/agents` — all preserved.
- **Channel plugin trait.** That is Phase C. However, B3 is **deliberately shaped** so Phase C can lift Telegram + CLI into a `ChannelPlugin` trait with zero rewriting — see §11.1.
- **CLI changes.** Path 2 is already the target; the CLI barely moves.
- **Engine / channel / store plugins.** Phase C.
- **Skill-creator installation.** Moved to Phase 0 (see `2026-04-15-phase-0-doctrine-design.md` §3). Phase B assumes `skill-creator` and `skill-eval` are already on disk.

## 4. Architecture

### 4.1 Rust surface after Phase B

The Rust core exposes exactly these tool primitives (grouped under Phase A plugins — see the A spec for the plugin layout):

- **Workspace**: `read_file`, `list_directory`, `write_file`, `run_command`
- **Network**: `http_request`
- **Crypto**: `sign_and_send_transaction`, `sign_message`, `get_wallet_address`, `abi_encode`
- **Memory**: `remember`, `memory_write`, `memory_get`, `memory_search`
- **Sessions**: `sessions_list`, `sessions_send`, `sessions_history`
- **Subagent spawning**: `sessions_spawn`, `sessions_fan_out`, `subagents` (list/kill/steer)
- **Shared cache**: `shared_cache` (opt-in per agent)
- **MCP bridge**: arbitrary external tools declared outside the binary via MCP

Plus the skill loader, the engine, and the channel layer. **No orchestration tools live in Rust after Phase B.** `plan_*`, `tasks_*`, `classify_*`, `dispatch_*` — none of them exist as tools; all their behaviour is instructed via the orchestration skill.

This matches other modern harnesses that expose generic tool-calling + MCP for custom tools and leave policy to skills and system prompts.

### 4.2 The `orchestration` skill

A documentation skill (no execution template; no tools generated). Loaded by `SkillPlugin` (Phase A §4.6) into `SkillCatalog`; its compact XML catalog entry is injected into every main-agent system prompt via the engine's prompt assembly. The full `SKILL.md` body is pulled on-demand when the main agent decides the skill applies, following the three-tier progressive-disclosure pattern (~100 tokens catalog, <500 lines body, optional bundled references/scripts).

**Draft `skills/orchestration/SKILL.md`** — concrete starting point for implementation:

```markdown
---
name: orchestration
description: Use whenever a user request has multiple steps, spans specialist agents, can be parallelized, or requires progress tracking. Teaches decomposition, delegation via sessions_spawn and sessions_fan_out, failure handling, and progress logging. Use this skill even when the user doesn't explicitly ask to "orchestrate" — any request that touches two or more subagents, or has sequential dependencies, triggers this skill.
---

# Orchestration Playbook

This skill teaches you (the main agent) how to decompose user requests, delegate work to subagents, and drive multi-step plans without a central coordinator. You are the orchestrator.

## When to orchestrate

Trigger this playbook when any of the following apply:

- The request has two or more distinct sub-tasks (e.g. "research X, then mint Y").
- A sub-task needs specialist knowledge that matches a team member in your roster.
- Sub-tasks are independent and can run in parallel.
- The work is long enough that the user benefits from progress updates.
- A previous attempt failed and you need a different decomposition.

If the request is a single-step, single-specialty question, **don't orchestrate — answer directly.**

## Read your roster first

Your system prompt lists the team members you can delegate to. Each has a role key, a description, and capabilities. Read the roster carefully before deciding how to decompose. If no specialist matches a sub-task, handle it yourself.

## Decomposition

1. Read the full request before planning anything.
2. Sketch the decomposition: what needs to happen, in what order, which tasks are independent.
3. For each sub-task, pick the team member whose description matches best, or keep it for yourself.
4. Identify which sub-tasks can run in parallel (no output dependency) and which must be sequential.

## Sequential delegation

When step N+1 needs step N's output:

- Call `sessions_spawn(agent="<role>", prompt="<focused prompt>")` and await the result.
- The subagent's output lands in your context. Read it.
- When composing the next spawn's prompt, embed only the **relevant** parts of the previous output. Don't forward everything — long outputs waste context.
- If the previous output is large and only a fraction matters downstream, summarize it yourself before embedding.

## Parallel delegation

When sub-tasks are independent:

- Call `sessions_fan_out(requests=[{agent, prompt}, ...])` with all independent sub-tasks at once.
- Fan-out returns combined results. Read them and decide next.
- Prefer fan-out whenever it cuts wall-clock time and subagents don't need each other's outputs.

## Failure handling

- When a spawned subagent returns an error, read the message and decide:
  - **Retry** if the failure looks transient (rate limit, timeout, HTTP 5xx).
  - **Switch strategy** if the failure is structural (wrong specialist, missing data).
  - **Abandon** if further work is impossible or unsafe.
- When a tool inside your own turn fails, apply the same reasoning.
- If one sub-task fails and downstream work depends on it, reason explicitly about whether the downstream step still makes sense. If not, explain the cascade to the user.

## Progress tracking

- For multi-step work taking more than ~30s of wall-clock time, write a short status note via `memory_write` under `memory/YYYY-MM-DD.md`.
- Record identifiers (tx hashes, addresses, UUIDs, doc ids) in the daily log so they survive across turns.
- Do not hide progress behind silence.

## Steering a running subagent

If a subagent is running and you realize you gave wrong instructions, call `subagents steer <id> <message>` to send mid-run guidance. Don't wait for it to finish if you know it's off track.

## When NOT to orchestrate

- Single-step requests — answer directly.
- Conversation, clarification, small talk — respond directly.
- Requests explicitly about your own opinion or synthesis — don't delegate.

## Primitive cheat sheet

| Tool | Use for |
|---|---|
| `sessions_spawn` | Sequential delegation; await result |
| `sessions_fan_out` | Parallel delegation; independent tasks in one call |
| `subagents list/steer/kill` | Inspect or correct running subagents |
| `sessions_list/send/history` | Coordinate with existing long-lived sessions |
| `memory_write` | Record progress and identifiers in the daily log |
```

The description is deliberately "pushy" per skill-creator's guidance to combat under-triggering. The body stays under 500 lines and uses only primitives that already exist in Rust today — no new tool, no new plumbing.

### 4.3 `skill-creator` (already installed in Phase 0)

**Moved to Phase 0.** See `2026-04-15-phase-0-doctrine-design.md` §3. Phase B starts with `skill-creator` already on disk, hash-pinned against upstream, and with the companion `skill-eval` skill available. Phase B's work in this area is now content-only: author `skills/orchestration/SKILL.md` using the already-installed `skill-creator` as a co-author in the implementation session.

The portability constraint (`skills are portable, must never be modified by the platform`) is enforced by the Phase 0 CI hash check; Phase B inherits that guarantee.

### 4.4 `skill-cleaner` (new, authored via skill-creator)

No upstream equivalent — we author it as a documentation skill with a small bundled script. Purpose:

- Walk the three-tier skill hierarchy.
- For each installed skill, cross-reference against recent usage (via daily logs in `memory/YYYY-MM-DD.md` and the activity publisher's tool-call history).
- Flag skills that haven't been triggered in N days (configurable; default 30) as candidates for archival.
- Optionally move flagged skills to an `archive/` subdir rather than deleting, so they can be restored if needed.
- Produce a report listing flagged skills, last-use date, and archival recommendation.

Authoring flow: use `skill-creator` in an implementation session to co-create `skills/skill-cleaner/SKILL.md` and its companion script. `skill-cleaner` ends up as a ~150-line SKILL.md + a ~100-line Python or bash script invoked through `run_command`. It does not execute automatically — it runs when the user or the main agent explicitly triggers it.

### 4.5 Telegram channel migration

`telegram_builder.rs::TelegramTaskExecutor` is removed. The Telegram channel becomes a thin layer over `channel_runtime.rs` — the same runtime the CLI uses:

```rust
// Before (simplified):
async fn on_message(msg) {
    let plan = build_plan(msg).await?;                // task_builder.rs
    let (tx, rx) = mpsc::channel();
    spawn_workers(plan, tx.clone(), executor).await;  // agent_builder.rs
    run_orchestrator(plan, rx, tx, cfg).await?;       // event_orchestrator.rs
}

// After:
async fn on_message(msg) {
    let (agent, instruction) = parse_at_mention(&msg);                   // ~20 LOC, in telegram adapter
    let session = channel_runtime.open_session(&chat_id, agent).await?;
    session.send_user_message(instruction).await?;                       // main agent runs; may spawn subagents
    session.stream_output(|chunk| telegram_send(chat_id, chunk)).await?;
}
```

`parse_at_mention` handles `@researcher: do X` by starting the session with the `researcher` agent instead of the default main agent — that replaces `classify_request` entirely. All other Telegram channel behaviour is preserved (see §3 non-goals).

### 4.6 Type cleanup in `types.rs`

Removed:
- `OrchestratorEvent` enum (all 7 variants)
- `Plan`, `PlanTask`, `TaskStatus`, `TaskId`
- `RoleDependencies`
- `RouteDecision`
- `AgentTaskExecutor` trait

No replacements added. The state that orchestration needs now lives in the LLM's message history plus the daily log — not in Rust types.

## 5. Migration plan

Strictly sequential. Phase B cannot start until Phase A has shipped through A9 (the `SkillCatalog` path must exist so the orchestration skill's frontmatter reaches the main-agent system prompt).

| # | PR | What lands | LOC Δ | Risk |
|---|---|---|---|---|
| B1 | **[moved to Phase 0 — see P0-5 / P0-6]** `skill-creator` and `skill-eval` are already installed when Phase B starts. | — | — | — |
| B2 | Author `skills/orchestration/SKILL.md` using the already-installed `skill-creator` (content matches the draft in §4.2, refined through `skill-eval`'s feedback loop). | `skills/orchestration/` | 0 Rust | low (content quality) |
| B3 | Telegram migration — replace `TelegramTaskExecutor` dispatch with `channel_runtime` session. Add `parse_at_mention`. Preserve keyboard, `/stop`, attachments, redaction. | `telegram_builder.rs` | −400 | **high** (parity) |
| B4 | Delete `event_orchestrator.rs`, `agent_builder.rs`, `task_builder.rs`. | 3 files | −1 700 | low (all callers migrated in B3) |
| B5 | Delete dead types from `types.rs`. | `types.rs` | −200 | low (leaf module; compilation catches strays) |
| B6 | Config cleanup — remove legacy orchestration config structs from `config.rs`. | `config.rs` | −150 | low |
| B7 (follow-up, optional) | Author `skills/skill-cleaner/` via `skill-creator`. Not on the Phase B critical path; can land after B6. | `skills/skill-cleaner/` | 0 Rust | none |

**Cumulative Rust LOC delta:** approximately −2 450 removed, ≈ 0 added. The dominant value comes from B3–B6. B1, B2, B7 are content work, not Rust.

### 5.1 The B3 risk in detail

B3 is the only step where runtime behaviour can drift. Mitigations:

1. **Feature-flag gate.** A new `telegram_use_channel_runtime` flag defaults to `true`; can be flipped to `false` for one release to revert. Removed in the release after.
2. **Parity test matrix.** For each legacy Telegram scenario, a smoke test exercises the new path:
   - `@researcher: do X` routes to the researcher agent.
   - A multi-step request causes the main agent to spawn subagents (fan-out observable in logs).
   - `/stop` cancels an in-flight run cleanly.
   - Inline-keyboard approval gates a crypto transaction.
   - File upload is attached to the main agent's context.
   - Secret redaction fires on a fake API key in an LLM response.
3. **Staged rollout.** B3 ships on a branch and runs against a test bot for 48 h before merge.

B4–B6 are pure deletions of already-dead code once B3 ships.

### 5.2 Ordering rationale

- B1 is a no-op row (moved to Phase 0); `skill-creator` and `skill-eval` are on disk before Phase B starts.
- B2 before B3: the orchestration skill must be in place so the main agent, when Telegram sessions start, has the playbook in its system prompt.
- B3 before B4: Telegram must stop calling the legacy orchestrator before we can delete it.
- B4 before B5: dead-type cleanup needs the files that reference the types to be gone first.
- B7 optional and parallelizable: skill-cleaner is a quality-of-life addition, not a dependency.

## 6. Error handling and edge cases

- **Subagent failure.** The orchestration skill (§4.2 "Failure handling") instructs the main agent how to react. No central retry coordinator. The `is_retryable()` heuristic from `agent_builder.rs` is not replaced — the main LLM makes the retry decision with full context, which is strictly more informed than a regex match on error strings.
- **Global token budget.** Per-session counter already tracked in `channel_runtime.rs`. Phase A §4.1 plumbs subagent token usage back through the activity publisher, so the session counter aggregates across the whole tree. No change needed in Phase B.
- **Cancellation.** `/stop` flips a `CancellationToken` owned by the channel session. `sessions_spawn` and `sessions_fan_out` accept the token (Phase A `ToolCtx` carries it) and propagate it into subagents. One uniform cancellation model.
- **Structured output extraction (`RESEARCH_OUTPUT:` regex).** Deleted — not replaced. The main agent reads raw subagent output and picks what matters for the next step. This was a workaround for the legacy path's inability to let one agent read another's full output.

## 7. Testing strategy

- **Unit tests** — n/a for skill content beyond frontmatter-parse tests (Phase A's `SkillPlugin` already has those).
- **Skill eval** — use `skill-creator`'s eval loop to test the `orchestration` skill against a fixed prompt set (produced during B2):
  - "research paper X then mint it as an IP token" → expect sequential spawn(researcher) + spawn(minter).
  - "fetch the latest prices for A, B, C" → expect parallel fan-out.
  - "what's 2+2?" → expect direct answer, no spawn.
  - "my trade failed with HTTP 503" → expect retry of the same call, not decomposition.
  - "the deploy broke, check logs, restart, verify" → expect sequential multi-step with progress notes in the daily log.
- **Telegram parity matrix** (§5.1).
- **Regression sweep** — CLI smoke tests must produce byte-identical output before and after Phase B on fixed prompts + seeds (CLI path does not change; any drift is a regression).
- **Test-bot 48 h run** for B3.

## 8. Risks and mitigations

| Risk | Mitigation |
|---|---|
| Orchestration skill under-triggers; main agent fails to delegate when it should | skill-creator's description-improver script optimizes the frontmatter `description` for triggering accuracy. Re-run after any skill content change. |
| Orchestration skill over-triggers; main agent delegates trivial one-step questions | Eval set includes explicit "don't orchestrate" cases (e.g. "what's 2+2?"). skill-creator's variance analysis catches over-triggering. |
| Telegram behavioural parity — legacy path had subtle ordering guarantees (e.g. failures always before summary) | Parity test matrix in §5.1; explicit ordering contract documented in `channel_runtime.rs::Session::stream_output`. |
| Users relied on implicit Tier 2 summarization of long outputs | No auto summarizer in Phase B. If real-world use reveals a gap, the orchestration skill tells the main agent to summarize inline — or, failing that, add an explicit `summarize` tool. No hidden behaviour restored. |
| `classify_request` routing was doing something non-obviously useful | Unlikely — `@role:` is preserved via `parse_at_mention`. For implicit routing, the main agent has the full team roster in its system prompt and decides per-message with full context. At worst one added round-trip; at best more accurate. |
| Deleting types in `types.rs` breaks unrelated code that imported them | Compilation catches it. `types.rs` is a leaf module. |
| Skills drift from upstream skill-creator | CI check on B1 verifies the `skill-creator` hash matches upstream. An upstream update triggers a re-copy PR, not an inline edit. |
| Hard DAG enforcement required by a future workflow | Confirmed not needed today. If it appears, add a narrow `plan_tool` for that specific workflow — don't pre-build speculatively. |

## 9. Open questions

- **Should `orchestration` skill recommend writing a daily-log entry on every run, or only when wall-clock exceeds a threshold?** Leaning on "only when > 30s" to keep the log clean. Tunable via skill content — no code change.
- **Does `skill-cleaner` need a sandbox mode (report only, never archive)?** Probably yes as the default; destructive action requires explicit user confirmation. Decide during B7 authoring.
- **Should we install other useful upstream skills as part of B1 (e.g. `doc-coauthoring`, `mcp-builder`)?** Out of scope for Phase B; can be a follow-up content PR.

## 10. Dependencies on Phase A

Phase B cannot start without:
- `ToolPlugin` trait and `ToolRegistry` (from A0).
- `SubagentsPlugin` exposing `sessions_spawn`, `sessions_fan_out`, `subagents` as first-class tools (from A7).
- `ToolCtx` carrying `&SubagentRegistry`, cancellation, and activity publisher (from A0).
- `SkillPlugin` + `SkillCatalog` — the documentation-skill loader that injects the orchestration skill's frontmatter into the main-agent system prompt (from A8).
- `channel_runtime.rs` wired through `PluginRegistry` (from A0).

Without those, the `orchestration` skill has nothing to plug into and Telegram's new path has no uniform tool registry to call through.

## 11. What the Rust core looks like after Phase B

Approximate line counts (adapters/ only, excluding tests):

| Module | Before Phase B | After Phase B |
|---|---:|---:|
| `event_orchestrator.rs` | 1 149 | — (deleted) |
| `agent_builder.rs` | 130 | — (deleted) |
| `task_builder.rs` | 420 | — (deleted) |
| `telegram_builder.rs` | 2 255 | ~1 850 |
| `types.rs` | 918 | ~720 |
| `config.rs` | 1 013 | ~860 |
| `orchestrator.rs` | 629 | 629 |
| `subagent_builder.rs` | existing | unchanged + slimmed by Phase A |
| `channel_runtime.rs` | 676 | ~700 (small growth for Telegram session path) |
| **adapters/ total** | ~15 000 (pre Phase A) | ~10 000 (post Phase A + B) |

And the skills tree:

| Path | Status |
|---|---|
| `skills/orchestration/` | new (§4.2) |
| `skills/skill-creator/` | new — verbatim upstream copy (§4.3) |
| `skills/skill-cleaner/` | new — authored via skill-creator (§4.4), B7 |
| `skills/aura-orchestrator/` | existing |
| `skills/beach-science/` | existing |
| `skills/molecule-x402/` | existing |
| `skills/privy-agentic-wallets/` | existing |
| `skills/telegram-rag-ingest/` | existing |

### 11.1 Why this unblocks channel pluggability

Today Telegram is structurally coupled to the legacy orchestrator — `telegram_builder.rs` reaches into `event_orchestrator.rs`, owns its own `TelegramTaskExecutor`, and bypasses the session lifecycle that CLI goes through. The two channels share almost no shape. That coupling is the blocker for making channels pluggable.

B3 breaks it: after the migration, **Telegram and CLI both drop through `channel_runtime.rs` via the same `open_session` → `send_user_message` → `stream_output` flow**. They become indistinguishable in shape, differing only in how they read input (Telegram API webhook vs stdin) and how they stream output (Telegram `sendMessage` vs terminal write).

Once they're shaped the same way, Phase C's `ChannelPlugin` trait is **pure extraction**, not rewriting. The trait (sketched in the Phase C stub spec) looks like:

```rust
#[async_trait]
pub(crate) trait ChannelPlugin: Send + Sync {
    fn name(&self) -> &'static str;
    async fn start(&self, ctx: &ChannelCtx<'_>) -> Result<()>;
}
```

and `CliChannel` + `TelegramChannel` each move about 50 lines of their existing entry-point code into a `start()` implementation. No code is rewritten; it's moved.

This means that adding future messengers — Slack, Discord, Matrix, SMTP/IMAP mail — becomes a self-contained per-channel PR in Phase C or after:

- Write `src/adapters/plugins/channels/slack.rs` (~150 LOC using a crate like `slack-morphism` or `serenity` for Discord).
- Implement `ChannelPlugin::start` — parse incoming events, call `runtime.open_session(...)`, call `session.send_user_message(...)`, stream tokens back through the messenger's native API.
- Add the name to `PluginRegistry::channel_from_name` match arm.
- Add a `[[channels]] name = "slack"` entry in `config.toml`.

Four edits, one file, no touching existing channels. **That's the trajectory B3 is explicitly paving the way for.** See `2026-04-15-phase-c-engine-channel-store-plugins-design.md` for the full sketch.

## 12. Out of scope reminder

- Engine / channel / store plugins — Phase C.
- Multi-session persistence, resume-after-disconnect — not this spec, not this phase.
- Feature additions of any kind beyond what the skills in §4 provide.
- Installing upstream skills other than `skill-creator`.
- Any Rust code dedicated to orchestration, planning, tasks, classification, routing, or DAG enforcement.
