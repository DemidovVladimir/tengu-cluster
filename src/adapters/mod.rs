// --- types & config ---
pub mod config;
pub(crate) mod token;
pub mod types;

// Re-export engine types at the adapters level for ergonomic access.
pub use types::{Engine, EngineContext, EngineDiagnostics};

pub(crate) mod chat_builder;
pub mod memory;
pub(crate) mod secret_builder;
pub(crate) mod skill_builder;
pub(crate) mod tool_builder;
pub(crate) mod usage;

// --- services & ports ---
pub(crate) mod flow_builder;
pub(crate) mod ports;
pub(crate) mod prompt_budget;
// --- adapters ---
pub(crate) mod channel_runtime;
#[cfg(feature = "claude_code")]
pub(crate) mod claude_code_engine;
pub(crate) mod engine_builder;
pub(crate) mod eval_builder;
pub(crate) mod mcp_bridge;
pub mod orchestrator;
pub(crate) mod plugins;
pub(crate) mod prune;
pub(crate) mod scaffold;
pub(crate) mod shell_executor;
pub(crate) mod skill_lifecycle;
#[cfg(feature = "telegram")]
pub(crate) mod telegram_builder;
pub(crate) mod tool_plugin;
pub(crate) mod tui;
