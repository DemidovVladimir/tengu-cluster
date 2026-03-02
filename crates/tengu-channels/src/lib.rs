//! Channel adapters (`Pipe` implementations) for inbound/outbound messaging.
//!
//! Each module wraps a messaging transport behind the unified `Pipe` trait:
//! - `cli` — local stdin/stdout pipe
//! - `telegram` — Telegram bot via teloxide (feature-gated)

pub mod cli;

#[cfg(feature = "telegram")]
pub mod telegram;

pub use cli::CliPipe;

#[cfg(feature = "telegram")]
pub use telegram::TelegramPipe;
