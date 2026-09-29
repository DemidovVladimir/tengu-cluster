//! Ports — traits the application layer depends on; adapters implement them.
//! Imports only `domain` and `config` (see `tests/layering_lint.rs`).

pub(crate) mod decision;
pub(crate) mod engine;
pub(crate) mod memory;
pub(crate) mod observation;
pub(crate) mod orchestration;
pub(crate) mod shell;
pub(crate) mod skill_source;
pub(crate) mod solana_signer;
pub(crate) mod tool;
pub(crate) mod tool_activity;
