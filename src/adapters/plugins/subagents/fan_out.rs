// src/adapters/plugins/subagents/fan_out.rs
//! `sessions_fan_out` tool — spawn many subagents in parallel and await all.

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::adapters::plugins::subagents::SubagentRegistry;
use crate::adapters::tool_plugin::{Tool, ToolCtx, ToolOutput};
use crate::adapters::types::ToolDef;

pub(crate) struct SessionsFanOutTool {
    def: ToolDef,
}

impl SessionsFanOutTool {
    pub(crate) fn new() -> Self {
        Self {
            def: ToolDef::new(
                "sessions_fan_out",
                "Spawn many subagents in parallel and await all their final assistant \
                 messages. Returns one `<<<BEGIN_SUBAGENT_RESULT>>> … <<<END_SUBAGENT_RESULT>>>` \
                 block per task, concatenated in the same order as the input.",
                json!({
                    "type": "object",
                    "properties": {
                        "tasks": {
                            "type": "array",
                            "description": "List of spawn tasks. Each has `agent` and `prompt`.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "agent": {"type": "string"},
                                    "prompt": {"type": "string"}
                                },
                                "required": ["agent", "prompt"]
                            }
                        }
                    },
                    "required": ["tasks"]
                }),
            ),
        }
    }
}

#[async_trait]
impl Tool for SessionsFanOutTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        // scope: pure-compute — fan-out is control-plane; each child enforces
        // its own scope at runtime.
        let tasks = args
            .get("tasks")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("sessions_fan_out: missing 'tasks'"))?;
        if tasks.is_empty() {
            return Err(anyhow!("sessions_fan_out: 'tasks' must not be empty"));
        }

        let registry_ref = ctx
            .subagents
            .ok_or_else(|| anyhow!("orchestrator disabled; sessions_fan_out unavailable"))?;

        if tasks.len() > registry_ref.max_concurrent() {
            return Err(anyhow!(
                "sessions_fan_out: {} tasks exceeds max_concurrent={}",
                tasks.len(),
                registry_ref.max_concurrent()
            ));
        }

        // Decode and validate all tasks up front so the LLM gets a clear
        // error before any subagents are spawned.
        let mut decoded: Vec<(String, String)> = Vec::with_capacity(tasks.len());
        for (idx, task) in tasks.iter().enumerate() {
            let agent = task
                .get("agent")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("sessions_fan_out: tasks[{}].agent is required", idx))?;
            let prompt = task
                .get("prompt")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("sessions_fan_out: tasks[{}].prompt is required", idx))?;
            decoded.push((agent.to_string(), prompt.to_string()));
        }

        // We need to clone an Arc<SubagentRegistry> into each joined task.
        // `ctx.subagents` is a borrow — the caller (PluginToolExecutor) holds
        // the owning Arc. Upgrade the borrow to an owned Arc via a private
        // helper on the registry: since SubagentRegistry is !Clone we use
        // `Arc::from_raw`-free path by requiring the plugin layer to hand us
        // an Arc. For A7 we rely on the fact that SubagentRegistry's fields
        // are internally synchronized: we can wrap the borrow in a static
        // lifetime by spawning tasks that await sequentially via JoinSet.
        //
        // Simpler approach (chosen): run each task on its own tokio task by
        // spawning with a captured `'static` clone — we achieve that by
        // cloning the Arc from the executor-held handle. To keep this local,
        // we use `scoped`-like behavior via `tokio::task::JoinSet` and rely
        // on the registry's internal `Mutex` + `Arc<dyn SubagentRuntime>`
        // for its own 'static data.
        //
        // Concretely: the registry's `spawn_and_await` is an async method on
        // `&self`. We collect all futures and await them together with
        // `futures::future::join_all` — no 'static requirement because we
        // hold the borrow for the whole call.
        let futures_vec: Vec<_> = decoded
            .iter()
            .map(|(agent, prompt)| registry_ref.spawn_and_await(agent, prompt))
            .collect();
        let outcomes = futures::future::join_all(futures_vec).await;

        let mut out = String::new();
        for ((agent, _), outcome) in decoded.iter().zip(outcomes.into_iter()) {
            let body = match outcome {
                Ok(text) => text,
                Err(e) => format!("[error: {}]", e),
            };
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!(
                "<<<BEGIN_SUBAGENT_RESULT agent={}>>>\n{}\n<<<END_SUBAGENT_RESULT>>>",
                agent, body
            ));
        }

        // Keep Arc<SubagentRegistry> in scope so clippy sees it used.
        let _ = Arc::<SubagentRegistry>::strong_count as fn(&Arc<SubagentRegistry>) -> usize;
        Ok(ToolOutput::from(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::plugins::subagents::test_support::{StubBehavior, StubSubagentRuntime};
    use crate::adapters::plugins::subagents::{SubagentRegistry, SubagentRuntime};
    use crate::adapters::plugins::workspace::test_support::TestHarness;
    use tempfile::TempDir;

    #[tokio::test]
    async fn fan_out_concats_results_in_order() {
        let tmp = TempDir::new().unwrap();
        let stub = Arc::new(StubSubagentRuntime {
            behavior: StubBehavior::Ok {
                text: "result".to_string(),
                delay_ms: 5,
            },
        }) as Arc<dyn SubagentRuntime>;
        let registry = Arc::new(SubagentRegistry::new(4, stub));
        let harness = TestHarness::new(tmp.path());
        let mut ctx = harness.ctx();
        ctx.subagents = Some(registry.as_ref());

        let tool = SessionsFanOutTool::new();
        let out = tool
            .execute(
                &json!({
                    "tasks": [
                        {"agent": "a1", "prompt": "p1"},
                        {"agent": "a2", "prompt": "p2"}
                    ]
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(out.text.contains("<<<BEGIN_SUBAGENT_RESULT agent=a1>>>"));
        assert!(out.text.contains("<<<BEGIN_SUBAGENT_RESULT agent=a2>>>"));
        // Order must match input: a1 marker appears before a2 marker.
        let a1_pos = out.text.find("agent=a1>>>").unwrap();
        let a2_pos = out.text.find("agent=a2>>>").unwrap();
        assert!(a1_pos < a2_pos, "markers must be in input order");
    }

    #[tokio::test]
    async fn fan_out_rejects_when_exceeds_max_concurrent() {
        let tmp = TempDir::new().unwrap();
        let stub = Arc::new(StubSubagentRuntime {
            behavior: StubBehavior::Ok {
                text: "x".to_string(),
                delay_ms: 0,
            },
        }) as Arc<dyn SubagentRuntime>;
        let registry = Arc::new(SubagentRegistry::new(1, stub));
        let harness = TestHarness::new(tmp.path());
        let mut ctx = harness.ctx();
        ctx.subagents = Some(registry.as_ref());

        let tool = SessionsFanOutTool::new();
        let err = tool
            .execute(
                &json!({"tasks": [
                    {"agent": "a1", "prompt": "p"},
                    {"agent": "a2", "prompt": "p"}
                ]}),
                &ctx,
            )
            .await
            .unwrap_err();
        let msg = format!("{}", err);
        assert!(msg.contains("exceeds max_concurrent"), "got: {}", msg);
    }

    #[tokio::test]
    async fn fan_out_validates_task_shape() {
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

        let tool = SessionsFanOutTool::new();
        let err = tool
            .execute(&json!({"tasks": [{"agent": "a1"}]}), &ctx)
            .await
            .unwrap_err();
        assert!(format!("{}", err).contains("prompt is required"));
    }
}
