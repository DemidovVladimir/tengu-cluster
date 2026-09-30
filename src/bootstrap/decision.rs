//! Composition for `[decision_loops.<name>]`: Jev client + the loop agent's
//! tool executor (same allow-list, scopes and workspace a `run-agent`
//! subprocess of that agent gets; wrapped in `SanitizedToolExecutor` with the
//! caller's `SecretRegistry`) + the agent workspace's observation store
//! (`<workspace>/.tengu/observations.db`, source of `state.world`; fail-soft)
//! → `application::decision_loop::DecisionLoop`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tracing::warn;

use crate::adapters::outbound::decisions::JevClient;
use crate::adapters::outbound::noop::NoopActivity;
use crate::adapters::outbound::observations::SqliteObservationStore;
use crate::adapters::outbound::secrets::SanitizedToolExecutor;
use crate::application::decision_loop::DecisionLoop;
use crate::config::Config;
use crate::domain::secrets::SecretRegistry;
use crate::ports::decision::Escalator;
use crate::ports::engine::ToolExecutor;
use crate::ports::observation::ObservationStore;

/// `<TENGU_HOME>/logs/decisions.jsonl` — one line per decision.
pub(crate) fn audit_path() -> PathBuf {
    crate::config::paths::resolve_tengu_home()
        .join("logs")
        .join("decisions.jsonl")
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
    let (_, executor) = crate::bootstrap::tools::build_subprocess_tool_executor(
        agent,
        config,
        &workspace,
        &secrets,
        Arc::new(NoopActivity),
        None,
    );
    let inner: Arc<dyn ToolExecutor> = match executor {
        Some(e) => Arc::new(e),
        None => Arc::new(crate::adapters::outbound::noop::NoopRuntimeToolExecutor),
    };
    let tools: Arc<dyn ToolExecutor> = Arc::new(SanitizedToolExecutor::new(inner, secrets));

    let observations = match SqliteObservationStore::open(&workspace) {
        Ok(s) => Some(Arc::new(s) as Arc<dyn ObservationStore>),
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
