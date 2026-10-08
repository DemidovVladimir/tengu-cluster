//! EU TED Search → source records (O2): one `ted_search` row of `[sources]`
//! read into `sources.db`, one publication day at a time. The request body,
//! the field allow-list, the decoder and the record of each notice are
//! `domain::source::ted`.
//!
//! | Step | Request | Snapshot `request_key` |
//! |---|---|---|
//! | per publication day (Europe/Paris) not yet read whole for the query | `POST api.ted.europa.eu/v3/notices/search`, anonymous: the row's query with `{from}` = `{to}` = the day, page 1, 2, … until every match is read | `search:<query sha256>:<YYYYMMDD>:<page>:<limit>` |
//!
//! | Concern | Rule |
//! |---|---|
//! | Row | `sources::fetch_gate`: kind `ted_search`, enabled, reviewed terms, not switched off at runtime — else refused before any request; [`source_ted_client`]: scope = the row's `hosts`, budget = its `rate_limit`, `auth = "none"` (a keyed row is refused) |
//! | Gate | `egress::policy().check_url` + the scope's `net_hosts` per request — a denial is `fatal`, nothing sent; the budget before sending; a 429 drains it (`Limiters::penalize`) |
//! | Errors | `http_class` (a 400 such as `SEARCH_WINDOW_TOO_WIDE` = `fatal`, 429 `rate_limited`, 5xx `transient`); a reply with `timedOut: true` = `transient`; retried per `Retry` |
//! | Days | the publication days (Europe/Paris) from `from` that ended by `min(to, now)` — today's edition is read once it is over; no `from` = the day after the cursor; a day with a complete coverage row of the query is skipped (resume) |
//! | Pages | `limit` [`TED_MAX_LIMIT`] (`TedFetch::limit`); the server reads no further than `page × limit` = [`TED_MAX_WINDOW`]: a bigger day is read to there, its coverage incomplete (`not_applicable`) |
//! | A page | its reply a snapshot (the body when `store_raw`); each notice one record (`notice_record`; `prior` = the stored and pending records of its event, so a change notice names what it corrects); a notice without a publication number = a row error, the day incomplete (`decode`); committed per page |
//! | Day end | the last batch adds the coverage `query:<sha256>` over the day `[start, end]` and — only when every match was read — the cursor `query:<sha256>` = the newest whole day (`YYYY-MM-DD`, never moved back) |
//! | Stop | a request failure after the retries or a page that does not decode: what was read is committed, the day's coverage `complete = false` with the class; later days are not read |
//! | Clocks | a page's `fetched_ms` = the clock after its reply = its records' `observed_ms`; `parsed_ms` = the clock after (never before) |
//! | Audit | one egress line per request: `tool = "ted_search"`, `host`, `path`, `status`, `ms` — the query sits in the snapshot, never in the line |
//! | Import | [`ted_import`]: a saved search reply as if read at `observed_ms` — the same gate and records, one snapshot (`import:<file name>`), no coverage (nothing says which query it answered or that it was whole) |

use std::collections::BTreeSet;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use chrono::NaiveDate;
use reqwest::header::{HeaderValue, ACCEPT, CONTENT_TYPE};
use reqwest::Url;
use serde_json::{json, Value};

use crate::adapters::outbound::backfill::sec::{reply_json, Reply};
use crate::adapters::outbound::backfill::{BackfillReport, ReportRow, Retry};
use crate::adapters::outbound::egress;
use crate::adapters::outbound::http_class::{
    http_status_error, read_error, reqwest_error, retry_after_ms, HttpError, Scrubber,
};
use crate::adapters::outbound::rate_limit::{limiters, Limiters, Priority};
use crate::adapters::outbound::sources::fetch_gate;
use crate::config::rate_limits::RateLimitConfig;
use crate::config::sections::SandboxSections;
use crate::config::sources::{SourceAuth, SourceEntry, SourceKind};
use crate::domain::marketdata::fmt_time;
use crate::domain::observation::ErrorClass;
use crate::domain::scope::ToolScope;
use crate::domain::source::ted::{
    day_of, day_span, decode_page, notice_record, query_key, search_body, TedPage, TedRead,
    TED_MAX_LIMIT, TED_MAX_WINDOW, TED_SEARCH_PATH,
};
use crate::domain::source::{Coverage, Fact, ParseStatus, SourceRecord, SourceStamp};
use crate::ports::clock::Clock;
use crate::ports::source_store::{Batch, Cursor, RecordQuery, Snapshot, SnapshotKind, SourceStore};

/// The TED API.
pub(crate) const TED_API_URL: &str = "https://api.ted.europa.eu";
const TIMEOUT: Duration = Duration::from_secs(60);
const MAX_BUDGET_WAIT: Duration = Duration::from_secs(120);

/// The search endpoint with its scope and budget (module table: row, gate).
pub(crate) struct TedClient {
    http: reqwest::Client,
    base: Url,
    scope: ToolScope,
    budget_name: String,
    budget: Option<RateLimitConfig>,
    timeout: Duration,
    max_budget_wait: Duration,
    limiters: &'static Limiters,
    /// Tests: every audit event, as sent to the egress log.
    #[cfg(test)]
    audit_tap: Option<std::sync::Arc<std::sync::Mutex<Vec<Value>>>>,
}

/// The client of a `ted_search` row (module table: row).
pub(crate) fn source_ted_client(
    sections: &SandboxSections,
    id: &str,
    entry: &SourceEntry,
) -> Result<TedClient> {
    if entry.kind != SourceKind::TedSearch {
        bail!(
            "source `{id}` is a `{}` row, not ted_search",
            entry.kind.as_str()
        );
    }
    match entry
        .auth()
        .map_err(|e| anyhow!("source `{id}` auth: {e}"))?
    {
        SourceAuth::None => {}
        _ => bail!("source `{id}`: the TED Search API is anonymous — auth = \"none\""),
    }
    let scope = ToolScope {
        net_hosts: entry.hosts.clone(),
        ..Default::default()
    };
    let http = egress::policy().tool_client(TIMEOUT)?;
    TedClient::new(
        http,
        TED_API_URL,
        scope,
        &entry.rate_limit,
        sections.rate_limits.get(&entry.rate_limit).cloned(),
    )
}

impl TedClient {
    pub(crate) fn new(
        http: reqwest::Client,
        base: &str,
        scope: ToolScope,
        budget_name: &str,
        budget: Option<RateLimitConfig>,
    ) -> Result<Self> {
        let base = Url::parse(base.trim().trim_end_matches('/'))
            .ok()
            .filter(|u| u.host_str().is_some_and(|h| !h.is_empty()))
            .ok_or_else(|| anyhow!("TED base `{base}` is not a URL with a host"))?;
        Ok(Self {
            http,
            base,
            scope,
            budget_name: budget_name.to_string(),
            budget,
            timeout: TIMEOUT,
            max_budget_wait: MAX_BUDGET_WAIT,
            limiters: limiters(),
            #[cfg(test)]
            audit_tap: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn with_limiters(mut self, limiters: &'static Limiters) -> Self {
        self.limiters = limiters;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_audit_tap(
        mut self,
        tap: std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
    ) -> Self {
        self.audit_tap = Some(tap);
        self
    }

    /// The search URL (what a snapshot names).
    pub(crate) fn search_url(&self) -> Url {
        let mut url = self.base.clone();
        url.set_path(TED_SEARCH_PATH);
        url
    }

    /// One search POST (module table: gate, errors): the 2xx [`Reply`];
    /// errors carry an [`HttpError`].
    pub(crate) async fn search_reply(&self, body: &Value) -> Result<Reply> {
        let url = self.search_url();
        let host = url.host_str().unwrap_or_default().to_string();
        let scrub = Scrubber::for_api(&url);
        let gate = egress::policy()
            .check_url(&url)
            .and_then(|_| self.scope.check_net_host(&host));
        if let Err(e) = gate {
            let err = HttpError::new(ErrorClass::Fatal, format!("{e:#}"));
            self.audit(&url, &host, Err(&err), None);
            return Err(err.into());
        }
        let budget = self.budget.as_ref();
        if let Err(err) = self
            .limiters
            .acquire(
                &self.budget_name,
                budget,
                1,
                Priority::Read,
                self.max_budget_wait,
            )
            .await
        {
            self.audit(&url, &host, Err(&err), None);
            return Err(err.into());
        }
        let started = Instant::now();
        let sent = self
            .http
            .post(url.clone())
            .timeout(self.timeout)
            .header(ACCEPT, HeaderValue::from_static("application/json"))
            .json(body)
            .send()
            .await;
        let result = match sent {
            Err(e) => Err(reqwest_error(e, &host, &scrub)),
            Ok(resp) => {
                let status = resp.status().as_u16();
                let retry_after = retry_after_ms(resp.headers());
                let content_type = resp
                    .headers()
                    .get(CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("application/json")
                    .to_string();
                match resp.text().await {
                    Err(e) => Err(reqwest_error(e, &host, &scrub)),
                    Ok(body) if (200..300).contains(&status) => Ok(Reply {
                        url: url.to_string(),
                        status,
                        content_type,
                        body,
                    }),
                    Ok(body) => Err(http_status_error(status, retry_after, &body, &host, &scrub)),
                }
            }
        };
        let ms = Some(started.elapsed().as_millis() as u64);
        match result {
            Ok(reply) => {
                self.audit(&url, &host, Ok(reply.status), ms);
                Ok(reply)
            }
            Err(err) => {
                if err.class == ErrorClass::RateLimited {
                    self.limiters
                        .penalize(&self.budget_name, budget, err.retry_after_ms);
                }
                self.audit(&url, &host, Err(&err), ms);
                Err(err.into())
            }
        }
    }

    /// [`Self::search_reply`], a `timedOut` reply an error (`transient`).
    async fn search_page(&self, body: &Value) -> Result<Reply> {
        let reply = self.search_reply(body).await?;
        let timed_out = serde_json::from_str::<Value>(reply.body.trim())
            .ok()
            .and_then(|v| v.get("timedOut").and_then(Value::as_bool))
            == Some(true);
        if timed_out {
            return Err(HttpError::new(
                ErrorClass::Transient,
                "the TED search timed out: its notices may be incomplete",
            )
            .into());
        }
        Ok(reply)
    }

    fn audit(
        &self,
        url: &Url,
        host: &str,
        outcome: std::result::Result<u16, &HttpError>,
        ms: Option<u64>,
    ) {
        let mut event = json!({
            "tool": "ted_search",
            "host": host,
            "path": url.path(),
            "ms": ms,
        });
        match outcome {
            Ok(status) => {
                event["verdict"] = "allowed".into();
                event["status"] = status.into();
            }
            Err(e) => {
                event["verdict"] = if ms.is_some() { "error" } else { "denied" }.into();
                event["class"] = e.class.as_str().into();
                event["reason"] = e.message.clone().into();
                if let Some(s) = e.http_status {
                    event["status"] = s.into();
                }
            }
        }
        #[cfg(test)]
        if let Some(tap) = &self.audit_tap {
            tap.lock().unwrap().push(event.clone());
        }
        egress::policy().audit(event);
    }
}

/// What a fetch reads with and writes to.
pub(crate) struct TedFetch<'a> {
    pub client: &'a TedClient,
    pub store: &'a dyn SourceStore,
    pub clock: &'a dyn Clock,
    pub retry: &'a Retry,
    /// Notices per page (module table: pages).
    pub limit: u32,
}

/// The days of `[from_ms, to_ms)` (module table: days); `from_ms = None`
/// starts the day after the cursor. Never fails: a refused row lands in
/// `errors`, a day's errors in its row (`instrument` = `day:<YYYY-MM-DD>`).
pub(crate) async fn ted_source_fetch(
    f: &TedFetch<'_>,
    id: &str,
    entry: &SourceEntry,
    from_ms: Option<i64>,
    to_ms: i64,
) -> BackfillReport {
    let mut report = BackfillReport::default();
    let stamp = match fetch_gate(f.store, id, entry, SourceKind::TedSearch).await {
        Ok(s) => s,
        Err(e) => {
            report.errors.push(e);
            return report;
        }
    };
    let Some(template) = entry.query.as_deref() else {
        report.errors.push(format!(
            "source `{id}` has no query (a ted_search row needs one)"
        ));
        return report;
    };
    let key = query_key(template);
    report.notes.push(format!("query {key} = {template}"));
    let days = match plan_days(f, id, &key, from_ms, to_ms).await {
        Ok((days, skipped)) => {
            if skipped > 0 {
                report.notes.push(format!(
                    "{skipped} day(s) already read whole for this query: skipped"
                ));
            }
            days
        }
        Err(e) => {
            report.errors.push(format!("{e:#}"));
            return report;
        }
    };
    if days.is_empty() {
        report
            .notes
            .push("no publication day left to read in the window".into());
    }
    let limit = f.limit.clamp(1, TED_MAX_LIMIT);
    for (i, day) in days.iter().enumerate() {
        let mut run = DayRun::new(f, &stamp, entry.store_raw, &key, *day, limit);
        let (row, stopped) = run.fetch(template).await;
        report.rows.push(row);
        if stopped && i + 1 < days.len() {
            report.notes.push(format!(
                "stopped at {}: {} later day(s) not read (a re-run resumes)",
                day.format("%Y-%m-%d"),
                days.len() - i - 1
            ));
            break;
        }
    }
    report
}

/// The days to read and how many were skipped (module table: days).
async fn plan_days(
    f: &TedFetch<'_>,
    id: &str,
    key: &str,
    from_ms: Option<i64>,
    to_ms: i64,
) -> Result<(Vec<NaiveDate>, usize)> {
    let to = to_ms.min(f.clock.now_ms());
    let first = match from_ms {
        Some(ms) => {
            if ms >= to {
                bail!(
                    "nothing to read: {} is not before {}",
                    fmt_time(ms),
                    fmt_time(to)
                );
            }
            day_of(ms)
        }
        None => {
            let cursor = f.store.cursor(id, key).await?.ok_or_else(|| {
                anyhow!("no cursor for {key} yet: give --from (the first day to read)")
            })?;
            NaiveDate::parse_from_str(&cursor.value, "%Y-%m-%d")
                .ok()
                .and_then(|d| d.succ_opt())
                .ok_or_else(|| anyhow!("cursor {key} = `{}` is not a day", cursor.value))?
        }
    };
    let whole: BTreeSet<(i64, i64)> = f
        .store
        .coverage(Some(id))
        .await?
        .into_iter()
        .filter(|c| c.query_key == key && c.complete)
        .map(|c| (c.from_ms, c.to_ms))
        .collect();
    let (mut days, mut skipped) = (Vec::new(), 0);
    let mut day = first;
    while day_span(day).1 <= to {
        if whole.contains(&day_span(day)) {
            skipped += 1;
        } else {
            days.push(day);
        }
        day = match day.succ_opt() {
            Some(d) => d,
            None => break,
        };
    }
    Ok((days, skipped))
}

#[derive(Debug, Default, Clone, Copy)]
struct Tally {
    notices: u64,
    partial: usize,
    unparsed: usize,
    /// Change notices linked to the record they correct.
    linked: usize,
    /// Change notices naming nothing stored (the backfill began after it).
    unlinked: usize,
}

/// Why a day's read is not whole without a failed request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Short {
    /// A notice without a publication number (no record can hold it).
    Unkeyed,
    /// The server's page window ends before the day's matches.
    Window,
    /// Fewer notices than `totalNoticeCount`.
    Fewer,
}

/// After a page: what comes next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PageStep {
    /// Read the next page.
    Next,
    /// Every match read.
    Done,
    /// A short page before every match was read: the pages ended early.
    Fewer,
    /// The next page would pass the server's window.
    Window,
}

/// `page` (1-based) of `limit` held `got` notices; `read` so far of `total`.
fn page_step(page: u32, limit: u32, got: usize, read: u64, total: u64) -> PageStep {
    let asked = u64::from(page) * u64::from(limit);
    if read >= total {
        PageStep::Done
    } else if asked >= total || (got as u64) < u64::from(limit) {
        PageStep::Fewer
    } else if asked + u64::from(limit) > TED_MAX_WINDOW {
        PageStep::Window
    } else {
        PageStep::Next
    }
}

/// One publication day's read (module table).
struct DayRun<'a> {
    f: &'a TedFetch<'a>,
    stamp: &'a SourceStamp,
    store_raw: bool,
    key: &'a str,
    day: NaiveDate,
    limit: u32,
    batch: Batch,
    short: Option<Short>,
    tally: Tally,
}

impl<'a> DayRun<'a> {
    fn new(
        f: &'a TedFetch<'a>,
        stamp: &'a SourceStamp,
        store_raw: bool,
        key: &'a str,
        day: NaiveDate,
        limit: u32,
    ) -> Self {
        Self {
            f,
            stamp,
            store_raw,
            key,
            day,
            limit,
            batch: Batch::default(),
            short: None,
            tally: Tally::default(),
        }
    }

    /// The day's row, and whether the fetch must stop (module table: stop).
    async fn fetch(&mut self, template: &str) -> (ReportRow, bool) {
        let label = self.day.format("%Y-%m-%d").to_string();
        let mut row = ReportRow::new(
            &format!("day:{label}"),
            "notices",
            None,
            &self.stamp.source_id,
        );
        let read = self.read_pages(&mut row, template).await;
        let committed = self.finish(&mut row, read.as_ref().err()).await;
        let t = self.tally;
        row.notes.push(format!(
            "{} notice(s): {} partial, {} unparsed; change notices: {} linked to what they correct, {} naming nothing stored (no link)",
            t.notices, t.partial, t.unparsed, t.linked, t.unlinked
        ));
        let stopped = read.is_err() || committed.is_err();
        if let Err(e) = read {
            row.fail(&e);
        }
        if let Err(e) = committed {
            row.fail(&e.context("commit"));
        }
        (row, stopped)
    }

    async fn read_pages(&mut self, row: &mut ReportRow, template: &str) -> Result<()> {
        let (client, limit) = (self.f.client, self.limit);
        let compact = self.day.format("%Y%m%d").to_string();
        let sha = self.key.trim_start_matches("query:").to_string();
        let mut page = 1u32;
        loop {
            row.reads += 1;
            let body = search_body(template, self.day, self.day, page, limit);
            let reply = self
                .f
                .retry
                .run("ted search", || client.search_page(&body))
                .await?;
            let fetched_ms = self.f.clock.now_ms();
            let request_key = format!("search:{sha}:{compact}:{page}:{limit}");
            let snapshot = self.keep(&reply, &request_key, fetched_ms);
            let decoded = reply_json(&reply)
                .and_then(|v| decode_page(&v))
                .map_err(|e| decode(format!("page {page}: {e}")))?;
            let (total, got) = (decoded.total, decoded.notices.len());
            self.page(row, decoded, &snapshot, fetched_ms, page).await?;
            let read = self.tally.notices;
            match page_step(page, limit, got, read, total) {
                PageStep::Done => return Ok(()),
                PageStep::Fewer => {
                    self.short.get_or_insert(Short::Fewer);
                    row.fail(&decode(format!(
                        "read {read} of {total} notice(s): page {page} held {got} of {limit}"
                    )));
                    return Ok(());
                }
                PageStep::Window => {
                    self.short.get_or_insert(Short::Window);
                    row.fail(&anyhow::Error::from(HttpError::new(
                        ErrorClass::NotApplicable,
                        format!(
                            "{total} notice(s) match; the server pages no further than \
                             {TED_MAX_WINDOW}: read {read} — narrow the query"
                        ),
                    )));
                    return Ok(());
                }
                PageStep::Next => {
                    let batch = std::mem::take(&mut self.batch);
                    self.commit(row, batch).await?;
                    page += 1;
                }
            }
        }
    }

    /// Keep `reply` as a snapshot of the batch; its sha256.
    fn keep(&mut self, reply: &Reply, request_key: &str, fetched_ms: i64) -> String {
        let s = Snapshot::read(
            SnapshotKind::Response,
            &self.stamp.source_id,
            request_key,
            &reply.url,
            reply.status,
            &reply.content_type,
            fetched_ms,
            reply.body.as_bytes(),
            self.store_raw,
        );
        let sha = s.sha256.clone();
        if !self.batch.snapshots.iter().any(|b| b.sha256 == sha) {
            self.batch.snapshots.push(s);
        }
        sha
    }

    /// One page's notices → records (module table: a page).
    async fn page(
        &mut self,
        row: &mut ReportRow,
        page: TedPage,
        snapshot: &str,
        fetched_ms: i64,
        n: u32,
    ) -> Result<()> {
        for item in page.notices {
            self.tally.notices += 1;
            let read = match item {
                Ok(r) => r,
                Err(e) => {
                    self.short.get_or_insert(Short::Unkeyed);
                    row.fail(&decode(format!("page {n}: {e}")));
                    continue;
                }
            };
            let prior = prior_of(self.f.store, &self.batch, &self.stamp.source_id, &read).await?;
            let parsed = self.f.clock.now_ms().max(fetched_ms);
            let r = notice_record(
                self.stamp,
                &read,
                &prior,
                snapshot.to_string(),
                fetched_ms,
                parsed,
            );
            if let Err(p) = r.validate() {
                bail!("record {} is not valid: {}", r.record_id, p.join("; "));
            }
            self.count(&read, &r);
            self.batch.records.push(r);
        }
        Ok(())
    }

    fn count(&mut self, read: &TedRead, r: &SourceRecord) {
        match r.parse {
            ParseStatus::Ok => {}
            ParseStatus::Partial => self.tally.partial += 1,
            ParseStatus::Error => self.tally.unparsed += 1,
        }
        match (&read.changes, &r.supersedes) {
            (Some(_), Some(_)) => self.tally.linked += 1,
            (Some(_), None) => self.tally.unlinked += 1,
            (None, _) => {}
        }
    }

    async fn commit(&mut self, row: &mut ReportRow, batch: Batch) -> Result<()> {
        let times: Vec<i64> = batch.records.iter().map(|r| r.published_ms).collect();
        let report = self.f.store.commit(batch).await?;
        row.wrote(
            report.records_new,
            if report.records_new > 0 {
                times
            } else {
                Vec::new()
            },
        );
        Ok(())
    }

    /// The last batch: what is pending, the day's coverage and (whole
    /// only) the cursor (module table: day end).
    async fn finish(&mut self, row: &mut ReportRow, failed: Option<&anyhow::Error>) -> Result<()> {
        let fetched_ms = self.f.clock.now_ms();
        let failure = failed
            .map(|e| read_error("ted", e).class)
            .or(match self.short {
                None => None,
                Some(Short::Window) => Some(ErrorClass::NotApplicable),
                Some(Short::Unkeyed | Short::Fewer) => Some(ErrorClass::Decode),
            });
        let (from_ms, to_ms) = day_span(self.day);
        let source_id = self.stamp.source_id.clone();
        let mut batch = std::mem::take(&mut self.batch);
        batch.coverage.push(Coverage {
            source_id: source_id.clone(),
            query_key: self.key.to_string(),
            fetched_ms,
            from_ms,
            to_ms,
            complete: failure.is_none(),
            error_class: failure.map(|c| c.as_str().to_string()),
        });
        if failure.is_none() {
            let value = self.day.format("%Y-%m-%d").to_string();
            let newer = match self.f.store.cursor(&source_id, self.key).await? {
                Some(c) => value > c.value,
                None => true,
            };
            if newer {
                batch.cursor = Some(Cursor {
                    source_id,
                    query_key: self.key.to_string(),
                    value,
                    updated_ms: fetched_ms,
                });
            }
        }
        self.commit(row, batch).await
    }
}

/// The stored and pending records of `read`'s event (for a change notice
/// only — nothing else needs them).
async fn prior_of(
    store: &dyn SourceStore,
    pending: &Batch,
    source_id: &str,
    read: &TedRead,
) -> Result<Vec<SourceRecord>> {
    if read.changes.is_none() {
        return Ok(Vec::new());
    }
    let event = read.event_key();
    let mut prior = store
        .records(&RecordQuery {
            source_id: Some(source_id.to_string()),
            entity: None,
            event_key: Some(event.clone()),
            upto_ms: i64::MAX,
        })
        .await?;
    prior.extend(
        pending
            .records
            .iter()
            .filter(|r| r.event_key == event)
            .cloned(),
    );
    Ok(prior)
}

/// A saved search reply read as if fetched at `observed_ms` (module table:
/// import). Never fails: problems land in `errors` or the file's row.
pub(crate) async fn ted_import(
    store: &dyn SourceStore,
    clock: &dyn Clock,
    id: &str,
    entry: &SourceEntry,
    path: &Path,
    observed_ms: i64,
) -> BackfillReport {
    let mut report = BackfillReport::default();
    let stamp = match fetch_gate(store, id, entry, SourceKind::TedSearch).await {
        Ok(s) => s,
        Err(e) => {
            report.errors.push(e);
            return report;
        }
    };
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut row = ReportRow::new(&format!("file:{name}"), "notices", None, id);
    row.reads = 1;
    let now = clock.now_ms();
    if observed_ms > now {
        row.fail(&anyhow!(
            "--observed-at {} is after now ({})",
            fmt_time(observed_ms),
            fmt_time(now)
        ));
        report.rows.push(row);
        return report;
    }
    let body = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            row.fail(&anyhow!("{}: {e}", path.display()));
            report.rows.push(row);
            return report;
        }
    };
    let request_key = if !name.is_empty() && !name.chars().any(char::is_whitespace) {
        format!("import:{name}")
    } else {
        "import".to_string()
    };
    let search_url = format!("{TED_API_URL}{TED_SEARCH_PATH}");
    let snapshot = Snapshot::read(
        SnapshotKind::Response,
        id,
        &request_key,
        &search_url,
        200,
        "application/json",
        observed_ms,
        &body,
        entry.store_raw,
    );
    let sha = snapshot.sha256.clone();
    let mut batch = Batch {
        snapshots: vec![snapshot],
        ..Default::default()
    };
    let decoded = serde_json::from_slice::<Value>(&body)
        .map_err(|e| format!("not JSON ({e})"))
        .and_then(|v| decode_page(&v));
    match decoded {
        Err(e) => row.fail(&decode(format!("{}: {e}", path.display()))),
        Ok(page) => {
            let (mut partial, mut unparsed) = (0, 0);
            for item in page.notices {
                let read = match item {
                    Ok(r) => r,
                    Err(e) => {
                        row.fail(&decode(e));
                        continue;
                    }
                };
                let prior = match prior_of(store, &batch, id, &read).await {
                    Ok(p) => p,
                    Err(e) => {
                        row.fail(&e);
                        break;
                    }
                };
                let parsed = clock.now_ms().max(observed_ms);
                let r = notice_record(&stamp, &read, &prior, sha.clone(), observed_ms, parsed);
                if let Err(p) = r.validate() {
                    row.fail(&anyhow!(
                        "record {} is not valid: {}",
                        r.record_id,
                        p.join("; ")
                    ));
                    continue;
                }
                match r.fact {
                    Fact::Unparsed { .. } => unparsed += 1,
                    _ if r.parse == ParseStatus::Partial => partial += 1,
                    _ => {}
                }
                batch.records.push(r);
            }
            row.notes.push(format!(
                "{} record(s) built: {partial} partial, {unparsed} unparsed; no coverage row (an import names no query and no whole window)",
                batch.records.len()
            ));
        }
    }
    let times: Vec<i64> = batch.records.iter().map(|r| r.published_ms).collect();
    match store.commit(batch).await {
        Ok(c) => row.wrote(
            c.records_new,
            if c.records_new > 0 { times } else { Vec::new() },
        ),
        Err(e) => row.fail(&e.context("commit")),
    }
    report.rows.push(row);
    report
}

fn decode(message: String) -> anyhow::Error {
    HttpError::new(ErrorClass::Decode, message).into()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::adapters::outbound::backfill::test_support::FAST;
    use crate::adapters::outbound::http_class::test_support::{
        canned, local_scope, serve, test_client, Canned,
    };
    use crate::adapters::outbound::sources::store::SqliteSourceStore;
    use crate::domain::marketdata::parse_time;
    use crate::domain::source::ted::query_for;
    use crate::ports::clock::SimClock;
    use crate::ports::source_store::SourceSwitch;

    const HASH: &str = "4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b";
    const QUERY: &str = "classification-cpv IN (72000000 48000000) AND publication-date >= {from} AND publication-date <= {to}";

    fn fixture(name: &str) -> Value {
        let path = format!("{}/tests/fixtures/ted/{name}", env!("CARGO_MANIFEST_DIR"));
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
        serde_json::from_str(&text).unwrap()
    }

    fn t(s: &str) -> i64 {
        parse_time(s).unwrap()
    }

    /// A `ted_search` row: enabled with synthetic terms.
    fn entry() -> SourceEntry {
        let text = format!(
            r#"
            kind = "ted_search"
            class = "law_regulator"
            trust = "primary"
            revision = "immutable"
            enabled = true
            hosts = ["api.ted.europa.eu"]
            auth = "none"
            rate_limit = "ted"
            store_raw = true
            jurisdiction = "EU"
            language = "en"
            query = "{QUERY}"
            license = "synthetic test terms"
            terms_url = "https://example.org/terms"
            terms_sha256 = "{HASH}"
            terms_reviewed_at = "2026-10-08"
            raw_retention_days = 90
            record_retention_days = 0
            "#
        );
        toml::from_str(&text).unwrap_or_else(|e| panic!("{e}\n{text}"))
    }

    fn client(base: &str, scope: ToolScope, budget: Option<RateLimitConfig>) -> TedClient {
        TedClient::new(test_client(), base, scope, "ted", budget)
            .unwrap()
            .with_limiters(Box::leak(Box::new(Limiters::default())))
    }

    /// The first `n` notices of page 1 as one page of a day of `total`.
    fn page(from: usize, n: usize, total: u64) -> Canned {
        let all = fixture("search_page_1.json")["notices"].clone();
        let notices: Vec<Value> = all.as_array().unwrap()[from..from + n].to_vec();
        canned(
            200,
            json!({"notices": notices, "totalNoticeCount": total, "iterationNextToken": null, "timedOut": false})
                .to_string(),
        )
    }

    fn bodies(seen: &Arc<Mutex<Vec<String>>>) -> Vec<Value> {
        seen.lock()
            .unwrap()
            .iter()
            .map(|req| {
                let (head, body) = req.split_once("\r\n\r\n").unwrap();
                assert!(head.starts_with("POST /v3/notices/search "), "{head}");
                serde_json::from_str(body).unwrap()
            })
            .collect()
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

    fn fetch<'a>(
        c: &'a TedClient,
        store: &'a SqliteSourceStore,
        clock: &'a SimClock,
    ) -> TedFetch<'a> {
        TedFetch {
            client: c,
            store,
            clock,
            retry: &FAST,
            limit: 2,
        }
    }

    /// The row's host is the scope: a client aimed anywhere else is denied
    /// before sending — one `denied` audit line, nothing stored but the
    /// day's incomplete coverage.
    #[tokio::test]
    async fn denied_host_sends_nothing() {
        let (base, seen) = serve(vec![page(0, 2, 2)]).await;
        let scope = ToolScope {
            net_hosts: vec!["api.ted.europa.eu".into()],
            ..Default::default()
        };
        let tap = Arc::new(Mutex::new(Vec::new()));
        let c = client(&base, scope, None).with_audit_tap(tap.clone());
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSourceStore::open(dir.path()).unwrap();
        let clock = SimClock::at(t("2026-10-08T12:00:00Z"));
        let report = ted_source_fetch(
            &fetch(&c, &store, &clock),
            "ted_search",
            &entry(),
            Some(t("2026-10-01")),
            t("2026-10-03"),
        )
        .await;
        assert!(seen.lock().unwrap().is_empty(), "nothing sent");
        assert_eq!(report.rows.len(), 1, "{}", report.render());
        assert_eq!(report.rows[0].classes, vec![ErrorClass::Fatal]);
        assert!(
            report
                .notes
                .iter()
                .any(|n| n.contains("1 later day(s) not read")),
            "{:?}",
            report.notes
        );
        let events = tap.lock().unwrap().clone();
        assert_eq!(events.len(), 1);
        assert_eq!(
            (events[0]["tool"].as_str(), events[0]["verdict"].as_str()),
            (Some("ted_search"), Some("denied"))
        );
        assert!(all(&store).await.is_empty());
        let cov = store.coverage(None).await.unwrap();
        assert_eq!(
            (cov.len(), cov[0].complete, cov[0].error_class.as_deref()),
            (1, false, Some("fatal"))
        );
        // A keyed row has no anonymous client.
        let mut keyed = entry();
        keyed.auth = "api_key_env:TED_KEY".into();
        let e = source_ted_client(&SandboxSections::default(), "ted_search", &keyed)
            .err()
            .unwrap()
            .to_string();
        assert!(e.contains("anonymous"), "{e}");
    }

    /// A 429 is retried and drains the row's budget; the retried page is
    /// read whole.
    #[tokio::test]
    async fn rate_limited_reply_is_retried_and_penalized() {
        let (base, seen) = serve(vec![
            canned(429, r#"{"message":"slow down"}"#),
            page(0, 2, 2),
        ])
        .await;
        let budget = RateLimitConfig {
            per_minute: 600,
            burst: Some(5),
            exec_reserve: 0,
        };
        let limiters: &'static Limiters = Box::leak(Box::new(Limiters::default()));
        let tap = Arc::new(Mutex::new(Vec::new()));
        let c = TedClient::new(test_client(), &base, local_scope(), "ted", Some(budget))
            .unwrap()
            .with_limiters(limiters)
            .with_audit_tap(tap.clone());
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSourceStore::open(dir.path()).unwrap();
        let clock = SimClock::at(t("2026-10-08T12:00:00Z"));
        let report = ted_source_fetch(
            &fetch(&c, &store, &clock),
            "ted_search",
            &entry(),
            Some(t("2026-10-01")),
            t("2026-10-02"),
        )
        .await;
        assert_eq!(report.error_count(), 0, "{}", report.render());
        assert_eq!(seen.lock().unwrap().len(), 2, "the 429 was retried");
        let events = tap.lock().unwrap().clone();
        assert_eq!(events[0]["status"], 429);
        assert_eq!(events[0]["class"], "rate_limited");
        assert_eq!(events[1]["status"], 200);
        // Drained by the 429: without it 5 − 2 = 3 tokens would be left.
        let left = limiters.tokens("ted").unwrap();
        assert!(left < 2.0, "the budget was not drained: {left}");
        assert_eq!(all(&store).await.len(), 2);
    }

    /// Two days of two pages each: the second page of day 2 fails after the
    /// retries. Day 1 is whole (coverage, cursor); day 2 keeps its first
    /// page, its coverage is incomplete, the cursor stays on day 1. A
    /// re-run without `from` resumes after the cursor and reads day 2 again
    /// (the stored page adds nothing); a third run has nothing to read.
    #[tokio::test]
    async fn crash_between_pages_resumes_from_the_cursor() {
        let mut replies = vec![page(0, 2, 4), page(2, 2, 4), page(0, 2, 4)];
        for _ in 0..4 {
            replies.push(canned(503, "busy"));
        }
        let (base, seen) = serve(replies).await;
        let c = client(&base, local_scope(), None);
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSourceStore::open(dir.path()).unwrap();
        let clock = SimClock::at(t("2026-10-08T12:00:00Z"));
        let row = entry();
        let key = query_key(QUERY);
        let report = ted_source_fetch(
            &fetch(&c, &store, &clock),
            "ted_search",
            &row,
            Some(t("2026-10-01")),
            t("2026-10-03"),
        )
        .await;
        assert_eq!(report.rows.len(), 2, "{}", report.render());
        assert!(report.rows[0].errors.is_empty(), "{:?}", report.rows[0]);
        assert_eq!(report.rows[1].classes, vec![ErrorClass::Transient]);
        let sent = bodies(&seen);
        assert_eq!(sent.len(), 7, "4 pages + 3 retries");
        assert_eq!(
            sent[0]["query"],
            query_for(
                QUERY,
                NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(),
                NaiveDate::from_ymd_opt(2026, 10, 1).unwrap()
            )
        );
        assert_eq!(
            (sent[0]["page"].as_u64(), sent[1]["page"].as_u64()),
            (Some(1), Some(2))
        );
        assert_eq!(
            (sent[0]["limit"].as_u64(), sent[0]["scope"].as_str()),
            (Some(2), Some("ALL"))
        );
        assert!(sent[2]["query"]
            .as_str()
            .unwrap()
            .ends_with(">= 20261002 AND publication-date <= 20261002"));
        // Four records from day 1, nothing new from day 2's first page (the
        // same notices: the same records).
        assert_eq!(all(&store).await.len(), 4);
        let cov = store.coverage(Some("ted_search")).await.unwrap();
        let day1 = day_span(NaiveDate::from_ymd_opt(2026, 10, 1).unwrap());
        let day2 = day_span(NaiveDate::from_ymd_opt(2026, 10, 2).unwrap());
        assert_eq!(cov.len(), 2);
        assert_eq!(
            (cov[0].from_ms, cov[0].to_ms, cov[0].complete),
            (day1.0, day1.1, true)
        );
        assert_eq!(
            (
                cov[1].from_ms,
                cov[1].complete,
                cov[1].error_class.as_deref()
            ),
            (day2.0, false, Some("transient"))
        );
        assert_eq!(cov[0].query_key, key);
        assert_eq!(
            store
                .cursor("ted_search", &key)
                .await
                .unwrap()
                .unwrap()
                .value,
            "2026-10-01"
        );
        // Every page is a snapshot with its request.
        let records = all(&store).await;
        let snap = store
            .snapshot(&records[0].snapshots[0])
            .await
            .unwrap()
            .unwrap();
        assert!(
            snap.request_key
                .starts_with(&format!("search:{}:20261001:", &key["query:".len()..])),
            "{}",
            snap.request_key
        );
        assert!(snap.url.ends_with("/v3/notices/search"));
        assert!(snap.body.is_some());

        // Resume an hour later: no `from`, day 2 again, both pages.
        clock.advance(3_600_000);
        let (base, seen) = serve(vec![page(0, 2, 4), page(2, 2, 4)]).await;
        let c = client(&base, local_scope(), None);
        let report = ted_source_fetch(
            &fetch(&c, &store, &clock),
            "ted_search",
            &row,
            None,
            t("2026-10-03"),
        )
        .await;
        assert_eq!(report.error_count(), 0, "{}", report.render());
        let sent = bodies(&seen);
        assert_eq!(sent.len(), 2);
        assert!(sent[0]["query"].as_str().unwrap().contains("20261002"));
        assert_eq!(
            store
                .cursor("ted_search", &key)
                .await
                .unwrap()
                .unwrap()
                .value,
            "2026-10-02"
        );
        assert_eq!(
            all(&store).await.len(),
            4,
            "the same notices are the same records"
        );
        // Nothing left; an explicit earlier `from` skips the whole days.
        let (base, seen) = serve(vec![]).await;
        let c = client(&base, local_scope(), None);
        let report = ted_source_fetch(
            &fetch(&c, &store, &clock),
            "ted_search",
            &row,
            Some(t("2026-10-01")),
            t("2026-10-03"),
        )
        .await;
        assert_eq!(report.error_count(), 0, "{}", report.render());
        assert!(
            report
                .notes
                .iter()
                .any(|n| n.contains("2 day(s) already read whole")),
            "{:?}",
            report.notes
        );
        assert!(seen.lock().unwrap().is_empty());
        // A day not over yet is not read.
        let report = ted_source_fetch(
            &fetch(&c, &store, &clock),
            "ted_search",
            &row,
            Some(t("2026-10-08")),
            i64::MAX,
        )
        .await;
        assert!(
            report.rows.is_empty() && report.error_count() == 0,
            "{}",
            report.render()
        );
    }

    /// A change notice read after its original links to it; a notice
    /// without a publication number leaves the day incomplete; a page past
    /// the server window is cut and said.
    #[tokio::test]
    async fn change_notices_link_and_short_days_stay_incomplete() {
        let change = fixture("search_change_notice.json");
        let (base, _) = serve(vec![
            canned(200, change.to_string()),
            canned(200, fixture("search_bad_notice.json").to_string()),
        ])
        .await;
        let c = client(&base, local_scope(), None);
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSourceStore::open(dir.path()).unwrap();
        let clock = SimClock::at(t("2026-10-08T12:00:00Z"));
        let f = TedFetch {
            limit: 250,
            ..fetch(&c, &store, &clock)
        };
        let report = ted_source_fetch(
            &f,
            "ted_search",
            &entry(),
            Some(t("2026-10-01")),
            t("2026-10-03"),
        )
        .await;
        let records = all(&store).await;
        let original = records
            .iter()
            .find(|r| r.native_id == "657981-2026")
            .unwrap();
        let changed = records
            .iter()
            .find(|r| r.native_id == "674231-2026")
            .unwrap();
        assert_eq!(
            changed.supersedes.as_deref(),
            Some(original.record_id.as_str())
        );
        assert!(
            report.rows[0].notes[0]
                .contains("change notices: 1 linked to what they correct, 0 naming nothing stored"),
            "{:?}",
            report.rows[0].notes
        );
        // Day 2: the bad page — three records, one notice unkeyed. 674295-2026 names a
        // `<notice uuid>-<version>` never stored: counted unlinked.
        let day2 = &report.rows[1];
        assert_eq!(day2.classes, vec![ErrorClass::Decode], "{day2:?}");
        assert!(
            day2.errors[0].contains("no publication-number"),
            "{:?}",
            day2.errors
        );
        assert!(
            day2.notes
                .iter()
                .any(|n| n == "4 notice(s): 1 partial, 1 unparsed; change notices: 0 linked to what they correct, 1 naming nothing stored (no link)"),
            "{:?}",
            day2.notes
        );
        assert_eq!(records.len(), 5);
        let cov = store.coverage(None).await.unwrap();
        assert_eq!(
            (
                cov[0].complete,
                cov[1].complete,
                cov[1].error_class.as_deref()
            ),
            (true, false, Some("decode"))
        );
        assert_eq!(
            store
                .cursor("ted_search", &query_key(QUERY))
                .await
                .unwrap()
                .unwrap()
                .value,
            "2026-10-01"
        );

        // A short page before the total: the day stops there, incomplete;
        // the limit is capped at the server's.
        let (base, seen) = serve((0..3).map(|_| page(0, 2, 15_001)).collect()).await;
        let c = client(&base, local_scope(), None);
        let f = TedFetch {
            limit: 5_000,
            ..fetch(&c, &store, &clock)
        };
        let report = ted_source_fetch(
            &f,
            "ted_search",
            &entry(),
            Some(t("2026-10-05")),
            t("2026-10-06"),
        )
        .await;
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert_eq!(
            bodies(&seen)[0]["limit"].as_u64(),
            Some(u64::from(TED_MAX_LIMIT))
        );
        assert!(
            report.rows[0].errors[0].contains("read 2 of 15001 notice(s): page 1 held 2 of 250"),
            "{:?}",
            report.rows[0].errors
        );
    }

    #[test]
    fn pages_stop_at_the_total_a_short_page_or_the_server_window() {
        use PageStep::*;
        for (page, limit, got, read, total, want) in [
            (1, 250, 0, 0, 0, Done),
            (1, 250, 4, 4, 4, Done),
            (1, 2, 2, 2, 4, Next),
            (2, 2, 2, 4, 4, Done),
            (2, 2, 1, 3, 4, Fewer),
            (2, 2, 2, 4, 5, Next),
            (3, 2, 0, 4, 5, Fewer),
            (59, 250, 250, 14_750, 20_000, Next),
            (60, 250, 250, 15_000, 20_000, Window),
            (60, 250, 250, 15_000, 15_000, Done),
        ] {
            assert_eq!(
                page_step(page, limit, got, read, total),
                want,
                "{page} {limit} {got} {read} {total}"
            );
        }
    }

    /// A refused row sends nothing; the runtime switch refuses too.
    #[tokio::test]
    async fn a_refused_row_is_never_fetched() {
        let (base, seen) = serve(vec![page(0, 2, 2)]).await;
        let c = client(&base, local_scope(), None);
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSourceStore::open(dir.path()).unwrap();
        let clock = SimClock::at(t("2026-10-08T12:00:00Z"));
        let off = SourceEntry {
            enabled: false,
            ..entry()
        };
        let unreviewed = SourceEntry {
            terms_sha256: None,
            ..entry()
        };
        let sec = SourceEntry {
            kind: SourceKind::SecEdgar,
            ..entry()
        };
        for (row, needle) in [
            (off, "disabled"),
            (unreviewed, "no reviewed terms"),
            (sec, "not ted_search"),
        ] {
            let report = ted_source_fetch(
                &fetch(&c, &store, &clock),
                "ted_search",
                &row,
                Some(t("2026-10-01")),
                i64::MAX,
            )
            .await;
            assert!(report.rows.is_empty());
            assert!(report.errors[0].contains(needle), "{:?}", report.errors);
        }
        store
            .set_switch(&SourceSwitch {
                source_id: "ted_search".into(),
                enabled: false,
                at_ms: clock.now_ms(),
                reason: "terms under review".into(),
            })
            .await
            .unwrap();
        let report = ted_source_fetch(
            &fetch(&c, &store, &clock),
            "ted_search",
            &entry(),
            Some(t("2026-10-01")),
            i64::MAX,
        )
        .await;
        assert!(
            report.errors[0].contains("switched off at runtime"),
            "{:?}",
            report.errors
        );
        let path = format!(
            "{}/tests/fixtures/ted/search_page_1.json",
            env!("CARGO_MANIFEST_DIR")
        );
        let report = ted_import(
            &store,
            &clock,
            "ted_search",
            &entry(),
            Path::new(&path),
            t("2026-10-02"),
        )
        .await;
        assert!(
            report.errors[0].contains("switched off at runtime"),
            "{:?}",
            report.errors
        );
        assert!(seen.lock().unwrap().is_empty(), "nothing sent");
        assert!(all(&store).await.is_empty() && store.coverage(None).await.unwrap().is_empty());
        // No cursor yet and no `from`: refused, named.
        store
            .set_switch(&SourceSwitch {
                source_id: "ted_search".into(),
                enabled: true,
                at_ms: clock.now_ms() + 1,
                reason: "reviewed".into(),
            })
            .await
            .unwrap();
        let report = ted_source_fetch(
            &fetch(&c, &store, &clock),
            "ted_search",
            &entry(),
            None,
            i64::MAX,
        )
        .await;
        assert!(
            report.errors[0].contains("give --from"),
            "{:?}",
            report.errors
        );
    }

    /// An import is the fetch's records from a saved page, read at the
    /// given time, with no coverage.
    #[tokio::test]
    async fn import_reads_a_saved_page_without_coverage() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSourceStore::open(dir.path()).unwrap();
        let clock = SimClock::at(t("2026-10-08T12:00:00Z"));
        let path = format!(
            "{}/tests/fixtures/ted/search_page_1.json",
            env!("CARGO_MANIFEST_DIR")
        );
        let at = t("2026-10-02T06:00:00Z");
        let report = ted_import(&store, &clock, "ted_search", &entry(), Path::new(&path), at).await;
        assert_eq!(report.error_count(), 0, "{}", report.render());
        assert_eq!(report.rows[0].instrument, "file:search_page_1.json");
        let records = all(&store).await;
        assert_eq!(records.len(), 4);
        for r in &records {
            assert_eq!(r.observed_ms, at);
            let snap = store.snapshot(&r.snapshots[0]).await.unwrap().unwrap();
            assert_eq!(
                (snap.request_key.as_str(), snap.fetched_ms),
                ("import:search_page_1.json", at)
            );
            assert_eq!(snap.url, "https://api.ted.europa.eu/v3/notices/search");
        }
        assert!(store.coverage(None).await.unwrap().is_empty());
        // Again: nothing new.
        let again = ted_import(&store, &clock, "ted_search", &entry(), Path::new(&path), at).await;
        assert_eq!(again.rows[0].rows, 0);
        // Read in the future: refused.
        let late = ted_import(
            &store,
            &clock,
            "ted_search",
            &entry(),
            Path::new(&path),
            t("2026-10-09"),
        )
        .await;
        assert!(
            late.rows[0].errors[0].contains("after now"),
            "{:?}",
            late.rows[0].errors
        );
    }
}
