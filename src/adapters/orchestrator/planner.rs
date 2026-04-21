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
    fn parse_verdict(raw: &str) -> anyhow::Result<PlannerVerdict> {
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
