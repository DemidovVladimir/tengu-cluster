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

use crate::adapters::memory::manager::MemoryManager;

/// Runs the orchestrator as an actual agent (LLM call with memory_search tool).
pub struct OrchestratorAgentPlanner {
    orchestrator_agent: String,
    chat: Arc<dyn OrchestratorChatPort>,
    #[allow(dead_code)]
    memory: Arc<MemoryManager>,
    #[allow(dead_code)]
    roster_md: String, // cached — computed once at construction time
}

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
}

impl OrchestratorAgentPlanner {
    pub fn new(
        orchestrator_agent: String,
        chat: Arc<dyn OrchestratorChatPort>,
        memory: Arc<MemoryManager>,
        roster_md: String,
    ) -> Self {
        Self {
            orchestrator_agent,
            chat,
            memory,
            roster_md,
        }
    }

    /// Parse the orchestrator LLM's response into a `PlannerVerdict`.
    ///
    /// Tolerates three common sloppiness patterns from LLMs:
    ///   1. Raw JSON — parses directly.
    ///   2. JSON wrapped in markdown fences (```json ... ```).
    ///   3. JSON embedded in prose ("Here's my plan: {...}. Let me know if…").
    ///
    /// For #3, extracts the largest balanced `{...}` substring and parses
    /// that. Returns a parse error only when no balanced JSON object is
    /// present at all.
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

        // No viable path — return the serde error for the original trimmed
        // text (most informative).
        let verdict: PlannerVerdict = serde_json::from_str(trimmed)?;
        Ok(verdict)
    }
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

#[async_trait]
impl Planner for OrchestratorAgentPlanner {
    async fn plan(&self, user_message: &str) -> anyhow::Result<PlannerVerdict> {
        let raw = self
            .chat
            .run_orchestrator_turn(&self.orchestrator_agent, user_message)
            .await?;
        Self::parse_verdict(&raw)
    }

    async fn replan(
        &self,
        user_message: &str,
        prior_plan: &Plan,
        failed_step_id: &str,
        error: &str,
    ) -> anyhow::Result<PlannerVerdict> {
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
        let raw = self
            .chat
            .run_orchestrator_turn(&self.orchestrator_agent, &context)
            .await?;
        Self::parse_verdict(&raw)
    }
}

// =====================================================================
// RagPlanner — Phase 4 of the redesign.
//
// Same `Planner` trait, same `OrchestratorChatPort` for the LLM call,
// same `parse_verdict` for tolerant JSON parsing. The ONLY difference
// from `OrchestratorAgentPlanner` is the roster: instead of a static
// markdown table baked at startup, the planner queries `tengu_registry`
// per turn and prepends a top-K ranked list to the user message.
//
// What Phase 4 deliberately does NOT do (tracked for later):
//   - Load skills/orchestrator/SKILL.md as a custom system prompt
//     (today the orchestrator agent's identity.instructions is used).
//   - Persist user messages to tengu_messages.
//   - Inject cross-plan recall context from tengu_outputs on replan.
//   - Swap ChatWorker for SubprocessRunner (that's Phase 4b).
//   - Emit OrchestratorEvent::RagQueried.
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
}

#[cfg(feature = "qdrant")]
impl RagPlanner {
    pub fn new(
        orchestrator_agent: String,
        chat: Arc<dyn OrchestratorChatPort>,
        memory_config: crate::adapters::config::MemoryConfig,
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
        }
    }

    /// Resolve (and cache) the underlying `RagStore`. Constructing it requires
    /// `OPENROUTER_API_KEY` and a reachable Qdrant on `memory.qdrant_url`; if
    /// either is missing the planner returns `Err` from this helper and the
    /// caller falls through to graceful degradation in `plan()`/`replan()`.
    async fn rag(&self) -> anyhow::Result<&Arc<crate::adapters::rag::RagStore>> {
        self.rag
            .get_or_try_init(|| async {
                crate::adapters::rag::RagStore::from_config(self.memory_config.clone())
                    .await
                    .map(Arc::new)
            })
            .await
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
        let hits = match self.rag().await {
            Ok(rag) => rag
                .search_registry(user_message, self.top_k)
                .await
                .unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "rag search_registry failed; falling back to empty roster");
                    Vec::new()
                }),
            Err(e) => {
                tracing::warn!(error = %e, "RagStore unavailable; falling back to empty roster");
                Vec::new()
            }
        };
        let roster = Self::format_roster(&hits);
        let combined = format!("{}\n## User message\n\n{}", roster, user_message);
        let raw = self
            .chat
            .run_orchestrator_turn_with_system(
                &self.orchestrator_agent,
                &self.system_prompt,
                &combined,
            )
            .await?;
        OrchestratorAgentPlanner::parse_verdict(&raw)
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
        let roster = Self::format_roster(&hits);
        let context = format!(
            "{}\n## A previous plan failed\n\n\
             Failed step: {}\nError after retries: {}\n\n\
             Prior plan steps:\n{}\n\n\
             Produce a new plan that avoids this failure, or respond directly if recovery is not possible.\n\n\
             ## Original user message\n\n{}",
            roster,
            failed_step_id,
            error,
            serde_json::to_string_pretty(prior_plan)?,
            user_message,
        );
        let raw = self
            .chat
            .run_orchestrator_turn_with_system(
                &self.orchestrator_agent,
                &self.system_prompt,
                &context,
            )
            .await?;
        OrchestratorAgentPlanner::parse_verdict(&raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::orchestrator::plan::StepId;

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

    #[test]
    fn extracts_json_from_prose() {
        let raw = r#"Here's my plan: {"kind": "direct", "response": "hi there"}. Let me know if that works!"#;
        match OrchestratorAgentPlanner::parse_verdict(raw).unwrap() {
            PlannerVerdict::Direct { response } => assert_eq!(response, "hi there"),
            _ => panic!("expected direct"),
        }
    }

    #[test]
    fn extracts_json_with_braces_in_strings() {
        // String literal contains { and } — must not unbalance the extractor.
        let raw =
            r#"Sure: {"kind": "direct", "response": "the answer has a { brace in it"}. Done."#;
        let v = OrchestratorAgentPlanner::parse_verdict(raw).unwrap();
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
        let v = OrchestratorAgentPlanner::parse_verdict(raw).unwrap();
        match v {
            PlannerVerdict::Direct { response } => assert_eq!(response, r#"{"embedded": true}"#),
            _ => panic!(),
        }
    }
}
