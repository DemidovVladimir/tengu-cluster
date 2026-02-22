//! Model backend adapters implementing the `Engine` trait.
//!
//! Current baseline implementation:
//! - `ollama`
//! - `anthropic`
//! - `openai`
//! - `claude-code` (subprocess CLI)
//! - `huggingface` (Inference Providers API)
//!
//! Potential use case:
//! Add hosted providers (OpenAI/Anthropic/Google) behind one runtime contract.
pub mod anthropic;
pub mod claude_code;
pub mod huggingface;
pub mod ollama;
pub mod openai;

// Feature-gated modules
// #[cfg(feature = "google")]
// pub mod google;
pub use anthropic::AnthropicEngine;
pub use claude_code::ClaudeCodeEngine;
pub use huggingface::HuggingFaceEngine;
pub use ollama::OllamaEngine;
pub use openai::OpenAIEngine;
