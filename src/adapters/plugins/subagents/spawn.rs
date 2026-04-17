// src/adapters/plugins/subagents/spawn.rs
//! `sessions_spawn` tool — spawn one subagent and await its result.

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::adapters::tool_plugin::{Tool, ToolCtx, ToolOutput};
use crate::adapters::types::ToolDef;

pub(crate) struct SessionsSpawnTool {
    def: ToolDef,
}

impl SessionsSpawnTool {
    pub(crate) fn new() -> Self {
        Self {
            def: ToolDef::new(
                "sessions_spawn",
                "Spawn a single subagent by name with a prompt and await its final \
                 assistant message. Returns the result wrapped in \
                 `<<<BEGIN_SUBAGENT_RESULT>>> … <<<END_SUBAGENT_RESULT>>>` markers.",
                json!({
                    "type": "object",
                    "properties": {
                        "agent": {
                            "type": "string",
                            "description": "Name of the agent to spawn (must match a key in config.agents)."
                        },
                        "prompt": {
                            "type": "string",
                            "description": "User-facing prompt for the subagent."
                        }
                    },
                    "required": ["agent", "prompt"]
                }),
            ),
        }
    }
}

#[async_trait]
impl Tool for SessionsSpawnTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        // scope: pure-compute — spawning is not a direct resource access.
        // The spawned subagent enforces its OWN ToolScope via the standard
        // PluginToolExecutor path, so we intentionally do not gate here.
        let agent = args
            .get("agent")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("sessions_spawn: missing 'agent'"))?;
        let prompt = args
            .get("prompt")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("sessions_spawn: missing 'prompt'"))?;

        let registry = ctx
            .subagents
            .ok_or_else(|| anyhow!("orchestrator disabled; sessions_spawn unavailable"))?;

        let result = registry.spawn_and_await(agent, prompt).await?;

        let wrapped = format!(
            "<<<BEGIN_SUBAGENT_RESULT agent={}>>>\n{}\n<<<END_SUBAGENT_RESULT>>>",
            agent, result
        );
        Ok(ToolOutput::from(wrapped))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::plugins::subagents::test_support::{StubBehavior, StubSubagentRuntime};
    use crate::adapters::plugins::subagents::{SubagentRegistry, SubagentRuntime};
    use crate::adapters::plugins::workspace::test_support::TestHarness;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn harness_with_registry(
        tmp: &TempDir,
        registry: Arc<SubagentRegistry>,
    ) -> (TestHarness, Arc<SubagentRegistry>) {
        let h = TestHarness::new(tmp.path());
        (h, registry)
    }

    #[tokio::test]
    async fn spawn_returns_wrapped_marker_block() {
        let tmp = TempDir::new().unwrap();
        let stub = Arc::new(StubSubagentRuntime {
            behavior: StubBehavior::Ok {
                text: "child said hi".to_string(),
                delay_ms: 0,
            },
        }) as Arc<dyn SubagentRuntime>;
        let registry = Arc::new(SubagentRegistry::new(2, stub));
        let (harness, registry) = harness_with_registry(&tmp, registry);
        let mut ctx = harness.ctx();
        ctx.subagents = Some(registry.as_ref());

        let tool = SessionsSpawnTool::new();
        let out = tool
            .execute(
                &json!({"agent": "worker", "prompt": "ping"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.text.starts_with("<<<BEGIN_SUBAGENT_RESULT agent=worker>>>"));
        assert!(out.text.contains("child said hi"));
        assert!(out.text.trim_end().ends_with("<<<END_SUBAGENT_RESULT>>>"));
    }

    #[tokio::test]
    async fn spawn_errors_without_registry() {
        let tmp = TempDir::new().unwrap();
        let harness = TestHarness::new(tmp.path());
        let ctx = harness.ctx(); // subagents: None
        let tool = SessionsSpawnTool::new();
        let err = tool
            .execute(&json!({"agent": "x", "prompt": "y"}), &ctx)
            .await
            .unwrap_err();
        assert!(format!("{}", err).contains("orchestrator disabled"));
    }

    #[tokio::test]
    async fn spawn_requires_agent_and_prompt() {
        let tmp = TempDir::new().unwrap();
        let stub = Arc::new(StubSubagentRuntime {
            behavior: StubBehavior::Ok {
                text: "ok".to_string(),
                delay_ms: 0,
            },
        }) as Arc<dyn SubagentRuntime>;
        let registry = Arc::new(SubagentRegistry::new(2, stub));
        let harness = TestHarness::new(tmp.path());
        let mut ctx = harness.ctx();
        ctx.subagents = Some(registry.as_ref());

        let tool = SessionsSpawnTool::new();
        let err = tool.execute(&json!({}), &ctx).await.unwrap_err();
        assert!(format!("{}", err).contains("missing 'agent'"));

        let err = tool
            .execute(&json!({"agent": "x"}), &ctx)
            .await
            .unwrap_err();
        assert!(format!("{}", err).contains("missing 'prompt'"));
    }
}
