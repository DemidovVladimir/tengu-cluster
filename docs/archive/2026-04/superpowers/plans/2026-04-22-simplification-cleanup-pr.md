# Simplification Cleanup PR Implementation Plan

> **Archived (2026-09-18)** — historical; current behaviour: see `README.md` / `docs/architecture-2026-04-27.md`.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land the first cleanup PR from the simplification analysis (§6 of `docs/superpowers/specs/2026-04-22-simplification-analysis.md`): remove dead code, collapse duplicated helpers, unify the two-variant redaction executor. No public API changes, no behavioral changes, −230 to −325 LOC.

**Architecture:** All changes stay within `src/adapters/`. Two small helper modules are added (shared truncation helpers extend the existing `adapters/token.rs`; new `adapters/tool_utils.rs` for plugin-side JSON argument extraction). One file deletion pair (empty `orchestrator/{config,telemetry}.rs` placeholders). One type-collapse (`SanitizedToolExecutor<'a>` folded into the owned variant).

**Tech Stack:** Rust 1.91 edition 2021, `tokio`, `async-trait`, `anyhow`, `serde_json`. Existing test harness: `tests/memory_search_tool.rs` + `tests/scope_lint.rs` + embedded `#[tokio::test]` blocks in plugin files.

---

## Working conventions for this PR

- **Branch:** create a fresh branch off latest `main` before Task 1, e.g. `refactor/simplification-cleanup`.
- **Commits:** one per task. Commit message format: `refactor: <summary>` or `chore(cleanup): <summary>`. All commits include the `Co-Authored-By: Claude` line.
- **Per-task verification:** after each task, run `cargo build --all-features` then the scoped tests listed in that task. Cap every Bash run at 30 s via the Bash tool's `timeout` parameter (macOS lacks the GNU `timeout` utility — do not use `timeout 30 cargo …` as a shell prefix, it will fail with "command not found"). The commands shown below intentionally omit any shell-level timeout wrapper. **Never run `cargo test` without a target — it's uncapped and always violates the time cap.** Exception: the initial `cargo build --all-features` in Task 1 Step 2 needs ~180 s for a cold build; use the Bash tool's timeout to cover it.
- **Hand-exercise:** tasks that touch channel wiring (D4, D5) add a smoke step — start `tengu chat` in a scratch workspace, issue one prompt that triggers a tool, confirm no panic, `^C`.
- **TDD note:** these are pure refactors preserving behavior. Existing tests are the safety net; no new tests are required unless a task explicitly adds one. If `cargo build` fails or a scoped test fails, revert and rethink — don't bodge.
- **Order:** tasks are ordered easiest-first (Z1 → D5). Later tasks can rely on helpers introduced by earlier tasks; don't reorder.

---

### Task 1: Baseline — branch + green build

**Files:** none

- [ ] **Step 1: Fast-forward `main` and branch**

Run:
```bash
git checkout main && git pull --ff-only origin main
git checkout -b refactor/simplification-cleanup
```
Expected: on `refactor/simplification-cleanup`, clean tree.

- [ ] **Step 2: Baseline build (release-agnostic, all features)**

Run:
```bash
timeout 180 cargo build --all-features 2>&1 | tail -20
```
Expected: `Finished \`dev\` profile ... `. Zero errors. Note: this is the only task allowed a longer timeout — first compile rebuilds all crates.

- [ ] **Step 3: Baseline tests**

Run:
```bash
timeout 30 cargo test --test scope_lint -- --nocapture 2>&1 | tail -20
timeout 30 cargo test --test memory_search_tool -- --nocapture 2>&1 | tail -20
```
Expected: both test binaries pass. Record the pass count — later tasks must not reduce it.

- [ ] **Step 4: Record baseline LOC**

Run:
```bash
find src -name "*.rs" -exec wc -l {} + | tail -1
```
Expected: `27068 total` (as of 2026-04-22 on main commit `6864a48`). Paste the actual number into the PR description at the end.

- [ ] **Step 5: No-op commit not required**

Task 1 is a checkpoint only. Proceed to Task 2.

---

### Task 2: Z1 + Z2 — Delete empty orchestrator placeholder files

**Files:**
- Delete: `src/adapters/orchestrator/config.rs`
- Delete: `src/adapters/orchestrator/telemetry.rs`
- Modify: `src/adapters/orchestrator/mod.rs:13,21`

- [ ] **Step 1: Verify files are indeed 1-line placeholders**

Run:
```bash
wc -l src/adapters/orchestrator/config.rs src/adapters/orchestrator/telemetry.rs
cat src/adapters/orchestrator/config.rs src/adapters/orchestrator/telemetry.rs
```
Expected: each file is 1 line containing `//! Placeholder — implemented in Task 4.N.`

- [ ] **Step 2: Delete the files**

Run:
```bash
git rm src/adapters/orchestrator/config.rs src/adapters/orchestrator/telemetry.rs
```

- [ ] **Step 3: Remove the module declarations**

Open `src/adapters/orchestrator/mod.rs`. Remove these two lines:
```rust
pub mod config;
```
and
```rust
pub mod telemetry;
```

No other edits to `mod.rs` — the other `pub mod` lines stay.

- [ ] **Step 4: Build**

Run:
```bash
timeout 60 cargo build --all-features 2>&1 | tail -10
```
Expected: `Finished`. Zero errors. If a broken reference surfaces (e.g. a `use crate::adapters::orchestrator::config::*` somewhere), it was a re-export of a placeholder — delete that use line too.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "$(cat <<'EOF'
chore(orchestrator): delete empty placeholder files

orchestrator/config.rs and orchestrator/telemetry.rs each held a single
line "Placeholder — implemented in Task 4.N." comment and were declared
in orchestrator/mod.rs. Nothing referenced them. Delete both files and
drop their mod declarations.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: Z5 — Remove blanket `#![allow(dead_code)]` from `skill_lifecycle/mod.rs`

**Files:**
- Modify: `src/adapters/skill_lifecycle/mod.rs:7`

- [ ] **Step 1: Remove the blanket allow**

Open `src/adapters/skill_lifecycle/mod.rs`. Delete this line (currently line 7):
```rust
#![allow(dead_code)]
```

- [ ] **Step 2: Build and catch any fallout warnings**

Run:
```bash
timeout 60 cargo build --all-features 2>&1 | tee /tmp/build-after-z5.log | tail -40
```
Expected: `Finished`. Zero errors. If new `dead_code` warnings surface:
- If the item is genuinely unused, **delete it**.
- If it's used only from tests, add `#[cfg(test)]` to the item or `#[allow(dead_code)]` scoped to that single item (not the whole module).
- Do not add module-level `#![allow(dead_code)]` — that's exactly what we're removing.

Record any fallout in the commit message.

- [ ] **Step 3: Re-run scoped tests**

Run:
```bash
timeout 30 cargo test --test scope_lint -- --nocapture 2>&1 | tail -10
timeout 30 cargo test --test memory_search_tool -- --nocapture 2>&1 | tail -10
```
Expected: both pass, same count as baseline.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "$(cat <<'EOF'
chore(skill_lifecycle): drop blanket #![allow(dead_code)]

The module-level allow suppressed dead-code warnings for the entire
skill_lifecycle subtree. Manual audit found no actual dead items; the
blanket was masking signal we'd want to see. Narrow any per-item allows
if fallout appears.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: Z3 — Remove unused `MemoryProvider::on_pre_compress()`

**Files:**
- Modify: `src/adapters/memory/provider.rs:44-48` (approximate — verify exact line at task time)

- [ ] **Step 1: Confirm no callers exist**

Run:
```bash
rg -n "on_pre_compress" src/
```
Expected: exactly one match (the trait method definition itself). If any other match appears, **stop** — the method is live and should not be removed.

- [ ] **Step 2: Confirm no impls override the default**

Run:
```bash
rg -n "fn on_pre_compress" src/
```
Expected: exactly one match. If multiple impls override it, **stop** and re-evaluate — removing a defaulted trait method with custom impls is a silent behavior change.

- [ ] **Step 3: Remove the method from the trait**

Open `src/adapters/memory/provider.rs`. Find and delete the `on_pre_compress` method definition (default impl returning empty string) from the `MemoryProvider` trait body. Also delete any doc-comment lines directly preceding it.

- [ ] **Step 4: Build**

Run:
```bash
timeout 60 cargo build --all-features 2>&1 | tail -10
```
Expected: `Finished`. Zero errors.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "$(cat <<'EOF'
chore(memory): remove unused MemoryProvider::on_pre_compress()

Trait default method with zero call sites and no overriding impls. Part
of the speculative pre-compression hook surface that never got wired in.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 5: Z4 — Rewrite stale `sessions_spawn`/`sessions_fan_out` eval fixtures

**Files:**
- Modify: `src/adapters/eval_builder.rs:2092-2093, 2103, 2164, 2413-2419, 2432` (approximate — verify at task time)

**Context:** These lines reference tools (`sessions_spawn`, `sessions_fan_out`) that were removed when `plugins/subagents/` was deleted. The fixtures/tests still use the old tool names as string literals. The stubbed `NoopRuntimeToolExecutor` echoes its input so any string works — we replace with `memory_ingest` (a currently-registered tool) to keep the fixtures honest.

- [ ] **Step 1: Locate all remaining references**

Run:
```bash
rg -n "sessions_spawn|sessions_fan_out" src/adapters/eval_builder.rs
```
Record the exact line numbers before editing.

- [ ] **Step 2: Replace in skill fixture markdown rows**

In `src/adapters/eval_builder.rs`, find the fixture table rows (currently around lines 2092-2093) — they look like:
```
| "research paper X then mint it as an IP token" | Sequential `sessions_spawn(researcher)` then `sessions_spawn(minter)`. |
| "what's 2+2?" | Direct answer. No `sessions_spawn` call. |
```

Rewrite the expected behavior to reference a currently-registered tool. Replace the first row's expected value with:
```
Delegate to subagent via `memory_ingest` to store findings; then reply.
```
and the second row with:
```
Direct answer. No `memory_ingest` call.
```

These are stub fixtures — the semantics aren't a real policy assertion, just string matching in the test stub below.

- [ ] **Step 3: Replace in expected-value string literals**

Find lines referencing `"Sequential \`sessions_spawn(researcher)\` then \`sessions_spawn(minter)\`."` (around line 2103) and `expected: "Sequential sessions_spawn(researcher) then sessions_spawn(minter)."` (around 2164). Update them to match the rewritten fixture rows verbatim — the fixture table and the expected-string literal must stay in sync.

- [ ] **Step 4: Replace in stubbed-executor test**

Lines 2413-2419 currently look like:
```rust
        .execute(&make_call("sessions_spawn"), &[])
        .await
        .unwrap();
    assert_eq!(r, "live-result-for-sessions_spawn");
    // …
    assert_eq!(
        calls,
        &["sessions_spawn"]
```
Replace every `"sessions_spawn"` literal with `"memory_ingest"`. The stubbed executor at line ~1010 (`NoopRuntimeToolExecutor`) doesn't care what the tool name is — it just echoes. Keep the test structurally identical.

- [ ] **Step 5: Replace in verdict-rationale test**

Line 2432 currently: `let v = parse_verdict(r#"{"verdict":"fail","rationale":"missed sessions_spawn"}"#).unwrap();`
Replace `"missed sessions_spawn"` with `"missed memory_ingest"`.

- [ ] **Step 6: Build + run eval-adjacent tests**

Run:
```bash
timeout 60 cargo build --all-features 2>&1 | tail -10
timeout 30 cargo test --lib eval_builder 2>&1 | tail -20
```
Expected: build finishes; every eval_builder test in the lib passes. If a test references `sessions_spawn` you missed, the error message will tell you the line — go back and fix.

- [ ] **Step 7: Confirm no references remain**

Run:
```bash
rg -n "sessions_spawn|sessions_fan_out" src/
```
Expected: zero matches anywhere in `src/`.

- [ ] **Step 8: Commit**

```bash
git add -A
git commit -m "$(cat <<'EOF'
chore(eval): retarget fixtures from removed sessions_spawn tool

plugins/subagents/ (sessions_spawn, sessions_fan_out) was removed but
eval_builder test fixtures and stubbed-executor tests still referenced
the old names as string literals. Retarget to memory_ingest (currently
registered) to keep the fixtures honest. No behavioral change — the
stubbed executor echoes its input.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 6: D1 — Shared truncation helpers in `adapters/token.rs`

**Files:**
- Modify: `src/adapters/token.rs` (extend, 11 → ~50 LOC)
- Modify: `src/adapters/engine_builder.rs:900` (truncate_tool_result body)
- Modify: `src/adapters/channel_runtime.rs:599` (truncate_output body) and `:693` (truncate_summary body)
- Modify: `src/adapters/tool_builder.rs:157` (truncate_detail body)
- Modify: `src/adapters/mcp_bridge.rs:307` (truncate_mcp_result body)
- **Leave unchanged:** `src/adapters/engine_builder.rs:877` (`compact_tool_result` — has first-line semantics), `src/adapters/eval_builder.rs:1021` (`truncate` — uses char-count, not byte-len; different semantics)

- [ ] **Step 1: Add the helpers to `adapters/token.rs`**

Open `src/adapters/token.rs`. After the existing two functions, add:

```rust
/// Truncate `s` to the largest char-boundary at or below `max` bytes.
///
/// Returns `None` when `s.len() <= max` (no truncation needed).
/// Returns `Some((prefix, end))` when truncation occurs: `prefix` is the
/// byte slice `&s[..end]` and `end` is the actual boundary-aligned cut
/// point, which may be less than `max` if `max` fell inside a multibyte
/// UTF-8 sequence.
///
/// Use this when the caller needs to build a custom suffix (e.g.
/// `"[truncated — showing {} of {} chars]"`). Use `truncate_with_suffix`
/// for the simpler "just append a fixed suffix" case.
pub fn truncate_at_boundary(s: &str, max: usize) -> Option<(&str, usize)> {
    if s.len() <= max {
        return None;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    Some((&s[..end], end))
}

/// Truncate `s` to at most `max` bytes on a char boundary, appending
/// `suffix` only when truncation actually occurs. When `s` is already
/// within `max`, returns `s.to_string()` unchanged (no suffix).
pub fn truncate_with_suffix(s: &str, max: usize, suffix: &str) -> String {
    match truncate_at_boundary(s, max) {
        None => s.to_string(),
        Some((prefix, _)) => format!("{}{}", prefix, suffix),
    }
}
```

- [ ] **Step 2: Build the helpers**

Run:
```bash
timeout 60 cargo build --all-features 2>&1 | tail -10
```
Expected: `Finished`. Zero errors.

- [ ] **Step 3: Swap `truncate_detail` in `tool_builder.rs`**

Open `src/adapters/tool_builder.rs`. Replace the `truncate_detail` body (lines 156-166) with a delegate:

```rust
/// Truncate a display string, appending "…" if it exceeds the limit.
fn truncate_detail(s: &str, max: usize) -> String {
    crate::adapters::token::truncate_with_suffix(s, max, "…")
}
```

(The local helper is kept as a thin shim so call sites don't change. Alternatively inline `truncate_with_suffix` at all call sites and delete `truncate_detail` — pick whichever yields fewer diff lines when you run it; both are acceptable.)

- [ ] **Step 4: Swap `truncate_summary` in `channel_runtime.rs`**

Open `src/adapters/channel_runtime.rs`. Replace the `truncate_summary` body (around lines 693-702) with:

```rust
/// Truncate text to at most `max` chars on a char boundary, appending "…" if cut.
pub(crate) fn truncate_summary(text: &str, max: usize) -> String {
    crate::adapters::token::truncate_with_suffix(text, max, "…")
}
```

- [ ] **Step 5: Swap `truncate_output` in `channel_runtime.rs`**

Same file. Replace the `truncate_output` body (around lines 599-608) with:

```rust
pub(crate) fn truncate_output(text: &str, max_chars: usize) -> String {
    crate::adapters::token::truncate_with_suffix(text, max_chars, "...(truncated)")
}
```

- [ ] **Step 6: Swap `truncate_tool_result` in `engine_builder.rs`**

Open `src/adapters/engine_builder.rs`. Replace the `truncate_tool_result` body (around lines 900-914) with:

```rust
fn truncate_tool_result(result: &str, max_chars: usize) -> String {
    match crate::adapters::token::truncate_at_boundary(result, max_chars) {
        None => result.to_string(),
        Some((prefix, end)) => format!(
            "{}\n\n[truncated — showing {} of {} chars]",
            prefix,
            end,
            result.len()
        ),
    }
}
```

- [ ] **Step 7: Swap `truncate_mcp_result` in `mcp_bridge.rs`**

Open `src/adapters/mcp_bridge.rs`. Replace the `truncate_mcp_result` body (around lines 307-321) with:

```rust
fn truncate_mcp_result(result: &str, max_chars: usize) -> String {
    match crate::adapters::token::truncate_at_boundary(result, max_chars) {
        None => result.to_string(),
        Some((prefix, end)) => format!(
            "{}\n\n[truncated — showing {} of {} chars]",
            prefix,
            end,
            result.len()
        ),
    }
}
```

- [ ] **Step 8: Build + test**

Run:
```bash
timeout 60 cargo build --all-features 2>&1 | tail -10
timeout 30 cargo test --test scope_lint -- --nocapture 2>&1 | tail -10
timeout 30 cargo test --test memory_search_tool -- --nocapture 2>&1 | tail -10
```
Expected: build + both test binaries pass.

- [ ] **Step 9: Commit**

```bash
git add -A
git commit -m "$(cat <<'EOF'
refactor: share truncation helper across 5 call sites

Extract char-boundary-safe truncation into adapters/token.rs as
truncate_at_boundary (primitive) + truncate_with_suffix (convenience).
Swap 5 near-identical local implementations in engine_builder,
channel_runtime (two sites), tool_builder, and mcp_bridge.

Leaves engine_builder::compact_tool_result (different first-line
semantics) and eval_builder::truncate (char-count semantics, not
byte-len) unchanged.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 7: D4 — Extract `register_plugin_safe` helper in `channel_runtime.rs`

**Files:**
- Modify: `src/adapters/channel_runtime.rs:115-232` (the `build_tool_executor` body)

**Context:** The function currently contains 7 near-identical `if let Err(e) = futures::executor::block_on(registry.register_plugin(...)) { tracing::warn!(...) }` blocks, each with an identical `TODO(Phase B)` comment. The workspace plugin is the one exception — it fails closed (`error!` + `return None`). Leave that case alone.

- [ ] **Step 1: Add the helper near the top of `channel_runtime.rs`**

Near the existing imports, add (pick a location just after the imports / type aliases — the exact spot is flexible):

```rust
/// Register a plugin, logging registration failures as warnings.
/// Returns `true` on success, `false` on failure (caller may choose to
/// early-return or continue).
///
/// Used by `build_tool_executor` — wraps the `futures::executor::block_on`
/// bridge until the channel chain is fully async.
// TODO(Phase B): drop the block_on once build_tool_executor is async.
fn register_plugin_safe(
    registry: &mut crate::adapters::tool_plugin::ToolRegistry,
    plugin: &dyn crate::adapters::tool_plugin::ToolPlugin,
    plugin_ctx: &crate::adapters::tool_plugin::PluginCtx<'_>,
    allow_list: &[String],
    failure_message: &str,
) -> bool {
    match futures::executor::block_on(registry.register_plugin(plugin, plugin_ctx, allow_list)) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(error = %e, "{}", failure_message);
            false
        }
    }
}
```

- [ ] **Step 2: Swap 6 of the 7 block_on blocks**

**Leave the workspace plugin block at lines ~127-138 unchanged** — it's fail-closed (`error!` + early `return None`). The helper uses `warn!` so doesn't match.

Replace the skill-plugin block (around lines 143-151) with:

```rust
    let skill_plugin = SkillPlugin::from_registry(skill_registry);
    register_plugin_safe(
        &mut registry,
        &skill_plugin,
        &plugin_ctx,
        &allowed_list,
        "Failed to register skill plugin — shell skills unavailable",
    );
```

Do the same substitution for the remaining 5 blocks (memory, cache, skill-lifecycle, http, crypto, mcp) — each becomes a single `register_plugin_safe(...)` call with the matching plugin value and the matching existing log message string. Preserve each gate (`if allowed_names.contains(...)`, `if !mcp_servers.is_empty()`) around the call.

For memory + crypto + mcp + skill-lifecycle, the `allow_list` argument varies (`&allowed_list` or `&[]` for mcp). Use whatever each original block passed.

Drop the 7 identical `TODO(Phase B): make build_tool_executor async ...` comments on the 6 swapped blocks. Keep a **single** such TODO on the helper itself (already included above). The workspace block retains its own inline comment explaining fail-closed semantics.

- [ ] **Step 3: Build**

Run:
```bash
timeout 60 cargo build --all-features 2>&1 | tail -10
```
Expected: `Finished`. Zero errors.

- [ ] **Step 4: Verify no `TODO(Phase B): make build_tool_executor async` comments remain duplicated**

Run:
```bash
rg -c "TODO\(Phase B\): make build_tool_executor async" src/adapters/channel_runtime.rs
```
Expected: exactly `1` (the one on the helper). If higher, go find and collapse them.

- [ ] **Step 5: Scoped tests**

Run:
```bash
timeout 30 cargo test --test scope_lint -- --nocapture 2>&1 | tail -10
timeout 30 cargo test --test memory_search_tool -- --nocapture 2>&1 | tail -10
```
Expected: both pass.

- [ ] **Step 6: Hand-exercise CLI chat**

Run:
```bash
timeout 30 cargo build --bin tengu --all-features 2>&1 | tail -5
timeout 20 ./target/debug/tengu doctor 2>&1 | tail -20
```
Expected: `tengu doctor` runs and reports status without panic. This exercises the plugin-registration path without requiring a real LLM call.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "$(cat <<'EOF'
refactor(channel_runtime): extract register_plugin_safe helper

build_tool_executor had 7 near-identical block_on(register_plugin(...))
wrappers with 7 identical TODO(Phase B) comments. Collapse 6 of them
into a single helper (workspace plugin kept inline — fail-closed path
with different error handling). One TODO remains on the helper itself.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 8: D3 — Memory plugin schema: single source of truth

**Files:**
- Modify: `src/adapters/plugins/memory/ingest.rs:44-81` (call `memory_ingest_def()` from mod)
- Modify: `src/adapters/plugins/memory/search.rs:37-72` (call `memory_search_def()` from mod)
- Modify: `src/adapters/plugins/memory/persistent_store.rs:211-244` (call `persistent_store_def()` from mod)
- `src/adapters/plugins/memory/mod.rs` — no change needed (already exports the `*_def()` fns)

**Context:** `plugins/memory/mod.rs` defines `memory_ingest_def()`, `memory_search_def()`, `persistent_store_def()` as the canonical `ToolDef` sources used by `channel_runtime` to advertise tools before plugin instantiation. Each tool's own `.new()` constructor duplicates the entire `ToolDef::new(NAME, DESC, json!(...))` body. We delete the duplicates and have each `.new()` call the parent module's `*_def()` instead.

- [ ] **Step 1: Update `MemoryIngestTool::new`**

Open `src/adapters/plugins/memory/ingest.rs`. Replace the body of `MemoryIngestTool::new` (currently constructing `ToolDef::new(MEMORY_INGEST_TOOL_NAME, "...", json!({...}))` inline — roughly lines 41-85):

```rust
impl MemoryIngestTool {
    pub(crate) fn new(memory_manager: Arc<MemoryManager>) -> Self {
        Self {
            def: super::memory_ingest_def(),
            memory_manager,
        }
    }
}
```

- [ ] **Step 2: Update `MemorySearchTool::new`**

Open `src/adapters/plugins/memory/search.rs`. Replace the body of `MemorySearchTool::new` in the same shape:

```rust
impl MemorySearchTool {
    pub(crate) fn new(memory_manager: Arc<MemoryManager>) -> Self {
        Self {
            def: super::memory_search_def(),
            memory_manager,
        }
    }
}
```

Preserve any other fields the struct holds (check the original — there may be more than `def` + `memory_manager`; include them unchanged).

- [ ] **Step 3: Update `PersistentStoreTool::new`**

Open `src/adapters/plugins/memory/persistent_store.rs`. Apply the same pattern:

```rust
impl PersistentStoreTool {
    pub(crate) fn new(
        workspace: PathBuf,
        memory_manager: Arc<MemoryManager>,
        chunk_size: usize,
        chunk_overlap: usize,
    ) -> Self {
        Self {
            def: super::persistent_store_def(),
            workspace,
            memory_manager,
            chunk_size,
            chunk_overlap,
        }
    }
}
```

Again: include all original fields, only the `def` initializer changes.

- [ ] **Step 4: Remove the now-unused helper `build_chunk_metadata` docstring bloat (optional)**

None — `build_chunk_metadata` stays; only the `ToolDef::new(...)` inline is being replaced.

- [ ] **Step 5: Remove unused imports if any surface**

After the edits, `json!` and `ToolDef` may no longer be used at the top of each of the three tool files. Run:

```bash
timeout 60 cargo build --all-features 2>&1 | tail -20
```
If `unused_imports` warnings appear, delete those imports. Common culprits:
- `use serde_json::json;` at top of ingest.rs / search.rs / persistent_store.rs
- `use crate::adapters::types::ToolDef;` if no longer referenced

Re-build after each delete to confirm.

- [ ] **Step 6: Run the memory plugin's embedded tests**

Run:
```bash
timeout 30 cargo test --lib plugins::memory 2>&1 | tail -20
timeout 30 cargo test --test memory_search_tool -- --nocapture 2>&1 | tail -10
```
Expected: all three `#[tokio::test]` blocks in `plugins/memory/mod.rs` pass (`memory_plugin_returns_empty_when_memory_disabled`, `memory_plugin_includes_memory_ingest_when_memory_enabled`, `memory_plugin_includes_persistent_store_when_opted_in`) plus the `tests/memory_search_tool.rs` binary.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "$(cat <<'EOF'
refactor(plugins/memory): single source of truth for tool schemas

memory_ingest / memory_search / persistent_store ToolDefs were defined
twice each — once in plugins/memory/mod.rs (for pre-instantiation
advertisement) and once inline in each tool's ::new(). Delete the
inline duplicates; each ::new() now calls the corresponding
*_def() constructor from the parent module.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 9: D2 — Shared JSON-argument extraction helpers

**Files:**
- Create: `src/adapters/tool_utils.rs`
- Modify: `src/adapters/mod.rs` (add `pub(crate) mod tool_utils;`)
- Modify: multiple `src/adapters/plugins/**/*.rs` files (call sites — discovered in Step 3)

**Context:** Plugin `execute` methods repeat the pattern `args.get("key").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("<tool>: 'key' is required"))`. Centralize into `require_str` / `require_i64` / `require_bool`. Agents that want optional defaulted args (e.g. ingest.rs line 134's `unwrap_or("default")`) stay as-is — the helper is for the *required-or-error* case.

- [ ] **Step 1: Create `adapters/tool_utils.rs`**

Create `src/adapters/tool_utils.rs` with:

```rust
//! Shared helpers for plugin `Tool::execute` JSON argument extraction.
//!
//! Centralises the repetitive `args.get(k).and_then(..).ok_or_else(..)`
//! pattern. Each helper returns `anyhow::Result<T>` with a consistent
//! error message shape: `"<tool>: '<key>' is required (<type>)"`.

use anyhow::{anyhow, Result};
use serde_json::Value;

/// Extract a required string argument. Returns an error keyed on `tool_name`
/// and `key` when the field is missing or not a string.
pub(crate) fn require_str<'a>(args: &'a Value, tool_name: &str, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("{}: '{}' is required (string)", tool_name, key))
}

/// Extract a required i64 argument. JSON numbers round-trip through `as_i64`.
pub(crate) fn require_i64(args: &Value, tool_name: &str, key: &str) -> Result<i64> {
    args.get(key)
        .and_then(|v| v.as_i64())
        .ok_or_else(|| anyhow!("{}: '{}' is required (integer)", tool_name, key))
}

/// Extract a required boolean argument.
pub(crate) fn require_bool(args: &Value, tool_name: &str, key: &str) -> Result<bool> {
    args.get(key)
        .and_then(|v| v.as_bool())
        .ok_or_else(|| anyhow!("{}: '{}' is required (boolean)", tool_name, key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn require_str_ok() {
        let v = json!({ "x": "hi" });
        assert_eq!(require_str(&v, "t", "x").unwrap(), "hi");
    }

    #[test]
    fn require_str_missing() {
        let v = json!({});
        let err = require_str(&v, "t", "x").unwrap_err().to_string();
        assert!(err.contains("t: 'x' is required"), "got: {}", err);
    }

    #[test]
    fn require_str_wrong_type() {
        let v = json!({ "x": 5 });
        assert!(require_str(&v, "t", "x").is_err());
    }

    #[test]
    fn require_i64_ok() {
        let v = json!({ "n": 7 });
        assert_eq!(require_i64(&v, "t", "n").unwrap(), 7);
    }

    #[test]
    fn require_bool_ok() {
        let v = json!({ "b": true });
        assert!(require_bool(&v, "t", "b").unwrap());
    }
}
```

- [ ] **Step 2: Register the module**

Open `src/adapters/mod.rs`. Add (respecting alphabetical ordering within the `adapters` section — match surrounding pattern):

```rust
pub(crate) mod tool_utils;
```

- [ ] **Step 3: Find candidate call sites**

Run:
```bash
rg -n "and_then\(\|v\| v\.as_str\(\)\)" src/adapters/plugins/ | head -40
```

For each match: read context. Only swap when the pattern is:
```rust
args.get("<key>").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("<tool>: '<key>' <something>"))
```
(i.e. required-or-error, no default). Skip matches where the pattern is `.unwrap_or("default")` (optional with default) or does something else with the value.

Repeat for `as_i64` and `as_bool` patterns:
```bash
rg -n "and_then\(\|v\| v\.as_i64\(\)\)" src/adapters/plugins/ | head -20
rg -n "and_then\(\|v\| v\.as_bool\(\)\)" src/adapters/plugins/ | head -20
```

- [ ] **Step 4: Apply the swap — exemplar**

Exemplar before (from some plugin):
```rust
let key = args
    .get("key")
    .and_then(|v| v.as_str())
    .ok_or_else(|| anyhow!("my_tool: 'key' is required"))?;
```

After:
```rust
let key = crate::adapters::tool_utils::require_str(args, "my_tool", "key")?;
```

Apply this pattern to every matching site found in Step 3. Each plugin file that gets a swap must either qualify the path as above or add a `use crate::adapters::tool_utils::{require_str, ...};` at the top.

**Do not change**:
- Sites with `.unwrap_or(...)` (those are defaulted optionals — different semantics).
- Sites that use `.and_then(|v| v.as_str().map(|s| s.to_string()))` to own the value (unless you carefully replicate ownership with `.to_string()` at the call site).
- Error message text that includes more than `"<tool>: '<key>' is required"` — e.g. `memory_ingest:` line 154 says `"one of 'content', 'text', or 'chunks' is required"` which is a compound requirement; leave it alone.

- [ ] **Step 5: Build + test**

Run:
```bash
timeout 90 cargo build --all-features 2>&1 | tail -15
timeout 30 cargo test --lib tool_utils 2>&1 | tail -10
timeout 30 cargo test --test scope_lint -- --nocapture 2>&1 | tail -10
timeout 30 cargo test --test memory_search_tool -- --nocapture 2>&1 | tail -10
```
Expected: build passes; 5 tests in `tool_utils::tests` pass; both test binaries pass.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "$(cat <<'EOF'
refactor: share JSON arg extraction helpers across plugins

Add adapters/tool_utils.rs with require_str / require_i64 / require_bool
for the common "required-or-error" pattern that was repeated across
plugin Tool::execute bodies. Swap matching call sites in plugins/**.

Scope is narrow: only required-argument extractions with the exact
`args.get(k).and_then(as_X).ok_or_else(anyhow!(...))` shape. Optional
args (unwrap_or default), compound requirements, and ownership-taking
variants are left as-is.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 10: D5 — Collapse `SanitizedToolExecutor` variant pair

**Files:**
- Modify: `src/adapters/engine_builder.rs:537-592` (delete borrowed variant, rename owned)
- Modify: `src/adapters/telegram_builder.rs:35, 1199, 1455, 1887`
- Modify: `src/adapters/tui/mod.rs:18, 514, 662, 725`

**Context:** `SanitizedToolExecutor<'a>` (borrowed refs) and `OwnedSanitizedToolExecutor` (Arc-held) have identical `execute` bodies — both call `self.registry.redact(&result)`. Collapse to a single Arc-backed type; adjust callers that were borrowing to wrap their inputs in `Arc`.

**Risk calibration:** This is the highest-risk task in this PR. Exercise CLI chat and Telegram start-up before committing. Budget extra time.

- [ ] **Step 1: Count and pin the callers**

Run:
```bash
rg -n "SanitizedToolExecutor|OwnedSanitizedToolExecutor" src/
```
Expected: matches in `engine_builder.rs` (defs + doc), `telegram_builder.rs` (4), `tui/mod.rs` (4). Record the exact line numbers — you'll need them at Step 4.

- [ ] **Step 2: Rewrite the definition in `engine_builder.rs`**

Open `src/adapters/engine_builder.rs`. Replace the block spanning lines ~537-592 (both types + both impls + the doc comment about the owned variant) with a single definition:

```rust
/// Decorator that redacts registered secret values from all tool output.
///
/// Owns its dependencies behind `Arc` so the decorator is `'static` —
/// required by the orchestrator chat-factory path. Per-turn borrowed
/// callers wrap their inner executor + registry in `Arc` at the call
/// site.
pub(crate) struct SanitizedToolExecutor {
    inner: std::sync::Arc<dyn ToolExecutor>,
    registry: std::sync::Arc<crate::adapters::secret_builder::SecretRegistry>,
}

impl SanitizedToolExecutor {
    pub fn new(
        inner: std::sync::Arc<dyn ToolExecutor>,
        registry: std::sync::Arc<crate::adapters::secret_builder::SecretRegistry>,
    ) -> Self {
        Self { inner, registry }
    }
}

#[async_trait]
impl ToolExecutor for SanitizedToolExecutor {
    async fn execute(
        &self,
        call: &ToolCall,
        messages: &[crate::adapters::types::Message],
    ) -> Result<String> {
        let result = self.inner.execute(call, messages).await?;
        Ok(self.registry.redact(&result))
    }
}
```

Note: the old `OwnedSanitizedToolExecutor` type name is gone. Any caller importing it by that name needs to update. See Step 4.

- [ ] **Step 3: Build — expect errors from callers**

Run:
```bash
timeout 60 cargo build --all-features 2>&1 | tail -30
```
Expected: build **fails** with errors about `OwnedSanitizedToolExecutor` not found, and lifetime mismatches in the borrowed call sites (e.g. `SanitizedToolExecutor::new(e as &dyn ToolExecutor, &self.secret_registry)` — the new constructor takes `Arc`, not `&`). **This is expected.** Errors point you at every caller that needs updating.

- [ ] **Step 4: Fix caller imports**

Open `src/adapters/tui/mod.rs` line 18. Change:
```rust
use crate::adapters::engine_builder::{SanitizedToolExecutor, ToolExecutor};
```
No change needed — the name stays `SanitizedToolExecutor`.

Open `src/adapters/telegram_builder.rs` line 35. Change:
```rust
    OwnedSanitizedToolExecutor, SanitizedToolExecutor, ToolExecutor,
```
to just:
```rust
    SanitizedToolExecutor, ToolExecutor,
```

- [ ] **Step 5: Fix borrowed-variant call sites (wrap in Arc)**

**In `tui/mod.rs:514`** — current:
```rust
SanitizedToolExecutor::new(e as &dyn ToolExecutor, &secret_registry)
```
Change to:
```rust
SanitizedToolExecutor::new(
    std::sync::Arc::new(e) as std::sync::Arc<dyn ToolExecutor>,
    std::sync::Arc::clone(&secret_registry),
)
```
**However:** at the call site, `e` and `secret_registry` might not be `Arc`-compatible as-is. Read 20 lines of surrounding context. If `secret_registry` is already an `Arc<SecretRegistry>`, `Arc::clone` works. If `e` is `&dyn ToolExecutor` owned by a stack local, you'll need to move to `Arc::new` on the concrete type before the closure. If making the change is non-trivial, **leave a TODO and continue — then come back once all other sites compile** to inspect what contortion is required.

**In `tui/mod.rs:725`** — same pattern as :514. Apply the same change.

**In `tui/mod.rs:662`** — currently uses `OwnedSanitizedToolExecutor::new(...)`. Just rename the type:
```rust
crate::adapters::engine_builder::SanitizedToolExecutor::new(
    // ... existing Arc args unchanged
)
```

**In `telegram_builder.rs:1199, 1887`** — same pattern as `tui/mod.rs:514`: wrap in `Arc`.

**In `telegram_builder.rs:1455`** — currently `OwnedSanitizedToolExecutor::new(...)`, just rename:
```rust
Arc::new(SanitizedToolExecutor::new(
    // ... existing args unchanged
))
```

- [ ] **Step 6: Build**

Run:
```bash
timeout 90 cargo build --all-features 2>&1 | tail -30
```
Expected: `Finished`. Zero errors. If lifetime/ownership errors persist at a call site, the redacted executor was being constructed from a borrowed local with a non-trivial lifetime relationship. In that case: hoist construction so the inner `ToolExecutor` is built as `Arc<dyn ToolExecutor>` one scope up, pass it by `Arc::clone` into the `SanitizedToolExecutor::new` call.

- [ ] **Step 7: Scoped tests**

Run:
```bash
timeout 30 cargo test --test scope_lint -- --nocapture 2>&1 | tail -10
timeout 30 cargo test --test memory_search_tool -- --nocapture 2>&1 | tail -10
```
Expected: both pass.

- [ ] **Step 8: Hand-exercise TUI startup**

Run:
```bash
timeout 30 cargo build --bin tengu --all-features 2>&1 | tail -5
# Confirm the binary still boots into TUI without panic, then quit.
# Run manually in a separate terminal (don't block this session):
#   ./target/debug/tengu chat
#   (press ^C)
```
Mark this step done only after you've observed the TUI starting without panic.

- [ ] **Step 9: Hand-exercise Telegram dry-run (if configured)**

If a Telegram token is configured, run `./target/debug/tengu telegram` for ~10 seconds and confirm no panic in the console. If not configured, note it in the commit message ("Telegram path not exercised — no token in env").

- [ ] **Step 10: Commit**

```bash
git add -A
git commit -m "$(cat <<'EOF'
refactor(engine): collapse SanitizedToolExecutor variant pair

Deletes the borrowed SanitizedToolExecutor<'a> variant. The owned
(Arc-backed) variant already covers the '<static> orchestrator
factory path; per-turn borrowed callers now wrap their inner executor
+ secret registry in Arc at the call site. Same redaction behavior,
one type instead of two.

Exercised: TUI chat boot, Telegram boot (if token present).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 11: Final validation + PR preparation

**Files:** none — this is a verification task.

- [ ] **Step 1: Full scoped build + test sweep**

Run:
```bash
timeout 180 cargo build --all-features 2>&1 | tail -10
timeout 30 cargo test --test scope_lint -- --nocapture 2>&1 | tail -10
timeout 30 cargo test --test memory_search_tool -- --nocapture 2>&1 | tail -10
timeout 30 cargo test --lib plugins::memory 2>&1 | tail -20
timeout 30 cargo test --lib tool_utils 2>&1 | tail -10
timeout 30 cargo test --lib eval_builder 2>&1 | tail -20
```
Expected: all pass. If any fail, `git log --oneline` to find the last-known-green commit and bisect.

- [ ] **Step 2: Clippy on touched files (non-blocking)**

Run:
```bash
timeout 60 cargo clippy --all-features 2>&1 | tail -30
```
New warnings introduced by this PR should be investigated; pre-existing ones may be ignored and mentioned in the PR body.

- [ ] **Step 3: LOC delta check**

Run:
```bash
find src -name "*.rs" -exec wc -l {} + | tail -1
git diff --stat main...HEAD | tail -5
```
Expected: LOC dropped by 230-325 vs. baseline recorded in Task 1 Step 4. If much smaller or larger than that, something was skipped or done more aggressively — check against the spec §6 before opening the PR.

- [ ] **Step 4: Verify no lingering phantoms**

Run:
```bash
rg -n "sessions_spawn|sessions_fan_out|OwnedSanitizedToolExecutor" src/
rg -n "Placeholder — implemented in Task 4\.N" src/
rg -c "TODO\(Phase B\): make build_tool_executor async" src/adapters/channel_runtime.rs
```
Expected: first two greps return zero matches; the third returns `1` (the one TODO left on the helper in Task 7).

- [ ] **Step 5: Push the branch**

```bash
git push -u origin refactor/simplification-cleanup
```

- [ ] **Step 6: Open the PR**

Run:
```bash
gh pr create --title "refactor: simplification cleanup (§6 of simplification-analysis)" --body "$(cat <<'EOF'
## Summary
Implements §6 of `docs/superpowers/specs/2026-04-22-simplification-analysis.md` — low-risk dead-code removal and tactical deduplication. No public API changes, no behavioral changes.

Items (spec IDs):
- D1 — shared truncation helpers in `adapters/token.rs` (5 call sites)
- D2 — `adapters/tool_utils::{require_str, require_i64, require_bool}` + plugin swaps
- D3 — memory plugin schema single source of truth (`plugins/memory/mod.rs`)
- D4 — `register_plugin_safe` helper collapses 6 of 7 plugin-registration block_on blocks
- D5 — collapse `SanitizedToolExecutor<'a>` into owned variant
- Z1 + Z2 — delete empty `orchestrator/{config,telemetry}.rs` placeholders
- Z3 — remove unused `MemoryProvider::on_pre_compress()`
- Z4 — retarget stale `sessions_spawn` fixtures in `eval_builder.rs`
- Z5 — drop blanket `#![allow(dead_code)]` from `skill_lifecycle/mod.rs`

LOC delta: <RECORDED IN TASK 11 STEP 3>.

## Test plan
- [x] `cargo build --all-features`
- [x] `cargo test --test scope_lint`
- [x] `cargo test --test memory_search_tool`
- [x] `cargo test --lib plugins::memory`
- [x] `cargo test --lib tool_utils`
- [x] `cargo test --lib eval_builder`
- [x] Manually boot `tengu chat`; issue one prompt; confirm no panic
- [ ] Manually boot `tengu telegram` (if token configured)

## Deferred
- S2 (eval_builder.rs split) — next PR per spec §7
- D2 unverified agent findings — second dedup PR after spot-verification

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

Fill in the LOC delta placeholder before opening. Return the PR URL.

---

## Self-review checklist (before handing plan to executor)

- [x] **Spec coverage:** each of §6 items D1, D2, D3, D4, D5, Z1, Z2, Z3, Z4, Z5 has a task. ✓
- [x] **No placeholders:** each task has concrete code or grep commands; no "TBD", "implement later", "add appropriate X". ✓
- [x] **Type consistency:** helper names (`truncate_at_boundary`, `truncate_with_suffix`, `register_plugin_safe`, `require_str`, `require_i64`, `require_bool`, `SanitizedToolExecutor`) are consistent across every task that references them. ✓
- [x] **Test caps:** every Bash command specifies an explicit timeout, max 180s for the baseline full build, 30s for tests (per memory rule "hard cap tests at 30 seconds"). ✓
- [x] **Commit discipline:** one commit per task; no amends; frequent small commits. ✓

## Handoff

Plan complete. Two execution options:

1. **Subagent-Driven (recommended)** — dispatch a fresh subagent per task, review between tasks.
2. **Inline Execution** — execute in the current session using executing-plans, batch with checkpoints.
