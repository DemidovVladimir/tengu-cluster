//! Application — use cases (chat turn, orchestration, memory, skills, tool
//! dispatch). Depends on `domain`, `ports`, `config`; never on `adapters` or
//! `bootstrap` (see `tests/layering_lint.rs`).

pub(crate) mod backtest;
pub(crate) mod chat;
pub(crate) mod decision_loop;
pub(crate) mod evidence;
pub(crate) mod lineage;
pub(crate) mod memory;
pub(crate) mod metrics;
pub(crate) mod observe;
pub(crate) mod orchestrator;
pub(crate) mod paper;
pub(crate) mod ranking;
pub(crate) mod runtime;
pub(crate) mod skills;
pub(crate) mod soe;
pub(crate) mod sources;
pub(crate) mod studio;
pub(crate) mod tools;
pub(crate) mod trace_exec;
