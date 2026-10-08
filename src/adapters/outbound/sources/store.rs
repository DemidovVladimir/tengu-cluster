//! `SqliteSourceStore` — `ports::source_store::SourceStore` over ONE database
//! per `[sources]` state dir, `<state dir>/sources.db` (O2): outside every
//! workspace and fs root (load rule, `config/sources.rs`), file mode 0600 (raw
//! bodies may hold contact data). WAL (`synchronous = NORMAL`: a power cut
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
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use rusqlite::{params, Connection, OptionalExtension, Transaction};

use crate::config::sources::sources_db;
use crate::domain::evidence::valid_sha256;
use crate::domain::source::record::valid_source_id;
use crate::domain::source::{Coverage, ParseStatus, Purge, SourceRecord};
use crate::ports::source_store::{
    body_sha256, Batch, CommitReport, Cursor, PurgeRequest, RecordQuery, Snapshot, SnapshotKind,
    SourceStore, SourceSwitch,
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
WHERE (?1 IS NULL OR source_id = ?1) ORDER BY at_ms, rowid";

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
    conn: Arc<Mutex<Connection>>,
}

impl SqliteSourceStore {
    /// Open (creating) `<state_dir>/sources.db`.
    pub(crate) fn open(state_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(state_dir)
            .with_context(|| format!("create {}", state_dir.display()))?;
        let path: PathBuf = sources_db(state_dir);
        let conn = Connection::open(&path).with_context(|| format!("open {}", path.display()))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA busy_timeout=5000;",
        )?;
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
        owner_only(&path)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    async fn with_conn<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let mut guard = conn.lock().map_err(|e| anyhow!("source store lock: {e}"))?;
            f(&mut guard)
        })
        .await
        .context("source store task")?
    }
}

/// `chmod 600` (raw bodies may hold contact data).
fn owner_only(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod 600 {}", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
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
            if body_sha256(body) != s.sha256 {
                out.push(format!("{at}: the body's sha256 is {}", body_sha256(body)));
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
        self.with_conn(move |conn| commit_tx(conn, &batch)).await
    }

    async fn records(&self, query: &RecordQuery) -> Result<Vec<SourceRecord>> {
        let q = query.clone();
        self.with_conn(move |conn| records_tx(conn, &q)).await
    }

    async fn coverage(&self, source_id: Option<&str>) -> Result<Vec<Coverage>> {
        let source = source_id.map(str::to_string);
        self.with_conn(move |conn| {
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
        self.with_conn(move |conn| {
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
        self.with_conn(move |conn| {
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
        self.with_conn(move |conn| {
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
        self.with_conn(move |conn| purge_tx(conn, &req)).await
    }

    async fn purges(&self, source_id: Option<&str>) -> Result<Vec<Purge>> {
        let source = source_id.map(str::to_string);
        self.with_conn(move |conn| {
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
        self.with_conn(move |conn| {
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
        self.with_conn(move |conn| {
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
mod tests {
    use serde_json::Value;

    use super::*;
    use crate::domain::canonical::canonical_json;
    use crate::domain::marketdata::fmt_time;
    use crate::domain::source::record::{sec_cik_entity, Origin};
    use crate::domain::source::testkit::{copy_of, edited, rec, worlds, Src, D, H, T0};
    use crate::domain::source::{AsOfInput, AsOfMode, AsOfQuery, EvidencePacket};
    use crate::ports::source_store::switched_off;

    fn open() -> (tempfile::TempDir, SqliteSourceStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSourceStore::open(dir.path()).unwrap();
        (dir, store)
    }

    /// A metadata-only snapshot row for every sha256 `records` name.
    fn snapshots_of(records: &[SourceRecord]) -> Vec<Snapshot> {
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        for r in records {
            for s in &r.snapshots {
                if seen.insert(s.clone()) {
                    out.push(Snapshot {
                        sha256: s.clone(),
                        kind: SnapshotKind::Response,
                        source_id: r.source_id.clone(),
                        request_key: format!("test:{}", r.native_id),
                        url: r.url.clone(),
                        http_status: 200,
                        content_type: "application/json".into(),
                        fetched_ms: r.observed_ms,
                        bytes: 0,
                        body: None,
                        purged_ms: None,
                    });
                }
            }
        }
        out
    }

    fn batch(records: Vec<SourceRecord>) -> Batch {
        Batch {
            snapshots: snapshots_of(&records),
            records,
            ..Default::default()
        }
    }

    fn everything(upto_ms: i64) -> RecordQuery {
        RecordQuery {
            upto_ms,
            ..Default::default()
        }
    }

    fn json(r: &SourceRecord) -> String {
        serde_json::to_string(r).unwrap()
    }

    #[tokio::test]
    async fn records_round_trip_with_full_ids() {
        let (_dir, store) = open();
        let sec = rec(Src::SEC, "0000000001-26-000001", T0, T0 + H, T0 + H + 5);
        let ted = rec(Src::TED, "600001-2026", T0 + D, T0 + D + H, T0 + D + H);
        let wire = copy_of(&sec, Src::WIRE, "wire:story:0001", T0 + 2 * H, T0 + 3 * H);
        let report = store
            .commit(batch(vec![sec.clone(), ted.clone(), wire.clone()]))
            .await
            .unwrap();
        assert_eq!(
            report,
            CommitReport {
                snapshots_new: 3,
                records_new: 3,
                coverage_new: 0,
                cursor_moved: false
            }
        );
        let back = store.records(&everything(i64::MAX)).await.unwrap();
        assert_eq!(
            back,
            vec![sec.clone(), wire.clone(), ted.clone()],
            "by read time"
        );
        for r in &back {
            assert_eq!(r.validate(), Ok(()));
        }
        let text: Vec<String> = back.iter().map(json).collect();
        for whole in [
            sec.record_id.as_str(),
            "0000000001-26-000001",
            "sec:cik:0000000001",
            "00000000-0000-4000-8000-000000000000",
            ted.content_hash.as_str(),
            wire.origin.as_ref().unwrap().native_id.as_deref().unwrap(),
        ] {
            assert!(
                text.iter().any(|t| t.contains(whole)),
                "`{whole}` not whole"
            );
        }
        // By entity, source and event; and the snapshot rows.
        let q = RecordQuery {
            entity: Some(sec_cik_entity("0000000001")),
            ..everything(i64::MAX)
        };
        assert_eq!(
            store.records(&q).await.unwrap(),
            vec![sec.clone(), wire.clone()]
        );
        let q = RecordQuery {
            source_id: Some("ted_search".into()),
            ..everything(i64::MAX)
        };
        assert_eq!(store.records(&q).await.unwrap(), vec![ted.clone()]);
        let s = store.snapshot(&sec.snapshots[0]).await.unwrap().unwrap();
        assert_eq!(
            (s.sha256.as_str(), s.source_id.as_str(), s.body),
            (sec.snapshots[0].as_str(), "sec_edgar", None)
        );
        assert_eq!(store.snapshot(&"0".repeat(64)).await.unwrap(), None);
    }

    #[tokio::test]
    async fn refetch_of_the_same_content_adds_nothing() {
        let (_dir, store) = open();
        let r = rec(Src::SEC, "0000000001-26-000001", T0, T0 + H, T0 + H);
        store.commit(batch(vec![r.clone()])).await.unwrap();
        // The same batch again, and a re-read a day later (new bytes, a
        // newer parser, the same content): no record added.
        let again = store.commit(batch(vec![r.clone()])).await.unwrap();
        assert_eq!(again, CommitReport::default());
        let mut later = r.clone();
        later.observed_ms += D;
        later.parsed_ms += D;
        later.parser_version = "sec-submissions/2".into();
        later.snapshots = vec![crate::domain::canonical::sha256_hex("re-read body")];
        let later = later.with_identity();
        assert_eq!(later.record_id, r.record_id);
        let report = store.commit(batch(vec![later.clone()])).await.unwrap();
        assert_eq!((report.snapshots_new, report.records_new), (1, 0));
        // An older read of the same content imported later never moves the
        // stored read earlier (later = conservative in both clocks).
        let mut older = r.clone();
        older.observed_ms -= H;
        older.parsed_ms -= H;
        let report = store
            .commit(batch(vec![older.with_identity()]))
            .await
            .unwrap();
        assert_eq!(report.records_new, 0);
        let back = store.records(&everything(i64::MAX)).await.unwrap();
        assert_eq!(back, vec![r]);
    }

    #[tokio::test]
    async fn changed_content_appends_a_version() {
        let (_dir, store) = open();
        let r = rec(Src::REPO, "item-1", T0, T0 + H, T0 + H);
        store.commit(batch(vec![r.clone()])).await.unwrap();
        let before = json(&store.records(&everything(i64::MAX)).await.unwrap()[0]);
        let e = edited(&r, T0 + D, "release r1");
        let report = store.commit(batch(vec![e.clone()])).await.unwrap();
        assert_eq!(report.records_new, 1);
        let back = store.records(&everything(i64::MAX)).await.unwrap();
        assert_eq!(back, vec![r.clone(), e.clone()]);
        assert_eq!(
            json(&back[0]),
            before,
            "the first version is byte-identical"
        );
        assert_eq!(
            (back[0].native_id.as_str(), back[1].native_id.as_str()),
            ("item-1", "item-1")
        );
        assert_ne!(back[0].record_id, back[1].record_id);
    }

    #[tokio::test]
    async fn cursor_moves_only_with_a_committed_batch() {
        let (_dir, store) = open();
        let cursor = |value: &str| Cursor {
            source_id: "sec_edgar".into(),
            query_key: "cik:0000000001".into(),
            value: value.into(),
            updated_ms: T0,
        };
        let cov = Coverage {
            source_id: "sec_edgar".into(),
            query_key: "cik:0000000001".into(),
            fetched_ms: T0 + 2 * H,
            from_ms: T0 - D,
            to_ms: T0 + 2 * H,
            complete: true,
            error_class: None,
        };
        let a = rec(Src::SEC, "0000000001-26-000001", T0, T0 + H, T0 + H);
        let report = store
            .commit(Batch {
                cursor: Some(cursor("0000000001-26-000001")),
                coverage: vec![cov.clone()],
                ..batch(vec![a.clone()])
            })
            .await
            .unwrap();
        assert!(report.cursor_moved && report.coverage_new == 1);
        assert_eq!(
            store.cursor("sec_edgar", "cik:0000000001").await.unwrap(),
            Some(cursor("0000000001-26-000001"))
        );

        // One bad record fails the whole batch: no record, coverage or cursor.
        let b = rec(
            Src::SEC,
            "0000000001-26-000002",
            T0 + D,
            T0 + D + H,
            T0 + D + H,
        );
        let mut bad = rec(
            Src::SEC,
            "0000000001-26-000003",
            T0 + D,
            T0 + D + H,
            T0 + D + H,
        );
        bad.published_ms += 1; // content changed, identity not recomputed
        let cov2 = Coverage {
            fetched_ms: T0 + D + 2 * H,
            ..cov.clone()
        };
        let e = store
            .commit(Batch {
                cursor: Some(cursor("0000000001-26-000003")),
                coverage: vec![cov2.clone()],
                ..batch(vec![b.clone(), bad.clone()])
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("nothing written") && e.contains("content_hash"),
            "{e}"
        );
        assert_eq!(
            store
                .cursor("sec_edgar", "cik:0000000001")
                .await
                .unwrap()
                .unwrap()
                .value,
            "0000000001-26-000001"
        );
        assert_eq!(
            store.records(&everything(i64::MAX)).await.unwrap(),
            vec![a.clone()]
        );
        assert_eq!(store.coverage(None).await.unwrap(), vec![cov.clone()]);

        // A record naming a snapshot the store does not know; a body that
        // is not its sha256; a complete fetch with an error class.
        let e = store
            .commit(Batch {
                records: vec![b.clone()],
                ..Default::default()
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("neither in the batch nor stored"), "{e}");
        let mut lie = Snapshot::read(
            SnapshotKind::Response,
            "sec_edgar",
            "submissions:0000000001",
            "https://data.sec.gov/submissions/CIK0000000001.json",
            200,
            "application/json",
            T0,
            b"{}",
            true,
        );
        lie.body = Some(b"[]".to_vec());
        let e = store
            .commit(Batch {
                snapshots: vec![lie],
                coverage: vec![Coverage {
                    error_class: Some("fatal".into()),
                    ..cov2
                }],
                ..Default::default()
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("the body's sha256") && e.contains("no error_class"),
            "{e}"
        );
        // The good part alone commits; the cursor then moves.
        store
            .commit(Batch {
                cursor: Some(cursor("0000000001-26-000002")),
                ..batch(vec![b.clone()])
            })
            .await
            .unwrap();
        assert_eq!(
            store
                .cursor("sec_edgar", "cik:0000000001")
                .await
                .unwrap()
                .unwrap()
                .value,
            "0000000001-26-000002"
        );
    }

    #[tokio::test]
    async fn purge_leaves_tombstones_and_hashes() {
        let (_dir, store) = open();
        let body = |s: &str| s.as_bytes().to_vec();
        let old_body = Snapshot::read(
            SnapshotKind::Response,
            "sec_edgar",
            "index:0000000001-26-000001",
            "https://www.sec.gov/x-index.htm",
            200,
            "text/html",
            T0,
            &body("old page, Jane Doe +1 555 0100"),
            true,
        );
        let new_body = Snapshot::read(
            SnapshotKind::Response,
            "sec_edgar",
            "index:0000000001-26-000002",
            "https://www.sec.gov/y-index.htm",
            200,
            "text/html",
            T0 + 2 * D,
            &body("new page"),
            true,
        );
        let terms = Snapshot::terms(
            "sec_edgar",
            "https://www.sec.gov/terms",
            T0 - D,
            b"terms v1",
        );
        let mut old = rec(Src::SEC, "0000000001-26-000001", T0, T0, T0);
        old.snapshots = vec![old_body.sha256.clone()];
        let old = old.with_identity();
        let mut new = rec(
            Src::SEC,
            "0000000001-26-000002",
            T0 + 2 * D,
            T0 + 2 * D,
            T0 + 2 * D,
        );
        new.snapshots = vec![new_body.sha256.clone()];
        let new = new.with_identity();
        store
            .commit(Batch {
                snapshots: vec![old_body.clone(), new_body.clone(), terms.clone()],
                records: vec![old.clone(), new.clone()],
                ..Default::default()
            })
            .await
            .unwrap();

        // Raw retention: the old body goes; its hash, size and record stay;
        // the terms page is never purged.
        let now = T0 + 3 * D;
        let raw = store
            .purge(&PurgeRequest {
                source_id: "sec_edgar".into(),
                raw_before_ms: Some(T0 + D),
                records_before_ms: None,
                now_ms: now,
                reason: "raw_retention_days = 1".into(),
            })
            .await
            .unwrap();
        assert_eq!((raw.snapshots, raw.records), (1, 0));
        let gone = store.snapshot(&old_body.sha256).await.unwrap().unwrap();
        assert_eq!((gone.body, gone.purged_ms), (None, Some(now)));
        assert_eq!(
            (gone.sha256.as_str(), gone.bytes),
            (old_body.sha256.as_str(), old_body.bytes)
        );
        let kept = store.snapshot(&terms.sha256).await.unwrap().unwrap();
        assert_eq!(
            (kept.kind, kept.body.as_deref()),
            (SnapshotKind::Terms, Some(&b"terms v1"[..]))
        );
        assert_eq!(
            store
                .snapshot(&new_body.sha256)
                .await
                .unwrap()
                .unwrap()
                .body,
            new_body.body
        );
        let back = store.records(&everything(i64::MAX)).await.unwrap();
        assert_eq!(
            back,
            vec![old.clone(), new.clone()],
            "records keep their hashes"
        );

        // Record retention: the old record and its entity rows go.
        let recs = store
            .purge(&PurgeRequest {
                source_id: "sec_edgar".into(),
                raw_before_ms: None,
                records_before_ms: Some(T0 + D),
                now_ms: now + H,
                reason: "record_retention_days = 1".into(),
            })
            .await
            .unwrap();
        assert_eq!((recs.snapshots, recs.records), (0, 1));
        let q = RecordQuery {
            entity: Some(old.entities[0].clone()),
            ..everything(i64::MAX)
        };
        assert_eq!(store.records(&q).await.unwrap(), vec![new.clone()]);
        // A purge that removes nothing leaves no tombstone; a reason is required.
        let none = store
            .purge(&PurgeRequest {
                source_id: "sec_edgar".into(),
                raw_before_ms: Some(T0 + D),
                records_before_ms: Some(T0 + D),
                now_ms: now + 2 * H,
                reason: "again".into(),
            })
            .await
            .unwrap();
        assert_eq!((none.snapshots, none.records), (0, 0));
        assert!(store
            .purge(&PurgeRequest {
                source_id: "sec_edgar".into(),
                raw_before_ms: Some(T0),
                records_before_ms: None,
                now_ms: now,
                reason: " ".into(),
            })
            .await
            .is_err());
        let tombs = store.purges(None).await.unwrap();
        assert_eq!(tombs, vec![raw, recs]);
        assert!(store.purges(Some("ted_search")).await.unwrap().is_empty());
        assert_eq!(store.purges(Some("sec_edgar")).await.unwrap().len(), 2);
    }

    /// Append-only by construction: no statement replaces a row, none
    /// updates a record; records are inserted `OR IGNORE` and deleted only
    /// by the record purge.
    #[test]
    fn no_replace_or_update_on_records_sql() {
        for sql in ALL_SQL {
            let s = sql.to_ascii_uppercase();
            let words: BTreeSet<&str> = s
                .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .filter(|w| !w.is_empty())
                .collect();
            assert!(!words.contains("REPLACE"), "{sql}");
            if words.contains("RECORDS") || words.contains("RECORD_ENTITIES") {
                assert!(!words.contains("UPDATE"), "{sql}");
            }
            if words.contains("INSERT") {
                assert!(
                    s.contains("INSERT OR IGNORE")
                        || s.contains("INTO CURSORS")
                        || s.contains("INTO PURGES")
                        || s.contains("INTO SWITCHES"),
                    "{sql}"
                );
            }
            if words.contains("UPDATE") {
                assert!(
                    sql == PURGE_RAW_SQL || sql == MOVE_CURSOR_SQL,
                    "only the raw purge and the cursor update: {sql}"
                );
            }
            if words.contains("DELETE") {
                assert!(
                    sql == PURGE_RECORDS_SQL || sql == PURGE_ENTITIES_SQL,
                    "only the record purge deletes: {sql}"
                );
            }
        }
        assert!(PUT_RECORD_SQL.contains("INSERT OR IGNORE INTO records"));
        assert!(PUT_SNAPSHOT_SQL.contains("INSERT OR IGNORE INTO snapshots"));
        assert!(
            PURGE_RAW_SQL.contains("kind = 'response'"),
            "terms pages stay"
        );
    }

    /// A read at t keeps whole items and events: the first read of an item
    /// (the knowable clock reads it) and a report of the event that lacks
    /// the queried entity both come along.
    #[tokio::test]
    async fn a_read_at_t_brings_whole_items_and_events() {
        let (_dir, store) = open();
        let t = T0 + 10 * D;
        // First read after t, published after t; a later version names an
        // earlier publication (an edit under an immutable id).
        let first = rec(Src::SEC, "0000000001-26-000009", t + 5, t + 10, t + 10);
        let mut edit = edited(&first, t + 20, "Other events, restated");
        edit.published_ms = t - 5;
        edit.valid_from_ms = t - 5;
        let edit = edit.with_identity();
        // An outlet's report of another filing, without the CIK entity.
        let filing = rec(Src::SEC, "0000000001-26-000001", T0, T0 + H, T0 + H);
        let mut report = copy_of(&filing, Src::DAILY, "daily:0001", T0 + 2 * H, T0 + 3 * H);
        report.origin = None::<Origin>;
        report.entities = vec!["daily:desk:markets".into()];
        let report = report.with_identity();
        let all_records = vec![first.clone(), edit.clone(), filing.clone(), report.clone()];
        store.commit(batch(all_records.clone())).await.unwrap();

        let q = RecordQuery {
            entity: Some(sec_cik_entity("0000000001")),
            ..everything(t)
        };
        let read = store.records(&q).await.unwrap();
        for r in [&first, &edit, &filing, &report] {
            assert!(read.contains(r), "{} missing", r.record_id);
        }
        let policies = crate::domain::source::testkit::policies(&[Src::SEC, Src::DAILY]);
        for mode in [AsOfMode::Captured, AsOfMode::Knowable] {
            let input = AsOfInput {
                records: &read,
                coverage: &[],
                purges: &[],
                policies: &policies,
            };
            let p = EvidencePacket::build(&input, t, mode, &AsOfQuery::default());
            assert!(
                p.facts.iter().all(|f| f.record_id != edit.record_id),
                "{mode:?}: the edit read after t leaked"
            );
            let whole = AsOfInput {
                records: &all_records,
                ..input
            };
            assert_eq!(
                canonical_json(&serde_json::to_value(&p).unwrap()),
                canonical_json(
                    &serde_json::to_value(EvidencePacket::build(
                        &whole,
                        t,
                        mode,
                        &AsOfQuery::default()
                    ))
                    .unwrap()
                )
            );
        }
    }

    fn canon(p: &EvidencePacket) -> Value {
        serde_json::from_str(&canonical_json(&serde_json::to_value(p).unwrap())).unwrap()
    }

    /// The store's reads give the packet the whole store gives, for the
    /// testkit worlds: every source query exactly; an entity or event
    /// query in everything but freshness (port table: fidelity).
    #[tokio::test]
    async fn store_reads_give_the_packet_of_the_whole_store() {
        for w in worlds() {
            let (_dir, store) = open();
            let mut records = w.records.clone();
            records.sort_by_key(|r| (r.observed_ms, r.parsed_ms));
            store
                .commit(Batch {
                    coverage: w.coverage.clone(),
                    ..batch(records)
                })
                .await
                .unwrap();
            let coverage = store.coverage(None).await.unwrap();
            let mut want = w.coverage.clone();
            want.sort_by(|a, b| {
                (&a.source_id, &a.query_key, a.fetched_ms, a.from_ms, a.to_ms).cmp(&(
                    &b.source_id,
                    &b.query_key,
                    b.fetched_ms,
                    b.from_ms,
                    b.to_ms,
                ))
            });
            assert_eq!(coverage, want, "{}: coverage round-trips", w.name);
            let entities: Vec<String> = w
                .records
                .iter()
                .flat_map(|r| r.entities.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .take(3)
                .collect();
            let events: Vec<String> = w
                .records
                .iter()
                .map(|r| r.event_key.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .step_by(5)
                .take(3)
                .collect();
            let (a, b) = w.span;
            for i in 0..=8 {
                let t = a + (b - a) * i / 8;
                let mut cases: Vec<(AsOfQuery, RecordQuery, bool)> =
                    vec![(AsOfQuery::default(), everything(t), true)];
                for s in w.policies.keys() {
                    cases.push((
                        AsOfQuery {
                            source: Some(s.clone()),
                            ..Default::default()
                        },
                        RecordQuery {
                            source_id: Some(s.clone()),
                            ..everything(t)
                        },
                        true,
                    ));
                }
                for e in &entities {
                    cases.push((
                        AsOfQuery {
                            entity: Some(e.clone()),
                            ..Default::default()
                        },
                        RecordQuery {
                            entity: Some(e.clone()),
                            ..everything(t)
                        },
                        false,
                    ));
                }
                for k in &events {
                    cases.push((
                        AsOfQuery {
                            event_key: Some(k.clone()),
                            ..Default::default()
                        },
                        RecordQuery {
                            event_key: Some(k.clone()),
                            ..everything(t)
                        },
                        false,
                    ));
                }
                for (packet_q, store_q, exact) in &cases {
                    let read = store.records(store_q).await.unwrap();
                    let input = AsOfInput {
                        records: &read,
                        coverage: &coverage,
                        purges: &w.purges,
                        policies: &w.policies,
                    };
                    for mode in [AsOfMode::Captured, AsOfMode::Knowable] {
                        let mut got = canon(&EvidencePacket::build(&input, t, mode, packet_q));
                        let mut whole =
                            canon(&EvidencePacket::build(&w.input(), t, mode, packet_q));
                        if !exact {
                            got["freshness"] = Value::Null;
                            whole["freshness"] = Value::Null;
                        }
                        assert!(
                            got == whole,
                            "{} at {} {mode:?} {packet_q:?}: the store read differs",
                            w.name,
                            fmt_time(t)
                        );
                    }
                }
            }
            assert!(w.records.len() > 10, "{}: not vacuous", w.name);
        }
    }

    /// Critic U10: the runtime kill switch is an append log — every row
    /// kept, the newest per source wins; a version-1 file gains the table.
    #[tokio::test]
    async fn switches_append_and_the_newest_wins() {
        let (dir, store) = open();
        let row = |enabled: bool, at_ms: i64, reason: &str| SourceSwitch {
            source_id: "ted_search".into(),
            enabled,
            at_ms,
            reason: reason.into(),
        };
        assert!(store.switches(None).await.unwrap().is_empty());
        store
            .set_switch(&row(false, T0, "terms under review"))
            .await
            .unwrap();
        let rows = store.switches(Some("ted_search")).await.unwrap();
        assert_eq!(
            switched_off(&rows, "ted_search").map(|s| s.reason.as_str()),
            Some("terms under review")
        );
        assert_eq!(switched_off(&rows, "sec_edgar"), None);
        store
            .set_switch(&row(true, T0 + H, "reviewed"))
            .await
            .unwrap();
        let rows = store.switches(None).await.unwrap();
        assert_eq!(
            rows,
            vec![
                row(false, T0, "terms under review"),
                row(true, T0 + H, "reviewed")
            ]
        );
        assert_eq!(switched_off(&rows, "ted_search"), None);
        for bad in [
            SourceSwitch {
                source_id: "TED".into(),
                ..row(false, T0, "x")
            },
            row(false, T0, " "),
        ] {
            assert!(store.set_switch(&bad).await.is_err(), "{bad:?}");
        }
        assert_eq!(store.switches(None).await.unwrap().len(), 2);
        // Cursors list by source.
        assert!(store.cursors(None).await.unwrap().is_empty());
        let a = rec(Src::SEC, "0000000001-26-000001", T0, T0 + H, T0 + H);
        let cursor = Cursor {
            source_id: "sec_edgar".into(),
            query_key: "cik:0000000001".into(),
            value: "0000000001-26-000001".into(),
            updated_ms: T0,
        };
        store
            .commit(Batch {
                cursor: Some(cursor.clone()),
                ..batch(vec![a])
            })
            .await
            .unwrap();
        assert_eq!(store.cursors(None).await.unwrap(), vec![cursor.clone()]);
        assert!(store.cursors(Some("ted_search")).await.unwrap().is_empty());
        // A version-1 file opens, gains `switches`, becomes version 2.
        drop(store);
        let path = sources_db(dir.path());
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("DROP TABLE switches; PRAGMA user_version = 1;")
            .unwrap();
        drop(conn);
        let store = SqliteSourceStore::open(dir.path()).unwrap();
        assert!(store.switches(None).await.unwrap().is_empty());
        assert_eq!(store.cursors(None).await.unwrap(), vec![cursor]);
        let conn = Connection::open(&path).unwrap();
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
    }
}
