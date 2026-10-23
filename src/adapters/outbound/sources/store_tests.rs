//! Tests of `src/adapters/outbound/sources/store.rs`.

use serde_json::Value;

use super::*;
use crate::domain::canonical::canonical_json;
use crate::domain::marketdata::fmt_time;
use crate::domain::source::record::{sec_cik_entity, Origin};
use crate::domain::source::testkit::{copy_of, edited, rec, worlds, Src, D, H, T0};
use crate::domain::source::{AsOfInput, AsOfMode, AsOfQuery, EvidencePacket};
use crate::ports::source_store::switched_off;

fn open() -> (tempfile::TempDir, SqliteSourceStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSourceStore::open(dir.path()).unwrap();
    (dir, store)
}

/// A metadata-only snapshot row for every sha256 `records` name.
fn snapshots_of(records: &[SourceRecord]) -> Vec<Snapshot> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for r in records {
        for s in &r.snapshots {
            if seen.insert(s.clone()) {
                out.push(Snapshot {
                    sha256: s.clone(),
                    kind: SnapshotKind::Response,
                    source_id: r.source_id.clone(),
                    request_key: format!("test:{}", r.native_id),
                    url: r.url.clone(),
                    http_status: 200,
                    content_type: "application/json".into(),
                    fetched_ms: r.observed_ms,
                    bytes: 0,
                    body: None,
                    purged_ms: None,
                });
            }
        }
    }
    out
}

fn batch(records: Vec<SourceRecord>) -> Batch {
    Batch {
        snapshots: snapshots_of(&records),
        records,
        ..Default::default()
    }
}

fn everything(upto_ms: i64) -> RecordQuery {
    RecordQuery {
        upto_ms,
        ..Default::default()
    }
}

fn json(r: &SourceRecord) -> String {
    serde_json::to_string(r).unwrap()
}

/// The WAL and its index hold the same pages as the database (raw
/// bodies included): each is owner-only while the store is open, and a
/// file left wider by an older binary is narrowed on open.
#[cfg(unix)]
#[tokio::test]
async fn db_wal_and_shm_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    let dir = tempfile::tempdir().unwrap();
    let db = sources_db(dir.path());
    let files = [
        db.clone(),
        PathBuf::from(format!("{}-wal", db.display())),
        PathBuf::from(format!("{}-shm", db.display())),
    ];
    let store = SqliteSourceStore::open(dir.path()).unwrap();
    let sec = rec(Src::SEC, "0000000001-26-000001", T0, T0 + H, T0 + H);
    store.commit(batch(vec![sec])).await.unwrap();
    for f in &files {
        assert!(f.is_file(), "{} missing", f.display());
        assert_eq!(mode(f), 0o600, "{}: {:o}", f.display(), mode(f));
    }
    // A file left 0644 (an earlier binary) is narrowed on the next open.
    for f in &files {
        std::fs::set_permissions(f, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    let again = SqliteSourceStore::open(dir.path()).unwrap();
    for f in &files {
        assert_eq!(
            mode(f),
            0o600,
            "{} after reopen: {:o}",
            f.display(),
            mode(f)
        );
    }
    drop((store, again));
}

#[tokio::test]
async fn records_round_trip_with_full_ids() {
    let (_dir, store) = open();
    let sec = rec(Src::SEC, "0000000001-26-000001", T0, T0 + H, T0 + H + 5);
    let ted = rec(Src::TED, "600001-2026", T0 + D, T0 + D + H, T0 + D + H);
    let wire = copy_of(&sec, Src::WIRE, "wire:story:0001", T0 + 2 * H, T0 + 3 * H);
    let report = store
        .commit(batch(vec![sec.clone(), ted.clone(), wire.clone()]))
        .await
        .unwrap();
    assert_eq!(
        report,
        CommitReport {
            snapshots_new: 3,
            records_new: 3,
            coverage_new: 0,
            cursor_moved: false
        }
    );
    let back = store.records(&everything(i64::MAX)).await.unwrap();
    assert_eq!(
        back,
        vec![sec.clone(), wire.clone(), ted.clone()],
        "by read time"
    );
    for r in &back {
        assert_eq!(r.validate(), Ok(()));
    }
    let text: Vec<String> = back.iter().map(json).collect();
    for whole in [
        sec.record_id.as_str(),
        "0000000001-26-000001",
        "sec:cik:0000000001",
        "00000000-0000-4000-8000-000000000000",
        ted.content_hash.as_str(),
        wire.origin.as_ref().unwrap().native_id.as_deref().unwrap(),
    ] {
        assert!(
            text.iter().any(|t| t.contains(whole)),
            "`{whole}` not whole"
        );
    }
    // By entity, source and event; and the snapshot rows.
    let q = RecordQuery {
        entity: Some(sec_cik_entity("0000000001")),
        ..everything(i64::MAX)
    };
    assert_eq!(
        store.records(&q).await.unwrap(),
        vec![sec.clone(), wire.clone()]
    );
    let q = RecordQuery {
        source_id: Some("ted_search".into()),
        ..everything(i64::MAX)
    };
    assert_eq!(store.records(&q).await.unwrap(), vec![ted.clone()]);
    let s = store.snapshot(&sec.snapshots[0]).await.unwrap().unwrap();
    assert_eq!(
        (s.sha256.as_str(), s.source_id.as_str(), s.body),
        (sec.snapshots[0].as_str(), "sec_edgar", None)
    );
    assert_eq!(store.snapshot(&"0".repeat(64)).await.unwrap(), None);
}

#[tokio::test]
async fn refetch_of_the_same_content_adds_nothing() {
    let (_dir, store) = open();
    let r = rec(Src::SEC, "0000000001-26-000001", T0, T0 + H, T0 + H);
    store.commit(batch(vec![r.clone()])).await.unwrap();
    // The same batch again, and a re-read a day later (new bytes, a
    // newer parser, the same content): no record added.
    let again = store.commit(batch(vec![r.clone()])).await.unwrap();
    assert_eq!(again, CommitReport::default());
    let mut later = r.clone();
    later.observed_ms += D;
    later.parsed_ms += D;
    later.parser_version = "sec-submissions/2".into();
    later.snapshots = vec![crate::domain::canonical::sha256_hex("re-read body")];
    let later = later.with_identity();
    assert_eq!(later.record_id, r.record_id);
    let report = store.commit(batch(vec![later.clone()])).await.unwrap();
    assert_eq!((report.snapshots_new, report.records_new), (1, 0));
    // An older read of the same content imported later never moves the
    // stored read earlier (later = conservative in both clocks).
    let mut older = r.clone();
    older.observed_ms -= H;
    older.parsed_ms -= H;
    let report = store
        .commit(batch(vec![older.with_identity()]))
        .await
        .unwrap();
    assert_eq!(report.records_new, 0);
    let back = store.records(&everything(i64::MAX)).await.unwrap();
    assert_eq!(back, vec![r]);
}

#[tokio::test]
async fn changed_content_appends_a_version() {
    let (_dir, store) = open();
    let r = rec(Src::REPO, "item-1", T0, T0 + H, T0 + H);
    store.commit(batch(vec![r.clone()])).await.unwrap();
    let before = json(&store.records(&everything(i64::MAX)).await.unwrap()[0]);
    let e = edited(&r, T0 + D, "release r1");
    let report = store.commit(batch(vec![e.clone()])).await.unwrap();
    assert_eq!(report.records_new, 1);
    let back = store.records(&everything(i64::MAX)).await.unwrap();
    assert_eq!(back, vec![r.clone(), e.clone()]);
    assert_eq!(
        json(&back[0]),
        before,
        "the first version is byte-identical"
    );
    assert_eq!(
        (back[0].native_id.as_str(), back[1].native_id.as_str()),
        ("item-1", "item-1")
    );
    assert_ne!(back[0].record_id, back[1].record_id);
}

#[tokio::test]
async fn cursor_moves_only_with_a_committed_batch() {
    let (_dir, store) = open();
    let cursor = |value: &str| Cursor {
        source_id: "sec_edgar".into(),
        query_key: "cik:0000000001".into(),
        value: value.into(),
        updated_ms: T0,
    };
    let cov = Coverage {
        source_id: "sec_edgar".into(),
        query_key: "cik:0000000001".into(),
        fetched_ms: T0 + 2 * H,
        from_ms: T0 - D,
        to_ms: T0 + 2 * H,
        complete: true,
        error_class: None,
    };
    let a = rec(Src::SEC, "0000000001-26-000001", T0, T0 + H, T0 + H);
    let report = store
        .commit(Batch {
            cursor: Some(cursor("0000000001-26-000001")),
            coverage: vec![cov.clone()],
            ..batch(vec![a.clone()])
        })
        .await
        .unwrap();
    assert!(report.cursor_moved && report.coverage_new == 1);
    assert_eq!(
        store.cursor("sec_edgar", "cik:0000000001").await.unwrap(),
        Some(cursor("0000000001-26-000001"))
    );

    // One bad record fails the whole batch: no record, coverage or cursor.
    let b = rec(
        Src::SEC,
        "0000000001-26-000002",
        T0 + D,
        T0 + D + H,
        T0 + D + H,
    );
    let mut bad = rec(
        Src::SEC,
        "0000000001-26-000003",
        T0 + D,
        T0 + D + H,
        T0 + D + H,
    );
    bad.published_ms += 1; // content changed, identity not recomputed
    let cov2 = Coverage {
        fetched_ms: T0 + D + 2 * H,
        ..cov.clone()
    };
    let e = store
        .commit(Batch {
            cursor: Some(cursor("0000000001-26-000003")),
            coverage: vec![cov2.clone()],
            ..batch(vec![b.clone(), bad.clone()])
        })
        .await
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("nothing written") && e.contains("content_hash"),
        "{e}"
    );
    assert_eq!(
        store
            .cursor("sec_edgar", "cik:0000000001")
            .await
            .unwrap()
            .unwrap()
            .value,
        "0000000001-26-000001"
    );
    assert_eq!(
        store.records(&everything(i64::MAX)).await.unwrap(),
        vec![a.clone()]
    );
    assert_eq!(store.coverage(None).await.unwrap(), vec![cov.clone()]);

    // A record naming a snapshot the store does not know; a body that
    // is not its sha256; a complete fetch with an error class.
    let e = store
        .commit(Batch {
            records: vec![b.clone()],
            ..Default::default()
        })
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("neither in the batch nor stored"), "{e}");
    let mut lie = Snapshot::read(
        SnapshotKind::Response,
        "sec_edgar",
        "submissions:0000000001",
        "https://data.sec.gov/submissions/CIK0000000001.json",
        200,
        "application/json",
        T0,
        b"{}",
        true,
    );
    lie.body = Some(b"[]".to_vec());
    let e = store
        .commit(Batch {
            snapshots: vec![lie],
            coverage: vec![Coverage {
                error_class: Some("fatal".into()),
                ..cov2
            }],
            ..Default::default()
        })
        .await
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("the body's sha256") && e.contains("no error_class"),
        "{e}"
    );
    // The good part alone commits; the cursor then moves.
    store
        .commit(Batch {
            cursor: Some(cursor("0000000001-26-000002")),
            ..batch(vec![b.clone()])
        })
        .await
        .unwrap();
    assert_eq!(
        store
            .cursor("sec_edgar", "cik:0000000001")
            .await
            .unwrap()
            .unwrap()
            .value,
        "0000000001-26-000002"
    );
}

#[tokio::test]
async fn purge_leaves_tombstones_and_hashes() {
    let (_dir, store) = open();
    let body = |s: &str| s.as_bytes().to_vec();
    let old_body = Snapshot::read(
        SnapshotKind::Response,
        "sec_edgar",
        "index:0000000001-26-000001",
        "https://www.sec.gov/x-index.htm",
        200,
        "text/html",
        T0,
        &body("old page, Jane Doe +1 555 0100"),
        true,
    );
    let new_body = Snapshot::read(
        SnapshotKind::Response,
        "sec_edgar",
        "index:0000000001-26-000002",
        "https://www.sec.gov/y-index.htm",
        200,
        "text/html",
        T0 + 2 * D,
        &body("new page"),
        true,
    );
    let terms = Snapshot::terms(
        "sec_edgar",
        "https://www.sec.gov/terms",
        T0 - D,
        b"terms v1",
    );
    let mut old = rec(Src::SEC, "0000000001-26-000001", T0, T0, T0);
    old.snapshots = vec![old_body.sha256.clone()];
    let old = old.with_identity();
    let mut new = rec(
        Src::SEC,
        "0000000001-26-000002",
        T0 + 2 * D,
        T0 + 2 * D,
        T0 + 2 * D,
    );
    new.snapshots = vec![new_body.sha256.clone()];
    let new = new.with_identity();
    store
        .commit(Batch {
            snapshots: vec![old_body.clone(), new_body.clone(), terms.clone()],
            records: vec![old.clone(), new.clone()],
            ..Default::default()
        })
        .await
        .unwrap();

    // Raw retention: the old body goes; its hash, size and record stay;
    // the terms page is never purged.
    let now = T0 + 3 * D;
    let raw = store
        .purge(&PurgeRequest {
            source_id: "sec_edgar".into(),
            raw_before_ms: Some(T0 + D),
            records_before_ms: None,
            now_ms: now,
            reason: "raw_retention_days = 1".into(),
        })
        .await
        .unwrap();
    assert_eq!((raw.snapshots, raw.records), (1, 0));
    let gone = store.snapshot(&old_body.sha256).await.unwrap().unwrap();
    assert_eq!((gone.body, gone.purged_ms), (None, Some(now)));
    assert_eq!(
        (gone.sha256.as_str(), gone.bytes),
        (old_body.sha256.as_str(), old_body.bytes)
    );
    let kept = store.snapshot(&terms.sha256).await.unwrap().unwrap();
    assert_eq!(
        (kept.kind, kept.body.as_deref()),
        (SnapshotKind::Terms, Some(&b"terms v1"[..]))
    );
    assert_eq!(
        store
            .snapshot(&new_body.sha256)
            .await
            .unwrap()
            .unwrap()
            .body,
        new_body.body
    );
    let back = store.records(&everything(i64::MAX)).await.unwrap();
    assert_eq!(
        back,
        vec![old.clone(), new.clone()],
        "records keep their hashes"
    );

    // Record retention: the old record and its entity rows go.
    let recs = store
        .purge(&PurgeRequest {
            source_id: "sec_edgar".into(),
            raw_before_ms: None,
            records_before_ms: Some(T0 + D),
            now_ms: now + H,
            reason: "record_retention_days = 1".into(),
        })
        .await
        .unwrap();
    assert_eq!((recs.snapshots, recs.records), (0, 1));
    let q = RecordQuery {
        entity: Some(old.entities[0].clone()),
        ..everything(i64::MAX)
    };
    assert_eq!(store.records(&q).await.unwrap(), vec![new.clone()]);
    // A purge that removes nothing leaves no tombstone; a reason is required.
    let none = store
        .purge(&PurgeRequest {
            source_id: "sec_edgar".into(),
            raw_before_ms: Some(T0 + D),
            records_before_ms: Some(T0 + D),
            now_ms: now + 2 * H,
            reason: "again".into(),
        })
        .await
        .unwrap();
    assert_eq!((none.snapshots, none.records), (0, 0));
    assert!(store
        .purge(&PurgeRequest {
            source_id: "sec_edgar".into(),
            raw_before_ms: Some(T0),
            records_before_ms: None,
            now_ms: now,
            reason: " ".into(),
        })
        .await
        .is_err());
    let tombs = store.purges(None).await.unwrap();
    assert_eq!(tombs, vec![raw, recs]);
    assert!(store.purges(Some("ted_search")).await.unwrap().is_empty());
    assert_eq!(store.purges(Some("sec_edgar")).await.unwrap().len(), 2);
}

/// Append-only by construction: no statement replaces a row, none
/// updates a record; records are inserted `OR IGNORE` and deleted only
/// by the record purge.
#[test]
fn no_replace_or_update_on_records_sql() {
    for sql in ALL_SQL {
        let s = sql.to_ascii_uppercase();
        let words: BTreeSet<&str> = s
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .filter(|w| !w.is_empty())
            .collect();
        assert!(!words.contains("REPLACE"), "{sql}");
        if words.contains("RECORDS") || words.contains("RECORD_ENTITIES") {
            assert!(!words.contains("UPDATE"), "{sql}");
        }
        if words.contains("INSERT") {
            assert!(
                s.contains("INSERT OR IGNORE")
                    || s.contains("INTO CURSORS")
                    || s.contains("INTO PURGES")
                    || s.contains("INTO SWITCHES"),
                "{sql}"
            );
        }
        if words.contains("UPDATE") {
            assert!(
                sql == PURGE_RAW_SQL || sql == MOVE_CURSOR_SQL,
                "only the raw purge and the cursor update: {sql}"
            );
        }
        if words.contains("DELETE") {
            assert!(
                sql == PURGE_RECORDS_SQL || sql == PURGE_ENTITIES_SQL,
                "only the record purge deletes: {sql}"
            );
        }
    }
    assert!(PUT_RECORD_SQL.contains("INSERT OR IGNORE INTO records"));
    assert!(PUT_SNAPSHOT_SQL.contains("INSERT OR IGNORE INTO snapshots"));
    assert!(
        PURGE_RAW_SQL.contains("kind = 'response'"),
        "terms pages stay"
    );
}

/// A read at t keeps whole items and events: the first read of an item
/// (the knowable clock reads it) and a report of the event that lacks
/// the queried entity both come along.
#[tokio::test]
async fn a_read_at_t_brings_whole_items_and_events() {
    let (_dir, store) = open();
    let t = T0 + 10 * D;
    // First read after t, published after t; a later version names an
    // earlier publication (an edit under an immutable id).
    let first = rec(Src::SEC, "0000000001-26-000009", t + 5, t + 10, t + 10);
    let mut edit = edited(&first, t + 20, "Other events, restated");
    edit.published_ms = t - 5;
    edit.valid_from_ms = t - 5;
    let edit = edit.with_identity();
    // An outlet's report of another filing, without the CIK entity.
    let filing = rec(Src::SEC, "0000000001-26-000001", T0, T0 + H, T0 + H);
    let mut report = copy_of(&filing, Src::DAILY, "daily:0001", T0 + 2 * H, T0 + 3 * H);
    report.origin = None::<Origin>;
    report.entities = vec!["daily:desk:markets".into()];
    let report = report.with_identity();
    let all_records = vec![first.clone(), edit.clone(), filing.clone(), report.clone()];
    store.commit(batch(all_records.clone())).await.unwrap();

    let q = RecordQuery {
        entity: Some(sec_cik_entity("0000000001")),
        ..everything(t)
    };
    let read = store.records(&q).await.unwrap();
    for r in [&first, &edit, &filing, &report] {
        assert!(read.contains(r), "{} missing", r.record_id);
    }
    let policies = crate::domain::source::testkit::policies(&[Src::SEC, Src::DAILY]);
    for mode in [AsOfMode::Captured, AsOfMode::Knowable] {
        let input = AsOfInput {
            records: &read,
            coverage: &[],
            purges: &[],
            policies: &policies,
        };
        let p = EvidencePacket::build(&input, t, mode, &AsOfQuery::default());
        assert!(
            p.facts.iter().all(|f| f.record_id != edit.record_id),
            "{mode:?}: the edit read after t leaked"
        );
        let whole = AsOfInput {
            records: &all_records,
            ..input
        };
        assert_eq!(
            canonical_json(&serde_json::to_value(&p).unwrap()),
            canonical_json(
                &serde_json::to_value(EvidencePacket::build(
                    &whole,
                    t,
                    mode,
                    &AsOfQuery::default()
                ))
                .unwrap()
            )
        );
    }
}

fn canon(p: &EvidencePacket) -> Value {
    serde_json::from_str(&canonical_json(&serde_json::to_value(p).unwrap())).unwrap()
}

/// The store's reads give the packet the whole store gives, for the
/// testkit worlds: every source query exactly; an entity or event
/// query in everything but freshness (port table: fidelity).
#[tokio::test]
async fn store_reads_give_the_packet_of_the_whole_store() {
    for w in worlds() {
        let (_dir, store) = open();
        let mut records = w.records.clone();
        records.sort_by_key(|r| (r.observed_ms, r.parsed_ms));
        store
            .commit(Batch {
                coverage: w.coverage.clone(),
                ..batch(records)
            })
            .await
            .unwrap();
        let coverage = store.coverage(None).await.unwrap();
        let mut want = w.coverage.clone();
        want.sort_by(|a, b| {
            (&a.source_id, &a.query_key, a.fetched_ms, a.from_ms, a.to_ms).cmp(&(
                &b.source_id,
                &b.query_key,
                b.fetched_ms,
                b.from_ms,
                b.to_ms,
            ))
        });
        assert_eq!(coverage, want, "{}: coverage round-trips", w.name);
        let entities: Vec<String> = w
            .records
            .iter()
            .flat_map(|r| r.entities.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .take(3)
            .collect();
        let events: Vec<String> = w
            .records
            .iter()
            .map(|r| r.event_key.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .step_by(5)
            .take(3)
            .collect();
        let (a, b) = w.span;
        for i in 0..=8 {
            let t = a + (b - a) * i / 8;
            let mut cases: Vec<(AsOfQuery, RecordQuery, bool)> =
                vec![(AsOfQuery::default(), everything(t), true)];
            for s in w.policies.keys() {
                cases.push((
                    AsOfQuery {
                        source: Some(s.clone()),
                        ..Default::default()
                    },
                    RecordQuery {
                        source_id: Some(s.clone()),
                        ..everything(t)
                    },
                    true,
                ));
            }
            for e in &entities {
                cases.push((
                    AsOfQuery {
                        entity: Some(e.clone()),
                        ..Default::default()
                    },
                    RecordQuery {
                        entity: Some(e.clone()),
                        ..everything(t)
                    },
                    false,
                ));
            }
            for k in &events {
                cases.push((
                    AsOfQuery {
                        event_key: Some(k.clone()),
                        ..Default::default()
                    },
                    RecordQuery {
                        event_key: Some(k.clone()),
                        ..everything(t)
                    },
                    false,
                ));
            }
            for (packet_q, store_q, exact) in &cases {
                let read = store.records(store_q).await.unwrap();
                let input = AsOfInput {
                    records: &read,
                    coverage: &coverage,
                    purges: &w.purges,
                    policies: &w.policies,
                };
                for mode in [AsOfMode::Captured, AsOfMode::Knowable] {
                    let mut got = canon(&EvidencePacket::build(&input, t, mode, packet_q));
                    let mut whole = canon(&EvidencePacket::build(&w.input(), t, mode, packet_q));
                    if !exact {
                        got["freshness"] = Value::Null;
                        whole["freshness"] = Value::Null;
                    }
                    assert!(
                        got == whole,
                        "{} at {} {mode:?} {packet_q:?}: the store read differs",
                        w.name,
                        fmt_time(t)
                    );
                }
            }
        }
        assert!(w.records.len() > 10, "{}: not vacuous", w.name);
    }
}

/// Critic U10: the runtime kill switch is an append log — every row
/// kept, the last appended per source wins (never the clock); a
/// version-1 file gains the table.
#[tokio::test]
async fn switches_append_and_the_newest_wins() {
    let (dir, store) = open();
    let row = |enabled: bool, at_ms: i64, reason: &str| SourceSwitch {
        source_id: "ted_search".into(),
        enabled,
        at_ms,
        reason: reason.into(),
    };
    assert!(store.switches(None).await.unwrap().is_empty());
    store
        .set_switch(&row(false, T0, "terms under review"))
        .await
        .unwrap();
    let rows = store.switches(Some("ted_search")).await.unwrap();
    assert_eq!(
        switched_off(&rows, "ted_search").map(|s| s.reason.as_str()),
        Some("terms under review")
    );
    assert_eq!(switched_off(&rows, "sec_edgar"), None);
    store
        .set_switch(&row(true, T0 + H, "reviewed"))
        .await
        .unwrap();
    let rows = store.switches(None).await.unwrap();
    assert_eq!(
        rows,
        vec![
            row(false, T0, "terms under review"),
            row(true, T0 + H, "reviewed")
        ]
    );
    assert_eq!(switched_off(&rows, "ted_search"), None);
    for bad in [
        SourceSwitch {
            source_id: "TED".into(),
            ..row(false, T0, "x")
        },
        row(false, T0, " "),
    ] {
        assert!(store.set_switch(&bad).await.is_err(), "{bad:?}");
    }
    assert_eq!(store.switches(None).await.unwrap().len(), 2);
    // A clock stepped back: the disable appended last is stamped before
    // the enable before it — still off (append order, never the clock).
    store
        .set_switch(&row(true, T0 + 3 * H, "lifted"))
        .await
        .unwrap();
    store
        .set_switch(&row(false, T0 + 2 * H, "stop now"))
        .await
        .unwrap();
    let rows = store.switches(Some("ted_search")).await.unwrap();
    assert_eq!(rows.last().unwrap().reason, "stop now");
    assert_eq!(
        switched_off(&rows, "ted_search").map(|s| s.reason.as_str()),
        Some("stop now")
    );
    // Cursors list by source.
    assert!(store.cursors(None).await.unwrap().is_empty());
    let a = rec(Src::SEC, "0000000001-26-000001", T0, T0 + H, T0 + H);
    let cursor = Cursor {
        source_id: "sec_edgar".into(),
        query_key: "cik:0000000001".into(),
        value: "0000000001-26-000001".into(),
        updated_ms: T0,
    };
    store
        .commit(Batch {
            cursor: Some(cursor.clone()),
            ..batch(vec![a])
        })
        .await
        .unwrap();
    assert_eq!(store.cursors(None).await.unwrap(), vec![cursor.clone()]);
    assert!(store.cursors(Some("ted_search")).await.unwrap().is_empty());
    // A version-1 file opens, gains `switches`, becomes version 2.
    drop(store);
    let path = sources_db(dir.path());
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("DROP TABLE switches; PRAGMA user_version = 1;")
        .unwrap();
    drop(conn);
    let store = SqliteSourceStore::open(dir.path()).unwrap();
    assert!(store.switches(None).await.unwrap().is_empty());
    assert_eq!(store.cursors(None).await.unwrap(), vec![cursor]);
    let conn = Connection::open(&path).unwrap();
    let v: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, SCHEMA_VERSION);
}
