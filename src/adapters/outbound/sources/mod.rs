//! Source adapters (O2): the append-only store of the source layer and the
//! fetchers that fill it from approved `[sources]` rows (`config/sources.rs`).
//! Records are built by the pure parsers of `domain/source/`; the as-of view
//! reads them back. Agents never fetch: `source_evidence` is read-only, the
//! operator fetches with `tengu sources fetch` (O2 C8).
//!
//! | File | Holds |
//! |---|---|
//! | `store.rs` | `SqliteSourceStore` — `ports::source_store::SourceStore` over `<TENGU_HOME>/state/<sources.state>/sources.db`: raw snapshots (sha256, body when `store_raw`), records, coverage, cursors, purge tombstones; append-only |
//! | `sec.rs` | a `sec_edgar` row → records keyed by CIK: `SecClient` (`backfill/sec.rs`) replies kept as snapshots, `domain::source::sec_records` builds each record, one batch per 25 records, coverage + cursor in the last |
//!
//! | Rule | Value |
//! |---|---|
//! | Store | [`open_source_store`]: refused without `[sources]` (`sources_state_missing`) |
//! | Fetch | only an enabled row with its reviewed terms (`SourceEntry::fetch_stamp`), refused before any request otherwise |

pub(crate) mod sec;
pub(crate) mod store;

use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, Result};

use crate::config::sections::SandboxSections;
use crate::config::sources::SOURCES_DB;
use crate::ports::source_store::SourceStore;

/// `[sources]`'s state dir, where `sources.db` lives; refused without it.
pub(crate) fn sources_state_dir(sections: &SandboxSections) -> Result<&Path> {
    sections.sources_state_dir.as_deref().ok_or_else(|| {
        anyhow!(
            "sources_state_missing: no [sources] section — source records live in \
             <TENGU_HOME>/state/<sources.state>/{SOURCES_DB}; add [sources] state = \"<name>\""
        )
    })
}

/// The sandbox's source store (`[sources]` state dir); refused without
/// `[sources]`.
#[cfg_attr(not(test), allow(dead_code))] // `tengu sources` + `source_evidence` (O2 C8–C9)
pub(crate) fn open_source_store(sections: &SandboxSections) -> Result<Arc<dyn SourceStore>> {
    let dir = sources_state_dir(sections)?;
    Ok(Arc::new(store::SqliteSourceStore::open(dir)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn no_sources_section_no_store() {
        let e = open_source_store(&SandboxSections::default())
            .err()
            .unwrap()
            .to_string();
        assert!(e.starts_with("sources_state_missing"), "{e}");
        let dir = tempfile::tempdir().unwrap();
        let sections = SandboxSections {
            sources_state_dir: Some(dir.path().join("soe")),
            ..Default::default()
        };
        let store = open_source_store(&sections).unwrap();
        assert!(store.coverage(None).await.unwrap().is_empty());
        let db = dir.path().join("soe").join(SOURCES_DB);
        assert!(db.is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&db).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "raw bodies may hold contact data");
        }
    }
}
