//! SEC EDGAR → source records keyed by CIK (O2): one `sec_edgar` row of
//! `[sources]` read per CIK into `sources.db`. Reuses `SecClient`
//! (`backfill/sec.rs`: egress gate, scope, budget, User-Agent, audit
//! `sec_edgar`), `Retry` and the `domain/sec.rs` decoders; each record is
//! built by `domain::source::sec_records`. `tengu history events` and
//! `market.db` are untouched.
//!
//! | Step | Request | Snapshot `request_key` |
//! |---|---|---|
//! | per CIK | `GET data.sec.gov/submissions/CIK##########.json` (+ the older pages the span needs) | `submissions:<cik10>` · `page:<name>` |
//! | per kept filing in the span (the row's `forms`) without a stored `ok` record | `GET www.sec.gov/Archives/edgar/data/…/<accession>-index.htm` | `index:<accession>` |
//!
//! | Concern | Rule |
//! |---|---|
//! | Row | `sources::fetch_gate`: kind `sec_edgar`, enabled, with license + terms hash, not switched off at runtime — else refused before any request; [`source_sec_client`]: scope = the row's `hosts`, budget = its `rate_limit`, User-Agent = its `user_agent_env` variable |
//! | Resume | a filing with a stored `ok` record keeps its time (no index read); its record is rebuilt from the new submissions row — the same content is the same id (nothing added) — and rests on that body only, so a changed row never passes for a reparse of old bytes |
//! | Index answered without a time | an HTTP error answer (404, 410, …) or a page without a readable `Accepted`: the record is `partial` (`sec_records`: a time never before the acceptance), the row gets an error, coverage `complete = false` with the class; the next filing is read |
//! | Stop | any other failure after the retries (egress / scope denial, budget, 401 / 403 / 429, 5xx, timeout) or an undecodable submissions page: records built so far and every snapshot read are committed, coverage `complete = false` with the class, no cursor move; the next CIK runs |
//! | Batches | one commit per 25 records (their snapshots with them); the last adds the coverage row `cik:<cik10>` over `[from, min(to, start))`, fetched at the end, and — only when complete — the cursor `cik:<cik10>` = the newest accession (by published time) of the CIK's `ok` records |
//! | Clocks | a snapshot's `fetched_ms` = the clock after its reply; a record's `observed_ms` = its newest snapshot's, `parsed_ms` = the clock after (never before) |
//! | Never stored | the User-Agent: a request header only — not in a snapshot (url, request key, body), a record, a report or an audit line |

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use anyhow::{anyhow, bail, Result};

use crate::adapters::outbound::backfill::sec::{
    reply_json, user_agent_from, Reply, SecClient, SEC_DATA_URL, SEC_WWW_URL,
};
use crate::adapters::outbound::backfill::{BackfillReport, ReportRow, Retry};
use crate::adapters::outbound::egress;
use crate::adapters::outbound::http_class::{read_error, HttpError};
use crate::adapters::outbound::sources::fetch_gate;
use crate::config::sections::SandboxSections;
use crate::config::sources::{SourceAuth, SourceEntry, SourceKind};
use crate::domain::marketdata::fmt_time;
use crate::domain::observation::ErrorClass;
use crate::domain::scope::ToolScope;
use crate::domain::sec::{
    filings_page, index_acceptance, older_pages_to_read, submissions, Filing,
};
use crate::domain::source::record::sec_cik_entity;
use crate::domain::source::sec_records::{filing_record, FilingTime};
use crate::domain::source::{Coverage, Fact, ParseError, ParseStatus, SourceStamp};
use crate::ports::clock::Clock;
use crate::ports::source_store::{Batch, Cursor, RecordQuery, Snapshot, SnapshotKind, SourceStore};

const TIMEOUT: Duration = Duration::from_secs(30);
const FLUSH: usize = 25;

/// The client of a `sec_edgar` row (module table: row).
pub(crate) fn source_sec_client(
    sections: &SandboxSections,
    id: &str,
    entry: &SourceEntry,
) -> Result<SecClient> {
    if entry.kind != SourceKind::SecEdgar {
        bail!(
            "source `{id}` is a `{}` row, not sec_edgar",
            entry.kind.as_str()
        );
    }
    let var = match entry
        .auth()
        .map_err(|e| anyhow!("source `{id}` auth: {e}"))?
    {
        SourceAuth::UserAgentEnv(v) => v,
        _ => bail!("source `{id}`: sec_edgar needs auth = \"user_agent_env:<VAR>\""),
    };
    let ua = user_agent_from(&var, std::env::var(&var).ok())?;
    let scope = ToolScope {
        net_hosts: entry.hosts.clone(),
        ..Default::default()
    };
    let http = egress::policy().tool_client(TIMEOUT)?;
    Ok(
        SecClient::new(http, SEC_WWW_URL, SEC_DATA_URL, scope, None, ua)?.with_budget(
            &entry.rate_limit,
            sections.rate_limits.get(&entry.rate_limit).cloned(),
        ),
    )
}

/// What a fetch reads with and writes to.
pub(crate) struct SecFetch<'a> {
    pub client: &'a SecClient,
    pub store: &'a dyn SourceStore,
    pub clock: &'a dyn Clock,
    pub retry: &'a Retry,
}

/// Every CIK of `ciks` (10 digits each) over `[from_ms, to_ms)` (module
/// tables). Never fails: a refused row lands in `errors`, a CIK's errors in
/// its row (`instrument` = `sec:cik:<cik10>`).
pub(crate) async fn sec_source_fetch(
    f: &SecFetch<'_>,
    id: &str,
    entry: &SourceEntry,
    ciks: &[String],
    from_ms: i64,
    to_ms: i64,
) -> BackfillReport {
    let mut report = BackfillReport::default();
    let stamp = match fetch_gate(f.store, id, entry, SourceKind::SecEdgar).await {
        Ok(s) => s,
        Err(e) => {
            report.errors.push(e);
            return report;
        }
    };
    let forms = entry.forms();
    for cik in ciks {
        let row = cik_fetch(f, &stamp, &forms, entry.store_raw, cik, from_ms, to_ms).await;
        report.rows.push(row);
    }
    report
}

async fn cik_fetch(
    f: &SecFetch<'_>,
    stamp: &SourceStamp,
    forms: &[String],
    store_raw: bool,
    cik: &str,
    from_ms: i64,
    to_ms: i64,
) -> ReportRow {
    let mut row = ReportRow::new(&sec_cik_entity(cik), "filings", None, &stamp.source_id);
    if !(cik.len() == 10 && cik.bytes().all(|b| b.is_ascii_digit())) {
        row.fail(&anyhow!("CIK `{cik}` is not 10 digits"));
        return row;
    }
    let to = to_ms.min(f.clock.now_ms());
    if from_ms >= to {
        row.fail(&anyhow!(
            "nothing to read: {} is not before {}",
            fmt_time(from_ms),
            fmt_time(to)
        ));
        return row;
    }
    let mut run = match CikRun::start(f, stamp, forms, store_raw, cik, from_ms, to).await {
        Ok(r) => r,
        Err(e) => {
            row.fail(&e);
            return row;
        }
    };
    let read = run.read_all(&mut row).await;
    let committed = run.finish(&mut row, read.as_ref().err()).await;
    let t = run.tally;
    row.notes.push(format!(
        "{} filing(s) in the span; {} index page(s) read, {} already stored (time kept)",
        t.kept, t.read, t.reused
    ));
    if let Err(e) = read {
        row.fail(&e);
    }
    if let Err(e) = committed {
        row.fail(&e.context("commit"));
    }
    row
}

#[derive(Debug, Default, Clone, Copy)]
struct Tally {
    kept: usize,
    read: usize,
    reused: usize,
}

/// One snapshot read: its sha256 and fetch time.
#[derive(Debug, Clone)]
struct Read {
    sha: String,
    fetched_ms: i64,
}

/// One CIK's read (module table).
struct CikRun<'a> {
    f: &'a SecFetch<'a>,
    stamp: &'a SourceStamp,
    forms: &'a [String],
    store_raw: bool,
    cik: &'a str,
    from: i64,
    to: i64,
    /// Accession → published_ms of a stored `ok` record (resume).
    stored: HashMap<String, i64>,
    /// Record ids already stored (a commit adds the rest).
    stored_ids: HashSet<String>,
    /// Newest `(published_ms, accession)` among the CIK's `ok` records.
    newest: Option<(i64, String)>,
    seen: HashSet<String>,
    batch: Batch,
    /// The class of the first filing whose index gave no time.
    unread: Option<ErrorClass>,
    tally: Tally,
}

impl<'a> CikRun<'a> {
    async fn start(
        f: &'a SecFetch<'a>,
        stamp: &'a SourceStamp,
        forms: &'a [String],
        store_raw: bool,
        cik: &'a str,
        from: i64,
        to: i64,
    ) -> Result<CikRun<'a>> {
        let mut run = CikRun {
            f,
            stamp,
            forms,
            store_raw,
            cik,
            from,
            to,
            stored: HashMap::new(),
            stored_ids: HashSet::new(),
            newest: None,
            seen: HashSet::new(),
            batch: Batch::default(),
            unread: None,
            tally: Tally::default(),
        };
        let query = RecordQuery {
            source_id: Some(stamp.source_id.clone()),
            entity: Some(sec_cik_entity(cik)),
            event_key: None,
            upto_ms: i64::MAX,
        };
        for r in f.store.records(&query).await? {
            run.stored_ids.insert(r.record_id.clone());
            if r.source_id == stamp.source_id
                && r.parse == ParseStatus::Ok
                && matches!(&r.fact, Fact::SecFiling(s) if s.cik == cik)
            {
                run.note_ok(r.published_ms, &r.native_id);
            }
        }
        Ok(run)
    }

    fn note_ok(&mut self, published_ms: i64, accession: &str) {
        self.stored.insert(accession.to_string(), published_ms);
        let key = (published_ms, accession.to_string());
        if self.newest.as_ref().map_or(true, |n| key > *n) {
            self.newest = Some(key);
        }
    }

    /// Keep `reply` as a snapshot of the batch.
    fn keep(&mut self, reply: &Reply, request_key: String) -> Read {
        let fetched_ms = self.f.clock.now_ms();
        let s = Snapshot::read(
            SnapshotKind::Response,
            &self.stamp.source_id,
            &request_key,
            &reply.url,
            reply.status,
            &reply.content_type,
            fetched_ms,
            reply.body.as_bytes(),
            self.store_raw,
        );
        let read = Read {
            sha: s.sha256.clone(),
            fetched_ms,
        };
        if !self.batch.snapshots.iter().any(|b| b.sha256 == s.sha256) {
            self.batch.snapshots.push(s);
        }
        read
    }

    /// The submissions file, then the older pages the span needs.
    async fn read_all(&mut self, row: &mut ReportRow) -> Result<()> {
        let (client, cik) = (self.f.client, self.cik);
        row.reads += 1;
        let reply = self
            .f
            .retry
            .run("submissions", || client.submissions_reply(cik))
            .await?;
        let page = self.keep(&reply, format!("submissions:{cik}"));
        let subs = reply_json(&reply)
            .and_then(|v| submissions(&v))
            .map_err(|e| decode(format!("submissions CIK{cik}: {e}")))?;
        if subs.cik != cik {
            return Err(decode(format!(
                "submissions for CIK{cik} name CIK{}",
                subs.cik
            )));
        }
        self.page(row, &subs.recent, &page).await?;
        for p in older_pages_to_read(&subs, self.from, self.to) {
            row.reads += 1;
            let reply = self
                .f
                .retry
                .run("submissions page", || client.older_page_reply(&p.name))
                .await?;
            let read = self.keep(&reply, format!("page:{}", p.name));
            let filings = reply_json(&reply)
                .and_then(|v| filings_page(&v))
                .map_err(|e| decode(format!("{}: {e}", p.name)))?;
            self.page(row, &filings, &read).await?;
        }
        Ok(())
    }

    /// The kept filings of one submissions page (`page` = its snapshot).
    async fn page(&mut self, row: &mut ReportRow, filings: &[Filing], page: &Read) -> Result<()> {
        for fl in filings {
            if !(self.forms.contains(&fl.form) && fl.may_fall_in(self.from, self.to)) {
                continue;
            }
            if !self.seen.insert(fl.accession.clone()) {
                continue;
            }
            let (time, snapshots, observed) = match self.stored.get(&fl.accession) {
                Some(&t) => {
                    self.tally.reused += 1;
                    (
                        FilingTime::Accepted(t),
                        vec![page.sha.clone()],
                        page.fetched_ms,
                    )
                }
                None => self.read_index(row, fl, page).await?,
            };
            if let FilingTime::Accepted(t) = time {
                if !(self.from..self.to).contains(&t) {
                    continue;
                }
            }
            let parsed = self.f.clock.now_ms().max(observed);
            let r = filing_record(self.stamp, self.cik, fl, time, snapshots, observed, parsed)
                .map_err(|e| anyhow!("{e}"))?;
            if let Err(p) = r.validate() {
                bail!("record {} is not valid: {}", r.record_id, p.join("; "));
            }
            self.tally.kept += 1;
            if r.parse == ParseStatus::Ok {
                self.note_ok(r.published_ms, &r.native_id);
            }
            self.batch.records.push(r);
            if self.batch.records.len() >= FLUSH {
                let batch = std::mem::take(&mut self.batch);
                self.commit(row, batch).await?;
            }
        }
        Ok(())
    }

    /// One index page → the filing's time, its snapshots, its read time
    /// (module table: index answered without a time, stop).
    async fn read_index(
        &mut self,
        row: &mut ReportRow,
        fl: &Filing,
        page: &Read,
    ) -> Result<(FilingTime, Vec<String>, i64)> {
        let (client, cik, acc) = (self.f.client, self.cik, fl.accession.as_str());
        row.reads += 1;
        match self
            .f
            .retry
            .run("filing index", || client.index_reply(cik, acc))
            .await
        {
            Ok(reply) => {
                let index = self.keep(&reply, format!("index:{acc}"));
                self.tally.read += 1;
                let snapshots = vec![page.sha.clone(), index.sha.clone()];
                match index_acceptance(&reply.body, acc) {
                    Ok(a) => {
                        if a.disagrees() {
                            row.notes.push(format!(
                                "index {acc}: Accepted {} New York = {}, its Last-Modified says {}; Accepted kept",
                                a.accepted_ny,
                                fmt_time(a.published_ms),
                                a.last_modified_ms.map(fmt_time).unwrap_or_default()
                            ));
                        }
                        Ok((
                            FilingTime::Accepted(a.published_ms),
                            snapshots,
                            index.fetched_ms,
                        ))
                    }
                    Err(m) => {
                        self.unread.get_or_insert(ErrorClass::Decode);
                        row.fail(&decode(format!("index {acc}: {m}")));
                        Ok((
                            FilingTime::Unread(ParseError::new("Accepted", m)),
                            snapshots,
                            index.fetched_ms,
                        ))
                    }
                }
            }
            Err(e) => {
                let class = read_error("index", &e).class;
                let status = e
                    .chain()
                    .find_map(|c| c.downcast_ref::<HttpError>())
                    .and_then(|h| h.http_status);
                let answered = matches!(class, ErrorClass::Fatal | ErrorClass::NotApplicable)
                    && status.is_some();
                if !answered {
                    return Err(e.context(format!("index {acc}")));
                }
                self.unread.get_or_insert(class);
                let why = format!(
                    "index page not read ({}, HTTP {})",
                    class.as_str(),
                    status.unwrap_or_default()
                );
                row.fail(&e.context(format!("index {acc}")));
                Ok((
                    FilingTime::Unread(ParseError::new("index", why)),
                    vec![page.sha.clone()],
                    page.fetched_ms,
                ))
            }
        }
    }

    async fn commit(&mut self, row: &mut ReportRow, batch: Batch) -> Result<()> {
        let fresh: Vec<i64> = batch
            .records
            .iter()
            .filter(|r| !self.stored_ids.contains(&r.record_id))
            .map(|r| r.published_ms)
            .collect();
        let ids: Vec<String> = batch.records.iter().map(|r| r.record_id.clone()).collect();
        let report = self.f.store.commit(batch).await?;
        self.stored_ids.extend(ids);
        row.wrote(report.records_new, fresh);
        Ok(())
    }

    /// The last batch: what is pending, the coverage row and (complete
    /// only) the cursor (module table: batches).
    async fn finish(&mut self, row: &mut ReportRow, failed: Option<&anyhow::Error>) -> Result<()> {
        let fetched_ms = self.f.clock.now_ms();
        let failure = failed.map(|e| read_error("sec", e).class).or(self.unread);
        let mut batch = std::mem::take(&mut self.batch);
        let query_key = format!("cik:{}", self.cik);
        batch.coverage.push(Coverage {
            source_id: self.stamp.source_id.clone(),
            query_key: query_key.clone(),
            fetched_ms,
            from_ms: self.from,
            to_ms: self.to,
            complete: failure.is_none(),
            error_class: failure.map(|c| c.as_str().to_string()),
        });
        if failure.is_none() {
            batch.cursor = self.newest.as_ref().map(|(_, acc)| Cursor {
                source_id: self.stamp.source_id.clone(),
                query_key,
                value: acc.clone(),
                updated_ms: fetched_ms,
            });
        }
        self.commit(row, batch).await
    }
}

fn decode(message: String) -> anyhow::Error {
    HttpError::new(ErrorClass::Decode, message).into()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use reqwest::header::HeaderValue;
    use serde_json::Value;

    use super::*;
    use crate::adapters::outbound::backfill::test_support::FAST;
    use crate::adapters::outbound::http_class::test_support::{
        canned, local_scope, serve, test_client, Canned,
    };
    use crate::adapters::outbound::rate_limit::Limiters;
    use crate::adapters::outbound::sources::store::SqliteSourceStore;
    use crate::domain::marketdata::parse_time;
    use crate::domain::source::{
        as_of, AsOfInput, AsOfMode, Revision, SourcePolicy, SourceRecord, SupersededBy,
    };
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
        let subs_sha =
            crate::ports::source_store::body_sha256(fixture("CIK0000320193.json").as_bytes());
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
            &"4 filing(s) in the span; 0 index page(s) read, 4 already stored (time kept)"
                .to_string()
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
        let sha = crate::ports::source_store::body_sha256(b"<html>maintenance</html>");
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
                stored
                    .push(String::from_utf8_lossy(snap.body.as_deref().unwrap_or_default()).into());
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
}
