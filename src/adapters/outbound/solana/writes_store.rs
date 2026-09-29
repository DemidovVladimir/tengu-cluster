//! `SqliteWriteStore` — `ports::solana_writes::SolanaWriteStore` over ONE
//! database per install, `<TENGU_HOME>/state/solana-writes.db` (not per
//! workspace: every process that may sign for a wallet must see the same
//! lease). WAL + busy_timeout; every mutation is one statement, so it is
//! atomic across processes. Rows are never purged by age — a lease expires
//! by its own `expires_at_ms`, a pending record is cleared when resolved.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use async_trait::async_trait;
use rusqlite::{params, Connection, OptionalExtension};

use crate::domain::solana_write::{Lease, PendingSend};
use crate::ports::solana_writes::SolanaWriteStore;

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS leases (
  resource TEXT PRIMARY KEY, holder TEXT NOT NULL,
  acquired_at_ms INTEGER NOT NULL, expires_at_ms INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS pending_sends (
  wallet TEXT PRIMARY KEY, signature TEXT NOT NULL, body TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS fences (
  wallet TEXT PRIMARY KEY, slot INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL);";

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
pub(crate) struct SqliteWriteStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteWriteStore {
    /// `<TENGU_HOME>/state`.
    pub(crate) fn default_dir() -> PathBuf {
        crate::config::paths::resolve_tengu_home().join("state")
    }

    /// Open (creating) `<dir>/solana-writes.db`.
    pub(crate) fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let path = dir.join("solana-writes.db");
        let conn = Connection::open(&path).with_context(|| format!("open {}", path.display()))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")?;
        conn.execute_batch(SCHEMA_SQL)?;
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
                .map_err(|e| anyhow::anyhow!("solana write store lock: {e}"))?;
            f(&guard)
        })
        .await
        .context("solana write store task")?
    }
}

#[async_trait]
impl SolanaWriteStore for SqliteWriteStore {
    async fn acquire(
        &self,
        resource: &str,
        holder: &str,
        ttl_ms: i64,
        now_ms: i64,
    ) -> Result<Lease> {
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
            Ok(Lease {
                granted: current == holder,
                resource,
                holder,
                acquired_at_ms: acquired,
                expires_at_ms: expires,
                current_holder: current,
            })
        })
        .await
    }

    async fn release(&self, resource: &str, holder: &str) -> Result<()> {
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

    async fn put_pending(&self, p: &PendingSend) -> Result<()> {
        let body = serde_json::to_string(p)?;
        let (wallet, sig) = (p.wallet.clone(), p.signature.clone());
        self.with_conn(move |c| {
            c.execute(
                "INSERT INTO pending_sends(wallet, signature, body) VALUES (?1, ?2, ?3)
                 ON CONFLICT(wallet) DO UPDATE SET signature = excluded.signature, body = excluded.body",
                params![wallet, sig, body],
            )?;
            Ok(())
        })
        .await
    }

    async fn pending(&self, wallet: &str) -> Result<Option<PendingSend>> {
        let wallet = wallet.to_string();
        self.with_conn(move |c| {
            let body: Option<String> = c
                .query_row(
                    "SELECT body FROM pending_sends WHERE wallet = ?1",
                    params![wallet],
                    |r| r.get(0),
                )
                .optional()?;
            body.map(|b| {
                serde_json::from_str(&b)
                    .with_context(|| format!("pending send record of {wallet} does not parse"))
            })
            .transpose()
        })
        .await
    }

    async fn clear_pending(&self, wallet: &str, signature: &str) -> Result<()> {
        let (wallet, sig) = (wallet.to_string(), signature.to_string());
        self.with_conn(move |c| {
            c.execute(
                "DELETE FROM pending_sends WHERE wallet = ?1 AND signature = ?2",
                params![wallet, sig],
            )?;
            Ok(())
        })
        .await
    }

    async fn raise_fence(&self, wallet: &str, slot: u64, now_ms: i64) -> Result<()> {
        let wallet = wallet.to_string();
        let slot = i64::try_from(slot).context("fence slot out of range")?;
        self.with_conn(move |c| {
            c.execute(
                "INSERT INTO fences(wallet, slot, updated_at_ms) VALUES (?1, ?2, ?3)
                 ON CONFLICT(wallet) DO UPDATE SET slot = MAX(fences.slot, excluded.slot),
                   updated_at_ms = excluded.updated_at_ms",
                params![wallet, slot, now_ms],
            )?;
            Ok(())
        })
        .await
    }

    async fn fence(&self, wallet: &str) -> Result<Option<u64>> {
        let wallet = wallet.to_string();
        self.with_conn(move |c| {
            let slot: Option<i64> = c
                .query_row(
                    "SELECT slot FROM fences WHERE wallet = ?1",
                    params![wallet],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(slot.map(|s| s as u64))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: &str = "AKnL4NNf3DGWZJS6cPknBuEGnVsV4A4m5tgebLHaRSZ9";

    /// Two stores on one file = two processes.
    fn pair() -> (tempfile::TempDir, SqliteWriteStore, SqliteWriteStore) {
        let dir = tempfile::tempdir().unwrap();
        let a = SqliteWriteStore::open(dir.path()).unwrap();
        let b = SqliteWriteStore::open(dir.path()).unwrap();
        (dir, a, b)
    }

    #[tokio::test]
    async fn lease_is_exclusive_across_connections_until_expiry() {
        let (_d, a, b) = pair();
        let r = format!("wallet:{W}");
        let la = a.acquire(&r, "A", 1_000, 10_000).await.unwrap();
        assert!(la.granted);
        assert_eq!((la.acquired_at_ms, la.expires_at_ms), (10_000, 11_000));
        let lb = b.acquire(&r, "B", 1_000, 10_500).await.unwrap();
        assert!(!lb.granted);
        assert_eq!(lb.current_holder, "A");
        // Renewal keeps acquired_at, extends expiry.
        let la2 = a.acquire(&r, "A", 1_000, 10_900).await.unwrap();
        assert!(la2.granted);
        assert_eq!((la2.acquired_at_ms, la2.expires_at_ms), (10_000, 11_900));
        assert!(!b.acquire(&r, "B", 1_000, 11_899).await.unwrap().granted);
        // Expired ⇒ B takes it.
        let lb2 = b.acquire(&r, "B", 1_000, 11_900).await.unwrap();
        assert!(lb2.granted);
        assert_eq!(lb2.acquired_at_ms, 11_900);
        // A's release is a no-op now; B's release frees it.
        a.release(&r, "A").await.unwrap();
        assert!(!a.acquire(&r, "A", 1_000, 12_000).await.unwrap().granted);
        b.release(&r, "B").await.unwrap();
        assert!(a.acquire(&r, "A", 1_000, 12_000).await.unwrap().granted);
    }

    #[tokio::test]
    async fn pending_record_round_trips_and_clears_by_signature() {
        let (_d, a, b) = pair();
        let p = PendingSend {
            wallet: W.into(),
            tool: "jupiter_swap".into(),
            label: "swap".into(),
            signature: "sig-1".into(),
            last_valid_block_height: 99,
            sent_at_ms: 5,
        };
        a.put_pending(&p).await.unwrap();
        assert_eq!(b.pending(W).await.unwrap(), Some(p.clone()));
        b.clear_pending(W, "other").await.unwrap();
        assert!(
            a.pending(W).await.unwrap().is_some(),
            "wrong signature keeps it"
        );
        b.clear_pending(W, "sig-1").await.unwrap();
        assert_eq!(a.pending(W).await.unwrap(), None);
    }

    #[tokio::test]
    async fn fence_only_rises() {
        let (_d, a, b) = pair();
        assert_eq!(a.fence(W).await.unwrap(), None);
        a.raise_fence(W, 100, 1).await.unwrap();
        b.raise_fence(W, 90, 2).await.unwrap();
        assert_eq!(b.fence(W).await.unwrap(), Some(100));
        b.raise_fence(W, 120, 3).await.unwrap();
        assert_eq!(a.fence(W).await.unwrap(), Some(120));
    }
}
