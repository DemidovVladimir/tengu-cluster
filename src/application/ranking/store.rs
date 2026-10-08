//! A strategy ranking's files under `<state dir>/strategy-rankings/`
//! (`config::xmarket::RANKINGS_DIR`): the per-date manifest, the dated
//! ranking and the `latest` view. Every write goes to a temp file in the
//! same dir, then a rename (readers never see a partial file; pattern of
//! `outbound/runtime_store.rs`).
//!
//! | Path (under `<contract id>/`) | Holds | Written |
//! |---|---|---|
//! | `<YYYY-MM-DD>/manifest.json` | [`Manifest`]: the date's window, the sealed contract's sha256, status `RUNNING` · `COMPLETE` · `INCOMPLETE` · `FAILED`, each strategy's run or failure, the holder | at start, after each strategy, at the end |
//! | `<YYYY-MM-DD>/ranking.json` · `ranking.md` | the ranking (`domain/backtest/ranking.rs`) | once per date; never rewritten once the manifest is `COMPLETE` / `INCOMPLETE` |
//! | `latest.md` · `latest.json` | the newest `COMPLETE` date's ranking, byte for byte | after both dated files, `.md` first — `latest.json` is the commit point (run-dir retention reads it) |

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::config::xmarket::rankings_dir;
use crate::domain::backtest::ranking::Ranking;
use crate::domain::lineage::value::{Locator, Time};

/// `manifest.json`'s schema tag.
pub(crate) const MANIFEST_SCHEMA: &str = "strategy_ranking_manifest/1";
pub(crate) const MANIFEST_JSON: &str = "manifest.json";
pub(crate) const RANKING_JSON: &str = "ranking.json";
pub(crate) const RANKING_MD: &str = "ranking.md";
pub(crate) const LATEST_JSON: &str = "latest.json";
pub(crate) const LATEST_MD: &str = "latest.md";

/// A ranking date's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum ManifestStatus {
    /// Strategies being run, or a run that stopped (shutdown, crash): the
    /// next run resumes it.
    Running,
    /// Published; `latest` replaced (when no newer date holds it).
    Complete,
    /// Published, `latest` kept: a listed strategy failed or went stale.
    Incomplete,
    /// The coordinator itself failed (writing the ranking, the lease): the
    /// next run resumes it.
    Failed,
}

impl ManifestStatus {
    /// `COMPLETE` / `INCOMPLETE`: the date's files are final; a rerun is a
    /// no-op.
    pub(crate) fn is_published(self) -> bool {
        matches!(self, ManifestStatus::Complete | ManifestStatus::Incomplete)
    }
}

/// One strategy's state in a manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum StrategyStatus {
    /// Its run dir is written; reused on resume while `report.json` still
    /// hashes `report_sha256`.
    Done,
    /// A stage failed (`stage`, `error`).
    Failed,
    /// An instrument's data is older than the contract's `[freshness]`.
    Stale,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StrategyEntry {
    pub status: StrategyStatus,
    /// `run:<state>/<run id>` (DONE).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    /// sha256 hex of the `report.json` bytes (DONE).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_sha256: Option<String>,
    /// `backtest` · `evaluate` · `freshness` (FAILED).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
    /// What failed, or the stale instruments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl StrategyEntry {
    /// The run id of a DONE entry's `run`.
    pub(crate) fn run_id(&self) -> Option<String> {
        match self.run.as_deref()?.parse::<Locator>().ok()? {
            Locator::Run { run_id, .. } => Some(run_id),
            _ => None,
        }
    }
}

/// `<contract id>/<date>/manifest.json` (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Manifest {
    pub schema: String,
    pub contract: String,
    /// The contract file's digest = its `[[sealed]]` row.
    pub contract_sha256: String,
    pub sealed_at: Time,
    pub sandbox: String,
    pub date: NaiveDate,
    pub tz: String,
    /// Decisions over `[from_ms, cutoff_ms)`, data through the cutoff.
    pub from_ms: i64,
    pub cutoff_ms: i64,
    pub status: ManifestStatus,
    /// By strategy name; a listed strategy not yet run has no entry.
    pub strategies: BTreeMap<String, StrategyEntry>,
    /// Who ran it last (`cli:<pid>`, the tool's call id).
    pub holder: String,
    pub started_at_ms: i64,
    pub updated_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at_ms: Option<i64>,
    /// `Ranking::content_sha256` of the published ranking.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_sha256: Option<String>,
    /// Whether this date's publish replaced `latest`.
    #[serde(default)]
    pub latest_replaced: bool,
    /// FAILED: why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `<state dir>/strategy-rankings/<contract id>`.
pub(crate) fn contract_dir(state_dir: &Path, contract: &str) -> PathBuf {
    rankings_dir(state_dir).join(contract)
}

/// `<state dir>/strategy-rankings/<contract id>/<YYYY-MM-DD>`.
pub(crate) fn date_dir(state_dir: &Path, contract: &str, date: NaiveDate) -> PathBuf {
    contract_dir(state_dir, contract).join(date.format("%Y-%m-%d").to_string())
}

/// Replace `dir/name` with `bytes` atomically (temp file + rename); the
/// temp file is removed when the rename fails.
pub(crate) fn write_atomic(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(name);
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        anyhow::Error::new(e).context(format!("replace {}", path.display()))
    })
}

/// `dir/manifest.json`: `None` when absent, `Err` when unreadable.
pub(crate) fn read_manifest(dir: &Path) -> Result<Option<Manifest>> {
    let path = dir.join(MANIFEST_JSON);
    let raw = match std::fs::read(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    serde_json::from_slice(&raw)
        .map(Some)
        .with_context(|| format!("{} does not parse", path.display()))
}

pub(crate) fn write_manifest(dir: &Path, m: &Manifest) -> Result<()> {
    write_atomic(dir, MANIFEST_JSON, &json_bytes(m)?)
}

/// Pretty JSON + a newline (`ranking.json`, `manifest.json`).
pub(crate) fn json_bytes<T: Serialize>(v: &T) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(v)?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// A `ranking.json` / `latest.json`.
pub(crate) fn read_ranking(path: &Path) -> Result<Ranking> {
    let raw = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&raw).with_context(|| format!("{} does not parse", path.display()))
}

/// The date `latest.json` holds; `None` when there is none (or it does not
/// read — the next COMPLETE date replaces it).
pub(crate) fn latest_date(state_dir: &Path, contract: &str) -> Option<NaiveDate> {
    read_ranking(&contract_dir(state_dir, contract).join(LATEST_JSON))
        .ok()
        .map(|r| r.date)
}

/// A published `ranking.md`: the date's, else `latest.md` — its path and
/// text; `None` when nothing is published there.
pub(crate) fn published_markdown(
    state_dir: &Path,
    contract: &str,
    date: Option<NaiveDate>,
) -> Result<Option<(PathBuf, String)>> {
    let path = match date {
        Some(d) => date_dir(state_dir, contract, d).join(RANKING_MD),
        None => contract_dir(state_dir, contract).join(LATEST_MD),
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(Some((path, text))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}
