//! Domain — plain data and pure policy. Imports nothing from the rest of the
//! crate except `domain` itself, and no IO crates (see
//! `tests/layering_lint.rs`).

pub(crate) mod memory;
pub(crate) mod message;
pub(crate) mod plan;
pub(crate) mod scope;
pub(crate) mod session;
pub(crate) mod token;
pub(crate) mod usage;
