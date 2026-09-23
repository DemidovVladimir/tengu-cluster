//! Metrics — context/token consumption telemetry.
//!
//! Lightweight observability layer over every LLM and embedding call so the
//! user can see how much context each turn cost and where the bytes went.
//!
//! ## Doctrine
//!
//! Metrics are **observability only** — code that records metrics never blocks
//! progress on success/failure of the metrics path. Every `record()` call is
//! cheap, fail-soft, and side-effect-isolated.
//!
//! ## Design
//!
//! - One `MetricsRecord` per LLM call (planner / subagent turn) and per
//!   embedding API call. Three "kinds" carried in the same struct so a single
//!   subscriber can render all of them.
//! - A process-global broadcast sink (`OnceLock<broadcast::Sender>`) so any
//!   code path can call `application::metrics::record(rec)` without trait/Arc plumbing.
//!   `build_orchestrator` installs the sink on first call; subsequent calls
//!   are silent no-ops.
//! - Every `record()` ALSO emits a structured `tracing::info!` line — this is
//!   the always-on baseline visible via `RUST_LOG=tengu=info`. The bus is
//!   only used by in-process subscribers (the TUI bottom-bar).
//! - A `MetricsAggregator` listens on the bus and rolls up totals per
//!   session / per agent / per kind. The TUI uses one to render a compact
//!   status line. Aggregator is opt-in — the bus works without it.
//!
//! ## Scope
//!
//! - Planner LLM call (one per user turn): full telemetry including
//!   per-context-layer breakdown (system / roster / cross_session / history /
//!   user_message).
//! - Subagent LLM turns (1..N per step): per-turn telemetry. Carried back over
//!   IPC via `AgentIpcOutput.metrics` and re-emitted on the parent's bus.
//! - Embedding calls: tokens via OpenRouter `usage.total_tokens` (we now read
//!   it back where previously we discarded the entire response wrapper).

use serde::{Deserialize, Serialize};

/// One metrics record per LLM/embedding call. Serialised over IPC for
/// subagent turns; broadcast on the in-process bus for everything.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsRecord {
    /// Unix epoch seconds at the time of recording.
    pub ts_unix: u64,
    /// `RagPlanner.session_id` for planner records, `SubprocessRunner.session_id`
    /// for subagent records, `"-"` for embeddings (no session attribution).
    pub session_id: String,
    pub kind: MetricsKind,
    /// Logical agent name. Planner: orchestrator agent. Subagent: the
    /// dispatched agent name. Embedding: `"embedder"`.
    pub agent: String,
    /// Engine-specific model slug (e.g. `anthropic/claude-sonnet-4-6` for
    /// OpenRouter, `claude-sonnet-4-6` for Claude Code, `text-embedding-3-small`
    /// for the embedder).
    pub model: String,
    /// Tokens used by the prompt/input. For embeddings this is the only
    /// non-zero token field (no completion).
    pub prompt_tokens: u32,
    /// Tokens used by the model's response. Always 0 for embeddings.
    pub completion_tokens: u32,
    /// `prompt_tokens + completion_tokens`. Stored for cheap rendering.
    pub total_tokens: u32,
    /// Pre-tokenisation char count of the assembled prompt. Cheap to compute
    /// locally and useful as a sanity check on the `prompt_tokens` figure
    /// when an engine doesn't return usage (rare but possible).
    pub prompt_chars: u32,
    /// Pre-tokenisation byte count (UTF-8) of the assembled prompt.
    pub prompt_bytes: u32,
    /// Char count of the model's response text.
    pub response_chars: u32,
    /// Wall-clock latency of the LLM/embedding call.
    pub latency_ms: u64,
    /// Pre-call breakdown of which prompt sections contributed which bytes.
    /// Empty for non-planner records (subagent turns don't compose layers
    /// the same way; embeddings are single-string).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub layers: Vec<MetricsLayer>,
    /// Plan step id for subagent records — links the metric back to the
    /// plan that dispatched it. `None` for planner + embedding records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_id: Option<String>,
}

/// Categorical label so subscribers can filter / aggregate by call type.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum MetricsKind {
    /// `RagPlanner::plan/replan` LLM call. One per user turn (or per replan).
    Planner,
    /// One iteration of the subagent's tool loop. Multiple per step in
    /// agents that call tools.
    Subagent,
    /// OpenRouter embeddings API call (used by reindex + per-turn searches).
    Embedding,
    /// OpenRouter chat call from the `agentic_memory` LLM Wiki compiler
    /// (`compile_wiki`). Occasional + explicit, not per-turn.
    WikiCompiler,
}

impl MetricsKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MetricsKind::Planner => "planner",
            MetricsKind::Subagent => "subagent",
            MetricsKind::Embedding => "embedding",
            MetricsKind::WikiCompiler => "wiki_compiler",
        }
    }
}

/// One row in `MetricsRecord.layers`.
///
/// Layer names come from a stable set so subscribers can group across runs:
/// `"system"`, `"roster"`, `"cross_session"`, `"history"`, `"recall"`,
/// `"user_message"`, `"prior_plan"`. Layer accounting is an approximation
/// — the actual prompt seen by the model also includes engine-injected
/// framing that we don't measure. Treat layers as relative attribution,
/// not absolute byte-perfect accounting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsLayer {
    pub name: String,
    pub chars: u32,
    pub bytes: u32,
}

impl MetricsLayer {
    pub fn from_text(name: impl Into<String>, text: &str) -> Self {
        Self {
            name: name.into(),
            chars: text.chars().count() as u32,
            bytes: text.len() as u32,
        }
    }
}
/// Convenience wrapper for the common "I'm about to make an LLM call" pattern.
/// Captures the start instant so the caller can compute latency at the end.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// =====================================================================
// In-process rollup aggregator.
// =====================================================================

/// Per-(session, agent, kind) running totals. The TUI uses this to render a
/// compact "tok in/out N/M · session S · researcher R" status line.
#[derive(Debug, Clone, Default)]
pub struct AgentRollup {
    pub call_count: u32,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub prompt_chars: u64,
    pub latency_ms_total: u64,
}

impl AgentRollup {
    pub fn absorb(&mut self, rec: &MetricsRecord) {
        self.call_count += 1;
        self.prompt_tokens += rec.prompt_tokens as u64;
        self.completion_tokens += rec.completion_tokens as u64;
        self.total_tokens += rec.total_tokens as u64;
        self.prompt_chars += rec.prompt_chars as u64;
        self.latency_ms_total += rec.latency_ms;
    }
}

/// Lightweight aggregator state — clone-cheap (it's small) so we can show
/// snapshots in the TUI without holding a lock across the render path.
#[derive(Debug, Clone, Default)]
pub struct AggregatorState {
    /// Last LLM record (any kind). Useful as a "current turn delta" indicator.
    pub last: Option<MetricsRecord>,
    /// Sum across every record this aggregator has seen.
    pub overall: AgentRollup,
    /// Per-agent breakdown — keyed by `agent` string.
    pub by_agent: std::collections::HashMap<String, AgentRollup>,
    /// Per-kind breakdown.
    pub by_kind: std::collections::HashMap<&'static str, AgentRollup>,
}

impl AggregatorState {
    pub fn absorb(&mut self, rec: MetricsRecord) {
        self.overall.absorb(&rec);
        self.by_agent
            .entry(rec.agent.clone())
            .or_default()
            .absorb(&rec);
        self.by_kind
            .entry(rec.kind.as_str())
            .or_default()
            .absorb(&rec);
        self.last = Some(rec);
    }

    /// Render a compact one-line summary for the TUI bottom bar.
    /// Returns `None` if no records have been seen yet.
    pub fn format_status_line(&self) -> Option<String> {
        let last = self.last.as_ref()?;
        let agent_part = self
            .by_agent
            .iter()
            .filter(|(_, r)| r.total_tokens > 0)
            .max_by_key(|(_, r)| r.total_tokens)
            .map(|(name, r)| format!(" · {} {}", name, fmt_tokens(r.total_tokens)))
            .unwrap_or_default();
        Some(format!(
            "tok in/out {}/{} · last {} ({}) · session {}{}",
            fmt_tokens(self.overall.prompt_tokens),
            fmt_tokens(self.overall.completion_tokens),
            last.kind.as_str(),
            fmt_tokens(last.total_tokens as u64),
            fmt_tokens(self.overall.total_tokens),
            agent_part,
        ))
    }
}

fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record_for(kind: MetricsKind, agent: &str, prompt: u32, completion: u32) -> MetricsRecord {
        MetricsRecord {
            ts_unix: 0,
            session_id: "s".to_string(),
            kind,
            agent: agent.to_string(),
            model: "m".to_string(),
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: prompt + completion,
            prompt_chars: 0,
            prompt_bytes: 0,
            response_chars: 0,
            latency_ms: 0,
            layers: Vec::new(),
            step_id: None,
        }
    }

    #[test]
    fn aggregator_sums_overall_and_per_agent() {
        let mut s = AggregatorState::default();
        s.absorb(record_for(MetricsKind::Planner, "aura", 100, 50));
        s.absorb(record_for(MetricsKind::Subagent, "researcher", 200, 100));
        s.absorb(record_for(MetricsKind::Subagent, "researcher", 300, 150));
        assert_eq!(s.overall.prompt_tokens, 600);
        assert_eq!(s.overall.completion_tokens, 300);
        assert_eq!(s.overall.total_tokens, 900);
        assert_eq!(s.by_agent["researcher"].total_tokens, 750);
        assert_eq!(s.by_agent["aura"].total_tokens, 150);
        assert_eq!(s.by_kind["subagent"].call_count, 2);
    }

    #[test]
    fn fmt_tokens_compact() {
        assert_eq!(fmt_tokens(950), "950");
        assert_eq!(fmt_tokens(1_500), "1.5k");
        assert_eq!(fmt_tokens(2_300_000), "2.3M");
    }

    #[test]
    fn metrics_layer_counts_chars_and_bytes() {
        // Russian "Привет" is 6 chars but 12 bytes in UTF-8.
        let l = MetricsLayer::from_text("hi", "Привет");
        assert_eq!(l.chars, 6);
        assert_eq!(l.bytes, 12);
    }

    #[test]
    fn record_round_trips_through_serde() {
        let rec = record_for(MetricsKind::Planner, "aura", 100, 50);
        let json = serde_json::to_string(&rec).unwrap();
        let back: MetricsRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.agent, "aura");
        assert_eq!(back.total_tokens, 150);
    }
}
