//! `SqliteObservationStore` — `ports::observation::ObservationStore` over
//! `<workspace>/.tengu/observations.db` (one `observations` table; WAL +
//! busy_timeout so run-agent children, the MCP bridge and decision loops can
//! share it). Slot-monotonic upsert, `Error` rows never stored, rows older
//! than 7 days purged on open. `get` errors on a row that does not parse
//! (`get_many` skips it); `put_if_unchanged` is one conditional statement
//! (atomic across processes). Every call runs on `spawn_blocking`.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use async_trait::async_trait;
use rusqlite::{params, Connection, OptionalExtension};
use tracing::warn;

use crate::domain::observation::{now_ms, ObsStatus, Observation};
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
