//! `RunDirSource` over a backtest run dir (`application/backtest/run_dir.rs`
//! writes it): plain files read, nothing written — a vault copy (read-only)
//! works as well as a live `<state dir>/backtests/<run id>/`.
//!
//! | File | Read as |
//! |---|---|
//! | `report.json` | `run_id`, `strategy` |
//! | `candidates.jsonl` · `trades-research.jsonl` | one `Candidate` / `Trade` per line; a bad line names its file and line |
//! | `decisions.jsonl` | one JSON value per line (the gate's audit); absent ⇒ not a gated run, refused |

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::ports::evidence::{RunDirSource, RunFiles};

/// A run dir on disk.
pub(crate) struct FsRunDir {
    dir: PathBuf,
}

impl FsRunDir {
    pub(crate) fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }
}

fn jsonl<T: DeserializeOwned>(path: &Path) -> Result<Vec<T>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    text.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, l)| {
            serde_json::from_str(l).with_context(|| format!("{} line {}", path.display(), i + 1))
        })
        .collect()
}

impl RunDirSource for FsRunDir {
    fn read(&self) -> Result<RunFiles> {
        let d = &self.dir;
        if !d.is_dir() {
            bail!("{}: no such run dir", d.display());
        }
        let decisions = d.join("decisions.jsonl");
        if !decisions.is_file() {
            bail!(
                "{}: no decisions.jsonl — not a gated run (tengu backtest --gate writes it)",
                d.display()
            );
        }
        let report_path = d.join("report.json");
        let report: Value = serde_json::from_str(
            &std::fs::read_to_string(&report_path)
                .with_context(|| format!("read {}", report_path.display()))?,
        )
        .with_context(|| format!("parse {}", report_path.display()))?;
        let field = |k: &str| {
            report
                .get(k)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| anyhow!("{}: no `{k}`", report_path.display()))
        };
        Ok(RunFiles {
            run_id: field("run_id")?,
            strategy: field("strategy")?,
            candidates: jsonl(&d.join("candidates.jsonl"))?,
            research: jsonl(&d.join("trades-research.jsonl"))?,
            decisions: jsonl(&decisions)?,
        })
    }

    fn describe(&self) -> String {
        self.dir.display().to_string()
    }
}
