//! Engine adapters — implementations of `ports::engine::Engine` and the
//! factory that builds one from an `[agents.<name>]` block.

#[cfg(feature = "claude_code")]
pub(crate) mod claude_code;
pub(crate) mod local;
pub(crate) mod openrouter;

use anyhow::Result;

use crate::ports::engine::Engine;

use local::LocalEngine;
use openrouter::OpenRouterEngine;

// ---------------------------------------------------------------------------
// Factory — build engines from config
// ---------------------------------------------------------------------------

/// Build a lightweight engine for the planner/classifier from explicit engine + model strings.
#[allow(dead_code)]
pub(crate) fn build_planner_engine(
    engine_type: &str,
    model: &str,
    claude_code_config: Option<&crate::config::ClaudeCodeConfig>,
) -> Result<Box<dyn Engine>> {
    match engine_type {
        "claude_code" => {
            #[cfg(feature = "claude_code")]
            {
                let cc = claude_code_config.cloned().unwrap_or_default();
                let model_opt = if model.is_empty() {
                    None
                } else {
                    Some(model.to_string())
                };
                Ok(Box::new(
                    crate::adapters::outbound::engines::claude_code::ClaudeCodeEngine::new(
                        std::path::PathBuf::from(&cc.cli_path),
                        crate::adapters::outbound::engines::claude_code::BuiltinToolsProfile::ReadOnly,
                        model_opt,
                        cc.timeout_secs,
                    ),
                ))
            }
            #[cfg(not(feature = "claude_code"))]
            {
                let _ = (model, claude_code_config);
                anyhow::bail!("claude_code engine requires --features claude_code")
            }
        }
        "local" => {
            let defaults = crate::config::LimitsConfig::default();
            Ok(Box::new(LocalEngine::new(
                &crate::config::AgentLocalConfig::default(),
                model,
                defaults.context_window as usize,
                defaults.request_timeout_secs,
                None,
            )?))
        }
        _ => {
            let defaults = crate::config::LimitsConfig::default();
            build_openrouter_engine(model, defaults.context_window as usize)
        }
    }
}

/// What a caller asks of a Claude Code engine's bridge beyond the defaults:
/// a `run-agent` step's workspace grant and `compress_and_store` summary
/// file (`claude_code::StepBridge`), and the config file the bridge loads.
/// Ignored by the other engines.
#[derive(Debug, Clone, Default)]
#[cfg_attr(not(feature = "claude_code"), allow(dead_code))]
pub(crate) struct StepOpts {
    pub grant_workspace: bool,
    pub summary_file: Option<std::path::PathBuf>,
    /// The config the bridge loads (`TENGU_CONFIG`) and takes
    /// `[agents.<agent_id>]` from; `None` = the config in effect
    /// (`config::paths::default_config_path`). `tengu eval` passes its
    /// expanded eval config (`eval.rs::bridge_config_file`).
    pub config_file: Option<std::path::PathBuf>,
}

/// Build configured engine instance for one agent. `agent_id` names the
/// `[agents.<id>]` block of the config in `TENGU_CONFIG` for the Claude Code
/// bridge (the base agent for a composed plan step).
pub(crate) fn build_engine(
    agent_id: &str,
    agent_config: &crate::config::AgentConfig,
    claude_code_config: Option<&crate::config::ClaudeCodeConfig>,
) -> Result<Box<dyn Engine>> {
    build_step_engine(
        agent_id,
        agent_config,
        claude_code_config,
        StepOpts::default(),
    )
}

/// [`build_engine`] for a `run-agent` step or a turn run like one (the
/// doctor's smoke turn): a Claude Code bridge gets `step` explicitly, never
/// through an inherited process env.
pub(crate) fn build_step_engine(
    agent_id: &str,
    agent_config: &crate::config::AgentConfig,
    claude_code_config: Option<&crate::config::ClaudeCodeConfig>,
    step: StepOpts,
) -> Result<Box<dyn Engine>> {
    match agent_config.engine.as_str() {
        "claude_code" => {
            #[cfg(feature = "claude_code")]
            {
                let cc = claude_code_config.cloned().unwrap_or_default();
                let profile = agent_config
                    .claude_code
                    .as_ref()
                    .map(|c| c.builtin_tools_profile.trim())
                    .unwrap_or("editor_shell");
                // `[egress]`: builtin Bash has no egress control — dropped
                // while a proxy is set. The CLI's own API traffic follows
                // `claude_cli_env` (HTTPS_PROXY) inside the engine.
                let profile =
                    crate::adapters::outbound::egress::policy().claude_code_profile(profile);
                let profile = crate::adapters::outbound::engines::claude_code::BuiltinToolsProfile::parse(profile)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "agents.{agent_id}.claude_code.builtin_tools_profile: unknown profile '{profile}' (none | read_only | editor | editor_shell)"
                        )
                    })?;
                let model_opt = if agent_config.model.is_empty() {
                    None
                } else {
                    Some(agent_config.model.clone())
                };
                let timeout = agent_config.limits.stream_event_timeout_secs;
                // The MCP bridge subprocess loads `[agents.<agent_id>]` from
                // the config in effect, or `step.config_file` (TENGU_BRIDGE_AGENT
                // + TENGU_CONFIG), so Claude Code tools see what in-process
                // ones do; the scope map (TENGU_BRIDGE_SCOPES) is its fallback.
                let config_file = step
                    .config_file
                    .unwrap_or_else(crate::config::paths::default_config_path);
                Ok(Box::new(
                    crate::adapters::outbound::engines::claude_code::ClaudeCodeEngine::new(
                        std::path::PathBuf::from(&cc.cli_path),
                        profile,
                        model_opt,
                        timeout,
                    )
                    .with_scopes(agent_config.scopes.clone())
                    .with_bridge_agent(agent_id, &config_file)
                    .with_step_bridge(
                        crate::adapters::outbound::engines::claude_code::StepBridge {
                            grant_workspace: step.grant_workspace,
                            summary_file: step.summary_file,
                        },
                    ),
                ))
            }
            #[cfg(not(feature = "claude_code"))]
            {
                let _ = (agent_id, claude_code_config, step);
                anyhow::bail!("claude_code engine requires --features claude_code")
            }
        }
        "local" => Ok(Box::new(LocalEngine::new(
            &agent_config.local.clone().unwrap_or_default(),
            &agent_config.model,
            agent_config.limits.context_window.max(1) as usize,
            agent_config.limits.request_timeout_secs,
            agent_config.limits.max_output_tokens_per_turn,
        )?)),
        _ => {
            let context_window = agent_config.limits.context_window.max(1) as usize;
            build_openrouter_engine_with_limits(
                &agent_config.model,
                context_window,
                agent_config.limits.request_timeout_secs,
                agent_config.limits.max_output_tokens_per_turn,
            )
        }
    }
}

pub fn build_openrouter_engine(model: &str, context_window: usize) -> Result<Box<dyn Engine>> {
    build_openrouter_engine_with_limits(
        model,
        context_window,
        crate::config::default_request_timeout_secs(),
        None,
    )
}

/// Same as [`build_openrouter_engine`] but threads the per-agent request
/// timeout and optional output-token cap from `[limits]`.
pub fn build_openrouter_engine_with_limits(
    model: &str,
    context_window: usize,
    request_timeout_secs: u64,
    max_output_tokens_override: Option<u32>,
) -> Result<Box<dyn Engine>> {
    let api_key = std::env::var("OPENROUTER_API_KEY")
        .map_err(|_| anyhow::anyhow!("OPENROUTER_API_KEY is required"))?;
    let base_url = std::env::var("OPENROUTER_BASE_URL")
        .unwrap_or_else(|_| "https://openrouter.ai/api".to_string());
    Ok(Box::new(OpenRouterEngine::new(
        &base_url,
        model,
        &api_key,
        context_window,
        request_timeout_secs,
        max_output_tokens_override,
    )?))
}
