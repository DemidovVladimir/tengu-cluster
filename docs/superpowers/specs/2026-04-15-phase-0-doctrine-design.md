# Phase 0 — Architecture Doctrine + Foundational Hooks

**Status:** Design, pending approval
**Date:** 2026-04-15
**Depends on:** Nothing (this is the new foundation — it ships before Phase A starts)
**Blocks:** Phase A (tool plugin architecture), and transitively Phases B, C, D

---

## 1. Doctrine

The purpose of Phase 0 is to commit the project to a single, load-bearing mental model for the harness, and to install the minimum hooks that make that model enforceable rather than aspirational. The existing four phases (A, B, C, D) already move substantially in this direction, but they are framed as four independent refactors rather than as one coherent architecture. Phase 0 fixes the framing and adds the hooks; the refactors stay.

A new `docs/architecture.md` is rewritten as the load-bearing doctrine. Every phase spec (0, A, B, C, D) references it. Every future PR is reviewed against it. It states four principles and one corollary.

### 1.1 The four principles

**1. LLM is the heart.** It consumes tokens and emits tokens. It has no behaviour of its own — no memory, no goals, no identity, no plans. Anything that looks like "the agent did X because…" is really "the context instructed the LLM, and the LLM produced X." The Rust core never hard-codes behaviour that belongs to the model.

**2. Context is the brain.** Everything the LLM *knows* on a given turn lives in the context window: system prompt, bootstrap files (AGENTS.md, MEMORY.md, daily logs, identity files), tool definitions, skill catalog entries, transcript history, pending tool results. The Rust core's job is **brain assembly** — deciding what goes into the context, how much, in what order, and what to do when it overflows (Phase D's RAG spill). The core does not decide what the brain *does* with that context.

**3. Tools and MCP are the hands and senses.** They are the only way the LLM touches the world. A tool reads a file, writes a file, runs a command, signs a transaction, calls an HTTP API, spawns a subagent. Tools are **gateable** (user decides which exist — `CapabilityId`) and **scopeable** (user decides what each is allowed to touch — `ToolScope`, §2 of this spec). MCP servers extend the hands without touching Rust. Adding a tool never requires adding Rust code beyond a new plugin file or a new `[[mcp_servers]]` entry.

**4. Skills are the logic.** Anything that looks like a strategy, a workflow, a plan, a playbook, or "how the agent decides what to do" is a skill, not Rust. Orchestration (Phase B). Decomposition. Delegation. Failure handling. Progress tracking. Even meta-behaviour like "how to write new skills" is a skill (`skill-creator`). The harness evolves because skills evolve — authored, evaluated, and retired by `skill-creator` and `skill-eval` running against user-visible traces. The Rust core ships the minimum substrate that lets skills do their job and stays out of the way.

### 1.2 The no-compromise corollary

If work during any phase is tempted to add Rust code that encodes *policy* — when to delegate, how to retry, what to prioritize, how to format output, when to ask for clarification — that code is a skill, not Rust. The test is: *does a non-engineer user need to change this behaviour by editing a markdown file, or by filing a PR?* If the answer is "markdown file," it's a skill.

This rule is the single sentence every PR reviewer checks against during the A/B/C/D phases. A violation is not a style issue; it is a doctrine violation and blocks the PR.

### 1.3 Why now

The existing four phases already implement most of this doctrine. Phase A makes tools pluggable and adds inbound MCP. Phase B moves orchestration from Rust into a skill. Phase C extends the plugin pattern to engines, channels, stores, and embedders. Phase D replaces lossy context caps with lossless RAG spill. But three pieces of the doctrine are not yet load-bearing in those specs:

- **User-controlled tool scoping.** Today `CapabilityId` is binary: a tool exists or it doesn't. There is no per-tool scope — no way to say "write_file is allowed, but only under `./research/`." The doctrine's "hands are controllable" claim has no teeth until scoping exists.
- **Self-evolution loop as a first-class fixture.** Phase B installs `skill-creator` as a one-off tool used to author `orchestration`. The doctrine treats `skill-creator` + `skill-eval` as the *permanent* mechanism by which the harness adapts to the user. That belongs before Phase A, not buried in Phase B.
- **The doctrine document itself.** No existing doc states the heart/brain/hands/skills model. Without it, every subsequent phase is on honour code.

Phase 0 addresses all three. It is additive, greenfield-only, and does not touch any of the files the A/B/C/D phases will rewrite.

## 2. ToolScope — user-controlled hands

`CapabilityId` (Phase A) stays. It is the **coarse** gate: does this tool exist at all for this agent? Phase 0 adds `ToolScope` underneath as the **fine-grained** gate: when the tool runs, what is it allowed to touch?

### 2.1 The type

New struct in `src/adapters/ports.rs` (final placement may migrate to `types.rs` during the implementation plan — mechanical):

```rust
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct ToolScope {
    /// Allowed filesystem roots. Every fs-touching tool must reject
    /// paths that, after canonicalization, do not start with one of these.
    /// Empty = no fs access.
    pub fs_roots: Vec<PathBuf>,

    /// Allowed outbound host patterns (exact or glob: "api.linear.app",
    /// "*.anthropic.com"). Empty = no network access.
    pub net_hosts: Vec<String>,

    /// Env var names the tool is allowed to read (via SecretVault $VAR refs).
    /// Empty = no env reads.
    pub env_reads: Vec<String>,

    /// For run_command: allowed binary names (basename match).
    /// Empty = no shell execution.
    pub shell_bins: Vec<String>,

    /// For crypto tools: allowed wallet labels.
    /// Empty = no crypto access.
    pub wallets: Vec<String>,
}
```

`ToolScope` is **default-deny**: every field empty means the tool can do nothing. Config must grant access explicitly. This inverts today's implicit-allow model and is the single largest "no unfortunate thing happens" guarantee the doctrine adds.

### 2.2 Where it lives

Phase A's `ToolCtx` (defined in `2026-04-15-phase-a-tool-plugin-architecture-design.md` §4.1) gains one field:

```rust
pub(crate) struct ToolCtx<'a> {
    // ... existing fields ...
    pub scope: &'a ToolScope,
}
```

`ToolCtx` is built per-tool-call by the registry, from a **per-tool** `ToolScope` stored in `ToolRegistry`. Different tools in the same agent can have different scopes: `write_file` might be restricted to `./research/`, while `read_file` can read anywhere under the workspace. `PluginCtx` (construction-time) also carries the scope map so plugins that build long-lived tool structs can bake scope in at construction if they choose — though most tools simply read from `ToolCtx` per call.

### 2.3 Enforcement rule (single and uniform)

Every `Tool::execute` that touches the outside world calls `ctx.scope.check_*(...)` as its **first** line, before any other work:

```rust
async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
    let a: Args = serde_json::from_value(args.clone())?;
    let path = ctx.scope.check_fs_write(&a.path)?;   // ← the rule
    // ... proceed with the actual write ...
}
```

The `check_*` methods return `Result<PathBuf>` (or the equivalent resolved/validated type for non-fs checks). On reject they return an `anyhow::Error` with a clear message ("write_file: path '/etc/passwd' outside allowed scope ['./research/', './drafts/']") so the LLM sees exactly why the call was refused and can adapt.

Path checking canonicalizes the input, walks up one directory at a time if the leaf doesn't exist yet (so `write_file` to a new file inside an allowed root works), and rejects if no `fs_roots` prefix matches. Host checking supports exact match and a single leading-wildcard glob (`*.anthropic.com`). Env-read checking matches the bare variable name. Shell-bin checking matches the basename of the resolved binary. Wallet checking matches the label exactly.

**No tool in the codebase is allowed to skip this call.** A CI regression test (§4.5 Delta 5 below, landed in Phase 0 as P0-7) asserts every tool's `execute` body begins with a `ctx.scope.check_*()` call via a grep-based lint. Crude but effective; fails CI if a new tool forgets.

### 2.4 Config shape

Scope lives in the agent config, per tool:

```toml
[[agents]]
name = "researcher"
engine = "openrouter"

[agents.scopes.write_file]
fs_roots = ["./research", "./drafts"]

[agents.scopes.read_file]
fs_roots = ["./", "/tmp/allowlisted"]

[agents.scopes.http_request]
net_hosts = ["api.linear.app", "*.anthropic.com"]
env_reads = ["LINEAR_API_KEY", "ANTHROPIC_API_KEY"]

[agents.scopes.run_command]
shell_bins = ["git", "cargo", "rg"]
fs_roots = ["./"]

[agents.scopes.sign_and_send_transaction]
wallets = ["research-wallet"]
```

No `[agents.scopes.xxx]` entry for a given tool = default-deny. The tool is not instantiated for this agent (same observable effect as the capability being off, but the reason is different: capability = "doesn't exist", scope-absent = "exists but has nothing to touch").

A top-level `[default_scopes]` block provides fallbacks so the user does not have to repeat common scopes for every agent:

```toml
[default_scopes.read_file]
fs_roots = ["./"]

[default_scopes.run_command]
shell_bins = ["git", "rg", "cargo"]
fs_roots = ["./"]
```

Per-agent `[agents.scopes.*]` entries override `[default_scopes.*]` entries wholesale (not field-merged) to keep the resolution rule simple.

### 2.5 Relationship to `CapabilityId`

Both are kept. They answer different questions:

| Question | Answered by |
|---|---|
| Does this tool exist at all for this agent? | `CapabilityId` (binary, per Phase A) |
| When the tool runs, what is it allowed to touch? | `ToolScope` (per Phase 0) |

Order of checks at registry construction:

1. Capability off → tool is not registered. Done.
2. Capability on, no scope entry (and no default_scope entry) → tool is not registered. Same observable effect as capability off.
3. Capability on, scope present → tool is registered with that scope bound in.

At runtime, the tool's `execute` body still calls `ctx.scope.check_*()` as its first line because (a) scope might be more restrictive than registry-time assumptions, and (b) the lint test enforces it uniformly.

## 3. Skill-creator + skill-eval as the evolution loop

Phase 0 installs the two skills that make the harness self-adapting, **before** Phase A starts. This is a promotion from "one-shot tool used in Phase B" to "permanent architectural fixture available to all phases."

### 3.1 What lands in Phase 0

**`skills/skill-creator/`** — verbatim copy of `anthropics/skills/skill-creator`, zero edits, per the existing `feedback_skills_portable.md` memory. Content covers create, modify, and a basic eval harness. A CI check hashes the directory and compares against a pinned upstream hash; drift triggers a re-copy PR, not an inline edit.

**`skills/skill-eval/`** — a new authored skill. Phase B's existing spec implicitly assumed eval capability lived inside `skill-creator`, but the user-facing lifecycle is different: `skill-creator` is invoked when *creating or modifying* behaviour, `skill-eval` is invoked when *measuring* whether existing skills still work. Independent triggers call for independent skills.

`skill-eval`'s job:

- Walk the three-tier skill hierarchy (`~/.tengu/skills/` → `.tengu/skills/` → `skills/`).
- For each skill, replay a small fixed prompt set against the current main agent. The prompt set is configured either in the skill's own `SKILL.md` under an `evals:` frontmatter key, or in a sibling `evals.yaml` file next to `SKILL.md`.
- Compare model outputs to expected outcomes in one of three modes: exact-match, regex, or LLM-judge.
- Report drift: which skills passed, which failed, which are silently un-triggered (the "dead skill" signal — skill present but no prompt triggered it, which suggests either a bad description or a no-longer-needed skill).
- Recommend action per skill: keep, revise (via `skill-creator`), or archive.

`skill-eval` is a documentation skill (frontmatter + `SKILL.md` body) plus one bundled script (Python or bash, ~150 LOC) invoked via `run_command`. It does not execute automatically; it runs when the user or an agent explicitly triggers it.

### 3.2 The evolution loop

Once both skills are installed, the harness has a closed feedback loop with no Rust code participating beyond providing the primitives (`run_command`, `read_file`, `write_file`, `memory_write`, `sessions_spawn`):

```
user sees unwanted behaviour          ─┐
  │                                    │
  ▼                                    │
user runs /skill-eval                  │    feedback
  │                                    │    loop
  ▼                                    │
skill-eval reports which skills        │
drifted or never triggered             │
  │                                    │
  ▼                                    │
user runs skill-creator (modify mode)  │
on the offending skill                 │
  │                                    │
  ▼                                    │
skill-creator rewrites SKILL.md and    │
re-runs skill-eval to confirm          │
  │                                    │
  ▼                                    │
skill is now aligned to the user      ─┘
```

The Rust core does not know this loop exists. It provides substrate; the loop runs on top.

### 3.3 Why Phase 0 and not Phase B

Phase B's B1 was originally planned to copy `skill-creator` and its B2 step authored `orchestration` using it. Moving both skill installs earlier to Phase 0 gives two immediate benefits:

1. **Phase A can dogfood the doctrine.** As the A-series PRs land scope enforcement per tool, a `scope-auditor` skill can be authored via `skill-creator` to walk the config and flag over-permissive scopes. No Rust work needed. The doctrine is tested on day one.
2. **Phase B's Rust-risk work is isolated.** Phase B's B3 step (Telegram migration) is the single riskiest change in the entire four-phase plan. The fewer non-Rust prerequisites sit next to it, the cleaner the review. Moving `skill-creator` out of B shortens B's critical path.

### 3.4 What does NOT land in Phase 0

- **`orchestration` skill** — authored in Phase B using the skill-creator now already installed. Phase 0 does not pre-author it; that is behaviour design best done by the humans + skill-creator together during Phase B.
- **`skill-cleaner`** — Phase B's optional B7 follow-up. It needs real usage traces to cross-reference, which Phase 0 doesn't have.
- **`scope-auditor`** — a plausible future skill referenced above as a dogfood example, but not in scope for Phase 0 itself. It can be authored in Phase A once scope enforcement goes live.

## 4. Reframing deltas for A/B/C/D

Each existing spec gets small, targeted edits. No content rewrite. No renaming. The deltas below are applied as a single "doctrine alignment" commit after the Phase 0 doctrine doc (P0-1) is approved and committed.

### 4.1 Phase A — Tool Plugin Architecture

**Delta 1 — §2 Goal, new top bullet.**
> **0.** Implements the "hands" principle of `docs/architecture.md`: tools are the only way the LLM touches the world; tools are gateable (`CapabilityId`) *and* scopeable (`ToolScope`); MCP extends hands without touching Rust.

**Delta 2 — §4.1 `ToolCtx`, new field.** Add `pub scope: &'a ToolScope,` with a sentence: *"Scope is built per-tool from agent config by the registry; tools enforce it on their first line via `ctx.scope.check_*()`. See `2026-04-15-phase-0-doctrine-design.md` §2 for the type and enforcement rule."*

**Delta 3 — §4.1 `PluginCtx`, new field.** Add a scope map so plugins that build long-lived tool structs can bake scope in at construction time if they choose.

**Delta 4 — Migration table note.** A single sentence under the table: *"Per-tool scope enforcement adds ~5 LOC per tool; already counted in each PR's delta."* No row changes.

**Delta 5 — New §4.10 "Scope enforcement lint."** One paragraph describing the CI regression guard: an integration test that asserts every tool's `execute` body begins with a `ctx.scope.check_*()` call (grep-based). Fails CI if a new tool forgets. The test is landed in Phase 0 as P0-7 and becomes effective tool-by-tool as the A-series migrates existing tools.

No other part of Phase A changes. The A-series LOC totals stay within the noise of the existing ~−2,400 estimate.

### 4.2 Phase B — Orchestration Collapse

**Delta 1 — §2 Goal, new top bullet.**
> **0.** Implements the "logic" principle of `docs/architecture.md`: all orchestration policy moves out of Rust and into `skills/orchestration/`. The `skill-creator` + `skill-eval` meta-loop (installed in Phase 0) is what makes this evolvable.

**Delta 2 — §4.3 "skill-creator (upstream, verbatim copy)" shrinks to a cross-reference.** Body becomes: *"See Phase 0 §3 — `skill-creator` is already installed by the time Phase B starts. Phase B authors content with it but does not install it."*

**Delta 3 — Migration table B1 becomes `[moved to Phase 0]`.** B2 is retained but shrinks to "author `skills/orchestration/SKILL.md` content using the already-installed skill-creator." B3–B7 unchanged.

**Delta 4 — §3 Non-goals gains one line.** *"Skill-creator installation — moved to Phase 0."*

Net effect: Phase B gets slightly shorter, drops its content-skill-install work to Phase 0, and Rust-only risk (B3 Telegram migration) is isolated from content work. Rust LOC totals unchanged.

### 4.3 Phase C — Engine / Channel / Store Plugins

**Delta 1 — §2 Goal, new top bullet.**
> **0.** Implements the "skeleton" principle of `docs/architecture.md`: engines, channels, stores, and embedders are plugins on the same pattern as tools, so adding a new backend is a one-file change — the harness stays user-alignable at the substrate level.

**Delta 2 — §4.2 `ChannelCtx`, new field.** Add `pub scopes: &'a HashMap<String, HashMap<String, ToolScope>>,` (outer key = agent name, inner key = tool name) so that when a channel opens a session for a given agent, the tools it exposes to that session see the right `ToolScope` per tool. No newtype is introduced to keep the addition mechanical; a type alias (`AgentScopes`) can be added during Phase C if the signature becomes unwieldy.

**Delta 3 — §4.1 `EngineBuildCtx`, clarifying note.** *"Engines do not carry a `ToolScope` — scope is per-tool, not per-engine. An engine sees the `ToolRegistry` built with per-agent scopes already applied."*

No other Phase C changes. LOC totals unchanged.

### 4.4 Phase D — Context Spill to RAG

**Delta 1 — §2 Goal, new top bullet.**
> **0.** Implements the "brain" principle of `docs/architecture.md`: context assembly is the Rust core's load-bearing job, and brain overflow becomes lossless (spill to RAG) rather than lossy (truncation). The core decides what enters and leaves the context window; it does not decide what the LLM does with it.

**Delta 2 — §4.1 "60% / 10× budget model," short note.** *"Every knob in this section is a 'what enters the brain' knob, not a 'what the LLM does' knob. The latter is skill territory."*

**Delta 3 — §5.3 orchestration-skill paragraph unchanged.** The Phase D addition to the orchestration skill ("summaries are authoritative") stays exactly as currently specified. It is a skill content change and belongs to the orchestration skill lifecycle, governed by `skill-eval`.

No LOC changes. Phase D's hot-path work (D5) is unchanged.

### 4.5 Rollout

All four spec edits land in a single "doctrine alignment" commit **after** the Phase 0 doctrine doc (P0-1) is approved and committed. The commit touches only the four Phase specs; no code, no other docs.

## 5. Migration plan

Phase 0 is mostly content plus a small foundational code landing. It must ship before Phase A starts, but it does not block on any other work — nothing in Phase 0 touches the legacy executor files, the legacy orchestrator, or the engine hot path. It is additive and greenfield-only.

### 5.1 PR sequence

| # | PR | Touches | LOC Δ | Risk |
|---|---|---|---|---|
| **P0-1** | **Doctrine doc.** Rewrite `docs/architecture.md` with the four principles + the no-compromise corollary from §1 of this spec. Add a top-of-file note that every subsequent phase spec references it. No code. | `docs/architecture.md` | +250 docs | none |
| **P0-2** | **Phase spec alignment.** Apply §4 of this spec — the A/B/C/D reframing deltas. One commit, four files. No code. | four existing phase spec files | ~+40 docs each | none |
| **P0-3** | **`ToolScope` type + enforcement helpers.** Add `ToolScope` struct to `src/adapters/ports.rs` (final placement decided during implementation planning — `types.rs` is an acceptable alternative). Add `check_fs_read`, `check_fs_write`, `check_net_host`, `check_env_read`, `check_shell_bin`, `check_wallet` methods. Unit tests for each check. No caller wiring yet — the type exists, unused, ready for Phase A's A0 to consume. | `ports.rs` (or `types.rs`), new unit tests | +180 code, +120 tests | **low** |
| **P0-4** | **Config schema for scopes.** Extend `AgentConfig` with `scopes: HashMap<String, ToolScope>` and an optional top-level `[default_scopes]` block. Deserialization tests on sample `config.toml` snippets. Still no caller wiring — Phase A's A0 plumbs this into `PluginCtx` / `ToolCtx`. | `config.rs`, config tests | +100 code, +60 tests | low |
| **P0-5** | **`skill-creator` install.** Copy `anthropics/skills/skill-creator` verbatim into `skills/skill-creator/`. Add a CI check that hashes the directory and compares against a pinned upstream hash (same mechanism previously planned for Phase B's B1). | `skills/skill-creator/`, new CI job | 0 Rust, +skill content | none |
| **P0-6** | **`skill-eval` authoring.** Author `skills/skill-eval/SKILL.md` + bundled eval-runner script (Python or bash, ~150 LOC). Use the now-installed `skill-creator` as a co-author in the implementation session. Include a minimal eval set that runs on every skill currently in the repo and reports. | `skills/skill-eval/` | 0 Rust, +skill content | low (content quality) |
| **P0-7** | **Scope enforcement lint test** (§4.1 Delta 5). Add an integration test file that fails CI if a tool's `execute` body does not begin with a `ctx.scope.check_*()` call. The test currently passes trivially because no tools are migrated yet; it **becomes active** as Phase A converts tools. | `tests/scope_lint.rs` | +60 tests | none |

### 5.2 Ordering and dependencies

- **P0-1 and P0-5 can ship in parallel** — one is docs, the other is content; they do not touch each other.
- **P0-2 must land after P0-1** — it references the doctrine doc.
- **P0-3 must land before P0-4** — config references the type.
- **P0-6 must land after P0-5** — `skill-eval` is authored using `skill-creator`.
- **P0-7 lands last** — the lint test goes green only once `ToolScope` exists (P0-3) and the test itself is in place. It is dormant until Phase A starts migrating tools.

No PR in Phase 0 touches legacy orchestration, engine assembly, existing tools, channels, or the memory hot path. Phase 0 is a **greenfield-only** addition. That is deliberate — the risk of a doctrine phase must be near-zero so Phase A can start on a clean base.

### 5.3 LOC totals

- **+250 docs** (P0-1 — doctrine doc)
- **+160 docs** (P0-2 — four phase spec deltas, ~40 lines each)
- **+280 code** (P0-3 + P0-4 — `ToolScope` type and config schema)
- **+240 tests** (P0-3 + P0-4 + P0-7 — unit tests, config tests, scope lint)
- **+content** (P0-5 + P0-6 — `skill-creator` copy and new `skill-eval` skill)
- **Net Rust code delta: +280** (additive, no deletions — deletions come in Phase A as tools get scoped-and-migrated)

### 5.4 Out of scope

- **Migrating any existing tool to enforce scope.** That is Phase A, per PR.
- **Retiring `CapabilityId`.** Kept. `CapabilityId` = coarse "does this tool exist for this agent," `ToolScope` = fine "what can it touch when it runs." Both stay.
- **Authoring `orchestration`, `skill-cleaner`, `scope-auditor`, or any skill other than `skill-eval`.** Those stay in their original phases.
- **Any change to `engine_builder.rs`, `channel_runtime.rs`, `subagent_builder.rs`, `telegram_builder.rs`, `chat_builder.rs`, `memory_builder.rs`, `skill_builder.rs`, or the seven legacy executor files.** Untouched.
- **`docs/architecture.md` sections that are not the doctrine** (e.g., the existing deployment or config reference sections). Preserved verbatim; the doctrine is added at the top.

## 6. Risks and mitigations

| Risk | Mitigation |
|---|---|
| **`ToolScope` field shape turns out wrong** once Phase A starts wiring it, and Phase A's PRs grow to patch it. | P0-3 lands with a **minimum viable struct** — exactly the fields in §2.1, nothing speculative. Doc comment explicitly marks it "subject to refinement during Phase A." Phase A's A0 has license to extend the type once if needed. |
| **Scope enforcement lint (P0-7) is dormant and decays.** Nothing triggers it until Phase A migrates tools, and by then nobody remembers it exists. | Lint test is wired into the CI pipeline from day one even while dormant — it parses the empty tool set and exits 0. Phase A's A0 PR flips it on by adding the first scoped tool; no one-shot reactivation needed. |
| **Doctrine drift** — subsequent PRs violate the no-compromise corollary because nobody reads `docs/architecture.md`. | The no-compromise corollary (§1.2) is copied into each of the A/B/C/D specs' Goal sections as Delta 1 of §4. Reviewers see it on every phase-related PR without needing to cross-reference. |
| **`skill-eval` content quality is low**, producing false positives on day one and teaching users to ignore it. | P0-6 ships `skill-eval` with a minimal, conservative eval set — one or two prompts per existing skill, exact-match or regex mode, no LLM-judge until the content has been iterated. Skill-eval is itself subject to `skill-eval`; the meta-loop is the mitigation. |
| **Feature-flag matrix** breaks when `ToolScope` is added to `ports.rs`. | `ToolScope` is plain Rust with no feature-gated fields. `derive(Default, Deserialize)` and `PathBuf` are always available. CI matrix runs unchanged. |
| **`CapabilityId` and `ToolScope` semantics diverge** over time, confusing users about which one to set. | §2.5 is the single authoritative mapping between them. Phase 0's `docs/architecture.md` section on tools includes the same mapping. Phase A's A0 adds a top-of-file doc comment in `ports.rs` summarising the rule. |

## 7. Open questions

- **Where does `ToolScope` live — `ports.rs` or `types.rs`?** Both are plausible. `ports.rs` already holds cross-cutting trait definitions; `types.rs` holds cross-cutting data types. Decision deferred to the implementation plan; either is acceptable. No behavioural difference.
- **Does `ToolScope` need a `deny` list in addition to the allow lists?** Today the design is pure allow-list. A `fs_deny_roots` field could let the user say "allow `./` but deny `./secrets/`." Decision: not in P0-3. If it becomes needed, add it during Phase A with no spec change required (the type is additive).
- **Should `skill-eval` run automatically on a schedule** (e.g., nightly cron), or only on explicit trigger? Design says explicit-only for Phase 0 to avoid noisy failures while content is iterated. Automatic scheduling is a post-Phase-D consideration.

## 8. What success looks like

After Phase 0 lands:

- `docs/architecture.md` states the heart/brain/hands/skills doctrine with the no-compromise corollary. Every phase spec (A, B, C, D) references it in its Goal section.
- `ToolScope` exists in `src/adapters/ports.rs`, with unit tests for every `check_*` method. The type is not yet used by any tool — but Phase A's A0 can consume it in its first commit without waiting for any other change.
- `AgentConfig` supports `scopes: HashMap<String, ToolScope>` and a `[default_scopes]` fallback block. The schema round-trips through `serde` with deserialization tests.
- `skills/skill-creator/` is present on disk, hash-pinned against upstream, verified by CI.
- `skills/skill-eval/` is present, runs via `run_command`, and reports on every skill currently in the repo. Its first eval pass serves as the doctrine-alignment smoke test.
- A dormant scope-enforcement CI lint is wired into the pipeline, waiting for Phase A to light it up.
- No line in `engine_builder.rs`, `channel_runtime.rs`, `subagent_builder.rs`, `telegram_builder.rs`, `chat_builder.rs`, `memory_builder.rs`, or `skill_builder.rs` has changed. Phase A starts on exactly the codebase it expected, plus one type, one config field, two skills, and a doctrine.

## 9. Dependencies

- **Upstream:** none. Phase 0 is the new foundation.
- **Downstream:** Phase A (directly), Phases B / C / D (transitively via the doctrine alignment deltas).
- **External:** `anthropics/skills/skill-creator` at a pinned commit. CI check is the drift guard.

## 10. Guardrail

**Do not start Phase A until Phase 0 is fully merged.** Partial Phase 0 (e.g., doctrine doc merged but `ToolScope` not yet merged) leaves Phase A's A0 without a type to import and forces the PR order to drift. Phase 0 is small enough (~280 code LOC plus docs and content) that it should ship as a coherent unit.
