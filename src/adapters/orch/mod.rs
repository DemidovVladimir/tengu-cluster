//! Harness-owned orchestration.
//!
//! - `config`    — OrchestratorConfig bridging (re-export from crate::adapters::config)
//! - `plan`      — Step, StepId, Plan types + topology helpers
//! - `planner`   — runs the orchestrator agent's LLM call
//! - `executor`  — DAG executor: parallel, ready-set scheduling
//! - `retry`     — per-step retry policy
//! - `replan`    — outer loop: re-invoke orchestrator on exhaustion
//! - `events`    — OrchestratorEvent enum + broadcast channel
//! - `roster`    — agent roster rendering + template substitution
//! - `telemetry` — event → tracing bridge
//!
//! NOTE: Temporarily named `orch` (not `orchestrator`) to avoid collision
//! with the legacy `src/adapters/orchestrator.rs` file. Will be renamed
//! to `orchestrator/` in Phase 7 when the legacy file is deleted.

pub mod config;
pub mod events;
pub mod executor;
pub mod plan;
pub mod planner;
pub mod replan;
pub mod retry;
pub mod roster;
pub mod telemetry;

// Public API re-exports
pub use events::{EventBus, EventReceiver, OrchestratorEvent};
pub use plan::{Plan, Step, StepId};
// `Orchestrator` struct is appended to this file in Task 4.9.
