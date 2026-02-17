//! Model backend adapters implementing the `Engine` trait.
//!
//! Current implementation:
//! - `ollama` backend.
//!
//! Dependency policy:
//! - Prefer official Rust SDKs when available and well maintained.
//! - If no official Rust SDK exists, use direct REST integration against
//!   official provider API docs with strict request/response typing.
//! - Avoid third-party multi-provider abstraction crates in core runtime.
//!
//! TODO(epic-backend-anthropic): Implement Anthropic backend via typed REST.
//! TODO(epic-backend-openai): Implement OpenAI backend via typed REST.
//! TODO(epic-backend-google): Implement Google Gemini backend via typed REST.
//! TODO(epic-backend-huggingface): Implement Hugging Face backend using `hf-hub`.
//! TODO(epic-backend-candle-local): Evaluate optional in-process Candle local engine
//! with CUDA/Metal acceleration and CPU fallback for direct local model execution.
//! TODO(epic-backend-capabilities): Normalize backend capability reporting
//! (tool use, streaming, context windows) for runtime selection.
pub mod ollama;

// Feature-gated modules
// #[cfg(feature = "anthropic")]
// pub mod anthropic;
// #[cfg(feature = "openai")]
// pub mod openai;
// #[cfg(feature = "google")]
// pub mod google;
// #[cfg(feature = "huggingface")]
// pub mod huggingface;

pub use ollama::OllamaEngine;
