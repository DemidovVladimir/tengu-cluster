//! Skill eval runner — `tengu eval <skill>`.
//!
//! Spec: `docs/superpowers/specs/2026-04-19-eval-runner-design.md`.
//! Replays `skills/<skill>/evals/prompts.{md,yaml}` through a live agent,
//! scores each row pass/fail via an LLM judge, and writes a report.

use anyhow::Result;
use std::path::PathBuf;

#[derive(Debug)]
pub struct EvalArgs {
    pub skills: Vec<String>,
    pub sandbox: Option<String>,
    pub judge_model: Option<String>,
    pub concurrency: usize,
    pub format: OutputFormat,
    pub out_dir: Option<PathBuf>,
    pub filter: Option<String>,
    pub keep_workspace: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Table,
    Json,
}

pub async fn run(args: EvalArgs) -> Result<i32> {
    // Scaffold — Task 9 replaces this body with the real driver.
    let _ = args;
    println!("tengu eval: scaffold");
    Ok(0)
}
