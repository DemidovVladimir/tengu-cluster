//! Channel adapters (`Pipe` implementations) for inbound/outbound messaging.
//!
//! Current baseline implementation:
//! - `cli`
//!
//! Potential use case:
//! Keep message transport pluggable so the same agent runtime can run on CLI/Telegram/Web.

pub mod cli;

// Feature-gated modules
// #[cfg(feature = "webchat")]
// pub mod webchat;
// #[cfg(feature = "telegram")]
// pub mod telegram;
// #[cfg(feature = "discord")]
// pub mod discord;

pub use cli::CliPipe;
