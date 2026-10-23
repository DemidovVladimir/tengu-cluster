//! One in-process agent turn ([`run_once`]) — an A2A agent endpoint
//! (`bootstrap/a2a.rs`) and `tengu tool turn` (`cli/tool.rs`): the agent's
//! engine, its workspace (else a temp dir for the turn), its tools, skills,
//! scopes and system prompt, then the tool loop (`collect_engine_response`)
//! over one user message.
//!
//! | Surface | Builds its own, from the same parts (`bootstrap/tools.rs`) |
//! |---|---|
//! | chat (TUI, Telegram) | a turn per message with state, recall and `/reload` |
//! | `run-agent` step | the IPC step: plan inputs, `compress_and_store`, metrics |
//! | `tengu eval` row | stubbed tools, an observer, the row's metrics |
//! | `tengu doctor --engines` | the smoke tool set on a temp workspace |

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};

use crate::adapters::outbound::engines::build_engine;
use crate::adapters::outbound::noop::NoopActivity;
use crate::application::chat::tool_loop::{collect_engine_response, EngineResponse, LoopOpts};
use crate::bootstrap::tools::{
    agent_base_tools, agent_skill_registry, bridge_inputs, build_tool_executor,
    rebuild_system_prompt, rebuild_tools, workspace_or_temp,
};
use crate::config::Config;
use crate::domain::message::{Message, Role};
use crate::domain::secrets::SecretRegistry;
use crate::ports::engine::EngineContext;

/// How [`run_once`] runs a turn.
pub(crate) struct TurnOpts<'a> {
    /// Replaces the agent's system prompt (`tengu tool turn`'s test prompt).
    pub system: Option<&'a str>,
    /// Earlier `(user, assistant)` pairs, oldest first.
    pub history: &'a [(String, String)],
    /// No tools at all (the `[orchestrator]` agent served as an agent).
    pub no_tools: bool,
    /// The memory tools and a memory manager when `[memory]` is on.
    pub memory: bool,
    /// Prefix of the temp workspace of an agent without one.
    pub temp_prefix: &'a str,
}

/// One turn of `agent_name` on `text` (module table); tool output is
/// redacted with `secrets`. An agent that should have tools but whose
/// executor does not build (no egress tool client) is an error, never a
/// turn without them.
pub(crate) async fn run_once(
    cfg: &Config,
    agent_name: &str,
    text: &str,
    secrets: &Arc<SecretRegistry>,
    opts: TurnOpts<'_>,
) -> Result<EngineResponse> {
    let mut agent = cfg
        .agents
        .get(agent_name)
        .cloned()
        .ok_or_else(|| anyhow!("no [agents.{agent_name}] in the config"))?;
    let configured_workspace = agent.workspace.is_some();
    // `_turn_dir` (a temp workspace) is removed when the turn ends.
    let (workspace, _turn_dir) = workspace_or_temp(agent.workspace.as_deref(), opts.temp_prefix)?;
    agent.workspace = Some(workspace.clone());
    let engine =
        build_engine(agent_name, &agent, cfg.claude_code.as_ref()).context("build engine")?;
    let has_memory = opts.memory && cfg.memory.enabled;
    let skills = agent_skill_registry(&workspace, &agent, has_memory);
    let system = match opts.system {
        Some(system) => system.to_string(),
        None => rebuild_system_prompt(&agent, &skills),
    };

    let stack = if opts.no_tools {
        None
    } else {
        let memory = if has_memory {
            Some(
                crate::bootstrap::memory::build_memory_manager_async(
                    &cfg.memory,
                    configured_workspace.then_some(workspace.as_path()),
                )
                .await,
            )
        } else {
            None
        };
        let tools = rebuild_tools(&agent_base_tools(&agent, true, has_memory), &skills);
        let stack = build_tool_executor(
            &workspace,
            &tools,
            &skills,
            &memory,
            secrets,
            Arc::new(NoopActivity),
            None,
            Some(&cfg.memory),
            &agent,
            &cfg.mcp_servers,
        )
        .with_context(|| format!("no tool executor for agent {agent_name}"))?;
        Some(stack)
    };
    let (tools, executor) = match &stack {
        Some(stack) => (stack.tools.clone(), Some(Arc::clone(&stack.executor))),
        None => (Vec::new(), None),
    };

    let message = |role: Role, content: &str| Message {
        role,
        content: content.to_string(),
        tool_call_id: None,
        tool_calls: None,
    };
    let mut messages = vec![message(Role::System, &system)];
    for (q, a) in opts.history {
        messages.push(message(Role::User, q));
        messages.push(message(Role::Assistant, a));
    }
    messages.push(message(Role::User, text));
    let (bridge_tools, mcp_servers) =
        bridge_inputs(engine.manages_own_workspace(), &tools, &cfg.mcp_servers);
    let limits = &agent.limits;
    let context = EngineContext {
        workspace: Some(workspace.clone()),
        system_prompt: Some(system),
        bridge_tools,
        max_tool_rounds: Some(limits.max_tool_rounds),
        max_mcp_result_chars: Some(limits.max_mcp_result_chars),
        mcp_servers,
    };
    collect_engine_response(
        engine.as_ref(),
        &messages,
        &tools,
        &context,
        executor.as_deref(),
        &LoopOpts::from_limits(limits),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_missing_agent_is_named() {
        let opts = TurnOpts {
            system: None,
            history: &[],
            no_tools: true,
            memory: false,
            temp_prefix: "tengu-turn-test-",
        };
        let secrets = Arc::new(SecretRegistry::new());
        let err = run_once(&Config::default(), "ghost", "hi", &secrets, opts)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("no [agents.ghost]"), "{err:#}");
    }
}
