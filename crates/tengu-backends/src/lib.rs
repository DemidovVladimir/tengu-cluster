//! Model backend adapters implementing the `Engine` trait.
//!
//! Current baseline implementation:
//! - `ollama`
//! - `anthropic`
//!
//! Potential use case:
//! Add hosted providers (OpenAI/Anthropic/Google) behind one runtime contract.
pub mod anthropic;
pub mod ollama;

// Feature-gated modules
// #[cfg(feature = "openai")]
// pub mod openai;
// #[cfg(feature = "google")]
// pub mod google;
// #[cfg(feature = "huggingface")]
// pub mod huggingface;

pub use anthropic::AnthropicEngine;
pub use ollama::OllamaEngine;
