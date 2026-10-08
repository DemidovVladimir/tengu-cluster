//! Source adapters (O2): the append-only store of the source layer and the
//! fetchers that fill it from approved `[sources]` rows (`config/sources.rs`).
//! Records are built by the pure parsers of `domain/source/`; the as-of
//! view reads them back. Agents never fetch: `source_evidence` is read-only, the
//! operator fetches with `tengu sources fetch` (`cli/sources.rs`).
//!
//! | File | Holds |
//! |---|---|
//! | `store.rs` | `SqliteSourceStore` — `ports::source_store::SourceStore` over `<TENGU_HOME>/state/<sources.state>/sources.db`: raw snapshots (sha256, body when `store_raw`), records, coverage, cursors, purge tombstones, runtime switches; append-only |
//! | `sec.rs` | a `sec_edgar` row → records keyed by CIK: `SecClient` (`backfill/sec.rs`) replies kept as snapshots, `domain::source::sec_records` builds each record, one batch per 25 records, coverage + cursor in the last |
//! | `ted.rs` | a `ted_search` row → records per notice: `TedClient` (anonymous `POST /v3/notices/search`), one-day windows paged, every page a snapshot, `domain::source::ted` builds each record (change notices linked to what they correct); an offline import of a saved page |
//!
//! | Rule | Value |
//! |---|---|
//! | Store | [`open_source_store`]: refused without `[sources]` (`sources_state_missing`); readers take [`existing_source_store`] (`None` until `sources.db` exists — a read creates nothing) |
//! | Fetch / import | [`fetch_gate`]: the row's kind, an enabled row with its reviewed terms (`SourceEntry::fetch_stamp`) and no runtime off switch (`tengu sources disable`) — refused before any request otherwise |

pub(crate) mod sec;
pub(crate) mod store;
pub(crate) mod ted;

use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, Result};

use crate::config::sections::SandboxSections;
use crate::config::sources::{SourceEntry, SourceKind, SOURCES_DB};
use crate::domain::marketdata::fmt_time;
use crate::domain::source::SourceStamp;
use crate::ports::source_store::{switched_off, SourceStore};

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
pub(crate) fn open_source_store(sections: &SandboxSections) -> Result<Arc<dyn SourceStore>> {
    let dir = sources_state_dir(sections)?;
    Ok(Arc::new(store::SqliteSourceStore::open(dir)?))
}

/// The sandbox's source store when `sources.db` exists, else `None` — a
/// reader (`tengu sources list|asof|purge`, the `source_evidence` tool)
/// creates nothing; refused without `[sources]`.
pub(crate) fn existing_source_store(
    sections: &SandboxSections,
) -> Result<Option<Arc<dyn SourceStore>>> {
    let dir = sources_state_dir(sections)?;
    if dir.join(SOURCES_DB).exists() {
        Ok(Some(open_source_store(sections)?))
    } else {
        Ok(None)
    }
}

/// What every record of row `id` carries, or why nothing may be fetched or
/// imported for it (module table: fetch / import).
pub(crate) async fn fetch_gate(
    store: &dyn SourceStore,
    id: &str,
    entry: &SourceEntry,
    kind: SourceKind,
) -> Result<SourceStamp, String> {
    if entry.kind != kind {
        return Err(format!(
            "source `{id}` is a `{}` row, not {}",
            entry.kind.as_str(),
            kind.as_str()
        ));
    }
    let stamp = entry.fetch_stamp(id)?;
    let rows = store
        .switches(Some(id))
        .await
        .map_err(|e| format!("source `{id}`: its runtime switch does not read: {e:#}"))?;
    if let Some(off) = switched_off(&rows, id) {
        return Err(format!(
            "source `{id}` is switched off at runtime since {} ({}) — `tengu sources enable --source {id}` lifts it",
            fmt_time(off.at_ms),
            off.reason
        ));
    }
    Ok(stamp)
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
