//! `SqliteObservationStore` — `ports::observation::ObservationStore` over
//! `<workspace>/.tengu/observations.db` (one `observations` table; WAL +
//! busy_timeout so run-agent children, the MCP bridge and decision loops can
//! share it). Slot-monotonic upsert, `Error` rows never stored, rows older
//! than 7 days purged on open. Every call runs on `spawn_blocking`.

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

fn read_row(conn: &Connection, key: &str) -> Result<Option<Observation>> {
    let body: Option<String> = conn
        .query_row(
            "SELECT body FROM observations WHERE key = ?1",
            params![key],
            |r| r.get(0),
        )
        .optional()?;
    Ok(
        body.and_then(|b| match serde_json::from_str::<Observation>(&b) {
            Ok(o) => Some(o),
            Err(e) => {
                warn!(key, error = %e, "observation row does not parse; ignoring it");
                None
            }
        }),
    )
}

#[async_trait]
impl ObservationStore for SqliteObservationStore {
    async fn get(&self, key: &str) -> Result<Option<Observation>> {
        let key = key.to_string();
        self.with_conn(move |c| read_row(c, &key)).await
    }

    async fn get_many(&self, keys: &[String]) -> Result<Vec<Option<Observation>>> {
        let keys = keys.to_vec();
        self.with_conn(move |c| keys.iter().map(|k| read_row(c, k)).collect())
            .await
    }

    async fn put(&self, obs: &Observation) -> Result<bool> {
        if obs.status == ObsStatus::Error {
            return Ok(false);
        }
        let body = serde_json::to_string(obs)?;
        let status = serde_json::to_value(obs.status)?
            .as_str()
            .unwrap_or_default()
            .to_string();
        let (key, schema, at, slot, ttl) = (
            obs.key.clone(),
            obs.schema.clone(),
            obs.observed_at_ms,
            obs.slot.map(|s| s as i64),
            obs.ttl_ms as i64,
        );
        self.with_conn(move |c| {
            let n = c.execute(
                UPSERT_SQL,
                params![key, schema, at, slot, ttl, status, body],
            )?;
            Ok(n > 0)
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
