//! `SqliteObservationStore` — `ports::observation::ObservationStore` over
//! `<workspace>/.tengu/observations.db` (one `observations` table; WAL +
//! busy_timeout so run-agent children, the MCP bridge and decision loops can
//! share it). Slot-monotonic upsert, `Error` rows never stored, rows older
//! than 7 days purged on open. `get` errors on a row that does not parse
//! (`get_many` skips it); `put_if_unchanged` is one conditional statement
//! (atomic across processes). Every call runs on `spawn_blocking`.
//!
//! `open_observation_store` is the one constructor tools and decision loops
//! use: this cache, wrapped by `RecordingObservationStore` (history,
//! `ops-history-recorder`) when `[recorder]` is on.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use async_trait::async_trait;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use tracing::warn;

use crate::adapters::outbound::history_sqlite::SqliteHistoryStore;
use crate::config::recorder::RecorderConfig;
use crate::config::sections::SandboxSections;
use crate::domain::observation::{now_ms, ObsStatus, Observation};
use crate::ports::history::{HistoryRow, HistoryStore};
use crate::ports::observation::ObservationStore;

/// Rows older than this are deleted when the store is opened.
const RETENTION_MS: i64 = 7 * 24 * 3600 * 1000;

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS observations (
  key TEXT PRIMARY KEY, schema TEXT NOT NULL, observed_at_ms INTEGER NOT NULL,
  slot INTEGER, ttl_ms INTEGER NOT NULL, status TEXT NOT NULL, body TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS observations_schema ON observations(schema, observed_at_ms);";

/// An older slot never overwrites a newer one (poller vs stream vs a
/// lagging RPC node); rows without a slot always replace.
const UPSERT_SQL: &str = "
INSERT INTO observations(key, schema, observed_at_ms, slot, ttl_ms, status, body)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
ON CONFLICT(key) DO UPDATE SET schema = excluded.schema, observed_at_ms = excluded.observed_at_ms,
  slot = excluded.slot, ttl_ms = excluded.ttl_ms, status = excluded.status, body = excluded.body
WHERE observations.slot IS NULL OR excluded.slot IS NULL OR excluded.slot >= observations.slot";

/// `put_if_unchanged` with an expected version: replace only that version.
const UPDATE_IF_SQL: &str = "
UPDATE observations SET schema = ?2, observed_at_ms = ?3, slot = ?4, ttl_ms = ?5, status = ?6,
  body = ?7
WHERE key = ?1 AND observed_at_ms = ?8
  AND (slot IS NULL OR ?4 IS NULL OR ?4 >= slot)";

/// `put_if_unchanged` expecting no row.
const INSERT_IF_ABSENT_SQL: &str = "
INSERT INTO observations(key, schema, observed_at_ms, slot, ttl_ms, status, body)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
ON CONFLICT(key) DO NOTHING";

#[derive(Clone)]
pub(crate) struct SqliteObservationStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteObservationStore {
    /// Open (creating) `<workspace>/.tengu/observations.db` and purge rows
    /// older than 7 days.
    pub(crate) fn open(workspace: &Path) -> Result<Self> {
        let dir = workspace.join(".tengu");
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        let path = dir.join("observations.db");
        let conn = Connection::open(&path).with_context(|| format!("open {}", path.display()))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")?;
        conn.execute_batch(SCHEMA_SQL)?;
        conn.execute(
            "DELETE FROM observations WHERE observed_at_ms < ?1",
            params![now_ms() - RETENTION_MS],
        )?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    async fn with_conn<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let guard = conn
                .lock()
                .map_err(|e| anyhow::anyhow!("observation store lock: {e}"))?;
            f(&guard)
        })
        .await
        .context("observation store task")?
    }
}

/// The stored body at `key`.
fn read_body(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT body FROM observations WHERE key = ?1",
            params![key],
            |r| r.get(0),
        )
        .optional()?)
}

fn parse_row(key: &str, body: &str) -> Result<Observation> {
    serde_json::from_str(body).with_context(|| format!("observation row {key} does not parse"))
}

/// Column values of `obs` for the write statements.
struct RowValues {
    key: String,
    schema: String,
    at: i64,
    slot: Option<i64>,
    ttl: i64,
    status: String,
    body: String,
}

impl RowValues {
    fn of(obs: &Observation) -> Result<Self> {
        Ok(Self {
            key: obs.key.clone(),
            schema: obs.schema.clone(),
            at: obs.observed_at_ms,
            slot: obs.slot.map(|s| s as i64),
            ttl: obs.ttl_ms as i64,
            status: serde_json::to_value(obs.status)?
                .as_str()
                .unwrap_or_default()
                .to_string(),
            body: serde_json::to_string(obs)?,
        })
    }
}

#[async_trait]
impl ObservationStore for SqliteObservationStore {
    async fn get(&self, key: &str) -> Result<Option<Observation>> {
        let key = key.to_string();
        self.with_conn(move |c| read_body(c, &key)?.map(|b| parse_row(&key, &b)).transpose())
            .await
    }

    async fn get_many(&self, keys: &[String]) -> Result<Vec<Option<Observation>>> {
        let keys = keys.to_vec();
        self.with_conn(move |c| {
            let mut out = Vec::with_capacity(keys.len());
            for k in &keys {
                out.push(read_body(c, k)?.and_then(|b| match parse_row(k, &b) {
                    Ok(o) => Some(o),
                    Err(e) => {
                        let error = format!("{e:#}");
                        warn!(key = %k, %error, "observation row does not parse; ignoring it");
                        None
                    }
                }));
            }
            Ok(out)
        })
        .await
    }

    async fn put(&self, obs: &Observation) -> Result<bool> {
        if obs.status == ObsStatus::Error {
            return Ok(false);
        }
        let v = RowValues::of(obs)?;
        self.with_conn(move |c| {
            let n = c.execute(
                UPSERT_SQL,
                params![v.key, v.schema, v.at, v.slot, v.ttl, v.status, v.body],
            )?;
            Ok(n > 0)
        })
        .await
    }

    async fn put_if_unchanged(
        &self,
        obs: &Observation,
        expected_observed_at_ms: Option<i64>,
    ) -> Result<bool> {
        if obs.status == ObsStatus::Error {
            return Ok(false);
        }
        let v = RowValues::of(obs)?;
        self.with_conn(move |c| {
            let n = match expected_observed_at_ms {
                Some(expected) => c.execute(
                    UPDATE_IF_SQL,
                    params![v.key, v.schema, v.at, v.slot, v.ttl, v.status, v.body, expected],
                )?,
                None => c.execute(
                    INSERT_IF_ABSENT_SQL,
                    params![v.key, v.schema, v.at, v.slot, v.ttl, v.status, v.body],
                )?,
            };
            Ok(n > 0)
        })
        .await
    }

    async fn remove(&self, keys: &[String]) -> Result<usize> {
        let keys = keys.to_vec();
        self.with_conn(move |c| {
            let mut n = 0;
            for k in &keys {
                n += c.execute("DELETE FROM observations WHERE key = ?1", params![k])?;
            }
            Ok(n)
        })
        .await
    }
}

/// The one constructor of a workspace's observation store (tool plugins,
/// `bootstrap/decision.rs`): the SQLite cache, wrapped by
/// `RecordingObservationStore` when `sections.history_dir` is set
/// (`[recorder] enabled` + `[xmarket]`). A history store that fails to open
/// only turns recording off (warn).
pub(crate) fn open_observation_store(
    workspace: &Path,
    sections: &SandboxSections,
) -> Result<Arc<dyn ObservationStore>> {
    let cache: Arc<dyn ObservationStore> = Arc::new(SqliteObservationStore::open(workspace)?);
    let Some(dir) = &sections.history_dir else {
        return Ok(cache);
    };
    match SqliteHistoryStore::open(dir, sections.recorder.retention_days) {
        Ok(history) => Ok(Arc::new(RecordingObservationStore::new(
            cache,
            Arc::new(history),
            sections.recorder.clone(),
        ))),
        Err(e) => {
            let error = format!("{e:#}");
            warn!(dir = %dir.display(), %error, "history store unavailable; not recording");
            Ok(cache)
        }
    }
}

/// `ObservationStore` decorator that also feeds a `HistoryStore`
/// (`ops-history-recorder`, tracker convention 5): the rows `put` /
/// `put_if_unchanged` wrote and every live result `observe()` passes to
/// `record` (`Error` and ttl-0 rows too). Only `[recorder] schemas`; per key
/// (this process's memory): each `(key, observed_at_ms)` once, nothing inside
/// `min_interval_secs`, and with `change_only` no unchanged row until the
/// last one is `heartbeat_secs` old; `data` only for `keep_data`. A failed
/// append never fails the cache call.
pub(crate) struct RecordingObservationStore {
    inner: Arc<dyn ObservationStore>,
    history: Arc<dyn HistoryStore>,
    policy: RecorderConfig,
    last: Mutex<HashMap<String, Recorded>>,
}

/// The last row recorded for a key.
#[derive(Clone, Copy)]
struct Recorded {
    at_ms: i64,
    fingerprint: u64,
}

impl RecordingObservationStore {
    pub(crate) fn new(
        inner: Arc<dyn ObservationStore>,
        history: Arc<dyn HistoryStore>,
        policy: RecorderConfig,
    ) -> Self {
        Self {
            inner,
            history,
            policy,
            last: Mutex::new(HashMap::new()),
        }
    }

    fn last(&self, key: &str) -> Option<Recorded> {
        self.last.lock().ok()?.get(key).copied()
    }

    async fn record_row(&self, obs: &Observation) -> Result<()> {
        if !self.policy.records(&obs.schema) {
            return Ok(());
        }
        let keep = self.policy.keeps_data(&obs.schema);
        let fingerprint = fingerprint(obs, keep);
        let prev = self.last(&obs.key);
        if let Some(p) = prev {
            let age = obs.observed_at_ms - p.at_ms;
            if age == 0 {
                return Ok(()); // this row is in already (`record`, then `put`)
            }
            if age > 0 {
                let age = age as u64;
                let heartbeat = self.policy.heartbeat_ms();
                let unchanged = self.policy.change_only && p.fingerprint == fingerprint;
                if age < self.policy.min_interval_ms(&obs.schema)
                    || (unchanged && (heartbeat == 0 || age < heartbeat))
                {
                    return Ok(());
                }
            }
        }
        self.history.append(&[HistoryRow::of(obs, keep)]).await?;
        if prev.is_none_or(|p| obs.observed_at_ms > p.at_ms) {
            if let Ok(mut last) = self.last.lock() {
                last.insert(
                    obs.key.clone(),
                    Recorded {
                        at_ms: obs.observed_at_ms,
                        fingerprint,
                    },
                );
            }
        }
        Ok(())
    }

    async fn record_after_write(&self, obs: &Observation) {
        if let Err(e) = self.record_row(obs).await {
            let error = format!("{e:#}");
            warn!(key = %obs.key, %error, "observation history write failed");
        }
    }
}

/// What `change_only` compares: status, errors, features without
/// `venue_ts_ms` (a new stamp alone is no change), and kept `data`.
fn fingerprint(obs: &Observation, keep_data: bool) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut features = obs.features.clone();
    features.remove("venue_ts_ms");
    let data = if keep_data { &obs.data } else { &Value::Null };
    let body = serde_json::json!([obs.status, obs.errors, features, data]);
    let mut h = std::collections::hash_map::DefaultHasher::new();
    body.to_string().hash(&mut h);
    h.finish()
}

#[async_trait]
impl ObservationStore for RecordingObservationStore {
    async fn get(&self, key: &str) -> Result<Option<Observation>> {
        self.inner.get(key).await
    }

    async fn get_many(&self, keys: &[String]) -> Result<Vec<Option<Observation>>> {
        self.inner.get_many(keys).await
    }

    async fn put(&self, obs: &Observation) -> Result<bool> {
        let written = self.inner.put(obs).await?;
        if written {
            self.record_after_write(obs).await;
        }
        Ok(written)
    }

    async fn put_if_unchanged(
        &self,
        obs: &Observation,
        expected_observed_at_ms: Option<i64>,
    ) -> Result<bool> {
        let written = self
            .inner
            .put_if_unchanged(obs, expected_observed_at_ms)
            .await?;
        if written {
            self.record_after_write(obs).await;
        }
        Ok(written)
    }

    async fn remove(&self, keys: &[String]) -> Result<usize> {
        self.inner.remove(keys).await
    }

    async fn record(&self, obs: &Observation) -> Result<()> {
        self.record_row(obs).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::observation::ObsSource;
    use serde_json::json;

    const POOL: &str = "5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS6";

    fn obs(slot: Option<u64>, status: ObsStatus, marker: i64) -> Observation {
        Observation {
            key: format!("dlmm_pool/1:{POOL}"),
            schema: "dlmm_pool/1".into(),
            tool: "dlmm_pool".into(),
            observed_at_ms: now_ms(),
            slot,
            ttl_ms: 5_000,
            source: ObsSource::Live,
            status,
            errors: vec![],
            headline: format!("dlmm_pool {POOL}"),
            features: [("active_id".to_string(), json!(marker))].into(),
            data: json!({"pool": POOL, "marker": marker}),
        }
    }

    #[tokio::test]
    async fn put_get_and_slot_monotonic_upsert() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteObservationStore::open(dir.path()).unwrap();
        assert!(dir.path().join(".tengu/observations.db").exists());
        let key = format!("dlmm_pool/1:{POOL}");
        assert!(store.get(&key).await.unwrap().is_none());

        assert!(store.put(&obs(Some(100), ObsStatus::Ok, 1)).await.unwrap());
        let got = store.get(&key).await.unwrap().unwrap();
        assert_eq!(got.data["marker"], json!(1));

        // Older slot is ignored; equal or newer replaces.
        assert!(!store.put(&obs(Some(99), ObsStatus::Ok, 2)).await.unwrap());
        assert_eq!(
            store.get(&key).await.unwrap().unwrap().data["marker"],
            json!(1)
        );
        assert!(store
            .put(&obs(Some(101), ObsStatus::Partial, 3))
            .await
            .unwrap());
        assert_eq!(
            store.get(&key).await.unwrap().unwrap().data["marker"],
            json!(3)
        );

        // Error rows are never stored.
        assert!(!store
            .put(&obs(Some(200), ObsStatus::Error, 4))
            .await
            .unwrap());
        assert_eq!(
            store.get(&key).await.unwrap().unwrap().data["marker"],
            json!(3)
        );

        let many = store
            .get_many(&[key.clone(), "missing/1:x".to_string()])
            .await
            .unwrap();
        assert_eq!(many.len(), 2);
        assert_eq!(many[0].as_ref().unwrap().key, key);
        assert!(many[1].is_none());
    }

    /// Regression (review #8): a body that no longer parses read as "no
    /// row", so `lp_state` callers overwrote it; `get` now errors (the
    /// caller decides), `get_many` still skips it.
    #[tokio::test]
    async fn an_unparseable_row_is_an_error_for_get_and_skipped_by_get_many() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteObservationStore::open(dir.path()).unwrap();
        let good = obs(None, ObsStatus::Ok, 1);
        assert!(store.put(&good).await.unwrap());
        let bad_key = format!("lp_state/1:{POOL}");
        let conn = Connection::open(dir.path().join(".tengu/observations.db")).unwrap();
        conn.execute(
            "INSERT INTO observations(key, schema, observed_at_ms, slot, ttl_ms, status, body)
             VALUES (?1, 'lp_state/1', 1, NULL, 1000, 'ok', '{not json')",
            params![bad_key],
        )
        .unwrap();
        let e = store.get(&bad_key).await.unwrap_err();
        assert!(format!("{e:#}").contains(&bad_key), "{e:#}");
        let many = store
            .get_many(&[bad_key.clone(), good.key.clone()])
            .await
            .unwrap();
        assert!(many[0].is_none());
        assert_eq!(many[1].as_ref().unwrap().key, good.key);
    }

    #[tokio::test]
    async fn remove_deletes_only_named_keys() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteObservationStore::open(dir.path()).unwrap();
        let mut a = obs(None, ObsStatus::Ok, 1);
        a.key = "t/1:a".into();
        let mut b = a.clone();
        b.key = "t/1:b".into();
        store.put(&a).await.unwrap();
        store.put(&b).await.unwrap();
        let n = store
            .remove(&["t/1:a".to_string(), "t/1:missing".to_string()])
            .await
            .unwrap();
        assert_eq!(n, 1);
        assert!(store.get("t/1:a").await.unwrap().is_none());
        assert!(store.get("t/1:b").await.unwrap().is_some());
    }

    /// Regression (review #11 #15): `lp_state` commits were blind whole-row
    /// replaces, so two writers lost each other's updates.
    #[tokio::test]
    async fn put_if_unchanged_is_a_compare_and_swap_on_observed_at() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteObservationStore::open(dir.path()).unwrap();
        let mut a = obs(None, ObsStatus::Ok, 1);
        a.observed_at_ms = 1_000;
        // No row expected: inserted once, then a conflict.
        assert!(store.put_if_unchanged(&a, None).await.unwrap());
        assert!(!store.put_if_unchanged(&a, None).await.unwrap());
        // Two writers read version 1_000; the first wins, the second conflicts.
        let mut b = obs(None, ObsStatus::Ok, 2);
        b.observed_at_ms = 2_000;
        let mut c = obs(None, ObsStatus::Ok, 3);
        c.observed_at_ms = 2_001;
        assert!(store.put_if_unchanged(&b, Some(1_000)).await.unwrap());
        assert!(!store.put_if_unchanged(&c, Some(1_000)).await.unwrap());
        let got = store.get(&a.key).await.unwrap().unwrap();
        assert_eq!(got.data["marker"], json!(2));
        // Re-read, then write on the current version.
        assert!(store.put_if_unchanged(&c, Some(2_000)).await.unwrap());
        assert_eq!(
            store.get(&a.key).await.unwrap().unwrap().data["marker"],
            json!(3)
        );
        // Error rows are never written.
        let mut e = obs(None, ObsStatus::Error, 4);
        e.observed_at_ms = 3_000;
        assert!(!store.put_if_unchanged(&e, Some(2_001)).await.unwrap());
    }

    // ── RecordingObservationStore ───────────────────────────────────

    const TSLA: &str = "hyperliquid:xyz:TSLA";

    /// History that keeps appended rows in memory.
    #[derive(Default)]
    struct MemHistory(Mutex<Vec<HistoryRow>>);

    #[async_trait]
    impl HistoryStore for MemHistory {
        async fn append(&self, rows: &[HistoryRow]) -> Result<usize> {
            self.0.lock().unwrap().extend_from_slice(rows);
            Ok(rows.len())
        }
        async fn range(&self, _: &str, _: i64, _: i64) -> Result<Vec<HistoryRow>> {
            Ok(Vec::new())
        }
        async fn asof(&self, keys: &[String], _: i64, _: u64) -> Result<Vec<Option<HistoryRow>>> {
            Ok(vec![None; keys.len()])
        }
    }

    fn ctx_row(at_ms: i64, mark: f64) -> Observation {
        Observation {
            key: format!("mkt_ctx/1:{TSLA}"),
            schema: "mkt_ctx/1".into(),
            tool: "hl_ctx".into(),
            observed_at_ms: at_ms,
            slot: None,
            ttl_ms: 15_000,
            source: ObsSource::Live,
            status: ObsStatus::Ok,
            errors: vec![],
            headline: format!("mkt_ctx hyperliquid:xyz:TSLA mark {mark}"),
            features: [
                ("mark".to_string(), json!(mark)),
                ("venue_ts_ms".to_string(), json!(at_ms - 5)),
            ]
            .into(),
            data: json!({"coin": "xyz:TSLA", "mark": mark}),
        }
    }

    fn recording(
        policy: &str,
    ) -> (
        tempfile::TempDir,
        RecordingObservationStore,
        Arc<MemHistory>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(SqliteObservationStore::open(dir.path()).unwrap());
        let history = Arc::new(MemHistory::default());
        let policy: RecorderConfig = toml::from_str(policy).unwrap();
        let store = RecordingObservationStore::new(cache, history.clone(), policy);
        (dir, store, history)
    }

    fn recorded(h: &MemHistory) -> Vec<(i64, Option<f64>, bool)> {
        h.0.lock()
            .unwrap()
            .iter()
            .map(|r| {
                let mark = r.features.get("mark").and_then(Value::as_f64);
                (r.observed_at_ms, mark, r.data.is_some())
            })
            .collect()
    }

    #[tokio::test]
    async fn change_only_skips_unchanged_rows_until_the_heartbeat() {
        let (_dir, store, history) =
            recording("enabled = true\nschemas = [\"mkt_ctx/1\"]\nheartbeat_secs = 300\n");
        let s = 1_000;
        for (at, mark) in [
            (0, 1.0),       // first row
            (10 * s, 1.0),  // unchanged (a new venue stamp alone is no change)
            (20 * s, 2.0),  // changed
            (30 * s, 2.0),  // unchanged
            (320 * s, 2.0), // unchanged, but the last row is 300 s old
        ] {
            store.record(&ctx_row(at, mark)).await.unwrap();
        }
        let mut err = ctx_row(330 * s, 2.0);
        err.status = ObsStatus::Error;
        store.record(&err).await.unwrap();
        // `observe()` records, then puts the same row: appended once.
        let row = ctx_row(340 * s, 3.0);
        store.record(&row).await.unwrap();
        assert!(store.put(&row).await.unwrap());
        assert_eq!(
            recorded(&history),
            [
                (0, Some(1.0), false),
                (20 * s, Some(2.0), false),
                (320 * s, Some(2.0), false),
                (330 * s, Some(2.0), false),
                (340 * s, Some(3.0), false),
            ]
        );
        // Unlisted schemas are never recorded; the cache is unaffected.
        let mut other = ctx_row(400 * s, 4.0);
        other.schema = "dlmm_pool/1".into();
        other.key = "dlmm_pool/1:5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS6".into();
        store.record(&other).await.unwrap();
        assert!(store.put(&other).await.unwrap());
        assert_eq!(history.0.lock().unwrap().len(), 5);
        assert!(store.get(&other.key).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn min_interval_and_keep_data() {
        let (_dir, store, history) = recording(
            "enabled = true\nschemas = [\"mkt_ctx/1\"]\nkeep_data = [\"mkt_ctx/1\"]\n\
             change_only = false\nmin_interval_secs = { \"mkt_ctx/1\" = 60 }\n",
        );
        let s = 1_000;
        for (at, mark) in [
            (0, 1.0),
            (30 * s, 2.0),
            (59 * s, 3.0),
            (60 * s, 3.0),
            (61 * s, 3.0),
        ] {
            store.record(&ctx_row(at, mark)).await.unwrap();
        }
        assert_eq!(
            recorded(&history),
            [(0, Some(1.0), true), (60 * s, Some(3.0), true)]
        );
        // Direct `put` writes are recorded too (`lp_state`-style rows).
        assert!(store.put(&ctx_row(200 * s, 5.0)).await.unwrap());
        assert_eq!(history.0.lock().unwrap().len(), 3);
        // A rejected write (older slot) is not.
        let mut newer = ctx_row(300 * s, 6.0);
        newer.slot = Some(10);
        let mut older = ctx_row(400 * s, 7.0);
        older.slot = Some(9);
        assert!(store.put(&newer).await.unwrap());
        assert!(!store.put(&older).await.unwrap());
        assert_eq!(history.0.lock().unwrap().len(), 4);
    }

    /// Minimal typed value for the end-to-end `observe()` path.
    #[derive(serde::Serialize, serde::Deserialize)]
    struct Mark(Option<f64>);

    impl crate::domain::observation::Observed for Mark {
        const SCHEMA: &'static str = "mkt_ctx/1";
        fn subject(&self) -> String {
            TSLA.into()
        }
        fn headline(&self) -> String {
            format!("mkt_ctx {TSLA}")
        }
        fn features(&self) -> crate::domain::observation::Features {
            let mut f = crate::domain::observation::Features::new();
            crate::domain::observation::set_num(&mut f, "mark", self.0);
            f
        }
        fn status(&self) -> ObsStatus {
            if self.0.is_some() {
                ObsStatus::Ok
            } else {
                ObsStatus::Error
            }
        }
    }

    /// `open_observation_store` + `observe()`: Error and ttl-0 rows reach the
    /// history day file, the cache holds only the usable ttl > 0 row.
    #[tokio::test]
    async fn observe_records_error_and_ttl0_rows_into_day_files() {
        use crate::application::observe::observe;
        use crate::domain::observation::CachePolicy;

        let ws = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let history_dir = state.path().join("history");
        let sections = SandboxSections {
            history_dir: Some(history_dir.clone()),
            recorder: toml::from_str("enabled = true\nschemas = [\"mkt_ctx/1\"]\n").unwrap(),
            ..SandboxSections::default()
        };
        let store = open_observation_store(ws.path(), &sections).unwrap();
        let policy = CachePolicy::new("mkt_ctx/1", TSLA, 15_000, &json!({}));
        let t0 = now_ms();
        for (at, mark, ttl) in [
            (t0, None, 15_000),
            (t0 + 1, Some(1.0), 0),
            (t0 + 2, Some(2.0), 15_000),
        ] {
            let obs = observe(Some(store.as_ref()), "hl_ctx", &policy, at, || async move {
                Ok((Mark(mark), ttl))
            })
            .await
            .unwrap();
            assert_eq!(obs.source, ObsSource::Live);
        }
        let cached = store.get(&policy.key).await.unwrap().unwrap();
        assert_eq!(cached.observed_at_ms, t0 + 2);

        let reader = SqliteHistoryStore::reader(&history_dir);
        let rows = reader.range(&policy.key, t0, t0 + 3).await.unwrap();
        let got: Vec<(i64, ObsStatus)> =
            rows.iter().map(|r| (r.observed_at_ms, r.status)).collect();
        assert_eq!(
            got,
            [
                (t0, ObsStatus::Error),
                (t0 + 1, ObsStatus::Ok),
                (t0 + 2, ObsStatus::Ok)
            ]
        );
        assert!(rows.iter().all(|r| r.data.is_none() && r.key == policy.key));

        // Recording off (no history dir) ⇒ the plain cache.
        let plain = open_observation_store(ws.path(), &SandboxSections::default()).unwrap();
        assert!(plain.record(&cached).await.is_ok());
    }

    #[tokio::test]
    async fn open_purges_rows_older_than_seven_days() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteObservationStore::open(dir.path()).unwrap();
        let mut old = obs(None, ObsStatus::Ok, 1);
        old.observed_at_ms = now_ms() - RETENTION_MS - 1_000;
        assert!(store.put(&old).await.unwrap());
        drop(store);
        let store = SqliteObservationStore::open(dir.path()).unwrap();
        assert!(store.get(&old.key).await.unwrap().is_none());
    }
}
