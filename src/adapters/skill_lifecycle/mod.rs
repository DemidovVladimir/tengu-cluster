//! Skill lifecycle — distillation, metric measurement, and bounded evolution.
//!
//! Harness-owned policy. Three entry points: the `skill_distill` LLM tool
//! (in `plugins/skill_lifecycle/`), `run_eval` (via `tengu eval`), and
//! `run_evolve` (via `tengu skill evolve`).

#![allow(dead_code)]

pub(crate) mod config;
pub(crate) mod fixtures;
pub(crate) mod metric_kinds;
pub(crate) mod metrics;
pub(crate) mod storage;
// pub(crate) mod runner;           // Task 12
pub(crate) mod scratch_worktree;
pub(crate) mod evolve;
pub(crate) mod approval_gate;
