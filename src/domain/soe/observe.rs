//! Observe (O3 step 1; PRD § 8 steps 1–2; roadmap O3 "read only O2 evidence
//! packets"): the cycle's only fact input is one O2 evidence packet
//! (`domain::source::EvidencePacket`) at the decision time. Pure: the packet
//! in, what the later stages read out — never a store, a fetch or a model.
//!
//! | Output | Rule |
//! |---|---|
//! | [`EvidenceIndex`] | one [`EvidenceRef`] per record the packet lists (facts, pending, expired, withdrawn, unparsed): event, source, class, trust, state, url (the packet's citation), times, and its event's confidence and independent `confirmations` = `EventRow.origins` |
//! | [`cited_views`] → `gates::CitedRecord` | one view per in-force or expired record: `knowable_at` = its `visible_ms` (the packet's clock), `valid_until` = `valid_until_ms`, `contradicted_at` = the earliest time a conflict naming it became visible (the latest `visible_ms` among that conflict's records); pending, withdrawn and unparsed records get no view — they support nothing |
//!
//! | View kind | When |
//! |---|---|
//! | `TRIGGER` | `trust = trigger_only`; a rule issue flags it; or a non-demand record whose event is only `single` / `trigger_only` (PRD § 5.1: news is a trigger until a primary source or a second independent origin confirms it) |
//! | `DEMAND` | `customer_demand` (the gates count its events: one post is not demand) |
//! | `FACT` | else — its event is `confirmed` (a primary record) or `corroborated` (≥ 2 independent origins) |
//!
//! Copies count once (C6): the origin rule — a syndicated copy speaks for
//! its `origin` — lives only in `domain/source/asof.rs`; this module reads
//! its result (`EventRow.origins`), never re-counts.

// Consumers land with the cycle (application `run_cycle`) and the tools.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use super::gates::{CitedKind, CitedRecord};
use crate::domain::lineage::value::Time;
use crate::domain::source::{Confidence, EvidencePacket, FactRow, RecordState, SourceClass, Trust};

/// One record the packet lists (module table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EvidenceRef {
    pub record_id: String,
    pub event_key: String,
    pub source_id: String,
    /// None for a withdrawn / unparsed row (the packet keeps no class).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_class: Option<SourceClass>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trust: Option<Trust>,
    pub state: RecordState,
    /// The packet's citation url; empty when it names none.
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published_ms: Option<i64>,
    pub observed_ms: i64,
    pub visible_ms: i64,
    /// The source a syndicated copy speaks for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// Its event's confidence at the packet's time.
    pub confidence: Confidence,
    /// Its event's independent origins (copies of one item count once).
    pub confirmations: usize,
}

/// Every record of one packet by id (module table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EvidenceIndex {
    pub as_of_ms: i64,
    pub refs: BTreeMap<String, EvidenceRef>,
}

impl EvidenceIndex {
    pub fn of(packet: &EvidencePacket) -> EvidenceIndex {
        let urls: BTreeMap<&str, &str> = packet
            .citations
            .iter()
            .map(|c| (c.record_id.as_str(), c.url.as_str()))
            .collect();
        let events: BTreeMap<&str, (Confidence, usize)> = packet
            .events
            .iter()
            .map(|e| (e.event_key.as_str(), (e.confidence, e.origins.len())))
            .collect();
        let event = |key: &str| events.get(key).copied().unwrap_or((Confidence::None, 0));
        let url = |id: &str| urls.get(id).map(|u| u.to_string()).unwrap_or_default();
        let mut refs = BTreeMap::new();
        let typed = [
            (RecordState::InForce, &packet.facts),
            (RecordState::Pending, &packet.pending),
            (RecordState::Expired, &packet.expired),
        ];
        for (state, rows) in typed {
            for r in rows {
                let (confidence, confirmations) = event(&r.event_key);
                refs.insert(
                    r.record_id.clone(),
                    EvidenceRef {
                        record_id: r.record_id.clone(),
                        event_key: r.event_key.clone(),
                        source_id: r.source_id.clone(),
                        source_class: Some(r.source_class),
                        trust: Some(r.trust),
                        state,
                        url: url(&r.record_id),
                        published_ms: Some(r.published_ms),
                        observed_ms: r.observed_ms,
                        visible_ms: r.visible_ms,
                        origin: r.origin.as_ref().map(|o| o.source_id.clone()),
                        confidence,
                        confirmations,
                    },
                );
            }
        }
        let untyped = packet
            .withdrawn
            .iter()
            .map(|w| {
                (
                    RecordState::Withdrawn,
                    &w.record_id,
                    &w.event_key,
                    &w.source_id,
                    w.observed_ms,
                    w.visible_ms,
                )
            })
            .chain(packet.unparsed.iter().map(|u| {
                (
                    RecordState::Unparsed,
                    &u.record_id,
                    &u.event_key,
                    &u.source_id,
                    u.observed_ms,
                    u.visible_ms,
                )
            }));
        for (state, id, event_key, source_id, observed_ms, visible_ms) in untyped {
            let (confidence, confirmations) = event(event_key);
            refs.insert(
                id.clone(),
                EvidenceRef {
                    record_id: id.clone(),
                    event_key: event_key.clone(),
                    source_id: source_id.clone(),
                    source_class: None,
                    trust: None,
                    state,
                    url: url(id),
                    published_ms: None,
                    observed_ms,
                    visible_ms,
                    origin: None,
                    confidence,
                    confirmations,
                },
            );
        }
        EvidenceIndex {
            as_of_ms: packet.as_of_ms,
            refs,
        }
    }

    pub fn get(&self, record_id: &str) -> Option<&EvidenceRef> {
        self.refs.get(record_id)
    }

    pub fn contains(&self, record_id: &str) -> bool {
        self.refs.contains_key(record_id)
    }
}

/// The kind of one typed row (module table: view kind).
fn kind_of(
    r: &FactRow,
    flagged: &BTreeSet<&str>,
    events: &BTreeMap<&str, Confidence>,
) -> CitedKind {
    if r.trust == Trust::TriggerOnly || flagged.contains(r.record_id.as_str()) {
        return CitedKind::Trigger;
    }
    if r.source_class == SourceClass::CustomerDemand {
        return CitedKind::Demand;
    }
    match events.get(r.event_key.as_str()) {
        Some(Confidence::Confirmed | Confidence::Corroborated) => CitedKind::Fact,
        _ => CitedKind::Trigger,
    }
}

/// Module table: the `CitedRecord` views of `packet`'s in-force and expired
/// records, by record id.
pub fn cited_views(packet: &EvidencePacket) -> Vec<CitedRecord> {
    let flagged: BTreeSet<&str> = packet.issues.iter().map(|i| i.record_id.as_str()).collect();
    let events: BTreeMap<&str, Confidence> = packet
        .events
        .iter()
        .map(|e| (e.event_key.as_str(), e.confidence))
        .collect();
    let rows: Vec<&FactRow> = packet.facts.iter().chain(&packet.expired).collect();
    let visible: BTreeMap<&str, i64> = packet
        .facts
        .iter()
        .chain(&packet.pending)
        .chain(&packet.expired)
        .map(|r| (r.record_id.as_str(), r.visible_ms))
        .collect();
    // A conflict is knowable once every record in it is.
    let mut contradicted: BTreeMap<&str, i64> = BTreeMap::new();
    for c in &packet.conflicts {
        let Some(at) = c
            .record_ids
            .iter()
            .map(|id| visible.get(id.as_str()).copied())
            .collect::<Option<Vec<i64>>>()
            .and_then(|v| v.into_iter().max())
        else {
            continue;
        };
        for id in &c.record_ids {
            let e = contradicted.entry(id.as_str()).or_insert(at);
            *e = (*e).min(at);
        }
    }
    let mut out: Vec<CitedRecord> = rows
        .into_iter()
        .map(|r| CitedRecord {
            record_id: r.record_id.clone(),
            event_key: r.event_key.clone(),
            kind: kind_of(r, &flagged, &events),
            knowable_at: Time::At(r.visible_ms),
            valid_until: r.valid_until_ms.map(Time::At),
            contradicted_at: contradicted.get(r.record_id.as_str()).map(|t| Time::At(*t)),
        })
        .collect();
    out.sort_by(|a, b| a.record_id.cmp(&b.record_id));
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::domain::source::testkit::{copy_of, policies, rec, sec_mut, Src, H};
    use crate::domain::source::{AsOfInput, AsOfMode, AsOfQuery, SourceRecord};

    /// 2026-10-05T12:00:00Z — the decision time of the O3 tests.
    pub(crate) fn t_ms() -> i64 {
        "2026-10-05T12:00:00Z"
            .parse::<Time>()
            .unwrap()
            .earliest()
            .unwrap()
    }

    /// The packet of `records` at `t` (knowable clock, every test source in
    /// the registry).
    pub(crate) fn packet(records: &[SourceRecord], t: i64) -> EvidencePacket {
        let policies = policies(&[
            Src::SEC,
            Src::TED,
            Src::WIRE,
            Src::DAILY,
            Src::SOCIAL,
            Src::FORUM,
            Src::REPO,
        ]);
        let input = AsOfInput {
            records,
            coverage: &[],
            purges: &[],
            policies: &policies,
        };
        EvidencePacket::build(&input, t, AsOfMode::Knowable, &AsOfQuery::default())
    }

    /// A filing published `days_before` the decision (read 5 min later).
    pub(crate) fn filing(native: &str, days_before: i64) -> SourceRecord {
        let p = t_ms() - days_before * 24 * H;
        rec(Src::SEC, native, p, p + 300_000, p + 360_000)
    }

    /// An outlet's own report of `of`'s event (no origin: independent).
    pub(crate) fn report(of: &SourceRecord, src: Src, native: &str, at: i64) -> SourceRecord {
        let mut r = rec(src, native, at, at + 60_000, at + 120_000);
        r.event_key = of.event_key.clone();
        r.with_identity()
    }

    fn view<'a>(views: &'a [CitedRecord], id: &str) -> &'a CitedRecord {
        views.iter().find(|v| v.record_id == id).unwrap()
    }

    #[test]
    fn copies_of_one_source_are_one_confirmation() {
        // Two articles copy one filing the packet does not hold: one origin.
        let f = filing("0000000001-26-000001", 3);
        let at = t_ms() - 2 * 24 * H;
        let wire = copy_of(&f, Src::WIRE, "w-1", at, at + 60_000);
        let daily = copy_of(&f, Src::DAILY, "d-1", at, at + 60_000);
        let pk = packet(&[wire.clone(), daily.clone()], t_ms());
        let idx = EvidenceIndex::of(&pk);
        for r in [&wire, &daily] {
            let e = idx.get(&r.record_id).unwrap();
            assert_eq!(
                (e.confirmations, e.confidence, e.origin.as_deref()),
                (1, Confidence::Single, Some("sec_edgar"))
            );
            assert_eq!(e.url, r.url);
        }
        let views = cited_views(&pk);
        assert_eq!(views.len(), 2);
        assert!(views.iter().all(|v| v.kind == CitedKind::Trigger));
        assert!(views.iter().all(|v| v.event_key == f.event_key));

        // An outlet reporting the same event on its own: two origins — a fact.
        let own = report(&f, Src::DAILY, "d-2", at);
        let pk = packet(&[wire.clone(), daily.clone(), own.clone()], t_ms());
        let idx = EvidenceIndex::of(&pk);
        assert_eq!(idx.get(&own.record_id).unwrap().confirmations, 2);
        assert_eq!(
            idx.get(&wire.record_id).unwrap().confidence,
            Confidence::Corroborated
        );
        assert!(cited_views(&pk).iter().all(|v| v.kind == CitedKind::Fact));

        // The filing itself (primary): confirmed, still one origin.
        let pk = packet(&[f.clone(), wire.clone(), daily.clone()], t_ms());
        let idx = EvidenceIndex::of(&pk);
        let e = idx.get(&f.record_id).unwrap();
        assert_eq!((e.confidence, e.confirmations), (Confidence::Confirmed, 1));
        assert!(cited_views(&pk).iter().all(|v| v.kind == CitedKind::Fact));
    }

    #[test]
    fn views_carry_the_packet_clock_kind_and_contradiction() {
        let f = filing("0000000001-26-000002", 4);
        // An independent outlet disagrees on the form, two days later.
        let at = t_ms() - 2 * 24 * H;
        let mut other = report(&f, Src::DAILY, "d-9", at);
        sec_mut(&mut other).form = "10-Q".into();
        let other = other.with_identity();
        let post = rec(Src::FORUM, "p-1", at, at + 60_000, at + 120_000);
        let social = rec(Src::SOCIAL, "s-1", at, at + 60_000, at + 120_000);
        // A filing published after the decision is not in the packet.
        let later = filing("0000000001-26-000003", -1);
        let pk = packet(
            &[
                f.clone(),
                other.clone(),
                post.clone(),
                social.clone(),
                later.clone(),
            ],
            t_ms(),
        );
        let views = cited_views(&pk);
        assert_eq!(views.len(), 4);
        assert!(views.iter().all(|v| v.record_id != later.record_id));
        let vf = view(&views, &f.record_id);
        assert_eq!(vf.kind, CitedKind::Fact);
        // Immutable source: knowable from publication.
        assert_eq!(vf.knowable_at, Time::At(f.published_ms));
        // The conflict became visible with the later record.
        let seen = Time::At(other.published_ms.max(other.observed_ms));
        assert_eq!(vf.contradicted_at, Some(seen));
        assert_eq!(view(&views, &other.record_id).contradicted_at, Some(seen));
        assert_eq!(view(&views, &post.record_id).kind, CitedKind::Demand);
        assert_eq!(view(&views, &social.record_id).kind, CitedKind::Trigger);
        let idx = EvidenceIndex::of(&pk);
        assert_eq!(idx.as_of_ms, t_ms());
        assert!(!idx.contains(&later.record_id));
        assert_eq!(
            idx.get(&post.record_id).unwrap().source_class,
            Some(SourceClass::CustomerDemand)
        );
    }
}
