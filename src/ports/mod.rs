//! Ports — traits the application layer depends on; adapters implement them.
//! Imports only `domain` and `config` (see `tests/layering_lint.rs`).

pub(crate) mod book;
pub(crate) mod builder;
pub(crate) mod clock;
pub(crate) mod decision;
pub(crate) mod engine;
pub(crate) mod evidence;
pub(crate) mod history;
pub(crate) mod lineage;
pub(crate) mod market_data;
pub(crate) mod memory;
pub(crate) mod observation;
pub(crate) mod orchestration;
pub(crate) mod paper;
pub(crate) mod runtime;
pub(crate) mod shell;
pub(crate) mod skill_source;
pub(crate) mod soe;
pub(crate) mod solana_signer;
pub(crate) mod solana_writes;
pub(crate) mod source_store;
pub(crate) mod tool;
pub(crate) mod tool_activity;
pub(crate) mod trace;
