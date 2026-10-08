//! `SourceStore` — the append-only store of the source layer (O2): raw
//! snapshots (content-addressed by sha256), source records
//! (`domain::source::SourceRecord`), coverage per fetch, cursors and purge
//! tombstones. Impl: `SqliteSourceStore` (`adapters/outbound/sources/store.rs`,
//! `<TENGU_HOME>/state/<sources.state>/sources.db` — never `market.db`). Fed
//! by the source fetchers (`adapters/outbound/sources/`), read by the as-of
//! view (`domain/source/asof.rs`) through `tengu sources` and the
//! `source_evidence` tool.
//!
//! | Call | Rule |
//! |---|---|
//! | `commit` | one transaction: snapshots, records, coverage and the cursor land together or not at all; every record `validate`d and every snapshot it names known (in the batch or stored); a body's sha256 checked — one bad row fails the batch, nothing written |
//! | append-only | a snapshot sha256 or record id already stored is kept as first stored (a re-read of the same content adds nothing, and an older read imported later never moves it earlier); coverage rows are appended |
//! | `records` | seeds = records matching every set filter with `min(published, observed) ≤ upto_ms` (nothing else can be visible at `upto_ms` in either clock); then every version of each seed's item and every record of each seed's event, to a fixed point — the as-of view needs the item's first read (knowable clock) and the event's corrections and copies (which may lack the entity). The as-of view makes the final cut |
//! | `records` fidelity | facts, events, conflicts, issues, demand and citations of a packet filtered like the seeds are exact; freshness counts (parse failures, partial parses) cover the whole source — seed by `source` (or nothing) for them |
//! | `cursor` | the only mutable row: one value per `(source_id, query_key)`, moved only inside a committed batch |
//! | `purge` | raw retention: `response` bodies fetched before `raw_before_ms` dropped (sha256, size and metadata kept; `terms` bodies never); record retention: records read before `records_before_ms` deleted; a purge that removed something leaves a tombstone ([`Purge`]) in the same transaction |

// The reads, the terms snapshot and the purge are called by `tengu sources`
// and the `source_evidence` tool (O2 C8–C9); until then by tests only.
#![cfg_attr(not(test), allow(dead_code))]

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::domain::source::{Coverage, Purge, SourceRecord};

/// What a raw snapshot holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SnapshotKind {
    /// A source reply a record was (or could be) parsed from.
    Response,
    /// The terms page the operator reviewed (`terms_sha256` of the registry
    /// row): kept whole, never purged.
    Terms,
}

impl SnapshotKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            SnapshotKind::Response => "response",
            SnapshotKind::Terms => "terms",
        }
    }

    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s {
            "response" => Some(SnapshotKind::Response),
            "terms" => Some(SnapshotKind::Terms),
            _ => None,
        }
    }
}

/// One raw body read from a source, content-addressed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Snapshot {
    /// sha256 of the body bytes, 64 lowercase hex — a record's `snapshots`.
    pub sha256: String,
    pub kind: SnapshotKind,
    pub source_id: String,
    /// What the read was for (`submissions:<cik10>`, `index:<accession>`,
    /// `terms`), in full.
    pub request_key: String,
    /// The URL read (never a secret: no key in any source URL).
    pub url: String,
    pub http_status: u16,
    pub content_type: String,
    pub fetched_ms: i64,
    /// Body size.
    pub bytes: u64,
    /// `None`: not kept (`store_raw = false`) or purged (`purged_ms`).
    pub body: Option<Vec<u8>>,
    /// When raw retention dropped the body; `None` on commit.
    pub purged_ms: Option<i64>,
}

/// sha256 of `bytes`, 64 lowercase hex.
pub(crate) fn body_sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

impl Snapshot {
    /// A body read at `fetched_ms`; `keep` = store the bytes (the registry
    /// row's `store_raw`), else only its sha256 and size.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn read(
        kind: SnapshotKind,
        source_id: &str,
        request_key: &str,
        url: &str,
        http_status: u16,
        content_type: &str,
        fetched_ms: i64,
        body: &[u8],
        keep: bool,
    ) -> Self {
        Self {
            sha256: body_sha256(body),
            kind,
            source_id: source_id.to_string(),
            request_key: request_key.to_string(),
            url: url.to_string(),
            http_status,
            content_type: content_type.to_string(),
            fetched_ms,
            bytes: body.len() as u64,
            body: keep.then(|| body.to_vec()),
            purged_ms: None,
        }
    }

    /// The reviewed terms page of `source_id` (critic U9): always kept.
    pub(crate) fn terms(source_id: &str, url: &str, fetched_ms: i64, body: &[u8]) -> Self {
        Self::read(
            SnapshotKind::Terms,
            source_id,
            "terms",
            url,
            200,
            "text/html",
            fetched_ms,
            body,
            true,
        )
    }
}

/// The cursor of one source query: where an incremental read resumes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Cursor {
    pub source_id: String,
    /// The query (`cik:<cik10>`, the sha256 of a query template), in full.
    pub query_key: String,
    /// Source-specific (SEC: the newest accession), in full.
    pub value: String,
    pub updated_ms: i64,
}

/// What one commit writes (module table: `commit`).
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Batch {
    pub snapshots: Vec<Snapshot>,
    pub records: Vec<SourceRecord>,
    pub coverage: Vec<Coverage>,
    /// Moves only if the whole batch commits.
    pub cursor: Option<Cursor>,
}

impl Batch {
    pub(crate) fn is_empty(&self) -> bool {
        self.snapshots.is_empty()
            && self.records.is_empty()
            && self.coverage.is_empty()
            && self.cursor.is_none()
    }
}

/// What a commit added (rows already stored are not counted).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub(crate) struct CommitReport {
    pub snapshots_new: usize,
    pub records_new: usize,
    pub coverage_new: usize,
    pub cursor_moved: bool,
}

/// Which records to read (module table: `records`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RecordQuery {
    pub source_id: Option<String>,
    /// `sec:cik:<10 digits>`, `ted:buyer:<country>:<id>`.
    pub entity: Option<String>,
    pub event_key: Option<String>,
    /// Seeds have `min(published_ms, observed_ms) ≤ upto_ms`.
    pub upto_ms: i64,
}

/// One retention purge (module table: `purge`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PurgeRequest {
    pub source_id: String,
    /// Drop `response` bodies fetched before it.
    pub raw_before_ms: Option<i64>,
    /// Delete records read (`observed_ms`) before it.
    pub records_before_ms: Option<i64>,
    pub now_ms: i64,
    /// Why (`raw_retention_days = 90`), kept on the tombstone.
    pub reason: String,
}

#[async_trait]
pub(crate) trait SourceStore: Send + Sync {
    /// Write `batch` in one transaction (module table).
    async fn commit(&self, batch: Batch) -> anyhow::Result<CommitReport>;
    /// Records for an as-of view at `query.upto_ms` (module table), sorted
    /// by `(observed_ms, parsed_ms, record_id)`.
    async fn records(&self, query: &RecordQuery) -> anyhow::Result<Vec<SourceRecord>>;
    /// Coverage rows of `source_id` (all sources when `None`), sorted by
    /// source, query, fetch time, span.
    async fn coverage(&self, source_id: Option<&str>) -> anyhow::Result<Vec<Coverage>>;
    async fn cursor(&self, source_id: &str, query_key: &str) -> anyhow::Result<Option<Cursor>>;
    /// A snapshot's metadata and body (when kept and not purged).
    async fn snapshot(&self, sha256: &str) -> anyhow::Result<Option<Snapshot>>;
    /// Apply one retention purge (module table); returns what it removed
    /// (counts `0` and no tombstone when nothing matched).
    async fn purge(&self, request: &PurgeRequest) -> anyhow::Result<Purge>;
    /// Tombstones of `source_id` (all when `None`), oldest first.
    async fn purges(&self, source_id: Option<&str>) -> anyhow::Result<Vec<Purge>>;
}
