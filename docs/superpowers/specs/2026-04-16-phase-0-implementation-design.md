# Phase 0 — Implementation Design

**Status:** Design, pending approval
**Date:** 2026-04-16
**Parent spec:** `2026-04-15-phase-0-doctrine-design.md`
**Constraint:** Every step leaves `cargo build && cargo test` green and both TUI + Telegram channels functional.

---

## 1. Spec deviations

This implementation design departs from the parent spec in two places. Both were discussed and approved during brainstorming.

### 1.1 `ToolScope` placement: `ports.rs`, not `types.rs`

The parent spec (§7) left this open. Decision: `ports.rs`. Rationale: `ports.rs` holds cross-cutting interface contracts. `ToolScope` is a gating contract that every tool executor must honour — it belongs alongside the port traits, not alongside data-transfer structs in `types.rs`.

### 1.2 `skill-creator` is authored, not copied from upstream

The parent spec (§3.1, §5.1 P0-5) called for a verbatim copy of `anthropics/skills/skill-creator` with a CI hash pin. That upstream skill depends on Python/TypeScript scripts. Tengu is a single-binary Rust project — adding a scripting runtime dependency for one skill is undesirable.

Instead, P0-5 authors a tengu-native `skill-creator` documentation skill from scratch, drawing on the upstream structure and the superpowers `writing-skills` methodology. It uses only tengu workspace primitives (`read_file`, `write_file`, `list_directory`). No scripts, no external runtime.

Consequence: the CI hash-pin check from the parent spec is dropped (no upstream to track).

`skill-eval` (P0-6) is similarly simplified: documentation skill only, no bundled Python script. Gating checks use `run_command`; manual prompt tests are human-in-the-loop. LLM-judge mode deferred to post-Phase-D.

---

## 2. Structure: three parallel lanes

Work is split into three independent lanes. Each step within a lane is sequential; lanes have no cross-dependencies. After all three lanes land, Phase 0 is done.

```
Lane 1 (docs):   P0-1  ──→  P0-2
Lane 2 (code):   P0-3  ──→  P0-4  ──→  P0-7
Lane 3 (skills): P0-5  ──→  P0-6
```

Lanes can be executed in any order or interleaved. The only hard rule: **all three lanes complete before Phase A starts** (per parent spec §10).

---

## 3. Lane 1 — Docs

### P0-1: Rewrite `docs/architecture.md` with the doctrine

**What changes:**
- Prepend the four principles (heart/brain/hands/skills) and the no-compromise corollary from parent spec §1 at the top of `docs/architecture.md`
- Preserve existing operational content (Core Loop, Engine Backends, Key Abstractions sections) below the doctrine
- Add a section mapping `ToolScope` vs `ToolAllowList` relationship (coarse gate vs fine gate, per parent spec §2.5)
- Add a top-of-file note: "Every phase spec (A, B, C, D) references this document."

**Files touched:** `docs/architecture.md`
**LOC delta:** ~+250 docs
**Risk:** None — zero code changes.

**No-break guarantee:** `cargo build` unaffected.
**Verification:** Read the file, confirm doctrine is stated clearly and existing content is preserved.

### P0-2: Phase spec alignment deltas

**What changes:** Apply the deltas from parent spec §4 to the four existing phase spec files.

**Phase A** (`2026-04-15-phase-a-tool-plugin-architecture-design.md`):
- §2 Goal: add bullet 0 — "hands" principle reference
- §4.1 `ToolCtx`: add `pub scope: &'a ToolScope` field note with cross-reference to Phase 0 §2
- §4.1 `PluginCtx`: add scope map note for construction-time bake-in
- Migration table: add note "+5 LOC per tool for scope enforcement"
- New §4.10: "Scope enforcement lint" paragraph describing CI guard (cross-ref P0-7)

**Phase B** (`2026-04-15-phase-b-orchestration-collapse-design.md`):
- §2 Goal: add bullet 0 — "logic" principle reference
- §4.3 skill-creator section: shrink to cross-reference ("See Phase 0 §3 — skill-creator is already installed")
- Migration table B1: mark as `[moved to Phase 0]`
- §3 Non-goals: add "Skill-creator installation — moved to Phase 0"

**Phase C** (`2026-04-15-phase-c-engine-channel-store-plugins-design.md`):
- §2 Goal: add bullet 0 — "skeleton" principle reference
- §4.2 `ChannelCtx`: add `pub scopes: &'a HashMap<String, HashMap<String, ToolScope>>` field note
- §4.1 `EngineBuildCtx`: add clarification "engines do not carry ToolScope — scope is per-tool"

**Phase D** (`2026-04-15-phase-d-context-spill-to-rag-design.md`):
- §2 Goal: add bullet 0 — "brain" principle reference
- §4.1 budget model: add note "every knob here is a brain-assembly knob, not a behaviour knob"
- §5.3 orchestration-skill paragraph: note it stays unchanged, governed by skill-eval lifecycle

**Files touched:** Four phase spec files
**LOC delta:** ~+40 docs per file (~+160 total)
**Risk:** None — zero code changes.

**No-break guarantee:** `cargo build` unaffected.
**Verification:** Read each file, confirm deltas are present and consistent with parent spec §4.

---

## 4. Lane 2 — Code

### P0-3: `ToolScope` type + enforcement helpers

**What changes:** Add `ToolScope` struct and `check_*` methods to `src/adapters/ports.rs`.

**The type:**
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

**Six enforcement methods on `ToolScope`:**

| Method | Input | Returns | Logic |
|--------|-------|---------|-------|
| `check_fs_read(&self, path: &Path)` | Raw path | `Result<PathBuf>` | Canonicalize (walk up for non-existent leaves), match against `fs_roots` |
| `check_fs_write(&self, path: &Path)` | Raw path | `Result<PathBuf>` | Same as read — write to new file inside allowed root must work |
| `check_net_host(&self, host: &str)` | Hostname | `Result<()>` | Exact match or single leading-wildcard (`*.anthropic.com`) |
| `check_env_read(&self, var: &str)` | Var name | `Result<()>` | Bare name match against `env_reads` |
| `check_shell_bin(&self, bin: &str)` | Binary path or name | `Result<()>` | Basename match against `shell_bins` |
| `check_wallet(&self, label: &str)` | Wallet label | `Result<()>` | Exact match against `wallets` |

All return `anyhow::Error` on reject with a clear message naming the tool, the attempted target, and the allowed set, so the LLM sees exactly why the call was refused and can adapt.

**Path canonicalization detail:** For `check_fs_read` and `check_fs_write`, if the target path doesn't exist yet (common for write), walk up parent directories until one exists, canonicalize that, then re-append the non-existent tail. This lets `write_file("./research/new-doc.md")` succeed when `./research/` exists and is under an allowed root.

**Imports added to `ports.rs`:**
- `use std::path::{Path, PathBuf};`
- `use serde::Deserialize;`

**Unit tests** (inline `#[cfg(test)]` in `ports.rs`):

| Test | Asserts |
|------|---------|
| `fs_allowed_root_passes` | Path under allowed root returns Ok with canonicalized path |
| `fs_disallowed_rejects` | Path outside all roots returns Err |
| `fs_subdir_passes` | Nested subdir of allowed root passes |
| `fs_traversal_rejects` | `../` traversal out of allowed root rejects |
| `fs_nonexistent_leaf_passes` | New file inside allowed root passes (parent walk-up) |
| `net_exact_match` | Exact host match passes |
| `net_wildcard_match` | `*.anthropic.com` matches `api.anthropic.com` |
| `net_wildcard_no_bare` | `*.anthropic.com` does NOT match `anthropic.com` |
| `net_empty_rejects_all` | Empty net_hosts rejects any host |
| `env_allowed_passes` | Listed var passes |
| `env_unlisted_rejects` | Unlisted var rejects |
| `shell_basename_match` | `/usr/bin/git` matches `git` in shell_bins |
| `shell_unlisted_rejects` | Unlisted binary rejects |
| `wallet_exact_match` | Listed label passes |
| `wallet_unlisted_rejects` | Unlisted label rejects |
| `default_denies_all` | `ToolScope::default()` rejects every check type |

**Files touched:** `src/adapters/ports.rs`
**LOC delta:** ~+180 code, ~+120 tests
**Risk:** Low — additive only. No existing code modified. `Deserialize` and `PathBuf` are always available, no feature flags involved.

**No-break guarantee:** New struct + impl + tests. No callers. Existing tool execution path unchanged.
**Verification:** `cargo test` — all new tests pass. `cargo run` — TUI and Telegram work.

### P0-4: Config schema for scopes

**What changes:** Extend `AgentConfig` and `Config` in `src/adapters/config.rs`.

**New field on `AgentConfig`:**
```rust
/// Per-tool scope restrictions. Key = tool name, value = ToolScope.
/// Tools without an entry (and no default_scopes entry) are not registered.
#[serde(default)]
pub scopes: HashMap<String, ToolScope>,
```

**New field on `Config`:**
```rust
/// Fallback scopes applied when an agent has no per-tool scope entry.
/// Per-agent scopes override default_scopes wholesale (not field-merged).
#[serde(default)]
pub default_scopes: HashMap<String, ToolScope>,
```

**New method on `Config`:**
```rust
/// Resolve the effective ToolScope for a given agent + tool.
/// Per-agent overrides default_scopes wholesale (not field-merged).
/// Returns None if neither agent nor default_scopes has an entry.
pub fn resolve_scope(&self, agent_name: &str, tool_name: &str) -> Option<ToolScope>
```

**Update to `Config::default()`:** Add `default_scopes: HashMap::new()` to the default config. Add `scopes: HashMap::new()` to the default agent.

**Import added:** `use crate::adapters::ports::ToolScope;` in `config.rs`.

**Tests** (inline `#[cfg(test)]` in `config.rs`):

| Test | Asserts |
|------|---------|
| `existing_config_no_scopes_parses` | Existing config.toml without scopes still parses and validates (backwards compat) |
| `agent_scopes_roundtrip` | TOML with `[agents.main.scopes.write_file]` deserializes correctly |
| `default_scopes_roundtrip` | TOML with `[default_scopes.read_file]` deserializes correctly |
| `resolve_scope_agent_overrides_default` | Agent scope wins over default_scopes for same tool |
| `resolve_scope_falls_back_to_default` | default_scopes used when agent has no entry |
| `resolve_scope_none_when_absent` | Returns None when neither has entry |
| `validate_passes_with_scopes` | Validation still passes with scopes present |

**Files touched:** `src/adapters/config.rs`
**LOC delta:** ~+100 code, ~+60 tests
**Risk:** Low — `#[serde(default)]` on both fields means existing configs parse unchanged.

**No-break guarantee:** Both new fields default to empty HashMap. `resolve_scope` has no callers yet. Existing config validation unchanged.
**Verification:** `cargo test` — all new + existing config tests pass. `cargo run` with current `config.toml` — both channels work.

### P0-7: Scope enforcement lint test (structural placeholder)

**What changes:** Create `tests/scope_lint.rs` — the first top-level integration test file.

**Why a placeholder:** The lint is meant to assert every tool's `execute` body begins with `ctx.scope.check_*()`. But the current tool execution signature is `ToolExecutionPort::execute_tool(&self, call: &ToolCall) -> Result<String>` — there is no `ToolCtx` with a scope field yet. That's Phase A. So the real grep-based lint cannot match a pattern that doesn't exist in the codebase.

**What the placeholder contains:**
- A doc comment explaining the intent: "This test will assert scope enforcement once Phase A introduces ToolCtx"
- One `#[test]` that reads all `*.rs` files under `src/adapters/` and asserts it can parse them (proves the test infrastructure works)
- A commented-out skeleton of the real lint logic (grep for `check_fs_read|check_fs_write|check_net_host|check_env_read|check_shell_bin|check_wallet` in execute bodies), ready to be activated by Phase A's first PR

**Files touched:** New `tests/scope_lint.rs`
**LOC delta:** ~+60 tests
**Risk:** None.

**No-break guarantee:** New file only. Passes trivially.
**Verification:** `cargo test` — lint test passes.

---

## 5. Lane 3 — Skills

### P0-5: Author `skill-creator` for tengu

**What changes:** Create `skills/skill-creator/SKILL.md`.

**Content outline:**

1. **Frontmatter:** name, description ("Use when creating a new skill or modifying an existing skill for a tengu agent"), skill type: documentation
2. **Overview:** Skills are the logic layer — any strategy, workflow, or policy is a skill, not Rust. The skill-creator teaches agents how to author and modify skills.
3. **Skill anatomy:**
   - Frontmatter format: `name`, `description` (required), `requires_bins`, `requires_env`, `os` (optional gating)
   - `SKILL.md` body structure: overview, when to use, core content, common mistakes
   - Optional `references/` directory for heavy reference material
   - Shell skills (create named tools with execution templates) vs documentation skills (frontmatter + body, progressive disclosure)
4. **Three-tier hierarchy:** managed (`~/.tengu/skills/`) → workspace (`.tengu/skills/`) → project (`skills/`). Higher tiers shadow lower.
5. **Description writing:** Start with "Use when...", describe triggering conditions not workflow, third person, include symptoms and concrete triggers. CSO principles (keywords, synonyms, error messages).
6. **Create flow:** Pick name → write frontmatter → write body → place in correct tier → test with an agent
7. **Modify flow:** Read existing skill → identify what to change → edit → test
8. **Anti-patterns:** Don't put Rust-level policy in skills (that's substrate). Don't summarize workflow in description. Don't add scripts that require external runtimes.
9. **The no-compromise test:** "Does a non-engineer user need to change this behaviour by editing a markdown file, or by filing a PR? If markdown, it's a skill."

**Files touched:** New `skills/skill-creator/SKILL.md`
**Risk:** None — content only, no Rust changes.

**No-break guarantee:** New files in `skills/` only.
**Verification:** `cargo build` unaffected. Load an agent, confirm skill-creator appears in the skill catalog. Ask the agent "How would you create a new skill for handling API rate limits?" — it should reference the skill-creator content.

### P0-6: Author `skill-eval` for tengu

**What changes:** Create `skills/skill-eval/SKILL.md`.

**Content outline:**

1. **Frontmatter:** name, description ("Use when evaluating whether existing skills still work, checking for drift, or auditing the skill inventory"), skill type: documentation
2. **Overview:** skill-eval is the feedback half of the evolution loop. skill-creator creates; skill-eval measures.
3. **Evaluation procedure:**
   - Enumerate all skills across the three-tier hierarchy using `list_directory` on each tier path
   - For each skill, read `SKILL.md` frontmatter
4. **Gating checks** (automated via `run_command`):
   - `requires_bins`: `which <bin>` — is the binary on PATH?
   - `requires_env`: is the env var set?
   - `os`: does the current OS match?
   - Verdict: gate-pass or gate-fail with specific missing dependency
5. **Drift checks** (semi-automated):
   - Skill references tool names that no longer exist in the tool registry
   - Skill references file paths or directories that no longer exist in the workspace
   - Skill description mentions capabilities the agent doesn't have
   - Verdict: pass or drift with specific stale reference
6. **Dead skill detection:**
   - Skill is present but its description doesn't match any plausible user workflow
   - Heuristic: no agent in the config has the skill's package in `skill_packages`
   - Verdict: active or dead with reasoning
7. **Report format:** Per-skill table: name, tier, verdict (pass / drift / dead / gate-fail), one-line reasoning, recommended action (keep / revise / archive)
8. **Recommended actions:**
   - **keep:** All checks pass
   - **revise:** Drift or stale references detected — invoke skill-creator in modify mode
   - **archive:** Dead skill — move out of active tier
9. **What skill-eval does NOT do (Phase 0):** No LLM-judge mode, no automatic prompt replay, no scheduled runs. These are post-Phase-D considerations.

**Files touched:** New `skills/skill-eval/SKILL.md`
**Risk:** Low — content quality is the main risk. Mitigated by conservative scope (no LLM-judge).

**No-break guarantee:** New files in `skills/` only.
**Verification:** `cargo build` unaffected. Load an agent, ask it to evaluate the existing skills in the repo. It should walk the hierarchy and produce a per-skill report.

---

## 6. Execution order summary

Any interleaving of lanes works. Within each lane, steps are sequential:

| Step | Lane | Description | Depends on |
|------|------|-------------|------------|
| **P0-1** | 1 | Doctrine doc | — |
| **P0-2** | 1 | Phase spec deltas | P0-1 |
| **P0-3** | 2 | ToolScope type + checks | — |
| **P0-4** | 2 | Config schema for scopes | P0-3 |
| **P0-7** | 2 | Scope lint placeholder | P0-4 |
| **P0-5** | 3 | skill-creator authoring | — |
| **P0-6** | 3 | skill-eval authoring | P0-5 |

**Per-step smoke test:** After every step, run `cargo build && cargo test`. Launch TUI (`cargo run`) and Telegram, send a test message through each. Confirm no regressions.

---

## 7. What success looks like

After all three lanes land:

- `docs/architecture.md` states the heart/brain/hands/skills doctrine. Every phase spec references it.
- `ToolScope` exists in `src/adapters/ports.rs` with unit tests for all six `check_*` methods. Default-deny proven by test.
- `AgentConfig.scopes` and `Config.default_scopes` parse from TOML with backwards compatibility. `resolve_scope()` implements the override rule.
- `skills/skill-creator/SKILL.md` teaches agents how to author and modify skills using workspace primitives.
- `skills/skill-eval/SKILL.md` teaches agents how to evaluate skill health with gating checks and drift detection.
- A structural lint test placeholder in `tests/scope_lint.rs` is ready for Phase A to activate.
- No line in `engine_builder.rs`, `channel_runtime.rs`, `subagent_builder.rs`, `telegram_builder.rs`, `chat_builder.rs`, `memory_builder.rs`, or `skill_builder.rs` has changed.

---

## 8. Out of scope

Inherited from parent spec §5.4, plus:

- Migrating any existing tool to enforce scope (Phase A)
- Wiring `ToolScope` into `ToolCtx` or `PluginCtx` (Phase A)
- Authoring `orchestration`, `skill-cleaner`, or `scope-auditor` skills
- LLM-judge eval mode for skill-eval
- Automatic/scheduled skill-eval runs
- CI hash-pin check for skill-creator (dropped — no upstream to track)
- Any change to existing Rust files other than `ports.rs` and `config.rs`
