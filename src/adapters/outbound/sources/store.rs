//! `SqliteSourceStore` — `ports::source_store::SourceStore` over ONE database
//! per `[sources]` state dir, `<state dir>/sources.db` (O2): outside every
//! workspace and fs root (load rule, `config/sources.rs`), mode 0600 for the
//! database, its WAL and the WAL index (raw bodies may hold contact data;
//! `sqlite.rs::open_wal`, as every store). WAL (`synchronous = NORMAL`: a power cut
//! may lose the last commit, which a re-fetch reads again — later, never
//! earlier) + busy_timeout; every call runs on `spawn_blocking`;
//! `user_version = 2` (2: + `switches`; a version-1 file gains the table on
//! open, an older binary refuses a version-2 file).
//!
//! | Table (`WITHOUT ROWID` but `purges`, `switches`) | Key | Columns | Writes |
//! |---|---|---|---|
//! | `snapshots` | sha256 | kind (`response` · `terms`), source_id, request_key, url, http_status, content_type, fetched_ms, bytes, body (NULL = not kept or purged), purged_ms | `INSERT OR IGNORE`; raw purge sets `body = NULL, purged_ms` |
//! | `records` | record_id | source_id, native_id, event_key, published_ms, observed_ms, parsed_ms, content_hash, parser_version, parse_status, body (the record's JSON) — indexed by item, event, read time | `INSERT OR IGNORE` only; record purge deletes |
//! | `record_entities` | (entity_id, record_id) | — | `INSERT OR IGNORE`; deleted with its record |
//! | `coverage` | (source_id, query_key, fetched_ms, from_ms, to_ms) | complete, error_class | `INSERT OR IGNORE` |
//! | `cursors` | (source_id, query_key) | value, updated_ms | the one upsert, inside a committed batch |
//! | `purges` | — (append log) | source_id, purged_ms, raw_before_ms, records_before_ms, snapshots, records, reason | `INSERT` |
//! | `switches` | — (append log) | source_id, enabled, at_ms, reason — the runtime kill switch (`tengu sources disable` / `enable`) | `INSERT` |
//!
//! The `records` key is `<source_id>:<native_id>:<content_hash>`, so it is
//! unique per `(source_id, native_id, content_hash)` (`source_id` has no `:`,
//! the hash is hex). Read rules: `ports/source_store.rs`.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use rusqlite::{params, Connection, OptionalExtension, Transaction};

use crate::adapters::outbound::sqlite::{open_wal, Durability, SharedConn};
use crate::config::sources::sources_db;
use crate::domain::canonical::sha256_hex;
use crate::domain::evidence::valid_sha256;
use crate::domain::source::record::valid_source_id;
use crate::domain::source::{Coverage, ParseStatus, Purge, SourceRecord};
use crate::ports::source_store::{
    Batch, CommitReport, Cursor, PurgeRequest, RecordQuery, Snapshot, SnapshotKind, SourceStore,
    SourceSwitch,
};

/// The schema version in `PRAGMA user_version`.
const SCHEMA_VERSION: i64 = 2;

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS snapshots (
  sha256 TEXT NOT NULL PRIMARY KEY, kind TEXT NOT NULL, source_id TEXT NOT NULL,
  request_key TEXT NOT NULL, url TEXT NOT NULL, http_status INTEGER NOT NULL,
  content_type TEXT NOT NULL, fetched_ms INTEGER NOT NULL, bytes INTEGER NOT NULL,
  body BLOB, purged_ms INTEGER) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS snapshots_by_read ON snapshots(source_id, kind, fetched_ms);
CREATE TABLE IF NOT EXISTS records (
  record_id TEXT NOT NULL PRIMARY KEY, source_id TEXT NOT NULL, native_id TEXT NOT NULL,
  event_key TEXT NOT NULL, published_ms INTEGER NOT NULL, observed_ms INTEGER NOT NULL,
  parsed_ms INTEGER NOT NULL, content_hash TEXT NOT NULL, parser_version TEXT NOT NULL,
  parse_status TEXT NOT NULL, body TEXT NOT NULL) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS records_by_item ON records(source_id, native_id);
CREATE INDEX IF NOT EXISTS records_by_event ON records(event_key);
CREATE INDEX IF NOT EXISTS records_by_read ON records(source_id, observed_ms);
CREATE TABLE IF NOT EXISTS record_entities (
  entity_id TEXT NOT NULL, record_id TEXT NOT NULL,
  PRIMARY KEY (entity_id, record_id)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS coverage (
  source_id TEXT NOT NULL, query_key TEXT NOT NULL, fetched_ms INTEGER NOT NULL,
  from_ms INTEGER NOT NULL, to_ms INTEGER NOT NULL, complete INTEGER NOT NULL, error_class TEXT,
  PRIMARY KEY (source_id, query_key, fetched_ms, from_ms, to_ms)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS cursors (
  source_id TEXT NOT NULL, query_key TEXT NOT NULL, value TEXT NOT NULL, updated_ms INTEGER NOT NULL,
  PRIMARY KEY (source_id, query_key)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS purges (
  source_id TEXT NOT NULL, purged_ms INTEGER NOT NULL, raw_before_ms INTEGER,
  records_before_ms INTEGER, snapshots INTEGER NOT NULL, records INTEGER NOT NULL,
  reason TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS switches (
  source_id TEXT NOT NULL, enabled INTEGER NOT NULL, at_ms INTEGER NOT NULL,
  reason TEXT NOT NULL);";

const PUT_SNAPSHOT_SQL: &str = "
INSERT OR IGNORE INTO snapshots(sha256, kind, source_id, request_key, url, http_status,
  content_type, fetched_ms, bytes, body, purged_ms)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, NULL)";

const PUT_RECORD_SQL: &str = "
INSERT OR IGNORE INTO records(record_id, source_id, native_id, event_key, published_ms,
  observed_ms, parsed_ms, content_hash, parser_version, parse_status, body)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)";

const PUT_ENTITY_SQL: &str =
    "INSERT OR IGNORE INTO record_entities(entity_id, record_id) VALUES (?1, ?2)";

const PUT_COVERAGE_SQL: &str = "
INSERT OR IGNORE INTO coverage(source_id, query_key, fetched_ms, from_ms, to_ms, complete,
  error_class) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)";

const MOVE_CURSOR_SQL: &str = "
INSERT INTO cursors(source_id, query_key, value, updated_ms) VALUES (?1, ?2, ?3, ?4)
ON CONFLICT(source_id, query_key) DO UPDATE SET value = excluded.value,
  updated_ms = excluded.updated_ms";

const PUT_PURGE_SQL: &str = "
INSERT INTO purges(source_id, purged_ms, raw_before_ms, records_before_ms, snapshots, records,
  reason) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)";

const PURGE_RAW_SQL: &str = "
UPDATE snapshots SET body = NULL, purged_ms = ?1
WHERE source_id = ?2 AND kind = 'response' AND fetched_ms < ?3 AND body IS NOT NULL";

const PURGE_ENTITIES_SQL: &str = "
DELETE FROM record_entities WHERE record_id IN
  (SELECT record_id FROM records WHERE source_id = ?1 AND observed_ms < ?2)";

const PURGE_RECORDS_SQL: &str = "DELETE FROM records WHERE source_id = ?1 AND observed_ms < ?2";

const SNAPSHOT_KNOWN_SQL: &str = "SELECT 1 FROM snapshots WHERE sha256 = ?1";

/// Seeds without an entity: `?1` upto, `?2` source (NULL = any), `?3` event.
const SEEDS_SQL: &str = "
SELECT r.record_id, r.source_id, r.native_id, r.event_key, r.body FROM records r
WHERE min(r.published_ms, r.observed_ms) <= ?1
  AND (?2 IS NULL OR r.source_id = ?2) AND (?3 IS NULL OR r.event_key = ?3)";

/// Seeds of one entity (`?4`).
const ENTITY_SEEDS_SQL: &str = "
SELECT r.record_id, r.source_id, r.native_id, r.event_key, r.body
FROM record_entities e JOIN records r ON r.record_id = e.record_id
WHERE e.entity_id = ?4 AND min(r.published_ms, r.observed_ms) <= ?1
  AND (?2 IS NULL OR r.source_id = ?2) AND (?3 IS NULL OR r.event_key = ?3)";

const ITEM_SQL: &str = "
SELECT r.record_id, r.source_id, r.native_id, r.event_key, r.body FROM records r
WHERE r.source_id = ?1 AND r.native_id = ?2";

const EVENT_SQL: &str = "
SELECT r.record_id, r.source_id, r.native_id, r.event_key, r.body FROM records r
WHERE r.event_key = ?1";

const COVERAGE_SQL: &str = "
SELECT source_id, query_key, fetched_ms, from_ms, to_ms, complete, error_class FROM coverage
WHERE (?1 IS NULL OR source_id = ?1)
ORDER BY source_id, query_key, fetched_ms, from_ms, to_ms";

const CURSOR_SQL: &str =
    "SELECT value, updated_ms FROM cursors WHERE source_id = ?1 AND query_key = ?2";

const SNAPSHOT_SQL: &str = "
SELECT sha256, kind, source_id, request_key, url, http_status, content_type, fetched_ms, bytes,
  body, purged_ms FROM snapshots WHERE sha256 = ?1";

const PURGES_SQL: &str = "
SELECT source_id, purged_ms, raw_before_ms, records_before_ms, snapshots, records, reason
FROM purges WHERE (?1 IS NULL OR source_id = ?1) ORDER BY purged_ms, rowid";

const CURSORS_SQL: &str = "
SELECT source_id, query_key, value, updated_ms FROM cursors
WHERE (?1 IS NULL OR source_id = ?1) ORDER BY source_id, query_key";

const PUT_SWITCH_SQL: &str =
    "INSERT INTO switches(source_id, enabled, at_ms, reason) VALUES (?1, ?2, ?3, ?4)";

const SWITCHES_SQL: &str = "
SELECT source_id, enabled, at_ms, reason FROM switches
WHERE (?1 IS NULL OR source_id = ?1) ORDER BY rowid";

/// Every statement, for the append-only check.
#[cfg(test)]
const ALL_SQL: [&str; 24] = [
    SCHEMA_SQL,
    PUT_SNAPSHOT_SQL,
    PUT_RECORD_SQL,
    PUT_ENTITY_SQL,
    PUT_COVERAGE_SQL,
    MOVE_CURSOR_SQL,
    PUT_PURGE_SQL,
    PURGE_RAW_SQL,
    PURGE_ENTITIES_SQL,
    PURGE_RECORDS_SQL,
    SNAPSHOT_KNOWN_SQL,
    SEEDS_SQL,
    ENTITY_SEEDS_SQL,
    ITEM_SQL,
    EVENT_SQL,
    COVERAGE_SQL,
    CURSOR_SQL,
    SNAPSHOT_SQL,
    PURGES_SQL,
    CURSORS_SQL,
    PUT_SWITCH_SQL,
    SWITCHES_SQL,
    "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA busy_timeout=5000;",
    "PRAGMA user_version",
];

pub(crate) struct SqliteSourceStore {
    conn: SharedConn,
}

impl SqliteSourceStore {
    /// Open (creating) `<state_dir>/sources.db`.
    pub(crate) fn open(state_dir: &Path) -> Result<Self> {
        let path: PathBuf = sources_db(state_dir);
        let conn = open_wal(&path, Durability::Normal)?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > SCHEMA_VERSION {
            bail!(
                "{} has schema version {version}; this binary reads {SCHEMA_VERSION} — use a newer tengu",
                path.display()
            );
        }
        conn.execute_batch(SCHEMA_SQL)
            .with_context(|| format!("schema of {}", path.display()))?;
        conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
        Ok(Self {
            conn: SharedConn::new(conn, "source store"),
        })
    }
}

/// Non-empty, no whitespace or control character.
fn token(s: &str) -> bool {
    !s.is_empty() && !s.chars().any(|c| c.is_whitespace() || c.is_control())
}

fn parse_status(p: ParseStatus) -> &'static str {
    match p {
        ParseStatus::Ok => "ok",
        ParseStatus::Partial => "partial",
        ParseStatus::Error => "error",
    }
}

fn i64_of(n: u64, what: &str) -> Result<i64> {
    i64::try_from(n).map_err(|_| anyhow!("{what} {n} is out of range"))
}

/// Every shape problem of `batch` that needs no database (module table).
fn batch_problems(batch: &Batch) -> Vec<String> {
    let mut out = Vec::new();
    for (i, s) in batch.snapshots.iter().enumerate() {
        let at = format!("snapshots[{i}] {}", s.sha256);
        if !valid_sha256(&s.sha256) {
            out.push(format!("{at}: sha256 is not 64 lowercase hex"));
        }
        if !valid_source_id(&s.source_id) {
            out.push(format!(
                "{at}: source_id `{}` is not [a-z0-9_]+",
                s.source_id
            ));
        }
        if !token(&s.request_key) {
            out.push(format!("{at}: request_key is empty or has whitespace"));
        }
        if !(token(&s.url) && (s.url.starts_with("https://") || s.url.starts_with("http://"))) {
            out.push(format!("{at}: url `{}` is not an http(s) URL", s.url));
        }
        if s.purged_ms.is_some() {
            out.push(format!(
                "{at}: purged_ms is set by a purge, never on commit"
            ));
        }
        if let Some(body) = &s.body {
            if sha256_hex(body) != s.sha256 {
                out.push(format!("{at}: the body's sha256 is {}", sha256_hex(body)));
            }
            if body.len() as u64 != s.bytes {
                out.push(format!(
                    "{at}: bytes {} but the body has {}",
                    s.bytes,
                    body.len()
                ));
            }
        }
    }
    for (i, r) in batch.records.iter().enumerate() {
        if let Err(problems) = r.validate() {
            for p in problems {
                out.push(format!("records[{i}] {}: {p}", r.record_id));
            }
        }
    }
    for (i, c) in batch.coverage.iter().enumerate() {
        let at = format!("coverage[{i}] {} {}", c.source_id, c.query_key);
        if !valid_source_id(&c.source_id) {
            out.push(format!("{at}: source_id is not [a-z0-9_]+"));
        }
        if !token(&c.query_key) {
            out.push(format!("{at}: query_key is empty or has whitespace"));
        }
        if c.from_ms > c.to_ms {
            out.push(format!(
                "{at}: from_ms {} is after to_ms {}",
                c.from_ms, c.to_ms
            ));
        }
        match (&c.error_class, c.complete) {
            (Some(_), true) => out.push(format!("{at}: a complete fetch has no error_class")),
            (Some(e), false) if !token(e) => {
                out.push(format!("{at}: error_class is empty or has whitespace"))
            }
            _ => {}
        }
    }
    if let Some(c) = &batch.cursor {
        if !valid_source_id(&c.source_id) || !token(&c.query_key) || !token(&c.value) {
            out.push(format!(
                "cursor {} {}: source_id [a-z0-9_]+, query_key and value without whitespace",
                c.source_id, c.query_key
            ));
        }
    }
    out
}

fn commit_tx(conn: &mut Connection, batch: &Batch) -> Result<CommitReport> {
    let mut problems = batch_problems(batch);
    let tx = conn.transaction()?;
    let in_batch: HashSet<&str> = batch.snapshots.iter().map(|s| s.sha256.as_str()).collect();
    {
        let mut known = tx.prepare_cached(SNAPSHOT_KNOWN_SQL)?;
        for (i, r) in batch.records.iter().enumerate() {
            for s in &r.snapshots {
                if !in_batch.contains(s.as_str()) && !known.exists(params![s])? {
                    problems.push(format!(
                        "records[{i}] {}: snapshot {s} is neither in the batch nor stored",
                        r.record_id
                    ));
                }
            }
        }
    }
    if !problems.is_empty() {
        bail!(
            "source batch refused, nothing written ({} problem(s)):\n  {}",
            problems.len(),
            problems.join("\n  ")
        );
    }
    let mut report = CommitReport::default();
    {
        let mut put = tx.prepare_cached(PUT_SNAPSHOT_SQL)?;
        for s in &batch.snapshots {
            report.snapshots_new += put.execute(params![
                s.sha256,
                s.kind.as_str(),
                s.source_id,
                s.request_key,
                s.url,
                s.http_status,
                s.content_type,
                s.fetched_ms,
                i64_of(s.bytes, "snapshot bytes")?,
                s.body,
            ])?;
        }
    }
    {
        let mut put = tx.prepare_cached(PUT_RECORD_SQL)?;
        let mut entity = tx.prepare_cached(PUT_ENTITY_SQL)?;
        for r in &batch.records {
            let body = serde_json::to_string(r)?;
            let new = put.execute(params![
                r.record_id,
                r.source_id,
                r.native_id,
                r.event_key,
                r.published_ms,
                r.observed_ms,
                r.parsed_ms,
                r.content_hash,
                r.parser_version,
                parse_status(r.parse),
                body,
            ])?;
            if new > 0 {
                report.records_new += new;
                for e in &r.entities {
                    entity.execute(params![e, r.record_id])?;
                }
            }
        }
    }
    {
        let mut put = tx.prepare_cached(PUT_COVERAGE_SQL)?;
        for c in &batch.coverage {
            report.coverage_new += put.execute(params![
                c.source_id,
                c.query_key,
                c.fetched_ms,
                c.from_ms,
                c.to_ms,
                c.complete,
                c.error_class,
            ])?;
        }
    }
    if let Some(c) = &batch.cursor {
        tx.execute(
            MOVE_CURSOR_SQL,
            params![c.source_id, c.query_key, c.value, c.updated_ms],
        )?;
        report.cursor_moved = true;
    }
    tx.commit()?;
    Ok(report)
}

/// One record row: (record_id, (source_id, native_id), event_key, body).
type Row = (String, (String, String), String, String);

fn rows(tx: &Transaction, sql: &str, p: impl rusqlite::Params) -> Result<Vec<Row>> {
    let mut stmt = tx.prepare_cached(sql)?;
    let out = stmt
        .query_map(p, |r| {
            Ok((
                r.get::<_, String>(0)?,
                (r.get::<_, String>(1)?, r.get::<_, String>(2)?),
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<Row>>>()?;
    Ok(out)
}

/// Seeds, then items and events to a fixed point (module table, port).
fn records_tx(conn: &mut Connection, q: &RecordQuery) -> Result<Vec<SourceRecord>> {
    let tx = conn.transaction()?;
    let seeds = match &q.entity {
        Some(e) => rows(
            &tx,
            ENTITY_SEEDS_SQL,
            params![q.upto_ms, q.source_id, q.event_key, e],
        )?,
        None => rows(&tx, SEEDS_SQL, params![q.upto_ms, q.source_id, q.event_key])?,
    };
    let mut bodies: BTreeMap<String, String> = BTreeMap::new();
    let mut items: BTreeSet<(String, String)> = BTreeSet::new();
    let mut events: BTreeSet<String> = BTreeSet::new();
    let (mut new_items, mut new_events) = (Vec::new(), Vec::new());
    let mut absorb =
        |found: Vec<Row>, new_items: &mut Vec<(String, String)>, new_events: &mut Vec<String>| {
            for (id, item, event, body) in found {
                if items.insert(item.clone()) {
                    new_items.push(item);
                }
                if events.insert(event.clone()) {
                    new_events.push(event);
                }
                bodies.entry(id).or_insert(body);
            }
        };
    absorb(seeds, &mut new_items, &mut new_events);
    while !(new_items.is_empty() && new_events.is_empty()) {
        let (items_now, events_now) = (
            std::mem::take(&mut new_items),
            std::mem::take(&mut new_events),
        );
        for (source, native) in items_now {
            let found = rows(&tx, ITEM_SQL, params![source, native])?;
            absorb(found, &mut new_items, &mut new_events);
        }
        for event in events_now {
            let found = rows(&tx, EVENT_SQL, params![event])?;
            absorb(found, &mut new_items, &mut new_events);
        }
    }
    let mut out = Vec::with_capacity(bodies.len());
    for (id, body) in bodies {
        let r: SourceRecord = serde_json::from_str(&body)
            .with_context(|| format!("stored record {id} does not decode"))?;
        if r.record_id != id {
            bail!("stored record {id} names itself {}", r.record_id);
        }
        out.push(r);
    }
    out.sort_by(|a, b| {
        (a.observed_ms, a.parsed_ms, &a.record_id).cmp(&(b.observed_ms, b.parsed_ms, &b.record_id))
    });
    Ok(out)
}

fn purge_tx(conn: &mut Connection, req: &PurgeRequest) -> Result<Purge> {
    if !valid_source_id(&req.source_id) {
        bail!("purge: source_id `{}` is not [a-z0-9_]+", req.source_id);
    }
    if req.reason.trim().is_empty() {
        bail!("purge: a reason is required (it stays on the tombstone)");
    }
    let tx = conn.transaction()?;
    let snapshots = match req.raw_before_ms {
        Some(before) => tx.execute(PURGE_RAW_SQL, params![req.now_ms, req.source_id, before])?,
        None => 0,
    };
    let records = match req.records_before_ms {
        Some(before) => {
            tx.execute(PURGE_ENTITIES_SQL, params![req.source_id, before])?;
            tx.execute(PURGE_RECORDS_SQL, params![req.source_id, before])?
        }
        None => 0,
    };
    let purge = Purge {
        source_id: req.source_id.clone(),
        purged_ms: req.now_ms,
        raw_before_ms: req.raw_before_ms,
        records_before_ms: req.records_before_ms,
        snapshots: snapshots as u64,
        records: records as u64,
        reason: req.reason.clone(),
    };
    if snapshots + records > 0 {
        tx.execute(
            PUT_PURGE_SQL,
            params![
                purge.source_id,
                purge.purged_ms,
                purge.raw_before_ms,
                purge.records_before_ms,
                i64_of(purge.snapshots, "purged snapshots")?,
                i64_of(purge.records, "purged records")?,
                purge.reason,
            ],
        )?;
    }
    tx.commit()?;
    Ok(purge)
}

#[async_trait]
impl SourceStore for SqliteSourceStore {
    async fn commit(&self, batch: Batch) -> Result<CommitReport> {
        if batch.is_empty() {
            return Ok(CommitReport::default());
        }
        self.conn.run(move |conn| commit_tx(conn, &batch)).await
    }

    async fn records(&self, query: &RecordQuery) -> Result<Vec<SourceRecord>> {
        let q = query.clone();
        self.conn.run(move |conn| records_tx(conn, &q)).await
    }

    async fn coverage(&self, source_id: Option<&str>) -> Result<Vec<Coverage>> {
        let source = source_id.map(str::to_string);
        self.conn
            .run(move |conn| {
                let mut stmt = conn.prepare_cached(COVERAGE_SQL)?;
                let out = stmt
                    .query_map(params![source], |r| {
                        Ok(Coverage {
                            source_id: r.get(0)?,
                            query_key: r.get(1)?,
                            fetched_ms: r.get(2)?,
                            from_ms: r.get(3)?,
                            to_ms: r.get(4)?,
                            complete: r.get(5)?,
                            error_class: r.get(6)?,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(out)
            })
            .await
    }

    async fn cursor(&self, source_id: &str, query_key: &str) -> Result<Option<Cursor>> {
        let (source, key) = (source_id.to_string(), query_key.to_string());
        self.conn
            .run(move |conn| {
                let row = conn
                    .query_row(CURSOR_SQL, params![source, key], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                    })
                    .optional()?;
                Ok(row.map(|(value, updated_ms)| Cursor {
                    source_id: source,
                    query_key: key,
                    value,
                    updated_ms,
                }))
            })
            .await
    }

    async fn cursors(&self, source_id: Option<&str>) -> Result<Vec<Cursor>> {
        let source = source_id.map(str::to_string);
        self.conn
            .run(move |conn| {
                let mut stmt = conn.prepare_cached(CURSORS_SQL)?;
                let out = stmt
                    .query_map(params![source], |r| {
                        Ok(Cursor {
                            source_id: r.get(0)?,
                            query_key: r.get(1)?,
                            value: r.get(2)?,
                            updated_ms: r.get(3)?,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(out)
            })
            .await
    }

    async fn snapshot(&self, sha256: &str) -> Result<Option<Snapshot>> {
        let sha = sha256.to_string();
        self.conn
            .run(move |conn| {
                let row = conn
                    .query_row(SNAPSHOT_SQL, params![sha], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, String>(3)?,
                            r.get::<_, String>(4)?,
                            r.get::<_, u16>(5)?,
                            r.get::<_, String>(6)?,
                            r.get::<_, i64>(7)?,
                            r.get::<_, i64>(8)?,
                            r.get::<_, Option<Vec<u8>>>(9)?,
                            r.get::<_, Option<i64>>(10)?,
                        ))
                    })
                    .optional()?;
                let Some((
                    sha256,
                    kind,
                    source_id,
                    request_key,
                    url,
                    http_status,
                    content_type,
                    fetched_ms,
                    bytes,
                    body,
                    purged_ms,
                )) = row
                else {
                    return Ok(None);
                };
                Ok(Some(Snapshot {
                    kind: SnapshotKind::parse(&kind)
                        .ok_or_else(|| anyhow!("snapshot {sha256}: unknown kind `{kind}`"))?,
                    sha256,
                    source_id,
                    request_key,
                    url,
                    http_status,
                    content_type,
                    fetched_ms,
                    bytes: u64::try_from(bytes).map_err(|_| anyhow!("snapshot bytes {bytes}"))?,
                    body,
                    purged_ms,
                }))
            })
            .await
    }

    async fn purge(&self, request: &PurgeRequest) -> Result<Purge> {
        let req = request.clone();
        self.conn.run(move |conn| purge_tx(conn, &req)).await
    }

    async fn purges(&self, source_id: Option<&str>) -> Result<Vec<Purge>> {
        let source = source_id.map(str::to_string);
        self.conn
            .run(move |conn| {
                let mut stmt = conn.prepare_cached(PURGES_SQL)?;
                let out = stmt
                    .query_map(params![source], |r| {
                        Ok(Purge {
                            source_id: r.get(0)?,
                            purged_ms: r.get(1)?,
                            raw_before_ms: r.get(2)?,
                            records_before_ms: r.get(3)?,
                            snapshots: r.get::<_, i64>(4)? as u64,
                            records: r.get::<_, i64>(5)? as u64,
                            reason: r.get(6)?,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(out)
            })
            .await
    }

    async fn set_switch(&self, switch: &SourceSwitch) -> Result<()> {
        if !valid_source_id(&switch.source_id) {
            bail!("switch: source_id `{}` is not [a-z0-9_]+", switch.source_id);
        }
        if switch.reason.trim().is_empty() {
            bail!("switch {}: a reason is required", switch.source_id);
        }
        let s = switch.clone();
        self.conn
            .run(move |conn| {
                conn.execute(
                    PUT_SWITCH_SQL,
                    params![s.source_id, s.enabled, s.at_ms, s.reason],
                )?;
                Ok(())
            })
            .await
    }

    async fn switches(&self, source_id: Option<&str>) -> Result<Vec<SourceSwitch>> {
        let source = source_id.map(str::to_string);
        self.conn
            .run(move |conn| {
                let mut stmt = conn.prepare_cached(SWITCHES_SQL)?;
                let out = stmt
                    .query_map(params![source], |r| {
                        Ok(SourceSwitch {
                            source_id: r.get(0)?,
                            enabled: r.get(1)?,
                            at_ms: r.get(2)?,
                            reason: r.get(3)?,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(out)
            })
            .await
    }
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
