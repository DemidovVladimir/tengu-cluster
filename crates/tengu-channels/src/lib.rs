//! Channel adapters (pipes) for inbound/outbound messaging surfaces.
//!
//! Current implementation:
//! - `cli` pipe is implemented.
//!
//! Dependency policy:
//! - Prefer platform-first crates with strong maintenance and security posture.
//! - Telegram target crate: `teloxide`.
//! - Discord target crate: `serenity` or `twilight` (final selection pending benchmark).
//! - WebChat target stack: `axum` + WebSocket runtime.
//!
//! TODO(epic-channel-webchat): Add WebChat pipe module and runtime wiring.
//! TODO(epic-channel-telegram): Add Telegram pipe module (teloxide) with access policy support.
//! TODO(epic-channel-discord): Add Discord pipe module (serenity/twilight) with thread/channel routing support.
//! TODO(epic-channel-lifecycle): Add unified connect/disconnect health semantics across pipes.
pub mod cli;

// Feature-gated modules
// #[cfg(feature = "webchat")]
// pub mod webchat;
// #[cfg(feature = "telegram")]
// pub mod telegram;
// #[cfg(feature = "discord")]
// pub mod discord;

pub use cli::CliPipe;
