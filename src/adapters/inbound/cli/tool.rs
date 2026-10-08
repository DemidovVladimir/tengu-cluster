//! `tengu tool` (hidden) — catalog tools run in-process the way a `run-agent`
//! child or a decision loop runs them, results printed as JSON. The
//! in-process half of the bridge conformance harness
//! (`tests/bridge_conformance.rs`, tracker item `x-bridge-conformance-test`).
//!
//! | Command | stdout (logs go to stderr) |
//! |---|---|
//! | `tengu tool list` | every catalog tool name, all opt-ins included (the harness's completeness source), one JSON array |
//! | `tengu tool call --agent <a> --tool <t> [--args '<json>'] [--call-id <id>] [--transcript <file>] [--sandbox <s>] [-c <file>]` | one `{"text", "observation", "is_error"}` |
//! | `tengu tool call --agent <a> --batch …` | stdin: one `{"tool", "args", "call_id"}` per line, all through ONE executor (like a `run-agent` step or a bridge session); stdout: one result object per line |
//! | `tengu tool turn --agent <a> --goal <text> [--sandbox <s>] [-c <file>]` | one engine turn as `[agents.<a>]` in this process — the `@<agent>` chat path (`chat/tool_loop.rs`: `chat:` call ids), so a private agent (no `description`: exec tools) runs too; its `tools` (`agent_base_tools`), shell skills (`skill_packages`) and `[[mcp_servers]]` tools, configured scopes as they are (no workspace grant), the bridge for `claude_code`; one `{"status", "output", "tools": [{name, ok}], "metrics"}` (the `run-agent` IPC fields the engine matrix reads) |
//!
//! `call` builds the executor like `run-agent` and `bootstrap/decision.rs`:
//!
//! | Input | Source |
//! |---|---|
//! | Config | `--sandbox` (`sandboxes/<s>/config.toml`) > `-c` > `$TENGU_CONFIG` > `<TENGU_HOME>/config.toml`; built-in defaults when absent |
//! | Agent | `[agents.<a>]` (no `description` needed) → `build_subprocess_tool_executor`: `subagent_config`, workspace root granted to every configured scope, `resolve_tool_scopes` with `no_shell_fallback`, `[[mcp_servers]]`, shell skills (`skill_packages`, none without a shell), the agent's `tools` allow-list |
//! | Workspace | the agent's `workspace` (`~` expanded), else a temp dir for this process — `run-agent`'s rule (`bootstrap::tools::workspace_or_temp`); `turn` too |
//! | Memory | `[memory] enabled` → `build_memory_manager_async` (as `run-agent`) |
//! | Secrets | `process_secret_registry` (inherited `TENGU_SECRETS_LOADED`, else the vault) + `SanitizedToolExecutor` |
//! | Call id | `--call-id` / a line's `call_id` → `ToolCtx.call_id`, mapped like the bridge's JSON-RPC id (`mcp_bridge::call_id`: `mcp:<process nonce>:<id>`, the id a string verbatim, a number in decimal); absent = none |
//! | Conversation | `--transcript <file>` (a JSON array of `Message`) → `ToolCtx.conversation` ending in the call, as the bridge builds it from `TENGU_BRIDGE_TRANSCRIPT_FILE` (`mcp_bridge::call_conversation`); absent = none (`skill_distill` refuses) |
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
        /// The conversation the tools see: a JSON array of messages, as a
        /// Claude Code run hands its bridge (none when absent).
        #[arg(long)]
        transcript: Option<PathBuf>,
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
    /// One engine turn as `[agents.<agent>]`; print `{status, output, tools, metrics}`.
    Turn {
        /// The `[agents.<name>]` block (no `description` needed).
        #[arg(long)]
        agent: String,
        /// The user message of the turn.
        #[arg(long)]
        goal: String,
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
            transcript,
            batch,
            sandbox,
            config,
        } => {
            let config = load_config(config.or(top_config), sandbox)?;
            let mut tools = AgentTools::build(&config, &agent).await?;
            tools.transcript = transcript;
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
        ToolAction::Turn {
            agent,
            goal,
            sandbox,
            config,
        } => {
            let config = load_config(config.or(top_config), sandbox)?;
            let out = match run_turn(&config, &agent, &goal).await {
                Ok(v) => v,
                Err(e) => json!({"status": "error", "error": format!("{e:#}")}),
            };
            println!("{out}");
            Ok(())
        }
    }
}

/// System prompt of `tengu tool turn`.
const TURN_SYSTEM: &str = "You are a tool-using agent under test. Follow the user's steps \
exactly, one tool call each, using the named tools. Do not ask questions.";

/// `tengu tool turn` (module table): the agent's engine and tools in this
/// process, one user message, the turn's text and tool runs.
async fn run_turn(config: &Config, agent_name: &str, goal: &str) -> Result<Value> {
    use crate::adapters::outbound::engines::build_engine;
    use crate::application::chat::tool_loop::collect_engine_response;
    use crate::bootstrap::tools::{agent_base_tools, agent_skill_registry, build_tool_executor};
    use crate::domain::message::{Message, Role};
    use crate::ports::engine::EngineContext;

    let mut agent = config
        .agents
        .get(agent_name)
        .cloned()
        .with_context(|| format!("no [agents.{agent_name}] in the config"))?;
    // The agent's workspace, else a temp dir for this turn (`_turn_dir`,
    // removed after it) — as a `run-agent` step.
    let configured_workspace = agent.workspace.is_some();
    let (workspace, _turn_dir) =
        crate::bootstrap::tools::workspace_or_temp(agent.workspace.as_deref(), "tengu-turn-")?;
    agent.workspace = Some(workspace.clone());
    let secrets = Arc::new(process_secret_registry(Some(&secrets_file_path(
        &resolve_tengu_home(),
    ))));
    let engine =
        build_engine(agent_name, &agent, config.claude_code.as_ref()).context("build engine")?;
    // The chat rule: the agent's `tools` (opt-ins included; empty = every
    // base tool), its shell skills, its `[[mcp_servers]]` tools (below).
    let skills = agent_skill_registry(&workspace, &agent, config.memory.enabled);
    let mut tools = agent_base_tools(&agent, true, config.memory.enabled);
    tools.extend(skills.active_tools());
    let memory = if config.memory.enabled {
        Some(
            crate::bootstrap::memory::build_memory_manager_async(
                &config.memory,
                configured_workspace.then_some(workspace.as_path()),
            )
            .await,
        )
    } else {
        None
    };
    let executor = build_tool_executor(
        &workspace,
        &tools,
        &skills,
        &memory,
        &secrets,
        Arc::new(crate::adapters::outbound::noop::NoopActivity),
        None,
        Some(&config.memory),
        &agent,
        &config.mcp_servers,
    )
    .context("no tool executor for this agent")?;
    tools.extend(executor.additional_tool_defs(&tools));
    let executor = SanitizedToolExecutor::new(Arc::new(executor), Arc::clone(&secrets));
    let message = |role, content: String| Message {
        role,
        content,
        tool_call_id: None,
        tool_calls: None,
    };
    let messages = [
        message(Role::System, TURN_SYSTEM.to_string()),
        message(Role::User, goal.to_string()),
    ];
    let limits = &agent.limits;
    let rounds = limits.max_tool_rounds.max(1);
    let context = EngineContext {
        workspace: Some(workspace.clone()),
        system_prompt: Some(TURN_SYSTEM.to_string()),
        bridge_tools: engine.manages_own_workspace().then(|| tools.clone()),
        max_tool_rounds: Some(rounds),
        max_mcp_result_chars: Some(limits.max_mcp_result_chars),
        mcp_servers: config.mcp_servers.clone(),
    };
    let resp = tokio::time::timeout(
        std::time::Duration::from_secs(limits.step_timeout_secs),
        collect_engine_response(
            engine.as_ref(),
            &messages,
            &tools,
            &context,
            Some(&executor),
            None,
            None,
            None,
            rounds,
            limits.max_tool_result_chars,
            limits.stream_event_timeout_secs,
            limits.compact_result_limit,
        ),
    )
    .await
    .with_context(|| format!("no answer within {}s", limits.step_timeout_secs))??;
    Ok(json!({
        "status": "ok",
        "output": resp.text,
        "tools": resp.tool_runs,
        "metrics": [{
            "prompt_tokens": resp.input_tokens_delta,
            "completion_tokens": resp.output_tokens_delta,
        }],
    }))
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
    /// `--transcript`: the conversation every call sees.
    transcript: Option<PathBuf>,
    /// The temp workspace of an agent without one, kept while this runs.
    _workspace_dir: Option<tempfile::TempDir>,
}

impl AgentTools {
    async fn build(config: &Config, agent_name: &str) -> Result<Self> {
        let mut agent = config
            .agents
            .get(agent_name)
            .cloned()
            .with_context(|| format!("no [agents.{agent_name}] in the config"))?;
        // As a `run-agent` step: the agent's workspace, else a temp dir kept
        // for this process (`workspace_dir`).
        let configured_workspace = agent.workspace.is_some();
        let (workspace, workspace_dir) =
            crate::bootstrap::tools::workspace_or_temp(agent.workspace.as_deref(), "tengu-step-")?;
        agent.workspace = Some(workspace.clone());

        let secrets = Arc::new(process_secret_registry(Some(&secrets_file_path(
            &resolve_tengu_home(),
        ))));
        let memory = if config.memory.enabled {
            Some(
                crate::bootstrap::memory::build_memory_manager_async(
                    &config.memory,
                    configured_workspace.then_some(workspace.as_path()),
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
            transcript: None,
            _workspace_dir: workspace_dir,
        })
    }

    /// One call → the printed object.
    async fn call(&self, tool: &str, args: Value, call_id: String) -> Value {
        let call = ToolCall {
            id: call_id,
            name: tool.to_string(),
            arguments: args,
        };
        let conversation = crate::adapters::inbound::mcp_bridge::call_conversation(
            self.transcript.as_deref(),
            &call,
        );
        match self.executor.execute_typed(&call, &conversation).await {
            Ok(out) => json!({
                "text": out.text,
                "observation": out.observation,
                "is_error": false,
            }),
            // `SanitizedToolExecutor` already redacted the error; `error` redacts
            // again (harmless), as the bridge does.
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
