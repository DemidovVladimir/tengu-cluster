use anyhow::Result;
use tengu_backends::{
    AnthropicEngine, ClaudeCodeEngine, HuggingFaceEngine, OllamaEngine, OpenAIEngine,
    OpenRouterEngine,
};
use tengu_core::Engine;
use tracing::error;

/// Build configured engine instance for one agent.
pub(crate) fn build_engine(
    _agent_id: &str,
    agent_config: &tengu_core::config::AgentConfig,
) -> Result<Box<dyn Engine>> {
    let context_window_override = agent_config
        .limits
        .context_window_override
        .map(|value| value.max(1) as usize);
    let max_output_tokens_override = agent_config
        .limits
        .max_output_tokens_per_turn
        .map(|value| value.max(1));

    match agent_config.engine.as_str() {
        "ollama" => {
            let base_url = std::env::var("OLLAMA_HOST")
                .unwrap_or_else(|_| "http://localhost:11434".to_string());
            Ok(Box::new(OllamaEngine::new(
                &base_url,
                &agent_config.model,
                context_window_override,
                max_output_tokens_override,
            )))
        }
        "anthropic" => {
            let api_key = std::env::var("ANTHROPIC_API_KEY").map_err(|_| {
                anyhow::anyhow!("ANTHROPIC_API_KEY is required for anthropic engine")
            })?;
            let base_url = std::env::var("ANTHROPIC_BASE_URL")
                .unwrap_or_else(|_| "https://api.anthropic.com".to_string());
            Ok(Box::new(AnthropicEngine::new(
                &base_url,
                &agent_config.model,
                &api_key,
                context_window_override,
                max_output_tokens_override,
            )))
        }
        "openai" => {
            let api_key = std::env::var("OPENAI_API_KEY")
                .map_err(|_| anyhow::anyhow!("OPENAI_API_KEY is required for openai engine"))?;
            let base_url = std::env::var("OPENAI_BASE_URL")
                .unwrap_or_else(|_| "https://api.openai.com".to_string());
            Ok(Box::new(OpenAIEngine::new(
                &base_url,
                &agent_config.model,
                &api_key,
                context_window_override,
                max_output_tokens_override,
            )))
        }
        "huggingface" => {
            let api_token = std::env::var("HF_TOKEN")
                .map_err(|_| anyhow::anyhow!("HF_TOKEN is required for huggingface engine"))?;
            let base_url = std::env::var("HF_BASE_URL")
                .unwrap_or_else(|_| "https://router.huggingface.co/v1".to_string());
            Ok(Box::new(HuggingFaceEngine::new(
                &base_url,
                &agent_config.model,
                &api_token,
                context_window_override,
                max_output_tokens_override,
            )))
        }
        "openrouter" => {
            let api_key = std::env::var("OPENROUTER_API_KEY").map_err(|_| {
                anyhow::anyhow!("OPENROUTER_API_KEY is required for openrouter engine")
            })?;
            let base_url = std::env::var("OPENROUTER_BASE_URL")
                .unwrap_or_else(|_| "https://openrouter.ai/api".to_string());
            Ok(Box::new(OpenRouterEngine::new(
                &base_url,
                &agent_config.model,
                &api_key,
                context_window_override,
                max_output_tokens_override,
            )))
        }
        "claude-code" => Ok(Box::new(ClaudeCodeEngine::new(
            &agent_config.model,
            context_window_override,
            max_output_tokens_override,
        ))),
        other => {
            error!(engine = %other, "Engine not yet implemented");
            Err(anyhow::anyhow!("Engine '{}' not yet implemented", other))
        }
    }
}
