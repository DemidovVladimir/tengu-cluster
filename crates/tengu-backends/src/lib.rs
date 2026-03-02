//! Model backend adapters implementing the `Engine` trait.
//!
//! Each module wraps a different AI provider behind the unified `Engine` contract:
//! - `ollama` — local Ollama inference server
//! - `anthropic` — Anthropic Messages API (Claude)
//! - `openai` — OpenAI Chat Completions API
//! - `openrouter` — OpenRouter unified API (aggregates Anthropic, OpenAI, Google, Meta, etc.)
//! - `claude_code` — Claude Code subprocess CLI
//! - `huggingface` — Hugging Face Inference Providers API
pub mod anthropic;
pub mod claude_code;
pub mod huggingface;
pub mod ollama;
pub mod openai;
pub mod openrouter;
mod tooling;

pub use anthropic::AnthropicEngine;
pub use claude_code::ClaudeCodeEngine;
pub use huggingface::HuggingFaceEngine;
pub use ollama::OllamaEngine;
pub use openai::OpenAIEngine;
pub use openrouter::OpenRouterEngine;
