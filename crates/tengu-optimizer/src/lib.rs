//! Prompt refinement implementations used before model invocation.
//!
//! Current implementations:
//! - `noop` (passthrough)
//! - `rules` (heuristic compression/summarization)
//!
//! Potential use case:
//! Switch between zero-cost passthrough and lightweight prompt compression by config profile.
pub mod noop;
pub mod rules;

// Feature-gated
// #[cfg(feature = "candle")]
// pub mod candle_ml;
// pub mod remote;

pub use noop::NoopRefiner;
pub use rules::RuleRefiner;
