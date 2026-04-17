// src/adapters/plugins/subagents/manage.rs
//! `subagents` tool — list, kill, or steer running subagents.

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::adapters::tool_plugin::{Tool, ToolCtx, ToolOutput};
use crate::adapters::types::ToolDef;

pub(crate) struct SubagentsTool {
    def: ToolDef,
}

impl SubagentsTool {
    pub(crate) fn new() -> Self {
        Self {
            def: ToolDef::new(
                "subagents",
                "Manage running subagents. `action=list` enumerates them; \
                 `action=kill` cancels a subagent by name; `action=steer` delivers \
                 free-form guidance to a named subagent's steer channel. \
                 Outputs for `list` are one line per agent; `kill`/`steer` return \
                 a short confirmation.",
                json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["list", "kill", "steer"],
                            "description": "Which management action to perform."
                        },
                        "agent": {
                            "type": "string",
                            "description": "Agent name — required for kill/steer."
                        },
                        "guidance": {
                            "type": "string",
                            "description": "Guidance text — required for steer."
                        }
                    },
                    "required": ["action"]
                }),
            ),
        }
    }
}

#[async_trait]
impl Tool for SubagentsTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        // scope: pure-compute — management actions only affect the in-memory
        // subagent registry; they don't reach out to fs / net / wallets.
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("subagents: missing 'action'"))?;

        let registry = ctx
            .subagents
            .ok_or_else(|| anyhow!("orchestrator disabled; subagents tool unavailable"))?;

        match action {
            "list" => {
                let list = registry.list().await;
                if list.is_empty() {
                    return Ok(ToolOutput::from("no running subagents".to_string()));
                }
                let mut lines = Vec::with_capacity(list.len());
                for info in list {
                    lines.push(format!(
                        "{}  running  {}s",
                        info.agent_name, info.elapsed_seconds
                    ));
                }
                Ok(ToolOutput::from(lines.join("\n")))
            }
            "kill" => {
                let agent = args
                    .get("agent")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("subagents: 'kill' requires 'agent'"))?;
                registry.kill(agent).await?;
                Ok(ToolOutput::from(format!("killed {}", agent)))
            }
            "steer" => {
                let agent = args
                    .get("agent")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("subagents: 'steer' requires 'agent'"))?;
                let guidance = args
                    .get("guidance")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("subagents: 'steer' requires 'guidance'"))?;
                registry.steer(agent, guidance.to_string()).await?;
                Ok(ToolOutput::from(format!(
                    "steered {} ({} bytes)",
                    agent,
                    guidance.len()
                )))
            }
            other => Err(anyhow!("subagents: unknown action '{}'", other)),
        }
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

    #[tokio::test]
    async fn manage_list_says_empty_when_none_running() {
        let tmp = TempDir::new().unwrap();
        let stub = Arc::new(StubSubagentRuntime {
            behavior: StubBehavior::Ok {
                text: "x".to_string(),
                delay_ms: 0,
            },
        }) as Arc<dyn SubagentRuntime>;
        let registry = Arc::new(SubagentRegistry::new(4, stub));
        let harness = TestHarness::new(tmp.path());
        let mut ctx = harness.ctx();
        ctx.subagents = Some(registry.as_ref());

        let tool = SubagentsTool::new();
        let out = tool
            .execute(&json!({"action": "list"}), &ctx)
            .await
            .unwrap();
        assert_eq!(out.text, "no running subagents");
    }

    #[tokio::test]
    async fn manage_kill_requires_agent_arg() {
        let tmp = TempDir::new().unwrap();
        let stub = Arc::new(StubSubagentRuntime {
            behavior: StubBehavior::Ok {
                text: "x".to_string(),
                delay_ms: 0,
            },
        }) as Arc<dyn SubagentRuntime>;
        let registry = Arc::new(SubagentRegistry::new(4, stub));
        let harness = TestHarness::new(tmp.path());
        let mut ctx = harness.ctx();
        ctx.subagents = Some(registry.as_ref());

        let tool = SubagentsTool::new();
        let err = tool
            .execute(&json!({"action": "kill"}), &ctx)
            .await
            .unwrap_err();
        assert!(format!("{}", err).contains("requires 'agent'"));
    }

    #[tokio::test]
    async fn manage_unknown_action_errors() {
        let tmp = TempDir::new().unwrap();
        let stub = Arc::new(StubSubagentRuntime {
            behavior: StubBehavior::Ok {
                text: "x".to_string(),
                delay_ms: 0,
            },
        }) as Arc<dyn SubagentRuntime>;
        let registry = Arc::new(SubagentRegistry::new(4, stub));
        let harness = TestHarness::new(tmp.path());
        let mut ctx = harness.ctx();
        ctx.subagents = Some(registry.as_ref());

        let tool = SubagentsTool::new();
        let err = tool
            .execute(&json!({"action": "dance"}), &ctx)
            .await
            .unwrap_err();
        assert!(format!("{}", err).contains("unknown action"));
    }
}
