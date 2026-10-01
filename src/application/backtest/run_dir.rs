//! A backtest's run dir, `<state dir>/backtests/<run id>/` (xlab,
//! `docs/xlab-2026-10-01.md` § 6; `docs/runtime-2026-09-30.md` § State
//! layout): the run id and the files [`write_run_dir`] writes. The files are
//! written in full before the path is returned (the CLI prints it last).
//!
//! | File | Holds |
//! |---|---|
//! | `report.json` | the `BacktestReport` (`backtest/1:<run id>`), pretty JSON |
//! | `report.md` | `render_markdown` |
//! | `trades-<arm>.jsonl` | one `Trade` per line (legs, fills, costs, funding, exit reason), decision order — one file per arm |
//! | `candidates.jsonl` | one `Candidate` per line: legs, side, signal, decided / data-as-of, exit plan, features as-of |
//! | `skips.json` | `candidates` (skips by reason), `data_notes`, `skipped` (every skip); per arm `dropped_counts` / `dropped` (fill drops) and `refusals_by_rule` / `refusals` (`[risk]` caps) |
//! | `BacktestRun::extra_files` | e.g. the gate arm's `decisions.jsonl` |
//!
//! | Rule | Value |
//! |---|---|
//! | Run id | `<YYYYMMDDTHHMMSSZ>-<strategy>` (UTC); taken ⇒ `-2`, `-3`, …; claimed with `create_dir` (never two runs in one dir) |
//! | An extra file's name | `[A-Za-z0-9._-]`, ≤ 128 chars, not starting with `.`, none of the files above |

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Serialize;

use super::{BacktestRun, Prepared};
use crate::domain::backtest::engine::{count_skips, Refusal, Skip};
use crate::domain::backtest::spec::valid_name;

/// Suffixes tried before giving up on a run id.
const MAX_SUFFIX: u32 = 10_000;
const REPORT_JSON: &str = "report.json";
const REPORT_MD: &str = "report.md";
const CANDIDATES: &str = "candidates.jsonl";
const SKIPS: &str = "skips.json";
const MAX_FILE_NAME: usize = 128;

/// `<YYYYMMDDTHHMMSSZ>-<strategy>` at `now_ms` (UTC).
pub(crate) fn run_id_base(now_ms: i64, strategy: &str) -> String {
    let stamp = chrono::DateTime::from_timestamp_millis(now_ms).map_or_else(
        || now_ms.to_string(),
        |t| t.format("%Y%m%dT%H%M%SZ").to_string(),
    );
    format!("{stamp}-{strategy}")
}

/// The `k`-th run id of `base`: `base`, then `base-2`, `base-3`, …
fn nth(base: &str, k: u32) -> String {
    if k <= 1 {
        base.to_string()
    } else {
        format!("{base}-{k}")
    }
}

/// The first run id of `base` without a dir under `root` (not claimed).
pub(crate) fn propose_run_id(root: &Path, base: &str) -> String {
    (1..=MAX_SUFFIX)
        .map(|k| nth(base, k))
        .find(|id| !root.join(id).exists())
        .unwrap_or_else(|| nth(base, MAX_SUFFIX))
}

/// Create the first free `root/<run id of base>` (`create_dir`: atomic, a
/// concurrent run never gets the same dir) → (run id, dir).
fn claim_run_dir(root: &Path, base: &str) -> Result<(String, PathBuf)> {
    std::fs::create_dir_all(root).with_context(|| format!("create {}", root.display()))?;
    for k in 1..=MAX_SUFFIX {
        let id = nth(base, k);
        let dir = root.join(&id);
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok((id, dir)),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("create {}", dir.display())),
        }
    }
    bail!(
        "{} already holds {MAX_SUFFIX} runs named {base}…",
        root.display()
    )
}

/// An extra file's name is one plain file (module table).
pub(crate) fn check_file_name(name: &str) -> Result<()> {
    let plain = !name.is_empty()
        && name.len() <= MAX_FILE_NAME
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    let reserved = [REPORT_JSON, REPORT_MD, CANDIDATES, SKIPS].contains(&name)
        || (name.starts_with("trades-") && name.ends_with(".jsonl"));
    if !plain || reserved {
        bail!(
            "run dir file `{name}`: a plain name ([A-Za-z0-9._-], ≤ {MAX_FILE_NAME} chars, \
             no leading dot) that is not one of the run's own files"
        );
    }
    Ok(())
}

/// One JSON value per line.
fn jsonl<T: Serialize>(rows: &[T]) -> Result<String> {
    let mut out = String::new();
    for row in rows {
        out.push_str(&serde_json::to_string(row)?);
        out.push('\n');
    }
    Ok(out)
}

/// One arm's part of `skips.json`.
#[derive(Serialize)]
struct ArmSkips<'a> {
    dropped_counts: BTreeMap<String, usize>,
    dropped: &'a [Skip],
    refusals_by_rule: BTreeMap<String, usize>,
    refusals: &'a [Refusal],
}

/// `skips.json` (module table).
#[derive(Serialize)]
struct SkipsFile<'a> {
    candidates: BTreeMap<String, usize>,
    data_notes: &'a [String],
    skipped: &'a [Skip],
    arms: BTreeMap<&'a str, ArmSkips<'a>>,
}

fn skips_file<'a>(p: &'a Prepared, run: &'a BacktestRun) -> SkipsFile<'a> {
    let arms = run
        .arms
        .iter()
        .map(|(name, a)| {
            let mut by_rule = BTreeMap::new();
            for r in &a.refusals {
                *by_rule.entry(r.rule.clone()).or_insert(0) += 1;
            }
            (
                name.as_str(),
                ArmSkips {
                    dropped_counts: count_skips(&a.skipped),
                    dropped: &a.skipped,
                    refusals_by_rule: by_rule,
                    refusals: &a.refusals,
                },
            )
        })
        .collect();
    SkipsFile {
        candidates: p.set.skip_counts(),
        data_notes: &p.set.notes,
        skipped: &p.set.skipped,
        arms,
    }
}

fn write(dir: &Path, name: &str, text: &str) -> Result<()> {
    let path = dir.join(name);
    std::fs::write(&path, text).with_context(|| format!("write {}", path.display()))
}

/// Step 4 (`backtest/mod.rs`): claim the run id (the report takes it) and
/// write every file of the module table; returns the run dir.
pub(crate) fn write_run_dir(p: &Prepared, run: &mut BacktestRun) -> Result<PathBuf> {
    for name in run.extra_files.keys() {
        check_file_name(name)?;
    }
    if let Some(name) = run.arms.keys().find(|n| !valid_name(n)) {
        bail!("arm name `{name}`: [a-z0-9_], 1-48 characters (it names trades-<arm>.jsonl)");
    }
    let (run_id, dir) = claim_run_dir(&p.backtests_dir, &p.run_id_base)?;
    run.report.run_id = run_id;
    write(
        &dir,
        REPORT_JSON,
        &(serde_json::to_string_pretty(&run.report)? + "\n"),
    )?;
    write(&dir, REPORT_MD, &run.report.render_markdown())?;
    for (name, arm) in &run.arms {
        write(&dir, &format!("trades-{name}.jsonl"), &jsonl(&arm.trades)?)?;
    }
    write(&dir, CANDIDATES, &jsonl(&p.set.candidates)?)?;
    write(
        &dir,
        SKIPS,
        &(serde_json::to_string_pretty(&skips_file(p, run))? + "\n"),
    )?;
    for (name, text) in &run.extra_files {
        write(&dir, name, text)?;
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_ids_are_utc_stamps_with_suffixes() {
        // 2026-10-01 12:00:34.999 UTC.
        let now = 1_790_856_034_999;
        assert_eq!(
            run_id_base(now, "weekend_fade"),
            "20261001T120034Z-weekend_fade"
        );
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("backtests");
        let base = run_id_base(now, "w");
        assert_eq!(propose_run_id(&root, &base), base, "no root yet");
        let (id, dir) = claim_run_dir(&root, &base).unwrap();
        assert_eq!((id.as_str(), dir.is_dir()), (base.as_str(), true));
        assert_eq!(propose_run_id(&root, &base), format!("{base}-2"));
        assert_eq!(claim_run_dir(&root, &base).unwrap().0, format!("{base}-2"));
        std::fs::create_dir(root.join(format!("{base}-3"))).unwrap();
        assert_eq!(claim_run_dir(&root, &base).unwrap().0, format!("{base}-4"));
    }

    #[test]
    fn extra_file_names_are_plain_and_not_the_runs_own() {
        for ok in ["decisions.jsonl", "gate-notes.md", "a_b.json"] {
            assert!(check_file_name(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "../x",
            "a/b",
            ".hidden",
            "report.json",
            "skips.json",
            "trades-jev.jsonl",
            "sp ace",
            &"x".repeat(129),
        ] {
            assert!(check_file_name(bad).is_err(), "{bad}");
        }
    }
}
