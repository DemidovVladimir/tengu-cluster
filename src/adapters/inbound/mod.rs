//! Inbound (driving) adapters — what turns outside input into use-case calls:
//! the chat channels (TUI, Telegram, webhooks), the MCP bridge server, and
//! the `tengu eval` / `tengu skill evolve` commands.

pub(crate) mod activity;
pub(crate) mod channel;
pub(crate) mod cli;
pub(crate) mod eval;
pub(crate) mod evolve;
pub(crate) mod mcp_bridge;
#[cfg(feature = "telegram")]
pub(crate) mod telegram;
pub(crate) mod tui;
#[cfg(feature = "webhooks")]
pub(crate) mod webhooks;
