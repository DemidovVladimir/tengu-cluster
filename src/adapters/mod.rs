//! Adapters — everything that talks to the outside world.
//!
//! - `outbound/` — driven adapters: engines, tools, memory stores, MCP client,
//!   egress, secrets, shell, subprocess runner.
//! - the rest (being split into `inbound/`, `application/`, `bootstrap/` —
//!   see `docs/hexagonal-plan-2026-09-23.md`).

pub(crate) mod inbound;
pub(crate) mod outbound;

pub(crate) mod channel_runtime;
pub(crate) mod chat_builder;
pub(crate) mod eval_builder;
pub(crate) mod flow_builder;
pub(crate) mod mcp_bridge;
pub mod memory;
pub mod orchestrator;
pub(crate) mod prompt_budget;
pub(crate) mod skill_builder;
pub(crate) mod skill_lifecycle;
#[cfg(feature = "telegram")]
pub(crate) mod telegram_builder;
pub(crate) mod tui;
#[cfg(feature = "webhooks")]
pub(crate) mod webhook_builder;
