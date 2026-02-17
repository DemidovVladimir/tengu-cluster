pub mod ollama;

// Feature-gated modules
// #[cfg(feature = "anthropic")]
// pub mod anthropic;
// #[cfg(feature = "huggingface")]
// pub mod huggingface;
// #[cfg(feature = "claude-code")]
// pub mod claude_code;

pub use ollama::OllamaEngine;
