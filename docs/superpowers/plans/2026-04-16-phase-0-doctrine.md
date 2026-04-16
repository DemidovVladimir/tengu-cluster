# Phase 0 — Doctrine + Foundational Hooks Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Install the heart/brain/hands/skills architecture doctrine, the `ToolScope` default-deny primitive, config schema for scopes, and the skill-creator/skill-eval evolution loop — all without breaking any existing functionality.

**Architecture:** Three parallel lanes (docs / code / skills). Each step is independently committable and leaves `cargo build && cargo test` green + TUI and Telegram functional. No existing Rust files are modified except `ports.rs` (new struct) and `config.rs` (new fields with `#[serde(default)]`).

**Tech Stack:** Rust (serde, anyhow, std::path), TOML config, Markdown skills

**Design spec:** `docs/superpowers/specs/2026-04-16-phase-0-implementation-design.md`
**Parent spec:** `docs/superpowers/specs/2026-04-15-phase-0-doctrine-design.md`

---

## File Map

| File | Action | Task | Purpose |
|------|--------|------|---------|
| `docs/architecture.md` | Modify | 1 | Prepend doctrine to existing content |
| `docs/superpowers/specs/2026-04-15-phase-a-*.md` | Verify | 2 | Confirm doctrine deltas from `12198f7` |
| `docs/superpowers/specs/2026-04-15-phase-b-*.md` | Verify | 2 | Confirm doctrine deltas from `12198f7` |
| `docs/superpowers/specs/2026-04-15-phase-c-*.md` | Verify | 2 | Confirm doctrine deltas from `12198f7` |
| `docs/superpowers/specs/2026-04-15-phase-d-*.md` | Verify | 2 | Confirm doctrine deltas from `12198f7` |
| `src/adapters/ports.rs` | Modify | 3 | Add `ToolScope` struct + `check_*` methods + tests |
| `src/adapters/config.rs` | Modify | 4 | Add `scopes`/`default_scopes` fields + `resolve_scope` + tests |
| `tests/scope_lint.rs` | Create | 5 | Structural placeholder for scope enforcement CI lint |
| `skills/skill-creator/SKILL.md` | Create | 6 | Teach agents how to author/modify skills |
| `skills/skill-eval/SKILL.md` | Create | 7 | Teach agents how to evaluate skill health |

---

## Lane 1 — Docs

### Task 1: Rewrite `docs/architecture.md` with the doctrine (P0-1)

**Files:**
- Modify: `docs/architecture.md:1-128`

- [ ] **Step 1: Read the current file**

Read `docs/architecture.md` to confirm current content. The file starts with `# Architecture` and has sections: Core Loop, Engine Backends, Key Abstractions, Module Map, Related.

- [ ] **Step 2: Prepend doctrine above existing content**

Replace the opening of `docs/architecture.md`. The new file structure is: doctrine first, then existing content preserved below. The full replacement for the top of the file (everything before `## Core Loop`):

```markdown
# Architecture

> Every phase spec (A, B, C, D) references this document. Every PR is reviewed against it.

Tengu is a single-binary AI agent runtime. All code lives in `src/adapters/` + `src/main.rs` — flat structure, no sub-crates.

---

## Doctrine

The Rust core exists to serve four principles. Violating any of them is a doctrine violation that blocks the PR.

### 1. LLM is the heart

It consumes tokens and emits tokens. It has no behaviour of its own — no memory, no goals, no identity, no plans. Anything that looks like "the agent did X because..." is really "the context instructed the LLM, and the LLM produced X." The Rust core never hard-codes behaviour that belongs to the model.

### 2. Context is the brain

Everything the LLM knows on a given turn lives in the context window: system prompt, bootstrap files (AGENTS.md, MEMORY.md, daily logs, identity files), tool definitions, skill catalog entries, transcript history, pending tool results. The Rust core's job is **brain assembly** — deciding what goes into the context, how much, in what order, and what to do when it overflows (Phase D's RAG spill). The core does not decide what the brain does with that context.

### 3. Tools and MCP are the hands and senses

They are the only way the LLM touches the world. A tool reads a file, writes a file, runs a command, signs a transaction, calls an HTTP API, spawns a subagent. Tools are:

- **Gateable** — the user decides which tools exist for each agent (`ToolAllowList`, today built from tool definitions)
- **Scopeable** — the user decides what each tool is allowed to touch (`ToolScope`, default-deny)

MCP servers extend the hands without touching Rust. Adding a tool never requires adding Rust code beyond a new plugin file or a new `[[mcp_servers]]` entry.

### 4. Skills are the logic

Anything that looks like a strategy, a workflow, a plan, a playbook, or "how the agent decides what to do" is a skill, not Rust. Orchestration. Decomposition. Delegation. Failure handling. Progress tracking. Even meta-behaviour like "how to write new skills" is a skill (`skill-creator`). The harness evolves because skills evolve — authored, evaluated, and retired by `skill-creator` and `skill-eval`. The Rust core ships the minimum substrate that lets skills do their job and stays out of the way.

### The no-compromise corollary

If work during any phase is tempted to add Rust code that encodes *policy* — when to delegate, how to retry, what to prioritize, how to format output, when to ask for clarification — that code is a skill, not Rust. The test is: *does a non-engineer user need to change this behaviour by editing a markdown file, or by filing a PR?* If the answer is "markdown file," it's a skill.

This rule is the single sentence every PR reviewer checks against. A violation is not a style issue; it is a doctrine violation and blocks the PR.

---

## Tool Access Control

Two mechanisms coexist. They answer different questions:

| Question | Mechanism | Where |
|----------|-----------|-------|
| Does this tool exist at all for this agent? | `ToolAllowList` (coarse, binary) | `src/adapters/types.rs` |
| When the tool runs, what can it touch? | `ToolScope` (fine, default-deny) | `src/adapters/ports.rs` |

Order of checks:
1. Tool not in allow-list → tool is not registered. Done.
2. Tool in allow-list, no scope entry → tool is not registered (same effect as #1).
3. Tool in allow-list, scope present → tool is registered with that scope.

At runtime, every tool's execute body calls `scope.check_*()` as its first line.

---
```

Everything from `## Core Loop` onward stays unchanged.

- [ ] **Step 3: Verify the build**

Run: `cargo build 2>&1 | tail -5`
Expected: compiles with no errors (docs-only change)

- [ ] **Step 4: Commit**

```bash
git add docs/architecture.md
git commit -m "rewrite architecture.md with heart/brain/hands/skills doctrine

Prepend the four principles + no-compromise corollary + tool access
control section above the existing operational docs. Existing Core Loop,
Engine Backends, Key Abstractions, and Module Map sections preserved."
```

---

### Task 2: Verify phase spec alignment deltas (P0-2)

**Files:**
- Verify: `docs/superpowers/specs/2026-04-15-phase-a-tool-plugin-architecture-design.md`
- Verify: `docs/superpowers/specs/2026-04-15-phase-b-orchestration-collapse-design.md`
- Verify: `docs/superpowers/specs/2026-04-15-phase-c-engine-channel-store-plugins-design.md`
- Verify: `docs/superpowers/specs/2026-04-15-phase-d-context-spill-to-rag-design.md`

**Note:** Commit `12198f7` ("align phase a/b/c/d specs with phase 0 doctrine") already applied these deltas. This task verifies completeness against the parent spec §4 and fills any gaps.

- [ ] **Step 1: Verify Phase A deltas**

Read `2026-04-15-phase-a-tool-plugin-architecture-design.md` and confirm:
- §2 Goal starts with bullet 0 referencing "hands" principle ✓
- §4.1 mentions `ToolCtx` carries `pub scope: &'a ToolScope`
- §4.1 mentions `PluginCtx` carries scope map
- §4.9 or §4.10 describes scope enforcement lint (cross-ref P0-7)
- Migration table has a note about +5 LOC per tool for scope enforcement

If any are missing, add them. If all present, move on.

- [ ] **Step 2: Verify Phase B deltas**

Read `2026-04-15-phase-b-orchestration-collapse-design.md` and confirm:
- §2 Goal starts with bullet 0 referencing "logic" principle ✓
- §4.3 is a cross-reference to Phase 0 (not a full skill-creator section)
- Migration table B1 is marked `[moved to Phase 0]` or equivalent
- §3 Non-goals includes "Skill-creator installation — moved to Phase 0"

If any are missing, add them. If all present, move on.

- [ ] **Step 3: Verify Phase C deltas**

Read `2026-04-15-phase-c-engine-channel-store-plugins-design.md` and confirm:
- §2 Goal starts with bullet 0 referencing "skeleton" principle ✓
- §4.2 mentions `ChannelCtx` carries scope map
- §4.1 clarifies engines do not carry `ToolScope`

If any are missing, add them. If all present, move on.

- [ ] **Step 4: Verify Phase D deltas**

Read `2026-04-15-phase-d-context-spill-to-rag-design.md` and confirm:
- §2 Goal starts with bullet 0 referencing "brain" principle ✓
- §4.1 has a brain-assembly knob note
- §5.3 notes orchestration-skill paragraph is governed by skill-eval

If any are missing, add them. If all present, move on.

- [ ] **Step 5: Commit if changes were needed**

If any gaps were filled:
```bash
git add docs/superpowers/specs/
git commit -m "fill remaining doctrine alignment gaps in phase A/B/C/D specs"
```

If no gaps found, skip this step — the work was already done in `12198f7`.

---

## Lane 2 — Code

### Task 3: `ToolScope` type + enforcement helpers (P0-3)

**Files:**
- Modify: `src/adapters/ports.rs:1-84`
- Test: inline `#[cfg(test)]` in `src/adapters/ports.rs`

- [ ] **Step 1: Write the failing tests**

Add imports and a test module at the bottom of `src/adapters/ports.rs`. These tests reference `ToolScope` which doesn't exist yet, so they'll fail to compile.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn scope_with_fs(roots: &[&str]) -> ToolScope {
        ToolScope {
            fs_roots: roots.iter().map(PathBuf::from).collect(),
            ..Default::default()
        }
    }

    // -- filesystem checks --

    #[test]
    fn fs_allowed_root_passes() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let scope = ToolScope {
            fs_roots: vec![root.clone()],
            ..Default::default()
        };
        let file = root.join("test.txt");
        fs::write(&file, "hello").unwrap();
        assert!(scope.check_fs_read(&file).is_ok());
    }

    #[test]
    fn fs_disallowed_rejects() {
        let tmp = TempDir::new().unwrap();
        let scope = ToolScope {
            fs_roots: vec![tmp.path().join("allowed")],
            ..Default::default()
        };
        let outside = tmp.path().join("outside.txt");
        fs::write(&outside, "hello").unwrap();
        assert!(scope.check_fs_read(&outside).is_err());
    }

    #[test]
    fn fs_subdir_passes() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let sub = root.join("deep").join("nested");
        fs::create_dir_all(&sub).unwrap();
        let file = sub.join("file.txt");
        fs::write(&file, "data").unwrap();
        let scope = ToolScope {
            fs_roots: vec![root],
            ..Default::default()
        };
        assert!(scope.check_fs_read(&file).is_ok());
    }

    #[test]
    fn fs_traversal_rejects() {
        let tmp = TempDir::new().unwrap();
        let allowed = tmp.path().join("allowed");
        fs::create_dir_all(&allowed).unwrap();
        let scope = ToolScope {
            fs_roots: vec![allowed],
            ..Default::default()
        };
        // Traverse out of allowed root
        let escaped = tmp.path().join("allowed").join("..").join("outside.txt");
        fs::write(tmp.path().join("outside.txt"), "data").unwrap();
        assert!(scope.check_fs_read(&escaped).is_err());
    }

    #[test]
    fn fs_nonexistent_leaf_passes() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let scope = ToolScope {
            fs_roots: vec![root.clone()],
            ..Default::default()
        };
        // Parent exists, leaf does not
        let new_file = root.join("new-doc.md");
        assert!(scope.check_fs_write(&new_file).is_ok());
    }

    #[test]
    fn fs_write_same_logic_as_read() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let file = root.join("test.txt");
        fs::write(&file, "hello").unwrap();
        let scope = ToolScope {
            fs_roots: vec![root],
            ..Default::default()
        };
        assert!(scope.check_fs_write(&file).is_ok());
    }

    // -- network checks --

    #[test]
    fn net_exact_match() {
        let scope = ToolScope {
            net_hosts: vec!["api.linear.app".into()],
            ..Default::default()
        };
        assert!(scope.check_net_host("api.linear.app").is_ok());
    }

    #[test]
    fn net_wildcard_match() {
        let scope = ToolScope {
            net_hosts: vec!["*.anthropic.com".into()],
            ..Default::default()
        };
        assert!(scope.check_net_host("api.anthropic.com").is_ok());
    }

    #[test]
    fn net_wildcard_no_bare() {
        let scope = ToolScope {
            net_hosts: vec!["*.anthropic.com".into()],
            ..Default::default()
        };
        // Wildcard requires at least one subdomain
        assert!(scope.check_net_host("anthropic.com").is_err());
    }

    #[test]
    fn net_empty_rejects_all() {
        let scope = ToolScope::default();
        assert!(scope.check_net_host("anything.com").is_err());
    }

    // -- env checks --

    #[test]
    fn env_allowed_passes() {
        let scope = ToolScope {
            env_reads: vec!["API_KEY".into()],
            ..Default::default()
        };
        assert!(scope.check_env_read("API_KEY").is_ok());
    }

    #[test]
    fn env_unlisted_rejects() {
        let scope = ToolScope {
            env_reads: vec!["API_KEY".into()],
            ..Default::default()
        };
        assert!(scope.check_env_read("SECRET_TOKEN").is_err());
    }

    // -- shell checks --

    #[test]
    fn shell_basename_match() {
        let scope = ToolScope {
            shell_bins: vec!["git".into()],
            ..Default::default()
        };
        assert!(scope.check_shell_bin("/usr/bin/git").is_ok());
        assert!(scope.check_shell_bin("git").is_ok());
    }

    #[test]
    fn shell_unlisted_rejects() {
        let scope = ToolScope {
            shell_bins: vec!["git".into()],
            ..Default::default()
        };
        assert!(scope.check_shell_bin("rm").is_err());
    }

    // -- wallet checks --

    #[test]
    fn wallet_exact_match() {
        let scope = ToolScope {
            wallets: vec!["research-wallet".into()],
            ..Default::default()
        };
        assert!(scope.check_wallet("research-wallet").is_ok());
    }

    #[test]
    fn wallet_unlisted_rejects() {
        let scope = ToolScope {
            wallets: vec!["research-wallet".into()],
            ..Default::default()
        };
        assert!(scope.check_wallet("prod-wallet").is_err());
    }

    // -- default deny --

    #[test]
    fn default_denies_all() {
        let scope = ToolScope::default();
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("any.txt");
        fs::write(&file, "data").unwrap();

        assert!(scope.check_fs_read(&file).is_err());
        assert!(scope.check_fs_write(&file).is_err());
        assert!(scope.check_net_host("any.com").is_err());
        assert!(scope.check_env_read("ANY").is_err());
        assert!(scope.check_shell_bin("any").is_err());
        assert!(scope.check_wallet("any").is_err());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib -p tengu-cluster -- ports::tests 2>&1 | tail -10`
Expected: FAIL — `ToolScope` not found, `check_fs_read` not found, etc.

- [ ] **Step 3: Add imports to `ports.rs`**

Add these imports at the top of `src/adapters/ports.rs`, after the existing `use` statements (after line 12):

```rust
use serde::Deserialize;
use std::path::{Path, PathBuf};
```

- [ ] **Step 4: Add `ToolScope` struct**

Add after the last trait definition (after line 83, before the closing blank line) in `src/adapters/ports.rs`:

```rust
// ---------------------------------------------------------------------------
// ToolScope — default-deny, per-tool access control
// ---------------------------------------------------------------------------

/// Fine-grained scope for tool execution. Default-deny: every field empty
/// means the tool can do nothing. Config must grant access explicitly.
///
/// This type is defined by Phase 0 and consumed by Phase A's `ToolCtx`.
/// Subject to refinement during Phase A if additional fields are needed.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct ToolScope {
    /// Allowed filesystem roots. Every fs-touching tool must reject
    /// paths that, after canonicalization, do not start with one of these.
    /// Empty = no fs access.
    #[serde(default)]
    pub fs_roots: Vec<PathBuf>,

    /// Allowed outbound host patterns (exact or glob: "api.linear.app",
    /// "*.anthropic.com"). Empty = no network access.
    #[serde(default)]
    pub net_hosts: Vec<String>,

    /// Env var names the tool is allowed to read (via SecretVault $VAR refs).
    /// Empty = no env reads.
    #[serde(default)]
    pub env_reads: Vec<String>,

    /// For run_command: allowed binary names (basename match).
    /// Empty = no shell execution.
    #[serde(default)]
    pub shell_bins: Vec<String>,

    /// For crypto tools: allowed wallet labels.
    /// Empty = no crypto access.
    #[serde(default)]
    pub wallets: Vec<String>,
}
```

- [ ] **Step 5: Add `check_*` methods**

Add the impl block right after the struct:

```rust
impl ToolScope {
    /// Validate a filesystem path for read access.
    /// Canonicalizes the path (walking up for non-existent leaves) and checks
    /// it falls under at least one `fs_roots` entry.
    pub(crate) fn check_fs_read(&self, path: &Path) -> anyhow::Result<PathBuf> {
        self.check_fs(path, "read")
    }

    /// Validate a filesystem path for write access.
    /// Same logic as read — split into two methods so Phase A can diverge
    /// them later if needed (e.g., write might check disk space).
    pub(crate) fn check_fs_write(&self, path: &Path) -> anyhow::Result<PathBuf> {
        self.check_fs(path, "write")
    }

    /// Validate an outbound host against allowed patterns.
    /// Supports exact match and single leading-wildcard (`*.anthropic.com`).
    pub(crate) fn check_net_host(&self, host: &str) -> anyhow::Result<()> {
        for pattern in &self.net_hosts {
            if pattern == host {
                return Ok(());
            }
            if let Some(suffix) = pattern.strip_prefix("*.") {
                // Wildcard: host must end with .suffix and have at least one
                // char before the dot (bare domain doesn't match *.domain).
                if let Some(prefix) = host.strip_suffix(suffix) {
                    if prefix.ends_with('.') && prefix.len() > 1 {
                        return Ok(());
                    }
                }
            }
        }
        anyhow::bail!(
            "host '{}' not in allowed net_hosts {:?}",
            host,
            self.net_hosts
        )
    }

    /// Validate an environment variable name against allowed reads.
    pub(crate) fn check_env_read(&self, var: &str) -> anyhow::Result<()> {
        if self.env_reads.iter().any(|v| v == var) {
            return Ok(());
        }
        anyhow::bail!(
            "env var '{}' not in allowed env_reads {:?}",
            var,
            self.env_reads
        )
    }

    /// Validate a shell binary against allowed binaries (basename match).
    pub(crate) fn check_shell_bin(&self, bin: &str) -> anyhow::Result<()> {
        let basename = Path::new(bin)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(bin);
        if self.shell_bins.iter().any(|b| b == basename) {
            return Ok(());
        }
        anyhow::bail!(
            "binary '{}' not in allowed shell_bins {:?}",
            bin,
            self.shell_bins
        )
    }

    /// Validate a wallet label against allowed wallets (exact match).
    pub(crate) fn check_wallet(&self, label: &str) -> anyhow::Result<()> {
        if self.wallets.iter().any(|w| w == label) {
            return Ok(());
        }
        anyhow::bail!(
            "wallet '{}' not in allowed wallets {:?}",
            label,
            self.wallets
        )
    }

    // -- private helpers --

    fn check_fs(&self, path: &Path, op: &str) -> anyhow::Result<PathBuf> {
        if self.fs_roots.is_empty() {
            anyhow::bail!(
                "fs {} denied for '{}': no fs_roots configured (default-deny)",
                op,
                path.display()
            );
        }

        // Canonicalize: walk up to the nearest existing ancestor, then
        // re-append the non-existent tail. This lets write_file to a new
        // file inside an allowed root succeed.
        let canonical = self.canonicalize_with_walkup(path)?;

        for root in &self.fs_roots {
            let canonical_root = if root.exists() {
                root.canonicalize().unwrap_or_else(|_| root.clone())
            } else {
                root.clone()
            };
            if canonical.starts_with(&canonical_root) {
                return Ok(canonical);
            }
        }
        anyhow::bail!(
            "fs {} denied for '{}' (resolved: '{}'): not under any allowed fs_roots {:?}",
            op,
            path.display(),
            canonical.display(),
            self.fs_roots
        )
    }

    fn canonicalize_with_walkup(&self, path: &Path) -> anyhow::Result<PathBuf> {
        // If the full path exists, just canonicalize it.
        if path.exists() {
            return path.canonicalize().map_err(Into::into);
        }

        // Walk up until we find an existing ancestor.
        let mut existing = path.to_path_buf();
        let mut tail_parts: Vec<std::ffi::OsString> = Vec::new();
        loop {
            if existing.exists() {
                let mut result = existing.canonicalize()?;
                for part in tail_parts.into_iter().rev() {
                    result.push(part);
                }
                return Ok(result);
            }
            match existing.file_name() {
                Some(name) => {
                    tail_parts.push(name.to_os_string());
                    existing.pop();
                }
                None => {
                    // Reached filesystem root without finding an existing dir.
                    // Return the original path as-is.
                    return Ok(path.to_path_buf());
                }
            }
        }
    }
}
```

- [ ] **Step 6: Add `tempfile` dev-dependency**

Check if `tempfile` is already in `Cargo.toml` under `[dev-dependencies]`. If not, add it:

Run: `grep tempfile Cargo.toml`

If not present, add to `Cargo.toml`:
```toml
[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 7: Run tests to verify they pass**

Run: `cargo test --lib -p tengu-cluster -- ports::tests 2>&1`
Expected: all 16 tests pass.

- [ ] **Step 8: Run full test suite + build**

Run: `cargo build && cargo test 2>&1 | tail -20`
Expected: full build succeeds, all tests pass (existing + new).

- [ ] **Step 9: Smoke test TUI**

Run: `cargo run` (or however TUI is launched), send a test message, confirm it works. Kill with Ctrl-C.

- [ ] **Step 10: Commit**

```bash
git add src/adapters/ports.rs Cargo.toml Cargo.lock
git commit -m "add ToolScope type with default-deny enforcement helpers

Six check_* methods (fs_read, fs_write, net_host, env_read, shell_bin,
wallet) with path canonicalization and clear error messages. 16 unit
tests including default-deny proof. No callers yet -- Phase A wires
this into ToolCtx."
```

---

### Task 4: Config schema for scopes (P0-4)

**Files:**
- Modify: `src/adapters/config.rs:112-137` (Config struct)
- Modify: `src/adapters/config.rs:206-241` (AgentConfig struct)
- Modify: `src/adapters/config.rs:917-955` (Config::default)
- Test: inline `#[cfg(test)]` in `src/adapters/config.rs`

- [ ] **Step 1: Write the failing tests**

Add these tests inside the existing `#[cfg(test)] mod tests` block in `src/adapters/config.rs` (after the last existing test, before the closing `}`):

```rust
    #[test]
    fn existing_config_no_scopes_parses() {
        // Backwards compatibility: config without any scopes fields must
        // still parse and validate.
        let toml_str = r#"
            runtime_profile = "auto"
            [hub]
            bind = "127.0.0.1"
            port = 7070
            auth_mode = "token"

            [agents.main]
            default = true
            engine = "openrouter"
            model = "anthropic/claude-sonnet-4.5"
        "#;
        let config: Config = toml::from_str(toml_str).expect("should parse");
        assert!(config.validate().is_ok());
        assert!(config.agents["main"].scopes.is_empty());
        assert!(config.default_scopes.is_empty());
    }

    #[test]
    fn agent_scopes_roundtrip() {
        let toml_str = r#"
            runtime_profile = "auto"

            [agents.main]
            default = true
            engine = "openrouter"
            model = "anthropic/claude-sonnet-4.5"

            [agents.main.scopes.write_file]
            fs_roots = ["./research", "./drafts"]

            [agents.main.scopes.http_request]
            net_hosts = ["api.linear.app", "*.anthropic.com"]
            env_reads = ["LINEAR_API_KEY"]
        "#;
        let config: Config = toml::from_str(toml_str).expect("should parse");
        let scopes = &config.agents["main"].scopes;
        assert_eq!(scopes["write_file"].fs_roots.len(), 2);
        assert_eq!(scopes["http_request"].net_hosts.len(), 2);
        assert_eq!(scopes["http_request"].env_reads.len(), 1);
    }

    #[test]
    fn default_scopes_roundtrip() {
        let toml_str = r#"
            runtime_profile = "auto"

            [agents.main]
            default = true
            engine = "openrouter"
            model = "anthropic/claude-sonnet-4.5"

            [default_scopes.read_file]
            fs_roots = ["./"]

            [default_scopes.run_command]
            shell_bins = ["git", "rg", "cargo"]
            fs_roots = ["./"]
        "#;
        let config: Config = toml::from_str(toml_str).expect("should parse");
        assert_eq!(config.default_scopes["read_file"].fs_roots.len(), 1);
        assert_eq!(config.default_scopes["run_command"].shell_bins.len(), 3);
    }

    #[test]
    fn resolve_scope_agent_overrides_default() {
        let toml_str = r#"
            runtime_profile = "auto"

            [default_scopes.write_file]
            fs_roots = ["./global"]

            [agents.main]
            default = true
            engine = "openrouter"
            model = "anthropic/claude-sonnet-4.5"

            [agents.main.scopes.write_file]
            fs_roots = ["./agent-specific"]
        "#;
        let config: Config = toml::from_str(toml_str).expect("should parse");
        let scope = config.resolve_scope("main", "write_file").unwrap();
        assert_eq!(scope.fs_roots.len(), 1);
        assert_eq!(scope.fs_roots[0].to_str().unwrap(), "./agent-specific");
    }

    #[test]
    fn resolve_scope_falls_back_to_default() {
        let toml_str = r#"
            runtime_profile = "auto"

            [default_scopes.read_file]
            fs_roots = ["./"]

            [agents.main]
            default = true
            engine = "openrouter"
            model = "anthropic/claude-sonnet-4.5"
        "#;
        let config: Config = toml::from_str(toml_str).expect("should parse");
        let scope = config.resolve_scope("main", "read_file").unwrap();
        assert_eq!(scope.fs_roots.len(), 1);
    }

    #[test]
    fn resolve_scope_none_when_absent() {
        let config = Config::default();
        assert!(config.resolve_scope("main", "write_file").is_none());
    }

    #[test]
    fn validate_passes_with_scopes() {
        let toml_str = r#"
            runtime_profile = "auto"

            [default_scopes.read_file]
            fs_roots = ["./"]

            [agents.main]
            default = true
            engine = "openrouter"
            model = "anthropic/claude-sonnet-4.5"

            [agents.main.scopes.write_file]
            fs_roots = ["./research"]
        "#;
        let config: Config = toml::from_str(toml_str).expect("should parse");
        assert!(config.validate().is_ok());
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib -p tengu-cluster -- config::tests 2>&1 | tail -10`
Expected: FAIL — `scopes` field not found on `AgentConfig`, `default_scopes` not found on `Config`, `resolve_scope` method not found.

- [ ] **Step 3: Add import to `config.rs`**

Add after the existing `use` statements (after line 6 in `config.rs`):

```rust
use crate::adapters::ports::ToolScope;
```

- [ ] **Step 4: Add `default_scopes` field to `Config`**

In the `Config` struct (around line 136), add before the closing brace:

```rust
    /// Fallback scopes applied when an agent has no per-tool scope entry.
    /// Per-agent scopes override default_scopes wholesale (not field-merged).
    #[serde(default)]
    pub default_scopes: HashMap<String, ToolScope>,
```

- [ ] **Step 5: Add `scopes` field to `AgentConfig`**

In the `AgentConfig` struct (around line 240), add before the `claude_code` field:

```rust
    /// Per-tool scope restrictions (default-deny). Key = tool name.
    /// See `ToolScope` in `ports.rs` for field definitions.
    #[serde(default)]
    pub scopes: HashMap<String, ToolScope>,
```

- [ ] **Step 6: Update `Config::default()`**

In `Config::default()` (around line 944), add `default_scopes`:

```rust
        Self {
            runtime_profile: "auto".to_string(),
            hub: HubConfig::default(),
            agents,
            orchestrator: None,
            memory: MemoryConfig::default(),
            telegram: TelegramConfig::default(),
            scaffold: None,
            claude_code: None,
            default_scopes: HashMap::new(),
        }
```

In the `AgentConfig` inside `Config::default()` (around line 940), add `scopes`:

```rust
                workspace_tools: vec![],
                scopes: HashMap::new(),
                claude_code: None,
```

- [ ] **Step 7: Add `resolve_scope` method**

Add to the `impl Config` block (after `substitute_env_vars`, before the closing `}`):

```rust
    /// Resolve the effective ToolScope for a given agent + tool.
    /// Per-agent scopes override default_scopes wholesale (not field-merged).
    /// Returns None if neither agent nor default_scopes has an entry.
    pub fn resolve_scope(&self, agent_name: &str, tool_name: &str) -> Option<ToolScope> {
        if let Some(agent) = self.agents.get(agent_name) {
            if let Some(scope) = agent.scopes.get(tool_name) {
                return Some(scope.clone());
            }
        }
        self.default_scopes.get(tool_name).cloned()
    }
```

- [ ] **Step 8: Run tests to verify they pass**

Run: `cargo test --lib -p tengu-cluster -- config::tests 2>&1`
Expected: all tests pass (existing + 7 new).

- [ ] **Step 9: Run full test suite + build**

Run: `cargo build && cargo test 2>&1 | tail -20`
Expected: full build succeeds, all tests pass.

- [ ] **Step 10: Smoke test TUI**

Run: `cargo run`, send a test message, confirm it works. Your existing `config.toml` has no `scopes` or `default_scopes` — both default to empty, so nothing changes at runtime.

- [ ] **Step 11: Commit**

```bash
git add src/adapters/config.rs
git commit -m "add scopes and default_scopes to config schema

AgentConfig.scopes: HashMap<String, ToolScope> for per-tool restrictions.
Config.default_scopes: fallback scopes when agent has no entry.
resolve_scope() implements wholesale override rule. Both fields use
#[serde(default)] for backwards compatibility with existing configs."
```

---

### Task 5: Scope enforcement lint test placeholder (P0-7)

**Files:**
- Create: `tests/scope_lint.rs`

- [ ] **Step 1: Create the test file**

Create `tests/scope_lint.rs`:

```rust
//! Scope enforcement lint — structural placeholder.
//!
//! This test will assert that every tool's `execute` body begins with a
//! `ctx.scope.check_*()` call once Phase A introduces `ToolCtx`. Until then,
//! it verifies the test infrastructure works by reading source files.
//!
//! Phase A's first PR activates the real lint by uncommenting the pattern
//! check below and adding the first scoped tool.

use std::fs;
use std::path::Path;

/// Collect all `.rs` files under a directory, recursively.
fn collect_rs_files(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                files.extend(collect_rs_files(&path));
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                files.push(path);
            }
        }
    }
    files
}

#[test]
fn can_read_all_adapter_sources() {
    let adapters_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/adapters");
    let files = collect_rs_files(&adapters_dir);

    // Sanity: we should find at least ports.rs, config.rs, types.rs
    assert!(
        files.len() >= 3,
        "Expected at least 3 .rs files in src/adapters/, found {}",
        files.len()
    );

    // Verify every file is readable
    for file in &files {
        let content = fs::read_to_string(file)
            .unwrap_or_else(|e| panic!("Failed to read {}: {}", file.display(), e));
        assert!(
            !content.is_empty(),
            "File {} is empty",
            file.display()
        );
    }
}

// ==========================================================================
// Phase A activation: uncomment the test below when the first tool is
// migrated to use ToolCtx with scope enforcement.
// ==========================================================================
//
// #[test]
// fn every_tool_execute_checks_scope() {
//     use regex::Regex;
//
//     let adapters_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/adapters");
//     let files = collect_rs_files(&adapters_dir);
//
//     // Pattern: find `fn execute(` inside an impl block, then check the body
//     // contains `scope.check_` within the next ~10 lines.
//     let execute_re = Regex::new(r"fn execute\s*\(").unwrap();
//     let scope_check_re = Regex::new(r"scope\.check_").unwrap();
//
//     let mut violations = Vec::new();
//
//     for file in &files {
//         let content = fs::read_to_string(file).unwrap();
//         for (i, line) in content.lines().enumerate() {
//             if execute_re.is_match(line) {
//                 // Look at the next 10 lines for a scope check
//                 let window: String = content.lines()
//                     .skip(i)
//                     .take(10)
//                     .collect::<Vec<_>>()
//                     .join("\n");
//                 if !scope_check_re.is_match(&window) {
//                     violations.push(format!(
//                         "{}:{} — execute() without scope.check_*()",
//                         file.display(),
//                         i + 1
//                     ));
//                 }
//             }
//         }
//     }
//
//     assert!(
//         violations.is_empty(),
//         "Tools missing scope enforcement:\n{}",
//         violations.join("\n")
//     );
// }
```

- [ ] **Step 2: Run the test**

Run: `cargo test --test scope_lint 2>&1`
Expected: PASS — `can_read_all_adapter_sources` succeeds.

- [ ] **Step 3: Run full test suite**

Run: `cargo test 2>&1 | tail -10`
Expected: all tests pass (existing + ports + config + lint).

- [ ] **Step 4: Commit**

```bash
git add tests/scope_lint.rs
git commit -m "add scope enforcement lint test placeholder

Structural placeholder for the CI lint that will assert every tool
checks ToolScope on execute. Currently verifies test infrastructure
by reading all adapter sources. Real lint activates when Phase A
introduces ToolCtx."
```

---

## Lane 3 — Skills

### Task 6: Author `skill-creator` for tengu (P0-5)

**Files:**
- Create: `skills/skill-creator/SKILL.md`

- [ ] **Step 1: Create the skill directory**

Run: `ls skills/` to confirm existing skills are present. Then:

Run: `mkdir -p skills/skill-creator`

- [ ] **Step 2: Write SKILL.md**

Create `skills/skill-creator/SKILL.md`:

```markdown
---
name: skill-creator
description: Use when creating a new skill or modifying an existing skill for a tengu agent. Covers skill anatomy, frontmatter, naming, the three-tier hierarchy, and the create/modify workflow using workspace primitives.
---

# Skill Creator

Author and modify skills for tengu agents. Skills are the logic layer -- any strategy, workflow, playbook, or policy is a skill, not Rust.

## When to Use

- Creating a new skill from scratch
- Modifying or improving an existing skill
- Need to understand how skills work in tengu

## Skill Anatomy

Every skill is a directory containing at minimum one `SKILL.md` file:

```
skills/
  my-skill/
    SKILL.md              # Main file (required)
    references/           # Optional: heavy reference material
      api-docs.md
```

### Frontmatter (YAML)

Required fields:

| Field | Description |
|-------|-------------|
| `name` | Skill name. Letters, numbers, hyphens only. |
| `description` | Starts with "Use when...". Triggering conditions only -- never summarize what the skill does. Third person. |

Optional gating fields (prevent loading when prerequisites are missing):

| Field | Description |
|-------|-------------|
| `requires_bins` | List of binaries that must be on PATH (e.g. `["git", "cargo"]`) |
| `requires_env` | List of env vars that must be set (e.g. `["API_KEY"]`) |
| `os` | OS filter (e.g. `["macos", "linux"]`) |

### Two Skill Types

**Documentation skills** (most common): Frontmatter + SKILL.md body. The agent reads the skill content and follows the instructions. Progressive disclosure -- compact catalog entry in the system prompt, agent reads the full SKILL.md on demand.

**Shell skills**: Create named tools with execution templates. The skill defines a tool name, description, and a command template that the runtime injects as a callable tool.

## Three-Tier Hierarchy

Skills load from three locations, higher tiers shadow lower:

| Tier | Path | Use case |
|------|------|----------|
| Managed | `~/.tengu/skills/` | User's personal skills, shared across workspaces |
| Workspace | `.tengu/skills/` | Workspace-specific skills, checked into the repo |
| Project | `skills/` | Project-level skills at the repo root |

If two tiers have a skill with the same name, the higher tier wins.

## Writing Good Descriptions

The description is how agents decide whether to load your skill. It must answer: "Should I read this right now?"

**Rules:**
- Start with "Use when..."
- Describe triggering conditions, not the skill's workflow
- Include concrete symptoms, situations, and contexts
- Write in third person
- Keep under 500 characters

```yaml
# BAD: summarizes workflow -- agent may follow description instead of reading skill
description: Creates skills by analyzing requirements, writing frontmatter, then testing

# BAD: too vague
description: For skill management

# GOOD: triggering conditions only
description: Use when creating a new skill or modifying an existing skill for a tengu agent
```

## Create Flow

1. **Pick a name.** Verb-first, hyphenated: `rate-limit-handler`, `api-auditor`. Match what the skill does.
2. **Write frontmatter.** `name` + `description` (required). Add gating fields if the skill depends on external tools.
3. **Write the body.** Structure: Overview (1-2 sentences) -> When to Use -> Core Content -> Common Mistakes.
4. **Place in the correct tier.** Personal? `~/.tengu/skills/`. Workspace? `.tengu/skills/`. Project? `skills/`.
5. **Test.** Load the agent and ask it a question the skill should handle. Verify it uses the skill content.

## Modify Flow

1. **Read the existing skill.** Use `read_file` to load `SKILL.md`.
2. **Identify what to change.** Is the description not triggering correctly? Is the body missing a case?
3. **Edit.** Use `write_file` to update the skill.
4. **Test.** Same as create -- verify the agent uses the updated content correctly.

## The No-Compromise Test

Before putting logic in Rust, ask: "Does a non-engineer user need to change this behaviour by editing a markdown file, or by filing a PR?" If the answer is "markdown file," it is a skill, not Rust.

## Anti-Patterns

- **Rust-level policy in a skill.** Skills instruct the LLM; they don't compile. If the behaviour needs enforcement at the binary level, it belongs in Rust.
- **Workflow summary in description.** Agents may follow the description instead of reading the full skill. Keep descriptions to triggering conditions only.
- **External runtime dependencies.** Skills should use only tengu workspace primitives (`read_file`, `write_file`, `list_directory`, `run_command`). No Python scripts, no TypeScript, no external runtimes.
- **Overly long skills.** If a skill exceeds 500 words, split heavy reference into `references/` files.
```

- [ ] **Step 3: Verify build is unaffected**

Run: `cargo build 2>&1 | tail -5`
Expected: compiles with no errors (content-only change).

- [ ] **Step 4: Commit**

```bash
git add skills/skill-creator/SKILL.md
git commit -m "author skill-creator for tengu agents

Documentation skill teaching agents how to create and modify skills.
Covers skill anatomy, frontmatter, three-tier hierarchy, create/modify
workflow, description writing, and anti-patterns. Pure markdown, uses
only workspace primitives."
```

---

### Task 7: Author `skill-eval` for tengu (P0-6)

**Files:**
- Create: `skills/skill-eval/SKILL.md`

- [ ] **Step 1: Create the skill directory**

Run: `mkdir -p skills/skill-eval`

- [ ] **Step 2: Write SKILL.md**

Create `skills/skill-eval/SKILL.md`:

```markdown
---
name: skill-eval
description: Use when evaluating whether existing skills still work, checking for drift or stale references, or auditing the skill inventory across all tiers.
---

# Skill Eval

Evaluate the health of installed skills. This is the feedback half of the evolution loop -- skill-creator creates, skill-eval measures.

## When to Use

- Periodic audit of all installed skills
- After changing the workspace or tool configuration
- When a skill seems to not be triggering correctly
- Before a major release to catch stale skills

## Evaluation Procedure

### Step 1: Enumerate Skills

Walk the three-tier hierarchy and list every installed skill:

```
~/.tengu/skills/*/SKILL.md      # Managed tier
.tengu/skills/*/SKILL.md        # Workspace tier
skills/*/SKILL.md               # Project tier
```

Use `list_directory` on each tier path. For each skill found, use `read_file` to load the frontmatter.

### Step 2: Gating Checks (automated)

For each skill, check the gating metadata in its frontmatter:

| Field | Check | Command |
|-------|-------|---------|
| `requires_bins` | Is each binary on PATH? | `run_command: which <bin>` |
| `requires_env` | Is each env var set? | `run_command: printenv <var>` (or check if non-empty) |
| `os` | Does the current OS match? | Compare against runtime OS |

**Verdict:** `gate-pass` (all prerequisites met) or `gate-fail` (list specific missing dependencies).

### Step 3: Drift Checks (semi-automated)

Read the full SKILL.md body and check for stale references:

- **Tool references:** Does the skill mention tool names (e.g. `http_request`, `sign_and_send_transaction`) that no longer exist in the agent's tool registry?
- **File references:** Does the skill reference file paths or directories that no longer exist in the workspace? Use `list_directory` or `read_file` to verify.
- **Capability references:** Does the skill mention capabilities or features the agent no longer has?

**Verdict:** `pass` (no stale references) or `drift` (list specific stale references).

### Step 4: Dead Skill Detection

A skill is "dead" if it is installed but never triggered:

- **Heuristic 1:** No agent in the config has the skill's package in `skill_packages`. Check the workspace `config.toml`.
- **Heuristic 2:** The skill's description doesn't match any plausible user workflow for the configured agents.

**Verdict:** `active` or `dead` with reasoning.

## Report Format

Produce a table with one row per skill:

| Skill | Tier | Verdict | Reasoning | Action |
|-------|------|---------|-----------|--------|
| `aura-orchestrator` | project | pass | All gates pass, no drift | keep |
| `molecule-x402` | project | gate-fail | Missing env: X402_GATEWAY_URL | keep (conditional) |
| `old-workflow` | workspace | drift | References removed tool `plan_create` | revise |
| `unused-skill` | managed | dead | No agent has this in skill_packages | archive |

## Recommended Actions

| Action | When | What to do |
|--------|------|------------|
| **keep** | All checks pass | No action needed |
| **keep (conditional)** | Gate-fail but skill is otherwise valid | Document the missing prerequisites |
| **revise** | Drift detected | Use skill-creator in modify mode to update stale references |
| **archive** | Dead skill | Move out of active tier (e.g. to a `skills/_archive/` directory) |

## Limitations (Phase 0)

This version of skill-eval does NOT support:
- **LLM-judge mode** -- no automatic prompt replay or output comparison
- **Scheduled runs** -- eval runs only on explicit trigger
- **Automatic remediation** -- reports only, does not auto-fix

These capabilities may be added in future phases once there is real usage data to calibrate against.
```

- [ ] **Step 3: Verify build is unaffected**

Run: `cargo build 2>&1 | tail -5`
Expected: compiles with no errors (content-only change).

- [ ] **Step 4: Commit**

```bash
git add skills/skill-eval/SKILL.md
git commit -m "author skill-eval for tengu agents

Documentation skill teaching agents how to evaluate skill health.
Covers gating checks, drift detection, dead skill detection, and
report format. Conservative Phase 0 scope: no LLM-judge, no
scheduled runs."
```

---

## Completion Checklist

After all 7 tasks are done, verify the full Phase 0 success criteria:

- [ ] `docs/architecture.md` states the doctrine with the no-compromise corollary
- [ ] Phase A/B/C/D specs reference the doctrine in their Goal sections
- [ ] `ToolScope` exists in `src/adapters/ports.rs` with 16 passing unit tests
- [ ] `AgentConfig.scopes` and `Config.default_scopes` parse from TOML with backwards compatibility
- [ ] `Config::resolve_scope()` implements the wholesale override rule
- [ ] `tests/scope_lint.rs` exists and passes
- [ ] `skills/skill-creator/SKILL.md` is present and loadable by agents
- [ ] `skills/skill-eval/SKILL.md` is present and loadable by agents
- [ ] `cargo build && cargo test` passes
- [ ] TUI channel works (send a message, get a response)
- [ ] Telegram channel works (send a message, get a response)
- [ ] No changes to: `engine_builder.rs`, `channel_runtime.rs`, `subagent_builder.rs`, `telegram_builder.rs`, `chat_builder.rs`, `memory_builder.rs`, `skill_builder.rs`
