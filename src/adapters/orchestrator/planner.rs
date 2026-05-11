//! Planner — runs the orchestrator agent's LLM call, returns Plan or direct response.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::adapters::orchestrator::plan::Plan;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlannerVerdict {
    Direct {
        response: String,
    },
    Plan {
        #[serde(flatten)]
        plan: Plan,
    },
}

#[async_trait]
pub trait Planner: Send + Sync {
    /// First-plan call: user message + empty context.
    async fn plan(&self, user_message: &str) -> anyhow::Result<PlannerVerdict>;

    /// Replan call: user message + failure context to avoid repeat mistakes.
    async fn replan(
        &self,
        user_message: &str,
        prior_plan: &Plan,
        failed_step_id: &str,
        error: &str,
    ) -> anyhow::Result<PlannerVerdict>;
}

use std::sync::Arc;

/// Minimal port the planner needs from the chat runtime (avoids cyclic deps).
#[async_trait]
pub trait OrchestratorChatPort: Send + Sync {
    /// Run a single LLM conversation turn and return the final message
    /// (JSON string). The port handles system-prompt assembly, tool
    /// loop (memory_search), and memory injection.
    async fn run_orchestrator_turn(
        &self,
        agent: &str,
        user_message: &str,
    ) -> anyhow::Result<String>;

    /// Phase 4c — like `run_orchestrator_turn` but with `system_prompt`
    /// replacing the agent's `identity.instructions` for this single call.
    /// Used by the RAG planner to inject `skills/orchestrator/SKILL.md`
    /// as the planner system prompt. Default impl falls back to the
    /// override-less call.
    async fn run_orchestrator_turn_with_system(
        &self,
        agent: &str,
        _system_prompt: &str,
        user_message: &str,
    ) -> anyhow::Result<String> {
        self.run_orchestrator_turn(agent, user_message).await
    }

    /// Metered variant — returns `(reply, telemetry)` so callers can build
    /// a `MetricsRecord` with prompt/completion tokens and wall-clock
    /// latency. Default impl falls back to `run_orchestrator_turn_with_system`
    /// and returns zeroed telemetry. The runtime impl
    /// (`ChatOrchestratorPortImpl`) overrides this to plumb real numbers.
    async fn run_orchestrator_turn_with_system_metered(
        &self,
        agent: &str,
        system_prompt: &str,
        user_message: &str,
    ) -> anyhow::Result<(
        String,
        crate::adapters::orchestrator::wiring::TurnTelemetry,
    )> {
        let reply = self
            .run_orchestrator_turn_with_system(agent, system_prompt, user_message)
            .await?;
        Ok((
            reply,
            crate::adapters::orchestrator::wiring::TurnTelemetry::default(),
        ))
    }
}

/// Parse the orchestrator LLM's response into a [`PlannerVerdict`].
///
/// Tolerates four common sloppiness patterns from LLMs:
///   1. Raw JSON — parses directly.
///   2. JSON wrapped in markdown fences (```json ... ```).
///   3. JSON embedded in prose ("Here's my plan: {...}. Let me know if…").
///   4. **Phase 7.4 — pure-prose fallback.** Some engines (notably
///      Claude Code CLI) return conversational text even when the system
///      prompt says JSON-only. When NO balanced `{...}` substring exists
///      at all, we wrap the whole text as a `Direct { response: ... }`
///      verdict. This is a safety net — much better UX than a hard parse
///      error, and the user still gets the model's reply.
///
/// Phase 7.1 — promoted from `OrchestratorAgentPlanner::parse_verdict`
/// (associated function with no `self`) to a free function when that
/// type was deleted. RagPlanner is the only remaining caller.
pub(crate) fn parse_verdict(raw: &str) -> anyhow::Result<PlannerVerdict> {
    let trimmed = raw.trim();

    // Path 1: raw JSON (covers the happy case).
    if let Ok(v) = serde_json::from_str::<PlannerVerdict>(trimmed) {
        return Ok(v);
    }

    // Path 2: markdown fences.
    let stripped = trimmed.strip_prefix("```json").unwrap_or(trimmed);
    let stripped = stripped.strip_prefix("```").unwrap_or(stripped);
    let stripped = stripped.strip_suffix("```").unwrap_or(stripped);
    if let Ok(v) = serde_json::from_str::<PlannerVerdict>(stripped.trim()) {
        return Ok(v);
    }

    // Path 3: JSON embedded in prose. Find the largest balanced {...}
    // substring (considers brace nesting + string literals with escapes
    // so "{" inside a string doesn't unbalance the tracker).
    if let Some(json) = extract_balanced_json_object(trimmed) {
        if let Ok(v) = serde_json::from_str::<PlannerVerdict>(&json) {
            return Ok(v);
        }
    }

    // Path 4 (Phase 7.4): pure-prose fallback. Some engines (Claude Code
    // CLI in particular) return conversational text even when told to
    // emit JSON-only. Rather than failing the whole turn, wrap the text
    // as a Direct verdict — same as if the planner had said
    // {"kind":"direct","response":"<this prose>"} explicitly.
    //
    // Logged at warn-level so users can see this is happening and tune
    // the SKILL.md or switch engines. Truncated to keep the log line
    // bounded.
    if !trimmed.is_empty() {
        let preview: String = trimmed.chars().take(300).collect();
        let suffix = if trimmed.chars().count() > 300 { "…" } else { "" };
        tracing::warn!(
            preview = %preview,
            suffix = %suffix,
            len = trimmed.len(),
            "planner LLM returned non-JSON text; wrapping as Direct verdict (Phase 7.4 fallback)"
        );
        return Ok(PlannerVerdict::Direct {
            response: trimmed.to_string(),
        });
    }

    // Truly empty output — return a parse error with the raw text included
    // so the user can see what the planner LLM actually said.
    let verdict: PlannerVerdict = serde_json::from_str(trimmed).map_err(|e| {
        let preview: String = raw.chars().take(300).collect();
        let suffix = if raw.chars().count() > 300 { "…" } else { "" };
        anyhow::anyhow!(
            "planner LLM returned empty/unparseable output: {} (raw: {:?}{})",
            e,
            preview,
            suffix
        )
    })?;
    Ok(verdict)
}

/// Find the largest `{...}` substring in `s` that is JSON-brace-balanced.
/// Naively handles string literals (so `{"x": "}"}` doesn't unbalance the
/// stack) including backslash escapes.
fn extract_balanced_json_object(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut start: Option<usize> = None;
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escape = false;

    for (i, &b) in bytes.iter().enumerate() {
        if in_string {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'{' => {
                if depth == 0 {
                    start = Some(i);
                }
                depth += 1;
            }
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    if let Some(s0) = start {
                        return Some(s[s0..=i].to_string());
                    }
                }
            }
            b'"' => in_string = true,
            _ => {}
        }
    }
    None
}

// =====================================================================
// RagPlanner — the only Planner implementation as of Phase 7.1.
//
// Queries `tengu_registry` per turn and prepends a top-K ranked list to
// the user message; uses `parse_verdict` (free fn above) for tolerant
// JSON parsing of the LLM's plan output.
// =====================================================================

#[cfg(feature = "qdrant")]
pub struct RagPlanner {
    orchestrator_agent: String,
    chat: Arc<dyn OrchestratorChatPort>,
    memory_config: crate::adapters::config::MemoryConfig,
    /// Lazy-initialised on first `plan()` call so the (sync) `build_orchestrator`
    /// constructor does not need to be made async or carry a tokio runtime.
    rag: tokio::sync::OnceCell<Arc<crate::adapters::rag::RagStore>>,
    top_k: usize,
    /// Phase 4c — body of `skills/orchestrator/SKILL.md` (frontmatter
    /// stripped) used as the planner system prompt. Falls back to a
    /// hardcoded minimal instruction if the file is missing.
    system_prompt: String,
    /// Phase 6.4 (lite) — in-memory ring buffer of recent user messages
    /// for THIS RagPlanner instance. Survives within one channel session,
    /// lost on restart; mixes turns from concurrent Telegram chats sharing
    /// one RagPlanner. Used as the source of the "## Recent user messages
    /// this session" prompt block. Phase 6.4 (full) writes to `tengu_messages`
    /// in addition for cross-restart durability.
    session_history: tokio::sync::Mutex<Vec<String>>,
    /// Phase 6.4 (full) — id stamped on every user message we persist to
    /// `tengu_messages`. Resolution order at construction:
    /// 1. `TENGU_SESSION_ID` env var if set (lets tests pin a known id;
    ///    advanced ops users can stitch sessions across restarts manually).
    /// 2. fresh `Uuid::new_v4()` — matches `SubprocessRunner` per-instance
    ///    behaviour. NOTE: a fresh UUID per process means restart-pickup
    ///    does NOT magically work via `WHERE session_id = ?` — the value
    ///    of writing it now is forward-compat: the writes are durable, and
    ///    a future commit can wire `search_messages` into the planner
    ///    prompt for cross-session semantic recall regardless of session_id.
    session_id: String,
    /// Phase 6.6 — passed through to `auto_reindex_once` so the registry
    /// reindex on first chat turn enumerates real MCP tools alongside the
    /// built-ins. Cloned from `Config.mcp_servers` at planner construction.
    mcp_servers: Vec<crate::adapters::config::McpServerConfig>,
    /// Phase 6.1 (full) — optional event bus for emitting
    /// `OrchestratorEvent::RagQueried` on every `plan()`/`replan()`.
    /// `None` means structured events are silently dropped (the
    /// `tracing::info!` line still fires regardless).
    bus: Option<crate::adapters::orchestrator::events::EventBus>,
}

#[cfg(feature = "qdrant")]
impl RagPlanner {
    pub fn new(
        orchestrator_agent: String,
        chat: Arc<dyn OrchestratorChatPort>,
        memory_config: crate::adapters::config::MemoryConfig,
        mcp_servers: Vec<crate::adapters::config::McpServerConfig>,
        bus: Option<crate::adapters::orchestrator::events::EventBus>,
        // Fix B (2026-05-09) — session_id resolved by `build_orchestrator`
        // and shared with `SubprocessRunner`, so the parent's recall query
        // and the child's `compress_and_store` write key match. Pre-Fix-B
        // the planner minted independently here and the runner minted
        // independently of THAT, so within-session output recall (Fix A)
        // would have filtered to a session_id no one else used.
        session_id: String,
    ) -> Self {
        let system_prompt = load_orchestrator_skill_body().unwrap_or_else(|| {
            tracing::warn!(
                "skills/orchestrator/SKILL.md missing or unreadable; using fallback inline planner prompt"
            );
            FALLBACK_PLANNER_PROMPT.to_string()
        });
        Self {
            orchestrator_agent,
            chat,
            memory_config,
            rag: tokio::sync::OnceCell::new(),
            top_k: 20,
            system_prompt,
            session_history: tokio::sync::Mutex::new(Vec::new()),
            session_id,
            mcp_servers,
            bus,
        }
    }

    /// Resolve (and cache) the underlying `RagStore`. Constructing it requires
    /// `OPENROUTER_API_KEY` and a reachable Qdrant on `memory.qdrant_url`; if
    /// either is missing the planner returns `Err` from this helper and the
    /// caller falls through to graceful degradation in `plan()`/`replan()`.
    ///
    /// Auto-reindex on first init: when the RagStore is built for the first
    /// time in a process, we run the same `clear + index_tools + index_agents
    /// + index_skills` sequence that `tengu registry reindex-all` does, so
    /// freshly-edited `agents/*.toml` entries take effect without a manual
    /// reindex step. Fail-soft — a reindex error (Qdrant write fault, OpenAI
    /// rate-limit, missing agents/ directory) is logged and the planner keeps
    /// going with whatever is already indexed.
    async fn rag(&self) -> anyhow::Result<&Arc<crate::adapters::rag::RagStore>> {
        self.rag
            .get_or_try_init(|| async {
                let store: Arc<crate::adapters::rag::RagStore> =
                    crate::adapters::rag::RagStore::from_config(self.memory_config.clone())
                        .await
                        .map(Arc::new)?;
                self.auto_reindex_once(&store).await;
                Ok::<_, anyhow::Error>(store)
            })
            .await
    }

    /// Run a one-shot registry reindex from the current working directory.
    /// Called from `rag()` on first init. Errors are logged, never propagated —
    /// the caller's planner pipeline must keep functioning even when the
    /// registry is stale.
    ///
    /// Phase 6.6 — passes `self.mcp_servers` through so the reindex
    /// includes real MCP tools alongside the built-ins. Connecting to
    /// dead/misconfigured MCP servers is tolerated downstream
    /// (`enumerate_mcp_tools` is fail-soft per server).
    async fn auto_reindex_once(&self, store: &crate::adapters::rag::RagStore) {
        let root = match std::env::current_dir() {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "auto-reindex skipped: could not resolve current_dir");
                return;
            }
        };
        match crate::adapters::rag::indexer::reindex_all_workspace(
            store,
            &root,
            &self.mcp_servers,
        )
        .await
        {
            Ok(r) if r.unchanged => tracing::info!(
                root = %root.display(),
                "auto-reindex skipped: workspace fingerprint unchanged (Phase 6.2 cache hit)"
            ),
            Ok(r) => tracing::info!(
                tools = r.tools_indexed,
                agents = r.agents_indexed,
                skills = r.skills_indexed,
                mcp_servers = self.mcp_servers.len(),
                root = %root.display(),
                "auto-reindexed tengu_registry on first rag-mode use"
            ),
            Err(e) => tracing::warn!(
                error = %e,
                "auto-reindex failed; continuing with existing registry contents"
            ),
        }

        // Phase 6.3 — TTL purge on the same cold-start hook. Internally
        // a no-op when `memory.ttl_days == 0` (default), so this is
        // free for users who haven't opted in. When >0, sweeps entries
        // older than the cutoff from tengu_messages + tengu_outputs.
        // Fail-soft: a Qdrant error logs warn and continues.
        match store.ttl_cleanup().await {
            Ok(0) => {} // either ttl_days=0 or nothing to purge — silent
            Ok(n) => tracing::info!(purged = n, "rag ttl_cleanup purged old entries"),
            Err(e) => tracing::warn!(error = %e, "rag ttl_cleanup failed; continuing"),
        }
    }

    /// Phase 6.4 (lite) — push the current user message into the session
    /// history buffer and return the formatted "recent dialogue" block.
    /// Returns an empty string for the first turn (no prior context to show).
    async fn update_and_format_history(&self, user_message: &str) -> String {
        let limit = self.memory_config.session_recent_n.max(1);
        let mut hist = self.session_history.lock().await;
        hist.push(user_message.to_string());
        // Cap the buffer at 2× the limit so we keep some lookahead for
        // unusual replan sequences without unbounded growth.
        let cap = limit.saturating_mul(2).max(limit + 4);
        if hist.len() > cap {
            let drain = hist.len() - cap;
            hist.drain(..drain);
        }
        format_history(&hist, limit)
    }

    /// Read-only sibling for paths that already pushed (e.g. replan() runs
    /// after plan() within a single turn cycle and shouldn't double-append).
    async fn format_history_only(&self) -> String {
        let limit = self.memory_config.session_recent_n.max(1);
        let hist = self.session_history.lock().await;
        format_history(&hist, limit)
    }

    /// Phase 6.4 (full, read-back) — semantic recall over `tengu_messages`.
    /// Returns a formatted prompt block of the top-K semantically-similar
    /// prior user messages, or an empty string when:
    /// - the config knob `memory.cross_session_msg_top_k` is 0 (default,
    ///   so existing users see no behaviour change),
    /// - the RagStore is unavailable (Qdrant down, OPENROUTER_API_KEY missing),
    /// - the search returns nothing (collection empty / cold start), or
    /// - every hit is a near-duplicate of the current message (filtered out
    ///   so the LLM doesn't see "the user already asked this").
    ///
    /// Called from BOTH `plan()` and `replan()` BEFORE
    /// `persist_user_message()` so the just-written current message can
    /// never appear in its own recall block. (The semantic embedder will
    /// happily return a self-match at score ~1.0 if we read after writing.)
    /// Fix E (2026-05-09) — `embed_vec` is the cached user-message vector
    /// from `plan()`; when `Some`, skip the duplicate embedder call.
    /// `None` means the caller didn't pre-compute (or the upstream embed
    /// already failed) — we fall back to embedding internally for backward
    /// compatibility with replan() and standalone test paths.
    async fn cross_session_recall_block(
        &self,
        user_message: &str,
        embed_vec: Option<&[f32]>,
    ) -> String {
        let k = self.memory_config.cross_session_msg_top_k;
        if k == 0 {
            return String::new();
        }
        let rag = match self.rag().await {
            Ok(r) => r.clone(),
            Err(e) => {
                tracing::debug!(error = %e, "skip cross_session_recall: RagStore unavailable");
                return String::new();
            }
        };
        // Over-fetch a couple slots so we have headroom after dedup-filter.
        let oversample = k.saturating_add(2);
        let raw_hits_res = match embed_vec {
            Some(vec) => rag.search_messages_with_vec(vec, oversample).await,
            None => rag.search_messages(user_message, oversample).await,
        };
        let raw_hits = match raw_hits_res {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(error = %e, "cross_session_recall: search_messages failed");
                return String::new();
            }
        };
        // Filter out exact-content duplicates — the planner has no use for
        // "the user previously asked X" where X is exactly the current
        // message. (Edge case: a Qdrant write that already propagated by
        // the time we read; or genuine repeat queries from the user.)
        let trimmed_current = user_message.trim();
        let filtered: Vec<_> = raw_hits
            .into_iter()
            .filter(|h| h.description.trim() != trimmed_current)
            .take(k)
            .collect();
        if filtered.is_empty() {
            return String::new();
        }
        let mut s = String::from("\n## Cross-session message recall\n\n");
        s.push_str("_(semantically-similar prior user messages from `tengu_messages`)_\n\n");
        for (i, h) in filtered.iter().enumerate() {
            // Truncate so the planner prompt stays bounded — same 400-char
            // budget as the lite per-session history block.
            let snippet: String = h.description.chars().take(400).collect();
            s.push_str(&format!("{}. score={:.2}  {}\n", i + 1, h.score, snippet));
            if h.description.chars().count() > 400 {
                s.push_str("   …(truncated)\n");
            }
        }
        s.push('\n');
        s
    }

    /// Fix A (2026-05-09) — within-session output recall.
    ///
    /// Returns a formatted prompt block of the top-K semantically-similar
    /// step outputs from THIS session (filtered by `rag_session_id ==
    /// self.session_id`), or an empty string when:
    /// - the config knob `memory.within_session_output_top_k` is 0 (default),
    /// - the RagStore is unavailable, or
    /// - no outputs in this session match.
    ///
    /// Pairs with Fix B's unified session_id wiring: `compress_and_store`
    /// writes `rag_session_id = AgentIpcInput.session_id`, which now equals
    /// the planner's `session_id`. Without Fix B this filter would never
    /// match (parent and child mint independent UUIDs).
    async fn session_output_recall_block(
        &self,
        user_message: &str,
        embed_vec: Option<&[f32]>,
    ) -> String {
        let k = self.memory_config.within_session_output_top_k;
        if k == 0 {
            return String::new();
        }
        let rag = match self.rag().await {
            Ok(r) => r.clone(),
            Err(e) => {
                tracing::debug!(error = %e, "skip session_output_recall: RagStore unavailable");
                return String::new();
            }
        };
        // Fix E — reuse the cached vector when plan() pre-computed it.
        let hits_res = match embed_vec {
            Some(vec) => {
                rag.search_outputs_for_session_with_vec(vec, &self.session_id, k)
                    .await
            }
            None => {
                rag.search_outputs_for_session(user_message, &self.session_id, k)
                    .await
            }
        };
        let hits = match hits_res {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(error = %e, "session_output_recall: search failed");
                return String::new();
            }
        };
        if hits.is_empty() {
            return String::new();
        }
        let mut s = String::from("\n## Recent step outputs (this session)\n\n");
        s.push_str("_(prior subagent results from `tengu_outputs`, filtered to this `session_id` — the planner can answer follow-up questions about completed work without redoing the steps)_\n\n");
        for (i, h) in hits.iter().enumerate() {
            let snippet: String = h.description.chars().take(400).collect();
            s.push_str(&format!(
                "{}. score={:.2}  step={}\n   {}\n",
                i + 1,
                h.score,
                h.name,
                snippet
            ));
            if h.description.chars().count() > 400 {
                s.push_str("   …(truncated)\n");
            }
        }
        s.push('\n');
        s
    }

    /// Phase 6.4 (full) — durably persist a user message to `tengu_messages`.
    /// Called from `plan()` only (not `replan()`, which receives the same
    /// `user_message` in the same turn cycle — a second write would create
    /// a duplicate row). Fail-soft: any error path (RagStore unavailable,
    /// embedder rate-limit, Qdrant unreachable) is logged and swallowed
    /// because the planner must still be able to plan + respond.
    ///
    /// `step_id` is `None` because this is a top-of-turn user message,
    /// not a step output. `created_at` is unix-seconds at write time.
    async fn persist_user_message(&self, user_message: &str, embed_vec: Option<&[f32]>) {
        let trimmed = user_message.trim();
        if trimmed.is_empty() {
            return;
        }
        let rag = match self.rag().await {
            Ok(r) => r.clone(),
            Err(e) => {
                tracing::debug!(error = %e, "skip persist_user_message: RagStore unavailable");
                return;
            }
        };
        let created_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let entry = crate::adapters::rag::MemoryEntry {
            kind: crate::adapters::rag::MemoryKind::Message,
            session_id: self.session_id.clone(),
            step_id: None,
            content: trimmed.to_string(),
            created_at,
        };
        // Fix E — reuse the cached vector when available; the trimmed
        // content matches the input the embedder would have used (modulo
        // leading/trailing whitespace, which the embedder ignores anyway).
        let store_res = match embed_vec {
            Some(vec) => rag.store_memory_with_vec(entry, vec.to_vec()).await,
            None => rag.store_memory(entry).await,
        };
        match store_res {
            Ok(id) => tracing::debug!(
                id = %id,
                session_id = %self.session_id,
                "persisted user message to tengu_messages"
            ),
            Err(e) => tracing::warn!(
                error = %e,
                session_id = %self.session_id,
                "persist_user_message failed; continuing without durable write"
            ),
        }
    }

    /// Build the ranked-roster markdown block injected before the user message.
    /// Groups hits by kind so the LLM can see agents/skills/tools separately.
    fn format_roster(results: &[crate::adapters::rag::RagResult]) -> String {
        use crate::adapters::rag::RagKind;

        let mut agents = Vec::new();
        let mut skills = Vec::new();
        let mut tools = Vec::new();
        for r in results {
            let line = format!(
                "{}. {} (score: {:.2})\n   {}",
                // numbered later per-section
                "#",
                r.name,
                r.score,
                r.description.lines().next().unwrap_or(&r.description)
            );
            match r.kind {
                RagKind::Agent => agents.push(line),
                RagKind::Skill => skills.push(line),
                RagKind::Tool => tools.push(line),
            }
        }

        let mut out = String::new();
        let render = |label: &str, items: &[String], out: &mut String| {
            if items.is_empty() {
                return;
            }
            out.push_str(&format!("## Available {} (ranked by relevance)\n\n", label));
            for (i, item) in items.iter().enumerate() {
                // Replace the placeholder "#" prefix with the index.
                let numbered = item.replacen('#', &(i + 1).to_string(), 1);
                out.push_str(&numbered);
                out.push_str("\n\n");
            }
        };
        render("agents", &agents, &mut out);
        render("skills", &skills, &mut out);
        render("tools", &tools, &mut out);
        if out.is_empty() {
            out.push_str("## Roster\n\n_(no results — RAG registry empty?)_\n\n");
        }
        out
    }
}

/// Read `skills/orchestrator/SKILL.md` from cwd and return the body
/// (everything after the YAML frontmatter, if present). Returns `None` if
/// the file is missing or unreadable. Phase 4c.
#[cfg(feature = "qdrant")]
fn load_orchestrator_skill_body() -> Option<String> {
    let path = std::path::Path::new("skills/orchestrator/SKILL.md");
    let content = std::fs::read_to_string(path).ok()?;
    if content.starts_with("---") {
        // Strip frontmatter: skip the opening `---`, then everything up to
        // and including the next `---` line.
        let after_first = &content[3..];
        if let Some(end) = after_first.find("\n---") {
            // +4 to skip "\n---" and any trailing newline.
            let mut body_start = end + 4;
            let bytes = after_first.as_bytes();
            if body_start < bytes.len() && bytes[body_start] == b'\n' {
                body_start += 1;
            }
            return Some(after_first[body_start..].trim_start().to_string());
        }
    }
    Some(content)
}

/// Last-resort planner prompt used when `skills/orchestrator/SKILL.md` is
/// missing on disk. Phase 4c.
#[cfg(feature = "qdrant")]
const FALLBACK_PLANNER_PROMPT: &str = "You are the orchestrator for tengu-cluster.

For every user message you receive a ranked roster of available agents, skills, and tools (with similarity scores), and the user's message.

Output ONLY raw JSON. No prose, no markdown fences. Pick exactly one of:

  {\"kind\":\"direct\",\"response\":\"<your reply to the user>\"}

  {\"kind\":\"plan\",\"steps\":[{\"id\":\"s1\",\"agent\":\"<exact name from roster>\",\"goal\":\"<one-sentence instruction>\",\"depends_on\":[]}]}

Rules:
- NEVER invent an agent name. If no agent above score 0.6, return a Direct asking the user to clarify.
- Keep plans minimal — one step is enough most of the time.
- The harness rejects anything that is not valid JSON matching one of the two shapes above.
";

#[cfg(feature = "qdrant")]
#[async_trait]
impl Planner for RagPlanner {
    async fn plan(&self, user_message: &str) -> anyhow::Result<PlannerVerdict> {
        // Fix E (2026-05-09) — embed the user message ONCE per turn and
        // reuse the vector across every retrieval lane (registry / messages /
        // outputs) plus the persist write to `tengu_messages`. Pre Fix-E the
        // same text was embedded up to 4 times per plan() call: once each
        // for search_registry, cross_session_recall_block, persist write,
        // and session_output_recall_block. That's the source of the
        // "embedding API quota exceeded" the user hit. Fail-soft: if the
        // single embed call fails, every downstream lane sees `None` and
        // falls back to its own embed-then-fail-soft path (same as
        // pre Fix-E behaviour).
        let embed_vec: Option<Vec<f32>> = match self.rag().await {
            Ok(rag) => match rag.embedder().embed(user_message).await {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "plan: embedder failed; RAG lanes will fail-soft this turn"
                    );
                    None
                }
            },
            Err(_) => None,
        };
        let embed_slice = embed_vec.as_deref();

        let hits = match (self.rag().await, embed_slice) {
            (Ok(rag), Some(vec)) => rag
                .search_registry_with_vec(vec, self.top_k)
                .await
                .unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "rag search_registry failed; falling back to empty roster");
                    Vec::new()
                }),
            (Ok(_), None) => {
                // Embedder failed above. Don't double-warn here — that path
                // already logged the error.
                Vec::new()
            }
            (Err(e), _) => {
                tracing::warn!(error = %e, "RagStore unavailable; falling back to empty roster");
                Vec::new()
            }
        };

        // Phase 6.1 (lite + full) — surface the planner's input roster on
        // both the tracing channel (always) and the OrchestratorEvent bus
        // (when one is wired into this planner).
        emit_rag_query("plan", user_message, &hits, self.bus.as_ref());

        // Phase 6.4 (full, read-back) — semantic recall of prior user
        // messages from `tengu_messages` BEFORE we persist the current
        // one, so the just-written message can never appear in its own
        // recall block by construction. Off when `cross_session_msg_top_k`
        // is 0 (default). Fail-soft.
        let cross_session_block = self
            .cross_session_recall_block(user_message, embed_slice)
            .await;

        // Phase 6.4 (full, write) — durable persist of user message AFTER
        // the read above. Fail-soft: if embedder or Qdrant is unavailable
        // we still want to plan and respond (in-memory buffer covers this
        // turn for "Recent user messages this session" injection).
        self.persist_user_message(user_message, embed_slice).await;

        // Phase 6.4 (lite) — append current message to per-instance
        // history buffer, then format the last N for prompt injection.
        let history_block = self.update_and_format_history(user_message).await;

        // Fix A (2026-05-09) — within-session output recall on the normal
        // plan() path. Without this, prior step outputs are only readable
        // on replan; follow-up questions like "was the molecule project
        // created?" return "I have no record of that step." Off by default
        // (`within_session_output_top_k = 0`) so existing users see no
        // behaviour change.
        let session_recall_block = self
            .session_output_recall_block(user_message, embed_slice)
            .await;

        let roster = Self::format_roster(&hits);
        let combined = format!(
            "{}{}{}{}\n## User message (current turn)\n\n{}",
            roster, cross_session_block, history_block, session_recall_block, user_message
        );

        // Per-layer breakdown captured BEFORE the LLM call so the metric
        // record represents what the planner actually sent (the prompt is
        // immutable from here on). System prompt is a fifth layer — not
        // part of `combined` but still part of what the LLM sees.
        let layers = vec![
            crate::adapters::metrics::MetricsLayer::from_text("system", &self.system_prompt),
            crate::adapters::metrics::MetricsLayer::from_text("roster", &roster),
            crate::adapters::metrics::MetricsLayer::from_text(
                "cross_session",
                &cross_session_block,
            ),
            crate::adapters::metrics::MetricsLayer::from_text("history", &history_block),
            crate::adapters::metrics::MetricsLayer::from_text(
                "session_recall",
                &session_recall_block,
            ),
            crate::adapters::metrics::MetricsLayer::from_text("user_message", user_message),
        ];

        let (raw, telemetry) = self
            .chat
            .run_orchestrator_turn_with_system_metered(
                &self.orchestrator_agent,
                &self.system_prompt,
                &combined,
            )
            .await?;
        emit_planner_metrics(
            "plan",
            &self.session_id,
            &self.orchestrator_agent,
            &combined,
            &self.system_prompt,
            layers,
            &telemetry,
            &raw,
        );
        parse_verdict(&raw)
    }

    async fn replan(
        &self,
        user_message: &str,
        prior_plan: &Plan,
        failed_step_id: &str,
        error: &str,
    ) -> anyhow::Result<PlannerVerdict> {
        let hits = match self.rag().await {
            Ok(rag) => rag
                .search_registry(user_message, self.top_k)
                .await
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        emit_rag_query("replan", user_message, &hits, self.bus.as_ref());

        // Phase 6.5 — cross-plan recall. Pull the top-K most similar
        // step outputs / file chunks from `tengu_outputs` so the planner
        // sees relevant prior work before deciding the new plan. Failure
        // is non-fatal — we still replan with whatever roster we have.
        let recall_k = self.memory_config.cross_plan_top_k;
        let recall_hits: Vec<crate::adapters::rag::RagResult> = match self.rag().await {
            Ok(rag) if recall_k > 0 => {
                let q = format!("{} {} {}", user_message, failed_step_id, error);
                rag.search_memory(&q, recall_k).await.unwrap_or_default()
            }
            _ => Vec::new(),
        };
        let recall_block = if recall_hits.is_empty() {
            String::new()
        } else {
            let mut s = String::from("\n## Relevant prior step outputs (cross-plan recall)\n\n");
            for (i, h) in recall_hits.iter().enumerate() {
                let snippet: String = h.description.chars().take(400).collect();
                s.push_str(&format!(
                    "{}. score={:.2}  {}\n   {}\n\n",
                    i + 1,
                    h.score,
                    h.name,
                    snippet
                ));
            }
            s
        };

        // Phase 6.4 (lite) — recent dialogue context for replan too.
        // Don't append again (same user_message already pushed by plan()
        // earlier in this turn cycle); just read the last N for injection.
        let history_block = self.format_history_only().await;

        // Phase 6.4 (full, read-back) — semantic recall over `tengu_messages`.
        // Same shape as the plan() call. Independent of the cross-plan
        // (`tengu_outputs`) recall above; both can appear in the prompt.
        // Replan doesn't pre-cache an embedding for the user message
        // (it has its own composite-query embed for cross-plan recall),
        // so `embed_vec = None` falls through to the str-input path.
        let cross_session_block = self.cross_session_recall_block(user_message, None).await;

        let roster = Self::format_roster(&hits);
        let prior_plan_text = serde_json::to_string_pretty(prior_plan)?;
        let failure_block = format!(
            "\n## A previous plan failed\n\n\
             Failed step: {}\nError after retries: {}\n\n\
             Prior plan steps:\n{}\n\n\
             Produce a new plan that avoids this failure, or respond directly if recovery is not possible.\n\n",
            failed_step_id, error, prior_plan_text,
        );
        let context = format!(
            "{}{}{}{}{}## Original user message\n\n{}",
            roster,
            cross_session_block,
            history_block,
            recall_block,
            failure_block,
            user_message,
        );

        let layers = vec![
            crate::adapters::metrics::MetricsLayer::from_text("system", &self.system_prompt),
            crate::adapters::metrics::MetricsLayer::from_text("roster", &roster),
            crate::adapters::metrics::MetricsLayer::from_text(
                "cross_session",
                &cross_session_block,
            ),
            crate::adapters::metrics::MetricsLayer::from_text("history", &history_block),
            crate::adapters::metrics::MetricsLayer::from_text("recall", &recall_block),
            crate::adapters::metrics::MetricsLayer::from_text("failure", &failure_block),
            crate::adapters::metrics::MetricsLayer::from_text("user_message", user_message),
        ];

        let (raw, telemetry) = self
            .chat
            .run_orchestrator_turn_with_system_metered(
                &self.orchestrator_agent,
                &self.system_prompt,
                &context,
            )
            .await?;
        emit_planner_metrics(
            "replan",
            &self.session_id,
            &self.orchestrator_agent,
            &context,
            &self.system_prompt,
            layers,
            &telemetry,
            &raw,
        );
        parse_verdict(&raw)
    }
}

/// Build a `MetricsRecord` from the planner's per-layer breakdown +
/// engine telemetry and emit it via the global metrics sink. Pure side-effect
/// — never returns an error and never blocks. Phase metrics — landed
/// alongside the metrics module.
#[cfg(feature = "qdrant")]
#[allow(clippy::too_many_arguments)]
fn emit_planner_metrics(
    phase: &'static str,
    session_id: &str,
    orchestrator_agent: &str,
    combined_prompt: &str,
    system_prompt: &str,
    layers: Vec<crate::adapters::metrics::MetricsLayer>,
    telemetry: &crate::adapters::orchestrator::wiring::TurnTelemetry,
    raw_response: &str,
) {
    let prompt_chars = (combined_prompt.chars().count() + system_prompt.chars().count()) as u32;
    let prompt_bytes = (combined_prompt.len() + system_prompt.len()) as u32;
    let total_tokens = telemetry
        .prompt_tokens
        .saturating_add(telemetry.completion_tokens);
    let agent_label = if phase == "plan" {
        format!("planner/{}", orchestrator_agent)
    } else {
        format!("replanner/{}", orchestrator_agent)
    };
    crate::adapters::metrics::record(crate::adapters::metrics::MetricsRecord {
        ts_unix: crate::adapters::metrics::now_unix(),
        session_id: session_id.to_string(),
        kind: crate::adapters::metrics::MetricsKind::Planner,
        agent: agent_label,
        model: telemetry.model.clone(),
        prompt_tokens: telemetry.prompt_tokens,
        completion_tokens: telemetry.completion_tokens,
        total_tokens,
        prompt_chars,
        prompt_bytes,
        response_chars: telemetry.response_chars.max(raw_response.chars().count() as u32),
        latency_ms: telemetry.latency_ms,
        layers,
        step_id: None,
    });
}

/// Phase 6.4 (lite) — render the planner's per-session history buffer as a
/// markdown "recent dialogue" block. Shows the last `limit` user messages.
/// Returns an empty string when there's only one entry (just the current
/// turn — no prior context worth showing).
#[cfg(feature = "qdrant")]
fn format_history(hist: &[String], limit: usize) -> String {
    if hist.len() <= 1 {
        return String::new();
    }
    let start = hist.len().saturating_sub(limit + 1);
    // Skip the LAST entry (the current message) — it's already shown to the
    // model under "## User message (current turn)" in the prompt.
    let prior = &hist[start..hist.len().saturating_sub(1)];
    if prior.is_empty() {
        return String::new();
    }
    let mut s = String::from("\n## Recent user messages this session\n\n");
    for (i, msg) in prior.iter().enumerate() {
        // Truncate very long messages so the planner prompt stays bounded.
        let snippet: String = msg.chars().take(400).collect();
        s.push_str(&format!("{}. {}\n", i + 1, snippet));
        if msg.chars().count() > 400 {
            s.push_str("   …(truncated)\n");
        }
    }
    s
}

/// Phase 6.1 (lite + full) — emit one info-level tracing line per planner
/// call AND fire `OrchestratorEvent::RagQueried` on the bus if one was
/// wired into this planner. The tracing line and the bus event carry the
/// same payload (top-10 hits with kind/name/score), so subscribers can
/// pick whichever transport suits them.
///
/// Bus send failure is non-fatal — `broadcast::Sender::send` returns
/// `Err` only when there are no live receivers, which is the steady
/// state on cold-start before the channel adapter subscribes; we silently
/// ignore that case rather than spamming warnings.
#[cfg(feature = "qdrant")]
fn emit_rag_query(
    phase: &'static str,
    query: &str,
    hits: &[crate::adapters::rag::RagResult],
    bus: Option<&crate::adapters::orchestrator::events::EventBus>,
) {
    // Tracing — same shape as the lite version, kept for `RUST_LOG=tengu=info`.
    if hits.is_empty() {
        tracing::info!(phase, query, "rag query returned 0 hits");
    } else {
        let summary: Vec<String> = hits
            .iter()
            .take(10)
            .map(|h| format!("{}:{}={:.2}", h.kind.as_str(), h.name, h.score))
            .collect();
        tracing::info!(
            phase,
            query,
            hits = %summary.join(", "),
            "rag query returned {} hits",
            hits.len()
        );
    }

    // Structured event — only when a bus is wired (the static-mode path
    // and the standalone unit tests pass `None`).
    if let Some(bus) = bus {
        let payload_hits: Vec<crate::adapters::orchestrator::events::RagQueriedHit> = hits
            .iter()
            .take(10)
            .map(|h| crate::adapters::orchestrator::events::RagQueriedHit {
                kind: h.kind.as_str().to_string(),
                name: h.name.clone(),
                score: h.score,
            })
            .collect();
        let _ = bus.send(crate::adapters::orchestrator::events::OrchestratorEvent::RagQueried {
            phase,
            query: query.to_string(),
            hits: payload_hits,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::orchestrator::plan::StepId;

    #[test]
    fn parses_direct() {
        let raw = r#"{"kind": "direct", "response": "hi"}"#;
        match parse_verdict(raw).unwrap() {
            PlannerVerdict::Direct { response } => assert_eq!(response, "hi"),
            _ => panic!(),
        }
    }

    #[test]
    fn parses_plan() {
        let raw = r#"{"kind": "plan", "steps": [{"id": "s1", "agent": "x", "goal": "g", "depends_on": []}]}"#;
        match parse_verdict(raw).unwrap() {
            PlannerVerdict::Plan { plan } => assert_eq!(plan.steps[0].id, StepId::new("s1")),
            _ => panic!(),
        }
    }

    #[test]
    fn strips_markdown_fences() {
        let raw = "```json\n{\"kind\": \"direct\", \"response\": \"hi\"}\n```";
        assert!(parse_verdict(raw).is_ok());
    }

    /// Phase 7.4 prose-fallback — non-empty malformed input is wrapped as a
    /// `Direct { response }` verdict rather than returning `Err`. Pre-7.4 this
    /// test asserted `is_err()` on `"{"`; that contract changed when the
    /// fallback landed (`parse_verdict` Path 4) so engines like Claude Code
    /// that emit conversational prose stop hard-erroring the whole turn.
    /// Truly empty input is the ONLY remaining error case.
    #[test]
    fn rejects_only_empty_input() {
        // Empty input still errors — there's nothing to wrap.
        assert!(parse_verdict("").is_err());
        assert!(parse_verdict("   ").is_err());

        // Malformed-but-non-empty input gets wrapped as Direct (prose fallback).
        match parse_verdict("{").unwrap() {
            PlannerVerdict::Direct { response } => assert_eq!(response, "{"),
            _ => panic!("expected Direct fallback wrapping the raw text"),
        }
        match parse_verdict("Sure, I'll do that.").unwrap() {
            PlannerVerdict::Direct { response } => {
                assert_eq!(response, "Sure, I'll do that.")
            }
            _ => panic!("expected Direct fallback wrapping the raw text"),
        }
    }

    #[test]
    fn extracts_json_from_prose() {
        let raw = r#"Here's my plan: {"kind": "direct", "response": "hi there"}. Let me know if that works!"#;
        match parse_verdict(raw).unwrap() {
            PlannerVerdict::Direct { response } => assert_eq!(response, "hi there"),
            _ => panic!("expected direct"),
        }
    }

    #[test]
    fn extracts_json_with_braces_in_strings() {
        // String literal contains { and } — must not unbalance the extractor.
        let raw =
            r#"Sure: {"kind": "direct", "response": "the answer has a { brace in it"}. Done."#;
        let v = parse_verdict(raw).unwrap();
        match v {
            PlannerVerdict::Direct { response } => {
                assert!(response.contains("brace"));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn prefers_raw_json_over_embedded() {
        // Raw JSON should parse without invoking the prose extractor.
        let raw = r#"{"kind": "direct", "response": "{\"embedded\": true}"}"#;
        let v = parse_verdict(raw).unwrap();
        match v {
            PlannerVerdict::Direct { response } => assert_eq!(response, r#"{"embedded": true}"#),
            _ => panic!(),
        }
    }
}
