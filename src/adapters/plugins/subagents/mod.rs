// src/adapters/plugins/subagents/mod.rs
//! Subagents plugin — LLM-driven subagent spawning (task A7).
//!
//! Provides three control-plane tools:
//! - `sessions_spawn`   — spawn one subagent and await its final message.
//! - `sessions_fan_out` — spawn many subagents in parallel and await all.
//! - `subagents`        — list, kill, or steer currently running subagents.
//!
//! These are "control-plane" operations: they do not touch user files,
//! wallets, or HTTP endpoints themselves. The subagent that gets spawned
//! enforces its OWN `ToolScope` via the standard `PluginToolExecutor` path.
//! Therefore every `execute()` is annotated `// scope: pure-compute` for the
//! scope lint — spawning itself is not a resource access.
//!
//! The registry, runtime bundle, and `run_single_turn` helper live here;
//! the three tool wrappers live in sibling files.

#![allow(dead_code)]

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, Mutex};

use crate::adapters::config::Config;
use crate::adapters::memory_builder::MemoryServiceHandle;
use crate::adapters::ports::ShellExecutionPort;
use crate::adapters::secret_builder::SecretRegistry;
use crate::adapters::tool_plugin::{PluginCtx, Tool, ToolPlugin};
use crate::adapters::types::ToolDef;

pub(crate) mod fan_out;
pub(crate) mod manage;
pub(crate) mod spawn;

pub(crate) use fan_out::SessionsFanOutTool;
pub(crate) use manage::SubagentsTool;
pub(crate) use spawn::SessionsSpawnTool;

/// Tool definitions advertised by the subagents plugin.
///
/// Only surfaced by `channel_runtime::compute_subagent_tools` when the
/// orchestrator is enabled — this keeps the default tool surface unchanged
/// for users without orchestration configured.
pub(crate) fn tool_defs() -> Vec<ToolDef> {
    vec![
        SessionsSpawnTool::new().definition().clone(),
        SessionsFanOutTool::new().definition().clone(),
        SubagentsTool::new().definition().clone(),
    ]
}

// ---------------------------------------------------------------------------
// SubagentRuntime — a pluggable builder for subagent bodies.
//
// In production this is `ProductionSubagentRuntime`, which resolves the
// agent config from `Config`, builds an engine + tool executor, and runs a
// single chat turn via `collect_engine_response`. In tests we swap in
// `StubSubagentRuntime` so the registry's queueing / cancellation logic can
// be exercised without a real API key.
// ---------------------------------------------------------------------------

/// Bundle of everything needed to build a fresh subagent body.
///
/// Implementors run one spawn-to-completion cycle. The returned string is
/// the subagent's final assistant message (no marker wrapping — the tools
/// add markers at the outer layer).
#[async_trait]
pub(crate) trait SubagentRuntime: Send + Sync {
    async fn run_turn(
        &self,
        agent_name: &str,
        prompt: &str,
        cancel: Arc<AtomicBool>,
        steer: mpsc::UnboundedReceiver<String>,
    ) -> Result<String>;
}

/// Production runtime: resolves an agent by name from the shared `Config`
/// and runs one full engine turn with its own tools.
///
/// Kept intentionally simple for A7. Phase B may extend this with richer
/// steer-channel semantics (the current loop does not consume the steer
/// receiver) and per-subagent activity logs.
pub(crate) struct ProductionSubagentRuntime {
    pub config: Arc<Config>,
    pub http: reqwest::Client,
    pub shell: Arc<dyn ShellExecutionPort>,
    pub memory: Option<Arc<MemoryServiceHandle>>,
    pub secret_registry: Arc<SecretRegistry>,
}

#[async_trait]
impl SubagentRuntime for ProductionSubagentRuntime {
    async fn run_turn(
        &self,
        agent_name: &str,
        prompt: &str,
        cancel: Arc<AtomicBool>,
        _steer: mpsc::UnboundedReceiver<String>,
    ) -> Result<String> {
        use crate::adapters::channel_runtime;
        use crate::adapters::engine_builder::{
            build_engine, collect_engine_response, SanitizedToolExecutor, ToolExecutor,
        };
        use crate::adapters::ports::ToolActivityPort;
        use crate::adapters::types::{EngineContext, Message, Role, ToolCall, ToolDef};
        use crate::adapters::skill_builder::SkillRegistry;

        let agent_config = self
            .config
            .agents
            .get(agent_name)
            .ok_or_else(|| anyhow!("unknown agent '{}'", agent_name))?;

        let engine = build_engine(
            agent_name,
            agent_config,
            self.config.claude_code.as_ref(),
        )?;

        // Workspace — expand tildes the same way the fleet orchestrator does.
        let workspace: Option<std::path::PathBuf> = agent_config
            .workspace
            .as_ref()
            .map(|ws_raw| crate::adapters::tool_builder::expand_tilde(ws_raw));

        // Compute base tools + skill registry inline. This mirrors
        // `orchestrator::run_fleet_orchestrator` but does not install any
        // subagent tools on the child — subagents cannot themselves spawn
        // (yet). Phase B can lift this restriction.
        let (system_prompt, tools, tool_executor): (
            String,
            Vec<ToolDef>,
            Arc<dyn ToolExecutor>,
        ) = if let Some(ref ws) = workspace {
            let base_tools = channel_runtime::compute_base_tools(
                true,
                self.memory.is_some(),
                &agent_config.workspace_tools,
            );
            let base_reserved: Vec<String> =
                base_tools.iter().map(|t| t.name.clone()).collect();
            let mut skill_registry = SkillRegistry::new(base_reserved)
                .with_allowlist(Some(agent_config.skill_packages.clone()));
            let skill_source =
                crate::adapters::skill_builder::FileSystemSkillSource::new(ws.clone());
            skill_registry.reload(&skill_source);

            let current_tools = channel_runtime::rebuild_tools(&base_tools, &skill_registry);
            let prompt = channel_runtime::rebuild_system_prompt(
                agent_config,
                true,
                &skill_registry,
                &current_tools,
            );

            let activity: Arc<dyn ToolActivityPort> = Arc::new(LogToolActivity);
            let executor_opt = channel_runtime::build_tool_executor(
                ws,
                &current_tools,
                &skill_registry,
                &self.memory,
                &self.secret_registry,
                activity,
                Some(Arc::clone(&cancel)),
                Some(&self.http),
                Some(&self.config.memory),
                agent_config,
                None, // subagents not yet nested
                &self.config.mcp_servers,
            );
            let tool_exec: Arc<dyn ToolExecutor> = executor_opt
                .map(|e| Arc::new(e) as Arc<dyn ToolExecutor>)
                .unwrap_or_else(|| Arc::new(NoopRuntimeToolExecutor));
            (prompt, current_tools, tool_exec)
        } else {
            let prompt =
                crate::adapters::skill_builder::build_system_prompt(agent_config, false, &[]);
            (prompt, vec![], Arc::new(NoopRuntimeToolExecutor) as Arc<dyn ToolExecutor>)
        };

        let messages = vec![Message {
            role: Role::User,
            content: prompt.to_string(),
            tool_call_id: None,
            tool_calls: None,
        }];

        let context = EngineContext {
            workspace: workspace.clone(),
            system_prompt: Some(system_prompt),
            bridge_tools: None,
            max_tool_rounds: Some(agent_config.limits.max_tool_rounds),
            max_mcp_result_chars: None,
        };

        let sanitized = SanitizedToolExecutor::new(tool_executor.as_ref(), &self.secret_registry);

        let response = collect_engine_response(
            engine.as_ref(),
            &messages,
            &tools,
            &context,
            Some(&sanitized),
            None,
            Some(&cancel),
            Some(agent_config.limits.max_tokens_per_flow as u32),
            agent_config.limits.max_tool_rounds,
            agent_config.limits.max_tool_result_chars,
            agent_config.limits.stream_event_timeout_secs,
            agent_config.limits.compact_result_limit,
        )
        .await?;

        // Scope the unused types to silence dead-code warnings in case they
        // are needed above for the compiler but not directly referenced.
        let _ = std::marker::PhantomData::<ToolCall>;

        Ok(response.text)
    }
}

struct LogToolActivity;
impl crate::adapters::ports::ToolActivityPort for LogToolActivity {
    fn publish_tool_activity(&self, call: &crate::adapters::types::ToolCall) {
        tracing::debug!(tool = %call.name, "subagent tool call");
    }
}

struct NoopRuntimeToolExecutor;

#[async_trait::async_trait]
impl crate::adapters::engine_builder::ToolExecutor for NoopRuntimeToolExecutor {
    async fn execute(&self, call: &crate::adapters::types::ToolCall) -> Result<String> {
        // scope: pure-compute — this is the engine-level ToolExecutor, not a
        // plugin Tool. It unconditionally errors for subagents without a
        // workspace and has no resource access to gate.
        anyhow::bail!(
            "No tools available (subagent has no workspace): {}",
            call.name
        )
    }
}

// ---------------------------------------------------------------------------
// Registry + handle
// ---------------------------------------------------------------------------

/// In-flight subagent state.
///
/// The registry stores an `AbortHandle` rather than the `JoinHandle` so the
/// entry can live in the map for the full lifetime of the spawn while the
/// caller awaits the join outside the lock. `kill` aborts via this handle.
pub(crate) struct SubagentHandle {
    pub agent_name: String,
    pub cancel: Arc<AtomicBool>,
    pub steer: mpsc::UnboundedSender<String>,
    pub started_at: Instant,
    pub abort: tokio::task::AbortHandle,
}

/// Lightweight view of a running subagent, safe to return from list().
#[derive(Debug, Clone)]
pub(crate) struct SubagentInfo {
    pub agent_name: String,
    pub elapsed_seconds: u64,
}

/// Tracks running subagents for one orchestrator.
///
/// Each registry owns one `SubagentRuntime` used to materialize spawns.
/// `max_concurrent` bounds the number of in-flight agents — exceeding it
/// returns an `Err` rather than queueing (Phase B may add queueing).
pub(crate) struct SubagentRegistry {
    max_concurrent: usize,
    runtime: Arc<dyn SubagentRuntime>,
    running: Mutex<HashMap<String, SubagentHandle>>,
}

impl SubagentRegistry {
    pub(crate) fn new(max_concurrent: usize, runtime: Arc<dyn SubagentRuntime>) -> Self {
        Self {
            max_concurrent: max_concurrent.max(1),
            runtime,
            running: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn max_concurrent(&self) -> usize {
        self.max_concurrent
    }

    /// Spawn a subagent by name and await its completion.
    ///
    /// Returns the subagent's final assistant message (no marker wrapping).
    /// The caller (the tool) wraps it in `<<<BEGIN/END_SUBAGENT_RESULT>>>`.
    pub(crate) async fn spawn_and_await(
        &self,
        agent_name: &str,
        prompt: &str,
    ) -> Result<String> {
        // Reserve a slot. If another subagent with the same name is already
        // running we keep them separate by suffixing an occurrence counter;
        // the caller (tool) only ever reads back the un-suffixed agent name
        // in its marker string, so this is transparent at the LLM surface.
        let cancel = Arc::new(AtomicBool::new(false));
        let (steer_tx, steer_rx) = mpsc::unbounded_channel::<String>();

        {
            let running = self.running.lock().await;
            if running.len() >= self.max_concurrent {
                return Err(anyhow!(
                    "subagent pool full ({} running, max={}); cannot spawn '{}'",
                    running.len(),
                    self.max_concurrent,
                    agent_name,
                ));
            }
        }

        let runtime = Arc::clone(&self.runtime);
        let agent_owned = agent_name.to_string();
        let prompt_owned = prompt.to_string();
        let cancel_clone = Arc::clone(&cancel);

        let join = tokio::spawn(async move {
            runtime
                .run_turn(&agent_owned, &prompt_owned, cancel_clone, steer_rx)
                .await
        });
        let abort = join.abort_handle();

        let handle_key = self.next_key(agent_name).await;
        {
            let mut running = self.running.lock().await;
            running.insert(
                handle_key.clone(),
                SubagentHandle {
                    agent_name: agent_name.to_string(),
                    cancel: Arc::clone(&cancel),
                    steer: steer_tx,
                    started_at: Instant::now(),
                    abort,
                },
            );
        }

        // Await completion OUTSIDE the registry lock. The entry stays in the
        // map until the join resolves; list()/kill() see it during that time.
        let outcome = join.await;

        // Reap: remove from the map regardless of outcome.
        {
            let mut running = self.running.lock().await;
            running.remove(&handle_key);
        }

        match outcome {
            Ok(Ok(text)) => Ok(text),
            Ok(Err(e)) => Err(e),
            Err(join_err) if join_err.is_cancelled() => {
                Err(anyhow!("subagent '{}' cancelled", agent_name))
            }
            Err(join_err) => Err(anyhow!("subagent join failed: {}", join_err)),
        }
    }

    /// Enumerate currently running subagents.
    pub(crate) async fn list(&self) -> Vec<SubagentInfo> {
        let running = self.running.lock().await;
        running
            .values()
            .map(|h| SubagentInfo {
                agent_name: h.agent_name.clone(),
                elapsed_seconds: h.started_at.elapsed().as_secs(),
            })
            .collect()
    }

    /// Cancel a running subagent by name. Flips its cancel flag and aborts
    /// the tokio task. Returns `Ok(())` if at least one match was killed,
    /// else `Err` describing the missing agent.
    pub(crate) async fn kill(&self, agent_name: &str) -> Result<()> {
        let mut running = self.running.lock().await;
        let keys: Vec<String> = running
            .iter()
            .filter(|(_, h)| h.agent_name == agent_name)
            .map(|(k, _)| k.clone())
            .collect();
        if keys.is_empty() {
            return Err(anyhow!("no running subagent named '{}'", agent_name));
        }
        for key in keys {
            if let Some(handle) = running.remove(&key) {
                handle
                    .cancel
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                handle.abort.abort();
            }
        }
        Ok(())
    }

    /// Deliver a guidance message to a running subagent's steer channel.
    ///
    /// The A7 runtime does not consume the steer channel — this is a
    /// placeholder handshake for Phase B, which will drain the receiver
    /// mid-turn and inject guidance into the conversation. For now, the
    /// call verifies the agent exists and the channel is still open; if
    /// the channel is closed the caller receives an error describing that.
    pub(crate) async fn steer(&self, agent_name: &str, guidance: String) -> Result<()> {
        let running = self.running.lock().await;
        let handle = running
            .values()
            .find(|h| h.agent_name == agent_name)
            .ok_or_else(|| anyhow!("no running subagent named '{}'", agent_name))?;
        handle
            .steer
            .send(guidance)
            .map_err(|_| anyhow!("steer channel for '{}' is closed", agent_name))?;
        Ok(())
    }

    /// Produce a unique key for a handle. Normally just `agent_name`, but if
    /// another spawn is already in-flight we append a numeric suffix.
    async fn next_key(&self, agent_name: &str) -> String {
        let running = self.running.lock().await;
        if !running.contains_key(agent_name) {
            return agent_name.to_string();
        }
        for i in 2.. {
            let candidate = format!("{}#{}", agent_name, i);
            if !running.contains_key(&candidate) {
                return candidate;
            }
        }
        unreachable!("subagent key exhaustion")
    }
}

// ---------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------

pub(crate) struct SubagentsPlugin;

#[async_trait]
impl ToolPlugin for SubagentsPlugin {
    fn name(&self) -> &'static str {
        "subagents"
    }

    async fn tools(&self, ctx: &PluginCtx<'_>) -> Result<Vec<Arc<dyn Tool>>> {
        // Only surface the tools if a registry is actually available.
        if ctx.subagents.is_none() {
            return Ok(vec![]);
        }
        Ok(vec![
            Arc::new(SessionsSpawnTool::new()),
            Arc::new(SessionsFanOutTool::new()),
            Arc::new(SubagentsTool::new()),
        ])
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    /// Test stub: simulates a subagent body that either returns a canned
    /// string after a short delay, errors, or blocks indefinitely until
    /// cancelled.
    pub(crate) struct StubSubagentRuntime {
        pub behavior: StubBehavior,
    }

    pub(crate) enum StubBehavior {
        /// Return the given string after `delay_ms`.
        Ok { text: String, delay_ms: u64 },
        /// Return an error immediately.
        Err(String),
        /// Sleep forever unless the cancel flag is set, then bail out.
        Blocking,
    }

    #[async_trait]
    impl SubagentRuntime for StubSubagentRuntime {
        async fn run_turn(
            &self,
            _agent_name: &str,
            _prompt: &str,
            cancel: Arc<AtomicBool>,
            _steer: mpsc::UnboundedReceiver<String>,
        ) -> Result<String> {
            match &self.behavior {
                StubBehavior::Ok { text, delay_ms } => {
                    tokio::time::sleep(Duration::from_millis(*delay_ms)).await;
                    Ok(text.clone())
                }
                StubBehavior::Err(msg) => Err(anyhow!(msg.clone())),
                StubBehavior::Blocking => {
                    loop {
                        if cancel.load(Ordering::Relaxed) {
                            return Err(anyhow!("cancelled"));
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{StubBehavior, StubSubagentRuntime};
    use super::*;

    fn make_registry(max: usize, behavior: StubBehavior) -> Arc<SubagentRegistry> {
        let stub = Arc::new(StubSubagentRuntime { behavior }) as Arc<dyn SubagentRuntime>;
        Arc::new(SubagentRegistry::new(max, stub))
    }

    #[tokio::test]
    async fn registry_list_is_empty_by_default() {
        let registry = make_registry(
            4,
            StubBehavior::Ok {
                text: "ok".to_string(),
                delay_ms: 0,
            },
        );
        let list = registry.list().await;
        assert!(list.is_empty(), "fresh registry should list 0 agents");
    }

    #[tokio::test]
    async fn registry_spawn_returns_runtime_output() {
        let registry = make_registry(
            4,
            StubBehavior::Ok {
                text: "hello from subagent".to_string(),
                delay_ms: 5,
            },
        );
        let result = registry.spawn_and_await("worker", "do stuff").await.unwrap();
        assert_eq!(result, "hello from subagent");
        // Handle should be removed after completion.
        assert!(registry.list().await.is_empty());
    }

    #[tokio::test]
    async fn registry_max_concurrent_enforced() {
        // max=1, use a blocking stub so the first slot stays occupied.
        let registry = make_registry(1, StubBehavior::Blocking);

        // Spawn the first agent in the background so the slot is held.
        let r1 = Arc::clone(&registry);
        let _first = tokio::spawn(async move {
            // This will block until aborted — we don't await its result.
            r1.spawn_and_await("blocker", "hang forever").await
        });

        // Give the first spawn a moment to register.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Second spawn should fail fast with a "pool full" error.
        let result = registry.spawn_and_await("other", "try").await;
        assert!(result.is_err(), "second spawn should have been rejected");
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("pool full") || msg.contains("max="),
            "expected pool-full error, got: {}",
            msg
        );

        // Clean up: kill the blocker so the test exits promptly.
        let _ = registry.kill("blocker").await;
    }

    #[tokio::test]
    async fn registry_kill_removes_handle() {
        let registry = make_registry(4, StubBehavior::Blocking);

        let r = Arc::clone(&registry);
        let _task = tokio::spawn(async move { r.spawn_and_await("long", "run").await });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let listed = registry.list().await;
        assert_eq!(listed.len(), 1, "expected 1 agent running");
        assert_eq!(listed[0].agent_name, "long");

        registry.kill("long").await.unwrap();

        // Kill removes the handle synchronously; list should now be empty.
        let after = registry.list().await;
        assert!(after.is_empty(), "kill should have removed the handle");

        // Subsequent kill of a missing name should error.
        let missing = registry.kill("long").await;
        assert!(missing.is_err(), "second kill should error");
    }

    #[tokio::test]
    async fn registry_steer_errors_when_agent_missing() {
        let registry = make_registry(
            4,
            StubBehavior::Ok {
                text: "ok".to_string(),
                delay_ms: 0,
            },
        );
        let result = registry
            .steer("nobody", "go faster".to_string())
            .await;
        assert!(result.is_err(), "steer on missing agent should error");
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("nobody"),
            "expected error to mention agent name, got: {}",
            msg
        );
    }

    #[tokio::test]
    async fn plugin_returns_empty_when_registry_absent() {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let config = Config::default();
        let agent_config = config.agents.get("main").unwrap().clone();
        let ctx = PluginCtx {
            workspace: tmp.path(),
            config: &agent_config,
            http: reqwest::Client::new(),
            shell: Arc::new(crate::adapters::shell_executor::LocalShellExecutor::new()),
            memory: None,
            secret_registry: Arc::new(SecretRegistry::new()),
            subagents: None,
        };
        let tools = SubagentsPlugin.tools(&ctx).await.unwrap();
        assert!(
            tools.is_empty(),
            "plugin should surface no tools without a registry"
        );
    }

    #[tokio::test]
    async fn plugin_returns_three_tools_when_registry_present() {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let config = Config::default();
        let agent_config = config.agents.get("main").unwrap().clone();
        let registry = make_registry(
            4,
            StubBehavior::Ok {
                text: "ok".to_string(),
                delay_ms: 0,
            },
        );
        let ctx = PluginCtx {
            workspace: tmp.path(),
            config: &agent_config,
            http: reqwest::Client::new(),
            shell: Arc::new(crate::adapters::shell_executor::LocalShellExecutor::new()),
            memory: None,
            secret_registry: Arc::new(SecretRegistry::new()),
            subagents: Some(registry),
        };
        let tools = SubagentsPlugin.tools(&ctx).await.unwrap();
        let names: Vec<String> = tools.iter().map(|t| t.definition().name.clone()).collect();
        assert_eq!(names.len(), 3);
        assert!(names.contains(&"sessions_spawn".to_string()));
        assert!(names.contains(&"sessions_fan_out".to_string()));
        assert!(names.contains(&"subagents".to_string()));
    }
}
