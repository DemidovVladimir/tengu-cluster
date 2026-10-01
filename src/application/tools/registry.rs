//! Tool registry + `PluginToolExecutor` — dispatches a model's tool call to
//! the registered `Tool`, building the per-call `ToolCtx` with the scope
//! configured for that tool.

#![allow(dead_code)]

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

use crate::domain::message::{ToolCall, ToolDef};
use crate::domain::scope::ToolScope;
use crate::domain::secrets::SecretRegistry;
use crate::ports::engine::ToolExecutor;
use crate::ports::memory::MemoryService;
use crate::ports::shell::ShellExecutionPort;
use crate::ports::tool::{ConversationView, PluginCtx, Tool, ToolCtx, ToolOutput, ToolPlugin};
use crate::ports::tool_activity::ToolActivityPort;

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

pub(crate) struct ToolRegistry {
    by_name: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub(crate) fn new() -> Self {
        Self {
            by_name: HashMap::new(),
        }
    }

    /// Register tools from a plugin, filtered by the allow list.
    pub(crate) async fn register_plugin(
        &mut self,
        plugin: &dyn ToolPlugin,
        ctx: &PluginCtx<'_>,
        allowed: &[String],
    ) -> Result<()> {
        for tool in plugin.tools(ctx).await? {
            let name = tool.definition().name.clone();
            if !allowed.is_empty() && !allowed.contains(&name) {
                continue;
            }
            if self.by_name.contains_key(&name) {
                anyhow::bail!("duplicate tool '{}' from plugin '{}'", name, plugin.name());
            }
            self.by_name.insert(name, tool);
        }
        Ok(())
    }

    /// Register a single tool directly.
    pub(crate) fn register_tool(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.definition().name.clone();
        self.by_name.insert(name, tool);
    }

    pub(crate) fn definitions(&self) -> Vec<ToolDef> {
        self.by_name
            .values()
            .map(|t| t.definition().clone())
            .collect()
    }

    pub(crate) fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.by_name.get(name)
    }

    pub(crate) fn tool_names(&self) -> Vec<String> {
        self.by_name.keys().cloned().collect()
    }

    pub(crate) async fn invoke(
        &self,
        name: &str,
        args: &Value,
        ctx: &ToolCtx<'_>,
    ) -> Result<ToolOutput> {
        let tool = self
            .by_name
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("unknown tool '{}'", name))?;
        tool.execute(args, ctx).await
    }
}

// ---------------------------------------------------------------------------
// PluginToolExecutor — bridges ToolRegistry into the engine's ToolExecutor
// ---------------------------------------------------------------------------

/// Wraps a ToolRegistry + context handles to implement the engine's ToolExecutor.
pub(crate) struct PluginToolExecutor {
    pub registry: ToolRegistry,
    pub workspace: std::path::PathBuf,
    pub shell: Arc<dyn ShellExecutionPort>,
    pub http: reqwest::Client,
    pub memory_manager: Option<Arc<dyn MemoryService>>,
    pub secret_registry: Arc<SecretRegistry>,
    pub activity: Arc<dyn ToolActivityPort>,
    pub scopes: HashMap<String, ToolScope>,
    /// Owned copy of the calling agent's config — borrowed into every
    /// `ToolCtx` so tools (e.g. `skill_distill`) can read the agent's
    /// engine + model when seeding generated artefacts. `None` only for
    /// harness-built executors that have no associated agent (currently
    /// none in production paths; some tests construct a stub executor
    /// without one).
    pub agent_config: Option<crate::config::AgentConfig>,
}

impl PluginToolExecutor {
    /// Return tool definitions registered in the executor but NOT in `already_advertised`.
    ///
    /// Used to surface dynamically-discovered plugin tools (currently: MCP proxy tools
    /// with `{server}__{tool}` names) to the LLM. The static plugins (workspace, http,
    /// crypto, cache, memory, skill) contribute tool defs via their own
    /// `tool_defs()` helpers which the caller already includes; this method returns
    /// only the extras.
    pub(crate) fn additional_tool_defs(&self, already_advertised: &[ToolDef]) -> Vec<ToolDef> {
        let known: std::collections::HashSet<&str> =
            already_advertised.iter().map(|t| t.name.as_str()).collect();
        self.registry
            .definitions()
            .into_iter()
            .filter(|def| !known.contains(def.name.as_str()))
            .collect()
    }
}

#[async_trait]
impl ToolExecutor for PluginToolExecutor {
    async fn execute(
        &self,
        call: &ToolCall,
        messages: &[crate::domain::message::Message],
    ) -> Result<String> {
        Ok(self.execute_typed(call, messages).await?.text)
    }

    async fn execute_typed(
        &self,
        call: &ToolCall,
        messages: &[crate::domain::message::Message],
    ) -> Result<ToolOutput> {
        self.activity.publish_tool_activity(call);

        if self.registry.get(&call.name).is_none() {
            // Name the tools it has: a model that invented a name (Gemini's
            // `UNEXPECTED_TOOL_CALL`) can retry with a real one.
            let mut names = self.registry.tool_names();
            names.sort();
            anyhow::bail!(
                "Tool '{}' is not available to this agent. Its tools: {}.",
                call.name,
                names.join(", ")
            );
        }

        let scope = self.scopes.get(&call.name).cloned().unwrap_or_default();
        let ctx = ToolCtx {
            workspace: &self.workspace,
            scope: &scope,
            shell: self.shell.as_ref(),
            http: &self.http,
            memory_manager: self.memory_manager.as_ref().map(|m| m.as_ref()),
            secret_registry: &self.secret_registry,
            activity: self.activity.as_ref(),
            conversation: ConversationView::new(messages),
            agent_config: self.agent_config.as_ref(),
            call_id: Some(call.id.as_str()).filter(|id| !id.is_empty()),
        };

        self.registry
            .invoke(&call.name, &call.arguments, &ctx)
            .await
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::outbound::shell::LocalShellExecutor;
    use crate::domain::secrets::SecretRegistry;
    use serde_json::Value;

    struct StubTool {
        def: ToolDef,
    }

    #[async_trait]
    impl Tool for StubTool {
        fn definition(&self) -> &ToolDef {
            &self.def
        }
        async fn execute(&self, _args: &Value, _ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
            Ok(ToolOutput::from(String::new()))
        }
    }

    struct StubActivity;
    impl ToolActivityPort for StubActivity {
        fn publish_tool_activity(&self, _call: &ToolCall) {}
    }

    fn make_executor(defs: Vec<ToolDef>) -> PluginToolExecutor {
        let mut registry = ToolRegistry::new();
        for def in defs {
            registry.register_tool(Arc::new(StubTool { def }));
        }
        PluginToolExecutor {
            registry,
            workspace: std::path::PathBuf::from("."),
            shell: Arc::new(LocalShellExecutor::new()),
            http: reqwest::Client::new(),
            memory_manager: None,
            secret_registry: Arc::new(SecretRegistry::new()),
            activity: Arc::new(StubActivity),
            scopes: HashMap::new(),
            agent_config: None,
        }
    }

    #[test]
    fn additional_tool_defs_returns_only_unadvertised_tools() {
        let static_def = ToolDef::new("workspace.read", "desc", serde_json::json!({}));
        let dynamic_def = ToolDef::new("github__create_issue", "desc", serde_json::json!({}));
        let exec = make_executor(vec![static_def.clone(), dynamic_def.clone()]);

        // Caller already advertises only the static tool.
        let already = vec![static_def.clone()];
        let extras = exec.additional_tool_defs(&already);

        assert_eq!(extras.len(), 1, "expected exactly one extra tool");
        assert_eq!(extras[0].name, "github__create_issue");
    }

    struct TypedTool {
        def: ToolDef,
    }

    #[async_trait]
    impl Tool for TypedTool {
        fn definition(&self) -> &ToolDef {
            &self.def
        }
        async fn execute(&self, _args: &Value, _ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
            use crate::domain::observation::{ObsSource, ObsStatus, Observation};
            let obs = Observation {
                key: "typed/1:s".into(),
                schema: "typed/1".into(),
                tool: "typed".into(),
                observed_at_ms: 0,
                slot: Some(7),
                ttl_ms: 1_000,
                source: ObsSource::Live,
                status: ObsStatus::Ok,
                errors: vec![],
                headline: "typed s".into(),
                features: Default::default(),
                data: serde_json::json!({"x": 1}),
            };
            Ok(ToolOutput::observed(obs, 0))
        }
    }

    #[tokio::test]
    async fn execute_typed_passes_the_observation_through() {
        let mut exec = make_executor(vec![]);
        exec.registry.register_tool(Arc::new(TypedTool {
            def: ToolDef::new("typed", "desc", serde_json::json!({})),
        }));
        let call = ToolCall {
            id: "1".into(),
            name: "typed".into(),
            arguments: serde_json::json!({}),
        };
        let out = exec.execute_typed(&call, &[]).await.unwrap();
        let obs = out.observation.expect("observation carried");
        assert_eq!(obs.key, "typed/1:s");
        assert!(out.text.starts_with("typed s | ok 0s slot=7 live"));
        assert_eq!(exec.execute(&call, &[]).await.unwrap(), out.text);
    }

    /// Echoes `ToolCtx.call_id` (`-` for `None`).
    struct CallIdTool {
        def: ToolDef,
    }

    #[async_trait]
    impl Tool for CallIdTool {
        fn definition(&self) -> &ToolDef {
            &self.def
        }
        async fn execute(&self, _args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
            // scope: pure-compute
            Ok(ToolOutput::from(ctx.call_id.unwrap_or("-").to_string()))
        }
    }

    #[tokio::test]
    async fn call_id_reaches_the_tool_ctx() {
        let mut exec = make_executor(vec![]);
        exec.registry.register_tool(Arc::new(CallIdTool {
            def: ToolDef::new("probe", "desc", serde_json::json!({})),
        }));
        let call = |id: &str| ToolCall {
            id: id.into(),
            name: "probe".into(),
            arguments: serde_json::json!({}),
        };
        let seen = exec.execute(&call("t:s-1:3"), &[]).await.unwrap();
        assert_eq!(seen, "t:s-1:3");
        assert_eq!(exec.execute(&call(""), &[]).await.unwrap(), "-");
    }

    #[test]
    fn additional_tool_defs_empty_when_all_known() {
        let a = ToolDef::new("a", "desc", serde_json::json!({}));
        let b = ToolDef::new("b", "desc", serde_json::json!({}));
        let exec = make_executor(vec![a.clone(), b.clone()]);

        let already = vec![a, b];
        assert!(exec.additional_tool_defs(&already).is_empty());
    }
}
