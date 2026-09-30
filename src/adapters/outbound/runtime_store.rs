//! `SqliteRuntimeStore` — `ports::runtime::RuntimeStore` over the runtime
//! state dir (the `[xmarket]` state dir, else `<TENGU_HOME>/state`):
//!
//! | File | Holds |
//! |---|---|
//! | `runtime.db` | the single-runner lease of `tengu run` — WAL + busy_timeout, one statement per change, atomic across processes (pattern of `solana/writes_store.rs`) |
//! | `run-<sandbox>.json` | the heartbeat — written to a temp file, then renamed (readers never see a partial file); [`read_heartbeat`] serves `tengu doctor --live` |

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use async_trait::async_trait;
use rusqlite::{params, Connection};

use crate::domain::runtime::{heartbeat_file, Heartbeat, RunnerLease};
use crate::ports::runtime::RuntimeStore;

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS leases (
  resource TEXT PRIMARY KEY, holder TEXT NOT NULL,
  acquired_at_ms INTEGER NOT NULL, expires_at_ms INTEGER NOT NULL);";

/// Take the lease when free, expired, or ours; renewal keeps
/// `acquired_at_ms`. `?3` = now.
const ACQUIRE_SQL: &str = "
INSERT INTO leases(resource, holder, acquired_at_ms, expires_at_ms) VALUES (?1, ?2, ?3, ?4)
ON CONFLICT(resource) DO UPDATE SET
  acquired_at_ms = CASE WHEN leases.holder = excluded.holder THEN leases.acquired_at_ms
                        ELSE excluded.acquired_at_ms END,
  holder = excluded.holder, expires_at_ms = excluded.expires_at_ms
WHERE leases.holder = excluded.holder OR leases.expires_at_ms <= ?3";

#[derive(Clone)]
pub(crate) struct SqliteRuntimeStore {
    conn: Arc<Mutex<Connection>>,
    dir: PathBuf,
    path: PathBuf,
}

/// Write `<dir>/run-<sandbox>.json` atomically (temp file + rename).
pub(crate) fn write_heartbeat(dir: &Path, hb: &Heartbeat) -> Result<()> {
    let path = dir.join(heartbeat_file(&hb.sandbox));
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        heartbeat_file(&hb.sandbox),
        std::process::id()
    ));
    std::fs::write(&tmp, serde_json::to_vec_pretty(hb)?)
        .with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("replace {}", path.display()))
}

/// `<dir>/run-<sandbox>.json`: `None` when absent, `Err` when unreadable.
pub(crate) fn read_heartbeat(dir: &Path, sandbox: &str) -> Result<Option<Heartbeat>> {
    let path = dir.join(heartbeat_file(sandbox));
    let raw = match std::fs::read(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    serde_json::from_slice(&raw)
        .map(Some)
        .with_context(|| format!("{} does not parse", path.display()))
}

impl SqliteRuntimeStore {
    /// Open (creating) `<dir>/runtime.db`.
    pub(crate) fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let path = dir.join("runtime.db");
        let conn = Connection::open(&path).with_context(|| format!("open {}", path.display()))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")?;
        conn.execute_batch(SCHEMA_SQL)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            dir: dir.to_path_buf(),
            path,
        })
    }

    /// The database file (for operator messages).
    pub(crate) fn path(&self) -> &Path {
        &self.path
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
                .map_err(|e| anyhow::anyhow!("runtime store lock: {e}"))?;
            f(&guard)
        })
        .await
        .context("runtime store task")?
    }
}

#[async_trait]
impl RuntimeStore for SqliteRuntimeStore {
    async fn acquire_lease(
        &self,
        resource: &str,
        holder: &str,
        ttl_ms: i64,
        now_ms: i64,
    ) -> Result<RunnerLease> {
        let (resource, holder) = (resource.to_string(), holder.to_string());
        self.with_conn(move |c| {
            c.execute(
                ACQUIRE_SQL,
                params![resource, holder, now_ms, now_ms.saturating_add(ttl_ms)],
            )?;
            let (current, acquired, expires): (String, i64, i64) = c.query_row(
                "SELECT holder, acquired_at_ms, expires_at_ms FROM leases WHERE resource = ?1",
                params![resource],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            Ok(RunnerLease {
                granted: current == holder,
                resource,
                holder,
                current_holder: current,
                acquired_at_ms: acquired,
                expires_at_ms: expires,
            })
        })
        .await
    }

    async fn release_lease(&self, resource: &str, holder: &str) -> Result<()> {
        let (resource, holder) = (resource.to_string(), holder.to_string());
        self.with_conn(move |c| {
            c.execute(
                "DELETE FROM leases WHERE resource = ?1 AND holder = ?2",
                params![resource, holder],
            )?;
            Ok(())
        })
        .await
    }

    async fn write_heartbeat(&self, hb: &Heartbeat) -> Result<()> {
        let (dir, hb) = (self.dir.clone(), hb.clone());
        tokio::task::spawn_blocking(move || write_heartbeat(&dir, &hb))
            .await
            .context("heartbeat write task")?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::runtime::lease_resource;

    /// Two stores on one file = two `tengu run` processes.
    fn pair() -> (tempfile::TempDir, SqliteRuntimeStore, SqliteRuntimeStore) {
        let dir = tempfile::tempdir().unwrap();
        let a = SqliteRuntimeStore::open(dir.path()).unwrap();
        let b = SqliteRuntimeStore::open(dir.path()).unwrap();
        (dir, a, b)
    }

    #[tokio::test]
    async fn lease_acquire_renew_refuse_takeover() {
        let (dir, a, b) = pair();
        assert_eq!(a.path(), dir.path().join("runtime.db"));
        let r = lease_resource("xmarket-weekend");
        let la = a.acquire_lease(&r, "A", 30_000, 10_000).await.unwrap();
        assert!(la.granted);
        assert_eq!((la.acquired_at_ms, la.expires_at_ms), (10_000, 40_000));
        // A second instance is refused and told who holds it.
        let lb = b.acquire_lease(&r, "B", 30_000, 20_000).await.unwrap();
        assert!(!lb.granted);
        assert_eq!(lb.current_holder, "A");
        assert_eq!(lb.expires_at_ms, 40_000);
        // Renewal keeps acquired_at and extends the expiry.
        let la2 = a.acquire_lease(&r, "A", 30_000, 30_000).await.unwrap();
        assert!(la2.granted);
        assert_eq!((la2.acquired_at_ms, la2.expires_at_ms), (10_000, 60_000));
        assert!(
            !b.acquire_lease(&r, "B", 30_000, 59_999)
                .await
                .unwrap()
                .granted
        );
        // Expired (A crashed) ⇒ B takes over; A's renewal is refused.
        let lb2 = b.acquire_lease(&r, "B", 30_000, 60_000).await.unwrap();
        assert!(lb2.granted);
        assert_eq!(lb2.acquired_at_ms, 60_000);
        let la3 = a.acquire_lease(&r, "A", 30_000, 60_001).await.unwrap();
        assert!(!la3.granted);
        assert_eq!(la3.current_holder, "B");
    }

    #[tokio::test]
    async fn release_frees_only_the_holders_lease() {
        let (_dir, a, b) = pair();
        let r = lease_resource("xmarket");
        assert!(a.acquire_lease(&r, "A", 30_000, 0).await.unwrap().granted);
        b.release_lease(&r, "B").await.unwrap();
        assert!(!b.acquire_lease(&r, "B", 30_000, 1).await.unwrap().granted);
        a.release_lease(&r, "A").await.unwrap();
        assert!(b.acquire_lease(&r, "B", 30_000, 2).await.unwrap().granted);
        // Other sandboxes' leases are independent.
        let other = lease_resource("xmarket-weekend");
        assert!(
            a.acquire_lease(&other, "A", 30_000, 3)
                .await
                .unwrap()
                .granted
        );
    }

    #[tokio::test]
    async fn heartbeat_round_trips_atomically() {
        use crate::domain::runtime::RunState;
        let (dir, a, _b) = pair();
        assert!(read_heartbeat(dir.path(), "xmarket-weekend")
            .unwrap()
            .is_none());
        let mut hb = Heartbeat {
            sandbox: "xmarket-weekend".into(),
            pid: 4242,
            holder: "vps-1:4242:0f8c6a52-5d0e-4c8e-9a71-3b2f1c9d7e44".into(),
            state: RunState::Running,
            stop_reason: None,
            started_at_ms: 1,
            ts_ms: 2,
            heartbeat_secs: 5,
            loops: Default::default(),
            feeds: Default::default(),
        };
        a.write_heartbeat(&hb).await.unwrap();
        hb.state = RunState::Stopped;
        hb.stop_reason = Some("SIGTERM".into());
        a.write_heartbeat(&hb).await.unwrap();
        assert_eq!(
            read_heartbeat(dir.path(), "xmarket-weekend").unwrap(),
            Some(hb)
        );
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".json"))
            .collect();
        assert_eq!(names, ["run-xmarket-weekend.json"], "no temp file left");
        std::fs::write(dir.path().join("run-broken.json"), "{").unwrap();
        let err = read_heartbeat(dir.path(), "broken").unwrap_err();
        assert!(format!("{err:#}").contains("run-broken.json does not parse"));
    }
}
