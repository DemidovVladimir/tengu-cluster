//! SEC EDGAR filings → source records (O2): one record per kept filing of a
//! CIK, built from its submissions row and its index page's acceptance (the
//! `domain/sec.rs` decoders). Pure; the fetch and the store are
//! `adapters/outbound/sources/sec.rs`.
//!
//! | Field | Value |
//! |---|---|
//! | `native_id` · `event_key` | the accession in full · `sec:filing:<accession>` — an amendment (`8-K/A`) has its own accession, so its own record and event (the submissions file names no amended filing) |
//! | `entities` | `sec:cik:<10 digits>` |
//! | `url` | the public index page `https://www.sec.gov/Archives/edgar/data/<cik>/<accession, no dashes>/<accession>-index.htm`, whatever base the client read |
//! | `published_ms` = `valid_from_ms` | the index page's `Accepted`, New York → UTC (`domain::sec::index_acceptance`) |
//! | index unread or undecodable | `parse = partial` naming why; `published_ms` = the later of `acceptanceDateTime` as written and the end of `filingDate` in New York ([`conservative_published_ms`]) — never before the true acceptance (the JSON clock is the instant or that + the New York offset; a filing is dated on or after its acceptance day) |
//! | `fact` | `sec_filing`: CIK, form, the 8-K items, title = `filing_title_full` (whole), else the form |
//! | `snapshots` | the submissions body, + the index body when it was read |
//! | `parser_version` · `access_method` | [`SEC_PARSER_VERSION`] · `http_get` |
//! | `observed_ms` | the fetch time of the newest snapshot it rests on (the caller's) |

use chrono::{Days, NaiveDate};

use super::record::{
    sec_cik_entity, sec_filing_key, AccessMethod, Fact, ParseError, ParseStatus, SecFiling,
    SourceRecord, SourceStamp, RECORD_SCHEMA,
};
use crate::domain::sec::{filing_title_full, index_path, Filing};
use crate::domain::tz::Zone;

/// The parser of every SEC record.
pub const SEC_PARSER_VERSION: &str = "sec-submissions/1";
/// The public archive host a record's `url` names.
pub const SEC_WWW: &str = "https://www.sec.gov";

/// Where a filing's `published_ms` comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilingTime {
    /// The index page's `Accepted` (read now, or kept from a stored record
    /// that read it).
    Accepted(i64),
    /// The index page gave no time: the field and why (module table).
    Unread(ParseError),
}

/// The public index page of a filing (module table).
pub fn filing_url(cik10: &str, accession: &str) -> Result<String, String> {
    Ok(format!("{SEC_WWW}{}", index_path(cik10, accession)?))
}

/// The 8-K items of `f`, as written (`2.02`, `9.01`); empty for other forms.
pub fn filing_items(f: &Filing) -> Vec<String> {
    f.items
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// A publication time never before `f`'s true acceptance (module table):
/// the later of `acceptanceDateTime` as written and midnight New York after
/// `filingDate`. `None` when `filingDate` is not `YYYY-MM-DD`.
pub fn conservative_published_ms(f: &Filing) -> Option<i64> {
    let day = NaiveDate::parse_from_str(f.filing_date.trim(), "%Y-%m-%d").ok()?;
    let next = day.checked_add_days(Days::new(1))?.and_hms_opt(0, 0, 0)?;
    let end_of_day = Zone::NewYork.to_utc_ms(next);
    Some(f.json_accepted_ms.map_or(end_of_day, |j| j.max(end_of_day)))
}

/// The record of filing `f` of `cik10` (module table), identity set. The
/// caller validates it (`SourceRecord::validate`) before the store does.
pub fn filing_record(
    stamp: &SourceStamp,
    cik10: &str,
    f: &Filing,
    time: FilingTime,
    snapshots: Vec<String>,
    observed_ms: i64,
    parsed_ms: i64,
) -> Result<SourceRecord, String> {
    let url = filing_url(cik10, &f.accession)?;
    let (published_ms, parse, parse_errors) = match time {
        FilingTime::Accepted(t) => (t, ParseStatus::Ok, Vec::new()),
        FilingTime::Unread(e) => {
            let t = conservative_published_ms(f).ok_or_else(|| {
                format!(
                    "{}: filingDate `{}` is not YYYY-MM-DD",
                    f.accession, f.filing_date
                )
            })?;
            (t, ParseStatus::Partial, vec![e])
        }
    };
    Ok(SourceRecord {
        schema: RECORD_SCHEMA.into(),
        record_id: String::new(),
        source_id: stamp.source_id.clone(),
        source_class: stamp.source_class,
        trust: stamp.trust,
        native_id: f.accession.clone(),
        event_key: sec_filing_key(&f.accession),
        entities: vec![sec_cik_entity(cik10)],
        url,
        published_ms,
        observed_ms,
        parsed_ms,
        valid_from_ms: published_ms,
        valid_until_ms: None,
        jurisdiction: stamp.jurisdiction.clone(),
        language: stamp.language.clone(),
        currency: None,
        content_hash: String::new(),
        snapshots,
        parser_version: SEC_PARSER_VERSION.into(),
        license_or_terms: stamp.license_or_terms.clone(),
        terms_sha256: stamp.terms_sha256.clone(),
        access_method: AccessMethod::HttpGet,
        parse,
        parse_errors,
        fact: Fact::SecFiling(SecFiling {
            cik: cik10.to_string(),
            form: f.form.clone(),
            items: filing_items(f),
            title: filing_title_full(f).unwrap_or_else(|| f.form.clone()),
        }),
        supersedes: None,
        origin: None,
    }
    .with_identity())
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;
    use crate::domain::canonical::sha256_hex;
    use crate::domain::marketdata::{fmt_time, parse_time};
    use crate::domain::sec::{index_acceptance, submissions};
    use crate::domain::source::{SourceClass, Trust};

    fn fixture(name: &str) -> String {
        let path = format!("{}/tests/fixtures/sec/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    fn stamp() -> SourceStamp {
        SourceStamp {
            source_id: "sec_edgar".into(),
            source_class: SourceClass::CompanyPrimary,
            trust: Trust::Primary,
            jurisdiction: "US".into(),
            language: "en".into(),
            license_or_terms: "synthetic test terms".into(),
            terms_sha256: sha256_hex("synthetic terms page"),
        }
    }

    fn apple() -> Vec<Filing> {
        let v: Value = serde_json::from_str(&fixture("CIK0000320193.json")).unwrap();
        submissions(&v).unwrap().recent
    }

    fn filing(accession: &str) -> Filing {
        apple()
            .into_iter()
            .find(|f| f.accession == accession)
            .unwrap()
    }

    fn accepted(accession: &str) -> i64 {
        let html = fixture(&format!("{accession}-index.htm"));
        index_acceptance(&html, accession).unwrap().published_ms
    }

    fn record(accession: &str, time: FilingTime) -> SourceRecord {
        let subs = sha256_hex(&fixture("CIK0000320193.json"));
        let index = sha256_hex(&fixture(&format!("{accession}-index.htm")));
        let observed = parse_time("2026-10-08T12:00:00Z").unwrap();
        filing_record(
            &stamp(),
            "0000320193",
            &filing(accession),
            time,
            vec![subs, index],
            observed,
            observed + 5,
        )
        .unwrap()
    }

    #[test]
    fn filing_records_carry_full_provenance() {
        let acc = "0000320193-26-000018";
        let r = record(acc, FilingTime::Accepted(accepted(acc)));
        assert_eq!(r.validate(), Ok(()));
        assert_eq!(r.record_id, format!("sec_edgar:{acc}:{}", r.content_hash));
        assert_eq!(
            (r.native_id.as_str(), r.event_key.as_str()),
            (acc, "sec:filing:0000320193-26-000018")
        );
        assert_eq!(r.entities, vec!["sec:cik:0000320193".to_string()]);
        assert_eq!(
            r.url,
            "https://www.sec.gov/Archives/edgar/data/320193/000032019326000018/0000320193-26-000018-index.htm"
        );
        // The index time, not the JSON clock (+4 h for AAPL).
        assert_eq!(fmt_time(r.published_ms), "2026-07-30T20:30:28Z");
        assert_eq!(r.valid_from_ms, r.published_ms);
        assert_eq!(
            (r.parse, r.parser_version.as_str()),
            (ParseStatus::Ok, SEC_PARSER_VERSION)
        );
        assert_eq!(
            (r.source_class, r.trust, r.access_method),
            (
                SourceClass::CompanyPrimary,
                Trust::Primary,
                AccessMethod::HttpGet
            )
        );
        assert_eq!(r.snapshots.len(), 2);
        let Fact::SecFiling(f) = &r.fact else {
            panic!("sec_filing expected")
        };
        assert_eq!(
            (f.cik.as_str(), f.form.as_str(), f.items.clone()),
            ("0000320193", "8-K", vec!["2.02".into(), "9.01".into()])
        );
        assert_eq!(
            f.title,
            "items 2.02 Results of operations · 9.01 Financial statements and exhibits"
        );
        // Read again later from new bytes: the same record id (the read is
        // not content); a 10-Q titles itself by its description.
        let mut again = r.clone();
        again.observed_ms += 3_600_000;
        again.parsed_ms += 3_600_000;
        again.snapshots = vec![sha256_hex("later submissions body")];
        assert_eq!(again.with_identity().record_id, r.record_id);
        let q = record(
            "0000320193-26-000020",
            FilingTime::Accepted(accepted("0000320193-26-000020")),
        );
        let Fact::SecFiling(qf) = &q.fact else {
            panic!("sec_filing expected")
        };
        assert_eq!((qf.form.as_str(), qf.title.as_str()), ("10-Q", "10-Q"));
        assert!(qf.items.is_empty());
    }

    #[test]
    fn an_amendment_is_its_own_record() {
        let k8 = "0000320193-26-000018";
        let k8a = "0001140361-26-035325";
        let orig = record(k8, FilingTime::Accepted(accepted(k8)));
        let amend = record(k8a, FilingTime::Accepted(accepted(k8a)));
        assert_eq!(amend.validate(), Ok(()));
        let Fact::SecFiling(f) = &amend.fact else {
            panic!("sec_filing expected")
        };
        assert_eq!(f.form, "8-K/A");
        assert_eq!(amend.native_id, k8a);
        assert_eq!(amend.event_key, "sec:filing:0001140361-26-035325");
        assert_eq!(
            amend.supersedes, None,
            "the submissions name no amended filing"
        );
        assert_ne!(amend.event_key, orig.event_key);
        assert_eq!(fmt_time(amend.published_ms), "2026-09-01T20:30:35Z");
        // Filed by an agent: the archive path keeps the filer's CIK.
        assert!(amend
            .url
            .contains("/data/320193/000114036126035325/0001140361-26-035325-index.htm"));
    }

    #[test]
    fn an_unread_index_is_partial_and_never_early() {
        for acc in [
            "0000320193-26-000018",
            "0000320193-26-000006",
            "0001140361-26-035325",
        ] {
            let why = ParseError::new("index", "index page not read (fatal, HTTP 404)");
            let r = record(acc, FilingTime::Unread(why.clone()));
            assert_eq!(r.validate(), Ok(()), "{acc}");
            assert_eq!(r.parse, ParseStatus::Partial);
            assert_eq!(r.parse_errors, vec![why]);
            let truth = accepted(acc);
            assert!(
                r.published_ms >= truth,
                "{acc}: {} is before the acceptance {}",
                fmt_time(r.published_ms),
                fmt_time(truth)
            );
            assert!(
                r.published_ms - truth < 2 * 86_400_000,
                "{acc}: within days"
            );
            // A different record from the one with the index time.
            assert_ne!(
                r.record_id,
                record(acc, FilingTime::Accepted(truth)).record_id
            );
        }
        // No JSON clock: midnight New York after the filing date (EDT: 04:00Z).
        let mut f = filing("0000320193-26-000018");
        f.json_accepted_ms = None;
        assert_eq!(
            conservative_published_ms(&f).map(fmt_time).as_deref(),
            Some("2026-07-31T04:00:00Z")
        );
        f.filing_date = "July".into();
        assert_eq!(conservative_published_ms(&f), None);
        let e = filing_record(
            &stamp(),
            "0000320193",
            &f,
            FilingTime::Unread(ParseError::new("index", "x")),
            vec![sha256_hex("b")],
            0,
            0,
        )
        .unwrap_err();
        assert!(e.contains("filingDate `July`"), "{e}");
    }
}
