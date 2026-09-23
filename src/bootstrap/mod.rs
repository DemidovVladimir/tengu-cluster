//! Bootstrap — the composition root. Builds concrete adapters and hands them
//! to the application as ports: tool executors, memory, the orchestrator,
//! sandbox config resolution. May import every layer; only `main.rs` and
//! inbound adapters import it (see `tests/layering_lint.rs`).

pub(crate) mod memory;
pub(crate) mod orchestrator;
pub(crate) mod sandbox;
pub(crate) mod tools;
