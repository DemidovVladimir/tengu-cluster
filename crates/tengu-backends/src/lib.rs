//! Model backend adapters implementing the `Engine` trait.
//!
//! Current baseline implementation:
//! - `ollama`
//! - `anthropic`
//! - `openai`
//! - `claude-code` (subprocess CLI)
//!
//! Potential use case:
//! Add hosted providers (OpenAI/Anthropic/Google) behind one runtime contract.
pub mod anthropic;
pub mod claude_code;
pub mod ollama;
pub mod openai;

// Feature-gated modules
// #[cfg(feature = "google")]
// pub mod google;
// #[cfg(feature = "huggingface")]
// pub mod huggingface;

pub use anthropic::AnthropicEngine;
pub use claude_code::ClaudeCodeEngine;
pub use ollama::OllamaEngine;
pub use openai::OpenAIEngine;
