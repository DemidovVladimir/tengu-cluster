//! Planner — runs the orchestrator agent's LLM call, returns Plan or direct response.

use async_trait::async_trait;

use crate::domain::plan::Plan;
use crate::ports::orchestration::{OrchestratorChatPort, Planner, PlannerVerdict};

use std::sync::Arc;

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
        let suffix = if trimmed.chars().count() > 300 {
            "…"
        } else {
            ""
        };
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
// Loads `TENGU_PLANNER_REGISTRY.md` per turn and prepends the full roster
// to the user message; uses `parse_verdict` (free fn above) for tolerant
// JSON parsing of the LLM's plan output.
// =====================================================================

#[derive(Debug, Clone)]
struct RegistryHit {
    kind: String,
    name: String,
    score: f32,
}

pub struct RagPlanner {
    orchestrator_agent: String,
    chat: Arc<dyn OrchestratorChatPort>,
    memory_config: crate::config::MemoryConfig,
    /// Routable subagents (`[agents.*]` blocks with a `description`),
    /// rendered into `TENGU_PLANNER_REGISTRY.md` every planner turn.
    agents: Vec<(String, crate::config::AgentConfig)>,
    workspace: std::path::PathBuf,
    /// Phase 4c — body of `skills/orchestrator/SKILL.md` (frontmatter
    /// stripped) used as the planner system prompt. Falls back to a
    /// hardcoded minimal instruction if the file is missing.
    system_prompt: String,
    /// Phase 6.4 (lite) — in-memory ring buffer of recent user messages
    /// for THIS RagPlanner instance. Survives within one channel session,
    /// lost on restart; mixes turns from concurrent Telegram chats sharing
    /// one RagPlanner. Used as the source of the "## Recent user messages
    /// this session" prompt block. With `postgres_memory`, the current user
    /// message is also written to Postgres `agentic_memory`.
    session_history: tokio::sync::Mutex<Vec<String>>,
    /// id stamped on every durable memory write. Resolved once by
    /// `channel_runtime::build_orchestrator` and shared with
    /// `SubprocessRunner` so planner and subagent writes line up.
    session_id: String,
    /// Phase 6.1 (full) — optional event bus for emitting
    /// `OrchestratorEvent::RagQueried` on every `plan()`/`replan()`.
    /// `None` means structured events are silently dropped (the
    /// `tracing::info!` line still fires regardless).
    bus: Option<crate::adapters::orchestrator::events::EventBus>,
    /// `Config.mcp_servers` — enumerated (live `tools/list`, fail-soft per
    /// server) once per planner instance so the registry's TOOLS section
    /// lists `<server>.<tool>` entries alongside the core tools.
    mcp_servers: Vec<crate::config::McpServerConfig>,
    /// Lazily-filled cache of the MCP enumeration above. Filled on the
    /// first `plan()`/`replan()`; restart `tengu chat` to pick up server
    /// changes (same rule as `[agents.*]` edits in the sandbox config).
    mcp_tools: tokio::sync::OnceCell<Vec<crate::domain::message::ToolDef>>,
}

impl RagPlanner {
    pub fn new(
        orchestrator_agent: String,
        chat: Arc<dyn OrchestratorChatPort>,
        memory_config: crate::config::MemoryConfig,
        agents: Vec<(String, crate::config::AgentConfig)>,
        mcp_servers: Vec<crate::config::McpServerConfig>,
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
            agents,
            workspace: std::env::current_dir().unwrap_or_default(),
            system_prompt,
            session_history: tokio::sync::Mutex::new(Vec::new()),
            session_id,
            bus,
            mcp_servers,
            mcp_tools: tokio::sync::OnceCell::new(),
        }
    }

    #[cfg(feature = "postgres_memory")]
    fn embedder(&self) -> Option<crate::adapters::outbound::memory::embedder::Embedder> {
        std::env::var("OPENROUTER_API_KEY").ok().map(|api_key| {
            crate::adapters::outbound::memory::embedder::Embedder::new(
                api_key,
                self.memory_config.embedding_model.clone(),
            )
        })
    }

    #[cfg(feature = "postgres_memory")]
    async fn embed_text(&self, text: &str) -> Option<Vec<f32>> {
        let Some(embedder) = self.embedder() else {
            return None;
        };
        match embedder.embed(text).await {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "planner embed failed; semantic memory lanes will fall back where possible"
                );
                None
            }
        }
    }

    async fn load_registry_block(&self) -> (String, Vec<RegistryHit>) {
        let mcp_tools = self
            .mcp_tools
            .get_or_init(|| async {
                let tools = crate::adapters::orchestrator::shared_files::enumerate_mcp_tools(
                    &self.mcp_servers,
                )
                .await;
                tracing::info!(
                    servers = self.mcp_servers.len(),
                    tools = tools.len(),
                    "planner registry: MCP tools enumerated"
                );
                tools
            })
            .await;
        match crate::adapters::orchestrator::shared_files::ensure_planner_registry(
            &self.workspace,
            &self.agents,
            mcp_tools,
        ) {
            Ok(snapshot) => {
                let hits = snapshot
                    .entries
                    .into_iter()
                    .map(|entry| RegistryHit {
                        kind: entry.kind,
                        name: entry.name,
                        score: 1.0,
                    })
                    .collect();
                (snapshot.prompt_block, hits)
            }
            Err(e) => {
                tracing::warn!(error = %e, "planner registry file unavailable");
                (
                    "\n## Planner registry\n\n_(registry file unavailable)_\n\n".to_string(),
                    Vec::new(),
                )
            }
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

    /// Phase 6.4 (full, read-back) — semantic recall over durable user messages.
    /// Returns a formatted prompt block of the top-K semantically-similar
    /// prior user messages, or an empty string when:
    /// - the config knob `memory.cross_session_msg_top_k` is 0 (default,
    ///   so existing users see no behaviour change),
    /// - the memory backend is unavailable,
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
        #[cfg(feature = "postgres_memory")]
        {
            let hits =
                match crate::adapters::outbound::tools::agentic_memory::recall_user_messages_with_vec(
                    user_message,
                    embed_vec,
                    k,
                    user_message,
                )
                .await
                {
                    Ok(h) => h,
                    Err(e) => {
                        tracing::debug!(error = %e, "skip cross_session_recall: agentic_memory unavailable");
                        Vec::new()
                    }
                };
            if hits.is_empty() {
                return String::new();
            }
            let mut s = String::from("\n## Cross-session message recall\n\n");
            s.push_str(
                "_(prior user messages from Postgres `agentic_memory`, excluding the current turn)_\n\n",
            );
            for (i, h) in hits.iter().enumerate() {
                let snippet: String = h.content.chars().take(400).collect();
                s.push_str(&format!(
                    "{}. score={:.2}  kind={}  {}\n",
                    i + 1,
                    h.score,
                    h.kind,
                    snippet
                ));
                if h.content.chars().count() > 400 {
                    s.push_str("   …(truncated)\n");
                }
            }
            s.push('\n');
            return s;
        }
        #[cfg(not(feature = "postgres_memory"))]
        {
            let _ = (user_message, embed_vec);
            String::new()
        }
    }

    /// Fix A (2026-05-09) — within-session output recall.
    ///
    /// Returns a formatted prompt block of the top-K semantically-similar
    /// step outputs from THIS session (filtered to `self.session_id`), or
    /// an empty string when:
    /// - the config knob `memory.within_session_output_top_k` is 0 (default),
    /// - Postgres `agentic_memory` is unavailable, or
    /// - no outputs in this session match.
    ///
    /// Pairs with Fix B's unified session_id wiring: the `run-agent` step
    /// summary capture stamps `AgentIpcInput.session_id`, which now equals
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
        #[cfg(feature = "postgres_memory")]
        {
            let hits =
                match crate::adapters::outbound::tools::agentic_memory::recall_step_outputs_for_session_with_vec(
                    &self.session_id,
                    user_message,
                    embed_vec,
                    k,
                )
                .await
                {
                    Ok(h) => h,
                    Err(e) => {
                        tracing::debug!(error = %e, "skip session_output_recall: agentic_memory unavailable");
                        Vec::new()
                    }
                };
            if hits.is_empty() {
                return String::new();
            }
            let mut s = String::from("\n## Recent step outputs (this session)\n\n");
            s.push_str("_(prior subagent results from Postgres `agentic_memory`, filtered to this `session_id`)_\n\n");
            for (i, h) in hits.iter().enumerate() {
                let snippet: String = h.content.chars().take(400).collect();
                let step = h.step_id.as_deref().unwrap_or(&h.id);
                s.push_str(&format!(
                    "{}. score={:.2}  kind={}  step={}\n   {}\n",
                    i + 1,
                    h.score,
                    h.kind,
                    step,
                    snippet
                ));
                if h.content.chars().count() > 400 {
                    s.push_str("   …(truncated)\n");
                }
            }
            s.push('\n');
            return s;
        }
        #[cfg(not(feature = "postgres_memory"))]
        {
            let _ = (user_message, embed_vec);
            String::new()
        }
    }

    /// Phase 6.4 (full) — durably persist a user message to the configured
    /// runtime memory backend.
    /// Called from `plan()` only (not `replan()`, which receives the same
    /// `user_message` in the same turn cycle — a second write would create
    /// a duplicate row). Fail-soft: any memory/backend error is logged and swallowed
    /// because the planner must still be able to plan + respond.
    ///
    /// `step_id` is `None` because this is a top-of-turn user message,
    /// not a step output. `created_at` is unix-seconds at write time.
    async fn persist_user_message(&self, user_message: &str, embed_vec: Option<&[f32]>) {
        let trimmed = user_message.trim();
        if trimmed.is_empty() {
            return;
        }
        #[cfg(feature = "postgres_memory")]
        {
            match crate::adapters::outbound::tools::agentic_memory::write_user_message_with_embedding(
                &self.session_id,
                trimmed,
                embed_vec,
            )
            .await
            {
                Ok(id) => tracing::debug!(
                    id = %id,
                    session_id = %self.session_id,
                    "persisted user message to agentic_memory"
                ),
                Err(e) => tracing::warn!(
                    error = %e,
                    session_id = %self.session_id,
                    "persist_user_message to agentic_memory failed; continuing without durable write"
                ),
            }
            return;
        }
        #[cfg(not(feature = "postgres_memory"))]
        {
            let _ = embed_vec;
        }
    }
}

/// Read `skills/orchestrator/SKILL.md` from cwd and return the body
/// (everything after the YAML frontmatter, if present). Returns `None` if
/// the file is missing or unreadable. Phase 4c.
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
const FALLBACK_PLANNER_PROMPT: &str = "You are the orchestrator for tengu-cluster.

For every user message you receive the available agents, skills, tools, and the user's message.

Output ONLY raw JSON. No prose, no markdown fences. Pick exactly one of:

  {\"kind\":\"direct\",\"response\":\"<your reply to the user>\"}

  {\"kind\":\"plan\",\"steps\":[{\"id\":\"s1\",\"agent\":\"<exact name from roster>\",\"goal\":\"<one-sentence instruction>\",\"depends_on\":[]}]}

Rules:
- NEVER invent an agent name. If no listed agent fits, return a Direct asking the user to clarify.
- Keep plans minimal — one step is enough most of the time.
- The harness rejects anything that is not valid JSON matching one of the two shapes above.
";

#[async_trait]
impl Planner for RagPlanner {
    fn session_id(&self) -> Option<String> {
        Some(self.session_id.clone())
    }

    async fn plan(&self, user_message: &str) -> anyhow::Result<PlannerVerdict> {
        // Embed the user message once per turn and reuse the vector across
        // Postgres memory recall/write lanes when `postgres_memory` is on.
        // Planner registry routing itself is file-backed and does not embed.
        #[cfg(feature = "postgres_memory")]
        let embed_vec: Option<Vec<f32>> = self.embed_text(user_message).await;
        #[cfg(not(feature = "postgres_memory"))]
        let embed_vec: Option<Vec<f32>> = None;
        let embed_slice = embed_vec.as_deref();

        let (registry_block, hits) = self.load_registry_block().await;

        // Phase 6.1 (lite + full) — surface the planner's input roster on
        // both the tracing channel (always) and the OrchestratorEvent bus
        // (when one is wired into this planner).
        emit_rag_query("plan", user_message, &hits, self.bus.as_ref());

        // Semantic recall of prior user messages from runtime memory BEFORE
        // we persist the current
        // one, so the just-written message can never appear in its own
        // recall block by construction. Off when `cross_session_msg_top_k`
        // is 0 (default). Fail-soft.
        let cross_session_block = self
            .cross_session_recall_block(user_message, embed_slice)
            .await;

        // Phase 6.4 (full, write) — durable persist of user message AFTER
        // the read above. Fail-soft: if embedder or Postgres is unavailable
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

        let combined = format!(
            "{}{}{}{}\n## User message (current turn)\n\n{}",
            registry_block, cross_session_block, history_block, session_recall_block, user_message
        );

        // Per-layer breakdown captured BEFORE the LLM call so the metric
        // record represents what the planner actually sent (the prompt is
        // immutable from here on). System prompt is a fifth layer — not
        // part of `combined` but still part of what the LLM sees.
        let layers = vec![
            crate::domain::metrics::MetricsLayer::from_text("system", &self.system_prompt),
            crate::domain::metrics::MetricsLayer::from_text("roster", &registry_block),
            crate::domain::metrics::MetricsLayer::from_text("cross_session", &cross_session_block),
            crate::domain::metrics::MetricsLayer::from_text("history", &history_block),
            crate::domain::metrics::MetricsLayer::from_text(
                "session_recall",
                &session_recall_block,
            ),
            crate::domain::metrics::MetricsLayer::from_text("user_message", user_message),
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
        let (registry_block, hits) = self.load_registry_block().await;
        emit_rag_query("replan", user_message, &hits, self.bus.as_ref());

        // Phase 6.5 — cross-plan recall. Pull the top-K most similar
        // step outputs so the planner sees relevant prior work before
        // deciding the new plan. Failure is non-fatal — we still replan
        // with whatever roster we have.
        let recall_k = self.memory_config.cross_plan_top_k;
        let recall_query = format!("{} {} {}", user_message, failed_step_id, error);
        let recall_block = if recall_k == 0 {
            String::new()
        } else {
            #[cfg(feature = "postgres_memory")]
            {
                let embed_vec = self.embed_text(&recall_query).await;
                let hits =
                    match crate::adapters::outbound::tools::agentic_memory::recall_step_outputs_with_vec(
                        &recall_query,
                        embed_vec.as_deref(),
                        recall_k,
                    )
                    .await
                    {
                        Ok(h) => h,
                        Err(e) => {
                            tracing::debug!(
                                error = %e,
                                "skip cross-plan recall: agentic_memory unavailable"
                            );
                            Vec::new()
                        }
                    };
                if hits.is_empty() {
                    String::new()
                } else {
                    let mut s =
                        String::from("\n## Relevant prior step outputs (cross-plan recall)\n\n");
                    s.push_str("_(prior subagent results from Postgres `agentic_memory`)_\n\n");
                    for (i, h) in hits.iter().enumerate() {
                        let snippet: String = h.content.chars().take(400).collect();
                        let step = h.step_id.as_deref().unwrap_or(&h.id);
                        s.push_str(&format!(
                            "{}. score={:.2}  kind={}  step={}\n   {}\n\n",
                            i + 1,
                            h.score,
                            h.kind,
                            step,
                            snippet
                        ));
                    }
                    s
                }
            }
            #[cfg(not(feature = "postgres_memory"))]
            {
                let _ = &recall_query;
                String::new()
            }
        };

        // Phase 6.4 (lite) — recent dialogue context for replan too.
        // Don't append again (same user_message already pushed by plan()
        // earlier in this turn cycle); just read the last N for injection.
        let history_block = self.format_history_only().await;

        // Semantic recall over runtime memory.
        // Same shape as the plan() call. Independent of the cross-plan
        // (step-output) recall above; both can appear in the prompt.
        // Replan doesn't pre-cache an embedding for the user message
        // (it has its own composite-query embed for cross-plan recall),
        // so `embed_vec = None` falls through to the str-input path.
        let cross_session_block = self.cross_session_recall_block(user_message, None).await;

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
            registry_block,
            cross_session_block,
            history_block,
            recall_block,
            failure_block,
            user_message,
        );

        let layers = vec![
            crate::domain::metrics::MetricsLayer::from_text("system", &self.system_prompt),
            crate::domain::metrics::MetricsLayer::from_text("roster", &registry_block),
            crate::domain::metrics::MetricsLayer::from_text("cross_session", &cross_session_block),
            crate::domain::metrics::MetricsLayer::from_text("history", &history_block),
            crate::domain::metrics::MetricsLayer::from_text("recall", &recall_block),
            crate::domain::metrics::MetricsLayer::from_text("failure", &failure_block),
            crate::domain::metrics::MetricsLayer::from_text("user_message", user_message),
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
#[allow(clippy::too_many_arguments)]
fn emit_planner_metrics(
    phase: &'static str,
    session_id: &str,
    orchestrator_agent: &str,
    combined_prompt: &str,
    system_prompt: &str,
    layers: Vec<crate::domain::metrics::MetricsLayer>,
    telemetry: &crate::ports::orchestration::TurnTelemetry,
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
    crate::application::metrics::record(crate::domain::metrics::MetricsRecord {
        ts_unix: crate::domain::metrics::now_unix(),
        session_id: session_id.to_string(),
        kind: crate::domain::metrics::MetricsKind::Planner,
        agent: agent_label,
        model: telemetry.model.clone(),
        prompt_tokens: telemetry.prompt_tokens,
        completion_tokens: telemetry.completion_tokens,
        total_tokens,
        prompt_chars,
        prompt_bytes,
        response_chars: telemetry
            .response_chars
            .max(raw_response.chars().count() as u32),
        latency_ms: telemetry.latency_ms,
        layers,
        step_id: None,
    });
}

/// Phase 6.4 (lite) — render the planner's per-session history buffer as a
/// markdown "recent dialogue" block. Shows the last `limit` user messages.
/// Returns an empty string when there's only one entry (just the current
/// turn — no prior context worth showing).
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
fn emit_rag_query(
    phase: &'static str,
    query: &str,
    hits: &[RegistryHit],
    bus: Option<&crate::adapters::orchestrator::events::EventBus>,
) {
    // Tracing — same shape as the lite version, kept for `RUST_LOG=tengu=info`.
    if hits.is_empty() {
        tracing::info!(phase, query, "rag query returned 0 hits");
    } else {
        let summary: Vec<String> = hits
            .iter()
            .take(10)
            .map(|h| format!("{}:{}={:.2}", h.kind, h.name, h.score))
            .collect();
        tracing::info!(
            phase,
            query,
            hits = %summary.join(", "),
            "rag query returned {} hits",
            hits.len()
        );
    }

    // Structured event — only when a bus is wired (the standalone unit
    // tests pass `None`).
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
        let _ = bus.send(
            crate::adapters::orchestrator::events::OrchestratorEvent::RagQueried {
                phase,
                query: query.to_string(),
                hits: payload_hits,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::plan::StepId;

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
