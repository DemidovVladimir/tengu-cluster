//! Application — use cases (chat turn, orchestration, memory, skills, tool
//! dispatch). Depends on `domain`, `ports`, `config`; never on `adapters` or
//! `bootstrap` (see `tests/layering_lint.rs`).

pub(crate) mod chat;
pub(crate) mod memory;
pub(crate) mod metrics;
pub(crate) mod orchestrator;
pub(crate) mod skills;
pub(crate) mod tools;
