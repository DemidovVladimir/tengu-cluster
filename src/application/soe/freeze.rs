//! Freeze a run dir (roadmap O4 "assumptions frozen before later outcomes
//! are observed"): the cycle's files become a sealed record. The cycle dir
//! is frozen in place — it already sits in the private SOE state — instead
//! of a copy in an evidence vault; the manifest format is the vault's
//! (`application::evidence::manifest_bytes`).
//!
//! | Use case | Rule |
//! |---|---|
//! | [`freeze`] | every file of the run dir (`CycleStore::files`) hashed → `MANIFEST.json` (`{path, sha256, bytes}` sorted by path, pretty JSON + `\n`) → `CycleStore::freeze` (read-only); returns the manifest's sha256 |
//! | [`verify`] | the manifest re-read and every file re-hashed: `MATCH` · `MISMATCH` · `ABSENT` (listed, gone) · `EXTRA` (present, not listed); no manifest ⇒ not frozen |
//! | [`decision_sha256`] | canonical sha256 of `{file: sha256}` over every file but `ops.json` (latencies are measurement, not decision): two runs of the same inputs give the same value |
//! | [`verify_state`] | the whole SOE state (`tengu soe verify`, the review packet): every run dir [`verify`]d — the cycles under `cycles/`, each replay's report and case dirs (`replays.jsonl`), each review (`reviews.jsonl`); `forecast-log.jsonl` `verify_chain`ed; each frozen cycle against its log line ([`LogState`]) |
//!
//! | [`LogState`] (one per frozen cycle, and per line without one) | When |
//! |---|---|
//! | `MATCH` | the cycle's line equals its `forecast-line.json`, and its `forecast_sha256` is the canonical sha256 of the frozen `forecast.json` |
//! | `MISMATCH` | either differs: the forecast or the line was rewritten |
//! | `MISSING` | a frozen cycle with no line: the run stopped between its freeze and its appends (cycle module table: Learn) — its own `candidates.jsonl` / `episodes.jsonl` / `forecast-line.json` keep what was lost |
//! | `ORPHAN` | a line whose cycle is not a frozen dir |
//!
//! An `OPEN` run (claimed, never frozen) is listed, never a failure: a
//! failed cycle keeps its `failed.json`.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::cycle::{FORECAST, FORECAST_LINE};
use crate::application::evidence::manifest_bytes;
use crate::domain::canonical::{canonical_sha256, sha256_hex};
use crate::domain::evidence::ManifestEntry;
use crate::domain::soe::forecast::{sha256_of, verify_chain, Forecast, LogLine};
use crate::domain::soe::record::from_json;
use crate::ports::soe::{CycleStore, RunDir, RunStatus, StateLog, MANIFEST};

/// The file whose bytes measure the run, never decide it.
pub(crate) const OPS: &str = "ops.json";

fn sha256_bytes(b: &[u8]) -> String {
    format!("{:x}", Sha256::digest(b))
}

/// One manifest entry per file of `dir`, sorted by path.
pub(crate) fn entries(store: &dyn CycleStore, dir: &RunDir) -> Result<Vec<ManifestEntry>> {
    let mut out = Vec::new();
    for name in store.files(dir)? {
        let bytes = store
            .read(dir, &name)?
            .with_context(|| format!("{dir}/{name}: listed, not readable"))?;
        out.push(ManifestEntry {
            path: name,
            sha256: sha256_bytes(&bytes),
            bytes: bytes.len() as u64,
        });
    }
    Ok(out)
}

/// Module table: seal `dir`; the manifest's sha256.
pub(crate) fn freeze(store: &dyn CycleStore, dir: &RunDir) -> Result<String> {
    let manifest = manifest_bytes(&entries(store, dir)?)?;
    store.freeze(dir, &manifest)?;
    Ok(sha256_hex(
        std::str::from_utf8(&manifest).context("MANIFEST.json is not UTF-8")?,
    ))
}

/// One file's state against the manifest (module table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum FileCheck {
    Match,
    Mismatch,
    Absent,
    Extra,
}

/// [`verify`]'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Verified {
    pub run: String,
    /// `None`: no `MANIFEST.json` — the run was never frozen.
    pub manifest_sha256: Option<String>,
    pub files: Vec<(String, FileCheck)>,
}

impl Verified {
    pub(crate) fn ok(&self) -> bool {
        self.manifest_sha256.is_some() && self.files.iter().all(|(_, c)| *c == FileCheck::Match)
    }
}

/// Module table: `dir` re-hashed against its manifest.
pub(crate) fn verify(store: &dyn CycleStore, dir: &RunDir) -> Result<Verified> {
    let Some(bytes) = store.read(dir, MANIFEST)? else {
        return Ok(Verified {
            run: dir.to_string(),
            manifest_sha256: None,
            files: Vec::new(),
        });
    };
    let listed: Vec<ManifestEntry> =
        serde_json::from_slice(&bytes).with_context(|| format!("{dir}/{MANIFEST}"))?;
    let now: BTreeMap<String, ManifestEntry> = entries(store, dir)?
        .into_iter()
        .map(|e| (e.path.clone(), e))
        .collect();
    let mut files: Vec<(String, FileCheck)> = listed
        .iter()
        .map(|e| {
            let check = match now.get(&e.path) {
                None => FileCheck::Absent,
                Some(n) if n.sha256 == e.sha256 && n.bytes == e.bytes => FileCheck::Match,
                Some(_) => FileCheck::Mismatch,
            };
            (e.path.clone(), check)
        })
        .collect();
    for path in now.keys() {
        if !listed.iter().any(|e| &e.path == path) {
            files.push((path.clone(), FileCheck::Extra));
        }
    }
    files.sort();
    Ok(Verified {
        run: dir.to_string(),
        manifest_sha256: Some(sha256_hex(
            std::str::from_utf8(&bytes).context("MANIFEST.json is not UTF-8")?,
        )),
        files,
    })
}

/// Module table: the identity of what `dir` decided.
pub(crate) fn decision_sha256(store: &dyn CycleStore, dir: &RunDir) -> Result<String> {
    let files: BTreeMap<String, String> = entries(store, dir)?
        .into_iter()
        .filter(|e| e.path != OPS)
        .map(|e| (e.path, e.sha256))
        .collect();
    Ok(canonical_sha256(&serde_json::to_value(files)?))
}

/// A frozen cycle against its forecast-log line (module table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum LogState {
    Match,
    Mismatch,
    Missing,
    Orphan,
}

/// [`verify_state`]'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct StateCheck {
    /// Every frozen run dir, re-hashed.
    pub runs: Vec<Verified>,
    /// Run dirs claimed and never frozen (`cycles/<id>`, a replay's dirs).
    pub open: Vec<String>,
    /// `forecast-log.jsonl` lines.
    pub log_lines: usize,
    /// `verify_chain`'s breaks (none = intact).
    pub chain: Vec<String>,
    /// Per frozen cycle (and orphan line), by cycle id.
    pub log: Vec<(String, LogState)>,
}

impl StateCheck {
    pub(crate) fn ok(&self) -> bool {
        self.chain.is_empty()
            && self.runs.iter().all(Verified::ok)
            && self.log.iter().all(|(_, s)| *s == LogState::Match)
    }
}

/// Every `run` of a `replays.jsonl` / `reviews.jsonl` line, and its case dirs.
fn logged_runs(store: &dyn CycleStore, log: StateLog) -> Result<Vec<RunDir>> {
    #[derive(Deserialize)]
    struct Case {
        run: String,
    }
    #[derive(Deserialize)]
    struct Line {
        run: String,
        #[serde(default)]
        cases: Vec<Case>,
    }
    let mut out = Vec::new();
    for (i, l) in store.lines(log)?.iter().enumerate() {
        let line: Line = serde_json::from_str(l)
            .with_context(|| format!("{} line {}", log.file_name(), i + 1))?;
        for run in std::iter::once(&line.run).chain(line.cases.iter().map(|c| &c.run)) {
            out.push(
                RunDir::parse(run)
                    .with_context(|| format!("{} line {}: run `{run}`", log.file_name(), i + 1))?,
            );
        }
    }
    Ok(out)
}

/// Module table: the whole SOE state checked.
pub(crate) fn verify_state(store: &dyn CycleStore) -> Result<StateCheck> {
    let mut dirs: Vec<RunDir> = store.cycles()?.into_iter().map(RunDir::Cycle).collect();
    dirs.extend(logged_runs(store, StateLog::Replays)?);
    dirs.extend(logged_runs(store, StateLog::Reviews)?);
    let mut runs = Vec::new();
    let mut open = Vec::new();
    let mut frozen_cycles = BTreeSet::new();
    let mut seen = BTreeSet::new();
    for d in dirs {
        if !seen.insert(d.clone()) {
            continue;
        }
        match store.status(&d)? {
            RunStatus::Frozen => {
                if let RunDir::Cycle(id) = &d {
                    frozen_cycles.insert(id.clone());
                }
                runs.push(verify(store, &d)?);
            }
            RunStatus::Open => open.push(d.to_string()),
            // A logged run gone: its manifest is gone with it.
            RunStatus::Absent => runs.push(Verified {
                run: d.to_string(),
                manifest_sha256: None,
                files: Vec::new(),
            }),
        }
    }
    let raw = store.lines(StateLog::ForecastLog)?;
    let mut lines: Vec<LogLine> = Vec::new();
    let mut chain = Vec::new();
    for (i, l) in raw.iter().enumerate() {
        match serde_json::from_str::<LogLine>(l) {
            Ok(x) => lines.push(x),
            Err(e) => chain.push(format!(
                "{} line {}: {e}",
                StateLog::ForecastLog.file_name(),
                i + 1
            )),
        }
    }
    if chain.is_empty() {
        if let Err(e) = verify_chain(&lines) {
            chain.extend(e.iter().map(ToString::to_string));
        }
    }
    let mut log = Vec::new();
    for id in &frozen_cycles {
        let dir = RunDir::Cycle(id.clone());
        let state = match lines.iter().find(|l| &l.cycle_id == id) {
            None => LogState::Missing,
            Some(l) => {
                let own: Option<LogLine> = store
                    .read(&dir, FORECAST_LINE)?
                    .and_then(|b| serde_json::from_slice(&b).ok());
                let forecast: Option<Forecast> = store
                    .read(&dir, FORECAST)?
                    .and_then(|b| String::from_utf8(b).ok())
                    .and_then(|t| from_json::<Forecast>(t.trim_end()).ok());
                let sha = forecast.and_then(|f| sha256_of(&f).ok());
                if own.as_ref() == Some(l) && sha.as_deref() == Some(l.forecast_sha256.as_str()) {
                    LogState::Match
                } else {
                    LogState::Mismatch
                }
            }
        };
        log.push((id.clone(), state));
    }
    for l in &lines {
        if !frozen_cycles.contains(&l.cycle_id) {
            log.push((l.cycle_id.clone(), LogState::Orphan));
        }
    }
    log.sort();
    runs.sort_by(|a, b| a.run.cmp(&b.run));
    Ok(StateCheck {
        runs,
        open,
        log_lines: raw.len(),
        chain,
        log,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::soe::cycle::Target;
    use crate::application::soe::review::{build_review, ReviewRange};
    use crate::application::soe::tests::{load_case, Bench};

    /// The whole state: cycles, a review (its log line names its dir);
    /// a dropped log line is `MISSING`, a line without its cycle `ORPHAN`,
    /// an open run is listed and fails nothing.
    #[tokio::test]
    async fn state_check_finds_every_break() {
        let bench = Bench::new();
        bench
            .run_case(&load_case("weekly_rerun_w42"), Target::Cycle)
            .await;
        build_review(&*bench.store, &bench.clock, ReviewRange::default()).unwrap();
        bench
            .store
            .claim(&RunDir::Cycle("2026-W43".into()))
            .unwrap();
        let s = verify_state(&*bench.store).unwrap();
        assert!(s.ok(), "{s:?}");
        let runs: Vec<&str> = s.runs.iter().map(|r| r.run.as_str()).collect();
        assert_eq!(
            runs,
            ["cycles/2026-W41", "cycles/2026-W42", "reviews/2026-10-03"]
        );
        assert_eq!(s.open, ["cycles/2026-W43"]);
        assert_eq!(
            s.log,
            [
                ("2026-W41".to_string(), LogState::Match),
                ("2026-W42".to_string(), LogState::Match)
            ]
        );
        // The second line lost (a stop between freeze and append) …
        let second = {
            let mut m = bench.store.mem.lock().unwrap();
            m.logs
                .get_mut(&StateLog::ForecastLog)
                .unwrap()
                .pop()
                .unwrap()
        };
        let s = verify_state(&*bench.store).unwrap();
        assert!(!s.ok() && s.chain.is_empty());
        assert_eq!(s.log[1], ("2026-W42".to_string(), LogState::Missing));
        // … and a line whose cycle dir is gone.
        {
            let mut m = bench.store.mem.lock().unwrap();
            m.logs.get_mut(&StateLog::ForecastLog).unwrap().push(second);
            m.dirs.remove(&RunDir::Cycle("2026-W42".into()));
            m.frozen.remove(&RunDir::Cycle("2026-W42".into()));
        }
        let s = verify_state(&*bench.store).unwrap();
        assert!(
            s.log.contains(&("2026-W42".to_string(), LogState::Orphan)),
            "{s:?}"
        );
    }
}
