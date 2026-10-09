//! Evaluation set of the source layer (O2 exit; compiled for tests only):
//! 12 dated source-level cases, replayed from
//! `tests/fixtures/sources/eval/cases.json` alone — each case's records,
//! coverage, tombstones, policies, instant t, mode and query, its expected
//! packet (canonical JSON) and a hand-written summary of what it must show.
//! Opportunity-level (O0) cases join later in their own set; this harness
//! does not change for them.
//!
//! | # | Case | t | Records from | Shows |
//! |---|---|---|---|---|
//! | 01 | `fresh_filing` | 2026-07-30T21:00Z | SEC 8-K 0000320193-26-000018 (`tests/fixtures/sec/`) read 5 min after acceptance | one fact, `confirmed` (primary) |
//! | 02 | `amendment_after_t` | 2026-08-15 knowable | the 8-K + its 8-K/A 0001140361-26-035325 accepted 2026-09-01 | only the 8-K: a later amendment is never back-dated |
//! | 03 | `ted_change_notice_after_t` | 2026-09-30 knowable | TED 657981-2026 + its change notice 674231-2026 (published 10-01), both read 10-02 (`tests/fixtures/ted/`) | the original stands; no supersession before the change is public |
//! | 04 | `duplicate_notice_two_pages` | 2026-10-02T12:00Z | the four notices of a search page; page 2 re-serves 674231-2026 a minute later | each notice once; the duplicate keeps its first read |
//! | 05 | `syndicated_copy` | 2026-07-31 | synthetic: an outlet's report of the 8-K + two wire copies of it | `single` with one origin — copies never corroborate |
//! | 06 | `stale_coverage` | 2026-10-06 | the search page; complete fetches of 09-29 and 10-01, two failed fetches | the newest complete fetch's age, one gap, two failed fetches — `partial` |
//! | 07 | `parse_failure` | 2026-10-02T12:00Z | the hand-edited bad page (`search_bad_notice.json`) | one `unparsed`, one `partial` parse, a failed (decode) fetch |
//! | 08 | `purged_raw` | 2026-08-02 | the 8-K; a raw purge on 08-01 | the fact stays with its snapshot hashes; the tombstone shows |
//! | 09 | `expired_tender` | 2026-10-30 | 657981-2026 alone (deadline 2026-10-29T09:00Z) | `expired`, not in force |
//! | 10 | `conflicting_values` | 2026-10-03 | the change notice read before its target was stored (no `supersedes`), the original after | both stand; a `deadline_ms` conflict |
//! | 11 | `in_place_edit` | 2026-10-01T12:00Z knowable | synthetic: a registry listing read 10-01, edited and re-read 10-02 | the first read only: an in-place edit is visible from its read, never its publication |
//! | 12 | `no_evidence_hold` | 2026-07-31 | the 8-K; the queried CIK (0001318605) fetched, nothing filed | no row (`absent`) — HOLD |
//!
//! | Check | Test |
//! |---|---|
//! | the 12 cases in order; every record well-formed and its id recomputes from its content (stored records stay readable); the packet at t equals `packet` byte for byte; its summary equals `expect` | `eval_set_replays_without_lookahead` |
//! | no lookahead: the packet at t is unchanged under every split-world move after t (`checks.rs`: delete, next ms, flood, coverage, reparse — not in knowable mode — and purge) | same |
//! | removing every copy whose origin is visible at t changes no event's confidence or origins; a copy's own source is never an origin | `no_case_counts_a_copy_as_confirmation` |
//!
//! The set's shape and its summary are below; the builder from the captures,
//! the file IO and the tests sit in `tests`.
//!
//! Regenerate (only when a case or the packet shape changes; review the
//! diff): `TENGU_REGEN_SOURCE_EVAL=1 cargo test --bin tengu
//! domain::source::eval` rebuilds every case from the captures above and
//! [`super::testkit`], checks each hand summary, and rewrites the file.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::asof::{AsOfMode, Coverage, Purge};
use super::packet::{AsOfQuery, EvidencePacket};
use super::record::SourceRecord;
use super::rules::{Revision, SourcePolicy};
use super::testkit::World;
use crate::domain::marketdata::fmt_time;
use crate::domain::observation::Observed;

/// The file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EvalSet {
    schema: String,
    about: String,
    cases: Vec<Case>,
}

/// One registry row's policy, as stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyRow {
    revision: Revision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    listing_max_age_ms: Option<i64>,
}

impl From<SourcePolicy> for PolicyRow {
    fn from(p: SourcePolicy) -> Self {
        Self {
            revision: p.revision,
            listing_max_age_ms: p.listing_max_age_ms,
        }
    }
}

/// One dated case (module table).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    id: String,
    /// t, RFC 3339.
    date: String,
    about: String,
    /// Where its records come from.
    inputs: String,
    /// Holds records of synthetic (testkit) sources.
    synthetic: bool,
    at_ms: i64,
    mode: AsOfMode,
    query: AsOfQuery,
    policies: BTreeMap<String, PolicyRow>,
    records: Vec<SourceRecord>,
    #[serde(default)]
    coverage: Vec<Coverage>,
    #[serde(default)]
    purges: Vec<Purge>,
    /// What the packet must show, written by hand.
    expect: Expect,
    /// The packet at t (its canonical JSON is compared).
    packet: Value,
}

impl Case {
    fn world(&self) -> World {
        World {
            name: "eval",
            records: self.records.clone(),
            coverage: self.coverage.clone(),
            purges: self.purges.clone(),
            policies: self
                .policies
                .iter()
                .map(|(id, p)| {
                    (
                        id.clone(),
                        SourcePolicy {
                            revision: p.revision,
                            listing_max_age_ms: p.listing_max_age_ms,
                        },
                    )
                })
                .collect(),
            span: (self.at_ms, self.at_ms),
        }
    }

    fn build(&self, w: &World) -> EvidencePacket {
        EvidencePacket::build(&w.input(), self.at_ms, self.mode, &self.query)
    }
}

/// A packet in a few lines (module table): records as `<native id>
/// read=<observed>`, sorted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Expect {
    /// `ok` · `partial` · `absent` (absent = no evidence: HOLD).
    status: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    facts: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pending: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    expired: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    withdrawn: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    unparsed: Vec<String>,
    /// `<old> → <new> (<by>)`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    superseded: Vec<String>,
    /// `<event> <confidence> origins=<a,b|->`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    events: Vec<String>,
    /// `<event> <field>[ lots=<a,b>]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    conflicts: Vec<String>,
    /// Per source `<source> failed=… parse_failures=… partial_parses=…
    /// purges=…`, then per query `<source> <query> last_fetch=… gaps=…`.
    freshness: Vec<String>,
}

/// `p`'s summary (module table); `records` names the record ids of a
/// supersession.
fn summary(p: &EvidencePacket, records: &[SourceRecord]) -> Expect {
    // Each id at its earliest read, as the view keeps it.
    let mut by_id: BTreeMap<&str, &SourceRecord> = BTreeMap::new();
    for r in records {
        let e = by_id.entry(r.record_id.as_str()).or_insert(r);
        if r.observed_ms < e.observed_ms {
            *e = r;
        }
    }
    let read = |native: &str, observed: i64| format!("{native} read={}", fmt_time(observed));
    let name = |id: &str| -> String {
        let r = by_id
            .get(id)
            .unwrap_or_else(|| panic!("{id} is no record of the case"));
        read(&r.native_id, r.observed_ms)
    };
    let sorted = |mut v: Vec<String>| {
        v.sort();
        v
    };
    let list = |v: &[String]| {
        if v.is_empty() {
            "-".to_string()
        } else {
            v.join(",")
        }
    };
    let mut freshness = Vec::new();
    for f in &p.freshness {
        freshness.push(format!(
            "{} failed={} parse_failures={} partial_parses={} purges={}",
            f.source_id,
            f.failed.len(),
            f.parse_failures,
            f.partial_parses,
            f.purges.len()
        ));
        for q in &f.queries {
            freshness.push(format!(
                "{} {} last_fetch={} gaps={}",
                f.source_id,
                q.query_key,
                fmt_time(q.last_fetch_ms),
                q.gaps.len()
            ));
        }
    }
    Expect {
        status: p.status().as_str().to_string(),
        facts: sorted(
            p.facts
                .iter()
                .map(|f| read(&f.native_id, f.observed_ms))
                .collect(),
        ),
        pending: sorted(
            p.pending
                .iter()
                .map(|f| read(&f.native_id, f.observed_ms))
                .collect(),
        ),
        expired: sorted(
            p.expired
                .iter()
                .map(|f| read(&f.native_id, f.observed_ms))
                .collect(),
        ),
        withdrawn: sorted(
            p.withdrawn
                .iter()
                .map(|w| read(&w.native_id, w.observed_ms))
                .collect(),
        ),
        unparsed: sorted(
            p.unparsed
                .iter()
                .map(|u| read(&u.native_id, u.observed_ms))
                .collect(),
        ),
        superseded: sorted(
            p.superseded
                .iter()
                .map(|s| {
                    let by = match s.by {
                        super::asof::SupersededBy::Revision => "revision",
                        super::asof::SupersededBy::Correction => "correction",
                    };
                    format!("{} → {} ({by})", name(&s.old), name(&s.new))
                })
                .collect(),
        ),
        events: p
            .events
            .iter()
            .map(|e| {
                format!(
                    "{} {} origins={}",
                    e.event_key,
                    e.confidence.as_str(),
                    list(&e.origins)
                )
            })
            .collect(),
        conflicts: p
            .conflicts
            .iter()
            .map(|c| {
                let lots = if c.lots.is_empty() {
                    String::new()
                } else {
                    format!(" lots={}", c.lots.join(","))
                };
                format!("{} {}{lots}", c.event_key, c.field)
            })
            .collect(),
        freshness,
    }
}

#[cfg(test)]
mod tests {
    use chrono::NaiveDate;

    use super::*;
    use crate::domain::canonical::{canonical_json, sha256_hex};
    use crate::domain::marketdata::parse_time;
    use crate::domain::sec::{index_acceptance, submissions};
    use crate::domain::source::asof::as_of;
    use crate::domain::source::checks::{allowed, assert_same, canon, moved, MOVES};
    use crate::domain::source::record::{
        sec_cik_entity, sec_filing_key, ted_procedure_key, SourceClass, SourceStamp, Trust,
    };
    use crate::domain::source::sec_records::{filing_record, FilingTime};
    use crate::domain::source::ted::{day_span, decode_page, notice_record, query_key};
    use crate::domain::source::testkit::{copy_of, coverage, edited, rec, Src};

    const CASES_PATH: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/sources/eval/cases.json"
    );
    const EVAL_SCHEMA: &str = "source_eval/1";
    const REGEN_ENV: &str = "TENGU_REGEN_SOURCE_EVAL";
    const CASE_IDS: [&str; 12] = [
        "01_fresh_filing",
        "02_amendment_after_t",
        "03_ted_change_notice_after_t",
        "04_duplicate_notice_two_pages",
        "05_syndicated_copy",
        "06_stale_coverage",
        "07_parse_failure",
        "08_purged_raw",
        "09_expired_tender",
        "10_conflicting_values",
        "11_in_place_edit",
        "12_no_evidence_hold",
    ];

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    // ── The cases, built from the captures (regeneration only) ──────────

    const CIK_APPLE: &str = "0000320193";
    const CIK_TESLA: &str = "0001318605";
    const K8: &str = "0000320193-26-000018";
    const K8A: &str = "0001140361-26-035325";
    /// The soe row's TED query (`sandboxes/soe`).
    const TED_QUERY: &str = "classification-cpv IN (72000000 48000000) AND publication-date >= {from} AND publication-date <= {to}";
    const TED_PROCEDURE: &str = "f08aa593-61f7-418e-90ec-043a09cc23b1";
    const TERMS: &str = "synthetic eval terms";

    fn t(s: &str) -> i64 {
        parse_time(s).unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    fn capture(path: &str) -> String {
        let full = format!("{}/tests/fixtures/{path}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&full).unwrap_or_else(|e| panic!("{full}: {e}"))
    }

    fn stamp(source_id: &str, class: SourceClass, jurisdiction: &str) -> SourceStamp {
        SourceStamp {
            source_id: source_id.into(),
            source_class: class,
            trust: Trust::Primary,
            jurisdiction: jurisdiction.into(),
            language: "en".into(),
            license_or_terms: TERMS.into(),
            terms_sha256: sha256_hex("synthetic eval terms page"),
        }
    }

    fn policy(revision: Revision) -> PolicyRow {
        PolicyRow {
            revision,
            listing_max_age_ms: None,
        }
    }

    /// Apple filing `accession` as `sec_records` builds it from the captured
    /// submissions and index page, read at `observed` (parsed 1 s later).
    fn sec_filing(accession: &str, observed: &str) -> SourceRecord {
        let subs = capture("sec/CIK0000320193.json");
        let v: Value = serde_json::from_str(&subs).unwrap();
        let filing = submissions(&v)
            .unwrap()
            .recent
            .into_iter()
            .find(|f| f.accession == accession)
            .unwrap_or_else(|| panic!("{accession} is not in the capture"));
        let index = capture(&format!("sec/{accession}-index.htm"));
        let accepted = index_acceptance(&index, accession).unwrap().published_ms;
        let at = t(observed);
        filing_record(
            &stamp("sec_edgar", SourceClass::CompanyPrimary, "US"),
            CIK_APPLE,
            &filing,
            FilingTime::Accepted(accepted),
            vec![sha256_hex(&subs), sha256_hex(&index)],
            at,
            at + 1_000,
        )
        .unwrap()
    }

    fn sec_cov(cik: &str, fetched: &str, from: &str) -> Coverage {
        Coverage {
            source_id: "sec_edgar".into(),
            query_key: format!("cik:{cik}"),
            fetched_ms: t(fetched),
            from_ms: t(from),
            to_ms: t(fetched),
            complete: true,
            error_class: None,
        }
    }

    /// Every readable notice of the captured TED reply `file`, read at
    /// `observed`, in reply order, each linked against `prior` and the ones
    /// before it (as a fetch builds a page).
    fn ted_page(file: &str, prior: &[SourceRecord], observed: &str) -> Vec<SourceRecord> {
        ted_reply(&capture(&format!("ted/{file}")), prior, observed)
    }

    /// A later page re-serving `publications` of the captured reply `file`
    /// (TED pages shift while a day is read): a reply built from the capture,
    /// in its own bytes.
    fn ted_page_again(
        file: &str,
        publications: &[&str],
        prior: &[SourceRecord],
        observed: &str,
    ) -> Vec<SourceRecord> {
        let v: Value = serde_json::from_str(&capture(&format!("ted/{file}"))).unwrap();
        let notices: Vec<Value> = v["notices"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|n| publications.contains(&n["publication-number"].as_str().unwrap_or("")))
            .cloned()
            .collect();
        let body =
            serde_json::json!({"notices": notices, "totalNoticeCount": v["totalNoticeCount"]});
        ted_reply(&body.to_string(), prior, observed)
    }

    /// The records of TED reply `text`, read at `observed`.
    fn ted_reply(text: &str, prior: &[SourceRecord], observed: &str) -> Vec<SourceRecord> {
        let page = decode_page(&serde_json::from_str(text).unwrap()).unwrap();
        let sha = sha256_hex(text);
        let at = t(observed);
        let stamp = stamp("ted_search", SourceClass::LawRegulator, "EU");
        let mut seen: Vec<SourceRecord> = prior.to_vec();
        let mut out = Vec::new();
        for read in page.notices.into_iter().flatten() {
            let r = notice_record(&stamp, &read, &seen, sha.clone(), at, at + 1_000);
            seen.push(r.clone());
            out.push(r);
        }
        out
    }

    /// Notice `publication` of `file`, read at `observed`, linked against `prior`.
    fn ted_notice(
        file: &str,
        publication: &str,
        prior: &[SourceRecord],
        observed: &str,
    ) -> SourceRecord {
        ted_page(file, prior, observed)
            .into_iter()
            .find(|r| r.native_id == publication)
            .unwrap_or_else(|| panic!("{publication} is not in {file}"))
    }

    /// The soe query's coverage of publication `day`, fetched at `fetched`;
    /// `error` = an incomplete fetch's class.
    fn ted_cov(day: &str, fetched: &str, error: Option<&str>) -> Coverage {
        let (from_ms, to_ms) = day_span(NaiveDate::parse_from_str(day, "%Y-%m-%d").unwrap());
        Coverage {
            source_id: "ted_search".into(),
            query_key: query_key(TED_QUERY),
            fetched_ms: t(fetched),
            from_ms,
            to_ms,
            complete: error.is_none(),
            error_class: error.map(String::from),
        }
    }

    /// A case before its packet: `expect` written by hand.
    #[allow(clippy::too_many_arguments)]
    fn case(
        id: &str,
        at: &str,
        mode: AsOfMode,
        query: AsOfQuery,
        about: &str,
        inputs: &str,
        synthetic: bool,
        policies: &[(&str, PolicyRow)],
        records: Vec<SourceRecord>,
        coverage: Vec<Coverage>,
        purges: Vec<Purge>,
        expect: Expect,
    ) -> Case {
        for r in &records {
            assert_eq!(r.validate(), Ok(()), "{id}: {}", r.record_id);
        }
        let mut c = Case {
            id: id.into(),
            date: fmt_time(t(at)),
            about: about.into(),
            inputs: inputs.into(),
            synthetic,
            at_ms: t(at),
            mode,
            query,
            policies: policies.iter().map(|(s, p)| (s.to_string(), *p)).collect(),
            records,
            coverage,
            purges,
            expect,
            packet: Value::Null,
        };
        let packet = c.build(&c.world());
        let got = summary(&packet, &c.records);
        assert_eq!(
            got, c.expect,
            "{id}: the packet does not show what the case says"
        );
        c.packet = serde_json::to_value(&packet).unwrap();
        c
    }

    fn entity(e: &str) -> AsOfQuery {
        AsOfQuery {
            entity: Some(e.into()),
            ..AsOfQuery::default()
        }
    }

    fn source(s: &str) -> AsOfQuery {
        AsOfQuery {
            source: Some(s.into()),
            ..AsOfQuery::default()
        }
    }

    fn event(k: &str) -> AsOfQuery {
        AsOfQuery {
            event_key: Some(k.into()),
            ..AsOfQuery::default()
        }
    }

    /// The 12 cases (module table), from the captures and the testkit.
    fn build_cases() -> Vec<Case> {
        use AsOfMode::{Captured, Knowable};
        let sec = [("sec_edgar", policy(Revision::Immutable))];
        let ted = [("ted_search", policy(Revision::Immutable))];
        let apple = sec_cik_entity(CIK_APPLE);
        let k8_event = sec_filing_key(K8);
        let proc_event = ted_procedure_key(TED_PROCEDURE);
        let sec_fresh = |extra: &[&str]| {
            let mut f = strings(&["sec_edgar failed=0 parse_failures=0 partial_parses=0 purges=0"]);
            f.extend(strings(extra));
            f
        };
        let q = query_key(TED_QUERY);
        let k8 = sec_filing(K8, "2026-07-30T20:35:00Z");
        let k8_cov = sec_cov(CIK_APPLE, "2026-07-30T20:35:00Z", "2026-07-01T00:00:00Z");
        let k8_cov_line =
            format!("sec_edgar cik:{CIK_APPLE} last_fetch=2026-07-30T20:35:00Z gaps=0");
        let k8_confirmed = format!("{k8_event} confirmed origins=sec_edgar");
        let k8_fact = format!("{K8} read=2026-07-30T20:35:00Z");

        let mut cases = Vec::new();

        cases.push(case(
        "01_fresh_filing",
        "2026-07-30T21:00:00Z",
        Captured,
        entity(&apple),
        "A filing read 5 min after its acceptance is one fact, confirmed by its primary source.",
        "sec/CIK0000320193.json + sec/0000320193-26-000018-index.htm",
        false,
        &sec,
        vec![k8.clone()],
        vec![k8_cov.clone()],
        Vec::new(),
        Expect {
            status: "ok".into(),
            facts: vec![k8_fact.clone()],
            events: vec![k8_confirmed.clone()],
            freshness: sec_fresh(&[k8_cov_line.as_str()]),
            ..Expect::default()
        },
    ));

        let k8a = sec_filing(K8A, "2026-09-01T20:40:00Z");
        cases.push(case(
        "02_amendment_after_t",
        "2026-08-15T00:00:00Z",
        Knowable,
        entity(&apple),
        "An 8-K/A accepted after t is not in the knowable view at t: only the 8-K it amends.",
        "sec/CIK0000320193.json + the index pages of 0000320193-26-000018 and 0001140361-26-035325",
        false,
        &sec,
        vec![k8.clone(), k8a],
        vec![
            k8_cov.clone(),
            sec_cov(CIK_APPLE, "2026-09-01T20:40:00Z", "2026-07-30T20:35:00Z"),
        ],
        Vec::new(),
        Expect {
            status: "ok".into(),
            facts: vec![k8_fact.clone()],
            events: vec![k8_confirmed.clone()],
            freshness: sec_fresh(&[k8_cov_line.as_str()]),
            ..Expect::default()
        },
    ));

        let original = ted_notice(
            "search_change_notice.json",
            "657981-2026",
            &[],
            "2026-10-02T08:00:00Z",
        );
        let change = ted_notice(
            "search_change_notice.json",
            "674231-2026",
            std::slice::from_ref(&original),
            "2026-10-02T08:00:00Z",
        );
        assert_eq!(
            change.supersedes.as_deref(),
            Some(original.record_id.as_str())
        );
        let ted_fresh = |extra: &[String]| {
            let mut f =
                strings(&["ted_search failed=0 parse_failures=0 partial_parses=0 purges=0"]);
            f.extend(extra.iter().cloned());
            f
        };
        cases.push(case(
        "03_ted_change_notice_after_t",
        "2026-09-30T00:00:00Z",
        Knowable,
        source("ted_search"),
        "A change notice published after t (both notices read later) is not knowable at t: the original stands, nothing is superseded.",
        "ted/search_change_notice.json",
        false,
        &ted,
        vec![original.clone(), change.clone()],
        Vec::new(),
        Vec::new(),
        Expect {
            status: "ok".into(),
            facts: strings(&["657981-2026 read=2026-10-02T08:00:00Z"]),
            events: vec![format!("{proc_event} confirmed origins=ted_search")],
            freshness: ted_fresh(&[]),
            ..Expect::default()
        },
    ));

        let page = ted_page("search_page_1.json", &[], "2026-10-02T06:00:00Z");
        let dup = ted_page_again(
            "search_page_1.json",
            &["674231-2026"],
            &page,
            "2026-10-02T06:01:00Z",
        );
        assert!(
            dup.len() == 1
                && page.iter().any(|r| r.record_id == dup[0].record_id)
                && !page.iter().any(|r| r.snapshots == dup[0].snapshots),
            "page 2 re-serves 674231-2026 as page 1 read it, in other bytes"
        );
        let mut dup_records = page.clone();
        dup_records.extend(dup);
        let page_events: Vec<String> = {
            let mut e: Vec<String> = page
                .iter()
                .map(|r| format!("{} confirmed origins=ted_search", r.event_key))
                .collect();
            e.sort();
            e
        };
        let page_facts = |read: &str| -> Vec<String> {
            let mut f: Vec<String> = page
                .iter()
                .map(|r| format!("{} read={read}", r.native_id))
                .collect();
            f.sort();
            f
        };
        let page_cov = ted_cov("2026-10-01", "2026-10-02T06:00:00Z", None);
        cases.push(case(
        "04_duplicate_notice_two_pages",
        "2026-10-02T12:00:00Z",
        Captured,
        source("ted_search"),
        "A notice served on two pages of one day's search is one record: listed once, cited with its first read.",
        "ted/search_page_1.json; page 2 re-serving 674231-2026 built from it",
        false,
        &ted,
        dup_records,
        vec![page_cov.clone()],
        Vec::new(),
        Expect {
            status: "ok".into(),
            facts: page_facts("2026-10-02T06:00:00Z"),
            events: page_events.clone(),
            freshness: ted_fresh(&[format!(
                "ted_search {q} last_fetch=2026-10-02T06:00:00Z gaps=0"
            )]),
            ..Expect::default()
        },
    ));

        // An outlet's own report of the 8-K (no origin), copied twice by a wire.
        let mut daily = k8.clone();
        daily.source_id = Src::DAILY.id.into();
        daily.source_class = Src::DAILY.class;
        daily.trust = Src::DAILY.trust;
        daily.native_id = "daily-2026-07-30-0001".into();
        daily.url = "https://example.org/trade_daily/daily-2026-07-30-0001".into();
        daily.published_ms = t("2026-07-30T20:50:00Z");
        daily.valid_from_ms = daily.published_ms;
        daily.observed_ms = t("2026-07-30T20:55:00Z");
        daily.parsed_ms = daily.observed_ms + 1_000;
        daily.snapshots = vec![sha256_hex("trade_daily daily-2026-07-30-0001 first read")];
        let daily = daily.with_identity();
        let wire1 = copy_of(
            &daily,
            Src::WIRE,
            "wire-2026-07-30-0001",
            t("2026-07-30T21:00:00Z"),
            t("2026-07-30T21:05:00Z"),
        );
        let wire2 = copy_of(
            &daily,
            Src::WIRE,
            "wire-2026-07-30-0002",
            t("2026-07-30T21:10:00Z"),
            t("2026-07-30T21:15:00Z"),
        );
        cases.push(case(
        "05_syndicated_copy",
        "2026-07-31T00:00:00Z",
        Captured,
        event(&k8_event),
        "An outlet's report of a filing and two wire copies of it are one origin: single, never corroborated.",
        "synthetic (testkit trade_daily, news_wire) shaped as the 8-K 0000320193-26-000018",
        true,
        &[
            (Src::DAILY.id, Src::DAILY.policy().into()),
            (Src::WIRE.id, Src::WIRE.policy().into()),
        ],
        vec![daily, wire1, wire2],
        Vec::new(),
        Vec::new(),
        Expect {
            status: "ok".into(),
            facts: strings(&[
                "daily-2026-07-30-0001 read=2026-07-30T20:55:00Z",
                "wire-2026-07-30-0001 read=2026-07-30T21:05:00Z",
                "wire-2026-07-30-0002 read=2026-07-30T21:15:00Z",
            ]),
            events: vec![format!("{k8_event} single origins=trade_daily")],
            freshness: strings(&[
                "news_wire failed=0 parse_failures=0 partial_parses=0 purges=0",
                "trade_daily failed=0 parse_failures=0 partial_parses=0 purges=0",
            ]),
            ..Expect::default()
        },
    ));

        cases.push(case(
        "06_stale_coverage",
        "2026-10-06T00:00:00Z",
        Captured,
        source("ted_search"),
        "The newest complete fetch is days old, one publication day was never covered and two later fetches failed: the facts stand, the packet is partial.",
        "ted/search_page_1.json; coverage rows synthetic",
        false,
        &ted,
        page.clone(),
        vec![
            ted_cov("2026-09-29", "2026-09-30T06:00:00Z", None),
            page_cov.clone(),
            ted_cov("2026-10-02", "2026-10-03T06:00:00Z", Some("transient")),
            ted_cov("2026-10-03", "2026-10-04T06:00:00Z", Some("rate_limited")),
        ],
        Vec::new(),
        Expect {
            status: "partial".into(),
            facts: page_facts("2026-10-02T06:00:00Z"),
            events: page_events.clone(),
            freshness: strings(&[
                "ted_search failed=2 parse_failures=0 partial_parses=0 purges=0",
                format!("ted_search {q} last_fetch=2026-10-02T06:00:00Z gaps=1").as_str(),
            ]),
            ..Expect::default()
        },
    ));

        let bad = ted_page("search_bad_notice.json", &[], "2026-10-02T06:00:00Z");
        cases.push(case(
        "07_parse_failure",
        "2026-10-02T12:00:00Z",
        Captured,
        source("ted_search"),
        "A notice without a type is unparsed, one with an unreadable date is partial (its read time as publication), one without a publication number has no record: visible, never dropped silently.",
        "ted/search_bad_notice.json (hand-edited capture)",
        false,
        &ted,
        bad,
        vec![ted_cov("2026-10-01", "2026-10-02T06:00:00Z", Some("decode"))],
        Vec::new(),
        Expect {
            status: "partial".into(),
            facts: strings(&[
                "674429-2026 read=2026-10-02T06:00:00Z",
                "674677-2026 read=2026-10-02T06:00:00Z",
            ]),
            unparsed: strings(&["674295-2026 read=2026-10-02T06:00:00Z"]),
            // The planning notice is its own event; the unparsed one grades `none`.
            events: strings(&[
                "ted:notice:674677-2026 confirmed origins=ted_search",
                "ted:procedure:78d76088-524a-4903-a15c-b1c3408f47a9 none origins=-",
                "ted:procedure:7df1ac9e-fad5-4c0c-9e16-f71db26503a5 confirmed origins=ted_search",
            ]),
            freshness: strings(&[
                "ted_search failed=1 parse_failures=1 partial_parses=1 purges=0",
            ]),
            ..Expect::default()
        },
    ));

        cases.push(case(
        "08_purged_raw",
        "2026-08-02T00:00:00Z",
        Captured,
        entity(&apple),
        "A raw-retention purge deletes bodies, never the record: the fact keeps its snapshot hashes and the tombstone shows.",
        "sec/CIK0000320193.json + sec/0000320193-26-000018-index.htm; the tombstone synthetic",
        false,
        &sec,
        vec![k8.clone()],
        vec![k8_cov.clone()],
        vec![Purge {
            source_id: "sec_edgar".into(),
            purged_ms: t("2026-08-01T00:00:00Z"),
            raw_before_ms: Some(t("2026-07-31T00:00:00Z")),
            records_before_ms: None,
            snapshots: 2,
            records: 0,
            reason: "retention: raw_retention_days = 1, record_retention_days = 0 (forever) (tengu sources purge)".into(),
        }],
        Expect {
            status: "ok".into(),
            facts: vec![k8_fact.clone()],
            events: vec![k8_confirmed.clone()],
            freshness: strings(&[
                "sec_edgar failed=0 parse_failures=0 partial_parses=0 purges=1",
                k8_cov_line.as_str(),
            ]),
            ..Expect::default()
        },
    ));

        let lone = ted_notice(
            "search_change_notice.json",
            "657981-2026",
            &[],
            "2026-09-25T06:00:00Z",
        );
        cases.push(case(
        "09_expired_tender",
        "2026-10-30T00:00:00Z",
        Captured,
        source("ted_search"),
        "A tender past its deadline (2026-10-29T09:00Z) is expired, not in force; the event still happened.",
        "ted/search_change_notice.json (657981-2026 only)",
        false,
        &ted,
        vec![lone],
        vec![ted_cov("2026-09-24", "2026-09-25T06:00:00Z", None)],
        Vec::new(),
        Expect {
            status: "ok".into(),
            expired: strings(&["657981-2026 read=2026-09-25T06:00:00Z"]),
            events: vec![format!("{proc_event} confirmed origins=ted_search")],
            freshness: ted_fresh(&[format!(
                "ted_search {q} last_fetch=2026-09-25T06:00:00Z gaps=0"
            )]),
            ..Expect::default()
        },
    ));

        let change_first = ted_notice(
            "search_page_1.json",
            "674231-2026",
            &[],
            "2026-10-02T06:00:00Z",
        );
        let original_later = ted_notice(
            "search_change_notice.json",
            "657981-2026",
            std::slice::from_ref(&change_first),
            "2026-10-02T08:00:00Z",
        );
        assert_eq!(change_first.supersedes, None);
        cases.push(case(
        "10_conflicting_values",
        "2026-10-03T00:00:00Z",
        Captured,
        event(&proc_event),
        "A change notice read before the notice it changes was stored carries no supersedes link: both stand and their deadlines conflict — shown, never resolved by guess.",
        "ted/search_page_1.json (674231-2026) + ted/search_change_notice.json (657981-2026, read later)",
        false,
        &ted,
        vec![change_first, original_later],
        vec![
            page_cov.clone(),
            ted_cov("2026-09-24", "2026-10-02T08:00:00Z", None),
        ],
        Vec::new(),
        Expect {
            status: "partial".into(),
            facts: strings(&[
                "657981-2026 read=2026-10-02T08:00:00Z",
                "674231-2026 read=2026-10-02T06:00:00Z",
            ]),
            events: vec![format!("{proc_event} confirmed origins=ted_search")],
            conflicts: vec![format!("{proc_event} deadline_ms lots=LOT-0001")],
            freshness: ted_fresh(&[format!(
                "ted_search {q} last_fetch=2026-10-02T08:00:00Z gaps=1"
            )]),
            ..Expect::default()
        },
    ));

        let listing = rec(
            Src::REPO,
            "repo-item-1",
            t("2026-09-30T12:00:00Z"),
            t("2026-10-01T00:00:00Z"),
            t("2026-10-01T00:00:01Z"),
        );
        let edit = edited(&listing, t("2026-10-02T00:00:00Z"), "release r1");
        cases.push(case(
        "11_in_place_edit",
        "2026-10-01T12:00:00Z",
        Knowable,
        entity("repo:owner:example"),
        "An in-place listing edited and re-read after t: knowable at t is the first read only — an edit counts from its read, never from the listing's publication.",
        "synthetic (testkit repo_listing)",
        true,
        &[(Src::REPO.id, Src::REPO.policy().into())],
        vec![listing, edit],
        vec![
            coverage(
                Src::REPO,
                "repo:owner:example",
                t("2026-10-01T00:00:00Z"),
                t("2026-09-24T00:00:00Z"),
                t("2026-10-01T00:00:00Z"),
                true,
            ),
            coverage(
                Src::REPO,
                "repo:owner:example",
                t("2026-10-02T00:00:00Z"),
                t("2026-10-01T00:00:00Z"),
                t("2026-10-02T00:00:00Z"),
                true,
            ),
        ],
        Vec::new(),
        Expect {
            status: "ok".into(),
            facts: strings(&["repo-item-1 read=2026-10-01T00:00:00Z"]),
            events: strings(&["repo:item:repo-item-1 confirmed origins=repo_listing"]),
            freshness: strings(&[
                "repo_listing failed=0 parse_failures=0 partial_parses=0 purges=0",
                "repo_listing repo:owner:example last_fetch=2026-10-01T00:00:00Z gaps=0",
            ]),
            ..Expect::default()
        },
    ));

        cases.push(case(
        "12_no_evidence_hold",
        "2026-07-31T00:00:00Z",
        Captured,
        entity(&sec_cik_entity(CIK_TESLA)),
        "The queried company was fetched and filed nothing in the window: no row, absent — no evidence means HOLD, never a zero.",
        "sec/CIK0000320193.json + sec/0000320193-26-000018-index.htm (another CIK's filing); coverage synthetic",
        false,
        &sec,
        vec![k8],
        vec![
            k8_cov,
            sec_cov(CIK_TESLA, "2026-07-30T21:00:00Z", "2026-07-01T00:00:00Z"),
        ],
        Vec::new(),
        Expect {
            status: "absent".into(),
            freshness: sec_fresh(&[
                k8_cov_line.as_str(),
                format!("sec_edgar cik:{CIK_TESLA} last_fetch=2026-07-30T21:00:00Z gaps=0").as_str(),
            ]),
            ..Expect::default()
        },
    ));
        cases
    }

    fn regenerate() {
        let set = EvalSet {
        schema: EVAL_SCHEMA.into(),
        about: "Source-level evaluation set (O2 exit): 12 dated cases replayed from this file alone \
                — src/domain/source/eval.rs. Records come from the captures under tests/fixtures/sec \
                and tests/fixtures/ted (public filings and notices; no contact field) or from synthetic \
                testkit sources (`synthetic`). Regenerate with TENGU_REGEN_SOURCE_EVAL=1."
            .into(),
        cases: build_cases(),
    };
        let text = serde_json::to_string_pretty(&set).unwrap() + "\n";
        // Atomic: the other test reads the file meanwhile.
        let path = std::path::Path::new(CASES_PATH);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text).unwrap();
        std::fs::rename(&tmp, path).unwrap();
    }

    fn load() -> EvalSet {
        let text = std::fs::read_to_string(CASES_PATH).unwrap_or_else(|e| {
            panic!("{CASES_PATH}: {e} — regenerate with {REGEN_ENV}=1 (module doc)")
        });
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{CASES_PATH}: {e}"))
    }

    #[test]
    fn eval_set_replays_without_lookahead() {
        if std::env::var_os(REGEN_ENV).is_some() {
            regenerate();
        }
        let set = load();
        assert_eq!(set.schema, EVAL_SCHEMA);
        let ids: Vec<&str> = set.cases.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, CASE_IDS);
        let mut modes = (0, 0);
        for c in &set.cases {
            assert_eq!(c.date, fmt_time(c.at_ms), "{}", c.id);
            // Stored records stay readable: well-formed, ids from their content.
            for r in &c.records {
                assert_eq!(r.validate(), Ok(()), "{}: {}", c.id, r.record_id);
                let again = r.clone().with_identity();
                assert_eq!(
                    (&again.record_id, &again.content_hash),
                    (&r.record_id, &r.content_hash),
                    "{}: stored id no longer derives from its content",
                    c.id
                );
            }
            let w = c.world();
            let packet = c.build(&w);
            let base = canon(&packet);
            assert_same(
                &base,
                &canonical_json(&c.packet),
                &format!("{} replay at {}", c.id, c.date),
            );
            assert_eq!(summary(&packet, &c.records), c.expect, "{}", c.id);
            // Nothing after t moves the packet at t.
            for how in MOVES.into_iter().filter(|m| allowed(c.mode, *m)) {
                let m = moved(&w, c.at_ms, c.mode, how);
                assert_same(
                    &canon(&c.build(&m)),
                    &base,
                    &format!("{} {:?} {how:?} after {}", c.id, c.mode, c.date),
                );
            }
            match c.mode {
                AsOfMode::Captured => modes.0 += 1,
                AsOfMode::Knowable => modes.1 += 1,
            }
        }
        assert!(modes.0 > 0 && modes.1 > 0, "both modes replayed: {modes:?}");
    }

    #[test]
    fn no_case_counts_a_copy_as_confirmation() {
        let set = load();
        let mut copies_beside_origin = 0;
        for c in &set.cases {
            let w = c.world();
            let view = as_of(&w.input(), c.at_ms, c.mode);
            let packet = c.build(&w);
            let origin_visible = |r: &SourceRecord| {
                r.origin.as_ref().is_some_and(|o| {
                    view.visible.values().any(|v| {
                        v.source_id == o.source_id && Some(&v.native_id) == o.native_id.as_ref()
                    })
                })
            };
            let mut without = w.clone();
            without.records.retain(|r| !origin_visible(r));
            copies_beside_origin += w.records.len() - without.records.len();
            let alone = c.build(&without);
            for e in &packet.events {
                let a = alone
                    .events
                    .iter()
                    .find(|x| x.event_key == e.event_key)
                    .unwrap_or_else(|| panic!("{}: {} lost with its copies", c.id, e.event_key));
                assert_eq!(
                    (e.confidence, &e.origins),
                    (a.confidence, &a.origins),
                    "{}: copies changed {}",
                    c.id,
                    e.event_key
                );
                // A copy's own source is never an origin unless it also
                // reported on its own.
                for id in &e.records {
                    let r = view.visible[id.as_str()];
                    if r.origin.is_none() {
                        continue;
                    }
                    let own = e.records.iter().any(|x| {
                        let y = view.visible[x.as_str()];
                        y.origin.is_none() && y.source_id == r.source_id
                    });
                    assert!(
                        own || !e.origins.contains(&r.source_id),
                        "{}: copy {} counted as origin {}",
                        c.id,
                        r.record_id,
                        r.source_id
                    );
                }
            }
        }
        assert!(
            copies_beside_origin >= 2,
            "no case holds a copy beside its origin"
        );
    }
}
