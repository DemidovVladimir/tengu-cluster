//! Planner — runs the orchestrator agent's LLM call, returns Plan or direct response.

use async_trait::async_trait;

use crate::config::AgentConfig;
use crate::domain::message::{Message, Role};
use crate::domain::plan::Plan;
use crate::ports::engine::{Engine, EngineContext};
use crate::ports::orchestration::{Planner, PlannerVerdict, TurnTelemetry};

use std::sync::Arc;

/// The planner's one LLM call: the `[orchestrator] agent`'s engine, the
/// planner prompt as system prompt, the assembled planner message as the
/// only message — and nothing else: no tools, no tool executor, no bridge,
/// no `[[mcp_servers]]`, no memory, no history. The planner holds an engine,
/// not a chat service, so "the planner has no hands" (doctrine 1) is
/// structural, and every surface (chat, Telegram, webhooks, eval, A2A) runs
/// the same call with the same telemetry.
#[derive(Clone)]
pub struct PlannerLlm {
    pub engine: Arc<dyn Engine>,
    /// `[agents.<orchestrator.agent>]`: its model, workspace and limits.
    pub agent: Arc<AgentConfig>,
}

impl PlannerLlm {
    pub async fn call(
        &self,
        system_prompt: &str,
        text: &str,
    ) -> anyhow::Result<(String, TurnTelemetry)> {
        let started = std::time::Instant::now();
        let limits = &self.agent.limits;
        let context = EngineContext {
            workspace: self.agent.workspace.clone(),
            system_prompt: Some(system_prompt.to_string()),
            bridge_tools: None,
            max_tool_rounds: Some(limits.max_tool_rounds),
            max_mcp_result_chars: Some(limits.max_mcp_result_chars),
            mcp_servers: Vec::new(),
        };
        let messages = [Message {
            role: Role::User,
            content: text.to_string(),
            tool_call_id: None,
            tool_calls: None,
        }];
        let resp = crate::application::chat::tool_loop::collect_engine_response(
            self.engine.as_ref(),
            &messages,
            &[],
            &context,
            None,
            &crate::application::chat::tool_loop::LoopOpts::from_limits(limits),
        )
        .await?;
        let telemetry = TurnTelemetry {
            prompt_tokens: resp.input_tokens_delta,
            cached_input_tokens: resp.cached_input_tokens_delta,
            completion_tokens: resp.output_tokens_delta,
            model: self.agent.model.clone(),
            latency_ms: started.elapsed().as_millis() as u64,
            response_chars: resp.text.chars().count() as u32,
        };
        Ok((resp.text, telemetry))
    }
}

/// Parse the orchestrator LLM's response into a [`PlannerVerdict`].
///
/// Tolerates four common sloppiness patterns from LLMs:
///   1. Raw JSON — parses directly.
///   2. JSON wrapped in markdown fences (```json ... ```).
///   3. JSON embedded in prose ("Here's my plan: {...}. Let me know if…").
///   4. Pure prose: some engines (notably the Claude Code CLI) answer
///      conversationally despite a JSON-only prompt. With no balanced
///      `{...}` substring at all, the whole text becomes a
///      `Direct { response }` verdict, so the user still gets the reply.
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

    // Path 3: JSON embedded in prose. Take the first balanced {...}
    // substring (considers brace nesting + string literals with escapes
    // so "{" inside a string doesn't unbalance the tracker).
    if let Some(json) = extract_balanced_json_object(trimmed) {
        if let Ok(v) = serde_json::from_str::<PlannerVerdict>(&json) {
            return Ok(v);
        }
    }

    // Path 4: pure-prose fallback. Some engines (Claude Code
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
// RegistryPlanner — the only Planner implementation.
//
// Loads `TENGU_PLANNER_REGISTRY.md` per turn and prepends the full roster
// to the user message; uses `parse_verdict` (free fn above) for tolerant
// JSON parsing of the LLM's plan output.
// =====================================================================

pub struct RegistryPlanner {
    orchestrator_agent: String,
    llm: PlannerLlm,
    memory_config: crate::config::MemoryConfig,
    /// Routable subagents (`[agents.*]` blocks with a `description`),
    /// rendered into `TENGU_PLANNER_REGISTRY.md` every planner turn.
    agents: Vec<(String, crate::config::AgentConfig)>,
    workspace: std::path::PathBuf,
    /// Body of `skills/orchestrator/SKILL.md` (frontmatter
    /// stripped) used as the planner system prompt. Falls back to a
    /// hardcoded minimal instruction if the file is missing.
    system_prompt: String,
    /// In-memory ring buffer of recent user messages
    /// for THIS RegistryPlanner instance. Survives within one channel session,
    /// lost on restart; mixes turns from concurrent Telegram chats sharing
    /// one RegistryPlanner. Used as the source of the "## Recent user messages
    /// this session" prompt block. With `postgres_memory`, the current user
    /// message is also written to Postgres `agentic_memory`.
    session_history: tokio::sync::Mutex<Vec<String>>,
    /// id stamped on every durable memory write. Resolved once by
    /// `crate::bootstrap::orchestrator::build_orchestrator` and shared with
    /// `SubprocessRunner` so planner and subagent writes line up.
    session_id: String,
    /// Source of the registry's TOOLS section: the tool catalog plus every
    /// `[[mcp_servers]]` tool (`<server>__<tool>`, live `tools/list`).
    tool_directory: Arc<dyn crate::ports::tool::ToolDirectory>,
    /// Lazily-filled cache of `tool_directory`. Filled on the first
    /// `plan()`/`replan()`; restart `tengu chat` to pick up server changes
    /// (same rule as `[agents.*]` edits in the sandbox config).
    tool_defs: tokio::sync::OnceCell<Vec<crate::domain::message::ToolDef>>,
    /// Durable runtime memory for the recall lanes + user-message writes.
    /// `None` (no `postgres_memory`) = those prompt blocks stay empty.
    recall: Option<Arc<dyn crate::ports::memory::RecallStore>>,
    /// Embeds the user message once per turn for `recall`. Only used when
    /// `recall` is set.
    embedder: Option<Arc<dyn crate::ports::memory::Embedding>>,
}

impl RegistryPlanner {
    pub fn new(
        orchestrator_agent: String,
        llm: PlannerLlm,
        memory_config: crate::config::MemoryConfig,
        agents: Vec<(String, crate::config::AgentConfig)>,
        tool_directory: Arc<dyn crate::ports::tool::ToolDirectory>,
        recall: Option<Arc<dyn crate::ports::memory::RecallStore>>,
        embedder: Option<Arc<dyn crate::ports::memory::Embedding>>,
        // Resolved once by `build_orchestrator` and shared with
        // `SubprocessRunner`: the parent's recall query and the child's
        // `compress_and_store` write use the same key.
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
            llm,
            memory_config,
            agents,
            workspace: std::env::current_dir().unwrap_or_default(),
            system_prompt,
            session_history: tokio::sync::Mutex::new(Vec::new()),
            session_id,
            tool_directory,
            tool_defs: tokio::sync::OnceCell::new(),
            recall,
            embedder,
        }
    }

    /// Embed `text` for the recall lanes. `None` when there is no recall
    /// store or embedder, or the call fails (lanes fall back to text).
    async fn embed_text(&self, text: &str) -> Option<Vec<f32>> {
        self.recall.as_ref()?;
        let embedder = self.embedder.as_ref()?;
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

    async fn load_registry_block(&self) -> String {
        let tool_defs = self
            .tool_defs
            .get_or_init(|| async {
                let tools = self.tool_directory.all_tool_defs().await;
                tracing::info!(tools = tools.len(), "planner registry: tools enumerated");
                tools
            })
            .await;
        match crate::application::orchestrator::shared_files::ensure_planner_registry(
            &self.workspace,
            &self.agents,
            tool_defs,
        ) {
            Ok(block) => block,
            Err(e) => {
                tracing::warn!(error = %e, "planner registry file unavailable");
                "\n## Planner registry\n\n_(registry file unavailable)_\n\n".to_string()
            }
        }
    }

    /// Push the current user message into the session
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

    /// Semantic recall over durable user messages.
    /// Returns a formatted prompt block of the top-K semantically-similar
    /// prior user messages, or an empty string when:
    /// - the config knob `memory.cross_session_msg_top_k` is 0 (the default),
    /// - the memory backend is unavailable,
    /// - the search returns nothing (collection empty / cold start), or
    /// - every hit is a near-duplicate of the current message (filtered out
    ///   so the LLM doesn't see "the user already asked this").
    ///
    /// Called from BOTH `plan()` and `replan()` BEFORE
    /// `persist_user_message()` so the just-written current message can
    /// never appear in its own recall block. (The semantic embedder will
    /// happily return a self-match at score ~1.0 if we read after writing.)
    /// `embed_vec` is the user-message vector `plan()` already computed;
    /// with `None` (replan, tests, a failed upstream embed) it embeds here.
    async fn cross_session_recall_block(
        &self,
        user_message: &str,
        embed_vec: Option<&[f32]>,
    ) -> String {
        let k = self.memory_config.cross_session_msg_top_k;
        if k == 0 {
            return String::new();
        }
        if let Some(recall) = &self.recall {
            let hits = match recall
                .recall_user_messages(user_message, embed_vec, k, user_message)
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
                s.push_str(&format!(
                    "{}. score={:.2}  kind={}\n{}\n",
                    i + 1,
                    h.score,
                    h.kind,
                    recalled(h)
                ));
            }
            s.push('\n');
            return s;
        }
        String::new()
    }

    /// Within-session output recall.
    ///
    /// Returns a formatted prompt block of the top-K semantically-similar
    /// step outputs from THIS session (filtered to `self.session_id`), or
    /// an empty string when:
    /// - the config knob `memory.within_session_output_top_k` is 0 (default),
    /// - Postgres `agentic_memory` is unavailable, or
    /// - no outputs in this session match.
    ///
    /// The `run-agent` step summary capture stamps `AgentIpcInput.session_id`,
    /// which is the planner's own `session_id` (one id per surface session),
    /// so the filter matches this session's steps.
    async fn session_output_recall_block(
        &self,
        user_message: &str,
        embed_vec: Option<&[f32]>,
    ) -> String {
        let k = self.memory_config.within_session_output_top_k;
        if k == 0 {
            return String::new();
        }
        if let Some(recall) = &self.recall {
            let hits = match recall
                .recall_step_outputs_for_session(&self.session_id, user_message, embed_vec, k)
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
                let step = h.step_id.as_deref().unwrap_or(&h.id);
                s.push_str(&format!(
                    "{}. score={:.2}  kind={}  step={}\n{}\n",
                    i + 1,
                    h.score,
                    h.kind,
                    step,
                    recalled(h)
                ));
            }
            s.push('\n');
            return s;
        }
        String::new()
    }

    /// Durably persist a user message to the configured
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
        if let Some(recall) = &self.recall {
            match recall
                .write_user_message(&self.session_id, trimmed, embed_vec)
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
        }
    }
}

/// Read `skills/orchestrator/SKILL.md` from cwd and return the body
/// (everything after the YAML frontmatter, if present). Returns `None` if
/// the file is missing or unreadable.
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
/// missing on disk.
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
impl Planner for RegistryPlanner {
    async fn plan(&self, user_message: &str) -> anyhow::Result<PlannerVerdict> {
        // Embed the user message once per turn and reuse the vector across
        // Postgres memory recall/write lanes when `postgres_memory` is on.
        // Planner registry routing itself is file-backed and does not embed.
        let embed_vec: Option<Vec<f32>> = self.embed_text(user_message).await;
        let embed_slice = embed_vec.as_deref();

        let registry_block = self.load_registry_block().await;

        // Semantic recall of prior user messages from runtime memory BEFORE
        // we persist the current
        // one, so the just-written message can never appear in its own
        // recall block by construction. Off when `cross_session_msg_top_k`
        // is 0 (default). Fail-soft.
        let cross_session_block = self
            .cross_session_recall_block(user_message, embed_slice)
            .await;

        // Durable persist of the user message AFTER
        // the read above. Fail-soft: if embedder or Postgres is unavailable
        // we still want to plan and respond (in-memory buffer covers this
        // turn for "Recent user messages this session" injection).
        self.persist_user_message(user_message, embed_slice).await;

        // Append the current message to the per-instance
        // history buffer, then format the last N for prompt injection.
        let history_block = self.update_and_format_history(user_message).await;

        // Within-session output recall on the plan() path, so a follow-up
        // ("was the report created?") sees this session's step outputs.
        // Off by default (`within_session_output_top_k = 0`).
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

        let (raw, telemetry) = self.llm.call(&self.system_prompt, &combined).await?;
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
        let registry_block = self.load_registry_block().await;

        // Cross-plan recall. Pull the top-K most similar
        // step outputs so the planner sees relevant prior work before
        // deciding the new plan. Failure is non-fatal — we still replan
        // with whatever roster we have.
        let recall_k = self.memory_config.cross_plan_top_k;
        let recall_query = format!("{} {} {}", user_message, failed_step_id, error);
        let recall_block = if recall_k == 0 {
            String::new()
        } else if let Some(recall) = &self.recall {
            {
                let embed_vec = self.embed_text(&recall_query).await;
                let hits = match recall
                    .recall_step_outputs(&recall_query, embed_vec.as_deref(), recall_k)
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
                        let step = h.step_id.as_deref().unwrap_or(&h.id);
                        s.push_str(&format!(
                            "{}. score={:.2}  kind={}  step={}\n{}\n\n",
                            i + 1,
                            h.score,
                            h.kind,
                            step,
                            recalled(h)
                        ));
                    }
                    s
                }
            }
        } else {
            String::new()
        };

        // Recent dialogue context for replan too.
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

        let prior_plan_text = serde_json::to_string(prior_plan)?;
        // The error carries the step's partial output: fenced.
        let failure_block = format!(
            "\n## A previous plan failed\n\n\
             Failed step: {}\nError after retries:\n{}\n\n\
             Prior plan steps:\n{}\n\n\
             Produce a new plan that avoids this failure, or respond directly if recovery is not possible.\n\n",
            failed_step_id,
            crate::domain::untrusted::fence(&format!("step-error:{failed_step_id}"), error),
            prior_plan_text,
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

        let (raw, telemetry) = self.llm.call(&self.system_prompt, &context).await?;
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
    telemetry: &TurnTelemetry,
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
        cached_input_tokens: telemetry.cached_input_tokens,
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

/// One recalled Open Brain row in a planner recall block: its first 400
/// chars in a `domain::untrusted` fence, origin `memory:<kind>`.
fn recalled(h: &crate::domain::memory::RecallHit) -> String {
    let mut text: String = h.content.chars().take(400).collect();
    if h.content.chars().count() > 400 {
        text.push_str(" …(truncated)");
    }
    crate::domain::untrusted::fence(&format!("memory:{}", h.kind), &text)
}

/// Render the planner's per-session history buffer as a
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The planner's call has no hands on any surface: no tools, no bridge,
    /// no `[[mcp_servers]]`; the planner prompt is the system prompt, the
    /// assembled message the only message, and the engine's usage is the
    /// telemetry (webhook and eval planner turns used to drop both).
    #[tokio::test]
    async fn planner_call_has_no_hands_and_reports_usage() {
        let engine = testkit::TextEngine::new("{\"kind\":\"direct\",\"response\":\"hi\"}", 0);
        let llm = testkit::llm(engine.clone());
        let (text, telemetry) = llm.call("PLANNER PROMPT", "the message").await.unwrap();
        assert!(text.contains("\"direct\""));
        assert_eq!(
            (telemetry.prompt_tokens, telemetry.completion_tokens),
            (100, 7)
        );
        assert_eq!(telemetry.model, "stub/planner");
        assert!(engine.called());
        let seen = engine.calls.lock().unwrap().clone();
        assert_eq!(seen.len(), 1);
        let s = &seen[0];
        assert_eq!((s.tools, s.bridge_tools, s.mcp_servers), (0, None, 0));
        assert_eq!(s.system_prompt.as_deref(), Some("PLANNER PROMPT"));
        assert_eq!(s.messages.len(), 1);
        assert_eq!(s.messages[0].content, "the message");
    }
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

    /// Non-empty malformed input becomes a `Direct { response }` verdict
    /// (`parse_verdict` Path 4); only empty input is an error.
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

/// A stub planner engine for tests on any surface.
#[cfg(test)]
pub(crate) mod testkit {
    use super::PlannerLlm;
    use crate::domain::message::{Message, ModelInfo, StreamEvent, ToolDef};
    use crate::ports::engine::{Engine, EngineContext};
    use std::sync::{Arc, Mutex};

    /// What one call was given.
    #[derive(Debug, Clone)]
    pub(crate) struct Seen {
        pub tools: usize,
        pub bridge_tools: Option<usize>,
        pub mcp_servers: usize,
        pub system_prompt: Option<String>,
        pub messages: Vec<Message>,
    }

    /// Answers every call with `text` (usage 100 in / 7 out) after
    /// `delay_ms`, recording what it was given.
    pub(crate) struct TextEngine {
        pub text: String,
        pub delay_ms: u64,
        pub calls: Mutex<Vec<Seen>>,
    }

    impl TextEngine {
        pub(crate) fn new(text: &str, delay_ms: u64) -> Arc<Self> {
            Arc::new(Self {
                text: text.to_string(),
                delay_ms,
                calls: Mutex::new(Vec::new()),
            })
        }

        /// Whether a call was entered (before its delay).
        pub(crate) fn called(&self) -> bool {
            !self.calls.lock().unwrap().is_empty()
        }
    }

    #[async_trait::async_trait]
    impl Engine for TextEngine {
        fn id(&self) -> &str {
            "text-stub"
        }
        fn context_window(&self) -> usize {
            100_000
        }
        fn supports_tool_use(&self) -> bool {
            true
        }
        fn manages_own_workspace(&self) -> bool {
            true
        }
        fn available_models(&self) -> Vec<ModelInfo> {
            Vec::new()
        }
        async fn run(
            &self,
            messages: &[Message],
            tools: &[ToolDef],
            context: &EngineContext,
        ) -> anyhow::Result<std::pin::Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send>>>
        {
            self.calls.lock().unwrap().push(Seen {
                tools: tools.len(),
                bridge_tools: context.bridge_tools.as_ref().map(Vec::len),
                mcp_servers: context.mcp_servers.len(),
                system_prompt: context.system_prompt.clone(),
                messages: messages.to_vec(),
            });
            tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
            Ok(Box::pin(futures::stream::iter(vec![
                StreamEvent::TextDelta {
                    text: self.text.clone(),
                },
                StreamEvent::Usage {
                    input_tokens: 100,
                    output_tokens: 7,
                    cached_input_tokens: 0,
                },
                StreamEvent::Done,
            ])))
        }
    }

    /// `engine` as the planner's call, with a default agent block.
    pub(crate) fn llm(engine: Arc<TextEngine>) -> PlannerLlm {
        PlannerLlm {
            engine,
            agent: Arc::new(
                toml::from_str("engine = \"openrouter\"\nmodel = \"stub/planner\"")
                    .expect("agent block"),
            ),
        }
    }
}
