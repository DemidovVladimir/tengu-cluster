pub mod noop;
pub mod rules;

// Feature-gated
// #[cfg(feature = "candle")]
// pub mod candle_ml;
// pub mod remote;

pub use noop::NoopRefiner;
pub use rules::RuleRefiner;
