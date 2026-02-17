//! Prompt/refinement implementations used before model execution.
//!
//! Current implementations:
//! - `noop` (passthrough)
//! - `rules` (heuristic compression/summarization)
//!
//! Candle policy:
//! - Use Candle for in-process local ML acceleration.
//! - Prefer GPU execution when available (CUDA/Metal), with CPU fallback.
//!
//! TODO(epic-refiner-candle): Add local ML refiner for embeddings/summarization.
//! TODO(epic-refiner-candle-gpu): Add CUDA/Metal backend selection and runtime telemetry.
//! TODO(epic-refiner-remote): Add remote refiner client mode.
//! TODO(epic-refiner-selection): Add runtime profile based auto-selection.
pub mod noop;
pub mod rules;

// Feature-gated
// #[cfg(feature = "candle")]
// pub mod candle_ml;
// pub mod remote;

pub use noop::NoopRefiner;
pub use rules::RuleRefiner;
