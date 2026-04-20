//! Skill lifecycle — distillation, metric measurement, and bounded evolution.
//!
//! Harness-owned policy. Three entry points: the `skill_distill` LLM tool
//! (in `plugins/skill_lifecycle/`), `run_eval` (via `tengu eval`), and
//! `run_evolve` (via `tengu skill evolve`).

#![allow(dead_code)]

pub(crate) mod config;
pub(crate) mod metrics;
// pub(crate) mod metric_kinds;     // Task 3
// pub(crate) mod storage;          // Task 7
// pub(crate) mod fixtures;         // Task 8
// pub(crate) mod runner;           // Task 12
// pub(crate) mod scratch_worktree; // Task 14
// pub(crate) mod evolve;           // Task 15
// pub(crate) mod approval_gate;    // Task 16
