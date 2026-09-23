//! Adapters — everything that talks to the outside world.
//!
//! - `inbound/` — driving adapters: CLI commands, TUI, Telegram, webhooks,
//!   MCP bridge, `tengu eval` / `tengu skill evolve`.
//! - `outbound/` — driven adapters: engines, tools, memory stores, MCP client,
//!   egress, secrets, shell, subprocess runner.
//!
//! Wiring lives in `crate::bootstrap`. See `docs/hexagonal-plan-2026-09-23.md`.

pub(crate) mod inbound;
pub(crate) mod outbound;
