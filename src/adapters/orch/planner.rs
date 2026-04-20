//! Planner — runs the orchestrator agent's LLM call, returns Plan or direct response.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::adapters::orch::plan::Plan;

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
    use crate::adapters::orch::plan::StepId;

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
