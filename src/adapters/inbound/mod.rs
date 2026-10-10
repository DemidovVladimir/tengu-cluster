//! Inbound (driving) adapters — what turns outside input into use-case calls:
//! the chat channels (TUI, Telegram, webhooks), the MCP bridge server, the
//! local Studio server (`studio`, feature `studio`, on by default), the A2A
//! server (`a2a`, feature `a2a`, on by default), and the
//! `tengu eval` / `tengu skill evolve` commands.

#[cfg(feature = "a2a")]
pub(crate) mod a2a;
pub(crate) mod activity;
pub(crate) mod channel;
pub(crate) mod cli;
pub(crate) mod eval;
pub(crate) mod evolve;
pub(crate) mod mcp_bridge;
pub(crate) mod run;
#[cfg(feature = "studio")]
pub(crate) mod studio;
#[cfg(feature = "telegram")]
pub(crate) mod telegram;
pub(crate) mod tui;
#[cfg(feature = "webhooks")]
pub(crate) mod webhooks;
