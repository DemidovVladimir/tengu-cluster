//! Tests of `src/adapters/outbound/sources/sec.rs`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use reqwest::header::HeaderValue;
use serde_json::Value;

use super::*;
use crate::adapters::outbound::http_class::test_support::FAST;
use crate::adapters::outbound::http_class::test_support::{
    canned, local_scope, serve, test_client, Canned,
};
use crate::adapters::outbound::rate_limit::Limiters;
use crate::adapters::outbound::sources::store::SqliteSourceStore;
use crate::domain::source::{
    as_of, AsOfInput, AsOfMode, Revision, SourcePolicy, SourceRecord, SupersededBy,
};
use crate::domain::time::parse_time;
use crate::ports::clock::SimClock;
use crate::ports::source_store::SourceSwitch;

const CIK: &str = "0000320193";
const UA: &str = "tengu-test-ua-7d1e ops@example.com";
const HASH: &str = "4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b";

fn fixture(name: &str) -> String {
    let path = format!("{}/tests/fixtures/sec/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn html(name: &str) -> Canned {
    Canned {
        status: 200,
        headers: "Content-Type: text/html\r\n",
        body: fixture(name),
        delay_ms: 0,
    }
}

fn t(s: &str) -> i64 {
    parse_time(s).unwrap()
}

/// A `sec_edgar` row: enabled with synthetic terms unless edited.
fn entry(edit: &str) -> SourceEntry {
    let text = format!(
        r#"
            kind = "sec_edgar"
            class = "company_primary"
            trust = "primary"
            revision = "immutable"
            enabled = true
            hosts = ["www.sec.gov", "data.sec.gov"]
            auth = "user_agent_env:SEC_USER_AGENT"
            rate_limit = "sec"
            store_raw = true
            jurisdiction = "US"
            language = "en"
            license = "synthetic test terms"
            terms_url = "https://example.org/terms"
            terms_sha256 = "{HASH}"
            terms_reviewed_at = "2026-10-08"
            raw_retention_days = 0
            record_retention_days = 0
            {edit}
            "#
    );
    toml::from_str(&text).unwrap_or_else(|e| panic!("{e}\n{text}"))
}

fn client(base: &str, tap: Option<Arc<Mutex<Vec<Value>>>>) -> SecClient {
    let c = SecClient::new(
        test_client(),
        base,
        base,
        local_scope(),
        None,
        HeaderValue::from_static(UA),
    )
    .unwrap()
    .with_limiters(Box::leak(Box::new(Limiters::default())));
    match tap {
        Some(tap) => c.with_audit_tap(tap),
        None => c,
    }
}

fn target(req: &str) -> String {
    req.lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_string()
}

async fn fetch(
    c: &SecClient,
    store: &SqliteSourceStore,
    clock: &SimClock,
    entry: &SourceEntry,
    from: &str,
) -> BackfillReport {
    let f = SecFetch {
        client: c,
        store,
        clock,
        retry: &FAST,
    };
    sec_source_fetch(
        &f,
        "sec_edgar",
        entry,
        &[CIK.to_string()],
        t(from),
        i64::MAX,
    )
    .await
}

async fn all(store: &SqliteSourceStore) -> Vec<SourceRecord> {
    store
        .records(&RecordQuery {
            upto_ms: i64::MAX,
            ..Default::default()
        })
        .await
        .unwrap()
}

fn published(records: &[SourceRecord]) -> Vec<(String, String, ParseStatus)> {
    let mut out: Vec<_> = records
        .iter()
        .map(|r| (fmt_time(r.published_ms), r.native_id.clone(), r.parse))
        .collect();
    out.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    out
}

/// AAPL from 2026-01-01: the submissions file, one index page per kept
/// filing (a 503 retried); times are the index's; every body kept as a
/// snapshot; a re-run an hour later reads the submissions only and adds
/// no record.
#[tokio::test]
async fn fetch_reuses_the_index_time_and_stores_snapshots() {
    let (base, seen) = serve(vec![
        canned(200, fixture("CIK0000320193.json")),
        html("0001140361-26-035325-index.htm"),
        canned(503, "busy"),
        html("0000320193-26-000020-index.htm"),
        html("0000320193-26-000018-index.htm"),
        html("0000320193-26-000006-index.htm"),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSourceStore::open(dir.path()).unwrap();
    let clock = SimClock::at(t("2026-10-08T12:00:00Z"));
    let c = client(&base, None);
    let row_entry = entry("");
    let report = fetch(&c, &store, &clock, &row_entry, "2026-01-01").await;
    assert_eq!(report.error_count(), 0, "{}", report.render());
    let row = &report.rows[0];
    assert_eq!(
        (row.instrument.as_str(), row.kind, row.source.as_str()),
        ("sec:cik:0000320193", "filings", "sec_edgar")
    );
    assert_eq!((row.rows, row.reads), (4, 5), "{row:?}");
    let paths: Vec<String> = seen.lock().unwrap().iter().map(|r| target(r)).collect();
    assert_eq!(paths[0], "/submissions/CIK0000320193.json");
    assert_eq!(paths.len(), 6, "the 503 was retried: {paths:?}");

    let records = all(&store).await;
    assert_eq!(
        published(&records),
        vec![
            (
                "2026-01-30T11:01:32Z".into(),
                "0000320193-26-000006".into(),
                ParseStatus::Ok
            ),
            (
                "2026-07-30T20:30:28Z".into(),
                "0000320193-26-000018".into(),
                ParseStatus::Ok
            ),
            (
                "2026-07-31T10:01:02Z".into(),
                "0000320193-26-000020".into(),
                ParseStatus::Ok
            ),
            (
                "2026-09-01T20:30:35Z".into(),
                "0001140361-26-035325".into(),
                ParseStatus::Ok
            ),
        ]
    );
    let subs_sha = crate::domain::canonical::sha256_hex(fixture("CIK0000320193.json").as_bytes());
    for r in &records {
        assert_eq!(r.validate(), Ok(()));
        assert_eq!(r.snapshots.len(), 2, "{}", r.record_id);
        assert_eq!(r.snapshots[0], subs_sha);
        let index = store.snapshot(&r.snapshots[1]).await.unwrap().unwrap();
        let body = fixture(&format!("{}-index.htm", r.native_id));
        assert_eq!(index.body.as_deref(), Some(body.as_bytes()));
        assert_eq!(index.request_key, format!("index:{}", r.native_id));
        assert_eq!(
            (index.kind, index.http_status, index.content_type.as_str()),
            (SnapshotKind::Response, 200, "text/html")
        );
        assert!(index.url.ends_with(&format!("{}-index.htm", r.native_id)));
        assert_eq!(r.observed_ms, index.fetched_ms);
        assert!(r.parsed_ms >= r.observed_ms);
    }
    let subs = store.snapshot(&subs_sha).await.unwrap().unwrap();
    assert_eq!(subs.request_key, "submissions:0000320193");
    assert_eq!(subs.content_type, "application/json");
    assert_eq!(
        subs.bytes,
        fixture("CIK0000320193.json").len() as u64,
        "the size is kept"
    );
    let cov = store.coverage(Some("sec_edgar")).await.unwrap();
    assert_eq!(
        cov,
        vec![Coverage {
            source_id: "sec_edgar".into(),
            query_key: "cik:0000320193".into(),
            fetched_ms: t("2026-10-08T12:00:00Z"),
            from_ms: t("2026-01-01"),
            to_ms: t("2026-10-08T12:00:00Z"),
            complete: true,
            error_class: None,
        }]
    );
    let cursor = store
        .cursor("sec_edgar", "cik:0000320193")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        cursor.value, "0001140361-26-035325",
        "the newest by published time"
    );

    // An hour later: the submissions only; nothing new but the coverage.
    clock.advance(3_600_000);
    let (base, seen) = serve(vec![canned(200, fixture("CIK0000320193.json"))]).await;
    let c = client(&base, None);
    let report = fetch(&c, &store, &clock, &row_entry, "2026-01-01").await;
    assert_eq!(report.error_count(), 0, "{}", report.render());
    assert_eq!((report.rows[0].rows, report.rows[0].reads), (0, 1));
    assert_eq!(report.rows[0].first_ms, None);
    assert!(report.rows[0].notes.contains(
        &"4 filing(s) in the span; 0 index page(s) read, 4 already stored (time kept)".to_string()
    ));
    assert_eq!(seen.lock().unwrap().len(), 1);
    assert_eq!(
        all(&store).await,
        records,
        "the same content adds no record"
    );
    assert_eq!(store.coverage(None).await.unwrap().len(), 2);

    // `store_raw = false`: hashes and sizes, no bodies.
    let (base, _) = serve(vec![
        canned(200, fixture("CIK0000320193.json")),
        html("0001140361-26-035325-index.htm"),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();
    let lean = SqliteSourceStore::open(dir.path()).unwrap();
    let c = client(&base, None);
    let lean_entry = SourceEntry {
        store_raw: false,
        ..entry("")
    };
    let report = fetch(&c, &lean, &clock, &lean_entry, "2026-08-15").await;
    assert_eq!(report.error_count(), 0, "{}", report.render());
    let only = all(&lean).await;
    assert_eq!(only.len(), 1);
    for s in &only[0].snapshots {
        let snap = lean.snapshot(s).await.unwrap().unwrap();
        assert!(snap.body.is_none() && snap.bytes > 0, "{snap:?}");
    }
}

/// An index page answered 404: the filing is a `partial` record with a
/// time never before its acceptance, the coverage is incomplete and the
/// cursor stays; a later read appends the version with the true time,
/// and the as-of view takes it as current.
#[tokio::test]
async fn index_404_is_partial_not_a_loss() {
    let (base, _) = serve(vec![
        canned(200, fixture("CIK0000320193.json")),
        html("0001140361-26-035325-index.htm"),
        canned(404, "<html>Not Found</html>"),
        html("0000320193-26-000018-index.htm"),
        html("0000320193-26-000006-index.htm"),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSourceStore::open(dir.path()).unwrap();
    let clock = SimClock::at(t("2026-10-08T12:00:00Z"));
    let row_entry = entry("");
    let report = fetch(
        &client(&base, None),
        &store,
        &clock,
        &row_entry,
        "2026-01-01",
    )
    .await;
    let row = &report.rows[0];
    assert_eq!(row.rows, 4, "{}", report.render());
    assert_eq!(row.errors.len(), 1, "{row:?}");
    assert!(
        row.errors[0].starts_with("index 0000320193-26-000020: HTTP 404"),
        "{:?}",
        row.errors
    );
    let records = all(&store).await;
    let partial = records
        .iter()
        .find(|r| r.native_id == "0000320193-26-000020")
        .unwrap();
    assert_eq!(partial.parse, ParseStatus::Partial);
    assert_eq!(partial.parse_errors[0].field, "index");
    assert_eq!(
        partial.parse_errors[0].message,
        "index page not read (fatal, HTTP 404)"
    );
    assert_eq!(partial.snapshots.len(), 1, "the submissions body only");
    let truth = t("2026-07-31T10:01:02Z");
    assert!(partial.published_ms >= truth, "never early");
    let cov = store.coverage(None).await.unwrap();
    assert_eq!(
        (cov[0].complete, cov[0].error_class.as_deref()),
        (false, Some("fatal"))
    );
    assert_eq!(
        store.cursor("sec_edgar", "cik:0000320193").await.unwrap(),
        None
    );

    // Read again: only the missing index; a new version, the old stays.
    clock.advance(3_600_000);
    let (base, seen) = serve(vec![
        canned(200, fixture("CIK0000320193.json")),
        html("0000320193-26-000020-index.htm"),
    ])
    .await;
    let report = fetch(
        &client(&base, None),
        &store,
        &clock,
        &row_entry,
        "2026-01-01",
    )
    .await;
    assert_eq!(report.error_count(), 0, "{}", report.render());
    assert_eq!(seen.lock().unwrap().len(), 2);
    let records = all(&store).await;
    assert_eq!(records.len(), 5);
    assert!(records.contains(partial), "the partial version stays");
    let fixed = records
        .iter()
        .find(|r| r.native_id == "0000320193-26-000020" && r.parse == ParseStatus::Ok)
        .unwrap();
    assert_eq!(fixed.published_ms, truth);
    assert_eq!(
        store
            .cursor("sec_edgar", "cik:0000320193")
            .await
            .unwrap()
            .unwrap()
            .value,
        "0001140361-26-035325"
    );
    let cov = store.coverage(None).await.unwrap();
    let policies = BTreeMap::from([(
        "sec_edgar".to_string(),
        SourcePolicy {
            revision: Revision::Immutable,
            listing_max_age_ms: None,
        },
    )]);
    let input = AsOfInput {
        records: &records,
        coverage: &cov,
        purges: &[],
        policies: &policies,
    };
    let view = as_of(&input, clock.now_ms(), AsOfMode::Captured);
    let current = view
        .current
        .iter()
        .find(|c| c.record.native_id == "0000320193-26-000020")
        .unwrap();
    assert_eq!(current.record.record_id, fixed.record_id);
    assert!(view.superseded.iter().any(|s| s.old == partial.record_id
        && s.new == fixed.record_id
        && s.by == SupersededBy::Revision));
}

/// A failure that is no answer about the page (a 429 after the
/// retries) stops the CIK: what was read stays, the coverage is
/// incomplete; an undecodable submissions file keeps its bytes.
#[tokio::test]
async fn a_failed_read_stops_the_cik_and_keeps_what_was_read() {
    let mut replies = vec![
        canned(200, fixture("CIK0000320193.json")),
        html("0001140361-26-035325-index.htm"),
    ];
    for _ in 0..4 {
        replies.push(canned(429, "slow down"));
    }
    let (base, _) = serve(replies).await;
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSourceStore::open(dir.path()).unwrap();
    let clock = SimClock::at(t("2026-10-08T12:00:00Z"));
    let report = fetch(
        &client(&base, None),
        &store,
        &clock,
        &entry(""),
        "2026-01-01",
    )
    .await;
    let row = &report.rows[0];
    assert_eq!(row.rows, 1, "{}", report.render());
    assert_eq!(row.classes, vec![ErrorClass::RateLimited], "{row:?}");
    assert_eq!(all(&store).await.len(), 1);
    let cov = store.coverage(None).await.unwrap();
    assert_eq!(
        (cov[0].complete, cov[0].error_class.as_deref()),
        (false, Some("rate_limited"))
    );
    assert_eq!(
        store.cursor("sec_edgar", "cik:0000320193").await.unwrap(),
        None
    );

    let (base, _) = serve(vec![canned(200, "<html>maintenance</html>")]).await;
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSourceStore::open(dir.path()).unwrap();
    let report = fetch(
        &client(&base, None),
        &store,
        &clock,
        &entry(""),
        "2026-01-01",
    )
    .await;
    assert_eq!(report.rows[0].classes, vec![ErrorClass::Decode]);
    assert!(all(&store).await.is_empty());
    let sha = crate::domain::canonical::sha256_hex(b"<html>maintenance</html>");
    let kept = store.snapshot(&sha).await.unwrap().unwrap();
    assert_eq!(kept.request_key, "submissions:0000320193");
    let cov = store.coverage(None).await.unwrap();
    assert_eq!(cov[0].error_class.as_deref(), Some("decode"));
}

/// The User-Agent is sent on every request and stored nowhere: not in
/// a snapshot, record, coverage, cursor, report or audit event.
#[tokio::test]
async fn user_agent_never_lands_in_records_or_audit() {
    let (base, seen) = serve(vec![
        canned(200, fixture("CIK0000320193.json")),
        html("0001140361-26-035325-index.htm"),
        canned(404, "<html>Not Found</html>"),
        html("0000320193-26-000018-index.htm"),
        html("0000320193-26-000006-index.htm"),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSourceStore::open(dir.path()).unwrap();
    let clock = SimClock::at(t("2026-10-08T12:00:00Z"));
    let tap = Arc::new(Mutex::new(Vec::new()));
    let report = fetch(
        &client(&base, Some(tap.clone())),
        &store,
        &clock,
        &entry(""),
        "2026-01-01",
    )
    .await;
    let marker = "tengu-test-ua-7d1e";
    for req in seen.lock().unwrap().iter() {
        assert!(req
            .to_ascii_lowercase()
            .contains(&format!("user-agent: {marker}")));
    }
    let mut stored = vec![report.render()];
    for r in all(&store).await {
        stored.push(serde_json::to_string(&r).unwrap());
        for s in &r.snapshots {
            let snap = store.snapshot(s).await.unwrap().unwrap();
            stored.push(format!(
                "{} {} {}",
                snap.url, snap.request_key, snap.content_type
            ));
            stored.push(String::from_utf8_lossy(snap.body.as_deref().unwrap_or_default()).into());
        }
    }
    stored.push(format!("{:?}", store.coverage(None).await.unwrap()));
    stored.push(format!(
        "{:?}",
        store.cursor("sec_edgar", "cik:0000320193").await.unwrap()
    ));
    let events = tap.lock().unwrap().clone();
    assert_eq!(events.len(), 5, "one audit event per request");
    for e in &events {
        assert_eq!(e["tool"], "sec_edgar");
        stored.push(e.to_string());
    }
    assert!(events.iter().any(|e| e["path"]
        == "/Archives/edgar/data/320193/000032019326000020/0000320193-26-000020-index.htm"
        && e["status"] == 404));
    for text in &stored {
        assert!(!text.contains(marker), "the User-Agent leaked into {text}");
    }
}

#[tokio::test]
async fn a_disabled_or_unreviewed_row_is_never_fetched() {
    let (base, seen) = serve(vec![canned(200, fixture("CIK0000320193.json"))]).await;
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSourceStore::open(dir.path()).unwrap();
    let clock = SimClock::at(t("2026-10-08T12:00:00Z"));
    let c = client(&base, None);
    let off = SourceEntry {
        enabled: false,
        ..entry("")
    };
    let unreviewed = SourceEntry {
        terms_sha256: None,
        ..entry("")
    };
    let ted = SourceEntry {
        kind: SourceKind::TedSearch,
        ..entry("")
    };
    for (row, needle) in [
        (off, "disabled"),
        (unreviewed, "no reviewed terms"),
        (ted, "not sec_edgar"),
    ] {
        let report = fetch(&c, &store, &clock, &row, "2026-01-01").await;
        assert!(report.rows.is_empty());
        assert!(report.errors[0].contains(needle), "{:?}", report.errors);
    }
    // Critic U10: the runtime kill switch refuses an enabled, reviewed row.
    store
        .set_switch(&SourceSwitch {
            source_id: "sec_edgar".into(),
            enabled: false,
            at_ms: clock.now_ms(),
            reason: "operator stop".into(),
        })
        .await
        .unwrap();
    let report = fetch(&c, &store, &clock, &entry(""), "2026-01-01").await;
    assert!(report.rows.is_empty());
    assert!(
        report.errors[0]
            .contains("switched off at runtime since 2026-10-08T12:00:00Z (operator stop)"),
        "{:?}",
        report.errors
    );
    assert!(seen.lock().unwrap().is_empty(), "nothing sent");
    assert!(store.coverage(None).await.unwrap().is_empty());
    store
        .set_switch(&SourceSwitch {
            source_id: "sec_edgar".into(),
            enabled: true,
            at_ms: clock.now_ms() + 1,
            reason: "resumed".into(),
        })
        .await
        .unwrap();
    // Bad CIKs and empty spans fail their row before any request.
    let f = SecFetch {
        client: &c,
        store: &store,
        clock: &clock,
        retry: &FAST,
    };
    let report = sec_source_fetch(
        &f,
        "sec_edgar",
        &entry(""),
        &["320193".to_string(), CIK.to_string()],
        t("2026-10-09"),
        i64::MAX,
    )
    .await;
    assert!(report.rows[0].errors[0].contains("not 10 digits"));
    assert!(report.rows[1].errors[0].contains("nothing to read"));
    assert!(seen.lock().unwrap().is_empty());
    // The client takes the row's User-Agent variable, hosts and budget.
    let mut row = entry("");
    row.auth = "user_agent_env:TENGU_TEST_SEC_UA_NEVER_SET_4C1B".into();
    let e = source_sec_client(&SandboxSections::default(), "sec_edgar", &row)
        .err()
        .unwrap()
        .to_string();
    assert!(
        e.contains("TENGU_TEST_SEC_UA_NEVER_SET_4C1B is not set"),
        "{e}"
    );
    row.auth = "none".into();
    assert!(source_sec_client(&SandboxSections::default(), "sec_edgar", &row).is_err());
}
