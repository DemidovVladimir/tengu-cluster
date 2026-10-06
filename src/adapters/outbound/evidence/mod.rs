//! Evidence IO adapters (`ports/evidence.rs`, `docs/lineage-2026-10-06.md`
//! § 3). Read-only by construction: SQLite through [`open_read_only`]
//! (`file:<path>?immutable=1`, `SQLITE_OPEN_READ_ONLY | SQLITE_OPEN_URI` —
//! no `-wal` / `-shm` created, no migration, no retention sweep); the only
//! write path is the new vault.
//!
//! | File | Implements |
//! |---|---|
//! | `vault.rs` | `Vault` — `<TENGU_HOME>/state/evidence/<vault>/` |
//! | `ledger_reader.rs` | `LedgerSource` — a paper `ledger.db`, old schema tolerated |
//! | `recorded.rs` | `RecordedHistory` (recorder day files) · `BackfillSource` (`market.db`) |

pub(crate) mod ledger_reader;
pub(crate) mod recorded;
pub(crate) mod vault;

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};

/// `file:` URI of `path`, with the characters a URI path cannot hold
/// percent-encoded.
fn sqlite_uri(path: &Path) -> String {
    let mut out = String::from("file:");
    for c in path.to_string_lossy().chars() {
        match c {
            '%' => out.push_str("%25"),
            '?' => out.push_str("%3f"),
            '#' => out.push_str("%23"),
            ' ' => out.push_str("%20"),
            _ => out.push(c),
        }
    }
    out.push_str("?immutable=1");
    out
}

/// Open `path` read-only and immutable (module doc). `Err` when it does
/// not exist (never created).
pub(crate) fn open_read_only(path: &Path) -> Result<Connection> {
    if !path.is_file() {
        anyhow::bail!("{}: no such file", path.display());
    }
    Connection::open_with_flags(
        sqlite_uri(path),
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open {} read-only", path.display()))
}

/// Column names of `table`; empty when the table does not exist.
pub(crate) fn columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let cols = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(cols)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_only_open_creates_nothing_and_refuses_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a b#c.db");
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1);")
                .unwrap();
        }
        let before: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        let c = open_read_only(&path).unwrap();
        let n: i64 = c
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        assert!(c.execute("INSERT INTO t VALUES (2)", []).is_err());
        assert_eq!(columns(&c, "t").unwrap(), vec!["x".to_string()]);
        assert!(columns(&c, "nope").unwrap().is_empty());
        drop(c);
        let after: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(before, after);
        assert!(open_read_only(&dir.path().join("missing.db")).is_err());
        assert!(!dir.path().join("missing.db").exists());
    }
}
