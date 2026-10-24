//! `[memory]` (vector memory, recall lanes, `persistent_store` chunking)
//! and `[prompt_budget]`.

use super::*;

/// Persistent vector memory configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Must stay 1536-dim: the Postgres schema is `vector(1536)`.
    #[serde(default = "default_embedding_model")]
    pub embedding_model: String,
    #[serde(default = "default_max_recall_entries")]
    pub max_recall_entries: usize,
    #[serde(default = "default_max_recall_tokens")]
    pub max_recall_tokens: usize,
    /// Vector store dir of agents without a workspace (an agent with one
    /// keeps `<workspace>/memory/`). Unset = `<TENGU_HOME>/memory/`, so a lab
    /// `TENGU_HOME` never writes the operator's store; `~` is expanded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store_path: Option<String>,
    /// Persistent store: chunk size in characters for file vectorization.
    #[serde(default = "default_persistent_store_chunk_size")]
    pub persistent_store_chunk_size: usize,
    /// Persistent store: overlap in characters between consecutive chunks.
    #[serde(default = "default_persistent_store_chunk_overlap")]
    pub persistent_store_chunk_overlap: usize,

    /// Number of most-recent user messages of this session the planner shows
    /// each turn for multi-turn dialogue coherence — kept in an in-memory ring
    /// per planner (lost on restart), not reloaded from storage. Deterministic,
    /// not vector-search based.
    #[serde(default = "default_session_recent_n")]
    pub session_recent_n: usize,
    /// Top-K breadth for fuzzy cross-plan recall during replan (vector
    /// search over `agentic_memory` step outputs only; user messages are
    /// excluded to keep signal/noise apart).
    #[serde(default = "default_cross_plan_top_k")]
    pub cross_plan_top_k: usize,
    /// Top-K breadth for fuzzy cross-session message recall (vector
    /// search over `agentic_memory` user messages, which the planner writes
    /// on every `plan()`). Defaults to `0` = off. Setting this
    /// to e.g. `3` injects a "Cross-session message recall" block of the
    /// top-3 semantically-similar prior user messages before the current
    /// turn, surviving across restarts and across `session_id`s. Use
    /// sparingly — values >5 inflate the planner prompt without much
    /// signal gain because conversational text tends to cluster.
    #[serde(default = "default_cross_session_msg_top_k")]
    pub cross_session_msg_top_k: usize,
    /// Top-K breadth for fuzzy WITHIN-session output recall on the planner's
    /// normal `plan()` path (vector search over `agentic_memory` step
    /// outputs filtered by the planner's `session_id`). Defaults to `0` =
    /// off. The planner and `SubprocessRunner` share one `session_id`, so
    /// step summaries captured by `run-agent` in this session are
    /// retrievable on the next user turn; with `0`, step outputs are read
    /// only on `replan()` (`cross_plan_top_k`), and a follow-up like "was
    /// the report saved?" finds no record of the step.
    /// Recommended `3–5` once your sandbox config opts in.
    #[serde(default = "default_within_session_output_top_k")]
    pub within_session_output_top_k: usize,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            embedding_model: default_embedding_model(),
            max_recall_entries: default_max_recall_entries(),
            max_recall_tokens: default_max_recall_tokens(),
            store_path: None,
            persistent_store_chunk_size: default_persistent_store_chunk_size(),
            persistent_store_chunk_overlap: default_persistent_store_chunk_overlap(),
            session_recent_n: default_session_recent_n(),
            cross_plan_top_k: default_cross_plan_top_k(),
            cross_session_msg_top_k: default_cross_session_msg_top_k(),
            within_session_output_top_k: default_within_session_output_top_k(),
        }
    }
}

pub(super) fn default_session_recent_n() -> usize {
    10
}
pub(super) fn default_cross_plan_top_k() -> usize {
    5
}
pub(super) fn default_cross_session_msg_top_k() -> usize {
    0
}
pub(super) fn default_within_session_output_top_k() -> usize {
    0
}

pub(super) fn default_embedding_model() -> String {
    crate::domain::memory::DEFAULT_EMBEDDING_MODEL.to_string()
}
pub(super) fn default_max_recall_entries() -> usize {
    5
}
pub(super) fn default_max_recall_tokens() -> usize {
    600
}
pub(super) fn default_persistent_store_chunk_size() -> usize {
    1000
}

pub(super) fn default_persistent_store_chunk_overlap() -> usize {
    200
}

/// System prompt token budget configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptBudgetConfig {
    #[serde(default = "default_max_file_tokens")]
    pub max_file_tokens: usize,
    #[serde(default = "default_max_skill_context_tokens")]
    pub max_skill_context_tokens: usize,
    #[serde(default = "default_max_total_tokens")]
    pub max_total_tokens: usize,
}

impl Default for PromptBudgetConfig {
    fn default() -> Self {
        Self {
            max_file_tokens: default_max_file_tokens(),
            max_skill_context_tokens: default_max_skill_context_tokens(),
            max_total_tokens: default_max_total_tokens(),
        }
    }
}

pub(super) fn default_max_file_tokens() -> usize {
    2000
}
pub(super) fn default_max_skill_context_tokens() -> usize {
    16000
}
pub(super) fn default_max_total_tokens() -> usize {
    32000
}
