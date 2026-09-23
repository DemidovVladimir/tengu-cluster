// --- types & config ---
pub mod config;

pub(crate) mod chat_builder;
pub mod memory;
pub mod metrics;
pub(crate) mod secret_builder;
pub(crate) mod skill_builder;
pub(crate) mod tool_builder;
pub(crate) mod tool_utils;

// --- services & ports ---
pub(crate) mod flow_builder;
pub(crate) mod prompt_budget;
// --- adapters ---
pub(crate) mod channel_runtime;
#[cfg(feature = "claude_code")]
pub(crate) mod claude_code_engine;
pub(crate) mod egress;
pub(crate) mod engine_builder;
pub(crate) mod eval_builder;
pub(crate) mod mcp_bridge;
pub(crate) mod noop;
pub mod orchestrator;
pub(crate) mod plugins;
// `rag` (legacy Qdrant facade) was removed in Phase 6 — Open Brain (Postgres `agentic_memory`) is the memory backend.
pub(crate) mod prune;
pub mod runner;
pub(crate) mod scaffold;
pub(crate) mod shell_executor;
pub(crate) mod skill_lifecycle;
#[cfg(feature = "telegram")]
pub(crate) mod telegram_builder;
pub(crate) mod tui;
#[cfg(feature = "webhooks")]
pub(crate) mod webhook_builder;
