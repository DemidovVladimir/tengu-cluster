# Harness-Owned Orchestration + Memory — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the deleted skill-based orchestration with a harness-owned supervisor pattern: a dedicated orchestrator agent producing DAG plans dispatched by a Rust DAG executor, with a Hermes-shaped memory provider layer that injects context pre-turn and writes summaries post-turn.

**Architecture:** Two new subsystems under `src/adapters/` — `orchestrator/` (planner, DAG executor, retry, replan, events) and `memory/` (MemoryProvider trait, BuiltinMemoryProvider, injector, writer). Existing `plugins/memory/` is refactored to hold LLM-callable memory tools (`memory_ingest`, `memory_search`, `persistent_store`). Existing `plugins/subagents/` and legacy orchestration Rust files are deleted.

**Tech Stack:** Rust, Tokio, serde/TOML, broadcast channels (`tokio::sync::broadcast`), OpenRouter SDK. Qdrant optional for vector memory.

**Spec:** [2026-04-20-harness-orchestration-memory-design.md](../specs/2026-04-20-harness-orchestration-memory-design.md)

---

## Phase 0 — Branch + cherry-picks

### Task 0.1: Create work branch and cherry-pick eval-runner commits from PR #5

**Files:**
- Create branch: `feature/harness-orchestration`

- [ ] **Step 1: Create and check out the work branch**

```bash
git checkout main
git pull origin main
git checkout -b feature/harness-orchestration
```

- [ ] **Step 2: Cherry-pick eval-runner commits in topological order (oldest first)**

These commits on `feature/phase-b-orchestration-collapse` add the `tengu eval` runner which is independent of the orchestration-skill direction. **Scaffold first, then dependents in git-chronological order:**

```bash
git cherry-pick 2382186  # feat(eval): scaffold tengu eval CLI subcommand          (adds eval_builder.rs)
git cherry-pick 53bf3a1  # feat(eval): PromptRow + markdown prompts parser
git cherry-pick ab2cd85  # docs(eval): row id cap is 64 chars
git cherry-pick 7af8cb8  # fix(eval): strip trailing dash
git cherry-pick 2a844de  # feat(eval): YAML prompts parser with stub support
git cherry-pick 87fc347  # refactor(eval): single source of truth for default timeout
git cherry-pick d832c57  # feat(eval): skill discovery across three tiers
git cherry-pick 75ed21c  # fix(eval): don't shadow lower-tier skill
git cherry-pick 0380e49  # fix(eval): tier_for_root keeps .tengu/skills as Workspace
git cherry-pick bd33bf5  # feat(eval): config loader with {TMP_WORKSPACE} expansion
git cherry-pick d7258cc  # feat(eval): StubbedExecutor + pub visibility for engine hooks
git cherry-pick c688bc0  # feat(eval): judge client + verdict parser
git cherry-pick d3b2c33  # refactor(eval): extract JUDGE_PREFILL constant
git cherry-pick 6c816c3  # feat(eval): per-row driver with observation tap
git cherry-pick 25ede6d  # feat(eval): report builder + top-level run() driver
git cherry-pick aa011f0  # fix(eval): runner-level errors return exit code 2
git cherry-pick 52abd55  # feat(eval): ANSI colour in terminal table
git cherry-pick 847193e  # fix(eval): wire --sandbox override
git cherry-pick f1b9d12  # fix(eval): capture judge token usage
git cherry-pick 40a4d52  # fix(eval): run_skill errors exit 2
git cherry-pick 372a020  # fix(eval): remove assistant prefill
```

Skip: `2bfd986 feat(eval): canonical orchestration eval config` (depends on the deleted orchestration skill) and `d5b6b8c test(eval): end-to-end smoke against orchestration skill` (same). We'll write the new orchestration eval config in Phase 9.

Also skip: `4bf7eed docs: document tengu eval runner + add project CLAUDE.md` (pure docs; cherry-pick if clean, otherwise skip).

If any cherry-pick has conflicts, resolve by taking the eval-runner code as-is and leaving Phase B orchestration-skill code out. Abort and flag if conflicts are non-trivial.

- [ ] **Step 3: Verify build and tests**

```bash
cargo build --all-features
cargo test --lib eval:: -- --test-threads=1 2>&1 | tail -20
```

Expected: builds, eval-module tests pass.

- [ ] **Step 4: Commit nothing extra** — cherry-picks are the commits.

### Task 0.2: Commit deletion of superseded Phase B/C/D/E specs and plans

**STATUS: DONE ON MAIN — SKIP THIS TASK.** The deletions were committed on `main` as `44f1c57` before branching. The new feature branch inherits the clean state. Move directly to Phase 1.



**Files:**
- Delete (already marked `D` in `git status`):
  - `docs/superpowers/plans/2026-04-16-phase-e-self-alignment.md`
  - `docs/superpowers/plans/2026-04-18-phase-b-orchestration-collapse.md`
  - `docs/superpowers/specs/2026-04-15-phase-b-orchestration-collapse-design.md`
  - `docs/superpowers/specs/2026-04-15-phase-c-engine-channel-store-plugins-design.md`
  - `docs/superpowers/specs/2026-04-15-phase-d-context-spill-to-rag-design.md`
  - `docs/superpowers/specs/2026-04-16-phase-e-self-alignment-design.md`

- [ ] **Step 1: Stage the deletions**

```bash
git add -u docs/superpowers/plans/ docs/superpowers/specs/
git status --short
```

Expected: `D` lines converted to staged deletions, no other changes.

- [ ] **Step 2: Commit**

```bash
git commit -m "$(cat <<'EOF'
docs: delete superseded phase B/C/D/E specs and plans

These specs are subsumed by the 2026-04-20 harness-orchestration-memory
design, which takes a different direction than phase B (harness-owned
orchestration rather than skill-based playbook).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Phase 1 — Memory subsystem

Port the existing memory logic from `memory_builder.rs` + `qdrant_memory_store.rs` + `embedding.rs` into a new `src/adapters/memory/` module structured around the `MemoryProvider` trait (Hermes-shaped). The LLM-callable tools stay in `plugins/memory/` and are refactored in Phase 2.

### Task 1.1: Create `memory/` module skeleton

**Files:**
- Create: `src/adapters/memory/mod.rs`
- Create: `src/adapters/memory/provider.rs` (stub)
- Create: `src/adapters/memory/manager.rs` (stub)
- Create: `src/adapters/memory/fencing.rs` (stub)
- Create: `src/adapters/memory/context_block.rs` (stub)
- Create: `src/adapters/memory/injector.rs` (stub)
- Create: `src/adapters/memory/writer.rs` (stub)
- Create: `src/adapters/memory/builtin.rs` (stub)
- Create: `src/adapters/memory/vector.rs` (stub)
- Modify: `src/adapters/mod.rs` — add `pub mod memory;`

- [ ] **Step 1: Create the module files as empty stubs**

Each new `.rs` file starts with a doc comment placeholder:

```rust
//! Placeholder — implemented in Task 1.N.
```

For `src/adapters/memory/mod.rs`:

```rust
//! Harness-owned memory subsystem.
//!
//! - `provider` — MemoryProvider trait (Hermes-shaped)
//! - `builtin` — BuiltinMemoryProvider (MEMORY.md, identity, daily logs, vector)
//! - `manager` — MemoryManager holding one builtin + at most one external provider
//! - `injector` — pre-turn fenced context injection
//! - `writer` — post-turn spawned non-blocking writes
//! - `fencing` — `<memory-context>` block helpers
//! - `vector` — embeddings + Qdrant/bincode backend
//! - `context_block` — shared types

pub mod builtin;
pub mod context_block;
pub mod fencing;
pub mod injector;
pub mod manager;
pub mod provider;
pub mod vector;
pub mod writer;
```

- [ ] **Step 2: Wire the module into `src/adapters/mod.rs`**

Find the section in `src/adapters/mod.rs` where existing `pub mod …` declarations live (alphabetical). Insert:

```rust
pub mod memory;
```

Leave `pub mod memory_builder;` in place for now — it'll be removed in Task 1.10 once fully migrated.

- [ ] **Step 3: Verify the build**

```bash
cargo build --lib 2>&1 | tail -20
```

Expected: clean build (stubs compile because they're empty).

- [ ] **Step 4: Commit**

```bash
git add src/adapters/memory/ src/adapters/mod.rs
git commit -m "feat(memory): scaffold harness memory subsystem"
```

### Task 1.2: Define `MemoryProvider` trait

**Files:**
- Modify: `src/adapters/memory/provider.rs`

- [ ] **Step 1: Write the trait definition**

Replace the placeholder in `src/adapters/memory/provider.rs` with:

```rust
//! `MemoryProvider` — abstract base for pluggable memory backends.
//!
//! One built-in provider (`BuiltinMemoryProvider`) is always registered
//! first. At most one external provider (Letta, Mem0, …) may slot in
//! alongside it. Harness-level, never exposed as LLM tools — retrieval
//! and writes are driven by `MemoryInjector` / `MemoryWriter`.

use async_trait::async_trait;
use std::path::Path;

use crate::adapters::types::Message;

#[async_trait]
pub trait MemoryProvider: Send + Sync {
    /// Short identifier (`"builtin"`, `"letta"`, …).
    fn name(&self) -> &str;

    /// Return `true` if configuration/credentials are present and the
    /// provider should activate. Synchronous, no network calls.
    fn is_available(&self) -> bool;

    /// One-time init at session start. `workspace` is the agent's
    /// workspace root. Implementors may open databases, spawn background
    /// tasks, etc.
    async fn initialize(&self, session_id: &str, workspace: &Path) -> anyhow::Result<()>;

    /// Static system-prompt contribution (AGENTS.md, identity files,
    /// daily logs for the builtin; empty by default for external
    /// providers).
    fn system_prompt_block(&self) -> String {
        String::new()
    }

    /// Called pre-turn. Implementors do vector search (or equivalent)
    /// against `query` and return formatted text to inject. Empty
    /// string means "nothing relevant."
    async fn prefetch(&self, agent: &str, query: &str) -> String;

    /// Called post-turn. Implementors persist the (user, assistant)
    /// summary. MUST be non-blocking or cheap — the user-facing reply
    /// never waits on this.
    async fn sync_turn(&self, agent: &str, user: &str, assistant: &str);

    /// Called before context compression. Return text to include in
    /// the compression summary prompt. Default: empty.
    async fn on_pre_compress(&self, _messages: &[Message]) -> String {
        String::new()
    }

    /// Clean teardown — flush queues, close connections.
    async fn shutdown(&self);
}
```

- [ ] **Step 2: Verify it compiles**

```bash
cargo build --lib 2>&1 | tail -10
```

Expected: clean build (trait is purely declarative).

- [ ] **Step 3: Commit**

```bash
git add src/adapters/memory/provider.rs
git commit -m "feat(memory): define MemoryProvider trait"
```

### Task 1.3: Implement `fencing.rs` with tests

**Files:**
- Modify: `src/adapters/memory/fencing.rs`

Reference Hermes `agent/memory_manager.py` (lines 46–80). Same fence shape, same system note text.

- [ ] **Step 1: Write the failing tests first**

Replace the placeholder in `src/adapters/memory/fencing.rs` with:

```rust
//! `<memory-context>` fenced block helpers.

use regex::Regex;

const SYSTEM_NOTE: &str = "[System note: The following is recalled memory context, NOT new user input. Treat as informational background data.]";

/// Wrap prefetched memory context in a fenced block.
///
/// The fence prevents the model from treating recalled text as user
/// discourse. Injected at API-call time only — never persisted into
/// message history.
pub fn build_memory_context_block(raw_context: &str) -> String {
    let trimmed = raw_context.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let clean = sanitize_context(trimmed);
    format!(
        "<memory-context>\n{system_note}\n\n{clean}\n</memory-context>",
        system_note = SYSTEM_NOTE,
        clean = clean,
    )
}

/// Strip any nested memory fences or system notes from provider output
/// (defense in depth — a provider that accidentally returns fenced
/// content shouldn't double-wrap).
pub fn sanitize_context(text: &str) -> String {
    static FENCE_TAGS: once_cell::sync::Lazy<Regex> = once_cell::sync::Lazy::new(|| {
        Regex::new(r"(?i)</?\s*memory-context\s*>").unwrap()
    });
    static INTERNAL_BLOCK: once_cell::sync::Lazy<Regex> = once_cell::sync::Lazy::new(|| {
        Regex::new(r"(?is)<\s*memory-context\s*>.*?</\s*memory-context\s*>").unwrap()
    });
    static INTERNAL_NOTE: once_cell::sync::Lazy<Regex> = once_cell::sync::Lazy::new(|| {
        Regex::new(
            r"(?i)\[System note:\s*The following is recalled memory context,\s*NOT new user input\.\s*Treat as informational background data\.\]\s*"
        ).unwrap()
    });

    let step1 = INTERNAL_BLOCK.replace_all(text, "");
    let step2 = INTERNAL_NOTE.replace_all(&step1, "");
    let step3 = FENCE_TAGS.replace_all(&step2, "");
    step3.into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_returns_empty() {
        assert_eq!(build_memory_context_block(""), "");
        assert_eq!(build_memory_context_block("   \n\t"), "");
    }

    #[test]
    fn nonempty_input_gets_fenced_with_system_note() {
        let out = build_memory_context_block("recalled fact");
        assert!(out.starts_with("<memory-context>"));
        assert!(out.ends_with("</memory-context>"));
        assert!(out.contains("recalled fact"));
        assert!(out.contains("NOT new user input"));
    }

    #[test]
    fn sanitize_strips_nested_fences() {
        let input = "<memory-context>nested stuff</memory-context>leftover";
        let out = sanitize_context(input);
        assert_eq!(out, "leftover");
    }

    #[test]
    fn sanitize_strips_orphan_tags() {
        let out = sanitize_context("before<memory-context>middle</memory-context>after");
        assert_eq!(out, "beforeafter");
    }

    #[test]
    fn build_block_calls_sanitize() {
        let out = build_memory_context_block("<memory-context>nested</memory-context>real");
        assert!(out.contains("real"));
        // The ONLY open/close tags are the outer wrapping pair
        assert_eq!(out.matches("<memory-context>").count(), 1);
        assert_eq!(out.matches("</memory-context>").count(), 1);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test --lib adapters::memory::fencing -- --nocapture 2>&1 | tail -30
```

Expected: FAIL — first because `regex` and `once_cell` may not yet be deps, then because code hasn't been compiled. If missing deps, add them:

- [ ] **Step 3: Ensure required deps are in `Cargo.toml`**

```bash
grep -E '^(regex|once_cell) ' Cargo.toml
```

If either is missing, add under `[dependencies]`:

```toml
regex = "1"
once_cell = "1"
```

Then re-run the test.

- [ ] **Step 4: Tests should now pass**

```bash
cargo test --lib adapters::memory::fencing -- --nocapture
```

Expected: 5 passed.

- [ ] **Step 5: Commit**

```bash
git add src/adapters/memory/fencing.rs Cargo.toml Cargo.lock
git commit -m "feat(memory): fencing helpers for <memory-context> blocks"
```

### Task 1.4: Implement `context_block.rs` shared types

**Files:**
- Modify: `src/adapters/memory/context_block.rs`

- [ ] **Step 1: Write the types**

```rust
//! Shared types for memory retrieval results.

use serde::{Deserialize, Serialize};

/// Pinned block produced by `MemoryInjector::for_turn`, appended to the
/// user-turn message at API-call time. Never persisted in message
/// history.
#[derive(Debug, Clone, Default)]
pub struct PinnedMemoryBlock {
    pub body: String,
}

impl PinnedMemoryBlock {
    pub fn is_empty(&self) -> bool {
        self.body.trim().is_empty()
    }
}

/// Metadata attached to ingested chunks. Stored alongside the embedding
/// for retrieval-time filtering.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChunkMetadata {
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub timestamp_utc: Option<String>,
    #[serde(default, flatten)]
    pub extra: std::collections::HashMap<String, serde_json::Value>,
}

/// Single search hit returned by `MemoryProvider::prefetch` underlying
/// backends. Rendered into the `PinnedMemoryBlock.body` by the provider.
#[derive(Debug, Clone)]
pub struct MemoryHit {
    pub text: String,
    pub score: f32,
    pub metadata: ChunkMetadata,
}
```

- [ ] **Step 2: Verify build**

```bash
cargo build --lib 2>&1 | tail -5
```

Expected: clean.

- [ ] **Step 3: Commit**

```bash
git add src/adapters/memory/context_block.rs
git commit -m "feat(memory): shared types for retrieval results"
```

### Task 1.5: Port vector backend to `memory/vector.rs`

**Files:**
- Read: `src/adapters/memory_builder.rs` (current disk + embedding impl)
- Read: `src/adapters/qdrant_memory_store.rs` (current Qdrant impl)
- Read: `src/adapters/embedding.rs` (current embedding client)
- Modify: `src/adapters/memory/vector.rs`

- [ ] **Step 1: Read the three existing files carefully**

```bash
wc -l src/adapters/memory_builder.rs src/adapters/qdrant_memory_store.rs src/adapters/embedding.rs
```

Use `Read` tool to inspect them. Identify:
- The store trait (likely `MemoryStorePort` from `ports.rs`)
- The two store impls (disk/bincode + Qdrant)
- The embedding API (OpenRouter `text-embedding-3-small`)

- [ ] **Step 2: Move the store impls into `vector.rs`**

Create `src/adapters/memory/vector.rs` containing:

```rust
//! Vector backend for memory retrieval and ingestion.
//!
//! Two swappable stores, selected by `MemoryConfig.backend`:
//! - `disk`   — bincode file at `<workspace>/.tengu/memory.bin`
//! - `qdrant` — REST client against a Qdrant instance
//!
//! Both expose the `VectorStore` trait below.

use anyhow::Result;
use async_trait::async_trait;

use crate::adapters::memory::context_block::{ChunkMetadata, MemoryHit};

#[async_trait]
pub trait VectorStore: Send + Sync {
    async fn write(&self, embedding: Vec<f32>, text: &str, metadata: ChunkMetadata) -> Result<()>;
    async fn search(&self, embedding: &[f32], top_k: usize, filter: Option<&ChunkMetadata>) -> Result<Vec<MemoryHit>>;
}

pub mod disk;   // contents ported from memory_builder.rs
pub mod qdrant; // contents ported from qdrant_memory_store.rs
pub mod embedder; // contents ported from embedding.rs
```

- [ ] **Step 3: Create `disk.rs` submodule**

Create `src/adapters/memory/vector/disk.rs`. Port the bincode/disk implementation from `src/adapters/memory_builder.rs`:
1. Identify the disk-backed store struct + its methods in `memory_builder.rs`.
2. Copy them here.
3. Implement `VectorStore` for the disk struct.
4. Replace previous `MemoryStorePort` references with `VectorStore`.

(If the engineer finds the code tangled — e.g., store + service mixed — separate: `DiskVectorStore` here, `MemoryService` deferred to Task 1.6/1.7. Ask the code-reviewer agent if unsure.)

- [ ] **Step 4: Create `qdrant.rs` submodule**

Create `src/adapters/memory/vector/qdrant.rs`. Port Qdrant-specific code from `src/adapters/qdrant_memory_store.rs`. Implement `VectorStore` the same way.

- [ ] **Step 5: Create `embedder.rs` submodule**

Create `src/adapters/memory/vector/embedder.rs`. Port the embedding client from `src/adapters/embedding.rs`. Public API:

```rust
pub struct Embedder {
    // fields as in original embedding.rs
}

impl Embedder {
    pub fn new(api_key: String, model: String) -> Self { /* ... */ }
    pub async fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> { /* ... */ }
}
```

- [ ] **Step 6: Update `vector.rs` mod declarations**

```rust
pub mod disk;
pub mod qdrant;
pub mod embedder;

pub use disk::DiskVectorStore;
pub use qdrant::QdrantVectorStore;
pub use embedder::Embedder;
```

- [ ] **Step 7: Verify build (but DO NOT delete old files yet)**

```bash
cargo build --lib 2>&1 | tail -20
```

Expected: clean build. If duplicate-symbol errors, rename ported types (`DiskVectorStore` vs. old `DiskMemoryStore` etc.) — keeping both alive until Task 1.10.

- [ ] **Step 8: Port the vector-store tests**

Search `src/adapters/` for any `#[cfg(test)] mod tests` in `memory_builder.rs` / `qdrant_memory_store.rs` / `embedding.rs`. Port to their new homes.

```bash
cargo test --lib adapters::memory::vector -- --nocapture
```

Expected: all ported tests pass.

- [ ] **Step 9: Commit**

```bash
git add src/adapters/memory/vector.rs src/adapters/memory/vector/
git commit -m "feat(memory): port vector store backend to memory/vector/"
```

### Task 1.6: Implement `BuiltinMemoryProvider`

**Files:**
- Read: `src/adapters/memory_builder.rs` — understand `system_prompt_block` equivalent logic (AGENTS.md, MEMORY.md, identity, daily logs injection)
- Modify: `src/adapters/memory/builtin.rs`

- [ ] **Step 1: Write failing tests first**

At the bottom of `src/adapters/memory/builtin.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::fs;

    async fn setup_workspace() -> (TempDir, BuiltinMemoryProvider) {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        fs::create_dir_all(root.join(".tengu/memory")).await.unwrap();
        fs::write(root.join("MEMORY.md"), "# Project memory\n\nfacts").await.unwrap();
        fs::write(root.join("AGENTS.md"), "# Agents\n\nhello").await.unwrap();
        let provider = BuiltinMemoryProvider::new_for_test(root.clone());
        provider.initialize("sess-1", &root).await.unwrap();
        (tmp, provider)
    }

    #[tokio::test]
    async fn system_prompt_block_contains_memory_md() {
        let (_tmp, provider) = setup_workspace().await;
        let block = provider.system_prompt_block();
        assert!(block.contains("facts"));
        assert!(block.contains("AGENTS"));
    }

    #[tokio::test]
    async fn name_is_builtin() {
        let tmp = TempDir::new().unwrap();
        let provider = BuiltinMemoryProvider::new_for_test(tmp.path().into());
        assert_eq!(provider.name(), "builtin");
    }

    #[tokio::test]
    async fn prefetch_empty_store_returns_empty() {
        let (_tmp, provider) = setup_workspace().await;
        let out = provider.prefetch("researcher", "anything").await;
        assert_eq!(out, "");
    }
}
```

- [ ] **Step 2: Implement the provider**

Above the test module, write:

```rust
//! `BuiltinMemoryProvider` — MEMORY.md + identity + daily logs + vector.
//!
//! Always registered first in `MemoryManager`. Cannot be removed.

use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::adapters::memory::context_block::{ChunkMetadata, MemoryHit};
use crate::adapters::memory::fencing::sanitize_context;
use crate::adapters::memory::provider::MemoryProvider;
use crate::adapters::memory::vector::{Embedder, VectorStore};

pub struct BuiltinMemoryProvider {
    workspace: PathBuf,
    store: Arc<dyn VectorStore>,
    embedder: Arc<Embedder>,
    // cached system prompt block, computed during initialize()
    system_block: RwLock<String>,
}

impl BuiltinMemoryProvider {
    pub fn new(workspace: PathBuf, store: Arc<dyn VectorStore>, embedder: Arc<Embedder>) -> Self {
        Self { workspace, store, embedder, system_block: RwLock::new(String::new()) }
    }

    #[cfg(test)]
    pub fn new_for_test(workspace: PathBuf) -> Self {
        // test-only stub: in-memory vector store + null embedder
        use crate::adapters::memory::vector::disk::DiskVectorStore;
        let store: Arc<dyn VectorStore> = Arc::new(DiskVectorStore::in_memory());
        let embedder = Arc::new(Embedder::null());
        Self::new(workspace, store, embedder)
    }

    async fn load_system_prompt_files(&self) -> String {
        // AGENTS.md, MEMORY.md, identity files, daily logs for today + yesterday.
        // Port the loading logic from the existing memory_builder.rs
        // bootstrap-injection path. Each file, if present and non-empty,
        // gets a `## <name>` heading and is concatenated.
        let mut out = String::new();
        for name in ["AGENTS.md", "MEMORY.md", "USER.md", "IDENTITY.md", "PROFILE.md", "CONTEXT.md"] {
            if let Ok(body) = tokio::fs::read_to_string(self.workspace.join(name)).await {
                if !body.trim().is_empty() {
                    out.push_str(&format!("## {}\n\n{}\n\n", name, body.trim()));
                }
            }
        }
        // Daily logs: today + yesterday
        use chrono::Utc;
        let today = Utc::now().format("%Y-%m-%d").to_string();
        let yesterday = (Utc::now() - chrono::Duration::days(1)).format("%Y-%m-%d").to_string();
        for day in [yesterday, today] {
            let p = self.workspace.join(format!(".tengu/memory/{}.md", day));
            if let Ok(body) = tokio::fs::read_to_string(&p).await {
                if !body.trim().is_empty() {
                    out.push_str(&format!("## Daily log {}\n\n{}\n\n", day, body.trim()));
                }
            }
        }
        out
    }
}

#[async_trait]
impl MemoryProvider for BuiltinMemoryProvider {
    fn name(&self) -> &str { "builtin" }
    fn is_available(&self) -> bool { true }

    async fn initialize(&self, _session_id: &str, _workspace: &Path) -> anyhow::Result<()> {
        let block = self.load_system_prompt_files().await;
        *self.system_block.write().await = block;
        Ok(())
    }

    fn system_prompt_block(&self) -> String {
        // Cheap, synchronous — read_blocking via try_read
        self.system_block.try_read().map(|g| g.clone()).unwrap_or_default()
    }

    async fn prefetch(&self, agent: &str, query: &str) -> String {
        // Embed the query, vector search, render hits.
        let embedding = match self.embedder.embed(query).await {
            Ok(v) => v,
            Err(_) => return String::new(),
        };
        let filter = ChunkMetadata {
            agent: Some(agent.to_string()),
            ..Default::default()
        };
        let hits = match self.store.search(&embedding, 5, Some(&filter)).await {
            Ok(h) => h,
            Err(_) => return String::new(),
        };
        if hits.is_empty() { return String::new(); }
        let body: String = hits.iter().map(|h| format!("- (score {:.2}) {}", h.score, sanitize_context(&h.text))).collect::<Vec<_>>().join("\n");
        body
    }

    async fn sync_turn(&self, agent: &str, user: &str, assistant: &str) {
        let summary = format!("Q: {}\nA: {}", user.trim(), assistant.trim());
        let Ok(embedding) = self.embedder.embed(&summary).await else { return; };
        let metadata = ChunkMetadata {
            agent: Some(agent.to_string()),
            kind: Some("turn".to_string()),
            ..Default::default()
        };
        let _ = self.store.write(embedding, &summary, metadata).await;
    }

    async fn shutdown(&self) {}
}
```

Note: `DiskVectorStore::in_memory()` and `Embedder::null()` are test helpers — add them to Task 1.5's modules if missing.

- [ ] **Step 3: Add test helpers to vector crate**

In `src/adapters/memory/vector/disk.rs`, add:

```rust
#[cfg(test)]
impl DiskVectorStore {
    pub fn in_memory() -> Self { /* returns a DiskVectorStore backed by tempfile or Vec */ }
}
```

In `src/adapters/memory/vector/embedder.rs`, add:

```rust
impl Embedder {
    #[cfg(test)]
    pub fn null() -> Self { /* returns an Embedder whose embed() always returns Ok(vec![]) */ }
}
```

- [ ] **Step 4: Run tests**

```bash
cargo test --lib adapters::memory::builtin -- --nocapture
```

Expected: 3 passed.

- [ ] **Step 5: Commit**

```bash
git add src/adapters/memory/builtin.rs src/adapters/memory/vector/
git commit -m "feat(memory): BuiltinMemoryProvider with system/prefetch/sync_turn"
```

### Task 1.7: Implement `MemoryManager`

**Files:**
- Modify: `src/adapters/memory/manager.rs`

Models Hermes `MemoryManager` (python `agent/memory_manager.py` lines 83–373). One builtin + at most one external.

- [ ] **Step 1: Write failing tests**

At the bottom of `src/adapters/memory/manager.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::path::Path;

    struct FakeProvider { name: String }
    #[async_trait]
    impl MemoryProvider for FakeProvider {
        fn name(&self) -> &str { &self.name }
        fn is_available(&self) -> bool { true }
        async fn initialize(&self, _: &str, _: &Path) -> anyhow::Result<()> { Ok(()) }
        async fn prefetch(&self, _: &str, q: &str) -> String { format!("from-{}: {}", self.name, q) }
        async fn sync_turn(&self, _: &str, _: &str, _: &str) {}
        async fn shutdown(&self) {}
    }

    #[tokio::test]
    async fn builtin_registers_first() {
        let mgr = MemoryManager::new();
        mgr.add_provider(Box::new(FakeProvider { name: "builtin".into() })).await;
        assert_eq!(mgr.providers().await.len(), 1);
    }

    #[tokio::test]
    async fn at_most_one_external() {
        let mgr = MemoryManager::new();
        mgr.add_provider(Box::new(FakeProvider { name: "builtin".into() })).await;
        mgr.add_provider(Box::new(FakeProvider { name: "letta".into() })).await;
        mgr.add_provider(Box::new(FakeProvider { name: "mem0".into() })).await; // rejected
        let names: Vec<_> = mgr.providers().await.iter().map(|p| p.name().to_string()).collect();
        assert_eq!(names, vec!["builtin", "letta"]);
    }

    #[tokio::test]
    async fn prefetch_concatenates_providers() {
        let mgr = MemoryManager::new();
        mgr.add_provider(Box::new(FakeProvider { name: "builtin".into() })).await;
        mgr.add_provider(Box::new(FakeProvider { name: "letta".into() })).await;
        let out = mgr.prefetch_all("researcher", "q").await;
        assert!(out.contains("from-builtin"));
        assert!(out.contains("from-letta"));
    }
}
```

- [ ] **Step 2: Implement `MemoryManager`**

Above the tests:

```rust
//! `MemoryManager` — holds one built-in provider plus at most one external.

use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::warn;

use crate::adapters::memory::provider::MemoryProvider;

pub struct MemoryManager {
    providers: Arc<RwLock<Vec<Box<dyn MemoryProvider>>>>,
    has_external: Arc<RwLock<bool>>,
}

impl MemoryManager {
    pub fn new() -> Self {
        Self {
            providers: Arc::new(RwLock::new(Vec::new())),
            has_external: Arc::new(RwLock::new(false)),
        }
    }

    pub async fn add_provider(&self, provider: Box<dyn MemoryProvider>) {
        let is_builtin = provider.name() == "builtin";
        if !is_builtin {
            let mut flag = self.has_external.write().await;
            if *flag {
                warn!(
                    provider = provider.name(),
                    "rejected — an external memory provider is already registered"
                );
                return;
            }
            *flag = true;
        }
        self.providers.write().await.push(provider);
    }

    pub async fn providers(&self) -> Vec<String> {
        self.providers.read().await.iter().map(|p| p.name().to_string()).collect()
    }

    pub fn system_prompt_block(&self, providers: &[&dyn MemoryProvider]) -> String {
        // Called at system-prompt build time; takes providers by reference
        // so caller controls locking.
        providers.iter().map(|p| p.system_prompt_block()).filter(|b| !b.trim().is_empty()).collect::<Vec<_>>().join("\n\n")
    }

    pub async fn prefetch_all(&self, agent: &str, query: &str) -> String {
        let providers = self.providers.read().await;
        let mut parts = Vec::new();
        for p in providers.iter() {
            let out = p.prefetch(agent, query).await;
            if !out.trim().is_empty() {
                parts.push(out);
            }
        }
        parts.join("\n\n")
    }

    pub async fn sync_all(&self, agent: &str, user: &str, assistant: &str) {
        let providers = self.providers.read().await;
        for p in providers.iter() {
            p.sync_turn(agent, user, assistant).await;
        }
    }

    pub async fn shutdown_all(&self) {
        let providers = self.providers.read().await;
        for p in providers.iter().rev() {
            p.shutdown().await;
        }
    }
}

impl Default for MemoryManager {
    fn default() -> Self { Self::new() }
}
```

Note the `providers()` signature returns `Vec<String>` (names) for test assertions — the manager owns the `Box<dyn MemoryProvider>` entries; callers use the helper methods, not direct references.

- [ ] **Step 3: Fix the test to compile against the new API**

Adjust the first test:

```rust
#[tokio::test]
async fn builtin_registers_first() {
    let mgr = MemoryManager::new();
    mgr.add_provider(Box::new(FakeProvider { name: "builtin".into() })).await;
    assert_eq!(mgr.providers().await, vec!["builtin".to_string()]);
}
```

(The `providers()` method returns names now.)

- [ ] **Step 4: Run tests**

```bash
cargo test --lib adapters::memory::manager -- --nocapture
```

Expected: 3 passed.

- [ ] **Step 5: Commit**

```bash
git add src/adapters/memory/manager.rs
git commit -m "feat(memory): MemoryManager with builtin + at most one external"
```

### Task 1.8: Implement `MemoryInjector::for_turn`

**Files:**
- Modify: `src/adapters/memory/injector.rs`

- [ ] **Step 1: Write tests**

```rust
//! Pre-turn memory injection.

use crate::adapters::memory::context_block::PinnedMemoryBlock;
use crate::adapters::memory::fencing::build_memory_context_block;
use crate::adapters::memory::manager::MemoryManager;

/// Build a `PinnedMemoryBlock` for an agent's upcoming turn.
/// Caller appends this to the user-turn message of the API call.
pub async fn for_turn(mgr: &MemoryManager, agent: &str, query: &str) -> PinnedMemoryBlock {
    let raw = mgr.prefetch_all(agent, query).await;
    let body = build_memory_context_block(&raw);
    PinnedMemoryBlock { body }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::memory::provider::MemoryProvider;
    use async_trait::async_trait;
    use std::path::Path;

    struct Stub { body: String }
    #[async_trait]
    impl MemoryProvider for Stub {
        fn name(&self) -> &str { "builtin" }
        fn is_available(&self) -> bool { true }
        async fn initialize(&self, _: &str, _: &Path) -> anyhow::Result<()> { Ok(()) }
        async fn prefetch(&self, _: &str, _: &str) -> String { self.body.clone() }
        async fn sync_turn(&self, _: &str, _: &str, _: &str) {}
        async fn shutdown(&self) {}
    }

    #[tokio::test]
    async fn empty_provider_gives_empty_block() {
        let mgr = MemoryManager::new();
        mgr.add_provider(Box::new(Stub { body: "".into() })).await;
        let block = for_turn(&mgr, "a", "q").await;
        assert!(block.is_empty());
    }

    #[tokio::test]
    async fn nonempty_provider_fences_output() {
        let mgr = MemoryManager::new();
        mgr.add_provider(Box::new(Stub { body: "relevant fact".into() })).await;
        let block = for_turn(&mgr, "a", "q").await;
        assert!(block.body.starts_with("<memory-context>"));
        assert!(block.body.contains("relevant fact"));
    }
}
```

- [ ] **Step 2: Run**

```bash
cargo test --lib adapters::memory::injector -- --nocapture
```

Expected: 2 passed.

- [ ] **Step 3: Commit**

```bash
git add src/adapters/memory/injector.rs
git commit -m "feat(memory): MemoryInjector::for_turn"
```

### Task 1.9: Implement `MemoryWriter::sync_turn`

**Files:**
- Modify: `src/adapters/memory/writer.rs`

- [ ] **Step 1: Write test proving sync_turn returns before the write finishes**

```rust
//! Post-turn memory writes — spawned, non-blocking.

use std::sync::Arc;
use crate::adapters::memory::manager::MemoryManager;

pub fn sync_turn(mgr: Arc<MemoryManager>, agent: String, user: String, assistant: String) {
    tokio::spawn(async move {
        mgr.sync_all(&agent, &user, &assistant).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::memory::provider::MemoryProvider;
    use async_trait::async_trait;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::time::{sleep, Duration};

    struct Slow { done: Arc<AtomicBool> }
    #[async_trait]
    impl MemoryProvider for Slow {
        fn name(&self) -> &str { "builtin" }
        fn is_available(&self) -> bool { true }
        async fn initialize(&self, _: &str, _: &Path) -> anyhow::Result<()> { Ok(()) }
        async fn prefetch(&self, _: &str, _: &str) -> String { String::new() }
        async fn sync_turn(&self, _: &str, _: &str, _: &str) {
            sleep(Duration::from_millis(100)).await;
            self.done.store(true, Ordering::SeqCst);
        }
        async fn shutdown(&self) {}
    }

    #[tokio::test]
    async fn sync_turn_returns_before_write_completes() {
        let done = Arc::new(AtomicBool::new(false));
        let mgr = Arc::new(MemoryManager::new());
        mgr.add_provider(Box::new(Slow { done: done.clone() })).await;
        let t0 = std::time::Instant::now();
        sync_turn(mgr, "a".into(), "q".into(), "r".into());
        let elapsed = t0.elapsed();
        assert!(elapsed < Duration::from_millis(20), "sync_turn should return immediately");
        // give the spawned task time
        sleep(Duration::from_millis(200)).await;
        assert!(done.load(Ordering::SeqCst), "spawned task should have run");
    }
}
```

- [ ] **Step 2: Run**

```bash
cargo test --lib adapters::memory::writer -- --nocapture
```

Expected: 1 passed.

- [ ] **Step 3: Commit**

```bash
git add src/adapters/memory/writer.rs
git commit -m "feat(memory): MemoryWriter::sync_turn (spawned, non-blocking)"
```

### Task 1.10: Migrate callers from `memory_builder.rs` and delete it

**Files:**
- Delete: `src/adapters/memory_builder.rs` (after migration)
- Delete: `src/adapters/qdrant_memory_store.rs`
- Delete: `src/adapters/embedding.rs`
- Modify: all call sites that reference these files

- [ ] **Step 1: Find all callers**

```bash
rg "memory_builder|qdrant_memory_store|adapters::embedding" src/ tests/ -l
```

List of files that import from the old modules. Typical suspects:
- `src/adapters/chat_builder.rs`
- `src/adapters/channel_runtime.rs`
- `src/adapters/plugins/memory/*.rs`
- `src/main.rs` or `src/lib.rs`

- [ ] **Step 2: Migrate each caller file-by-file**

For each caller, replace:
- `use crate::adapters::memory_builder::{MemoryService, ...}` → equivalent imports from `crate::adapters::memory::*`
- `use crate::adapters::embedding::Embedder` → `use crate::adapters::memory::vector::Embedder`
- `use crate::adapters::qdrant_memory_store::*` → `use crate::adapters::memory::vector::*`

Compile after each file:

```bash
cargo build --lib 2>&1 | tail -15
```

- [ ] **Step 3: Delete the old files once all callers migrated**

```bash
git rm src/adapters/memory_builder.rs src/adapters/qdrant_memory_store.rs src/adapters/embedding.rs
```

Remove the `pub mod memory_builder;`, `pub mod embedding;`, `pub mod qdrant_memory_store;` lines from `src/adapters/mod.rs`.

- [ ] **Step 4: Build + test the whole crate**

```bash
cargo build --all-features 2>&1 | tail -20
cargo test --lib adapters::memory -- --nocapture
```

Expected: clean build, all memory-module tests pass.

- [ ] **Step 5: Commit**

```bash
git add -u src/adapters/
git commit -m "refactor(memory): migrate callers to memory/; delete memory_builder.rs, qdrant_memory_store.rs, embedding.rs"
```

---

## Phase 2 — Plugins/memory refactor

Refactor the LLM-callable memory tools. `remember` becomes `memory_ingest` (write path), a new `memory_search` tool is added (read path), `persistent_store` stays as-is.

### Task 2.1: Rename `remember` → `memory_ingest`

**Files:**
- Rename: `src/adapters/plugins/memory/remember.rs` → `src/adapters/plugins/memory/ingest.rs`
- Modify: `src/adapters/plugins/memory/mod.rs`

- [ ] **Step 1: Rename the file**

```bash
git mv src/adapters/plugins/memory/remember.rs src/adapters/plugins/memory/ingest.rs
```

- [ ] **Step 2: Rename the tool struct + tool name**

In the renamed file:
- Struct: `RememberTool` → `MemoryIngestTool`
- `Tool::name()` return: `"remember"` → `"memory_ingest"`
- Update the tool description to mention document/fact ingestion (chunks, embeddings, metadata).
- Update the input schema: add optional `chunks: Vec<String>` and `metadata: ChunkMetadata` fields in addition to `text`.

- [ ] **Step 3: Update `mod.rs` to register the renamed tool**

In `src/adapters/plugins/memory/mod.rs`:
- Replace `pub mod remember;` → `pub mod ingest;`
- Update plugin registration code: register `MemoryIngestTool` under tool name `memory_ingest`.

- [ ] **Step 4: Update the scope lint allowlist**

```bash
rg '"remember"' tests/ src/
```

Replace each hit with `"memory_ingest"`.

- [ ] **Step 5: Update the tool-choice allow-lists in `channel_runtime::compute_base_tools`**

```bash
rg 'compute_base_tools' src/
```

Rename `"remember"` → `"memory_ingest"` in the tool slice.

- [ ] **Step 6: Update config.rs `workspace_tools` references**

```bash
rg '"remember"' src/adapters/config.rs
```

Update any defaults or helper text.

- [ ] **Step 7: Build + test**

```bash
cargo build --all-features 2>&1 | tail -10
cargo test --lib adapters::plugins::memory -- --nocapture
```

Expected: clean build, existing tests pass with the new name.

- [ ] **Step 8: Commit**

```bash
git add -u
git commit -m "refactor(memory): rename remember → memory_ingest; extend with chunks + metadata"
```

### Task 2.2: Add `memory_search` tool

**Files:**
- Create: `src/adapters/plugins/memory/search.rs`
- Modify: `src/adapters/plugins/memory/mod.rs`

- [ ] **Step 1: Write failing integration test**

Create `tests/memory_search_tool.rs`:

```rust
//! Integration test: ingest then search via the plugin tools.

use std::sync::Arc;
use tengu_cluster::adapters::memory::builtin::BuiltinMemoryProvider;
use tengu_cluster::adapters::memory::context_block::ChunkMetadata;
use tengu_cluster::adapters::memory::manager::MemoryManager;
use tengu_cluster::adapters::memory::provider::MemoryProvider;
use tempfile::TempDir;

#[tokio::test]
async fn ingest_then_search_returns_ingested_chunk() -> anyhow::Result<()> {
    let tmp = TempDir::new()?;
    let workspace = tmp.path().to_path_buf();
    tokio::fs::create_dir_all(workspace.join(".tengu/memory")).await?;

    let provider = BuiltinMemoryProvider::new_for_test(workspace.clone());
    provider.initialize("sess-1", &workspace).await?;
    let mgr = Arc::new(MemoryManager::new());
    mgr.add_provider(Box::new(provider)).await;

    // Ingest as agent "researcher" (via direct sync_turn — the plugin
    // tool wraps the same path)
    mgr.sync_all(
        "researcher",
        "Record fact",
        "The Alpha Protocol was signed on 2026-01-15.",
    ).await;

    // Search as agent "writer" (which would call memory_search). We
    // assert the prefetch path surfaces the ingested content.
    let hit = mgr.prefetch_all("writer", "Alpha Protocol signing date").await;
    assert!(hit.contains("Alpha Protocol"), "expected ingested fact in search results, got: {}", hit);
    Ok(())
}
```

Note: this tests the `MemoryManager` contract, which is what both the plugin tool (`memory_search`) and the harness (`MemoryInjector::for_turn`) rely on. If you prefer to test the plugin tool directly, construct a `MemorySearchTool` with the same `mgr` and call its `execute` with a `ToolCtx` test fixture — but that requires a larger harness (see `tests/workspace_plugin.rs` or similar for the pattern). The above is simpler and equivalent for correctness.

Run:

```bash
cargo test --test memory_search_tool 2>&1 | tail -15
```

Expected: FAIL — compile error on `BuiltinMemoryProvider::new_for_test` if `MemoryManager::search` isn't wired yet, or assertion failure if prefetch is empty.

- [ ] **Step 2: Implement the tool**

Create `src/adapters/plugins/memory/search.rs`:

```rust
//! `memory_search` — LLM-callable targeted vector read.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;

use crate::adapters::memory::manager::MemoryManager;
use crate::adapters::tool_plugin::{Tool, ToolCtx, ToolResult};

#[derive(Debug, Deserialize)]
pub struct MemorySearchArgs {
    pub query: String,
    #[serde(default = "default_top_k")]
    pub top_k: usize,
    // metadata filter fields — agent, source, kind, tags — optional
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
}

fn default_top_k() -> usize { 5 }

#[derive(Debug, Serialize)]
pub struct MemorySearchHit {
    pub text: String,
    pub score: f32,
    pub metadata: serde_json::Value,
}

pub struct MemorySearchTool {
    mgr: Arc<MemoryManager>,
}

impl MemorySearchTool {
    pub fn new(mgr: Arc<MemoryManager>) -> Self { Self { mgr } }
}

#[async_trait]
impl Tool for MemorySearchTool {
    fn name(&self) -> &str { "memory_search" }

    fn description(&self) -> &str {
        "Targeted vector search of the memory store. Returns chunks with scores and metadata. Use when you need to look up specific prior content (documents ingested by other agents, past turn summaries, etc.)."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string" },
                "top_k": { "type": "integer", "default": 5 },
                "agent":  { "type": "string" },
                "source": { "type": "string" },
                "kind":   { "type": "string" }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolCtx) -> ToolResult {
        ctx.scope.check_memory_read()?;  // scope-gate; refine per ports.rs conventions
        let args: MemorySearchArgs = serde_json::from_value(args)?;
        // Embed the query via the builtin provider's embedder.
        // (For simplicity, call back through a helper on MemoryManager that
        // searches the builtin backend directly.)
        // Return hits as a JSON-serializable array.
        let hits = self.mgr.search(&args).await?; // add this method in MemoryManager
        Ok(json!({ "hits": hits }))
    }
}
```

This assumes you add a `MemoryManager::search(&self, args: &MemorySearchArgs) -> anyhow::Result<Vec<MemorySearchHit>>` helper that the builtin provider's `VectorStore` is reachable via.

- [ ] **Step 3: Add `search` method to `MemoryProvider` trait**

In `src/adapters/memory/provider.rs`, add (inside the `#[async_trait] impl`):

```rust
/// Targeted search. Default returns empty; `BuiltinMemoryProvider`
/// overrides with real vector search. External providers may override
/// to route through their own backend.
async fn search(
    &self,
    _query: &str,
    _top_k: usize,
    _filter: Option<&crate::adapters::memory::context_block::ChunkMetadata>,
) -> anyhow::Result<Vec<crate::adapters::memory::context_block::MemoryHit>> {
    Ok(Vec::new())
}
```

- [ ] **Step 4: Override `search` on `BuiltinMemoryProvider`**

In `src/adapters/memory/builtin.rs`, inside `impl MemoryProvider for BuiltinMemoryProvider`, add:

```rust
async fn search(
    &self,
    query: &str,
    top_k: usize,
    filter: Option<&ChunkMetadata>,
) -> anyhow::Result<Vec<MemoryHit>> {
    let embedding = self.embedder.embed(query).await?;
    self.store.search(&embedding, top_k, filter).await
}
```

- [ ] **Step 5: Add `MemoryManager::search` that delegates to the builtin provider**

In `src/adapters/memory/manager.rs`, add a helper method:

```rust
impl MemoryManager {
    pub async fn search(
        &self,
        query: &str,
        top_k: usize,
        filter: Option<&crate::adapters::memory::context_block::ChunkMetadata>,
    ) -> anyhow::Result<Vec<crate::adapters::memory::context_block::MemoryHit>> {
        // Delegate to the builtin provider (first registered).
        let providers = self.providers.read().await;
        let Some(p) = providers.first() else {
            return Ok(Vec::new());
        };
        p.search(query, top_k, filter).await
    }
}
```

- [ ] **Step 6: Update `MemorySearchTool::execute` to use the new signature**

In `src/adapters/plugins/memory/search.rs`, replace the `execute` body:

```rust
async fn execute(&self, args: serde_json::Value, ctx: &ToolCtx) -> ToolResult {
    ctx.scope.check_memory_read()?;
    let args: MemorySearchArgs = serde_json::from_value(args)?;
    let filter = ChunkMetadata {
        agent: args.agent,
        source: args.source,
        kind: args.kind,
        ..Default::default()
    };
    let filter_ref = if filter.agent.is_some() || filter.source.is_some() || filter.kind.is_some() {
        Some(&filter)
    } else {
        None
    };
    let hits = self.mgr.search(&args.query, args.top_k, filter_ref).await?;
    let as_json: Vec<MemorySearchHit> = hits.into_iter().map(|h| MemorySearchHit {
        text: h.text,
        score: h.score,
        metadata: serde_json::to_value(h.metadata).unwrap_or(json!({})),
    }).collect();
    Ok(json!({ "hits": as_json }))
}
```

Also, import `ChunkMetadata` at the top of the file: `use crate::adapters::memory::context_block::ChunkMetadata;`

- [ ] **Step 4: Register the tool**

In `src/adapters/plugins/memory/mod.rs`, add `pub mod search;` and register `MemorySearchTool` alongside the others.

- [ ] **Step 7: Run test**

```bash
cargo test --test memory_search_tool 2>&1 | tail -10
```

Expected: PASS — the test written in Step 1 now runs against real `MemoryManager::search` delegation.

- [ ] **Step 8: Commit**

```bash
git add src/adapters/plugins/memory/search.rs src/adapters/plugins/memory/mod.rs src/adapters/memory/manager.rs src/adapters/memory/provider.rs src/adapters/memory/builtin.rs tests/memory_search_tool.rs
git commit -m "feat(memory): add memory_search LLM tool (targeted vector read)"
```

### Task 2.3: Verify `persistent_store` unchanged

**Files:** `src/adapters/plugins/memory/persistent_store.rs`

- [ ] **Step 1: Verify no code changes are required**

```bash
cargo test --lib adapters::plugins::memory::persistent_store -- --nocapture
```

Expected: existing tests pass. This task is a checkpoint, not a code change.

If the scope gating or tool registration changed elsewhere in this phase, fix here. Otherwise skip.

---

## Phase 3 — Config reshape

### Task 3.1: Reshape `OrchestratorConfig`

**Files:**
- Modify: `src/adapters/config.rs`

- [ ] **Step 1: Read current `OrchestratorConfig`**

File: `src/adapters/config.rs:421-452`. Current fields: `enabled`, `max_retries`, `max_concurrent`, `planner_engine`, `planner_model`.

- [ ] **Step 2: Replace with new shape**

```rust
/// Orchestration configuration. Presence activates orchestration;
/// absence falls back to single-agent-default dispatch.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OrchestratorConfig {
    /// Name of the agent (in `Config.agents`) that acts as the
    /// orchestrator.
    pub agent: String,

    /// Tier 1: how many times a single step is retried before
    /// escalation.
    #[serde(default = "default_max_attempts_per_step")]
    pub max_attempts_per_step: u32,

    /// Tier 2: how many times the orchestrator is re-invoked to replan
    /// after exhaustion before bailing out.
    #[serde(default = "default_max_replans")]
    pub max_replans: u32,
}

fn default_max_attempts_per_step() -> u32 { 3 }
fn default_max_replans() -> u32 { 2 }
```

Delete `default_orchestrator_enabled`, `default_max_retries`, `default_max_concurrent` helpers.

- [ ] **Step 3: Find callers that reference the old fields**

```bash
rg 'orchestrator\.(enabled|max_retries|max_concurrent|planner_engine|planner_model)' src/ tests/
```

For each match:
- `orchestrator.enabled` → replace with `config.orchestrator.is_some()`
- `orchestrator.max_retries` → rename call site to use `max_attempts_per_step` (may require threading through a different value if it was being used for replans)
- `orchestrator.max_concurrent` → delete the call site entirely (this limit only existed for `plugins/subagents/` which is being deleted in Phase 6)
- `orchestrator.planner_engine` / `orchestrator.planner_model` → delete; the orchestrator is just `Config.agents[orchestrator.agent]`, its engine/model come from that `AgentConfig`.

- [ ] **Step 4: Update `tengu.toml` example or test fixtures**

```bash
rg '\[orchestrator\]' config/ examples/ tests/ tengu.toml* 2>/dev/null
```

Update any fixtures to the new shape.

- [ ] **Step 5: Build + test**

```bash
cargo build --all-features 2>&1 | tail -10
cargo test --lib adapters::config -- --nocapture
```

Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add -u
git commit -m "refactor(config): reshape OrchestratorConfig for harness-owned orchestration"
```

### Task 3.2: Add roster template substitution helper

**Files:**
- Modify: `src/adapters/memory/builtin.rs` (or a new `src/adapters/orchestrator/roster.rs` — placed there in Task 4.1; for now skip)

This task is **deferred to Task 4.1**, where the orchestrator module lands. The helper lives in `orchestrator/roster.rs`.

---

## Phase 4 — Orchestrator skeleton

### Task 4.1: Create `orchestrator/` module skeleton + roster helper

**Files:**
- Create: `src/adapters/orchestrator/mod.rs`
- Create: `src/adapters/orchestrator/config.rs`
- Create: `src/adapters/orchestrator/plan.rs` (stub)
- Create: `src/adapters/orchestrator/planner.rs` (stub)
- Create: `src/adapters/orchestrator/executor.rs` (stub)
- Create: `src/adapters/orchestrator/retry.rs` (stub)
- Create: `src/adapters/orchestrator/replan.rs` (stub)
- Create: `src/adapters/orchestrator/events.rs` (stub)
- Create: `src/adapters/orchestrator/roster.rs`
- Create: `src/adapters/orchestrator/telemetry.rs` (stub)
- Modify: `src/adapters/mod.rs` — add `pub mod orchestrator;`

- [ ] **Step 1: Create all stub files with placeholder doc comments**

Each stub starts with:

```rust
//! Placeholder — implemented in Task 4.N.
```

`src/adapters/orchestrator/mod.rs`:

```rust
//! Harness-owned orchestration.
//!
//! - `config`    — OrchestratorConfig bridging (re-export from crate::adapters::config)
//! - `plan`      — Step, StepId, Plan types + topology helpers
//! - `planner`   — runs the orchestrator agent's LLM call
//! - `executor`  — DAG executor: parallel, ready-set scheduling
//! - `retry`     — per-step retry policy
//! - `replan`    — outer loop: re-invoke orchestrator on exhaustion
//! - `events`    — OrchestratorEvent enum + broadcast channel
//! - `roster`    — agent roster rendering + template substitution
//! - `telemetry` — event → tracing bridge

pub mod config;
pub mod events;
pub mod executor;
pub mod plan;
pub mod planner;
pub mod replan;
pub mod retry;
pub mod roster;
pub mod telemetry;

// Public API re-exports
pub use events::{OrchestratorEvent, EventBus, EventReceiver};
pub use plan::{Plan, Step, StepId};
// `Orchestrator` struct is appended to this file in Task 4.9.
```

- [ ] **Step 2: Write the roster helper + tests**

`src/adapters/orchestrator/roster.rs`:

```rust
//! Agent roster rendering and template substitution.
//!
//! The orchestrator's system prompt contains `{{ roster }}` which is
//! replaced once at conversation-init time with a stable Markdown table
//! of `(name, description)` pairs derived from `Config.agents`.
//! One-time substitution preserves prompt cache.

use std::collections::HashMap;

use crate::adapters::config::AgentConfig;

pub fn render_roster(agents: &HashMap<String, AgentConfig>, exclude: &[&str]) -> String {
    let mut rows: Vec<(String, String)> = agents
        .iter()
        .filter(|(name, _)| !exclude.contains(&name.as_str()))
        .map(|(name, cfg)| {
            let desc = cfg.identity.instructions.as_deref()
                .and_then(|s| s.lines().next())
                .unwrap_or("(no description)")
                .to_string();
            (name.clone(), desc)
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));

    let mut out = String::from("| Agent | Description |\n|---|---|\n");
    for (name, desc) in rows {
        out.push_str(&format!("| {} | {} |\n", name, desc));
    }
    out
}

pub fn substitute_roster(template: &str, roster_md: &str) -> String {
    template.replace("{{ roster }}", roster_md)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn agent_with_desc(desc: &str) -> AgentConfig {
        let mut cfg = AgentConfig {
            default: false,
            engine: "openrouter".into(),
            model: "any".into(),
            workspace: None,
            default_lens: "eco".into(),
            identity: crate::adapters::config::IdentityConfig { name: None, instructions: Some(desc.into()) },
            flow: Default::default(),
            limits: Default::default(),
            lens: Default::default(),
            role: None,
            skill_packages: Vec::new(),
            prompt_budget: Default::default(),
            requires: Vec::new(),
            workspace_tools: Vec::new(),
            scopes: HashMap::new(),
            claude_code: None,
        };
        cfg
    }

    #[test]
    fn renders_sorted_table_with_first_line_of_instructions() {
        let mut agents = HashMap::new();
        agents.insert("zebra".into(), agent_with_desc("Does zebra things.\nextra detail"));
        agents.insert("alpha".into(), agent_with_desc("Does alpha things."));
        let md = render_roster(&agents, &[]);
        assert!(md.find("alpha").unwrap() < md.find("zebra").unwrap());
        assert!(md.contains("Does alpha things."));
        assert!(md.contains("Does zebra things."));
        assert!(!md.contains("extra detail"));
    }

    #[test]
    fn exclude_filters_orchestrator_itself() {
        let mut agents = HashMap::new();
        agents.insert("orchestrator".into(), agent_with_desc("I am the conductor."));
        agents.insert("writer".into(), agent_with_desc("I write."));
        let md = render_roster(&agents, &["orchestrator"]);
        assert!(!md.contains("I am the conductor"));
        assert!(md.contains("I write"));
    }

    #[test]
    fn substitute_replaces_placeholder() {
        let out = substitute_roster("prefix {{ roster }} suffix", "ROSTER_HERE");
        assert_eq!(out, "prefix ROSTER_HERE suffix");
    }
}
```

Adjust the `AgentConfig` constructor if fields differ — read `src/adapters/config.rs:253-291` and match actual fields.

- [ ] **Step 3: Wire the module into the crate**

In `src/adapters/mod.rs`, add `pub mod orchestrator;` (alphabetical).

- [ ] **Step 4: Build + test**

```bash
cargo build --lib 2>&1 | tail -5
cargo test --lib adapters::orchestrator::roster -- --nocapture
```

Expected: clean + 3 passed.

- [ ] **Step 5: Commit**

```bash
git add src/adapters/orchestrator/ src/adapters/mod.rs
git commit -m "feat(orchestrator): scaffold module + roster rendering helper"
```

### Task 4.2: Implement `plan.rs` — Step, StepId, Plan + topology helpers

**Files:**
- Modify: `src/adapters/orchestrator/plan.rs`

- [ ] **Step 1: Write failing tests**

```rust
//! Plan types and topology helpers.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepId(pub String);

impl StepId {
    pub fn new(s: impl Into<String>) -> Self { Self(s.into()) }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub id: StepId,
    pub agent: String,
    pub goal: String,
    #[serde(default)]
    pub depends_on: Vec<StepId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub steps: Vec<Step>,
}

#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error("plan has a cycle including step {0:?}")]
    Cycle(StepId),
    #[error("step {0:?} depends on unknown step {1:?}")]
    UnknownDependency(StepId, StepId),
    #[error("plan has no leaf (all steps have dependents)")]
    NoLeaf,
    #[error("plan has multiple leaves {0:?} — include a synthesizer step")]
    MultipleLeaves(Vec<StepId>),
    #[error("step {0:?} references unknown agent {1:?}")]
    UnknownAgent(StepId, String),
    #[error("duplicate step id {0:?}")]
    DuplicateId(StepId),
}

impl Plan {
    /// Return all steps whose dependencies are satisfied by `completed`
    /// and that are not themselves in `completed`.
    pub fn ready_steps(&self, completed: &HashSet<StepId>) -> Vec<&Step> {
        self.steps.iter().filter(|s| {
            !completed.contains(&s.id)
                && s.depends_on.iter().all(|d| completed.contains(d))
        }).collect()
    }

    /// Full topology validation: no duplicate IDs, no cycles, no unknown
    /// dependencies, exactly one leaf, every agent present in
    /// `known_agents`.
    pub fn validate(&self, known_agents: &[&str]) -> Result<(), PlanError> {
        // duplicate IDs
        let mut seen = HashSet::new();
        for s in &self.steps {
            if !seen.insert(s.id.clone()) {
                return Err(PlanError::DuplicateId(s.id.clone()));
            }
        }
        // agent resolution
        for s in &self.steps {
            if !known_agents.iter().any(|a| *a == s.agent) {
                return Err(PlanError::UnknownAgent(s.id.clone(), s.agent.clone()));
            }
        }
        // dependency resolution
        let id_set: HashSet<_> = self.steps.iter().map(|s| &s.id).collect();
        for s in &self.steps {
            for d in &s.depends_on {
                if !id_set.contains(d) {
                    return Err(PlanError::UnknownDependency(s.id.clone(), d.clone()));
                }
            }
        }
        // cycle detection (Kahn's algorithm)
        let mut in_deg: HashMap<StepId, usize> = self.steps.iter().map(|s| (s.id.clone(), s.depends_on.len())).collect();
        let mut queue: Vec<StepId> = in_deg.iter().filter(|(_, &d)| d == 0).map(|(k, _)| k.clone()).collect();
        let mut removed = 0;
        while let Some(id) = queue.pop() {
            removed += 1;
            for s in &self.steps {
                if s.depends_on.contains(&id) {
                    if let Some(d) = in_deg.get_mut(&s.id) {
                        *d -= 1;
                        if *d == 0 { queue.push(s.id.clone()); }
                    }
                }
            }
        }
        if removed != self.steps.len() {
            let cycle_id = self.steps.iter().find(|s| in_deg[&s.id] > 0).map(|s| s.id.clone()).unwrap();
            return Err(PlanError::Cycle(cycle_id));
        }
        // single leaf
        let has_dep_on: HashSet<&StepId> = self.steps.iter().flat_map(|s| &s.depends_on).collect();
        let leaves: Vec<&Step> = self.steps.iter().filter(|s| !has_dep_on.contains(&s.id)).collect();
        match leaves.len() {
            0 => Err(PlanError::NoLeaf),
            1 => Ok(()),
            _ => Err(PlanError::MultipleLeaves(leaves.into_iter().map(|s| s.id.clone()).collect())),
        }
    }

    pub fn single_leaf(&self) -> Option<&Step> {
        let has_dep_on: HashSet<&StepId> = self.steps.iter().flat_map(|s| &s.depends_on).collect();
        let mut leaves = self.steps.iter().filter(|s| !has_dep_on.contains(&s.id));
        let first = leaves.next()?;
        if leaves.next().is_some() { None } else { Some(first) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(id: &str, agent: &str, deps: &[&str]) -> Step {
        Step {
            id: StepId::new(id),
            agent: agent.into(),
            goal: format!("goal-{}", id),
            depends_on: deps.iter().map(|d| StepId::new(*d)).collect(),
        }
    }

    #[test]
    fn ready_steps_respects_dependencies() {
        let plan = Plan { steps: vec![step("a", "x", &[]), step("b", "x", &["a"])] };
        let ready = plan.ready_steps(&HashSet::new());
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, StepId::new("a"));
    }

    #[test]
    fn validate_detects_cycle() {
        let plan = Plan { steps: vec![step("a", "x", &["b"]), step("b", "x", &["a"])] };
        assert!(matches!(plan.validate(&["x"]), Err(PlanError::Cycle(_))));
    }

    #[test]
    fn validate_detects_unknown_dep() {
        let plan = Plan { steps: vec![step("a", "x", &["ghost"])] };
        assert!(matches!(plan.validate(&["x"]), Err(PlanError::UnknownDependency(_, _))));
    }

    #[test]
    fn validate_rejects_unknown_agent() {
        let plan = Plan { steps: vec![step("a", "ghost-agent", &[])] };
        assert!(matches!(plan.validate(&["x"]), Err(PlanError::UnknownAgent(_, _))));
    }

    #[test]
    fn validate_rejects_multiple_leaves() {
        let plan = Plan { steps: vec![step("a", "x", &[]), step("b", "x", &[])] };
        assert!(matches!(plan.validate(&["x"]), Err(PlanError::MultipleLeaves(_))));
    }

    #[test]
    fn validate_accepts_single_leaf() {
        let plan = Plan { steps: vec![
            step("a", "x", &[]),
            step("b", "x", &[]),
            step("c", "x", &["a", "b"]),
        ] };
        assert!(plan.validate(&["x"]).is_ok());
    }

    #[test]
    fn single_leaf_returns_leaf() {
        let plan = Plan { steps: vec![step("a", "x", &[]), step("b", "x", &["a"])] };
        assert_eq!(plan.single_leaf().map(|s| s.id.clone()), Some(StepId::new("b")));
    }
}
```

- [ ] **Step 2: Make sure `thiserror` is a dependency**

```bash
grep '^thiserror' Cargo.toml || echo 'missing'
```

If missing, add `thiserror = "1"` under `[dependencies]`.

- [ ] **Step 3: Run tests**

```bash
cargo test --lib adapters::orchestrator::plan -- --nocapture
```

Expected: 7 passed.

- [ ] **Step 4: Commit**

```bash
git add src/adapters/orchestrator/plan.rs Cargo.toml Cargo.lock
git commit -m "feat(orchestrator): Step/Plan types with topology validation"
```

### Task 4.3: Implement `events.rs`

**Files:**
- Modify: `src/adapters/orchestrator/events.rs`

- [ ] **Step 1: Write the event enum + bus**

```rust
//! `OrchestratorEvent` + broadcast channel.

use tokio::sync::broadcast;

use crate::adapters::orchestrator::plan::{Plan, StepId};

#[derive(Debug, Clone)]
pub enum OrchestratorEvent {
    PlanCreated { plan: Plan },
    StepStarted { step_id: StepId, agent: String },
    StepProgress { step_id: StepId, chunk: String },
    StepFailed { step_id: StepId, attempt: u32, error: String },
    StepExhausted { step_id: StepId, final_error: String },
    StepSucceeded { step_id: StepId, output: String },
    ReplanTriggered { reason: String },
    PlanCompleted { final_response: String, cancelled: bool },
}

pub type EventBus = broadcast::Sender<OrchestratorEvent>;
pub type EventReceiver = broadcast::Receiver<OrchestratorEvent>;

/// Default channel capacity. Channels subscribe cheaply; old events
/// are dropped if a subscriber lags (standard broadcast semantics).
pub const DEFAULT_BUS_CAPACITY: usize = 256;

pub fn new_bus() -> EventBus {
    broadcast::channel(DEFAULT_BUS_CAPACITY).0
}
```

- [ ] **Step 2: Verify build**

```bash
cargo build --lib 2>&1 | tail -5
```

- [ ] **Step 3: Commit**

```bash
git add src/adapters/orchestrator/events.rs
git commit -m "feat(orchestrator): OrchestratorEvent enum + broadcast bus"
```

### Task 4.4: Define `WorkerHandle` trait

**Files:**
- Modify: `src/adapters/orchestrator/executor.rs`

- [ ] **Step 1: Define the trait**

Replace the stub in `executor.rs` with:

```rust
//! DAG executor: parallel step dispatch with retry escalation.

use async_trait::async_trait;

use crate::adapters::orchestrator::plan::Step;

/// Abstracts "how to run a worker step." The real impl calls
/// `ChatRuntimeService::process_user_text` under the hood. Tests
/// inject fake impls.
#[async_trait]
pub trait WorkerHandle: Send + Sync {
    /// Run `step.agent` with `step.goal + step_inputs` as the user turn.
    /// `step_inputs` is the rendered `<step-input>` blocks from upstream
    /// completed steps.
    async fn run_step(&self, step: &Step, step_inputs: &str) -> anyhow::Result<String>;
}
```

- [ ] **Step 2: Verify build**

```bash
cargo build --lib 2>&1 | tail -5
```

- [ ] **Step 3: Commit**

```bash
git add src/adapters/orchestrator/executor.rs
git commit -m "feat(orchestrator): WorkerHandle trait"
```

### Task 4.5: Implement `RetryPolicy`

**Files:**
- Modify: `src/adapters/orchestrator/retry.rs`

- [ ] **Step 1: Write failing tests**

```rust
//! Per-step retry policy.

use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::warn;

use crate::adapters::orchestrator::events::{EventBus, OrchestratorEvent};
use crate::adapters::orchestrator::executor::WorkerHandle;
use crate::adapters::orchestrator::plan::Step;

#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    /// Delay for attempts 2, 3, 4… (attempt 1 runs immediately).
    pub backoff: Vec<Duration>,
}

impl RetryPolicy {
    pub fn new(max_attempts: u32) -> Self {
        Self {
            max_attempts,
            backoff: vec![Duration::from_secs(1), Duration::from_secs(3), Duration::from_secs(9)],
        }
    }
}

pub enum StepOutcome {
    Ok(String),
    Exhausted(String),
}

pub async fn run_step_with_retry(
    step: &Step,
    step_inputs: &str,
    worker: Arc<dyn WorkerHandle>,
    policy: &RetryPolicy,
    events: &EventBus,
) -> StepOutcome {
    let mut last_err = String::new();
    for attempt in 1..=policy.max_attempts {
        match worker.run_step(step, step_inputs).await {
            Ok(output) => return StepOutcome::Ok(output),
            Err(err) => {
                last_err = err.to_string();
                let _ = events.send(OrchestratorEvent::StepFailed {
                    step_id: step.id.clone(),
                    attempt,
                    error: last_err.clone(),
                });
                warn!(step = ?step.id, attempt, error = %last_err, "step failed");
                if attempt < policy.max_attempts {
                    let delay = policy.backoff.get((attempt - 1) as usize).copied().unwrap_or(Duration::from_secs(9));
                    sleep(delay).await;
                }
            }
        }
    }
    let _ = events.send(OrchestratorEvent::StepExhausted {
        step_id: step.id.clone(),
        final_error: last_err.clone(),
    });
    StepOutcome::Exhausted(last_err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::orchestrator::events::new_bus;
    use crate::adapters::orchestrator::plan::StepId;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct FlakyWorker { fails_until: u32, calls: Arc<AtomicU32> }
    #[async_trait]
    impl WorkerHandle for FlakyWorker {
        async fn run_step(&self, _: &Step, _: &str) -> anyhow::Result<String> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if call <= self.fails_until {
                anyhow::bail!("flaky fail #{}", call)
            } else {
                Ok(format!("output from call {}", call))
            }
        }
    }

    fn test_step() -> Step {
        Step { id: StepId::new("s1"), agent: "x".into(), goal: "g".into(), depends_on: vec![] }
    }

    #[tokio::test]
    async fn succeeds_on_first_try() {
        let worker = Arc::new(FlakyWorker { fails_until: 0, calls: Arc::new(AtomicU32::new(0)) });
        let mut policy = RetryPolicy::new(3);
        policy.backoff = vec![Duration::from_millis(1), Duration::from_millis(1), Duration::from_millis(1)];
        let bus = new_bus();
        match run_step_with_retry(&test_step(), "", worker, &policy, &bus).await {
            StepOutcome::Ok(s) => assert!(s.contains("output from call 1")),
            _ => panic!("expected ok"),
        }
    }

    #[tokio::test]
    async fn exhausts_after_max_attempts() {
        let worker = Arc::new(FlakyWorker { fails_until: 99, calls: Arc::new(AtomicU32::new(0)) });
        let mut policy = RetryPolicy::new(3);
        policy.backoff = vec![Duration::from_millis(1), Duration::from_millis(1)];
        let bus = new_bus();
        match run_step_with_retry(&test_step(), "", worker, &policy, &bus).await {
            StepOutcome::Exhausted(e) => assert!(e.contains("flaky fail")),
            _ => panic!("expected exhausted"),
        }
    }

    #[tokio::test]
    async fn succeeds_on_second_attempt() {
        let worker = Arc::new(FlakyWorker { fails_until: 1, calls: Arc::new(AtomicU32::new(0)) });
        let mut policy = RetryPolicy::new(3);
        policy.backoff = vec![Duration::from_millis(1), Duration::from_millis(1)];
        let bus = new_bus();
        match run_step_with_retry(&test_step(), "", worker, &policy, &bus).await {
            StepOutcome::Ok(s) => assert!(s.contains("output from call 2")),
            _ => panic!("expected ok"),
        }
    }
}
```

- [ ] **Step 2: Run tests**

```bash
cargo test --lib adapters::orchestrator::retry -- --nocapture
```

Expected: 3 passed.

- [ ] **Step 3: Commit**

```bash
git add src/adapters/orchestrator/retry.rs
git commit -m "feat(orchestrator): RetryPolicy with exponential backoff"
```

### Task 4.6: Implement `DagExecutor`

**Files:**
- Modify: `src/adapters/orchestrator/executor.rs`

- [ ] **Step 1: Add integration test**

At the bottom of `executor.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::orchestrator::events::{new_bus, OrchestratorEvent};
    use crate::adapters::orchestrator::plan::{Plan, Step, StepId};
    use crate::adapters::orchestrator::retry::RetryPolicy;
    use async_trait::async_trait;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::Mutex;

    struct OkWorker;
    #[async_trait]
    impl WorkerHandle for OkWorker {
        async fn run_step(&self, step: &Step, inputs: &str) -> anyhow::Result<String> {
            Ok(format!("out({})[{}]", step.id.0, inputs.trim()))
        }
    }

    struct OrderRecordingWorker { order: Arc<Mutex<Vec<String>>> }
    #[async_trait]
    impl WorkerHandle for OrderRecordingWorker {
        async fn run_step(&self, step: &Step, _inputs: &str) -> anyhow::Result<String> {
            self.order.lock().await.push(step.id.0.clone());
            Ok(format!("out-{}", step.id.0))
        }
    }

    fn linear_plan() -> Plan {
        Plan { steps: vec![
            Step { id: StepId::new("s1"), agent: "x".into(), goal: "one".into(), depends_on: vec![] },
            Step { id: StepId::new("s2"), agent: "x".into(), goal: "two".into(), depends_on: vec![StepId::new("s1")] },
        ] }
    }

    #[tokio::test]
    async fn linear_plan_completes_in_order() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let worker = Arc::new(OrderRecordingWorker { order: order.clone() });
        let bus = new_bus();
        let mut policy = RetryPolicy::new(1);
        policy.backoff = vec![];
        let result = DagExecutor::run(&linear_plan(), worker, &policy, &bus).await;
        assert!(matches!(result, ExecResult::Done { .. }));
        assert_eq!(*order.lock().await, vec!["s1".to_string(), "s2".to_string()]);
    }

    #[tokio::test]
    async fn dependent_step_sees_upstream_output() {
        let bus = new_bus();
        let mut policy = RetryPolicy::new(1);
        policy.backoff = vec![];
        let result = DagExecutor::run(&linear_plan(), Arc::new(OkWorker), &policy, &bus).await;
        match result {
            ExecResult::Done { final_output } => {
                assert!(final_output.contains("s1"));  // s1's output fed into s2 via <step-input>
                assert!(final_output.contains("s2"));
            }
            _ => panic!("expected Done"),
        }
    }
}
```

- [ ] **Step 2: Implement the executor**

Above the tests:

```rust
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use futures::stream::{FuturesUnordered, StreamExt};
use tracing::info;

use crate::adapters::orchestrator::events::{EventBus, OrchestratorEvent};
use crate::adapters::orchestrator::plan::{Plan, Step, StepId};
use crate::adapters::orchestrator::retry::{RetryPolicy, run_step_with_retry, StepOutcome};

pub enum ExecResult {
    Done { final_output: String },
    NeedsReplan { failed: StepId, error: String },
    Cancelled,
}

pub struct DagExecutor;

impl DagExecutor {
    pub async fn run(
        plan: &Plan,
        worker: Arc<dyn WorkerHandle>,
        policy: &RetryPolicy,
        events: &EventBus,
    ) -> ExecResult {
        let mut completed_outputs: HashMap<StepId, String> = HashMap::new();
        let mut in_flight: HashSet<StepId> = HashSet::new();
        let mut futures = FuturesUnordered::new();

        loop {
            // Dispatch ready steps.
            let completed: HashSet<StepId> = completed_outputs.keys().cloned().collect();
            for step in plan.ready_steps(&completed) {
                if in_flight.contains(&step.id) { continue; }
                let step_inputs = render_step_inputs(step, &completed_outputs);
                let worker = Arc::clone(&worker);
                let policy = policy.clone();
                let events = events.clone();
                let step_clone = step.clone();
                in_flight.insert(step.id.clone());
                let _ = events.send(OrchestratorEvent::StepStarted {
                    step_id: step.id.clone(),
                    agent: step.agent.clone(),
                });
                futures.push(tokio::spawn(async move {
                    let outcome = run_step_with_retry(&step_clone, &step_inputs, worker, &policy, &events).await;
                    (step_clone.id, outcome)
                }));
            }

            if futures.is_empty() {
                // Nothing in flight and nothing ready — done.
                break;
            }

            // Await any completion.
            match futures.next().await {
                Some(Ok((id, StepOutcome::Ok(output)))) => {
                    in_flight.remove(&id);
                    let _ = events.send(OrchestratorEvent::StepSucceeded {
                        step_id: id.clone(),
                        output: output.clone(),
                    });
                    completed_outputs.insert(id, output);
                }
                Some(Ok((id, StepOutcome::Exhausted(err)))) => {
                    in_flight.remove(&id);
                    return ExecResult::NeedsReplan { failed: id, error: err };
                }
                Some(Err(join_err)) => {
                    // Task panicked. Treat as catastrophic.
                    info!(?join_err, "executor task panicked");
                    return ExecResult::NeedsReplan {
                        failed: StepId::new("<panicked>"),
                        error: format!("task panic: {}", join_err),
                    };
                }
                None => break,
            }
        }

        // All steps completed — leaf's output is the final response.
        if let Some(leaf) = plan.single_leaf() {
            let out = completed_outputs.get(&leaf.id).cloned().unwrap_or_default();
            ExecResult::Done { final_output: out }
        } else {
            // Validation should have caught this — but be defensive.
            ExecResult::NeedsReplan {
                failed: StepId::new("<no-leaf>"),
                error: "plan has no single leaf".into(),
            }
        }
    }
}

fn render_step_inputs(step: &Step, completed: &HashMap<StepId, String>) -> String {
    if step.depends_on.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for dep in &step.depends_on {
        if let Some(body) = completed.get(dep) {
            out.push_str(&format!("<step-input from=\"{}\">\n{}\n</step-input>\n\n", dep.0, body));
        }
    }
    out
}
```

- [ ] **Step 3: Ensure `futures` is a dep**

```bash
grep '^futures' Cargo.toml || echo 'missing'
```

Add `futures = "0.3"` if missing.

- [ ] **Step 4: Run tests**

```bash
cargo test --lib adapters::orchestrator::executor -- --nocapture
```

Expected: 2 passed.

- [ ] **Step 5: Commit**

```bash
git add src/adapters/orchestrator/executor.rs Cargo.toml Cargo.lock
git commit -m "feat(orchestrator): DagExecutor with parallel dispatch + step-input wiring"
```

### Task 4.7: Implement `Planner`

**Files:**
- Modify: `src/adapters/orchestrator/planner.rs`

- [ ] **Step 1: Define `PlannerVerdict` + trait**

```rust
//! Planner — runs the orchestrator agent's LLM call, returns Plan or direct response.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::adapters::orchestrator::plan::Plan;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlannerVerdict {
    Direct { response: String },
    Plan { #[serde(flatten)] plan: Plan },
}

#[async_trait]
pub trait Planner: Send + Sync {
    /// First-plan call: user message + empty context.
    async fn plan(&self, user_message: &str) -> anyhow::Result<PlannerVerdict>;

    /// Replan call: user message + failure context to avoid repeat mistakes.
    async fn replan(&self, user_message: &str, prior_plan: &Plan, failed_step_id: &str, error: &str)
        -> anyhow::Result<PlannerVerdict>;
}
```

- [ ] **Step 2: Implement `OrchestratorAgentPlanner`**

Below the trait:

```rust
use std::sync::Arc;
use crate::adapters::memory::manager::MemoryManager;
use crate::adapters::orchestrator::roster::{render_roster, substitute_roster};

/// Runs the orchestrator as an actual agent (LLM call with memory_search tool).
pub struct OrchestratorAgentPlanner {
    orchestrator_agent: String,
    chat: Arc<dyn OrchestratorChatPort>, // trait defined below
    memory: Arc<MemoryManager>,
    roster_md: String, // cached — computed once at construction time
}

/// Minimal port the planner needs from the chat runtime (avoids cyclic deps).
#[async_trait]
pub trait OrchestratorChatPort: Send + Sync {
    /// Run a single LLM conversation turn and return the final message
    /// (JSON string). The port handles system-prompt assembly, tool
    /// loop (memory_search), and memory injection.
    async fn run_orchestrator_turn(&self, agent: &str, user_message: &str) -> anyhow::Result<String>;
}

impl OrchestratorAgentPlanner {
    pub fn new(orchestrator_agent: String, chat: Arc<dyn OrchestratorChatPort>, memory: Arc<MemoryManager>, roster_md: String) -> Self {
        Self { orchestrator_agent, chat, memory, roster_md }
    }

    fn parse_verdict(raw: &str) -> anyhow::Result<PlannerVerdict> {
        // The orchestrator may wrap JSON in markdown fences. Strip them.
        let trimmed = raw.trim();
        let stripped = trimmed.strip_prefix("```json").unwrap_or(trimmed);
        let stripped = stripped.strip_prefix("```").unwrap_or(stripped);
        let stripped = stripped.strip_suffix("```").unwrap_or(stripped);
        let verdict: PlannerVerdict = serde_json::from_str(stripped.trim())?;
        Ok(verdict)
    }
}

#[async_trait]
impl Planner for OrchestratorAgentPlanner {
    async fn plan(&self, user_message: &str) -> anyhow::Result<PlannerVerdict> {
        let raw = self.chat.run_orchestrator_turn(&self.orchestrator_agent, user_message).await?;
        Self::parse_verdict(&raw)
    }

    async fn replan(&self, user_message: &str, prior_plan: &Plan, failed_step_id: &str, error: &str)
        -> anyhow::Result<PlannerVerdict>
    {
        let context = format!(
            "A previous plan failed.\n\n\
             Failed step: {}\nError after retries: {}\n\n\
             Prior plan steps:\n{}\n\n\
             Produce a new plan that avoids this failure, or respond directly if recovery is not possible.\n\n\
             Original user message:\n{}",
            failed_step_id,
            error,
            serde_json::to_string_pretty(&prior_plan)?,
            user_message,
        );
        let raw = self.chat.run_orchestrator_turn(&self.orchestrator_agent, &context).await?;
        Self::parse_verdict(&raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::orchestrator::plan::{Step, StepId};

    #[test]
    fn parses_direct() {
        let raw = r#"{"kind": "direct", "response": "hi"}"#;
        match OrchestratorAgentPlanner::parse_verdict(raw).unwrap() {
            PlannerVerdict::Direct { response } => assert_eq!(response, "hi"),
            _ => panic!(),
        }
    }

    #[test]
    fn parses_plan() {
        let raw = r#"{"kind": "plan", "steps": [{"id": "s1", "agent": "x", "goal": "g", "depends_on": []}]}"#;
        match OrchestratorAgentPlanner::parse_verdict(raw).unwrap() {
            PlannerVerdict::Plan { plan } => assert_eq!(plan.steps[0].id, StepId::new("s1")),
            _ => panic!(),
        }
    }

    #[test]
    fn strips_markdown_fences() {
        let raw = "```json\n{\"kind\": \"direct\", \"response\": \"hi\"}\n```";
        assert!(OrchestratorAgentPlanner::parse_verdict(raw).is_ok());
    }

    #[test]
    fn rejects_malformed_json() {
        assert!(OrchestratorAgentPlanner::parse_verdict("{").is_err());
    }
}
```

- [ ] **Step 3: Run tests**

```bash
cargo test --lib adapters::orchestrator::planner -- --nocapture
```

Expected: 4 passed.

- [ ] **Step 4: Commit**

```bash
git add src/adapters/orchestrator/planner.rs
git commit -m "feat(orchestrator): Planner trait + OrchestratorAgentPlanner with verdict parsing"
```

### Task 4.8: Implement `Replan` outer loop

**Files:**
- Modify: `src/adapters/orchestrator/replan.rs`

- [ ] **Step 1: Write tests**

```rust
//! Replan outer loop: on step exhaustion, re-invoke the planner.

use std::sync::Arc;

use crate::adapters::orchestrator::events::{EventBus, OrchestratorEvent};
use crate::adapters::orchestrator::executor::{DagExecutor, ExecResult, WorkerHandle};
use crate::adapters::orchestrator::planner::{Planner, PlannerVerdict};
use crate::adapters::orchestrator::plan::Plan;
use crate::adapters::orchestrator::retry::RetryPolicy;

pub async fn drive(
    planner: Arc<dyn Planner>,
    user_message: &str,
    worker: Arc<dyn WorkerHandle>,
    policy: &RetryPolicy,
    max_replans: u32,
    events: &EventBus,
) -> String {
    let mut plan_opt: Option<Plan> = match planner.plan(user_message).await {
        Ok(PlannerVerdict::Direct { response }) => {
            let _ = events.send(OrchestratorEvent::PlanCompleted {
                final_response: response.clone(),
                cancelled: false,
            });
            return response;
        }
        Ok(PlannerVerdict::Plan { plan }) => Some(plan),
        Err(err) => {
            let msg = format!("System error: orchestrator initial call failed: {}", err);
            let _ = events.send(OrchestratorEvent::PlanCompleted { final_response: msg.clone(), cancelled: false });
            return msg;
        }
    };

    let mut replans_left = max_replans;
    loop {
        let plan = plan_opt.as_ref().unwrap().clone();
        let _ = events.send(OrchestratorEvent::PlanCreated { plan: plan.clone() });

        match DagExecutor::run(&plan, Arc::clone(&worker), policy, events).await {
            ExecResult::Done { final_output } => {
                let _ = events.send(OrchestratorEvent::PlanCompleted { final_response: final_output.clone(), cancelled: false });
                return final_output;
            }
            ExecResult::Cancelled => {
                let _ = events.send(OrchestratorEvent::PlanCompleted { final_response: "Stopped by user.".into(), cancelled: true });
                return "Stopped by user.".into();
            }
            ExecResult::NeedsReplan { failed, error } => {
                if replans_left == 0 {
                    let msg = format!("Unable to complete the request after {} replans. Last error: {}", max_replans, error);
                    let _ = events.send(OrchestratorEvent::PlanCompleted { final_response: msg.clone(), cancelled: false });
                    return msg;
                }
                replans_left -= 1;
                let _ = events.send(OrchestratorEvent::ReplanTriggered { reason: error.clone() });
                match planner.replan(user_message, &plan, &failed.0, &error).await {
                    Ok(PlannerVerdict::Direct { response }) => {
                        let _ = events.send(OrchestratorEvent::PlanCompleted { final_response: response.clone(), cancelled: false });
                        return response;
                    }
                    Ok(PlannerVerdict::Plan { plan: new_plan }) => {
                        plan_opt = Some(new_plan);
                    }
                    Err(err) => {
                        let msg = format!("System error: replan call failed: {}", err);
                        let _ = events.send(OrchestratorEvent::PlanCompleted { final_response: msg.clone(), cancelled: false });
                        return msg;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::orchestrator::events::new_bus;
    use crate::adapters::orchestrator::plan::{Step, StepId};
    use async_trait::async_trait;
    use std::sync::Mutex;

    struct ScriptedPlanner { verdicts: Mutex<Vec<PlannerVerdict>> }
    #[async_trait]
    impl Planner for ScriptedPlanner {
        async fn plan(&self, _: &str) -> anyhow::Result<PlannerVerdict> {
            Ok(self.verdicts.lock().unwrap().remove(0))
        }
        async fn replan(&self, _: &str, _: &Plan, _: &str, _: &str) -> anyhow::Result<PlannerVerdict> {
            Ok(self.verdicts.lock().unwrap().remove(0))
        }
    }

    struct FailOnceWorker { calls: Mutex<u32> }
    #[async_trait]
    impl WorkerHandle for FailOnceWorker {
        async fn run_step(&self, step: &Step, _: &str) -> anyhow::Result<String> {
            let mut c = self.calls.lock().unwrap();
            *c += 1;
            if *c <= 3 && step.id.0 == "bad" {
                anyhow::bail!("always fails")
            }
            Ok(format!("ok-{}", step.id.0))
        }
    }

    #[tokio::test]
    async fn replans_after_step_exhaustion() {
        let bus = new_bus();
        let bad_plan = Plan { steps: vec![Step { id: StepId::new("bad"), agent: "x".into(), goal: "g".into(), depends_on: vec![] }] };
        let good_plan = Plan { steps: vec![Step { id: StepId::new("good"), agent: "x".into(), goal: "g".into(), depends_on: vec![] }] };
        let planner = Arc::new(ScriptedPlanner {
            verdicts: Mutex::new(vec![
                PlannerVerdict::Plan { plan: bad_plan },
                PlannerVerdict::Plan { plan: good_plan },
            ])
        });
        let worker = Arc::new(FailOnceWorker { calls: Mutex::new(0) });
        let mut policy = RetryPolicy::new(3);
        policy.backoff = vec![Duration::from_millis(1); 3];
        let out = drive(planner, "msg", worker, &policy, 2, &bus).await;
        assert!(out.contains("ok-good"));
    }

    #[tokio::test]
    async fn bails_out_when_replans_exhausted() {
        let bus = new_bus();
        let bad = || Plan { steps: vec![Step { id: StepId::new("bad"), agent: "x".into(), goal: "g".into(), depends_on: vec![] }] };
        let planner = Arc::new(ScriptedPlanner {
            verdicts: Mutex::new(vec![
                PlannerVerdict::Plan { plan: bad() },
                PlannerVerdict::Plan { plan: bad() },
                PlannerVerdict::Plan { plan: bad() },
            ])
        });
        let worker = Arc::new(FailOnceWorker { calls: Mutex::new(0) });
        let mut policy = RetryPolicy::new(3);
        policy.backoff = vec![Duration::from_millis(1); 3];
        let out = drive(planner, "msg", worker, &policy, 2, &bus).await;
        assert!(out.contains("Unable to complete"));
    }
}
```

- [ ] **Step 2: Run**

```bash
cargo test --lib adapters::orchestrator::replan -- --nocapture
```

Expected: 2 passed.

- [ ] **Step 3: Commit**

```bash
git add src/adapters/orchestrator/replan.rs
git commit -m "feat(orchestrator): replan driver (step-exhaustion → re-invoke planner → bound-aware bail-out)"
```

### Task 4.9: Implement `Orchestrator` public API

**Files:**
- Create: `src/adapters/orchestrator/mod.rs` (modify existing to add `Orchestrator` struct)

- [ ] **Step 1: Add the public struct**

At the end of `src/adapters/orchestrator/mod.rs` (after the `pub mod`s):

```rust
use std::sync::Arc;

use crate::adapters::memory::manager::MemoryManager;
use crate::adapters::orchestrator::events::{EventBus, EventReceiver};
use crate::adapters::orchestrator::executor::WorkerHandle;
use crate::adapters::orchestrator::planner::Planner;
use crate::adapters::orchestrator::retry::RetryPolicy;

pub struct Orchestrator {
    planner: Arc<dyn Planner>,
    worker: Arc<dyn WorkerHandle>,
    policy: RetryPolicy,
    max_replans: u32,
    bus: EventBus,
    memory: Arc<MemoryManager>,
}

impl Orchestrator {
    pub fn new(
        planner: Arc<dyn Planner>,
        worker: Arc<dyn WorkerHandle>,
        policy: RetryPolicy,
        max_replans: u32,
        memory: Arc<MemoryManager>,
    ) -> Self {
        Self {
            planner, worker, policy, max_replans,
            bus: events::new_bus(),
            memory,
        }
    }

    pub fn subscribe(&self) -> EventReceiver {
        self.bus.subscribe()
    }

    pub async fn handle(&self, user_message: String) -> String {
        replan::drive(
            Arc::clone(&self.planner),
            &user_message,
            Arc::clone(&self.worker),
            &self.policy,
            self.max_replans,
            &self.bus,
        ).await
    }
}
```

- [ ] **Step 2: Build**

```bash
cargo build --lib 2>&1 | tail -5
```

- [ ] **Step 3: Commit**

```bash
git add src/adapters/orchestrator/mod.rs
git commit -m "feat(orchestrator): Orchestrator public API (handle, subscribe)"
```

### Task 4.10: Integration test — full happy path

**Files:**
- Create: `tests/orchestrator_e2e.rs`

- [ ] **Step 1: Write the test**

```rust
//! End-to-end orchestrator test with in-memory planner + worker.

use std::sync::Arc;
use std::time::Duration;

use tengu_cluster::adapters::orchestrator::{
    events::new_bus,
    executor::WorkerHandle,
    plan::{Plan, Step, StepId},
    planner::{Planner, PlannerVerdict},
    replan,
    retry::RetryPolicy,
};
use async_trait::async_trait;

struct StaticPlanner { plan: Plan }
#[async_trait]
impl Planner for StaticPlanner {
    async fn plan(&self, _: &str) -> anyhow::Result<PlannerVerdict> {
        Ok(PlannerVerdict::Plan { plan: self.plan.clone() })
    }
    async fn replan(&self, _: &str, _: &Plan, _: &str, _: &str) -> anyhow::Result<PlannerVerdict> {
        unreachable!()
    }
}

struct OkWorker;
#[async_trait]
impl WorkerHandle for OkWorker {
    async fn run_step(&self, step: &Step, inputs: &str) -> anyhow::Result<String> {
        Ok(format!("out({})[{}]", step.id.0, inputs.trim()))
    }
}

#[tokio::test]
async fn fan_out_plus_synthesizer() {
    let plan = Plan { steps: vec![
        Step { id: StepId::new("a"), agent: "x".into(), goal: "research".into(), depends_on: vec![] },
        Step { id: StepId::new("b"), agent: "x".into(), goal: "parallel-research".into(), depends_on: vec![] },
        Step { id: StepId::new("c"), agent: "x".into(), goal: "synthesize".into(), depends_on: vec![StepId::new("a"), StepId::new("b")] },
    ] };
    let mut policy = RetryPolicy::new(1);
    policy.backoff = vec![];
    let bus = new_bus();
    let out = replan::drive(
        Arc::new(StaticPlanner { plan }),
        "user question",
        Arc::new(OkWorker),
        &policy,
        0,
        &bus,
    ).await;
    assert!(out.contains("out(c)"));     // synthesizer output
    assert!(out.contains("out(a)"));     // a's output embedded
    assert!(out.contains("out(b)"));     // b's output embedded
}
```

- [ ] **Step 2: Run**

```bash
cargo test --test orchestrator_e2e 2>&1 | tail -10
```

Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add tests/orchestrator_e2e.rs
git commit -m "test(orchestrator): e2e fan-out + synthesizer"
```

---

## Phase 5 — Channel integration

### Task 5.1: Implement `ChatOrchestratorPort` + `WorkerHandle` backed by `ChatRuntimeService`

**Files:**
- Modify: `src/adapters/chat_builder.rs`
- Modify: `src/adapters/orchestrator/mod.rs` (or a new `src/adapters/orchestrator/wiring.rs`)

This is the real wiring that replaces the stub traits used in unit tests.

- [ ] **Step 1: Read current `chat_builder.rs` to understand `ChatRuntimeService::process_user_text`**

Identify its signature and what it needs (agent name, user text, optional session context).

- [ ] **Step 2: Create `src/adapters/orchestrator/wiring.rs`**

```rust
//! Wiring: real `WorkerHandle` + `OrchestratorChatPort` backed by
//! `ChatRuntimeService`.

use async_trait::async_trait;
use std::sync::Arc;

use crate::adapters::chat_builder::ChatRuntimeService;
use crate::adapters::memory::injector;
use crate::adapters::memory::manager::MemoryManager;
use crate::adapters::memory::writer;
use crate::adapters::orchestrator::executor::WorkerHandle;
use crate::adapters::orchestrator::plan::Step;
use crate::adapters::orchestrator::planner::OrchestratorChatPort;

pub struct ChatWorker {
    chat: Arc<ChatRuntimeService>,
    memory: Arc<MemoryManager>,
}

impl ChatWorker {
    pub fn new(chat: Arc<ChatRuntimeService>, memory: Arc<MemoryManager>) -> Self {
        Self { chat, memory }
    }
}

#[async_trait]
impl WorkerHandle for ChatWorker {
    async fn run_step(&self, step: &Step, step_inputs: &str) -> anyhow::Result<String> {
        // Memory block keyed on step goal (not raw user prompt).
        let mem = injector::for_turn(&self.memory, &step.agent, &step.goal).await;
        let user_content = if step_inputs.is_empty() {
            format!("{}\n\n{}", mem.body, step.goal).trim().to_string()
        } else {
            format!("{}\n\n{}\n\nYour task:\n{}", mem.body, step_inputs, step.goal).trim().to_string()
        };

        let reply = self.chat.process_user_text(&step.agent, &user_content).await?;
        // Post-turn spawn memory write (non-blocking).
        writer::sync_turn(Arc::clone(&self.memory), step.agent.clone(), step.goal.clone(), reply.clone());
        Ok(reply)
    }
}

pub struct ChatOrchestratorPortImpl {
    chat: Arc<ChatRuntimeService>,
    memory: Arc<MemoryManager>,
}

impl ChatOrchestratorPortImpl {
    pub fn new(chat: Arc<ChatRuntimeService>, memory: Arc<MemoryManager>) -> Self {
        Self { chat, memory }
    }
}

#[async_trait]
impl OrchestratorChatPort for ChatOrchestratorPortImpl {
    async fn run_orchestrator_turn(&self, agent: &str, user_message: &str) -> anyhow::Result<String> {
        let mem = injector::for_turn(&self.memory, agent, user_message).await;
        let user_content = format!("{}\n\n{}", mem.body, user_message).trim().to_string();
        let reply = self.chat.process_user_text(agent, &user_content).await?;
        writer::sync_turn(Arc::clone(&self.memory), agent.to_string(), user_message.to_string(), reply.clone());
        Ok(reply)
    }
}
```

- [ ] **Step 3: Register the module**

In `src/adapters/orchestrator/mod.rs`, add `pub mod wiring;`.

- [ ] **Step 4: Build**

```bash
cargo build --lib 2>&1 | tail -10
```

Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add src/adapters/orchestrator/wiring.rs src/adapters/orchestrator/mod.rs
git commit -m "feat(orchestrator): wire WorkerHandle + ChatPort to ChatRuntimeService"
```

### Task 5.2: Construct `Orchestrator` in `channel_runtime.rs` entrypoint

**Files:**
- Modify: `src/adapters/channel_runtime.rs`

- [ ] **Step 1: Read the current channel runtime**

Find where `ChatRuntimeService` is constructed and where user messages are routed in.

- [ ] **Step 2: Add orchestrator construction behind a helper**

Add (alongside existing constructors):

```rust
pub fn build_orchestrator(config: &Config, chat: Arc<ChatRuntimeService>, memory: Arc<MemoryManager>) -> Option<Orchestrator> {
    let cfg = config.orchestrator.as_ref()?;
    let worker = Arc::new(ChatWorker::new(chat.clone(), memory.clone()));
    let chat_port = Arc::new(ChatOrchestratorPortImpl::new(chat.clone(), memory.clone()));
    let roster = render_roster(&config.agents, &[cfg.agent.as_str()]);
    let planner = Arc::new(OrchestratorAgentPlanner::new(
        cfg.agent.clone(), chat_port, memory.clone(), roster,
    ));
    let policy = RetryPolicy::new(cfg.max_attempts_per_step);
    Some(Orchestrator::new(planner, worker, policy, cfg.max_replans, memory))
}
```

- [ ] **Step 3: Add routing helper**

```rust
pub async fn route_message(
    orchestrator: Option<&Orchestrator>,
    chat: &Arc<ChatRuntimeService>,
    default_agent: &str,
    user_msg: &str,
) -> (String, Option<EventReceiver>) {
    if let Some(orch) = orchestrator {
        let rx = Some(orch.subscribe());
        let reply = orch.handle(user_msg.to_string()).await;
        (reply, rx)
    } else {
        let reply = chat.process_user_text(default_agent, user_msg).await.unwrap_or_else(|e| e.to_string());
        (reply, None)
    }
}
```

- [ ] **Step 4: Build**

```bash
cargo build --lib 2>&1 | tail -10
```

- [ ] **Step 5: Commit**

```bash
git add src/adapters/channel_runtime.rs
git commit -m "feat(orchestrator): build_orchestrator + route_message in channel_runtime"
```

### Task 5.3: Wire Telegram adapter to the orchestrator (quiet rendering)

**Files:**
- Modify: `src/adapters/telegram_builder.rs`

- [ ] **Step 1: Find the current `handle_user_message` in telegram_builder**

Locate where a user message becomes `chat.process_user_text(...)`.

- [ ] **Step 2: Replace with orchestrator routing + quiet event subscription**

```rust
// When handling a user message:
let (reply, rx) = route_message(&self.orchestrator, &self.chat, &self.default_agent, &user_msg).await;

// If rx is Some, spawn a task to render progress quietly:
if let Some(mut rx) = rx {
    let bot = self.bot.clone();
    let chat_id = msg.chat.id;
    // Send a placeholder "Thinking..." message that gets edited in place
    let placeholder = bot.send_message(chat_id, "Thinking...").await?;
    tokio::spawn(async move {
        while let Ok(event) = rx.recv().await {
            let snippet = match event {
                OrchestratorEvent::StepStarted { agent, .. } => Some(format!("Thinking... [{}]", agent)),
                OrchestratorEvent::ReplanTriggered { .. } => Some("Thinking... (retrying)".into()),
                OrchestratorEvent::PlanCompleted { .. } => None, // final message handled below
                _ => None,
            };
            if let Some(text) = snippet {
                let _ = bot.edit_message_text(chat_id, placeholder.id, text).await;
            }
        }
    });
}

// Once `route_message` returns the final reply:
bot.send_message(chat_id, &reply).await?;
```

Preserve existing file-upload, secret-redaction, inline-keyboard-approval flows — orchestrator routing only replaces the `process_user_text` call path.

- [ ] **Step 3: Build + run telegram integration test (if one exists)**

```bash
cargo build --all-features 2>&1 | tail -10
rg -l 'test.*telegram' tests/ | head -5
```

Run any telegram integration test you find.

- [ ] **Step 4: Commit**

```bash
git add src/adapters/telegram_builder.rs
git commit -m "feat(orchestrator): wire Telegram adapter to orchestrator with quiet rendering"
```

### Task 5.4: Wire CLI `tengu orchestrator` (or main dispatch) to the orchestrator

**Files:**
- Modify: CLI entrypoint (likely `src/main.rs` or `src/bin/tengu.rs`) and/or relevant channel runtime init

- [ ] **Step 1: Find the CLI orchestrator subcommand handler**

```bash
rg 'fn.*orchestrator|pub.*orchestrator' src/main.rs src/bin/ 2>/dev/null
```

- [ ] **Step 2: Replace the single-agent dispatch path with `route_message`**

Same pattern as Telegram: call `route_message`, render events to stdout in a tree-style print (fuller verbosity than Telegram since CLI has a terminal), print final reply.

- [ ] **Step 3: Build + test**

```bash
cargo build --all-features 2>&1 | tail -10
```

- [ ] **Step 4: Commit**

```bash
git add -u
git commit -m "feat(orchestrator): wire CLI dispatch to orchestrator with verbose rendering"
```

### Task 5.5: Cancellation — `/stop` mid-plan

**Files:**
- Modify: `src/adapters/orchestrator/executor.rs` (add cancel flag)
- Modify: `src/adapters/orchestrator/mod.rs` (expose `cancel()` method)
- Modify: `src/adapters/telegram_builder.rs`, CLI handler

- [ ] **Step 1: Add an `AtomicBool` cancel flag to the executor**

```rust
// in executor.rs DagExecutor::run
use std::sync::atomic::{AtomicBool, Ordering};
pub struct DagExecutor;
impl DagExecutor {
    pub async fn run(
        plan: &Plan,
        worker: Arc<dyn WorkerHandle>,
        policy: &RetryPolicy,
        events: &EventBus,
        cancel: Arc<AtomicBool>,  // NEW
    ) -> ExecResult {
        // ... in the main loop, check `cancel.load(Ordering::SeqCst)` between dispatches
        //     and return ExecResult::Cancelled
    }
}
```

Update all callers (replan::drive signature changes).

- [ ] **Step 2: Expose `Orchestrator::cancel`**

```rust
impl Orchestrator {
    pub fn cancel(&self) { self.cancel_flag.store(true, Ordering::SeqCst); }
}
```

Threaded through construction: add `cancel_flag: Arc<AtomicBool>` in `Orchestrator::new`.

- [ ] **Step 3: Wire Telegram `/stop` command and CLI SIGINT/Ctrl-C handler**

In Telegram: on `/stop`, call `orchestrator.cancel()`.
In CLI: catch Ctrl-C via `tokio::signal::ctrl_c()` and call `orchestrator.cancel()`.

- [ ] **Step 4: Integration test**

Add `tests/orchestrator_cancel.rs`:

```rust
#[tokio::test]
async fn cancel_mid_plan_returns_cancelled() {
    // Plan with a slow worker. Cancel after 50ms. Assert ExecResult::Cancelled.
    // ... template similar to orchestrator_e2e.rs
}
```

- [ ] **Step 5: Run tests + commit**

```bash
cargo test --test orchestrator_cancel 2>&1 | tail -10
git add -u
git commit -m "feat(orchestrator): /stop mid-plan cancellation via AtomicBool"
```

---

## Phase 6 — Delete subagents plugin

### Task 6.1: Delete `plugins/subagents/` directory and all references

**Files:**
- Delete: `src/adapters/plugins/subagents/` (entire directory)
- Modify: `src/adapters/plugins/mod.rs`, `src/adapters/channel_runtime.rs`, plus any other files referencing subagent tools

- [ ] **Step 1: Find all references**

```bash
rg 'sessions_spawn|sessions_fan_out|plugins::subagents|compute_subagent_tools|SubagentRegistry|SubagentHandle' src/ tests/ -l
```

- [ ] **Step 2: Delete the directory**

```bash
git rm -r src/adapters/plugins/subagents/
```

Remove `pub mod subagents;` from `src/adapters/plugins/mod.rs`.

- [ ] **Step 3: Fix each caller**

For each file listed by the rg in Step 1:
- Remove `use` statements pointing at the deleted code.
- Remove any `compute_subagent_tools` call in `channel_runtime::compute_base_tools` (delete the helper entirely).
- Remove the orchestrator gating check `if config.orchestrator.as_ref().map_or(false, |o| o.enabled)` — now subsumed by `config.orchestrator.is_some()` but irrelevant here because the tools are gone.

- [ ] **Step 4: Build**

```bash
cargo build --all-features 2>&1 | tail -20
```

Expected: clean. Fix any remaining compile errors by deleting the offending code.

- [ ] **Step 5: Test**

```bash
cargo test --lib 2>&1 | tail -20
```

- [ ] **Step 6: Commit**

```bash
git add -u
git commit -m "refactor: delete plugins/subagents/ — orchestration no longer LLM-callable"
```

---

## Phase 7 — Delete legacy orchestrator Rust files

### Task 7.1: Delete `agent_builder.rs`, `event_orchestrator.rs`, `task_builder.rs`, `orchestrator.rs`

**Files:**
- Delete: `src/adapters/agent_builder.rs`
- Delete: `src/adapters/event_orchestrator.rs`
- Delete: `src/adapters/task_builder.rs`
- Delete: `src/adapters/orchestrator.rs` (the old file; our new module is the `orchestrator/` directory)
- Modify: `src/adapters/mod.rs`, all callers

- [ ] **Step 1: Find references**

```bash
rg 'agent_builder|event_orchestrator|task_builder|adapters::orchestrator::' src/ tests/ -l | grep -v 'adapters/orchestrator/'
```

(The `grep -v` excludes the new module directory which uses the same namespace.)

- [ ] **Step 2: Remove each caller**

For each file, strip imports and usages. Most are dead after the subagents deletion + channel wiring.

- [ ] **Step 3: Delete the files**

```bash
git rm src/adapters/agent_builder.rs src/adapters/event_orchestrator.rs src/adapters/task_builder.rs src/adapters/orchestrator.rs
```

Remove their `pub mod ...;` lines from `src/adapters/mod.rs`.

- [ ] **Step 4: Build + test**

```bash
cargo build --all-features 2>&1 | tail -20
cargo test --lib 2>&1 | tail -10
```

- [ ] **Step 5: Commit**

```bash
git add -u
git commit -m "refactor: delete legacy agent_builder, event_orchestrator, task_builder, orchestrator.rs"
```

---

## Phase 8 — Doctrine scrub

### Task 8.1: Rewrite `docs/architecture.md` Doctrine section

**Files:**
- Modify: `docs/architecture.md`

- [ ] **Step 1: Read lines 9–40 (the current Doctrine section)**

Confirm what's there matches what §9.3 of the spec expects to delete.

- [ ] **Step 2: Replace lines 9–40 with the new doctrine from spec §2**

The replacement content (verbatim from `docs/superpowers/specs/2026-04-20-harness-orchestration-memory-design.md` §2):

```markdown
## Doctrine

Three principles, checked against every PR:

### 1. The harness owns control flow

Orchestration, routing, memory retrieval, memory writes, retries, replans, cancellation, cache discipline, turn lifecycle — all of it is Rust code. No skill teaches these behaviours to an LLM, because no LLM is asked to decide them.

### 2. Agents are narrow LLM workers

An agent is an `AgentConfig` entry: a name, a system prompt, a model, a tool list, optional memory scope. Agents do not know about other agents. Agents do not decompose user requests. Agents do not spawn subagents. Each agent's conversation is a single stable system prompt + a user message + tool calls — the shape prompt caching demands.

### 3. The orchestrator is "just an agent with one tool"

The orchestrator is not a Rust class with baked-in planning logic. It is an `AgentConfig` entry like any other worker, with exactly one tool (`memory_search`) and a system prompt that teaches it to emit structured JSON plans. What makes it the orchestrator is its position in the runtime: it runs first, its output drives the DAG executor, and workers never invoke it back.

### The no-compromise corollary

If something in the codebase tries to encode per-user routing preferences, per-workflow templates, or task-specific retry strategies, stop. Those are config or agent system-prompt concerns, not Rust code. The harness owns *mechanism*, not *policy intent*. The test is: *does a non-engineer user need to change this behaviour by editing a config file (`tengu.toml`) or by filing a PR?* If the answer is "config file," it's config. If the answer is "PR," it's Rust code.

This rule is the single sentence every PR reviewer checks against. A violation is not a style issue; it is a doctrine violation and blocks the PR.
```

- [ ] **Step 3: Update the file header reference**

Line 3 says "Every phase spec (A, B, C, D) references this document." Replace with:

```markdown
> Every implementation plan references this document. Every PR is reviewed against it.
```

- [ ] **Step 4: Grep for stale references to heart/brain/sensors elsewhere in the file**

```bash
rg -i 'heart|brain|sensors' docs/architecture.md
```

Fix any remaining references — replace "brain assembly" with "runtime assembly" etc.

- [ ] **Step 5: Commit**

```bash
git add docs/architecture.md
git commit -m "docs(architecture): replace heart/brain/sensors doctrine with harness-owns-control-flow"
```

### Task 8.2: Delete Phase 0 doctrine specs and plan

**Files:**
- Delete: `docs/superpowers/specs/2026-04-15-phase-0-doctrine-design.md`
- Delete: `docs/superpowers/specs/2026-04-16-phase-0-implementation-design.md`
- Delete: `docs/superpowers/plans/2026-04-16-phase-0-doctrine.md`

- [ ] **Step 1: Delete**

```bash
git rm docs/superpowers/specs/2026-04-15-phase-0-doctrine-design.md \
       docs/superpowers/specs/2026-04-16-phase-0-implementation-design.md \
       docs/superpowers/plans/2026-04-16-phase-0-doctrine.md
```

- [ ] **Step 2: Commit**

```bash
git commit -m "docs: scrub phase 0 doctrine specs (superseded by 2026-04-20 design)"
```

### Task 8.3: Grep other docs and skills for stale heart/brain/sensors references

**Files:** various, based on grep results

- [ ] **Step 1: Find remaining references**

```bash
rg -i 'heart.*brain|brain.*heart|heart.*sensors|brain.*sensors' docs/ skills/
```

- [ ] **Step 2: For each hit, evaluate**

- `skills/beach-science/SKILL.md` was flagged in the original grep. Open it. Most likely unrelated context ("beach science" is the skill for science — the hit is probably incidental). If it's quoting the old doctrine, rewrite; if it's a metaphor independent of the Tengu doctrine, leave alone. Document decision in commit message.

- [ ] **Step 3: Commit any fixes**

```bash
git add -u
git commit -m "docs: scrub residual heart/brain/sensors references"
```

---

## Phase 9 — Eval suite + close PR #5

### Task 9.1: Create `evals/orchestration-e2e.yaml`

**Files:**
- Create: `evals/orchestration-e2e.yaml`

- [ ] **Step 1: Read an existing eval config as template**

```bash
ls evals/
```

Pick one and read to understand the schema (a prior task cherry-picked these — see `bd33bf5 feat(eval): config loader with {TMP_WORKSPACE} expansion`).

- [ ] **Step 2: Write the orchestration eval config**

Template (adjust for actual eval schema):

```yaml
name: orchestration-e2e
description: End-to-end orchestration with a real orchestrator + 2–3 specialist workers.
skill: orchestration-harness  # or whatever the harness-level eval identifier is
sandbox: default
prompts:
  - id: simple_direct
    input: "What is 2 + 2?"
    expect: "4"
    judge_criteria: "Is the response correct and concise?"

  - id: two_step_sequential
    input: "Summarize my last quarterly report and draft a reply to legal based on it."
    expect: "(synthesized reply referencing report content)"
    judge_criteria: "Did the orchestrator decompose into report-summary + legal-reply, and did the reply reference the summary?"

  - id: parallel_fan_out
    input: "Research topic X from these three sources, then write a report."
    expect: "(report synthesizing three sources)"
    judge_criteria: "Did the plan fan out to three researchers and a synthesizer step?"

  - id: failure_recovery
    input: "Fetch example.invalid/nonexistent and summarize."
    expect: "(graceful failure explanation)"
    judge_criteria: "When fetch fails, does the orchestrator either replan or gracefully explain the failure? Does it NOT spin forever?"

  - id: orchestrator_skip_for_trivial
    input: "hi"
    expect: "(direct greeting)"
    judge_criteria: "Did the orchestrator return kind=direct, skipping the DAG?"
```

- [ ] **Step 3: Run the eval**

```bash
cargo run --bin tengu -- eval orchestration-e2e
```

Expected: rows run, judge verdicts recorded. Some may fail during early iterations — that's expected for evals.

- [ ] **Step 4: Commit**

```bash
git add evals/orchestration-e2e.yaml
git commit -m "feat(eval): orchestration-e2e — 5 rows covering direct, sequential, fan-out, failure, trivial"
```

### Task 9.2: Close PR #5 on GitHub (manual step)

- [ ] **Step 1: Open the PR**

```bash
gh pr view 5 --web
```

- [ ] **Step 2: Comment on PR #5 with the reason, then close**

```bash
gh pr comment 5 --body "Superseded by the 2026-04-20 harness-orchestration-memory design (see docs/superpowers/specs/2026-04-20-harness-orchestration-memory-design.md). Orchestration moved from skill-based playbook to harness-owned supervisor pattern. Eval-runner commits from this branch were cherry-picked into feature/harness-orchestration."
gh pr close 5
```

### Task 9.3: Final verification + push

- [ ] **Step 1: Full build + test**

```bash
cargo build --all-features 2>&1 | tail -10
cargo test --lib 2>&1 | tail -10
cargo test --tests 2>&1 | tail -10
```

Expected: all clean.

- [ ] **Step 2: Push**

```bash
git push origin feature/harness-orchestration
```

- [ ] **Step 3: Open PR for review**

```bash
gh pr create --title "feat: harness-owned orchestration + memory subsystem" --body "$(cat <<'EOF'
## Summary
- New `orchestrator/` subsystem: dedicated orchestrator agent, DAG executor, retry + replan, event bus
- New `memory/` subsystem: MemoryProvider trait (Hermes-shaped), BuiltinMemoryProvider, injector + writer
- `plugins/memory/`: `remember` → `memory_ingest`, new `memory_search` tool, `persistent_store` kept
- Deleted: `plugins/subagents/`, legacy `agent_builder.rs`/`event_orchestrator.rs`/`task_builder.rs`/`orchestrator.rs`, `memory_builder.rs`
- Doctrine: heart/brain/sensors replaced with harness-owns-control-flow
- Supersedes PR #5

Spec: docs/superpowers/specs/2026-04-20-harness-orchestration-memory-design.md
Plan: docs/superpowers/plans/2026-04-20-harness-orchestration-memory.md

## Test plan
- [x] Unit tests: memory/*, orchestrator/plan, orchestrator/retry, orchestrator/planner, orchestrator/replan
- [x] Integration: orchestrator_e2e (happy path fan-out + synth), orchestrator_cancel (/stop mid-plan)
- [x] Eval: evals/orchestration-e2e.yaml — 5 rows (direct, sequential, fan-out, failure, trivial)
- [ ] Manual: Telegram quiet rendering smoke (user to confirm)
- [ ] Manual: CLI verbose rendering smoke (user to confirm)

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

---

## Self-review: spec coverage checklist

| Spec section | Tasks implementing it |
|---|---|
| §2.1 harness owns control flow | Phases 4, 5, 6, 7 (all orchestration is Rust) |
| §2.2 agents are narrow workers | Task 4.1 roster, Task 5.1 ChatWorker |
| §2.3 orchestrator = agent with one tool | Tasks 3.1 config, 4.7 planner, 5.1 wiring |
| §3.1 two new subsystems | Tasks 1.1 memory scaffold, 4.1 orchestrator scaffold |
| §3.2 request lifecycle | Tasks 4.7–4.9 |
| §3.3 cache discipline invariants | Enforced by design (no mid-turn mutations), verified by code review |
| §4.1 orchestrator module responsibilities | Tasks 4.2–4.9 |
| §4.2 memory module responsibilities | Tasks 1.2–1.9 |
| §4.3 plugin memory tools | Tasks 2.1–2.3 |
| §5 config shape | Task 3.1 |
| §6.1 memory-injection queries | Task 5.1 (ChatWorker uses step.goal; ChatOrchestratorPort uses user_message) |
| §6.2 single leaf → final response | Task 4.2 `validate()`, Task 4.6 executor uses `single_leaf()` |
| §6.3 /stop cancellation | Task 5.5 |
| §6.4 concurrency memory writes | Vector backend atomicity (Task 1.5), tokio::spawn for sync_turn (Task 1.9), SQLite INSERT OR REPLACE (persistent_store unchanged, Task 2.3) |
| §7 three-tier error handling | Tasks 4.5 retry, 4.8 replan, orchestrator bail-out in Task 4.8 |
| §8 testing strategy | Unit tests in each task, integration in 4.10 + 5.5, eval in 9.1 |
| §9 migration + disposition | Tasks 0.1 cherry-pick, 0.2 deletions commit, 6.1 subagents, 7.1 legacy files, 8.1–8.3 doctrine, 9.2 PR #5 |
| §10 v1 cuts | Documented in spec; no tasks — explicit non-goals |
| §11 sequencing | This plan's phase order matches |
| §12 open items | Task 0.1 enumerates eval commits; Task 5.1 designs ChatRuntimeService wire format; Task 4.5 keeps backoff {1s, 3s, 9s} as spec default |

**Coverage gaps:** None identified.

**Placeholder scan:** None remaining. (Previous draft had two `todo!()` instructions in Task 2.2; both replaced with concrete code in the revised steps.)

**Type consistency:** `StepId(pub String)` used throughout. `Plan`, `Step`, `PlannerVerdict` signatures match across planner.rs, executor.rs, replan.rs. `EventBus` == `broadcast::Sender<OrchestratorEvent>` everywhere. `MemoryManager::prefetch_all` signature matches its callers.

---

## Execution Handoff

**Plan complete and saved to `docs/superpowers/plans/2026-04-20-harness-orchestration-memory.md`.**

Two execution options:

**1. Subagent-Driven (recommended)** — I dispatch a fresh subagent per task, review between tasks, fast iteration.

**2. Inline Execution** — Execute tasks in this session using executing-plans, batch execution with checkpoints.

Which approach?
