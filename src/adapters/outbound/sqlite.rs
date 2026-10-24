//! One opener for the install's SQLite stores (`runtime.db`, `ledger.db`,
//! `market.db`, `sources.db`, `solana-writes.db`, `observations.db`, the
//! decision cache, the history day files).
//!
//! | Piece | What |
//! |---|---|
//! | [`open_wal`] | parent dir created; the database, `-wal` and `-shm` owner-only ([`owner_only`]); WAL + busy_timeout 5 s; [`Durability`] picks `synchronous` |
//! | [`SharedConn`] | one connection behind a mutex; [`SharedConn::run`] runs a closure on `spawn_blocking` |
//!
//! Never for the evidence vault: its files are read through
//! `evidence::open_read_only`, which neither writes nor changes a mode.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use rusqlite::Connection;

/// What a power cut may cost.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Durability {
    /// SQLite's default `FULL`: a committed transaction survives (ledgers,
    /// leases, write fences).
    Full,
    /// `synchronous = NORMAL`: WAL stays consistent, the last commits may be
    /// lost — for data a rerun fetches or computes again.
    Normal,
}

/// Open (creating) `path` as a WAL database for this install's processes.
pub(crate) fn open_wal(path: &Path, durability: Durability) -> Result<Connection> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    // Before SQLite opens it: the WAL and its index take the database
    // file's mode when they are created.
    owner_only(path)?;
    let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
    let sync = match durability {
        Durability::Full => "",
        Durability::Normal => " PRAGMA synchronous=NORMAL;",
    };
    conn.execute_batch(&format!(
        "PRAGMA journal_mode=WAL;{sync} PRAGMA busy_timeout=5000;"
    ))
    .with_context(|| format!("pragmas of {}", path.display()))?;
    // The WAL now exists: narrow it too when an older binary left it wider.
    owner_only(path)?;
    Ok(conn)
}

/// The database file created 0600 when missing (an empty file is a new
/// database to SQLite); the database, `-wal` and `-shm` that exist lose
/// every group / other bit. Never adds a bit: a read-only file stays
/// read-only.
pub(crate) fn owner_only(db: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        if !db.exists() {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(db)
                .with_context(|| format!("create {}", db.display()))?;
        }
        for suffix in ["", "-wal", "-shm"] {
            let p = PathBuf::from(format!("{}{suffix}", db.display()));
            let Ok(meta) = std::fs::metadata(&p) else {
                continue;
            };
            let mode = meta.permissions().mode();
            if mode & 0o077 != 0 {
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode & 0o7700))
                    .with_context(|| format!("chmod go= {}", p.display()))?;
            }
        }
    }
    #[cfg(not(unix))]
    let _ = db;
    Ok(())
}

/// One connection shared by a store's handles.
#[derive(Clone)]
pub(crate) struct SharedConn {
    conn: Arc<Mutex<Connection>>,
    /// The store's name in lock / task errors.
    name: &'static str,
}

impl SharedConn {
    pub(crate) fn new(conn: Connection, name: &'static str) -> Self {
        Self {
            conn: Arc::new(Mutex::new(conn)),
            name,
        }
    }

    /// Run `f` on the connection, on a blocking thread.
    pub(crate) async fn run<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let (conn, name) = (Arc::clone(&self.conn), self.name);
        tokio::task::spawn_blocking(move || {
            let mut guard = conn.lock().map_err(|e| anyhow!("{name} lock: {e}"))?;
            f(&mut guard)
        })
        .await
        .with_context(|| format!("{name} task"))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn mode(p: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    fn files(db: &Path) -> [PathBuf; 3] {
        [
            db.to_path_buf(),
            PathBuf::from(format!("{}-wal", db.display())),
            PathBuf::from(format!("{}-shm", db.display())),
        ]
    }

    #[cfg(unix)]
    #[test]
    fn new_database_wal_and_shm_are_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("nested/x.db");
        let conn = open_wal(&db, Durability::Full).unwrap();
        conn.execute_batch("CREATE TABLE t(a); INSERT INTO t VALUES (1);")
            .unwrap();
        for f in files(&db) {
            assert_eq!(mode(&f), 0o600, "{}", f.display());
        }
    }

    #[cfg(unix)]
    #[test]
    fn wider_files_are_narrowed_and_no_bit_is_added() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("x.db");
        let conn = open_wal(&db, Durability::Normal).unwrap();
        conn.execute_batch("CREATE TABLE t(a); INSERT INTO t VALUES (1);")
            .unwrap();
        for f in files(&db) {
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        let again = open_wal(&db, Durability::Normal).unwrap();
        for f in files(&db) {
            assert_eq!(mode(&f), 0o600, "{}", f.display());
        }
        drop((conn, again));
        // A read-only file stays read-only: narrowed, never made writable.
        let ro = dir.path().join("ro.db");
        std::fs::write(&ro, b"").unwrap();
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o444)).unwrap();
        owner_only(&ro).unwrap();
        assert_eq!(mode(&ro), 0o400);
    }

    #[test]
    fn durability_sets_synchronous() {
        let dir = tempfile::tempdir().unwrap();
        let sync = |d| {
            let c = open_wal(&dir.path().join(format!("{d:?}.db")), d).unwrap();
            let journal: String = c
                .query_row("PRAGMA journal_mode", [], |r| r.get(0))
                .unwrap();
            assert_eq!(journal, "wal");
            c.query_row("PRAGMA synchronous", [], |r| r.get::<_, i64>(0))
                .unwrap()
        };
        assert_eq!(sync(Durability::Full), 2);
        assert_eq!(sync(Durability::Normal), 1);
    }

    #[tokio::test]
    async fn run_reaches_the_connection_and_names_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let shared = SharedConn::new(
            open_wal(&dir.path().join("x.db"), Durability::Full).unwrap(),
            "test store",
        );
        let n = shared
            .run(|c| {
                c.execute_batch("CREATE TABLE t(a); INSERT INTO t VALUES (7);")?;
                Ok(c.query_row("SELECT a FROM t", [], |r| r.get::<_, i64>(0))?)
            })
            .await
            .unwrap();
        assert_eq!(n, 7);
        let e = shared
            .run(|_| -> Result<()> { panic!("boom") })
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("test store task"), "{e:#}");
    }
}
