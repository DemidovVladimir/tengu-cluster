//! SEC EDGAR filings → `market.db` `events` (`source = sec`) + one
//! `event_coverage` row per instrument (Phase 7, `tengu history events`).
//! Decoders and the time rule (the index page's `Accepted`, New York →
//! UTC; the submissions `acceptanceDateTime` is not the time): `domain/sec.rs`.
//!
//! | Step | Request | Host |
//! |---|---|---|
//! | once per run | `GET /files/company_tickers.json` → ticker → CIK | `www.sec.gov` |
//! | per instrument with a CIK | `GET /submissions/CIK##########.json` (+ the older pages `filings.files[]` a span before the recent filings needs) | `data.sec.gov` |
//! | per kept filing in the span, not stored yet | `GET /Archives/edgar/data/<cik>/<accession, no dashes>/<accession>-index.htm` → `published_ms` | `www.sec.gov` |
//!
//! | Concern | Rule |
//! |---|---|
//! | Instruments | `hyperliquid:xyz:<TICKER>` only (`domain::sec::sec_ticker`; the CLI refuses others before any request); the ticker is looked up as written — an xyz name with another company's US ticker would map to that company |
//! | No CIK | `event_coverage` `covered = false`, note `no SEC CIK for ticker <T>` — the source's silence is no evidence of no news; status ok |
//! | A CIK | filings with `from ≤ published_ms < to` (`to` capped at now) upserted in batches of 25; then coverage `covered = true`, note `cik <10 digits> <TICKER>`, span `[from, to)` — joined with a stored covered span of the same CIK it overlaps or touches |
//! | Resume | a filing already stored (same accession) keeps its time: no index read |
//! | User-Agent | `$SEC_USER_AGENT` on every request — required (SEC fair access: a name + an email); [`operator_sec`] refuses without it; never logged |
//! | Gate | `egress::policy().check_url` (`[egress] allow_hosts`: both hosts) + the client's scope `net_hosts`, per request; a denial is `Fatal`, nothing sent |
//! | Budget | `[rate_limits.sec]` (`outbound/rate_limit.rs`; weight 1 per request; xlab-w2: 300 / min, burst 5 — SEC allows 10 / s); a 429 drains it for `Retry-After`; no section = unlimited |
//! | Errors | `http_class` mapping (403 `auth_required`: a missing / blocked User-Agent; 429 `rate_limited`; 5xx `transient`; a body that does not decode `decode`); retried per `Retry`; a failed request stops that instrument — events already written stay, no coverage row is written |
//! | Audit | one egress line per request: `tool = "sec_edgar"`, `host`, `path` (CIK and accession in full), `status`, `ms` |
//! | Notes | per instrument: the CIK, filings in the span, index pages read / already stored, how `acceptanceDateTime` related to the index time (`domain::sec::json_clock`), an index whose `Last-Modified` disagrees |
//! | Raw replies (O2) | `submissions_reply` / `older_page_reply` / `index_reply`: the same requests, the 2xx [`Reply`] undecoded (URL, status, content type, body) — the source store keeps the bytes (`outbound/sources/sec.rs`); `with_budget` names the `[sources]` row's budget, `user_agent_from` its User-Agent variable |

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use reqwest::header::{HeaderValue, ACCEPT, CONTENT_TYPE, USER_AGENT};
use reqwest::Url;
use serde_json::{json, Value};

use super::{ReportRow, Retry};
use crate::adapters::outbound::egress;
use crate::adapters::outbound::http_class::{
    http_status_error, reqwest_error, retry_after_ms, HttpError, Scrubber,
};
use crate::adapters::outbound::rate_limit::{limiters, Limiters, Priority};
use crate::config::rate_limits::RateLimitConfig;
use crate::config::sections::SandboxSections;
use crate::domain::marketdata::{fmt_time, EventCoverage, MarketEvent};
use crate::domain::observation::ErrorClass;
use crate::domain::scope::ToolScope;
use crate::domain::sec::{
    filing_event, filings_page, index_acceptance, index_path, is_accession, is_page_name,
    json_clock, older_pages_to_read, sec_ticker, submissions, ticker_ciks, Filing, IndexAcceptance,
    JsonClock, Submissions,
};
use crate::ports::market_data::MarketDataStore;

/// `company_tickers.json` and the filing archive.
pub(crate) const SEC_WWW_URL: &str = "https://www.sec.gov";
/// The submissions API.
pub(crate) const SEC_DATA_URL: &str = "https://data.sec.gov";
/// The env var holding the User-Agent (required).
pub(crate) const USER_AGENT_ENV: &str = "SEC_USER_AGENT";
/// The `[rate_limits.<name>]` SEC requests budget against.
pub(crate) const RATE_LIMIT: &str = "sec";
/// `events.source` / `event_coverage.source`.
pub(crate) const SOURCE: &str = "sec";
const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_BUDGET_WAIT: Duration = Duration::from_secs(120);
const FLUSH: usize = 25;
const DAY_MS: i64 = 86_400_000;

/// `$SEC_USER_AGENT` → the header (module table); unset or blank refused,
/// naming the variable — the value itself is never echoed.
pub(crate) fn user_agent(env: Option<String>) -> Result<HeaderValue> {
    user_agent_from(USER_AGENT_ENV, env)
}

/// [`user_agent`] read from the variable `var` (a `[sources]` row's
/// `auth = "user_agent_env:<VAR>"`).
pub(crate) fn user_agent_from(var: &str, env: Option<String>) -> Result<HeaderValue> {
    let value = env
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "{var} is not set: SEC EDGAR asks every automated client for a \
                 User-Agent naming it and an email (\"Sample Company admin@example.com\", \
                 https://www.sec.gov/os/accessing-edgar-data) — set it in the environment or \
                 the repo .env"
            )
        })?;
    HeaderValue::from_str(&value).map_err(|_| anyhow!("{var} is not a valid HTTP header value"))
}

/// One 2xx reply as read (the source store keeps it as a raw snapshot).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Reply {
    /// The URL read, in full.
    pub url: String,
    pub status: u16,
    /// The reply's `Content-Type`, else what was asked for.
    pub content_type: String,
    pub body: String,
}

/// The EDGAR endpoints with their scope, budget and User-Agent.
pub(crate) struct SecClient {
    http: reqwest::Client,
    www: Url,
    data: Url,
    scope: ToolScope,
    /// The `[rate_limits.<name>]` it budgets against ([`RATE_LIMIT`] unless
    /// a `[sources]` row names another).
    budget_name: String,
    budget: Option<RateLimitConfig>,
    user_agent: HeaderValue,
    timeout: Duration,
    max_budget_wait: Duration,
    limiters: &'static Limiters,
    /// Tests: every audit event, as sent to the egress log.
    #[cfg(test)]
    audit_tap: Option<std::sync::Arc<std::sync::Mutex<Vec<Value>>>>,
}

/// An operator command's client (`tengu history events`): the public hosts,
/// a scope that pins both (the `[egress]` policy stays the ceiling), the
/// sandbox's `[rate_limits.sec]`, `$SEC_USER_AGENT` (required).
pub(crate) fn operator_sec(sections: &SandboxSections) -> Result<SecClient> {
    let ua = user_agent(std::env::var(USER_AGENT_ENV).ok())?;
    let scope = ToolScope {
        net_hosts: vec!["www.sec.gov".into(), "data.sec.gov".into()],
        ..Default::default()
    };
    let http = egress::policy().tool_client(TIMEOUT)?;
    SecClient::new(
        http,
        SEC_WWW_URL,
        SEC_DATA_URL,
        scope,
        sections.rate_limits.get(RATE_LIMIT).cloned(),
        ua,
    )
}

impl SecClient {
    /// `www` = archive / ticker-map base, `data` = submissions base.
    pub(crate) fn new(
        http: reqwest::Client,
        www: &str,
        data: &str,
        scope: ToolScope,
        budget: Option<RateLimitConfig>,
        user_agent: HeaderValue,
    ) -> Result<Self> {
        let base = |s: &str| {
            Url::parse(s.trim().trim_end_matches('/'))
                .ok()
                .filter(|u| u.host_str().is_some_and(|h| !h.is_empty()))
                .ok_or_else(|| anyhow!("SEC base `{s}` is not a URL with a host"))
        };
        Ok(Self {
            http,
            www: base(www)?,
            data: base(data)?,
            scope,
            budget_name: RATE_LIMIT.to_string(),
            budget,
            user_agent,
            timeout: TIMEOUT,
            max_budget_wait: MAX_BUDGET_WAIT,
            limiters: limiters(),
            #[cfg(test)]
            audit_tap: None,
        })
    }

    /// Budget against `[rate_limits.<name>]` = `budget` instead.
    pub(crate) fn with_budget(mut self, name: &str, budget: Option<RateLimitConfig>) -> Self {
        self.budget_name = name.to_string();
        self.budget = budget;
        self
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

    /// One GET (module table): the 2xx body; errors carry an [`HttpError`].
    async fn get(&self, base: &Url, path: &str, accept: &'static str) -> Result<String> {
        Ok(self.get_reply(base, path, accept).await?.body)
    }

    /// One GET (module table): the 2xx [`Reply`]; errors carry an [`HttpError`].
    async fn get_reply(&self, base: &Url, path: &str, accept: &'static str) -> Result<Reply> {
        let mut url = base.clone();
        url.set_path(path);
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
            .get(url.clone())
            .timeout(self.timeout)
            .header(USER_AGENT, self.user_agent.clone())
            .header(ACCEPT, accept)
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
                    .unwrap_or(accept)
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

    fn audit(
        &self,
        url: &Url,
        host: &str,
        outcome: std::result::Result<u16, &HttpError>,
        ms: Option<u64>,
    ) {
        let mut event = json!({
            "tool": "sec_edgar",
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

    async fn get_json(&self, base: &Url, path: &str) -> Result<Value> {
        let body = self.get(base, path, "application/json").await?;
        serde_json::from_str(body.trim())
            .map_err(|e| decode_err(base, format!("{path}: the reply is not JSON ({e})")).into())
    }

    /// Ticker → CIK (10 digits).
    pub(crate) async fn ticker_map(&self) -> Result<BTreeMap<String, String>> {
        let path = "/files/company_tickers.json";
        let v = self.get_json(&self.www, path).await?;
        ticker_ciks(&v).map_err(|e| decode_err(&self.www, e).into())
    }

    /// `CIK##########.json`.
    pub(crate) async fn submissions(&self, cik10: &str) -> Result<Submissions> {
        let path = format!("/submissions/CIK{cik10}.json");
        let v = self.get_json(&self.data, &path).await?;
        submissions(&v).map_err(|e| decode_err(&self.data, e).into())
    }

    /// `CIK##########.json` as read, undecoded (the source store keeps the
    /// bytes even when they do not decode).
    pub(crate) async fn submissions_reply(&self, cik10: &str) -> Result<Reply> {
        if !(cik10.len() == 10 && cik10.bytes().all(|b| b.is_ascii_digit())) {
            bail!("CIK `{cik10}` is not 10 digits");
        }
        let path = format!("/submissions/CIK{cik10}.json");
        self.get_reply(&self.data, &path, "application/json").await
    }

    /// An older page (`filings.files[].name`).
    pub(crate) async fn older_page(&self, name: &str) -> Result<Vec<Filing>> {
        if !is_page_name(name) {
            bail!("`{name}` is not a submissions page name");
        }
        let v = self
            .get_json(&self.data, &format!("/submissions/{name}"))
            .await?;
        filings_page(&v).map_err(|e| decode_err(&self.data, format!("{name}: {e}")).into())
    }

    /// An older page as read, undecoded.
    pub(crate) async fn older_page_reply(&self, name: &str) -> Result<Reply> {
        if !is_page_name(name) {
            bail!("`{name}` is not a submissions page name");
        }
        self.get_reply(
            &self.data,
            &format!("/submissions/{name}"),
            "application/json",
        )
        .await
    }

    /// A filing's index page → its acceptance.
    pub(crate) async fn index(&self, cik10: &str, accession: &str) -> Result<IndexAcceptance> {
        let path = index_path(cik10, accession).map_err(|e| anyhow!(e))?;
        let html = self.get(&self.www, &path, "text/html").await?;
        index_acceptance(&html, accession).map_err(|e| decode_err(&self.www, e).into())
    }

    /// A filing's index page as read, undecoded.
    pub(crate) async fn index_reply(&self, cik10: &str, accession: &str) -> Result<Reply> {
        let path = index_path(cik10, accession).map_err(|e| anyhow!(e))?;
        self.get_reply(&self.www, &path, "text/html").await
    }
}

/// A reply's JSON (`decode` when it is not).
pub(crate) fn reply_json(reply: &Reply) -> std::result::Result<Value, String> {
    serde_json::from_str(reply.body.trim()).map_err(|e| format!("the reply is not JSON ({e})"))
}

fn decode_err(base: &Url, m: String) -> HttpError {
    HttpError::new(
        ErrorClass::Decode,
        format!("{}: {m}", base.host_str().unwrap_or_default()),
    )
}

/// The ticker map, retried per `retry`.
pub(crate) async fn sec_ticker_map(
    client: &SecClient,
    retry: &Retry,
) -> Result<BTreeMap<String, String>> {
    retry.run("company_tickers", || client.ticker_map()).await
}

/// What to read for one instrument.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SecPlan {
    /// `hyperliquid:xyz:<TICKER>`.
    pub instrument: String,
    pub from_ms: i64,
    /// Exclusive; capped at the fetch time.
    pub to_ms: i64,
}

/// Filings of `plan` → `events` + `event_coverage` (module table). Never
/// fails: errors land in the row.
pub(crate) async fn sec_filings(
    client: &SecClient,
    store: &dyn MarketDataStore,
    ciks: &BTreeMap<String, String>,
    plan: &SecPlan,
    retry: &Retry,
    now_ms: i64,
) -> ReportRow {
    let mut row = ReportRow::new(&plan.instrument, "filings", None, SOURCE);
    if let Err(e) = fetch(&mut row, client, store, ciks, plan, retry, now_ms).await {
        row.fail(&e);
    }
    row
}

async fn fetch(
    row: &mut ReportRow,
    client: &SecClient,
    store: &dyn MarketDataStore,
    ciks: &BTreeMap<String, String>,
    plan: &SecPlan,
    retry: &Retry,
    now_ms: i64,
) -> Result<()> {
    let ticker = sec_ticker(&plan.instrument).map_err(|e| anyhow!(e))?;
    let (from, to) = (plan.from_ms, plan.to_ms.min(now_ms));
    if from >= to {
        bail!(
            "nothing to read: {} is not before {}",
            fmt_time(from),
            fmt_time(to)
        );
    }
    let coverage = |covered: bool, note: String| EventCoverage {
        instrument: plan.instrument.clone(),
        source: SOURCE.to_string(),
        from_ms: from,
        to_ms: to,
        covered,
        note: Some(note),
        fetched_at_ms: now_ms,
    };
    let Some(cik) = ciks.get(&ticker) else {
        let note = format!("no SEC CIK for ticker {ticker}");
        store
            .put_event_coverage(&coverage(false, note.clone()))
            .await?;
        row.notes
            .push(format!("{note}: coverage recorded as not covered"));
        return Ok(());
    };
    let mapped = format!("cik {cik} {ticker}");
    row.notes.push(mapped.clone());
    let stored: HashMap<String, i64> = store
        .events(
            &plan.instrument,
            from.saturating_sub(DAY_MS),
            to.saturating_add(DAY_MS),
        )
        .await?
        .into_iter()
        .filter(|e| e.kind == "filing" && is_accession(&e.id))
        .map(|e| (e.id, e.published_ms))
        .collect();
    let mut run = Run {
        client,
        store,
        retry,
        instrument: &plan.instrument,
        cik,
        from,
        to,
        stored,
        seen: HashSet::new(),
        pending: Vec::new(),
        tally: Tally::default(),
    };
    let read = run.read_all(row).await;
    let flushed = run.flush(row).await;
    let tally = run.tally;
    row.notes.push(format!(
        "{} filing(s) in the span; {} index page(s) read, {} already stored (time kept)",
        tally.kept, tally.read, tally.reused
    ));
    if tally.read > 0 {
        row.notes.push(format!(
            "acceptanceDateTime vs the index time: {} true UTC, {} + the New York offset, {} other",
            tally.utc, tally.ny_offset, tally.other
        ));
    }
    read?;
    flushed?;
    let mut cov = coverage(true, mapped);
    let joined = store
        .event_coverage(&plan.instrument)
        .await?
        .into_iter()
        .find(|c| {
            c.source == SOURCE
                && c.covered
                && c.note == cov.note
                && c.from_ms <= cov.to_ms
                && cov.from_ms <= c.to_ms
        });
    if let Some(old) = joined {
        cov.from_ms = cov.from_ms.min(old.from_ms);
        cov.to_ms = cov.to_ms.max(old.to_ms);
        if (cov.from_ms, cov.to_ms) != (from, to) {
            row.notes.push(format!(
                "coverage joined with the stored span: {} … {}",
                fmt_time(cov.from_ms),
                fmt_time(cov.to_ms)
            ));
        }
    }
    store.put_event_coverage(&cov).await?;
    Ok(())
}

#[derive(Debug, Default, Clone, Copy)]
struct Tally {
    kept: usize,
    read: usize,
    reused: usize,
    utc: usize,
    ny_offset: usize,
    other: usize,
}

/// One instrument's read (module table).
struct Run<'a> {
    client: &'a SecClient,
    store: &'a dyn MarketDataStore,
    retry: &'a Retry,
    instrument: &'a str,
    cik: &'a str,
    from: i64,
    to: i64,
    /// Accession → stored `published_ms` (resume).
    stored: HashMap<String, i64>,
    seen: HashSet<String>,
    pending: Vec<MarketEvent>,
    tally: Tally,
}

impl Run<'_> {
    /// The submissions file, then the older pages the span needs.
    async fn read_all(&mut self, row: &mut ReportRow) -> Result<()> {
        let (client, cik) = (self.client, self.cik);
        row.reads += 1;
        let subs = self
            .retry
            .run("submissions", || client.submissions(cik))
            .await?;
        if subs.cik != cik {
            bail!("submissions for CIK{cik} name CIK{}", subs.cik);
        }
        self.page(row, &subs.recent).await?;
        for p in older_pages_to_read(&subs, self.from, self.to) {
            row.reads += 1;
            let filings = self
                .retry
                .run("submissions page", || client.older_page(&p.name))
                .await?;
            self.page(row, &filings).await?;
        }
        Ok(())
    }

    async fn page(&mut self, row: &mut ReportRow, filings: &[Filing]) -> Result<()> {
        let (client, cik, from, to) = (self.client, self.cik, self.from, self.to);
        for f in filings
            .iter()
            .filter(|f| f.is_kept_form() && f.may_fall_in(from, to))
        {
            if !self.seen.insert(f.accession.clone()) {
                continue;
            }
            let published = match self.stored.get(&f.accession) {
                Some(&t) => {
                    self.tally.reused += 1;
                    t
                }
                None => {
                    row.reads += 1;
                    let a = self
                        .retry
                        .run("filing index", || client.index(cik, &f.accession))
                        .await?;
                    self.tally.read += 1;
                    self.check(row, f, &a);
                    a.published_ms
                }
            };
            if (from..to).contains(&published) {
                self.tally.kept += 1;
                self.pending
                    .push(filing_event(self.instrument, f, published));
                if self.pending.len() >= FLUSH {
                    self.flush(row).await?;
                }
            }
        }
        Ok(())
    }

    /// The cross-checks of one index read (module table, notes).
    fn check(&mut self, row: &mut ReportRow, f: &Filing, a: &IndexAcceptance) {
        if a.disagrees() {
            row.notes.push(format!(
                "index {}: Accepted {} New York = {}, its Last-Modified says {}; Accepted kept",
                f.accession,
                a.accepted_ny,
                fmt_time(a.published_ms),
                a.last_modified_ms.map(fmt_time).unwrap_or_default()
            ));
        }
        match f.json_accepted_ms.map(|j| json_clock(j, a.published_ms)) {
            Some(JsonClock::Utc) => self.tally.utc += 1,
            Some(JsonClock::NyOffsetAdded) => self.tally.ny_offset += 1,
            Some(JsonClock::Other(delta)) => {
                self.tally.other += 1;
                row.notes.push(format!(
                    "{}: acceptanceDateTime is {} s off the index time {}",
                    f.accession,
                    delta / 1000,
                    fmt_time(a.published_ms)
                ));
            }
            None => {}
        }
    }

    async fn flush(&mut self, row: &mut ReportRow) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let batch = std::mem::take(&mut self.pending);
        let n = self.store.put_events(SOURCE, &batch).await?;
        row.wrote(n, batch.iter().map(|e| e.published_ms));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::outbound::backfill::test_support::FAST;
    use crate::adapters::outbound::http_class::read_error;
    use crate::adapters::outbound::http_class::test_support::{
        canned, local_scope, serve, test_client, Canned,
    };
    use crate::adapters::outbound::market_data::SqliteMarketData;
    use crate::domain::marketdata::parse_time;

    const AAPL: &str = "hyperliquid:xyz:AAPL";

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

    fn own_limiters() -> &'static Limiters {
        Box::leak(Box::new(Limiters::default()))
    }

    fn client(base: &str, budget: Option<RateLimitConfig>) -> SecClient {
        SecClient::new(
            test_client(),
            base,
            base,
            local_scope(),
            budget,
            HeaderValue::from_static("tengu-test ops@example.com"),
        )
        .unwrap()
        .with_limiters(own_limiters())
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

    fn ciks() -> BTreeMap<String, String> {
        ticker_ciks(&serde_json::from_str(&fixture("company_tickers.json")).unwrap()).unwrap()
    }

    /// AAPL from 2026-01-01: the recent file, then one index page per kept
    /// filing in the span (newest first); the Form 4 and the 2015 8-K are
    /// never asked for; times are the index's, not the JSON's (+4 / +5 h).
    #[tokio::test]
    async fn filings_get_their_index_time_and_a_covered_span() {
        let (base, seen) = serve(vec![
            canned(200, fixture("company_tickers.json")),
            canned(200, fixture("CIK0000320193.json")),
            html("0001140361-26-035325-index.htm"),
            Canned {
                status: 503,
                headers: "",
                body: "busy".into(),
                delay_ms: 0,
            },
            html("0000320193-26-000020-index.htm"),
            html("0000320193-26-000018-index.htm"),
            html("0000320193-26-000006-index.htm"),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteMarketData::open(dir.path()).unwrap();
        let c = client(&base, None);
        let map = sec_ticker_map(&c, &FAST).await.unwrap();
        assert_eq!(map, ciks());
        let now = t("2026-10-08T12:00:00Z");
        let plan = SecPlan {
            instrument: AAPL.into(),
            from_ms: t("2026-01-01"),
            to_ms: i64::MAX,
        };
        let row = sec_filings(&c, &store, &map, &plan, &FAST, now).await;
        assert!(row.errors.is_empty(), "{row:?}");
        assert_eq!((row.kind, row.source.as_str()), ("filings", "sec"));
        assert_eq!((row.rows, row.reads), (4, 5), "{row:?}");
        assert_eq!(row.first_ms, Some(t("2026-01-30T11:01:32Z")));
        assert_eq!(row.last_ms, Some(t("2026-09-01T20:30:35Z")));
        assert_eq!(row.notes[0], "cik 0000320193 AAPL");
        assert!(
            row.notes
                .contains(&"acceptanceDateTime vs the index time: 0 true UTC, 4 + the New York offset, 0 other".to_string()),
            "{:?}",
            row.notes
        );

        let reqs = seen.lock().unwrap().clone();
        let paths: Vec<String> = reqs.iter().map(|r| target(r)).collect();
        assert_eq!(
            paths,
            vec![
                "/files/company_tickers.json",
                "/submissions/CIK0000320193.json",
                "/Archives/edgar/data/320193/000114036126035325/0001140361-26-035325-index.htm",
                "/Archives/edgar/data/320193/000032019326000020/0000320193-26-000020-index.htm",
                "/Archives/edgar/data/320193/000032019326000020/0000320193-26-000020-index.htm",
                "/Archives/edgar/data/320193/000032019326000018/0000320193-26-000018-index.htm",
                "/Archives/edgar/data/320193/000032019326000006/0000320193-26-000006-index.htm",
            ],
            "the 503 was retried"
        );
        for r in &reqs {
            assert!(
                r.to_ascii_lowercase()
                    .contains("user-agent: tengu-test ops@example.com"),
                "{r}"
            );
        }

        let events = store.events(AAPL, 0, i64::MAX).await.unwrap();
        let got: Vec<(String, String, String)> = events
            .iter()
            .map(|e| (fmt_time(e.published_ms), e.id.clone(), e.form.clone()))
            .collect();
        let want = [
            ("2026-01-30T11:01:32Z", "0000320193-26-000006", "10-Q"),
            ("2026-07-30T20:30:28Z", "0000320193-26-000018", "8-K"),
            ("2026-07-31T10:01:02Z", "0000320193-26-000020", "10-Q"),
            ("2026-09-01T20:30:35Z", "0001140361-26-035325", "8-K/A"),
        ];
        assert_eq!(
            got,
            want.iter()
                .map(|(a, b, c)| (a.to_string(), b.to_string(), c.to_string()))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            events[1].title.as_deref(),
            Some("items 2.02 Results of operations · 9.01 Financial statements and exhibits")
        );
        let cov = store.event_coverage(AAPL).await.unwrap();
        assert_eq!(
            cov,
            vec![EventCoverage {
                instrument: AAPL.into(),
                source: "sec".into(),
                from_ms: t("2026-01-01"),
                to_ms: now,
                covered: true,
                note: Some("cik 0000320193 AAPL".into()),
                fetched_at_ms: now,
            }]
        );

        // A re-run an hour later from 2026-07-01: the stored filings keep
        // their times (no index read); the coverage joins the stored span.
        let (base, seen) = serve(vec![canned(200, fixture("CIK0000320193.json"))]).await;
        let c = client(&base, None);
        let plan = SecPlan {
            from_ms: t("2026-07-01"),
            ..plan
        };
        let row = sec_filings(&c, &store, &map, &plan, &FAST, now + 3_600_000).await;
        assert!(row.errors.is_empty(), "{row:?}");
        assert_eq!((row.rows, row.reads), (3, 1), "{row:?}");
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert!(
            row.notes.contains(
                &"3 filing(s) in the span; 0 index page(s) read, 3 already stored (time kept)"
                    .to_string()
            ),
            "{:?}",
            row.notes
        );
        let cov = store.event_coverage(AAPL).await.unwrap();
        assert_eq!(
            (cov[0].from_ms, cov[0].to_ms),
            (t("2026-01-01"), now + 3_600_000)
        );
    }

    /// From 2015-07-01: the recent filings, then the older page; the 2015
    /// 8-K of the recent file and the one on the older page both kept.
    #[tokio::test]
    async fn an_older_span_reads_the_older_page() {
        let (base, seen) = serve(vec![
            canned(200, fixture("CIK0000320193.json")),
            html("0001193125-15-322466-index.htm"),
            canned(200, fixture("CIK0000320193-submissions-001.json")),
            html("0001193125-15-273023-index.htm"),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteMarketData::open(dir.path()).unwrap();
        let c = client(&base, None);
        let plan = SecPlan {
            instrument: AAPL.into(),
            from_ms: t("2015-07-25"),
            to_ms: t("2015-10-01"),
        };
        let row = sec_filings(&c, &store, &ciks(), &plan, &FAST, t("2026-10-08")).await;
        assert!(row.errors.is_empty(), "{row:?}");
        let paths: Vec<String> = seen.lock().unwrap().iter().map(|r| target(r)).collect();
        assert_eq!(
            paths,
            vec![
                "/submissions/CIK0000320193.json",
                "/Archives/edgar/data/320193/000119312515322466/0001193125-15-322466-index.htm",
                "/submissions/CIK0000320193-submissions-001.json",
                "/Archives/edgar/data/320193/000119312515273023/0001193125-15-273023-index.htm",
            ]
        );
        let ids: Vec<(String, String)> = store
            .events(AAPL, 0, i64::MAX)
            .await
            .unwrap()
            .into_iter()
            .map(|e| (fmt_time(e.published_ms), e.id))
            .collect();
        assert_eq!(
            ids,
            vec![
                ("2015-07-31T20:35:41Z".into(), "0001193125-15-273023".into()),
                ("2015-09-17T20:31:39Z".into(), "0001193125-15-322466".into()),
            ],
            "the 10-Q of 2015-07-22 is before the span"
        );
    }

    /// TSLA's JSON clock is true UTC — counted so; a ticker without a CIK
    /// is recorded as not covered (no request); a failed index read leaves
    /// the events before it and no coverage.
    #[tokio::test]
    async fn no_cik_true_utc_json_and_a_failed_read() {
        let (base, seen) = serve(vec![
            canned(200, fixture("CIK0001318605.json")),
            html("0001628280-26-049270-index.htm"),
            canned(404, "<html>Not Found</html>"),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteMarketData::open(dir.path()).unwrap();
        let c = client(&base, None);
        let now = t("2026-10-08");

        let smsn = SecPlan {
            instrument: "hyperliquid:xyz:SMSN".into(),
            from_ms: t("2026-03-01"),
            to_ms: i64::MAX,
        };
        let row = sec_filings(&c, &store, &ciks(), &smsn, &FAST, now).await;
        assert!(row.errors.is_empty(), "{row:?}");
        assert_eq!((row.rows, row.reads), (0, 0));
        assert_eq!(
            row.notes,
            vec!["no SEC CIK for ticker SMSN: coverage recorded as not covered"]
        );
        assert_eq!(
            store.event_coverage(&smsn.instrument).await.unwrap(),
            vec![EventCoverage {
                instrument: smsn.instrument.clone(),
                source: "sec".into(),
                from_ms: t("2026-03-01"),
                to_ms: now,
                covered: false,
                note: Some("no SEC CIK for ticker SMSN".into()),
                fetched_at_ms: now,
            }]
        );
        assert!(seen.lock().unwrap().is_empty(), "nothing sent");

        let tsla = SecPlan {
            instrument: "hyperliquid:xyz:TSLA".into(),
            from_ms: t("2026-07-01"),
            to_ms: t("2026-08-01"),
        };
        let row = sec_filings(&c, &store, &ciks(), &tsla, &FAST, now).await;
        assert_eq!(row.errors.len(), 1, "{row:?}");
        assert!(row.errors[0].contains("HTTP 404"), "{:?}", row.errors);
        assert_eq!(row.classes, vec![ErrorClass::Fatal]);
        assert_eq!(
            (row.rows, row.reads),
            (1, 3),
            "the 10-Q before the 404 stays"
        );
        assert!(
            row.notes.contains(
                &"acceptanceDateTime vs the index time: 1 true UTC, 0 + the New York offset, 0 other"
                    .to_string()
            ),
            "{:?}",
            row.notes
        );
        let events = store.events(&tsla.instrument, 0, i64::MAX).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, "0001628280-26-049270");
        assert_eq!(fmt_time(events[0].published_ms), "2026-07-23T01:02:31Z");
        assert!(store
            .event_coverage(&tsla.instrument)
            .await
            .unwrap()
            .is_empty());

        // Not an xyz stock: refused before any request.
        let btc = SecPlan {
            instrument: "hyperliquid:BTC".into(),
            ..tsla
        };
        let row = sec_filings(&c, &store, &ciks(), &btc, &FAST, now).await;
        assert!(
            row.errors[0].contains("not an xyz stock"),
            "{:?}",
            row.errors
        );
        assert_eq!(seen.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn user_agent_scope_and_budget() {
        let e = user_agent(None).unwrap_err().to_string();
        assert!(e.contains("SEC_USER_AGENT is not set"), "{e}");
        assert!(user_agent(Some("  ".into())).is_err());
        assert!(user_agent(Some("bad\nvalue".into()))
            .unwrap_err()
            .to_string()
            .contains("not a valid HTTP header value"));
        assert_eq!(
            user_agent(Some(" Tengu ops@example.com ".into())).unwrap(),
            "Tengu ops@example.com"
        );

        let (base, seen) = serve(vec![
            canned(200, fixture("company_tickers.json")),
            canned(200, "<html>maintenance</html>"),
        ])
        .await;
        // Scope denial: nothing sent.
        let denied = SecClient::new(
            test_client(),
            &base,
            &base,
            ToolScope::default(),
            None,
            HeaderValue::from_static("t t@example.com"),
        )
        .unwrap()
        .with_limiters(own_limiters());
        let e = denied.ticker_map().await.unwrap_err();
        assert_eq!(read_error("x", &e).class, ErrorClass::Fatal);
        assert!(format!("{e:#}").contains("net_hosts"), "{e:#}");
        assert!(seen.lock().unwrap().is_empty());

        // 1 request / minute, bucket of 1: the second is refused unsent.
        let budget = RateLimitConfig {
            per_minute: 1,
            burst: Some(1),
            exec_reserve: 0,
        };
        let mut c = client(&base, Some(budget));
        c.max_budget_wait = Duration::ZERO;
        c.ticker_map().await.unwrap();
        let e = c.submissions("0000320193").await.unwrap_err();
        let r = read_error("x", &e);
        assert_eq!(r.class, ErrorClass::RateLimited);
        assert!(r.message.contains("[rate_limits.sec]"), "{}", r.message);
        assert_eq!(seen.lock().unwrap().len(), 1);

        // A body that is not JSON: decode.
        let c = client(&base, None);
        let e = c.submissions("0000320193").await.unwrap_err();
        assert_eq!(read_error("x", &e).class, ErrorClass::Decode);
        assert!(format!("{e:#}").contains("not JSON"), "{e:#}");
        assert!(c.older_page("../x.json").await.is_err());
    }
}
