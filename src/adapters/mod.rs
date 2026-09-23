//! Adapters — everything that talks to the outside world.
//!
//! - `inbound/` — driving adapters: CLI commands, TUI, Telegram, webhooks,
//!   MCP bridge (moving in phase 5).
//! - `outbound/` — driven adapters: engines, tools, memory stores, MCP client,
//!   egress, secrets, shell, subprocess runner.
//! - `channel_runtime` — composition root (becomes `bootstrap/` in phase 5).
//!
//! See `docs/hexagonal-plan-2026-09-23.md`.

pub(crate) mod inbound;
pub(crate) mod outbound;

pub(crate) mod channel_runtime;
pub(crate) mod mcp_bridge;
#[cfg(feature = "telegram")]
pub(crate) mod telegram_builder;
pub(crate) mod tui;
#[cfg(feature = "webhooks")]
pub(crate) mod webhook_builder;
