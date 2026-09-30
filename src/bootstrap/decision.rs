//! Composition for `[decision_loops.<name>]`: Jev client + the loop agent's
//! tool executor (same allow-list, scopes and workspace a `run-agent`
//! subprocess of that agent gets; wrapped in `SanitizedToolExecutor` with the
//! caller's `SecretRegistry`) + the agent workspace's observation store
//! (`open_observation_store`: `<workspace>/.tengu/observations.db`, source of
//! `state.world`, + the history recorder when `[recorder]` is on; fail-soft)
//! → `application::decision_loop::DecisionLoop`. [`agent_tool_executor`] also
//! builds each `[feeds]` tool feed's executor (`bootstrap/runtime.rs`).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tracing::warn;

use crate::adapters::outbound::decisions::JevClient;
use crate::adapters::outbound::noop::NoopActivity;
use crate::adapters::outbound::observations::open_observation_store;
use crate::adapters::outbound::secrets::SanitizedToolExecutor;
use crate::application::decision_loop::DecisionLoop;
use crate::config::{AgentConfig, Config};
use crate::domain::secrets::SecretRegistry;
use crate::ports::decision::Escalator;
use crate::ports::engine::ToolExecutor;

/// `<TENGU_HOME>/logs/decisions.jsonl` — one line per decision.
pub(crate) fn audit_path() -> PathBuf {
    crate::config::paths::resolve_tengu_home()
        .join("logs")
        .join("decisions.jsonl")
}

/// Tool executor of `agent` for a decision loop or a `[feeds]` feed: the
/// allow-list, scopes and workspace a `run-agent` child of that agent gets,
/// wrapped in `SanitizedToolExecutor` with the process `secrets` — plus the
/// tool names it runs (none when no executor could be built: every call
/// then fails).
pub(crate) fn agent_tool_executor(
    config: &Config,
    agent: &AgentConfig,
    workspace: &Path,
    secrets: &Arc<SecretRegistry>,
) -> (Arc<dyn ToolExecutor>, BTreeSet<String>) {
    let (_, executor) = crate::bootstrap::tools::build_subprocess_tool_executor(
        agent,
        config,
        workspace,
        secrets,
        Arc::new(NoopActivity),
        None,
    );
    let (inner, runs): (Arc<dyn ToolExecutor>, BTreeSet<String>) = match executor {
        Some(e) => {
            let runs = e.additional_tool_defs(&[]).into_iter().map(|d| d.name);
            (Arc::new(e), runs.collect())
        }
        None => (
            Arc::new(crate::adapters::outbound::noop::NoopRuntimeToolExecutor),
            BTreeSet::new(),
        ),
    };
    let tools = Arc::new(SanitizedToolExecutor::new(inner, Arc::clone(secrets)));
    (tools, runs)
}

/// `secrets` is the process registry: tool text and typed observations are
/// redacted with it before they reach history, the audit log or Jev.
pub(crate) fn build_decision_loop(
    config: &Config,
    name: &str,
    escalator: Option<Arc<dyn Escalator>>,
    secrets: Arc<SecretRegistry>,
) -> Result<Arc<DecisionLoop>> {
    let dl = config
        .decision_loops
        .get(name)
        .ok_or_else(|| anyhow!("no [decision_loops.{name}] block in this config"))?;
    let agent = config.agents.get(&dl.agent).ok_or_else(|| {
        anyhow!(
            "decision_loops.{name}.agent: no [agents.{}] block",
            dl.agent
        )
    })?;

    let engine = Arc::new(JevClient::from_env(
        &dl.model,
        Duration::from_secs(dl.timeout_secs),
    )?);

    let workspace = agent
        .workspace
        .as_ref()
        .map(|p| crate::config::paths::expand_tilde(p))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let (tools, _) = agent_tool_executor(config, agent, &workspace, &secrets);

    let observations = match open_observation_store(&workspace, &agent.sandbox) {
        Ok(s) => Some(s),
        Err(e) => {
            let error = format!("{e:#}");
            warn!(decision_loop = %name, %error, "observation store unavailable; world reads as errors");
            None
        }
    };

    Ok(Arc::new(DecisionLoop::new(
        name,
        dl.clone(),
        engine,
        tools,
        observations,
        escalator,
        Some(crate::application::decision_loop::AuditLog {
            path: audit_path(),
            sandbox: config.sandbox_name.clone(),
        }),
    )))
}
