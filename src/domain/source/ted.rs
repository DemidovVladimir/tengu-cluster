//! EU TED Search API notices → source records (O2): the body of a one-day
//! search page, the decoder of its reply and one record per notice. An
//! allow-list of fields: contact data (persons, emails, phones, addresses)
//! is never asked for and never read. Pure; the POST and the store are
//! `adapters/outbound/sources/ted.rs`. Field names and shapes pinned from
//! replies captured 2026-10-08 (`tests/fixtures/ted/`).
//!
//! | Request (`POST /v3/notices/search`, anonymous) | Value |
//! |---|---|
//! | `query` | the registry row's template, `{from}` / `{to}` = the window's days as `YYYYMMDD` ([`query_for`]) |
//! | `fields` | [`TED_FIELDS`] |
//! | `page` · `limit` | 1-based · ≤ [`TED_MAX_LIMIT`]; `page × limit` ≤ [`TED_MAX_WINDOW`] (the server refuses more: `SEARCH_WINDOW_TOO_WIDE`) |
//! | `scope` · `paginationMode` | `ALL` (every notice, not only active ones; the server's default, sent explicitly) · `PAGE_NUMBER` |
//! | Coverage / cursor key | [`query_key`] = `query:<sha256 of the template>` |
//!
//! | Reply | Rule |
//! |---|---|
//! | page | `notices[]` + `totalNoticeCount`; `timedOut: true` = an incomplete result, an error; any other shape = an error for the page |
//! | a notice without `publication-number` | no record (nothing to key it on): a per-notice error |
//! | every other notice | one record: a field that does not read is a `parse_error` (`partial`); no `notice-type` ⇒ `unparsed` — never a dropped page |
//!
//! | Record field | Value |
//! |---|---|
//! | `native_id` · `url` | `publication-number` in full · `https://ted.europa.eu/en/notice/-/detail/<publication number>` |
//! | `event_key` | `ted:procedure:<procedure-identifier>`; a notice naming none (planning: `pin-only`, `pmc`, `pin-rtl`): `ted:notice:<publication number>` |
//! | `entities` | `ted:buyer:<buyer-country>:<buyer-identifier>` per id — paired with the notice's one buyer country, else index by index; an id with whitespace or without a digit (`N/A`) makes none (ids are never rewritten) |
//! | `published_ms` = `valid_from_ms` | TED gives a date: the end of `publication-date` in its own offset (`2026-10-01+02:00` → `2026-10-01T22:00:00Z`) — never before the notice was public; unreadable ⇒ the read time, `partial` |
//! | `valid_until_ms` | the tender deadline when it is after publication (a competition is open until then), else none |
//! | `fact` (`ted_notice`) | `notice-type`; `procedure-identifier`; `identifier-lot`; `buyer-name` (the `eng` names, else the first language's, `; `-joined — an organisation, never a contact person); places: buyer `buyer-country` (ISO 3166), performance `place-of-performance-country-proc/-lot` (ISO 3166) + `-subdiv-proc/-lot` (NUTS); CPV: `main-classification-proc` then `classification-cpv`; lists without repeats |
//! | deadline | the earliest lot `deadline-receipt-tender-date-lot` at its `-time-lot` (one time = every lot's; a date alone = the end of that day); times that do not pair ⇒ none, `partial` |
//! | value | `estimated-value-proc` + `estimated-value-cur-proc` as written (text; a JSON number is refused — not the amount as written); none ⇒ the lot value of a one-lot notice |
//! | change notice | `change-notice-version-identifier` set (a publication number or `<notice uuid>-<version>`): `supersedes` = [`change_target`] |
//! | `parser_version` · `access_method` | [`TED_PARSER_VERSION`] · `http_post` |

use std::collections::BTreeSet;

use chrono::{DateTime, Days, FixedOffset, NaiveDate, TimeZone};
use serde_json::{json, Map, Value};

use super::record::{
    split_parser_version, ted_buyer_entity, ted_notice_key, ted_procedure_key, valid_cpv,
    valid_token, AccessMethod, Fact, NativeAmount, ParseError, ParseStatus, Place, PlaceRole,
    PlaceScheme, SourceRecord, SourceStamp, TedNotice, RECORD_SCHEMA,
};
use crate::domain::canonical::sha256_hex;
use crate::domain::tz::Zone;

/// The parser of every TED record.
pub const TED_PARSER_VERSION: &str = "ted-search/1";
/// The public notice pages a record's `url` names.
pub const TED_WWW: &str = "https://ted.europa.eu";
/// The search endpoint's path.
pub const TED_SEARCH_PATH: &str = "/v3/notices/search";
/// The largest `limit` the server takes.
pub const TED_MAX_LIMIT: u32 = 250;
/// The largest `page × limit` the server takes.
pub const TED_MAX_WINDOW: u64 = 15_000;

/// Every field a search asks for (module table) — no contact field.
pub const TED_FIELDS: [&str; 21] = [
    "publication-number",
    "publication-date",
    "notice-type",
    "procedure-identifier",
    "identifier-lot",
    "change-notice-version-identifier",
    "buyer-name",
    "buyer-country",
    "buyer-identifier",
    "main-classification-proc",
    "classification-cpv",
    "place-of-performance-country-proc",
    "place-of-performance-subdiv-proc",
    "place-of-performance-country-lot",
    "place-of-performance-subdiv-lot",
    "deadline-receipt-tender-date-lot",
    "deadline-receipt-tender-time-lot",
    "estimated-value-proc",
    "estimated-value-cur-proc",
    "estimated-value-lot",
    "estimated-value-cur-lot",
];

/// `query:<sha256 of the template>` — coverage and cursor key of a row.
pub fn query_key(template: &str) -> String {
    format!("query:{}", sha256_hex(template))
}

/// The template with `{from}` / `{to}` = the days as `YYYYMMDD`.
pub fn query_for(template: &str, from: NaiveDate, to: NaiveDate) -> String {
    template
        .replace("{from}", &from.format("%Y%m%d").to_string())
        .replace("{to}", &to.format("%Y%m%d").to_string())
}

/// The request body of one search page (module table).
pub fn search_body(template: &str, from: NaiveDate, to: NaiveDate, page: u32, limit: u32) -> Value {
    json!({
        "query": query_for(template, from, to),
        "fields": TED_FIELDS,
        "page": page,
        "limit": limit,
        "scope": "ALL",
        "paginationMode": "PAGE_NUMBER",
    })
}

/// A publication day as a span `[start, end)` of TED's calendar
/// (Europe/Paris = Luxembourg time): the coverage of a one-day read.
pub fn day_span(day: NaiveDate) -> (i64, i64) {
    let start = |d: NaiveDate| Zone::Paris.at(d, 0, 0);
    let next = day.checked_add_days(Days::new(1)).unwrap_or(day);
    (start(day), start(next))
}

/// The TED publication day at `ms` (Europe/Paris).
pub fn day_of(ms: i64) -> NaiveDate {
    Zone::Paris.local_date(ms)
}

/// The public page of a notice.
pub fn notice_url(publication_number: &str) -> String {
    format!("{TED_WWW}/en/notice/-/detail/{publication_number}")
}

/// One search page as read.
#[derive(Debug, Clone, PartialEq)]
pub struct TedPage {
    /// Per notice in reply order: its read, or why no record can hold it.
    pub notices: Vec<Result<TedRead, String>>,
    /// `totalNoticeCount`: every notice the query matches.
    pub total: u64,
}

/// One notice as read (allow-listed fields only), before it is a record.
#[derive(Debug, Clone, PartialEq)]
pub struct TedRead {
    pub publication_number: String,
    /// The end of `publication-date` (module table); `None` when unreadable.
    pub published_ms: Option<i64>,
    /// `procedure-identifier` (also when the notice is `unparsed`).
    pub procedure_id: Option<String>,
    /// `change-notice-version-identifier` as written.
    pub changes: Option<String>,
    /// `ted:buyer:<country>:<id>`.
    pub entities: Vec<String>,
    /// `ted_notice`, or `unparsed` without a `notice-type`.
    pub fact: Fact,
    pub errors: Vec<ParseError>,
}

impl TedRead {
    /// `ted:procedure:<id>`, else `ted:notice:<publication number>`.
    pub fn event_key(&self) -> String {
        match &self.procedure_id {
            Some(p) => ted_procedure_key(p),
            None => ted_notice_key(&self.publication_number),
        }
    }
}

/// One search reply → its notices (module table: reply).
pub fn decode_page(v: &Value) -> Result<TedPage, String> {
    let o = v.as_object().ok_or("the reply is not a JSON object")?;
    if o.get("timedOut").and_then(Value::as_bool) == Some(true) {
        return Err("the search timed out: its notices may be incomplete".into());
    }
    let notices = o
        .get("notices")
        .and_then(Value::as_array)
        .ok_or("no `notices` array")?;
    let total = o
        .get("totalNoticeCount")
        .and_then(Value::as_u64)
        .ok_or("no `totalNoticeCount` count")?;
    Ok(TedPage {
        notices: notices
            .iter()
            .enumerate()
            .map(|(i, n)| decode_notice(n).map_err(|e| format!("notice {i} of the page: {e}")))
            .collect(),
        total,
    })
}

/// Field reads: absent / `null` = none; the wrong JSON type = an error.
struct Fields<'a> {
    o: &'a Map<String, Value>,
    errors: Vec<ParseError>,
}

impl<'a> Fields<'a> {
    fn bad(&mut self, field: &str, message: impl Into<String>) {
        self.errors.push(ParseError::new(field, message));
    }

    fn text(&mut self, field: &str) -> Option<&'a str> {
        match self.o.get(field) {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.as_str()),
            Some(other) => {
                self.bad(field, format!("a JSON {}, not text", kind(other)));
                None
            }
        }
    }

    /// A list of texts (a lone text = a list of one).
    fn texts(&mut self, field: &str) -> Vec<&'a str> {
        match self.o.get(field) {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::String(s)) => vec![s.as_str()],
            Some(Value::Array(items)) => {
                let mut out = Vec::new();
                for item in items {
                    match item {
                        Value::String(s) => out.push(s.as_str()),
                        other => self.bad(field, format!("an item is a JSON {}", kind(other))),
                    }
                }
                out
            }
            Some(other) => {
                self.bad(field, format!("a JSON {}, not a list", kind(other)));
                Vec::new()
            }
        }
    }

    /// A token (no whitespace) or none; anything else an error.
    fn token(&mut self, field: &str) -> Option<String> {
        let s = self.text(field)?;
        if valid_token(s) {
            Some(s.to_string())
        } else {
            self.bad(field, format!("`{s}` is empty or has whitespace"));
            None
        }
    }
}

fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Keep the first of each value.
fn unique<T: Ord + Clone>(items: impl IntoIterator<Item = T>) -> Vec<T> {
    let mut seen = BTreeSet::new();
    items
        .into_iter()
        .filter(|i| seen.insert(i.clone()))
        .collect()
}

/// One notice (module tables).
fn decode_notice(v: &Value) -> Result<TedRead, String> {
    let o = v.as_object().ok_or("not a JSON object")?;
    let publication_number = match o.get("publication-number") {
        Some(Value::String(s)) if valid_token(s) => s.clone(),
        None | Some(Value::Null) => {
            return Err("no publication-number: nothing to key a record on".into())
        }
        Some(other) => return Err(format!("publication-number {other} is not an id")),
    };
    let mut f = Fields {
        o,
        errors: Vec::new(),
    };
    let published_ms = match f.text("publication-date") {
        None => {
            f.bad("publication-date", "missing: the read time stands in");
            None
        }
        Some(d) => match date_with_offset(d) {
            Some((day, offset)) => end_of_day(day, offset),
            None => {
                f.bad(
                    "publication-date",
                    format!("`{d}` is not YYYY-MM-DD with an offset: the read time stands in"),
                );
                None
            }
        },
    };
    let procedure_id = f.token("procedure-identifier");
    let changes = f.token("change-notice-version-identifier");
    let lot_ids = unique(lots(&mut f));
    let (buyer_name, entities) = buyers(&mut f);
    let places = places(&mut f);
    let cpv = cpv(&mut f);
    let deadline_ms = deadline(&mut f);
    let value = value(&mut f, lot_ids.len());
    let fact = match f.token("notice-type") {
        Some(notice_type) => Fact::TedNotice(TedNotice {
            notice_type,
            procedure_id: procedure_id.clone(),
            lot_ids,
            buyer_name,
            places,
            cpv,
            deadline_ms,
            value,
        }),
        None => {
            if !f.errors.iter().any(|e| e.field == "notice-type") {
                f.bad("notice-type", "missing");
            }
            Fact::Unparsed {
                reason: "no readable notice-type: the notice is not typed".into(),
            }
        }
    };
    Ok(TedRead {
        publication_number,
        published_ms,
        procedure_id,
        changes,
        entities,
        fact,
        errors: f.errors,
    })
}

fn lots(f: &mut Fields) -> Vec<String> {
    let mut out = Vec::new();
    for l in f.texts("identifier-lot") {
        if valid_token(l) {
            out.push(l.to_string());
        } else {
            f.bad(
                "identifier-lot",
                format!("`{l}` is empty or has whitespace"),
            );
        }
    }
    out
}

/// The buyer organisation name(s) and the buyer entities (module table).
fn buyers(f: &mut Fields) -> (Option<String>, Vec<String>) {
    let name = match f.o.get("buyer-name") {
        None | Some(Value::Null) => None,
        Some(Value::Object(by_lang)) => {
            let lang = if by_lang.contains_key("eng") {
                Some("eng")
            } else {
                by_lang.keys().map(String::as_str).min()
            };
            let mut names = Vec::new();
            match lang.and_then(|l| by_lang.get(l)) {
                Some(Value::String(s)) => names.push(s.trim().to_string()),
                Some(Value::Array(items)) => {
                    for item in items {
                        match item {
                            Value::String(s) => names.push(s.trim().to_string()),
                            other => {
                                f.bad("buyer-name", format!("an item is a JSON {}", kind(other)))
                            }
                        }
                    }
                }
                Some(other) => f.bad(
                    "buyer-name",
                    format!("a language holds a JSON {}", kind(other)),
                ),
                None => {}
            }
            let names = unique(names.into_iter().filter(|n| !n.is_empty()));
            (!names.is_empty()).then(|| names.join("; "))
        }
        Some(other) => {
            f.bad(
                "buyer-name",
                format!("a JSON {}, not names by language", kind(other)),
            );
            None
        }
    };
    let countries = f.texts("buyer-country");
    let ids = f.texts("buyer-identifier");
    let distinct = unique(countries.iter().copied());
    let paired: Vec<(&str, &str)> = if distinct.len() == 1 {
        ids.iter().map(|id| (distinct[0], *id)).collect()
    } else if countries.len() == ids.len() {
        countries.iter().copied().zip(ids.iter().copied()).collect()
    } else {
        Vec::new()
    };
    let iso = |c: &str| {
        Place {
            role: PlaceRole::Buyer,
            scheme: PlaceScheme::Iso3166,
            code: c.to_string(),
        }
        .validate()
        .is_ok()
    };
    let entities = unique(
        paired
            .into_iter()
            .filter(|(c, id)| iso(c) && valid_token(id) && id.bytes().any(|b| b.is_ascii_digit()))
            .map(|(c, id)| ted_buyer_entity(c, id)),
    );
    (name, entities)
}

fn places(f: &mut Fields) -> Vec<Place> {
    let mut out = Vec::new();
    let fields = [
        ("buyer-country", PlaceRole::Buyer, PlaceScheme::Iso3166),
        (
            "place-of-performance-country-proc",
            PlaceRole::Performance,
            PlaceScheme::Iso3166,
        ),
        (
            "place-of-performance-subdiv-proc",
            PlaceRole::Performance,
            PlaceScheme::Nuts,
        ),
        (
            "place-of-performance-country-lot",
            PlaceRole::Performance,
            PlaceScheme::Iso3166,
        ),
        (
            "place-of-performance-subdiv-lot",
            PlaceRole::Performance,
            PlaceScheme::Nuts,
        ),
    ];
    for (field, role, scheme) in fields {
        for code in f.texts(field) {
            let p = Place {
                role,
                scheme,
                code: code.to_string(),
            };
            match p.validate() {
                Ok(()) => out.push(p),
                Err(e) => f.bad(field, e),
            }
        }
    }
    unique(out.into_iter().map(|p| (p.role, p.scheme, p.code)))
        .into_iter()
        .map(|(role, scheme, code)| Place { role, scheme, code })
        .collect()
}

fn cpv(f: &mut Fields) -> Vec<String> {
    let mut out = Vec::new();
    for field in ["main-classification-proc", "classification-cpv"] {
        for c in f.texts(field) {
            if valid_cpv(c) {
                out.push(c.to_string());
            } else {
                f.bad(field, format!("`{c}` is not a CPV code"));
            }
        }
    }
    unique(out)
}

/// The earliest lot deadline (module table: deadline).
fn deadline(f: &mut Fields) -> Option<i64> {
    let dates = f.texts("deadline-receipt-tender-date-lot");
    let times = f.texts("deadline-receipt-tender-time-lot");
    if dates.is_empty() {
        if !times.is_empty() {
            f.bad("deadline-receipt-tender-date-lot", "times without a date");
        }
        return None;
    }
    if !(times.is_empty() || times.len() == 1 || times.len() == dates.len()) {
        f.bad(
            "deadline-receipt-tender-time-lot",
            format!(
                "{} times for {} dates: they do not pair",
                times.len(),
                dates.len()
            ),
        );
        return None;
    }
    let mut earliest: Option<i64> = None;
    for (i, d) in dates.iter().enumerate() {
        let time = match times.len() {
            0 => None,
            1 => Some(times[0]),
            _ => Some(times[i]),
        };
        let Some(t) = deadline_instant(d, time) else {
            f.bad(
                "deadline-receipt-tender-date-lot",
                format!(
                    "`{d}` at `{}` is not a date and time with an offset",
                    time.unwrap_or("-")
                ),
            );
            return None;
        };
        earliest = Some(earliest.map_or(t, |e| e.min(t)));
    }
    earliest
}

/// `YYYY-MM-DD±HH:MM` (or `Z`) at `HH:MM:SS±HH:MM`; a date alone = the
/// end of that day in its offset.
fn deadline_instant(date: &str, time: Option<&str>) -> Option<i64> {
    let (day, offset) = date_with_offset(date)?;
    let Some(time) = time else {
        return end_of_day(day, offset);
    };
    let time = time.trim();
    let has_offset = time.ends_with('Z') || time.get(8..).is_some_and(|z| z.contains(['+', '-']));
    let text = if has_offset {
        format!("{}T{time}", day.format("%Y-%m-%d"))
    } else {
        format!("{}T{time}{offset}", day.format("%Y-%m-%d"))
    };
    DateTime::parse_from_rfc3339(&text)
        .ok()
        .map(|t| t.timestamp_millis())
}

/// `YYYY-MM-DD` + `±HH:MM` / `Z`; no offset ⇒ `None`.
fn date_with_offset(s: &str) -> Option<(NaiveDate, FixedOffset)> {
    let s = s.trim();
    if s.len() < 11 || !s.is_char_boundary(10) {
        return None;
    }
    let (date, zone) = s.split_at(10);
    let day = NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    let offset = if zone == "Z" {
        FixedOffset::east_opt(0)?
    } else {
        DateTime::parse_from_rfc3339(&format!("{date}T00:00:00{zone}"))
            .ok()?
            .offset()
            .to_owned()
    };
    Some((day, offset))
}

/// The next midnight after `day` in `offset`, UTC ms.
fn end_of_day(day: NaiveDate, offset: FixedOffset) -> Option<i64> {
    let next = day.checked_add_days(Days::new(1))?.and_hms_opt(0, 0, 0)?;
    offset
        .from_local_datetime(&next)
        .single()
        .map(|t| t.timestamp_millis())
}

/// The procedure's estimated value as written (module table: value).
fn value(f: &mut Fields, lots: usize) -> Option<NativeAmount> {
    let (amount, currency, field) = match f.o.get("estimated-value-proc") {
        None | Some(Value::Null) => {
            let amounts = f.texts("estimated-value-lot");
            let currencies = f.texts("estimated-value-cur-lot");
            if !(lots == 1 && amounts.len() == 1 && currencies.len() == 1) {
                return None;
            }
            (amounts[0], Some(currencies[0]), "estimated-value-lot")
        }
        Some(Value::String(a)) => (
            a.as_str(),
            f.text("estimated-value-cur-proc"),
            "estimated-value-proc",
        ),
        Some(other) => {
            f.bad(
                "estimated-value-proc",
                format!("a JSON {}, not the amount as written", kind(other)),
            );
            return None;
        }
    };
    let Some(currency) = currency else {
        f.bad(field, format!("`{amount}` has no currency"));
        return None;
    };
    let v = NativeAmount::new(amount.trim(), currency.trim());
    match v.validate() {
        Ok(()) => Some(v),
        Err(e) => {
            f.bad(field, e);
            None
        }
    }
}

/// The current-version order of the as-of view: newest read, then higher
/// parser, then parse time, then id.
fn version_key(r: &SourceRecord) -> (i64, u32, i64, &str) {
    let n = split_parser_version(&r.parser_version).map_or(0, |(_, n)| n);
    (r.observed_ms, n, r.parsed_ms, r.record_id.as_str())
}

/// The record a change notice corrects (module table: change notice),
/// among `prior` records of `source_id` on `event_key` other than this
/// notice: the newest version of the publication it names; else the newest
/// earlier record (published at or before it) with the same notice type
/// and a shared lot; then down that record's own corrections published at
/// or before it. `None` when the notice changes nothing or nothing fits.
pub fn change_target<'a>(
    read: &TedRead,
    source_id: &str,
    event_key: &str,
    published_ms: i64,
    prior: &'a [SourceRecord],
) -> Option<&'a SourceRecord> {
    let named = read.changes.as_deref()?;
    let ours = |r: &&SourceRecord| {
        r.source_id == source_id
            && r.event_key == event_key
            && r.native_id != read.publication_number
            && r.published_ms <= published_ms
    };
    let by_name = prior
        .iter()
        .filter(ours)
        .filter(|r| r.native_id == named)
        .max_by(|a, b| version_key(a).cmp(&version_key(b)));
    let start = by_name.or_else(|| {
        let Fact::TedNotice(me) = &read.fact else {
            return None;
        };
        let shares = |p: &TedNotice| {
            p.notice_type == me.notice_type
                && (p.lot_ids.iter().any(|l| me.lot_ids.contains(l))
                    || p.lot_ids.is_empty() && me.lot_ids.is_empty())
        };
        prior
            .iter()
            .filter(ours)
            .filter(|r| matches!(&r.fact, Fact::TedNotice(p) if shares(p)))
            .max_by(|a, b| (a.published_ms, version_key(a)).cmp(&(b.published_ms, version_key(b))))
    })?;
    let mut target = start;
    let mut seen = BTreeSet::from([target.record_id.as_str()]);
    while let Some(next) = prior
        .iter()
        .filter(ours)
        .filter(|r| r.supersedes.as_deref() == Some(target.record_id.as_str()))
        .max_by(|a, b| version_key(a).cmp(&version_key(b)))
    {
        if !seen.insert(next.record_id.as_str()) {
            break;
        }
        target = next;
    }
    Some(target)
}

/// The record of one notice (module tables), identity set; `prior` = the
/// stored and pending records of its source, for [`change_target`]. The
/// caller validates it (`SourceRecord::validate`) before the store does.
pub fn notice_record(
    stamp: &SourceStamp,
    read: &TedRead,
    prior: &[SourceRecord],
    snapshot: String,
    observed_ms: i64,
    parsed_ms: i64,
) -> SourceRecord {
    let published_ms = read.published_ms.unwrap_or(observed_ms);
    let event_key = read.event_key();
    let deadline = match &read.fact {
        Fact::TedNotice(n) => n.deadline_ms,
        _ => None,
    };
    let supersedes = change_target(read, &stamp.source_id, &event_key, published_ms, prior)
        .map(|r| r.record_id.clone());
    let parse = match (&read.fact, read.errors.is_empty()) {
        (Fact::Unparsed { .. }, _) => ParseStatus::Error,
        (_, true) => ParseStatus::Ok,
        (_, false) => ParseStatus::Partial,
    };
    SourceRecord {
        schema: RECORD_SCHEMA.into(),
        record_id: String::new(),
        source_id: stamp.source_id.clone(),
        source_class: stamp.source_class,
        trust: stamp.trust,
        native_id: read.publication_number.clone(),
        event_key,
        entities: read.entities.clone(),
        url: notice_url(&read.publication_number),
        published_ms,
        observed_ms,
        parsed_ms,
        valid_from_ms: published_ms,
        valid_until_ms: deadline.filter(|d| *d > published_ms),
        jurisdiction: stamp.jurisdiction.clone(),
        language: stamp.language.clone(),
        currency: read.fact.currency().map(String::from),
        content_hash: String::new(),
        snapshots: vec![snapshot],
        parser_version: TED_PARSER_VERSION.into(),
        license_or_terms: stamp.license_or_terms.clone(),
        terms_sha256: stamp.terms_sha256.clone(),
        access_method: AccessMethod::HttpPost,
        parse,
        parse_errors: read.errors.clone(),
        fact: read.fact.clone(),
        supersedes,
        origin: None,
    }
    .with_identity()
}

#[cfg(test)]
mod tests {
    //! Fixtures (`tests/fixtures/ted/`), captured with curl from
    //! `POST https://api.ted.europa.eu/v3/notices/search` on 2026-10-08,
    //! fields = [`TED_FIELDS`], pretty-printed, content unchanged unless
    //! said: `search_page_1.json` = the soe row's query for 2026-10-01, page
    //! 1, limit 4 (318 matches); `search_change_notice.json` = both notices
    //! of procedure `f08aa593-61f7-418e-90ec-043a09cc23b1` (657981-2026 and
    //! its change 674231-2026); `search_bad_notice.json` = three notices of
    //! 2026-10-01 HAND-EDITED: 674677-2026 as served (a `pmc`, no
    //! procedure), 674429-2026 with `publication-date` "01.10.2026" and a
    //! numeric `estimated-value-proc`, 674295-2026 without `notice-type`, and
    //! a copy of the first without `publication-number` and `links`;
    //! `totalNoticeCount` set to 4. Buyers are organisations; no contact
    //! field was asked for.
    use super::*;
    use crate::domain::canonical::canonical_json;
    use crate::domain::marketdata::{fmt_time, parse_time};
    use crate::domain::source::{
        as_of, AsOfInput, AsOfMode, Revision, SourceClass, SourcePolicy, SupersededBy, Trust,
    };
    use std::collections::BTreeMap;

    fn fixture(name: &str) -> Value {
        let path = format!("{}/tests/fixtures/ted/{name}", env!("CARGO_MANIFEST_DIR"));
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
        serde_json::from_str(&text).unwrap()
    }

    fn stamp() -> SourceStamp {
        SourceStamp {
            source_id: "ted_search".into(),
            source_class: SourceClass::LawRegulator,
            trust: Trust::Primary,
            jurisdiction: "EU".into(),
            language: "en".into(),
            license_or_terms: "synthetic test terms".into(),
            terms_sha256: sha256_hex("synthetic terms page"),
        }
    }

    fn t(s: &str) -> i64 {
        parse_time(s).unwrap()
    }

    fn reads(name: &str) -> Vec<Result<TedRead, String>> {
        decode_page(&fixture(name)).unwrap().notices
    }

    fn read_of(name: &str, publication: &str) -> TedRead {
        reads(name)
            .into_iter()
            .flatten()
            .find(|r| r.publication_number == publication)
            .unwrap()
    }

    fn record(read: &TedRead, prior: &[SourceRecord], observed: &str) -> SourceRecord {
        let at = t(observed);
        notice_record(
            &stamp(),
            read,
            prior,
            sha256_hex(&format!("page with {}", read.publication_number)),
            at,
            at + 5,
        )
    }

    fn notice(r: &SourceRecord) -> &TedNotice {
        match &r.fact {
            Fact::TedNotice(n) => n,
            other => panic!("ted_notice expected, got {other:?}"),
        }
    }

    #[test]
    fn page_decodes_notices_with_full_ids() {
        let page = decode_page(&fixture("search_page_1.json")).unwrap();
        assert_eq!(page.total, 318);
        assert_eq!(page.notices.len(), 4);
        let records: Vec<SourceRecord> = page
            .notices
            .iter()
            .map(|n| record(n.as_ref().unwrap(), &[], "2026-10-08T12:00:00Z"))
            .collect();
        for r in &records {
            assert_eq!(r.validate(), Ok(()), "{}", r.record_id);
            assert_eq!(r.parse, ParseStatus::Ok, "{:?}", r.parse_errors);
            assert_eq!(
                r.record_id,
                format!("ted_search:{}:{}", r.native_id, r.content_hash)
            );
            assert_eq!(
                r.url,
                format!("https://ted.europa.eu/en/notice/-/detail/{}", r.native_id)
            );
            // A date with its offset: the end of 2026-10-01 in +02:00.
            assert_eq!(fmt_time(r.published_ms), "2026-10-01T22:00:00Z");
            assert_eq!(r.valid_from_ms, r.published_ms);
            assert_eq!(
                (r.parser_version.as_str(), r.access_method),
                (TED_PARSER_VERSION, AccessMethod::HttpPost)
            );
        }
        // 674231-2026 (a change notice: no earlier record here, so no link).
        let cz = &records[0];
        assert_eq!(cz.native_id, "674231-2026");
        assert_eq!(
            cz.event_key,
            "ted:procedure:f08aa593-61f7-418e-90ec-043a09cc23b1"
        );
        assert_eq!(cz.entities, vec!["ted:buyer:CZE:60457856".to_string()]);
        assert_eq!(cz.supersedes, None);
        let n = notice(cz);
        assert_eq!(n.notice_type, "cn-standard");
        assert_eq!(
            n.procedure_id.as_deref(),
            Some("f08aa593-61f7-418e-90ec-043a09cc23b1")
        );
        assert_eq!(n.lot_ids, vec!["LOT-0001".to_string()]);
        assert_eq!(
            n.buyer_name.as_deref(),
            Some("Středisko společných činností AV ČR, v. v. i.")
        );
        // Main CPV first, then the rest, no repeats.
        assert_eq!(
            n.cpv,
            ["48730000", "72268000", "72261000", "72253200", "72800000"]
        );
        let places: Vec<String> = n
            .places
            .iter()
            .map(|p| format!("{}:{}:{}", p.role.as_str(), p.scheme.as_str(), p.code))
            .collect();
        assert_eq!(
            places,
            [
                "buyer:iso3166:CZE",
                "performance:iso3166:CZE",
                "performance:nuts:CZ010"
            ]
        );
        // 2026-11-04 at 10:00+01:00.
        assert_eq!(
            n.deadline_ms.map(fmt_time).as_deref(),
            Some("2026-11-04T09:00:00Z")
        );
        assert_eq!(cz.valid_until_ms, n.deadline_ms);
        assert_eq!(n.value, Some(NativeAmount::new("5300000", "CZK")));
        assert_eq!(cz.currency.as_deref(), Some("CZK"));
        // 674236-2026: no procedure value, one lot ⇒ the lot's, as written.
        assert_eq!(
            notice(&records[1]).value,
            Some(NativeAmount::new("401698.9", "PLN"))
        );
        // 674237-2026: a veat with no deadline and no value.
        let veat = &records[2];
        assert_eq!(notice(veat).notice_type, "veat");
        assert_eq!(
            (notice(veat).deadline_ms, veat.valid_until_ms),
            (None, None)
        );
        assert_eq!(
            (notice(veat).value.as_ref(), veat.currency.as_ref()),
            (None, None)
        );
        // 674254-2026: four lots, one deadline, the procedure's total.
        let it = notice(&records[3]);
        assert_eq!(it.lot_ids.len(), 4);
        assert_eq!(
            it.deadline_ms.map(fmt_time).as_deref(),
            Some("2026-11-04T14:00:00Z")
        );
        assert_eq!(it.value, Some(NativeAmount::new("456379200", "EUR")));
        assert_eq!(
            it.places
                .iter()
                .filter(|p| p.scheme == PlaceScheme::Nuts)
                .count(),
            1,
            "repeated lot places count once"
        );
        // Every id whole in the record's JSON.
        let text = serde_json::to_string(&records).unwrap();
        for whole in [
            "674231-2026",
            "f08aa593-61f7-418e-90ec-043a09cc23b1",
            "40ce8ea4-78ed-4d56-9ffa-92a46828c114",
            "ted:buyer:ITA:05359681003",
            "ted:buyer:POL:5840203593",
        ] {
            assert!(text.contains(whole), "`{whole}` not whole");
        }
        // Read again from other bytes later: the same record (the read is
        // not content).
        let again = notice_record(
            &stamp(),
            page.notices[0].as_ref().unwrap(),
            &[],
            sha256_hex("a later page"),
            t("2026-10-09T00:00:00Z"),
            t("2026-10-09T00:00:01Z"),
        );
        assert_eq!(again.record_id, cz.record_id);
    }

    #[test]
    fn change_notice_supersedes_within_its_procedure() {
        let original = read_of("search_change_notice.json", "657981-2026");
        let change = read_of("search_change_notice.json", "674231-2026");
        assert_eq!(original.changes, None);
        assert_eq!(change.changes.as_deref(), Some("657981-2026"));
        let old = record(&original, &[], "2026-09-25T08:00:00Z");
        assert_eq!(old.validate(), Ok(()));
        assert_eq!(
            notice(&old).deadline_ms.map(fmt_time).as_deref(),
            Some("2026-10-29T09:00:00Z")
        );
        assert_eq!(
            record(&change, &[], "2026-10-02T08:00:00Z").supersedes,
            None
        );
        let new = record(&change, std::slice::from_ref(&old), "2026-10-02T08:00:00Z");
        assert_eq!(new.validate(), Ok(()));
        assert_eq!(new.supersedes.as_deref(), Some(old.record_id.as_str()));
        assert_eq!(new.event_key, old.event_key);

        // The as-of view: the original is corrected away, no deadline conflict.
        let policies = BTreeMap::from([(
            "ted_search".to_string(),
            SourcePolicy {
                revision: Revision::Immutable,
                listing_max_age_ms: None,
            },
        )]);
        let records = vec![old.clone(), new.clone()];
        let input = AsOfInput {
            records: &records,
            coverage: &[],
            purges: &[],
            policies: &policies,
        };
        let view = as_of(&input, t("2026-10-03"), AsOfMode::Captured);
        let current: Vec<&str> = view
            .current
            .iter()
            .map(|c| c.record.record_id.as_str())
            .collect();
        assert_eq!(current, [new.record_id.as_str()]);
        assert!(view.superseded.iter().any(|s| s.old == old.record_id
            && s.new == new.record_id
            && s.by == SupersededBy::Correction));
        assert!(view.conflicts.is_empty(), "{:?}", view.conflicts);
        // Before the change was read: the original stands.
        let before = as_of(&input, t("2026-09-30"), AsOfMode::Captured);
        assert_eq!(before.current[0].record.record_id, old.record_id);

        // Another procedure's record is never the target.
        let mut stranger = old.clone();
        stranger.event_key = ted_procedure_key("00000000-0000-4000-8000-000000000001");
        let stranger = stranger.with_identity();
        assert_eq!(record(&change, &[stranger], "2026-10-02").supersedes, None);
        // Neither is a record published after the change.
        let mut later = old.clone();
        later.published_ms = t("2026-10-05");
        later.valid_from_ms = later.published_ms;
        let later = later.with_identity();
        assert_eq!(record(&change, &[later], "2026-10-06").supersedes, None);

        // A `<notice uuid>-<version>` id names no publication: the newest
        // earlier record of the procedure with this notice type and lot.
        let mut by_uuid = change.clone();
        by_uuid.changes = Some("8f61a91a-2dcc-4952-a472-435b0c3d9df3-01".into());
        let r = record(&by_uuid, std::slice::from_ref(&old), "2026-10-02");
        assert_eq!(r.supersedes.as_deref(), Some(old.record_id.as_str()));
        let mut other_lot = old.clone();
        if let Fact::TedNotice(n) = &mut other_lot.fact {
            n.lot_ids = vec!["LOT-0009".into()];
        }
        let other_lot = other_lot.with_identity();
        assert_eq!(
            record(&by_uuid, &[other_lot], "2026-10-02").supersedes,
            None
        );

        // A chain: the target was itself corrected — the newest link wins.
        let mut middle_read = change.clone();
        middle_read.publication_number = "660000-2026".into();
        middle_read.published_ms = Some(t("2026-09-27T22:00:00Z"));
        let middle = record(&middle_read, std::slice::from_ref(&old), "2026-09-28");
        assert_eq!(middle.supersedes.as_deref(), Some(old.record_id.as_str()));
        let last = record(&change, &[old.clone(), middle.clone()], "2026-10-02");
        assert_eq!(last.supersedes.as_deref(), Some(middle.record_id.as_str()));
    }

    #[test]
    fn contact_fields_are_never_kept() {
        let contact = [
            "buyer-email",
            "buyer-person",
            "buyer-tel",
            "buyer-fax",
            "buyer-touchpoint-email",
            "buyer-post-code",
            "buyer-city",
            "buyer-street",
            "organisation-email-buyer",
            "organisation-person-buyer",
            "touchpoint-tel-buyer",
            "buyer-internet-address",
            "buyer-contact-point",
        ];
        for f in TED_FIELDS {
            for word in [
                "email",
                "person",
                "tel",
                "fax",
                "phone",
                "post-code",
                "street",
                "city",
                "touchpoint",
                "contact",
                "internet-address",
            ] {
                assert!(!f.contains(word), "`{f}` asks for contact data");
            }
        }
        let mut page = fixture("search_page_1.json");
        let n = &mut page["notices"][0];
        for (i, field) in contact.iter().enumerate() {
            n[*field] = json!([format!("synthetic-contact-{i}@example.invalid")]);
        }
        n["buyer-name"]["eng"] = json!(["Example Buyer Agency"]);
        let read = decode_page(&page).unwrap().notices.remove(0).unwrap();
        let r = record(&read, &[], "2026-10-08");
        assert_eq!(r.validate(), Ok(()));
        let text = serde_json::to_string(&r).unwrap();
        assert!(!text.contains("synthetic-contact"), "{text}");
        assert!(!text.contains("example.invalid"), "{text}");
        // The `eng` name is preferred over the original language's.
        assert_eq!(
            notice(&r).buyer_name.as_deref(),
            Some("Example Buyer Agency")
        );
    }

    #[test]
    fn a_bad_notice_is_partial_not_a_dropped_page() {
        let page = decode_page(&fixture("search_bad_notice.json")).unwrap();
        assert_eq!((page.total, page.notices.len()), (4, 4));
        let observed = "2026-10-08T12:00:00Z";
        let rec = |i: usize| record(page.notices[i].as_ref().unwrap(), &[], observed);

        // A planning notice naming no procedure: ok, its own event.
        let pmc = rec(0);
        assert_eq!(pmc.validate(), Ok(()));
        assert_eq!(pmc.parse, ParseStatus::Ok);
        assert_eq!(pmc.event_key, "ted:notice:674677-2026");
        assert_eq!(notice(&pmc).procedure_id, None);
        assert_eq!(pmc.entities, vec!["ted:buyer:FIN:0952029-9".to_string()]);
        assert_eq!(
            notice(&pmc).value,
            Some(NativeAmount::new("50000000", "EUR"))
        );

        // An unreadable date and a numeric amount: partial, both named; the
        // read time stands in for publication; "N/A" makes no entity.
        let ie = rec(1);
        assert_eq!(ie.validate(), Ok(()));
        assert_eq!(ie.parse, ParseStatus::Partial);
        let fields: Vec<&str> = ie.parse_errors.iter().map(|e| e.field.as_str()).collect();
        assert_eq!(fields, ["publication-date", "estimated-value-proc"]);
        assert_eq!(ie.published_ms, t(observed));
        assert_eq!(notice(&ie).value, None);
        assert_eq!(ie.entities, vec!["ted:buyer:IRL:123456IE".to_string()]);
        assert!(notice(&ie).deadline_ms.is_some(), "the rest still reads");

        // No notice type: unparsed, still keyed on its procedure; two
        // buyers paired index by index.
        let fi = rec(2);
        assert_eq!(fi.validate(), Ok(()));
        assert_eq!(fi.parse, ParseStatus::Error);
        assert_eq!(fi.native_id, "674295-2026");
        assert_eq!(
            fi.event_key,
            "ted:procedure:78d76088-524a-4903-a15c-b1c3408f47a9"
        );
        assert!(matches!(fi.fact, Fact::Unparsed { .. }));
        assert_eq!(
            fi.entities,
            ["ted:buyer:FIN:2705965-1", "ted:buyer:FIN:3251568-2"]
        );

        // No publication number: no record, the reason named.
        let e = page.notices[3].as_ref().unwrap_err();
        assert!(
            e.contains("notice 3 of the page: no publication-number"),
            "{e}"
        );

        // Page-level refusals.
        for (bad, why) in [
            (json!([]), "not a JSON object"),
            (json!({"message": "x"}), "no `notices` array"),
            (json!({"notices": []}), "no `totalNoticeCount`"),
            (
                json!({"notices": [], "totalNoticeCount": 0, "timedOut": true}),
                "timed out",
            ),
        ] {
            let e = decode_page(&bad).unwrap_err();
            assert!(e.contains(why), "{e}");
        }
    }

    #[test]
    fn search_body_is_stable() {
        let template = "classification-cpv IN (72000000 48000000) AND publication-date >= {from} AND publication-date <= {to}";
        let day = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        let body = search_body(template, day, day, 2, 250);
        assert_eq!(body, search_body(template, day, day, 2, 250));
        let fields = TED_FIELDS.map(|f| format!("\"{f}\"")).join(",");
        assert_eq!(
            canonical_json(&body),
            format!(
                "{{\"fields\":[{fields}],\"limit\":250,\"page\":2,\"paginationMode\":\"PAGE_NUMBER\",\"query\":\"classification-cpv IN (72000000 48000000) AND publication-date >= 20261001 AND publication-date <= 20261001\",\"scope\":\"ALL\"}}"
            )
        );
        let key = query_key(template);
        assert_eq!(key, format!("query:{}", sha256_hex(template)));
        assert_eq!(key.len(), "query:".len() + 64);
        assert_ne!(key, query_key("publication-date = {from} {to}"));
        // The day's span in TED's calendar (CEST on 2026-10-01).
        let (start, end) = day_span(day);
        assert_eq!(
            (fmt_time(start).as_str(), fmt_time(end).as_str()),
            ("2026-09-30T22:00:00Z", "2026-10-01T22:00:00Z")
        );
        assert_eq!(day_of(start), day);
        assert_eq!(day_of(end - 1), day);
        // The publication instant is the end of that span.
        let read = read_of("search_page_1.json", "674231-2026");
        assert_eq!(read.published_ms, Some(end));
        // A date alone is the end of its day; `Z` reads as UTC; no offset
        // does not read.
        assert_eq!(
            deadline_instant("2026-11-04+01:00", None)
                .map(fmt_time)
                .as_deref(),
            Some("2026-11-04T23:00:00Z")
        );
        assert_eq!(
            deadline_instant("2026-11-04Z", Some("10:00:00Z"))
                .map(fmt_time)
                .as_deref(),
            Some("2026-11-04T10:00:00Z")
        );
        assert_eq!(
            deadline_instant("2026-11-04+01:00", Some("10:00:00"))
                .map(fmt_time)
                .as_deref(),
            Some("2026-11-04T09:00:00Z")
        );
        assert_eq!(deadline_instant("2026-11-04", None), None);
        assert_eq!(date_with_offset("01.10.2026"), None);
        // Text that is no date never panics, multi-byte characters included.
        for junk in [
            "",
            "2026-1",
            "2026-11-04+01:00é",
            "２０２６-11-04+01:00",
            "x",
        ] {
            assert_eq!(
                deadline_instant(junk, Some("10:0é:00+01:00")),
                None,
                "{junk}"
            );
            assert_eq!(deadline_instant(junk, None), None, "{junk}");
        }
        assert_eq!(
            deadline_instant("2026-11-04+01:00", Some("10:0é:00ü+01:00")),
            None
        );
    }
}
