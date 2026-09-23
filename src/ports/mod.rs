//! Ports — traits the application layer depends on; adapters implement them.
//! Imports only `domain` and `config` (see `tests/layering_lint.rs`).

pub(crate) mod engine;
pub(crate) mod memory;
pub(crate) mod orchestration;
pub(crate) mod shell;
pub(crate) mod skill_source;
pub(crate) mod tool;
pub(crate) mod tool_activity;
