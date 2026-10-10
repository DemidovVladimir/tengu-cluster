//! Bootstrap — the composition root. Builds concrete adapters and hands them
//! to the application as ports: tool executors, memory, the orchestrator,
//! sandbox config resolution. May import every layer; only `main.rs` and
//! inbound adapters import it (see `tests/layering_lint.rs`).

// The server half is used only with the `a2a` feature (`tengu a2a serve`).
#[cfg_attr(not(feature = "a2a"), allow(dead_code))]
pub(crate) mod a2a;
pub(crate) mod decision;
pub(crate) mod memory;
pub(crate) mod orchestrator;
pub(crate) mod runtime;
pub(crate) mod sandbox;
pub(crate) mod soe;
pub(crate) mod studio;
pub(crate) mod tools;
pub(crate) mod trace;
