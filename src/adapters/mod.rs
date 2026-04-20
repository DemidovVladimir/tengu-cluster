// --- types & config ---
pub mod config;
pub(crate) mod token;
pub mod types;

// Re-export engine types at the adapters level for ergonomic access.
pub use types::{Engine, EngineContext, EngineDiagnostics};

pub(crate) mod agent_builder;
pub(crate) mod chat_builder;
pub(crate) mod memory_builder;
pub(crate) mod secret_builder;
pub(crate) mod skill_builder;
pub(crate) mod task_builder;
pub(crate) mod tool_builder;
pub(crate) mod usage;

// --- services & ports ---
pub(crate) mod ports;
pub(crate) mod event_orchestrator;
pub(crate) mod flow_builder;
pub(crate) mod prompt_budget;
// --- adapters ---
#[cfg(feature = "claude_code")]
pub(crate) mod claude_code_engine;
pub(crate) mod channel_runtime;
pub(crate) mod embedding;
pub(crate) mod engine_builder;
pub(crate) mod mcp_bridge;
pub(crate) mod orchestrator;
pub(crate) mod plugins;
pub(crate) mod prune;
#[cfg(feature = "qdrant")]
pub(crate) mod qdrant_memory_store;
pub(crate) mod scaffold;
pub(crate) mod shell_executor;
pub(crate) mod skill_lifecycle;
pub(crate) mod tool_plugin;
#[cfg(feature = "telegram")]
pub(crate) mod telegram_builder;
pub(crate) mod tui;
