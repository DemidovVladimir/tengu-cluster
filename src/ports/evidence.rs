//! Evidence IO (`tengu evidence`, `docs/lineage-2026-10-06.md` § 3) — what
//! the use cases in `application/evidence.rs` read and write. Every reader
//! is read-only by construction (SQLite `immutable=1` + read-only flags; no
//! store adapter that migrates or purges on open). Synchronous: CLI batch
//! work. Impls: `adapters/outbound/evidence/`.
//!
//! | Port | Does | Impl |
//! |---|---|---|
//! | [`Vault`] | the one write path: create `<TENGU_HOME>/state/evidence/<vault>/` once, inspect each source (kind, files, non-empty SQLite WALs), copy items in (sha256 of the copy = of the source), list + hash vault files, `MANIFEST.json`, `chmod a-w` | `evidence/vault.rs` |
//! | [`LedgerSource`] | every row of a paper `ledger.db` the grade reads (`domain::xm::grade::LedgerRows`), schema of binary 6fcb455 or later | `evidence/ledger_reader.rs` |
//! | [`RecordedHistory`] | recorder day files (`obs_history`) over one or more dirs: instants per schema, rows per key and time range | `evidence/recorded.rs` |
//! | [`BackfillSource`] | `market.db` bars and funding, read-only | `evidence/recorded.rs` |
//! | [`RunDirSource`] | a backtest run dir: `report.json` (run id, strategy), `candidates.jsonl`, `trades-research.jsonl`, `decisions.jsonl` (`tengu evidence evaluate`) | `evidence/run_dir.rs` |

use std::collections::BTreeSet;

use serde_json::Value;

use crate::domain::backtest::engine::{Candidate, Trade};
use crate::domain::evidence::{ItemKind, ManifestEntry};
use crate::domain::observation::{Features, ObsStatus};
use crate::domain::xm::grade::LedgerRows;

/// A source as found before the copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceInfo {
    pub kind: ItemKind,
    pub files: u64,
    pub bytes: u64,
    /// The source path, `~` expanded.
    pub path: String,
    /// Non-empty SQLite WALs: a FILE that is a SQLite database → its
    /// `<db>-wal` sibling; a DIR → every `*-wal` file under it. Their rows
    /// are invisible to the vault's immutable readers.
    pub live_wals: Vec<String>,
}

pub(crate) trait Vault {
    /// `<TENGU_HOME>/state/evidence/<vault>`, for messages.
    fn root_display(&self) -> String;
    /// The vault dir exists (a snapshot refuses it).
    fn exists(&self) -> bool;
    /// What `source` (`~/…` or absolute) is: a regular file or a dir of
    /// regular files only (a symlink or special file anywhere ⇒ `Err`), and
    /// the non-empty WALs it carries ([`SourceInfo::live_wals`]).
    fn inspect_source(&self, source: &str) -> anyhow::Result<SourceInfo>;
    /// `<vault>/<path>` is a directory (an item dir with no files).
    fn has_dir(&self, path: &str) -> bool;
    /// Create the vault dir; `Err` when it exists.
    fn create(&self) -> anyhow::Result<()>;
    /// Copy `source` to `<vault>/<path>`; one entry per copied file
    /// (vault-relative path, sha256 of the copy — checked equal to the
    /// source's — bytes).
    fn copy_in(
        &self,
        source: &str,
        kind: ItemKind,
        path: &str,
    ) -> anyhow::Result<Vec<ManifestEntry>>;
    /// Every regular file under the vault but `MANIFEST.json`,
    /// vault-relative, `/`-separated, sorted.
    fn list(&self) -> anyhow::Result<Vec<String>>;
    /// sha256 + bytes of one vault file.
    fn hash(&self, path: &str) -> anyhow::Result<ManifestEntry>;
    /// Write `MANIFEST.json` (new file only).
    fn write_manifest(&self, bytes: &[u8]) -> anyhow::Result<()>;
    /// `MANIFEST.json`'s bytes; `None` when absent.
    fn read_manifest(&self) -> anyhow::Result<Option<Vec<u8>>>;
    /// `chmod a-w` every file and dir of the vault (dirs last).
    fn seal(&self) -> anyhow::Result<()>;
}

pub(crate) trait LedgerSource {
    fn read(&self) -> anyhow::Result<LedgerRows>;
    /// For messages: which file, and which optional tables / columns it has.
    fn describe(&self) -> String;
}

/// One recorded row; `features` / `data` only when asked for.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RecordedRow {
    pub key: String,
    pub observed_at_ms: i64,
    pub status: ObsStatus,
    pub features: Features,
    pub data: Option<Value>,
    /// The row's `errors` JSON, verbatim.
    pub errors: Option<String>,
}

pub(crate) trait RecordedHistory {
    /// `(key, observed_at_ms, status)` of every `schema` row with `from ≤
    /// observed_at < to`, over every dir.
    fn instants(
        &self,
        schema: &str,
        from_ms: i64,
        to_ms: i64,
    ) -> anyhow::Result<Vec<(String, i64, ObsStatus)>>;
    /// Keys of `schema` with a row in `[from, to)`.
    fn keys(&self, schema: &str, from_ms: i64, to_ms: i64) -> anyhow::Result<BTreeSet<String>>;
    /// Rows of `key` with `from ≤ observed_at < to`, oldest first, with
    /// features (and `data` when `with_data`).
    fn rows(
        &self,
        key: &str,
        from_ms: i64,
        to_ms: i64,
        with_data: bool,
    ) -> anyhow::Result<Vec<RecordedRow>>;
    /// For messages: the dirs and day files read.
    fn describe(&self) -> String;
}

/// One backfilled bar.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct BackfilledBar {
    pub t_open_ms: i64,
    pub close: f64,
    pub trades: Option<i64>,
    pub fetched_at_ms: i64,
}

pub(crate) trait BackfillSource {
    /// Bars of `instrument` and `interval` (`1m`, `1h`) with `from ≤ t_open < to`.
    fn bars(
        &self,
        instrument: &str,
        interval: &str,
        from_ms: i64,
        to_ms: i64,
    ) -> anyhow::Result<Vec<BackfilledBar>>;
    /// Funding `(t_ms, rate_1h)` of `instrument` with `from ≤ t < to`.
    fn funding(
        &self,
        instrument: &str,
        from_ms: i64,
        to_ms: i64,
    ) -> anyhow::Result<Vec<(i64, f64)>>;
    fn describe(&self) -> String;
}

/// What `tengu evidence evaluate` reads of a gated backtest run dir.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RunFiles {
    /// `report.json` `run_id` / `strategy`.
    pub run_id: String,
    pub strategy: String,
    pub candidates: Vec<Candidate>,
    /// `trades-research.jsonl`: every candidate's outcome if taken.
    pub research: Vec<Trade>,
    /// `decisions.jsonl`, one JSON value per line.
    pub decisions: Vec<Value>,
    /// `trades-rules_capped.jsonl` + `trades-jev_capped.jsonl`: the two
    /// capped books (`[risk]` + `[paper]`), when the run had them.
    pub capped: Option<(Vec<Trade>, Vec<Trade>)>,
}

/// A backtest run dir, read-only (module table).
pub(crate) trait RunDirSource {
    fn read(&self) -> anyhow::Result<RunFiles>;
    /// The dir, for the output's header.
    fn describe(&self) -> String;
}
