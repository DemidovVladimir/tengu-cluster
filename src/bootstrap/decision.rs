//! Composition for `[decision_loops.<name>]`: Jev client + the loop agent's
//! tool executor (same allow-list, scopes and workspace a `run-agent`
//! subprocess of that agent gets) → `application::decision_loop::DecisionLoop`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};

use crate::adapters::outbound::decisions::JevClient;
use crate::adapters::outbound::noop::NoopActivity;
use crate::application::decision_loop::DecisionLoop;
use crate::config::Config;
use crate::domain::secrets::SecretRegistry;
use crate::ports::decision::Escalator;
use crate::ports::engine::ToolExecutor;

/// `<TENGU_HOME>/logs/decisions.jsonl` — one line per decision.
pub(crate) fn audit_path() -> PathBuf {
    crate::config::paths::resolve_tengu_home()
        .join("logs")
        .join("decisions.jsonl")
}

pub(crate) fn build_decision_loop(
    config: &Config,
    name: &str,
    escalator: Option<Arc<dyn Escalator>>,
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
    let (_, executor) = crate::bootstrap::tools::build_subprocess_tool_executor(
        agent,
        config,
        &workspace,
        &Arc::new(SecretRegistry::new()),
        Arc::new(NoopActivity),
        None,
    );
    let tools: Arc<dyn ToolExecutor> = match executor {
        Some(e) => Arc::new(e),
        None => Arc::new(crate::adapters::outbound::noop::NoopRuntimeToolExecutor),
    };

    Ok(Arc::new(DecisionLoop::new(
        name,
        dl.clone(),
        engine,
        tools,
        escalator,
        Some(audit_path()),
    )))
}
