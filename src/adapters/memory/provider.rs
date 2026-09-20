//! `MemoryProvider` — abstract base for pluggable memory backends.
//!
//! One built-in provider (`BuiltinMemoryProvider`) is always registered
//! first. At most one external provider (Letta, Mem0, …) may slot in
//! alongside it. Harness-level, never exposed as LLM tools — retrieval
//! and writes are driven by `MemoryInjector` / `MemoryWriter`.

use async_trait::async_trait;
use std::path::Path;

// Several lifecycle methods on this trait (`is_available`, `initialize`,
// `system_prompt_block`, `shutdown`) are unused on the v2 RagPlanner /
// SubprocessRunner path — subagents persist step summaries to Postgres `agentic_memory` themselves (`main.rs::try_persist_agentic_step_summary`)
// rather than going through `MemoryProvider`. The trait is kept intact so
// the static-mode path keeps compiling; Phase 7.1 will collapse this whole
// hierarchy once the static path is deleted.
#[allow(dead_code)]
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

    /// Clean teardown — flush queues, close connections.
    async fn shutdown(&self);
}
