//! Per-class evidence rules (PRD §5.1) and what the as-of view needs from a
//! `[sources.registry]` row ([`SourcePolicy`], built by `config/sources.rs`).
//! Pure. A broken rule never drops a record: it becomes an [`Issue`] on it,
//! listed in the packet, and the record then confirms nothing.
//!
//! | Class (PRD §5.1) | Rule | Enforced |
//! |---|---|---|
//! | `law_regulator` | jurisdiction + effective date | jurisdiction an ISO 3166 code (`US`, `DEU`, `US-CA`) or `EU` ([`valid_jurisdiction`]), else issue `jurisdiction_missing` (a row's `jurisdiction` must pass at load); the effective date is `valid_from_ms`: a fact not yet in force at t is `pending`, never in force |
//! | `registry_marketplace` | listing freshness | the newest read of its source at or before t (the record's `observed_ms`, or a complete coverage fetch) no older than the row's `listing_max_age_days`, else issue `listing_stale`; without the knob `listing_max_age_missing` (required at load) |
//! | `customer_demand` | aggregate; one post is not demand | [`DemandStatus`]: `single_signal` below [`MIN_DEMAND_EVENTS`] distinct events, `aggregate` from it (copies of one post share an event) |
//! | `independent_reporting` | a trigger until primary evidence is found | never `primary` (`SourceClass::allows`, on the record and the row) |
//! | `social_inference` | never sufficient | `trigger_only` only (same) |
//! | any | — | a record of a source no registry row lists: `source_not_in_registry` |
//!
//! | Revision | Meaning | Knowable as of t (`asof.rs`) |
//! |---|---|---|
//! | `immutable` | a published item never changes; a correction is its own item with its own publication time (SEC accessions, TED notices) | from `published_ms`; a later read of new bytes under the same id is an edit, from its read |
//! | `in_place` | an item may change under the same id (a page, a repository) | from `max(published_ms, observed_ms)` |

#![allow(dead_code)] // consumers (store, CLI, tool) land with the next O2 steps

use serde::{Deserialize, Serialize};

use super::record::{SourceClass, SourceRecord};
use crate::domain::marketdata::fmt_time;

/// Distinct customer-demand events that make demand: one post is not
/// demand (PRD §5.1).
pub const MIN_DEMAND_EVENTS: usize = 2;

/// How a source revises what it published (module table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Revision {
    Immutable,
    InPlace,
}

impl Revision {
    pub fn as_str(self) -> &'static str {
        match self {
            Revision::Immutable => "immutable",
            Revision::InPlace => "in_place",
        }
    }
}

/// What the as-of view needs from one registry row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourcePolicy {
    pub revision: Revision,
    /// `registry_marketplace`: the oldest a listing read may be at t.
    pub listing_max_age_ms: Option<i64>,
}

/// What [`class_issues`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueCode {
    /// `law_regulator` without an ISO 3166 / `EU` jurisdiction.
    JurisdictionMissing,
    /// `registry_marketplace` read longer ago than its row allows.
    ListingStale,
    /// `registry_marketplace` whose row has no `listing_max_age_days`.
    ListingMaxAgeMissing,
    /// No registry row for the record's source.
    SourceNotInRegistry,
    /// A correction naming a record of another event (ignored).
    SupersedesOtherEvent,
    /// A correction naming a record not visible at t (nothing removed).
    SupersedesNotVisible,
}

impl IssueCode {
    pub fn as_str(self) -> &'static str {
        match self {
            IssueCode::JurisdictionMissing => "jurisdiction_missing",
            IssueCode::ListingStale => "listing_stale",
            IssueCode::ListingMaxAgeMissing => "listing_max_age_missing",
            IssueCode::SourceNotInRegistry => "source_not_in_registry",
            IssueCode::SupersedesOtherEvent => "supersedes_other_event",
            IssueCode::SupersedesNotVisible => "supersedes_not_visible",
        }
    }
}

/// One broken rule on one record. `detail` is ours, never source text.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Issue {
    pub record_id: String,
    pub code: IssueCode,
    pub detail: String,
}

impl Issue {
    pub fn new(record_id: &str, code: IssueCode, detail: impl Into<String>) -> Self {
        Self {
            record_id: record_id.to_string(),
            code,
            detail: detail.into(),
        }
    }
}

/// Customer demand behind a view (PRD §5.1: aggregate).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DemandStatus {
    /// No customer-demand fact.
    None,
    /// Fewer than [`MIN_DEMAND_EVENTS`] distinct events: a signal, not demand.
    SingleSignal,
    Aggregate,
}

impl DemandStatus {
    pub fn of(events: usize) -> Self {
        match events {
            0 => DemandStatus::None,
            n if n < MIN_DEMAND_EVENTS => DemandStatus::SingleSignal,
            _ => DemandStatus::Aggregate,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            DemandStatus::None => "none",
            DemandStatus::SingleSignal => "single_signal",
            DemandStatus::Aggregate => "aggregate",
        }
    }
}

/// An ISO 3166-1 code (`US`, `DEU`), an ISO 3166-2 subdivision (`US-CA`,
/// `DE-BY`) or `EU` — never a placeholder like `unknown` or `n/a`.
pub fn valid_jurisdiction(j: &str) -> bool {
    let letters = |s: &str, lens: &[usize]| {
        lens.contains(&s.len()) && s.bytes().all(|c| c.is_ascii_uppercase())
    };
    match j.split_once('-') {
        Some((country, sub)) => {
            letters(country, &[2])
                && (1..=3).contains(&sub.len())
                && sub
                    .bytes()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        }
        None => letters(j, &[2, 3]),
    }
}

/// The issues of one current record at `t` (module table). `policy` = its
/// source's registry row; `newest_read_ms` = the newest read of that
/// source at or before `t` (`None`: none).
pub fn class_issues(
    r: &SourceRecord,
    policy: Option<&SourcePolicy>,
    newest_read_ms: Option<i64>,
    t: i64,
) -> Vec<Issue> {
    let Some(policy) = policy else {
        return vec![Issue::new(
            &r.record_id,
            IssueCode::SourceNotInRegistry,
            format!("source `{}` has no [sources.registry] row", r.source_id),
        )];
    };
    let mut out = Vec::new();
    match r.source_class {
        SourceClass::LawRegulator if !valid_jurisdiction(&r.jurisdiction) => {
            out.push(Issue::new(
                &r.record_id,
                IssueCode::JurisdictionMissing,
                "a law / regulator fact needs its jurisdiction: an ISO 3166 code or EU",
            ));
        }
        SourceClass::RegistryMarketplace => match (policy.listing_max_age_ms, newest_read_ms) {
            (None, _) => out.push(Issue::new(
                &r.record_id,
                IssueCode::ListingMaxAgeMissing,
                "a registry / marketplace fact needs its row's listing_max_age_days",
            )),
            (Some(_), None) => out.push(Issue::new(
                &r.record_id,
                IssueCode::ListingStale,
                "no read of its source at or before t",
            )),
            (Some(max), Some(read)) if t - read > max => out.push(Issue::new(
                &r.record_id,
                IssueCode::ListingStale,
                format!(
                    "newest read {} is {} ms before t; the row allows {max} ms",
                    fmt_time(read),
                    t - read
                ),
            )),
            _ => {}
        },
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::source::testkit::{rec, Src, H, T0};

    const POLICY: SourcePolicy = SourcePolicy {
        revision: Revision::Immutable,
        listing_max_age_ms: Some(48 * H),
    };

    #[test]
    fn jurisdictions_are_codes() {
        for ok in ["US", "EU", "DEU", "US-CA", "DE-BY", "GB-ENG", "FR-75"] {
            assert!(valid_jurisdiction(ok), "{ok}");
        }
        for bad in [
            "", "us", "unknown", "N/A", "U", "USAA", "US-", "US-CALI", "-CA", "US CA", "U-CA",
        ] {
            assert!(!valid_jurisdiction(bad), "{bad}");
        }
    }

    /// U6: a law / regulator fact needs a real jurisdiction; registry
    /// listings go stale; an unregistered source never counts.
    #[test]
    fn class_rules_become_issues_never_drops() {
        let mut law = rec(Src::TED, "100-2026", T0, T0 + H, T0 + H);
        assert!(class_issues(&law, Some(&POLICY), Some(T0 + H), T0 + 2 * H).is_empty());
        law.jurisdiction = "unknown".into();
        let law = law.with_identity();
        let issues = class_issues(&law, Some(&POLICY), Some(T0 + H), T0 + 2 * H);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].code, IssueCode::JurisdictionMissing);
        assert_eq!(issues[0].record_id, law.record_id);

        let listing = rec(Src::REPO, "repo-1", T0, T0, T0);
        let at = |t, read| class_issues(&listing, Some(&POLICY), read, t);
        assert!(at(T0 + 48 * H, Some(T0)).is_empty());
        assert_eq!(
            at(T0 + 48 * H + 1, Some(T0))[0].code,
            IssueCode::ListingStale
        );
        assert_eq!(at(T0 + H, None)[0].code, IssueCode::ListingStale);
        let no_knob = SourcePolicy {
            listing_max_age_ms: None,
            ..POLICY
        };
        assert_eq!(
            class_issues(&listing, Some(&no_knob), Some(T0), T0)[0].code,
            IssueCode::ListingMaxAgeMissing
        );
        assert_eq!(
            class_issues(&listing, None, Some(T0), T0)[0].code,
            IssueCode::SourceNotInRegistry
        );
        // Other classes carry no class rule here.
        let filing = rec(Src::SEC, "0000000001-26-000001", T0, T0, T0);
        assert!(class_issues(&filing, Some(&POLICY), None, T0 + 1000 * H).is_empty());
    }

    #[test]
    fn one_post_is_not_demand() {
        assert_eq!(DemandStatus::of(0), DemandStatus::None);
        assert_eq!(DemandStatus::of(1), DemandStatus::SingleSignal);
        assert_eq!(DemandStatus::of(MIN_DEMAND_EVENTS), DemandStatus::Aggregate);
        assert_eq!(
            serde_json::to_string(&DemandStatus::SingleSignal).unwrap(),
            "\"single_signal\""
        );
    }
}
