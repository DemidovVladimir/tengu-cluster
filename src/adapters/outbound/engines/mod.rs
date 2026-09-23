//! Engine adapters — implementations of `ports::engine::Engine` and the
//! factory that builds one from an `[agents.<name>]` block.

#[cfg(feature = "claude_code")]
pub(crate) mod claude_code;
pub(crate) mod openrouter;

use anyhow::Result;

use crate::ports::engine::Engine;

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
        _ => {
            let defaults = crate::config::LimitsConfig::default();
            build_openrouter_engine(model, defaults.context_window as usize)
        }
    }
}

/// Build configured engine instance for one agent.
pub(crate) fn build_engine(
    _agent_id: &str,
    agent_config: &crate::config::AgentConfig,
    claude_code_config: Option<&crate::config::ClaudeCodeConfig>,
) -> Result<Box<dyn Engine>> {
    match agent_config.engine.as_str() {
        "claude_code" => {
            #[cfg(feature = "claude_code")]
            {
                let cc = claude_code_config.cloned().unwrap_or_default();
                let profile = agent_config
                    .claude_code
                    .as_ref()
                    .map(|c| c.builtin_tools_profile.as_str())
                    .unwrap_or("editor_shell");
                // `[egress]`: builtin Bash has no egress control — dropped
                // while a proxy is set. The CLI's own API traffic follows
                // `claude_cli_env` (HTTPS_PROXY) inside the engine.
                let profile =
                    crate::adapters::outbound::egress::policy().claude_code_profile(profile);
                let model_opt = if agent_config.model.is_empty() {
                    None
                } else {
                    Some(agent_config.model.clone())
                };
                let timeout = agent_config.limits.stream_event_timeout_secs;
                // Per-tool scopes ride into the MCP bridge subprocess as
                // TENGU_BRIDGE_SCOPES so Claude Code subagents are gated the
                // same way in-process OpenRouter agents are.
                Ok(Box::new(
                    crate::adapters::outbound::engines::claude_code::ClaudeCodeEngine::new(
                        std::path::PathBuf::from(&cc.cli_path),
                        crate::adapters::outbound::engines::claude_code::BuiltinToolsProfile::from_str(profile),
                        model_opt,
                        timeout,
                    )
                    .with_scopes(agent_config.scopes.clone()),
                ))
            }
            #[cfg(not(feature = "claude_code"))]
            {
                let _ = claude_code_config;
                anyhow::bail!("claude_code engine requires --features claude_code")
            }
        }
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
