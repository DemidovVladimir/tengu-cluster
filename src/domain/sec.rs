//! SEC EDGAR decoders (Phase 7, `tengu history events`): the ticker map, the
//! submissions JSON (recent + older pages), a filing index page's acceptance
//! time, and the [`MarketEvent`] a filing becomes. Pure — the fetcher is
//! `adapters/outbound/backfill/sec.rs`, the store `market.db` `events`.
//!
//! | Input | URL | Decoder | Gives |
//! |---|---|---|---|
//! | ticker map | `https://www.sec.gov/files/company_tickers.json` | [`ticker_ciks`] | ticker → CIK (10 digits, zero-padded) |
//! | submissions | `https://data.sec.gov/submissions/CIK##########.json` | [`submissions`] | `filings.recent` (≥ 1 year or 1 000 filings, newest first) + `filings.files[]` (older pages) |
//! | an older page | `https://data.sec.gov/submissions/<files[].name>` | [`filings_page`] | the same columns |
//! | filing index | `https://www.sec.gov/Archives/edgar/data/<cik>/<accession, no dashes>/<accession>-index.htm` | [`index_acceptance`] | `Accepted` (New York wall clock) → UTC = `published_ms` |
//!
//! | Time field (verified 2026-10-08: 12 filers by hand, then the 844 filings of the first `@xyz_stocks` run — every JSON value one of the two readings below; pinned in the tests) | Means | Used as |
//! |---|---|---|
//! | index page `Accepted` | New York wall clock (EST / EDT); the page's `Last-Modified` meta (GMT) is the same instant on every page checked | `published_ms` = `Zone::NewYork.to_utc_ms(Accepted)` |
//! | submissions `acceptanceDateTime` (`…Z`) | per CIK file either the true UTC instant (TSLA, NVDA, MSFT, GOOGL, ASML, NOK, AMD) or that instant + the New York offset once more (+4 h EDT, +5 h EST: AAPL, AMZN, META, BB, BABA) — nothing in the file tells which | a ± 6 h prefilter and the cross-check [`json_clock`]; never the time |
//! | `filingDate` | the EDGAR filing date (accepted after 17:30 New York ⇒ the next business day) | older-page selection only |
//!
//! | Rule | Value |
//! |---|---|
//! | Forms kept ([`SEC_FORMS`]) | current reports `8-K`, `8-K/A`, `6-K`, `6-K/A`; periodic reports `10-Q`, `10-Q/A`, `10-K`, `10-K/A`, `20-F`, `40-F` — no insider (`4`), proxy, registration or prospectus forms |
//! | Event | `kind = filing`, `id` = the accession number in full, `form`, `title` = the 8-K items with short names ([`filing_title`]), else `primaryDocDescription`; ≤ `EVENT_TITLE_MAX_CHARS` |
//! | Older pages ([`older_pages_to_read`]) | read only when `from` is before the oldest recent filing (+ 2 days of slack: a filing is dated up to the next business day after its acceptance), and only the pages whose `filingFrom` … `filingTo` (− 5 / + 2 days) reaches `[from, to)` |
//! | Ids | accession `##########-##-######`, page names `CIK##########-submissions-###.json` — anything else is a decode error (nothing is fetched by it) |

use std::collections::BTreeMap;

use chrono::{NaiveDate, NaiveDateTime};
use serde_json::Value;

use crate::domain::market::InstrumentId;
use crate::domain::marketdata::{MarketEvent, EVENT_TITLE_MAX_CHARS};
use crate::domain::tz::Zone;

const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 24 * HOUR_MS;
/// `acceptanceDateTime` is the true instant or up to 5 h past it (module
/// table); ± 6 h keeps every reading.
pub const JSON_SLACK_MS: i64 = 6 * HOUR_MS;

/// The forms stored as events (module table).
pub const SEC_FORMS: [&str; 10] = [
    "8-K", "8-K/A", "6-K", "6-K/A", "10-Q", "10-Q/A", "10-K", "10-K/A", "20-F", "40-F",
];

/// 8-K item → short name (Form 8-K General Instructions B).
const ITEMS_8K: [(&str, &str); 32] = [
    ("1.01", "Material agreement"),
    ("1.02", "Agreement terminated"),
    ("1.03", "Bankruptcy or receivership"),
    ("1.04", "Mine safety"),
    ("1.05", "Cybersecurity incident"),
    ("2.01", "Acquisition or disposition"),
    ("2.02", "Results of operations"),
    ("2.03", "Financial obligation"),
    ("2.04", "Obligation accelerated"),
    ("2.05", "Exit or disposal costs"),
    ("2.06", "Material impairment"),
    ("3.01", "Delisting or listing transfer"),
    ("3.02", "Unregistered equity sale"),
    ("3.03", "Holder rights modified"),
    ("4.01", "Accountant changed"),
    ("4.02", "Financials non-reliance"),
    ("5.01", "Change in control"),
    ("5.02", "Officers or directors"),
    ("5.03", "Articles, bylaws or fiscal year"),
    ("5.04", "Benefit plan trading suspended"),
    ("5.05", "Code of ethics"),
    ("5.06", "Shell company status"),
    ("5.07", "Shareholder vote"),
    ("5.08", "Director nominations"),
    ("6.01", "ABS material"),
    ("6.02", "Servicer or trustee changed"),
    ("6.03", "Credit enhancement changed"),
    ("6.04", "Distribution missed"),
    ("6.05", "Securities Act update"),
    ("7.01", "Regulation FD"),
    ("8.01", "Other events"),
    ("9.01", "Financial statements and exhibits"),
];

/// A CIK as EDGAR's file names write it: 10 digits, zero-padded.
pub fn cik10(n: u64) -> String {
    format!("{n:010}")
}

/// `hyperliquid:xyz:<TICKER>` → the SEC ticker (`.` written `-`, as EDGAR
/// writes `BRK-B`).
pub fn sec_ticker(instrument: &str) -> Result<String, String> {
    let bad = || format!("`{instrument}` is not an xyz stock (hyperliquid:xyz:<TICKER>)");
    let id = InstrumentId::parse(instrument).map_err(|_| bad())?;
    let ticker = (id.venue() == "hyperliquid")
        .then(|| id.native().strip_prefix("xyz:"))
        .flatten()
        .filter(|t| !t.is_empty() && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '.'))
        .ok_or_else(bad)?;
    Ok(ticker.to_ascii_uppercase().replace('.', "-"))
}

/// `company_tickers.json` (`{"0": {"cik_str", "ticker", "title"}, …}`) →
/// ticker → CIK (10 digits). A ticker listed twice keeps its first entry
/// (by the numeric key).
pub fn ticker_ciks(v: &Value) -> Result<BTreeMap<String, String>, String> {
    let map = v
        .as_object()
        .ok_or("company_tickers.json is not an object")?;
    let mut rows: Vec<(u64, &Value)> = Vec::with_capacity(map.len());
    for (k, row) in map {
        let n = k
            .parse::<u64>()
            .map_err(|_| format!("company_tickers.json key `{k}` is not a number"))?;
        rows.push((n, row));
    }
    rows.sort_by_key(|(n, _)| *n);
    let mut out = BTreeMap::new();
    for (n, row) in rows {
        let ticker = row
            .get("ticker")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| format!("company_tickers.json row {n}: no ticker"))?;
        let cik = match row.get("cik_str") {
            Some(Value::Number(c)) => c.as_u64(),
            Some(Value::String(c)) => c.trim().parse::<u64>().ok(),
            _ => None,
        }
        .filter(|c| *c > 0 && *c < 10_000_000_000)
        .ok_or_else(|| format!("company_tickers.json row {n} ({ticker}): no CIK"))?;
        out.entry(ticker.to_ascii_uppercase())
            .or_insert_with(|| cik10(cik));
    }
    Ok(out)
}

/// One filing row of a submissions page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filing {
    /// Accession number, in full (`0000320193-26-000018`).
    pub accession: String,
    pub form: String,
    /// `YYYY-MM-DD` (module table).
    pub filing_date: String,
    /// `acceptanceDateTime` as written (module table: not the time).
    pub json_accepted_ms: Option<i64>,
    /// 8-K items, comma-separated (`2.02,9.01`); empty otherwise.
    pub items: String,
    pub primary_doc_description: String,
}

impl Filing {
    /// Midnight UTC of `filing_date`.
    pub fn filing_date_ms(&self) -> Option<i64> {
        date_ms(&self.filing_date)
    }

    /// A kept form ([`SEC_FORMS`]).
    pub fn is_kept_form(&self) -> bool {
        SEC_FORMS.contains(&self.form.as_str())
    }

    /// Whether the filing can have been published in `[from, to)` (by
    /// `acceptanceDateTime` ± [`JSON_SLACK_MS`], else by `filingDate`) —
    /// the exact test runs on the index time.
    pub fn may_fall_in(&self, from_ms: i64, to_ms: i64) -> bool {
        match (self.json_accepted_ms, self.filing_date_ms()) {
            (Some(j), _) => {
                j >= from_ms.saturating_sub(JSON_SLACK_MS)
                    && j < to_ms.saturating_add(JSON_SLACK_MS)
            }
            (None, Some(d)) => {
                d > from_ms.saturating_sub(2 * DAY_MS) && d < to_ms.saturating_add(5 * DAY_MS)
            }
            (None, None) => false,
        }
    }
}

/// An older submissions page (`filings.files[]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OlderPage {
    /// `CIK##########-submissions-###.json`.
    pub name: String,
    pub filing_from: String,
    pub filing_to: String,
}

/// A submissions file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submissions {
    /// 10 digits.
    pub cik: String,
    pub name: String,
    /// Newest first.
    pub recent: Vec<Filing>,
    pub older: Vec<OlderPage>,
}

/// `CIK##########.json` → its CIK, name, recent filings and older pages.
pub fn submissions(v: &Value) -> Result<Submissions, String> {
    let cik = v
        .get("cik")
        .and_then(|c| match c {
            Value::String(s) => s.trim().parse::<u64>().ok(),
            Value::Number(n) => n.as_u64(),
            _ => None,
        })
        .map(cik10)
        .ok_or("submissions: no cik")?;
    let filings = v
        .get("filings")
        .ok_or_else(|| format!("submissions CIK{cik}: no filings"))?;
    let recent = filings
        .get("recent")
        .ok_or_else(|| format!("submissions CIK{cik}: no filings.recent"))?;
    let recent = filings_page(recent).map_err(|e| format!("submissions CIK{cik} recent: {e}"))?;
    let mut older = Vec::new();
    if let Some(files) = filings.get("files") {
        let files = files
            .as_array()
            .ok_or_else(|| format!("submissions CIK{cik}: filings.files is not a list"))?;
        for (i, f) in files.iter().enumerate() {
            let field = |k: &str| {
                f.get(k)
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .ok_or_else(|| format!("submissions CIK{cik}: files[{i}] has no {k}"))
            };
            let name = field("name")?;
            if !is_page_name(&name) {
                return Err(format!(
                    "submissions CIK{cik}: files[{i}] `{name}` is not CIK##########-submissions-###.json"
                ));
            }
            let (filing_from, filing_to) = (field("filingFrom")?, field("filingTo")?);
            if date_ms(&filing_from).is_none() || date_ms(&filing_to).is_none() {
                return Err(format!(
                    "submissions CIK{cik}: files[{i}] `{name}` dates are not YYYY-MM-DD"
                ));
            }
            older.push(OlderPage {
                name,
                filing_from,
                filing_to,
            });
        }
    }
    Ok(Submissions {
        name: v
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        cik,
        recent,
        older,
    })
}

/// The column object of `filings.recent` or of an older page → filings, in
/// the file's order. Required columns `accessionNumber`, `form`,
/// `filingDate`, `acceptanceDateTime` (empty = none); `items`,
/// `primaryDocDescription` default to empty.
pub fn filings_page(v: &Value) -> Result<Vec<Filing>, String> {
    let col = |k: &str, required: bool| -> Result<Option<&Vec<Value>>, String> {
        match v.get(k) {
            Some(Value::Array(a)) => Ok(Some(a)),
            Some(_) => Err(format!("column {k} is not a list")),
            None if required => Err(format!("no column {k}")),
            None => Ok(None),
        }
    };
    let acc = col("accessionNumber", true)?.ok_or("no column accessionNumber")?;
    let n = acc.len();
    let mut cols = Vec::new();
    for (k, required) in [
        ("form", true),
        ("filingDate", true),
        ("acceptanceDateTime", true),
        ("items", false),
        ("primaryDocDescription", false),
    ] {
        let c = col(k, required)?;
        if let Some(c) = c {
            if c.len() != n {
                return Err(format!(
                    "column {k} has {} rows, accessionNumber {n}",
                    c.len()
                ));
            }
        }
        cols.push(c);
    }
    let text = |c: Option<&Vec<Value>>, i: usize| {
        c.and_then(|c| c[i].as_str())
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    let mut out = Vec::with_capacity(n);
    for (i, a) in acc.iter().enumerate() {
        let accession = a.as_str().unwrap_or_default().trim().to_string();
        if !is_accession(&accession) {
            return Err(format!(
                "row {i}: accession `{accession}` is not ##########-##-######"
            ));
        }
        let filing_date = text(cols[1], i);
        if date_ms(&filing_date).is_none() {
            return Err(format!(
                "row {i} ({accession}): filingDate `{filing_date}` is not YYYY-MM-DD"
            ));
        }
        let accepted = text(cols[2], i);
        let json_accepted_ms = if accepted.is_empty() {
            None
        } else {
            Some(
                chrono::DateTime::parse_from_rfc3339(&accepted)
                    .map_err(|e| {
                        format!("row {i} ({accession}): acceptanceDateTime `{accepted}`: {e}")
                    })?
                    .timestamp_millis(),
            )
        };
        out.push(Filing {
            accession,
            form: text(cols[0], i),
            filing_date,
            json_accepted_ms,
            items: text(cols[3], i),
            primary_doc_description: text(cols[4], i),
        });
    }
    Ok(out)
}

/// The older pages a read of `[from, to)` needs (module table), in file
/// order; none when `from` is inside the recent filings.
pub fn older_pages_to_read(subs: &Submissions, from_ms: i64, to_ms: i64) -> Vec<&OlderPage> {
    let oldest_recent = subs.recent.iter().filter_map(Filing::filing_date_ms).min();
    if oldest_recent.is_some_and(|d| from_ms >= d.saturating_add(2 * DAY_MS)) {
        return Vec::new();
    }
    subs.older
        .iter()
        .filter(|p| {
            let (Some(a), Some(b)) = (date_ms(&p.filing_from), date_ms(&p.filing_to)) else {
                return false;
            };
            b.saturating_add(2 * DAY_MS) > from_ms && a.saturating_sub(5 * DAY_MS) < to_ms
        })
        .collect()
}

/// What a filing index page says about its acceptance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexAcceptance {
    /// `Accepted`, New York wall clock, as written (`2026-07-30 16:30:28`).
    pub accepted_ny: String,
    /// That wall time in UTC (`Zone::NewYork`, DST-aware) — `published_ms`.
    pub published_ms: i64,
    /// The page's `<meta http-equiv="Last-Modified">` (GMT), when present.
    pub last_modified_ms: Option<i64>,
}

impl IndexAcceptance {
    /// The `Last-Modified` meta names another instant (a re-disseminated
    /// index); `Accepted` is kept.
    pub fn disagrees(&self) -> bool {
        self.last_modified_ms
            .is_some_and(|lm| lm != self.published_ms)
    }
}

/// A filing index page → its acceptance (module table). The page must name
/// `accession`.
pub fn index_acceptance(html: &str, accession: &str) -> Result<IndexAcceptance, String> {
    if !html.contains(accession) {
        return Err(format!("the index page does not name {accession}"));
    }
    let accepted_ny = info_after(html, ">Accepted<")
        .ok_or_else(|| format!("index {accession}: no Accepted line"))?;
    let local = NaiveDateTime::parse_from_str(&accepted_ny, "%Y-%m-%d %H:%M:%S").map_err(|e| {
        format!("index {accession}: Accepted `{accepted_ny}` is not YYYY-MM-DD HH:MM:SS ({e})")
    })?;
    let last_modified_ms = meta_last_modified(html);
    Ok(IndexAcceptance {
        published_ms: Zone::NewYork.to_utc_ms(local),
        accepted_ny,
        last_modified_ms,
    })
}

/// The text of the first `<div class="info">` after `marker`.
fn info_after(html: &str, marker: &str) -> Option<String> {
    let rest = &html[html.find(marker)? + marker.len()..];
    let open = "<div class=\"info\">";
    let rest = &rest[rest.find(open)? + open.len()..];
    let text = rest[..rest.find('<')?].trim();
    (!text.is_empty()).then(|| text.to_string())
}

fn meta_last_modified(html: &str) -> Option<i64> {
    let at = html.find("http-equiv=\"Last-Modified\"")?;
    let rest = &html[at..];
    let rest = &rest[rest.find("content=\"")? + "content=\"".len()..];
    let value = &rest[..rest.find('"')?];
    chrono::DateTime::parse_from_rfc2822(value.trim())
        .ok()
        .map(|t| t.timestamp_millis())
}

/// How a submissions `acceptanceDateTime` relates to the index time
/// (module table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonClock {
    /// The true UTC instant.
    Utc,
    /// The true instant + the New York offset (+4 h EDT, +5 h EST).
    NyOffsetAdded,
    /// Anything else: the difference, ms.
    Other(i64),
}

pub fn json_clock(json_ms: i64, published_ms: i64) -> JsonClock {
    let delta = json_ms - published_ms;
    if delta == 0 {
        JsonClock::Utc
    } else if delta == -Zone::NewYork.offset_ms(published_ms) {
        JsonClock::NyOffsetAdded
    } else {
        JsonClock::Other(delta)
    }
}

/// The event title (module table): [`filing_title_full`] cut to
/// `EVENT_TITLE_MAX_CHARS`.
pub fn filing_title(f: &Filing) -> Option<String> {
    filing_title_full(f).map(|t| clip(&t, EVENT_TITLE_MAX_CHARS))
}

/// The 8-K items with short names, else `primaryDocDescription` — whole (a
/// source record keeps it uncut, `domain/source/sec_records.rs`).
pub fn filing_title_full(f: &Filing) -> Option<String> {
    let items: Vec<String> = f
        .items
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|code| match ITEMS_8K.iter().find(|(c, _)| *c == code) {
            Some((c, name)) => format!("{c} {name}"),
            None => code.to_string(),
        })
        .collect();
    let title = if items.is_empty() {
        f.primary_doc_description.trim().to_string()
    } else {
        format!("items {}", items.join(" · "))
    };
    (!title.is_empty()).then_some(title)
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The `filing` event of `f` for `instrument`, published at `published_ms`
/// (the index time).
pub fn filing_event(instrument: &str, f: &Filing, published_ms: i64) -> MarketEvent {
    MarketEvent {
        instrument: instrument.to_string(),
        published_ms,
        kind: "filing".to_string(),
        id: f.accession.clone(),
        form: f.form.clone(),
        title: filing_title(f),
    }
}

/// `/Archives/edgar/data/<cik>/<accession, no dashes>/<accession>-index.htm`.
pub fn index_path(cik10: &str, accession: &str) -> Result<String, String> {
    let cik = cik10
        .parse::<u64>()
        .map_err(|_| format!("CIK `{cik10}` is not a number"))?;
    if !is_accession(accession) {
        return Err(format!(
            "accession `{accession}` is not ##########-##-######"
        ));
    }
    Ok(format!(
        "/Archives/edgar/data/{cik}/{}/{accession}-index.htm",
        accession.replace('-', "")
    ))
}

/// `##########-##-######`.
pub fn is_accession(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 3
        && [10, 2, 6]
            .iter()
            .zip(&parts)
            .all(|(n, p)| p.len() == *n && p.bytes().all(|b| b.is_ascii_digit()))
}

/// `CIK##########-submissions-###.json`.
pub fn is_page_name(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("CIK").and_then(|r| r.strip_suffix(".json")) else {
        return false;
    };
    let Some((cik, page)) = rest.split_once("-submissions-") else {
        return false;
    };
    cik.len() == 10
        && cik.bytes().all(|b| b.is_ascii_digit())
        && !page.is_empty()
        && page.bytes().all(|b| b.is_ascii_digit())
}

fn date_ms(s: &str) -> Option<i64> {
    NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d")
        .ok()?
        .and_hms_opt(0, 0, 0)
        .map(|t| t.and_utc().timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::marketdata::{fmt_time, parse_time};

    fn fixture(name: &str) -> String {
        let path = format!("{}/tests/fixtures/sec/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    fn json(name: &str) -> Value {
        serde_json::from_str(&fixture(name)).unwrap()
    }

    fn t(s: &str) -> i64 {
        parse_time(s).unwrap()
    }

    #[test]
    fn the_ticker_map_gives_ten_digit_ciks() {
        let m = ticker_ciks(&json("company_tickers.json")).unwrap();
        assert_eq!(m["AAPL"], "0000320193");
        assert_eq!(m["TSLA"], "0001318605");
        assert_eq!(m["NVDA"], "0001045810");
        assert_eq!(m["GOOG"], m["GOOGL"]);
        assert_eq!(m["BRK-B"], "0001067983");
        assert_eq!(m.len(), 8);
        assert_eq!(cik10(320193), "0000320193");
        let e = ticker_ciks(&serde_json::json!({"0": {"ticker": "X"}})).unwrap_err();
        assert!(e.contains("no CIK"), "{e}");
        // A repeated ticker keeps its first row by the numeric key ("10" > "9").
        let m = ticker_ciks(&serde_json::json!({
            "10": {"cik_str": 2, "ticker": "dup"},
            "9": {"cik_str": "1", "ticker": "DUP"}
        }))
        .unwrap();
        assert_eq!(m["DUP"], "0000000001");
    }

    #[test]
    fn xyz_ids_map_to_sec_tickers() {
        assert_eq!(sec_ticker("hyperliquid:xyz:AAPL").unwrap(), "AAPL");
        assert_eq!(sec_ticker("hyperliquid:xyz:brk.b").unwrap(), "BRK-B");
        for bad in [
            "hyperliquid:BTC",
            "hyperliquid:xyz:",
            "hyperliquid:xyz:A/B",
            "solana:So11111111111111111111111111111111111111112",
            "AAPL",
        ] {
            let e = sec_ticker(bad).unwrap_err();
            assert!(e.contains(bad) && e.contains("not an xyz stock"), "{e}");
        }
    }

    #[test]
    fn submissions_decode_recent_and_older_pages() {
        let s = submissions(&json("CIK0000320193.json")).unwrap();
        assert_eq!(
            (s.cik.as_str(), s.name.as_str()),
            ("0000320193", "Apple Inc.")
        );
        let ids: Vec<&str> = s.recent.iter().map(|f| f.accession.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "0001140361-26-038674",
                "0001140361-26-035325",
                "0000320193-26-000020",
                "0000320193-26-000018",
                "0000320193-26-000006",
                "0001193125-15-322466",
            ]
        );
        let kept: Vec<&str> = s
            .recent
            .iter()
            .filter(|f| f.is_kept_form())
            .map(|f| f.form.as_str())
            .collect();
        assert_eq!(
            kept,
            vec!["8-K/A", "10-Q", "8-K", "10-Q", "8-K"],
            "Form 4 dropped"
        );
        let k8 = &s.recent[3];
        assert_eq!(k8.items, "2.02,9.01");
        assert_eq!(k8.filing_date, "2026-07-30");
        assert_eq!(k8.json_accepted_ms, Some(t("2026-07-31T00:30:28Z")));
        assert_eq!(
            s.older,
            vec![OlderPage {
                name: "CIK0000320193-submissions-001.json".into(),
                filing_from: "1994-01-26".into(),
                filing_to: "2015-09-15".into(),
            }]
        );
        let page = filings_page(&json("CIK0000320193-submissions-001.json")).unwrap();
        let forms: Vec<(&str, &str)> = page
            .iter()
            .map(|f| (f.accession.as_str(), f.form.as_str()))
            .collect();
        assert_eq!(
            forms,
            vec![
                ("0001193125-15-318535", "424B2"),
                ("0001193125-15-273023", "8-K"),
                ("0001193125-15-259935", "10-Q"),
            ]
        );

        // Older pages only when `from` is before the oldest recent filing
        // (2015-09-17, + 2 days of slack).
        assert!(older_pages_to_read(&s, t("2015-09-19"), t("2026-10-08")).is_empty());
        assert_eq!(
            older_pages_to_read(&s, t("2015-09-16"), t("2026-10-08")).len(),
            1
        );
        assert_eq!(
            older_pages_to_read(&s, t("2015-07-01"), t("2026-10-08")).len(),
            1
        );
        // Before the oldest recent filing but after the page's last date
        // (2015-09-15, + 2 days): nothing older to read.
        assert!(older_pages_to_read(&s, t("2015-09-18"), t("2026-10-08")).is_empty());
        // A page that ends long before `from`, or starts after `to`: skipped.
        let mut early = s.clone();
        early.recent.clear();
        assert!(older_pages_to_read(&early, t("2016-01-01"), t("2026-10-08")).is_empty());
        assert!(older_pages_to_read(&early, t("1980-01-01"), t("1990-01-01")).is_empty());

        for (v, needle) in [
            (
                serde_json::json!({"accessionNumber": ["1"], "form": ["8-K"], "filingDate": ["2026-01-01"], "acceptanceDateTime": [""]}),
                "is not ##########-##-######",
            ),
            (
                serde_json::json!({"accessionNumber": ["0000320193-26-000018"], "form": [], "filingDate": ["2026-01-01"], "acceptanceDateTime": [""]}),
                "column form has 0 rows",
            ),
            (
                serde_json::json!({"accessionNumber": [], "form": []}),
                "no column filingDate",
            ),
            (
                serde_json::json!({"accessionNumber": ["0000320193-26-000018"], "form": ["8-K"], "filingDate": ["July"], "acceptanceDateTime": [""]}),
                "filingDate `July`",
            ),
        ] {
            let e = filings_page(&v).unwrap_err();
            assert!(e.contains(needle), "{e}");
        }
        let mut bad = json("CIK0000320193.json");
        bad["filings"]["files"][0]["name"] = "../../x.json".into();
        assert!(submissions(&bad)
            .unwrap_err()
            .contains("is not CIK##########-submissions"));
    }

    /// The point-in-time key (module table), pinned on real filings
    /// (fetched 2026-10-08): the index page's `Accepted` is New York wall
    /// time and its `Last-Modified` meta the same instant in GMT; the
    /// submissions `acceptanceDateTime` of AAPL carries the New York offset
    /// once more, TSLA's is the true instant.
    #[test]
    fn acceptance_is_new_york_time_and_the_json_clock_varies_by_filer() {
        let cases = [
            // (accession, Accepted New York, published UTC, JSON `Z` value, clock)
            (
                "0000320193-26-000018", // AAPL 8-K, EDT
                "2026-07-30 16:30:28",
                "2026-07-30T20:30:28Z",
                "2026-07-31T00:30:28Z",
                JsonClock::NyOffsetAdded,
            ),
            (
                "0000320193-26-000006", // AAPL 10-Q, EST
                "2026-01-30 06:01:32",
                "2026-01-30T11:01:32Z",
                "2026-01-30T16:01:32Z",
                JsonClock::NyOffsetAdded,
            ),
            (
                "0001193125-15-273023", // AAPL 8-K on the older page, EDT
                "2015-07-31 16:35:41",
                "2015-07-31T20:35:41Z",
                "2015-08-01T00:35:41Z",
                JsonClock::NyOffsetAdded,
            ),
            (
                "0001628280-26-049213", // TSLA 8-K, EDT
                "2026-07-22 16:35:52",
                "2026-07-22T20:35:52Z",
                "2026-07-22T20:35:52Z",
                JsonClock::Utc,
            ),
            (
                "0001628280-26-049270", // TSLA 10-Q after hours: filed the next day
                "2026-07-22 21:02:31",
                "2026-07-23T01:02:31Z",
                "2026-07-23T01:02:31Z",
                JsonClock::Utc,
            ),
        ];
        for (acc, ny, utc, json_z, clock) in cases {
            let a = index_acceptance(&fixture(&format!("{acc}-index.htm")), acc).unwrap();
            assert_eq!(a.accepted_ny, ny, "{acc}");
            assert_eq!(fmt_time(a.published_ms), utc, "{acc}");
            assert_eq!(
                a.last_modified_ms,
                Some(a.published_ms),
                "{acc}: meta = Accepted"
            );
            assert!(!a.disagrees());
            assert_eq!(json_clock(t(json_z), a.published_ms), clock, "{acc}");
        }
        // The usual guess for a `Z` that is not UTC — New York wall time
        // labelled `Z` — would be 4 h early here: neither reading of the
        // JSON is safe, only the index is.
        assert_eq!(
            json_clock(t("2026-07-30T16:30:28Z"), t("2026-07-30T20:30:28Z")),
            JsonClock::Other(-4 * HOUR_MS)
        );

        let page = fixture("0000320193-26-000018-index.htm");
        let e = index_acceptance(&page, "0000320193-26-000020").unwrap_err();
        assert!(e.contains("does not name 0000320193-26-000020"), "{e}");
        let e = index_acceptance(
            &page.replace("2026-07-30 16:30:28", "30 July"),
            "0000320193-26-000018",
        )
        .unwrap_err();
        assert!(e.contains("Accepted `30 July`"), "{e}");
        let moved = index_acceptance(
            &page.replace("20:30:28 GMT", "21:00:00 GMT"),
            "0000320193-26-000018",
        )
        .unwrap();
        assert!(moved.disagrees());
        assert_eq!(fmt_time(moved.published_ms), "2026-07-30T20:30:28Z");
    }

    #[test]
    fn events_carry_the_accession_form_and_a_short_title() {
        let s = submissions(&json("CIK0000320193.json")).unwrap();
        let e = filing_event(
            "hyperliquid:xyz:AAPL",
            &s.recent[3],
            t("2026-07-30T20:30:28Z"),
        );
        assert_eq!(
            e,
            MarketEvent {
                instrument: "hyperliquid:xyz:AAPL".into(),
                published_ms: t("2026-07-30T20:30:28Z"),
                kind: "filing".into(),
                id: "0000320193-26-000018".into(),
                form: "8-K".into(),
                title: Some(
                    "items 2.02 Results of operations · 9.01 Financial statements and exhibits"
                        .into()
                ),
            }
        );
        assert!(e.validate().is_ok());
        assert_eq!(filing_title(&s.recent[2]).as_deref(), Some("10-Q"));
        assert_eq!(
            filing_title(&s.recent[1]).as_deref(),
            Some("items 5.02 Officers or directors")
        );
        let long = Filing {
            items: vec!["9.99"; 100].join(","),
            ..s.recent[3].clone()
        };
        let title = filing_title(&long).unwrap();
        assert_eq!(title.chars().count(), EVENT_TITLE_MAX_CHARS);
        assert!(title.ends_with('…') && title.starts_with("items 9.99 · 9.99"));
        let none = Filing {
            items: String::new(),
            primary_doc_description: " ".into(),
            ..s.recent[3].clone()
        };
        assert_eq!(filing_title(&none), None);
    }

    #[test]
    fn prefilter_paths_and_ids() {
        let s = submissions(&json("CIK0000320193.json")).unwrap();
        let k8 = &s.recent[3]; // JSON 2026-07-31T00:30:28Z, true 2026-07-30T20:30:28Z
        assert!(k8.may_fall_in(t("2026-07-30T20:00:00Z"), t("2026-07-30T21:00:00Z")));
        assert!(k8.may_fall_in(t("2026-07-31T06:00:00Z"), t("2026-08-01")));
        assert!(!k8.may_fall_in(t("2026-07-31T06:30:29Z"), t("2026-08-01")));
        assert!(!k8.may_fall_in(t("2026-07-01"), t("2026-07-30T18:30:28Z")));
        let undated = Filing {
            json_accepted_ms: None,
            ..k8.clone()
        };
        assert!(undated.may_fall_in(t("2026-07-31"), t("2026-08-01")));
        assert!(!undated.may_fall_in(t("2026-08-02"), t("2026-08-03")));
        assert_eq!(
            index_path("0000320193", "0000320193-26-000018").unwrap(),
            "/Archives/edgar/data/320193/000032019326000018/0000320193-26-000018-index.htm"
        );
        assert!(index_path("0000320193", "0000320193-26-00001/").is_err());
        assert!(index_path("x", "0000320193-26-000018").is_err());
        assert!(is_accession("0001628280-26-049213"));
        assert!(!is_accession("0001628280-26-04921"));
        assert!(is_page_name("CIK0000320193-submissions-001.json"));
        assert!(!is_page_name("CIK0000320193-submissions-.json"));
        assert!(!is_page_name("CIK320193-submissions-001.json"));
    }
}
