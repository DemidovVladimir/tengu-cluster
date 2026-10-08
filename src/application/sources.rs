//! Source evidence use cases (O2): the as-of evidence packet of the source
//! store (`tengu sources asof`; the `source_evidence` tool next) and a row's
//! retention purge (`tengu sources purge`). Reads through
//! `ports::source_store`, rules from `[sources]`; no network.
//!
//! | Use case | Rule |
//! |---|---|
//! | [`evidence_as_of`] | refused for a source no registry row names; no store yet (`None`) = an empty packet, nothing created; the store read is seeded by the queried source (else every record) with `min(published, observed) ≤ t`, whole items and events added (`SourceStore::records`); every coverage row and tombstone; the registry's policies; then `EvidencePacket::build` filters by entity / event / publication — so the freshness counts are exact |
//! | [`purge_request`] | a row's retention as cutoffs: `raw_retention_days` / `record_retention_days` = N > 0 ⇒ before `now − N days`; `0` or unset ⇒ none (kept forever); both none ⇒ nothing to purge |

use anyhow::{bail, Result};

use crate::config::sources::{SourceEntry, SourcesConfig};
use crate::domain::source::{AsOfInput, AsOfMode, AsOfQuery, EvidencePacket};
use crate::ports::source_store::{PurgeRequest, RecordQuery, SourceStore};

const DAY_MS: i64 = 86_400_000;

/// One as-of question (module table).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AsOfRequest {
    pub at_ms: i64,
    pub mode: AsOfMode,
    pub source: Option<String>,
    pub entity: Option<String>,
    pub event_key: Option<String>,
    pub published_from_ms: Option<i64>,
}

/// The evidence packet of `store` at `req.at_ms` (module table); `None` =
/// nothing stored yet (an empty packet; no store is created).
pub(crate) async fn evidence_as_of(
    store: Option<&dyn SourceStore>,
    registry: &SourcesConfig,
    req: &AsOfRequest,
) -> Result<EvidencePacket> {
    if let Some(s) = req.source.as_deref() {
        if !registry.registry.contains_key(s) {
            bail!(
                "no [sources.registry.{s}] row (rows: {})",
                registry
                    .registry
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
    let (records, coverage, purges) = match store {
        None => (Vec::new(), Vec::new(), Vec::new()),
        Some(store) => (
            store
                .records(&RecordQuery {
                    source_id: req.source.clone(),
                    entity: None,
                    event_key: None,
                    upto_ms: req.at_ms,
                })
                .await?,
            store.coverage(None).await?,
            store.purges(None).await?,
        ),
    };
    let policies = registry.policies();
    let input = AsOfInput {
        records: &records,
        coverage: &coverage,
        purges: &purges,
        policies: &policies,
    };
    let query = AsOfQuery {
        source: req.source.clone(),
        entity: req.entity.clone(),
        event_key: req.event_key.clone(),
        published_from_ms: req.published_from_ms,
    };
    Ok(EvidencePacket::build(&input, req.at_ms, req.mode, &query))
}

/// Row `id`'s retention applied at `now_ms` (module table); `None` when it
/// keeps everything forever.
pub(crate) fn purge_request(id: &str, entry: &SourceEntry, now_ms: i64) -> Option<PurgeRequest> {
    let cut = |days: Option<u32>| {
        days.filter(|d| *d > 0)
            .map(|d| now_ms.saturating_sub(i64::from(d) * DAY_MS))
    };
    let raw_before_ms = cut(entry.raw_retention_days);
    let records_before_ms = cut(entry.record_retention_days);
    if raw_before_ms.is_none() && records_before_ms.is_none() {
        return None;
    }
    let days = |d: Option<u32>| match d {
        Some(0) => "0 (forever)".to_string(),
        Some(n) => n.to_string(),
        None => "unset (forever)".to_string(),
    };
    Some(PurgeRequest {
        source_id: id.to_string(),
        raw_before_ms,
        records_before_ms,
        now_ms,
        reason: format!(
            "retention: raw_retention_days = {}, record_retention_days = {} (tengu sources purge)",
            days(entry.raw_retention_days),
            days(entry.record_retention_days)
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::outbound::sources::store::SqliteSourceStore;
    use crate::domain::canonical::canonical_json;
    use crate::domain::source::testkit::{worlds, D, H};
    use crate::ports::source_store::{Batch, Snapshot, SnapshotKind};

    fn registry(text: &str) -> SourcesConfig {
        toml::from_str(text).unwrap_or_else(|e| panic!("{e}\n{text}"))
    }

    fn row(raw: Option<u32>, records: Option<u32>) -> SourceEntry {
        let mut text = r#"
            kind = "ted_search"
            class = "law_regulator"
            trust = "primary"
            revision = "immutable"
            enabled = false
            hosts = ["api.ted.europa.eu"]
            auth = "none"
            rate_limit = "ted"
            store_raw = true
            jurisdiction = "EU"
            language = "en"
            query = "publication-date >= {from} AND publication-date <= {to}"
        "#
        .to_string();
        if let Some(r) = raw {
            text.push_str(&format!("raw_retention_days = {r}\n"));
        }
        if let Some(r) = records {
            text.push_str(&format!("record_retention_days = {r}\n"));
        }
        toml::from_str(&text).unwrap()
    }

    #[test]
    fn retention_becomes_cutoffs() {
        let now = 1_760_000_000_000;
        let req = purge_request("ted_search", &row(Some(90), Some(0)), now).unwrap();
        assert_eq!(req.raw_before_ms, Some(now - 90 * DAY_MS));
        assert_eq!(req.records_before_ms, None);
        assert_eq!(
            req.reason,
            "retention: raw_retention_days = 90, record_retention_days = 0 (forever) (tengu sources purge)"
        );
        let both = purge_request("ted_search", &row(Some(1), Some(365)), now).unwrap();
        assert_eq!(
            (both.raw_before_ms, both.records_before_ms),
            (Some(now - DAY_MS), Some(now - 365 * DAY_MS))
        );
        for (raw, records) in [(Some(0), Some(0)), (None, None), (Some(0), None)] {
            assert_eq!(purge_request("ted_search", &row(raw, records), now), None);
        }
    }

    /// The store-backed packet equals the packet of the whole world for
    /// every test world, at several instants, in both modes.
    #[tokio::test]
    async fn evidence_as_of_reads_the_store_like_the_whole_world() {
        for w in worlds() {
            let dir = tempfile::tempdir().unwrap();
            let store = SqliteSourceStore::open(dir.path()).unwrap();
            let mut snapshots: Vec<Snapshot> = Vec::new();
            for r in &w.records {
                for s in &r.snapshots {
                    if !snapshots.iter().any(|x| &x.sha256 == s) {
                        let mut snap = Snapshot::read(
                            SnapshotKind::Response,
                            &r.source_id,
                            "test",
                            "https://example.org/x",
                            200,
                            "application/json",
                            r.observed_ms,
                            b"",
                            false,
                        );
                        snap.sha256 = s.clone();
                        snapshots.push(snap);
                    }
                }
            }
            store
                .commit(Batch {
                    snapshots,
                    records: w.records.clone(),
                    coverage: w.coverage.clone(),
                    cursor: None,
                })
                .await
                .unwrap();
            let mut text = "state = \"soe\"\n".to_string();
            for id in w.policies.keys() {
                text.push_str(&format!(
                    "[registry.{id}]\nkind = \"ted_search\"\nclass = \"law_regulator\"\ntrust = \"primary\"\n\
                     revision = \"{}\"\nenabled = false\nhosts = [\"example.org\"]\nauth = \"none\"\n\
                     rate_limit = \"x\"\nstore_raw = false\njurisdiction = \"EU\"\nlanguage = \"en\"\n",
                    w.policies[id].revision.as_str()
                ));
                if let Some(ms) = w.policies[id].listing_max_age_ms {
                    text.push_str(&format!("listing_max_age_days = {}\n", ms / D));
                }
            }
            let reg = registry(&text);
            assert_eq!(reg.policies(), w.policies, "{}", w.name);
            for t in [w.span.0, w.span.0 + 12 * H, w.span.1] {
                for mode in [AsOfMode::Captured, AsOfMode::Knowable] {
                    let req = AsOfRequest {
                        at_ms: t,
                        mode,
                        source: None,
                        entity: None,
                        event_key: None,
                        published_from_ms: None,
                    };
                    let got = evidence_as_of(Some(&store), &reg, &req).await.unwrap();
                    // The world's tombstones are not in this store (the
                    // store's own tests cover purges).
                    let input = AsOfInput {
                        purges: &[],
                        ..w.input()
                    };
                    let want = EvidencePacket::build(&input, t, mode, &AsOfQuery::default());
                    assert_eq!(
                        canonical_json(&serde_json::to_value(&got).unwrap()),
                        canonical_json(&serde_json::to_value(&want).unwrap()),
                        "{} at {t} {mode:?}",
                        w.name
                    );
                }
            }
            let unknown = AsOfRequest {
                at_ms: w.span.1,
                mode: AsOfMode::Captured,
                source: Some("nope".into()),
                entity: None,
                event_key: None,
                published_from_ms: None,
            };
            let e = evidence_as_of(None, &reg, &unknown)
                .await
                .unwrap_err()
                .to_string();
            assert!(e.contains("no [sources.registry.nope] row"), "{e}");
        }
    }
}
