//! `tengu tool` (hidden) — catalog tools run in-process the way a `run-agent`
//! child or a decision loop runs them, results printed as JSON. The
//! in-process half of the bridge conformance harness
//! (`tests/bridge_conformance.rs`, tracker item `x-bridge-conformance-test`).
//!
//! | Command | stdout (logs go to stderr) |
//! |---|---|
//! | `tengu tool list` | every catalog tool name, all opt-ins included (the harness's completeness source), one JSON array |
//! | `tengu tool call --agent <a> --tool <t> [--args '<json>'] [--call-id <id>] [--sandbox <s>] [-c <file>]` | one `{"text", "observation", "is_error"}` |
//! | `tengu tool call --agent <a> --batch …` | stdin: one `{"tool", "args", "call_id"}` per line, all through ONE executor (like a `run-agent` step or a bridge session); stdout: one result object per line |
//!
//! `call` builds the executor like `run-agent` and `bootstrap/decision.rs`:
//!
//! | Input | Source |
//! |---|---|
//! | Config | `--sandbox` (`sandboxes/<s>/config.toml`) > `-c` > `$TENGU_CONFIG` > `<TENGU_HOME>/config.toml`; built-in defaults when absent |
//! | Agent | `[agents.<a>]` (no `description` needed) → `build_subprocess_tool_executor`: `subagent_config`, workspace root granted to every configured scope, `resolve_tool_scopes` with `no_shell_fallback`, `[[mcp_servers]]`, the agent's `tools` allow-list |
//! | Workspace | the agent's `workspace` (`~` expanded), else the cwd |
//! | Memory | `[memory] enabled` → `build_memory_manager_async` (as `run-agent`) |
//! | Secrets | `process_secret_registry` (inherited `TENGU_SECRETS_LOADED`, else the vault) + `SanitizedToolExecutor` |
//! | Call id | `--call-id` / a line's `call_id` → `ToolCtx.call_id`, mapped like the bridge's JSON-RPC id (`mcp_bridge::call_id`: `mcp:<process nonce>:<id>`, the id a string verbatim, a number in decimal); absent = none |
//! | Egress | the config's `[egress]`; `TENGU_EGRESS` wins |
//!
//! A failed call prints `is_error: true` and `text` = `ERROR: <error>`,
//! redacted — the text the bridge returns.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Subcommand;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::adapters::outbound::secrets::{
    process_secret_registry, secrets_file_path, SanitizedToolExecutor,
};
use crate::config::paths::{default_config_path, resolve_tengu_home, TENGU_CONFIG_ENV};
use crate::config::Config;
use crate::domain::message::ToolCall;
use crate::domain::secrets::SecretRegistry;
use crate::ports::engine::ToolExecutor;

#[derive(Subcommand)]
pub(super) enum ToolAction {
    /// Every catalog tool name (all opt-ins) as a JSON array.
    List,
    /// Run tools as `[agents.<agent>]`; print `{text, observation, is_error}` per call.
    Call {
        /// The `[agents.<name>]` block whose executor runs the tools.
        #[arg(long)]
        agent: String,
        /// Tool name (must be in that agent's tool set).
        #[arg(long, required_unless_present = "batch")]
        tool: Option<String>,
        /// Tool arguments, a JSON object.
        #[arg(long, default_value = "{}")]
        args: String,
        /// `ToolCtx.call_id` (none when absent).
        #[arg(long)]
        call_id: Option<String>,
        /// Read `{"tool", "args", "call_id"}` lines from stdin; one executor
        /// for all; one result line each.
        #[arg(long, conflicts_with_all = ["tool", "call_id"])]
        batch: bool,
        /// Load config from sandboxes/<name>/config.toml.
        #[arg(long)]
        sandbox: Option<String>,
        /// Config file (after the subcommand; `tengu -c <file> tool …` works too).
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
}

/// Entry point; `top_config` is the top-level `-c/--config`.
pub(super) async fn run_tool_command(
    top_config: Option<PathBuf>,
    action: ToolAction,
) -> Result<()> {
    match action {
        ToolAction::List => {
            println!("{}", Value::from(catalog_tool_names()));
            Ok(())
        }
        ToolAction::Call {
            agent,
            tool,
            args,
            call_id,
            batch,
            sandbox,
            config,
        } => {
            let config = load_config(config.or(top_config), sandbox)?;
            let tools = AgentTools::build(&config, &agent).await?;
            if batch {
                return run_batch(&tools).await;
            }
            let args: Value = serde_json::from_str(&args).context("--args is not JSON")?;
            let tool = tool.context("--tool is required without --batch")?;
            let id = call_id.map_or(Value::Null, Value::String);
            let id = crate::adapters::inbound::mcp_bridge::call_id(&id);
            println!("{}", tools.call(&tool, args, id).await);
            Ok(())
        }
    }
}

/// `advertised_defs` with memory on and every `WORKSPACE_TOOLS` opt-in: the
/// catalog as this binary was built (feature-gated rows only when compiled in).
pub(crate) fn catalog_tool_names() -> Vec<String> {
    let opt_ins: Vec<String> = crate::domain::tools::WORKSPACE_TOOLS
        .iter()
        .map(|s| s.to_string())
        .collect();
    crate::adapters::outbound::tools::advertised_defs(true, &opt_ins)
        .into_iter()
        .map(|d| d.name)
        .collect()
}

/// The config `tengu chat` would use, pinned as `TENGU_CONFIG` (children and
/// MCP stdio servers resolve the same file), egress installed.
fn load_config(path: Option<PathBuf>, sandbox: Option<String>) -> Result<Config> {
    let path = path.unwrap_or_else(default_config_path);
    std::env::set_var(TENGU_CONFIG_ENV, &path);
    let base = if path.is_file() {
        Config::load(&path).with_context(|| format!("load config {}", path.display()))?
    } else {
        Config::default()
    };
    crate::adapters::outbound::egress::install(&base.egress)?;
    crate::bootstrap::sandbox::load_sandbox_or(sandbox, base)
}

/// One agent's in-process executor (module table).
struct AgentTools {
    executor: SanitizedToolExecutor,
    secrets: Arc<SecretRegistry>,
}

impl AgentTools {
    async fn build(config: &Config, agent_name: &str) -> Result<Self> {
        let mut agent = config
            .agents
            .get(agent_name)
            .cloned()
            .with_context(|| format!("no [agents.{agent_name}] in the config"))?;
        agent.workspace = agent
            .workspace
            .as_ref()
            .map(|p| crate::config::paths::expand_tilde(p));
        let workspace = agent
            .workspace
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        let secrets = Arc::new(process_secret_registry(Some(&secrets_file_path(
            &resolve_tengu_home(),
        ))));
        let memory = if config.memory.enabled {
            Some(
                crate::bootstrap::memory::build_memory_manager_async(
                    &config.memory,
                    Some(&workspace),
                )
                .await,
            )
        } else {
            None
        };
        let (_, executor) = crate::bootstrap::tools::build_subprocess_tool_executor(
            &agent,
            config,
            &workspace,
            &secrets,
            Arc::new(crate::adapters::outbound::noop::NoopActivity),
            memory,
        );
        let executor = executor.context("no tool executor for this agent")?;
        Ok(Self {
            executor: SanitizedToolExecutor::new(Arc::new(executor), Arc::clone(&secrets)),
            secrets,
        })
    }

    /// One call → the printed object.
    async fn call(&self, tool: &str, args: Value, call_id: String) -> Value {
        let call = ToolCall {
            id: call_id,
            name: tool.to_string(),
            arguments: args,
        };
        match self.executor.execute_typed(&call, &[]).await {
            Ok(out) => json!({
                "text": out.text,
                "observation": out.observation,
                "is_error": false,
            }),
            // Errors bypass `SanitizedToolExecutor` (as in the bridge).
            Err(e) => self.error(&format!("ERROR: {e}")),
        }
    }

    fn error(&self, text: &str) -> Value {
        json!({"text": self.secrets.redact(text), "observation": null, "is_error": true})
    }
}

/// `--batch`: one result line per stdin line, in order.
async fn run_batch(tools: &AgentTools) -> Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let out = match serde_json::from_str::<Value>(&line) {
            Ok(v) => match v["tool"].as_str() {
                Some(tool) => {
                    let args = v.get("args").cloned().unwrap_or_else(|| json!({}));
                    let id = crate::adapters::inbound::mcp_bridge::call_id(&v["call_id"]);
                    tools.call(tool, args, id).await
                }
                None => tools.error("ERROR: batch line has no \"tool\""),
            },
            Err(e) => tools.error(&format!("ERROR: batch line is not JSON: {e}")),
        };
        println!("{out}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every catalog row's names, opt-ins included, no duplicates.
    #[test]
    fn list_covers_the_catalog() {
        let names = catalog_tool_names();
        for n in ["read_file", "http_request", "memory_search", "shared_cache"] {
            assert!(names.iter().any(|x| x == n), "{n} missing: {names:?}");
        }
        for n in crate::domain::tools::WORKSPACE_TOOLS {
            if *n == crate::domain::tools::AGENTIC_MEMORY && !cfg!(feature = "postgres_memory") {
                continue;
            }
            assert!(names.iter().any(|x| x == n), "{n} missing");
        }
        let unique: std::collections::HashSet<&String> = names.iter().collect();
        assert_eq!(unique.len(), names.len(), "duplicates: {names:?}");
    }
}
