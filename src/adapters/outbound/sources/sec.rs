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
#[path = "sec_tests.rs"]
mod tests;
