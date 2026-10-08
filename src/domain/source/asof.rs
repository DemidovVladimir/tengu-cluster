//! As-of view (O2): what the source records said at an instant t — the
//! current version of each item, corrections applied, per-class rules
//! checked (`rules.rs`), events graded, disagreements listed. Pure: the
//! store hands in what it holds (a pre-filter `min(published, observed) ≤ t`
//! is safe), this module makes the final cut. `packet.rs` turns a view into
//! the evidence packet.
//!
//! | Clock | Captured — what this system had read and parsed by t | Knowable — what could have been known at t |
//! |---|---|---|
//! | Visible from | `max(published, observed, parsed)` | `immutable` source: `published` for an item's first read and every reparse of its bytes (a shared snapshot); a read of new bytes under the same id (an edit) and every `in_place` record: `max(published, observed)`; a withdrawal always from its read |
//! | Unchanged at t by | anything after t: new items, corrections, copies, edits, re-reads, reparses, withdrawals, coverage, purges | anything published after t; edits and withdrawals read after t of items first read at or before t; coverage; purges — not a reparse after t (it re-reads old bytes, so it changes what was knowable) |
//!
//! | Step | Rule |
//! |---|---|
//! | Dedup | one record per `record_id` (its earliest read) |
//! | Current version | per `(source_id, native_id)`, among visible versions: newest `observed_ms`, then higher parser `n`, then `parsed_ms`, `record_id`; the older ones are `superseded` by `revision` |
//! | Corrections | a visible record with `supersedes = X` removes X when X is visible and on the same `event_key` (`superseded` by `correction`); else issue `supersedes_other_event` / `supersedes_not_visible` |
//! | States | `withdrawn` (fact `withdrawn`) · `unparsed` (fact `unparsed`) · typed: `pending` before `valid_from_ms` (not yet in force), `expired` from `valid_until_ms`, else `in_force`; a withdrawn / unparsed item names its newest typed visible version (`last_fact`) |
//! | Issues | `rules::class_issues` on every current record; the registry freshness read = the newest of its `observed_ms` and complete coverage fetches at or before t |
//! | Origin | a record counts as its `origin.source_id` (a syndicated copy), else its own `source_id`: copies of one item count once |
//! | Confidence, per `event_key` | over current typed records without an issue and not `trigger_only`: `confirmed` ≥ 1 `primary` · `corroborated` ≥ 2 origins · `single` 1 origin; `trigger_only` when only trigger-level or issue-flagged typed records exist; `none` without a typed record |
//! | Conflicts | current typed facts of one event, the same kind and stage (TED: the notice type and the same lot set — distinct lots are never compared), disagreeing on a field (SEC: `cik`, `form`, `items`; TED: `value` as a number, `deadline_ms`, `cpv`) |

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::record::{split_parser_version, Fact, SourceRecord, Trust};
use super::rules::{class_issues, Issue, IssueCode, Revision, SourcePolicy};

/// Which clock decides visibility (module table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AsOfMode {
    Captured,
    Knowable,
}

impl AsOfMode {
    pub fn as_str(self) -> &'static str {
        match self {
            AsOfMode::Captured => "captured",
            AsOfMode::Knowable => "knowable",
        }
    }
}

/// One fetch of a source query: it read the items published in
/// `[from_ms, to_ms]`. Appended per fetch, never updated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Coverage {
    pub source_id: String,
    /// The fetch's query (a CIK, the sha256 of a query template), in full.
    pub query_key: String,
    pub fetched_ms: i64,
    pub from_ms: i64,
    pub to_ms: i64,
    /// Every page was read.
    pub complete: bool,
    /// Why it was not complete (`observation::ErrorClass` names).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_class: Option<String>,
}

/// A retention purge's tombstone: what it deleted, kept forever.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Purge {
    pub source_id: String,
    pub purged_ms: i64,
    /// Raw bodies read before it were deleted; records keep their sha256.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_before_ms: Option<i64>,
    /// Records read before it were deleted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub records_before_ms: Option<i64>,
    pub snapshots: u64,
    pub records: u64,
    pub reason: String,
}

/// What a view reads: the records and source health of the store, the
/// registry's [`SourcePolicy`] per source id.
#[derive(Debug, Clone, Copy)]
pub struct AsOfInput<'a> {
    pub records: &'a [SourceRecord],
    pub coverage: &'a [Coverage],
    pub purges: &'a [Purge],
    pub policies: &'a BTreeMap<String, SourcePolicy>,
}

/// Where a current record stands at t (module table: states).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordState {
    InForce,
    /// Visible, but its `valid_from_ms` is after t (e.g. a rule not yet effective).
    Pending,
    Expired,
    Withdrawn,
    Unparsed,
}

impl RecordState {
    #[allow(dead_code)] // for `domain/soe/` (O3) reports
    pub fn as_str(self) -> &'static str {
        match self {
            RecordState::InForce => "in_force",
            RecordState::Pending => "pending",
            RecordState::Expired => "expired",
            RecordState::Withdrawn => "withdrawn",
            RecordState::Unparsed => "unparsed",
        }
    }

    /// A typed fact about the item (in force, pending or expired).
    pub fn is_typed(self) -> bool {
        matches!(
            self,
            RecordState::InForce | RecordState::Pending | RecordState::Expired
        )
    }
}

/// The current version of one item at t.
#[derive(Debug, Clone)]
pub struct Current<'a> {
    pub record: &'a SourceRecord,
    pub visible_ms: i64,
    pub state: RecordState,
    /// Withdrawn / unparsed: the item's newest typed visible version.
    pub last_fact: Option<&'a SourceRecord>,
}

/// Why a record left the current set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SupersededBy {
    /// A newer version of the same item.
    Revision,
    /// A record naming it in `supersedes`.
    Correction,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Supersession {
    pub old: String,
    pub new: String,
    pub by: SupersededBy,
}

/// How well the sources establish one event (module table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Confirmed,
    Corroborated,
    Single,
    TriggerOnly,
    None,
}

impl Confidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Confidence::Confirmed => "confirmed",
            Confidence::Corroborated => "corroborated",
            Confidence::Single => "single",
            Confidence::TriggerOnly => "trigger_only",
            Confidence::None => "none",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventRow {
    pub event_key: String,
    pub confidence: Confidence,
    /// Counted `primary` records.
    pub primary: usize,
    /// Distinct independent origins of the counted records.
    pub origins: Vec<String>,
    /// Every current record of the event.
    pub records: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Conflict {
    pub event_key: String,
    /// The lot set both sides cover (TED); empty otherwise.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lots: Vec<String>,
    pub field: String,
    pub record_ids: Vec<String>,
}

/// The view at t (module tables). Every list is sorted.
#[derive(Debug, Clone)]
#[allow(dead_code)] // `at_ms` / `mode` name the view for `domain/soe/` (O3) readers
pub struct AsOfView<'a> {
    pub at_ms: i64,
    pub mode: AsOfMode,
    /// By `(source_id, native_id)`.
    pub current: Vec<Current<'a>>,
    pub superseded: Vec<Supersession>,
    pub issues: Vec<Issue>,
    pub events: Vec<EventRow>,
    pub conflicts: Vec<Conflict>,
    /// Every record visible at t, by id.
    pub visible: BTreeMap<&'a str, &'a SourceRecord>,
}

/// The source a record speaks for: its `origin` when it is a copy.
pub fn origin_of(r: &SourceRecord) -> &str {
    r.origin
        .as_ref()
        .map_or(r.source_id.as_str(), |o| o.source_id.as_str())
}

/// When `r` becomes visible (module table). `reads_base` = it rests on a
/// raw snapshot of its item's first read (the first read itself included).
pub fn visible_ms(r: &SourceRecord, revision: Revision, mode: AsOfMode, reads_base: bool) -> i64 {
    let read = r.published_ms.max(r.observed_ms);
    match mode {
        AsOfMode::Captured => read.max(r.parsed_ms),
        AsOfMode::Knowable => {
            let withdrawn = matches!(r.fact, Fact::Withdrawn(_));
            if revision == Revision::Immutable && reads_base && !withdrawn {
                r.published_ms
            } else {
                read
            }
        }
    }
}

/// The revision a source id is read with: its row's, else `in_place` (an
/// unlisted source may have edited anything).
fn revision_of(policies: &BTreeMap<String, SourcePolicy>, source_id: &str) -> Revision {
    policies
        .get(source_id)
        .map_or(Revision::InPlace, |p| p.revision)
}

/// One record per id: the earliest read (`observed_ms`, then `parsed_ms`).
fn dedup(records: &[SourceRecord]) -> BTreeMap<&str, &SourceRecord> {
    let mut by_id: BTreeMap<&str, &SourceRecord> = BTreeMap::new();
    for r in records {
        by_id
            .entry(r.record_id.as_str())
            .and_modify(|e| {
                if (r.observed_ms, r.parsed_ms) < (e.observed_ms, e.parsed_ms) {
                    *e = r;
                }
            })
            .or_insert(r);
    }
    by_id
}

/// Every record (deduplicated) with its visible instant in `mode`, grouped
/// by item `(source_id, native_id)`.
pub fn visibility<'a>(
    records: &'a [SourceRecord],
    policies: &BTreeMap<String, SourcePolicy>,
    mode: AsOfMode,
) -> BTreeMap<(&'a str, &'a str), Vec<(&'a SourceRecord, i64)>> {
    let mut items: BTreeMap<(&str, &str), Vec<&SourceRecord>> = BTreeMap::new();
    for r in dedup(records).into_values() {
        items
            .entry((r.source_id.as_str(), r.native_id.as_str()))
            .or_default()
            .push(r);
    }
    items
        .into_iter()
        .map(|(key, versions)| {
            let base = versions
                .iter()
                .min_by_key(|r| (r.observed_ms, r.parsed_ms, r.record_id.as_str()))
                .expect("an item has a version");
            let base_snaps: BTreeSet<&str> = base.snapshots.iter().map(String::as_str).collect();
            let revision = revision_of(policies, key.0);
            let out = versions
                .iter()
                .map(|r| {
                    let reads_base = r.record_id == base.record_id
                        || r.snapshots.iter().any(|s| base_snaps.contains(s.as_str()));
                    (*r, visible_ms(r, revision, mode, reads_base))
                })
                .collect();
            (key, out)
        })
        .collect()
}

/// Version order inside one item: newest read last (module table).
fn version_key(r: &SourceRecord) -> (i64, u32, i64, &str) {
    let n = split_parser_version(&r.parser_version).map_or(0, |(_, n)| n);
    (r.observed_ms, n, r.parsed_ms, r.record_id.as_str())
}

/// Per source: the newest complete coverage fetch at or before `t`.
pub fn newest_fetches(coverage: &[Coverage], t: i64) -> BTreeMap<&str, i64> {
    let mut out: BTreeMap<&str, i64> = BTreeMap::new();
    for c in coverage.iter().filter(|c| c.complete && c.fetched_ms <= t) {
        let e = out.entry(c.source_id.as_str()).or_insert(c.fetched_ms);
        *e = (*e).max(c.fetched_ms);
    }
    out
}

/// The view of `input` at `t` (module tables).
pub fn as_of<'a>(input: &AsOfInput<'a>, t: i64, mode: AsOfMode) -> AsOfView<'a> {
    let mut visible: BTreeMap<&'a str, &'a SourceRecord> = BTreeMap::new();
    let mut superseded: BTreeSet<Supersession> = BTreeSet::new();
    // Per item: its visible versions, oldest first.
    let mut items: Vec<Vec<(&'a SourceRecord, i64)>> = Vec::new();
    for (_, versions) in visibility(input.records, input.policies, mode) {
        let mut seen: Vec<(&SourceRecord, i64)> =
            versions.into_iter().filter(|(_, v)| *v <= t).collect();
        if seen.is_empty() {
            continue;
        }
        seen.sort_by(|a, b| version_key(a.0).cmp(&version_key(b.0)));
        for w in seen.windows(2) {
            superseded.insert(Supersession {
                old: w[0].0.record_id.clone(),
                new: w[1].0.record_id.clone(),
                by: SupersededBy::Revision,
            });
        }
        for (r, _) in &seen {
            visible.insert(r.record_id.as_str(), *r);
        }
        items.push(seen);
    }

    // Corrections: from every visible record, current or not.
    let mut issues: BTreeSet<Issue> = BTreeSet::new();
    let mut removed: BTreeSet<&str> = BTreeSet::new();
    let currents: BTreeSet<&str> = items
        .iter()
        .map(|v| v.last().expect("non-empty").0.record_id.as_str())
        .collect();
    for r in visible.values() {
        let Some(target) = r.supersedes.as_deref() else {
            continue;
        };
        let is_current = currents.contains(r.record_id.as_str());
        match visible.get(target) {
            None => {
                if is_current {
                    issues.insert(Issue::new(
                        &r.record_id,
                        IssueCode::SupersedesNotVisible,
                        "the record it corrects is not visible at t; nothing removed",
                    ));
                }
            }
            Some(x) if x.event_key != r.event_key => {
                if is_current {
                    issues.insert(Issue::new(
                        &r.record_id,
                        IssueCode::SupersedesOtherEvent,
                        "the record it corrects belongs to another event; ignored",
                    ));
                }
            }
            Some(x) => {
                removed.insert(x.record_id.as_str());
                superseded.insert(Supersession {
                    old: x.record_id.clone(),
                    new: r.record_id.clone(),
                    by: SupersededBy::Correction,
                });
            }
        }
    }

    let fetches = newest_fetches(input.coverage, t);
    let mut current: Vec<Current<'a>> = Vec::new();
    for seen in &items {
        let (record, visible_ms) = *seen.last().expect("non-empty");
        if removed.contains(record.record_id.as_str()) {
            continue;
        }
        let state = match &record.fact {
            Fact::Withdrawn(_) => RecordState::Withdrawn,
            Fact::Unparsed { .. } => RecordState::Unparsed,
            _ if record.valid_from_ms > t => RecordState::Pending,
            _ if record.valid_until_ms.is_some_and(|u| u <= t) => RecordState::Expired,
            _ => RecordState::InForce,
        };
        let last_fact = (!state.is_typed())
            .then(|| {
                seen.iter()
                    .rev()
                    .skip(1)
                    .map(|(r, _)| *r)
                    .find(|r| r.fact.is_typed())
            })
            .flatten();
        let read = [
            Some(record.observed_ms).filter(|o| *o <= t),
            fetches.get(record.source_id.as_str()).copied(),
        ]
        .into_iter()
        .flatten()
        .max();
        issues.extend(class_issues(
            record,
            input.policies.get(&record.source_id),
            read,
            t,
        ));
        current.push(Current {
            record,
            visible_ms,
            state,
            last_fact,
        });
    }

    let flagged: BTreeSet<&str> = issues.iter().map(|i| i.record_id.as_str()).collect();
    let events = events(&current, &flagged);
    let conflicts = conflicts(&current);
    AsOfView {
        at_ms: t,
        mode,
        current,
        superseded: superseded.into_iter().collect(),
        issues: issues.into_iter().collect(),
        events,
        conflicts,
        visible,
    }
}

/// Confidence per event (module table).
fn events(current: &[Current], flagged: &BTreeSet<&str>) -> Vec<EventRow> {
    let mut by_event: BTreeMap<&str, Vec<&Current>> = BTreeMap::new();
    for c in current {
        by_event
            .entry(c.record.event_key.as_str())
            .or_default()
            .push(c);
    }
    by_event
        .into_iter()
        .map(|(key, rows)| {
            let typed: Vec<&SourceRecord> = rows
                .iter()
                .filter(|c| c.state.is_typed())
                .map(|c| c.record)
                .collect();
            let counted: Vec<&SourceRecord> = typed
                .iter()
                .copied()
                .filter(|r| {
                    r.trust != Trust::TriggerOnly && !flagged.contains(r.record_id.as_str())
                })
                .collect();
            let primary = counted.iter().filter(|r| r.trust == Trust::Primary).count();
            let origins: BTreeSet<&str> = counted.iter().map(|r| origin_of(r)).collect();
            let confidence = if primary > 0 {
                Confidence::Confirmed
            } else if origins.len() >= 2 {
                Confidence::Corroborated
            } else if origins.len() == 1 {
                Confidence::Single
            } else if !typed.is_empty() {
                Confidence::TriggerOnly
            } else {
                Confidence::None
            };
            let mut records: Vec<String> =
                rows.iter().map(|c| c.record.record_id.clone()).collect();
            records.sort();
            EventRow {
                event_key: key.to_string(),
                confidence,
                primary,
                origins: origins.into_iter().map(String::from).collect(),
                records,
            }
        })
        .collect()
}

/// A decimal as a comparable number text: `150000.00` = `150000` = `0150000.0`.
fn decimal_key(a: &str) -> String {
    let (neg, body) = a.strip_prefix('-').map_or((false, a), |b| (true, b));
    let (int, frac) = body.split_once('.').unwrap_or((body, ""));
    let int = int.trim_start_matches('0');
    let frac = frac.trim_end_matches('0');
    let int = if int.is_empty() { "0" } else { int };
    let s = if frac.is_empty() {
        int.to_string()
    } else {
        format!("{int}.{frac}")
    };
    if neg && s != "0" {
        format!("-{s}")
    } else {
        s
    }
}

/// The fields compared within one stage, each `Some` when the fact states it.
fn compared(fact: &Fact) -> Vec<(&'static str, Option<String>)> {
    let sorted = |v: &[String]| {
        let mut v = v.to_vec();
        v.sort();
        v.join(",")
    };
    match fact {
        Fact::SecFiling(f) => vec![
            ("cik", Some(f.cik.clone())),
            ("form", Some(f.form.clone())),
            ("items", Some(sorted(&f.items))),
        ],
        Fact::TedNotice(n) => vec![
            (
                "value",
                n.value
                    .as_ref()
                    .map(|v| format!("{} {}", decimal_key(&v.amount), v.currency)),
            ),
            ("deadline_ms", n.deadline_ms.map(|d| d.to_string())),
            ("cpv", (!n.cpv.is_empty()).then(|| sorted(&n.cpv))),
        ],
        Fact::Unparsed { .. } | Fact::Withdrawn(_) => Vec::new(),
    }
}

/// `(kind, stage, lot set)` — facts are compared only within one.
fn stage(fact: &Fact) -> (&'static str, String, Vec<String>) {
    match fact {
        Fact::TedNotice(n) => {
            let mut lots = n.lot_ids.clone();
            lots.sort();
            (fact.kind(), n.notice_type.clone(), lots)
        }
        _ => (fact.kind(), String::new(), Vec::new()),
    }
}

/// Disagreements among current typed facts (module table).
fn conflicts(current: &[Current]) -> Vec<Conflict> {
    type Group<'a> =
        BTreeMap<(&'a str, (&'static str, String, Vec<String>)), Vec<&'a SourceRecord>>;
    let mut groups: Group = BTreeMap::new();
    for c in current.iter().filter(|c| c.state.is_typed()) {
        groups
            .entry((c.record.event_key.as_str(), stage(&c.record.fact)))
            .or_default()
            .push(c.record);
    }
    let mut out = Vec::new();
    for ((event_key, (_, _, lots)), rows) in groups {
        if rows.len() < 2 {
            continue;
        }
        let fields: Vec<Vec<(&'static str, Option<String>)>> =
            rows.iter().map(|r| compared(&r.fact)).collect();
        for (i, (field, _)) in fields[0].iter().enumerate() {
            let stated: Vec<(&str, &String)> = rows
                .iter()
                .zip(&fields)
                .filter_map(|(r, f)| f[i].1.as_ref().map(|v| (r.record_id.as_str(), v)))
                .collect();
            let values: BTreeSet<&String> = stated.iter().map(|(_, v)| *v).collect();
            if values.len() > 1 {
                let mut record_ids: Vec<String> =
                    stated.iter().map(|(id, _)| id.to_string()).collect();
                record_ids.sort();
                out.push(Conflict {
                    event_key: event_key.to_string(),
                    lots: lots.clone(),
                    field: (*field).to_string(),
                    record_ids,
                });
            }
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::source::record::{Origin, Withdrawn, WithdrawnHow};
    use crate::domain::source::testkit::{
        copy_of, correction_of, edited, policies, rec, reparsed, ted_mut, Src, D, H, T0,
    };

    fn view_ids(v: &AsOfView) -> Vec<String> {
        v.current
            .iter()
            .map(|c| c.record.record_id.clone())
            .collect()
    }

    fn view<'a>(
        records: &'a [SourceRecord],
        p: &'a BTreeMap<String, SourcePolicy>,
        t: i64,
        mode: AsOfMode,
    ) -> AsOfView<'a> {
        as_of(
            &AsOfInput {
                records,
                coverage: &[],
                purges: &[],
                policies: p,
            },
            t,
            mode,
        )
    }

    #[test]
    fn published_exactly_at_t_is_visible_t_plus_1ms_is_not() {
        let p = policies(&[Src::SEC, Src::REPO]);
        // Read the instant it was published: both clocks agree.
        let now = rec(Src::SEC, "0000000001-26-000001", T0, T0, T0);
        // Read an hour later: knowable from publication, captured from the read.
        let late = rec(Src::SEC, "0000000001-26-000002", T0, T0 + H, T0 + H + 1);
        // An in-place listing: from its read in both modes.
        let page = rec(Src::REPO, "repo-1", T0, T0 + H, T0 + H);
        let all = vec![now.clone(), late.clone(), page.clone()];
        for mode in [AsOfMode::Captured, AsOfMode::Knowable] {
            assert_eq!(
                view_ids(&view(&all, &p, T0 - 1, mode)),
                Vec::<String>::new()
            );
            assert!(
                view_ids(&view(&all, &p, T0, mode)).contains(&now.record_id),
                "{mode:?}"
            );
        }
        let at = |t, mode| view_ids(&view(&all, &p, t, mode));
        assert!(at(T0, AsOfMode::Knowable).contains(&late.record_id));
        assert!(!at(T0 + H, AsOfMode::Captured).contains(&late.record_id));
        assert!(at(T0 + H + 1, AsOfMode::Captured).contains(&late.record_id));
        for mode in [AsOfMode::Captured, AsOfMode::Knowable] {
            assert!(!at(T0 + H - 1, mode).contains(&page.record_id), "{mode:?}");
            assert!(at(T0 + H, mode).contains(&page.record_id), "{mode:?}");
        }
    }

    #[test]
    fn in_place_change_is_visible_only_from_its_observation() {
        let p = policies(&[Src::REPO, Src::SEC]);
        let first = rec(Src::REPO, "repo-1", T0, T0 + H, T0 + H);
        let edit = edited(&first, T0 + 10 * H, "release 2");
        let all = vec![first.clone(), edit.clone()];
        for mode in [AsOfMode::Captured, AsOfMode::Knowable] {
            for t in [T0 + H, T0 + 10 * H - 1] {
                let v = view(&all, &p, t, mode);
                assert_eq!(view_ids(&v), vec![first.record_id.clone()], "{mode:?} {t}");
                assert!(v.superseded.is_empty());
            }
            // Captured also waits for the parse, 1 s after the read.
            let v = view(&all, &p, T0 + 10 * H + 1_000, mode);
            assert_eq!(view_ids(&v), vec![edit.record_id.clone()]);
            assert_eq!(
                v.superseded,
                vec![Supersession {
                    old: first.record_id.clone(),
                    new: edit.record_id.clone(),
                    by: SupersededBy::Revision
                }]
            );
        }
        // A source declared immutable still shows new bytes for an item only
        // from their read: an edit is never back-dated to the publication.
        let filing = rec(Src::SEC, "0000000001-26-000001", T0, T0 + H, T0 + H);
        let changed = edited(&filing, T0 + 10 * H, "changed title");
        let all = vec![filing.clone(), changed.clone()];
        let v = view(&all, &p, T0 + 5 * H, AsOfMode::Knowable);
        assert_eq!(view_ids(&v), vec![filing.record_id.clone()]);
        let v = view(&all, &p, T0, AsOfMode::Knowable);
        assert_eq!(view_ids(&v), vec![filing.record_id.clone()]);
    }

    #[test]
    fn reparse_after_t_is_invisible_in_captured_mode() {
        let p = policies(&[Src::SEC]);
        let first = rec(Src::SEC, "0000000001-26-000001", T0, T0 + H, T0 + H);
        let again = reparsed(&first, T0 + 3 * D);
        assert_eq!(again.snapshots, first.snapshots);
        assert_ne!(again.record_id, first.record_id);
        let all = vec![first.clone(), again.clone()];
        let captured = |t| view_ids(&view(&all, &p, t, AsOfMode::Captured));
        assert_eq!(captured(T0 + 3 * D - 1), vec![first.record_id.clone()]);
        assert_eq!(captured(T0 + 3 * D), vec![again.record_id.clone()]);
        // Knowable: today's parse of bytes that existed at t wins (by design).
        let knowable = view_ids(&view(&all, &p, T0, AsOfMode::Knowable));
        assert_eq!(knowable, vec![again.record_id.clone()]);
    }

    #[test]
    fn correction_after_t_never_changes_the_t_view() {
        let p = policies(&[Src::TED]);
        let notice = rec(Src::TED, "100-2026", T0, T0 + H, T0 + H);
        let fix = correction_of(&notice, "101-2026", T0 + 2 * D, T0 + 2 * D + H, |n| {
            ted_mut(n).value = Some(crate::domain::source::NativeAmount::new("120000.00", "EUR"))
        });
        let before = vec![notice.clone()];
        let after = vec![notice.clone(), fix.clone()];
        for mode in [AsOfMode::Captured, AsOfMode::Knowable] {
            for t in [T0 + H, T0 + D, T0 + 2 * D - 1] {
                let (a, b) = (view(&before, &p, t, mode), view(&after, &p, t, mode));
                assert_eq!(view_ids(&a), view_ids(&b), "{mode:?} {t}");
                assert_eq!(a.superseded, b.superseded);
                assert_eq!(a.events, b.events);
            }
        }
        // From its own read it replaces the original; the original stays a record.
        let v = view(&after, &p, T0 + 2 * D + H, AsOfMode::Captured);
        assert_eq!(view_ids(&v), vec![fix.record_id.clone()]);
        assert_eq!(v.superseded[0].by, SupersededBy::Correction);
        assert_eq!(v.superseded[0].old, notice.record_id);
        assert!(v.visible.contains_key(notice.record_id.as_str()));
        // A correction naming another event's record removes nothing.
        let mut stray = correction_of(&notice, "102-2026", T0 + D, T0 + D, |_| {});
        stray.event_key = "ted:procedure:another".into();
        let stray = stray.with_identity();
        let all = vec![notice.clone(), stray.clone()];
        let v = view(&all, &p, T0 + 2 * D, AsOfMode::Captured);
        assert_eq!(v.current.len(), 2);
        assert_eq!(v.issues[0].code, IssueCode::SupersedesOtherEvent);
        assert_eq!(v.issues[0].record_id, stray.record_id);
    }

    #[test]
    fn syndicated_copies_count_once() {
        let p = policies(&[Src::SEC, Src::WIRE, Src::DAILY, Src::SOCIAL]);
        let filing = rec(Src::SEC, "0000000001-26-000001", T0, T0, T0);
        let copy = |src: Src, native: &str| copy_of(&filing, src, native, T0 + H, T0 + H);
        let wire_a = copy(Src::WIRE, "wire-1");
        let mut wire_b = copy(Src::WIRE, "wire-2");
        wire_b.source_id = "news_wire_b".into();
        let wire_b = wire_b.with_identity();
        let mut p2 = p.clone();
        p2.insert("news_wire_b".into(), Src::WIRE.policy());
        let grade = |records: &[SourceRecord]| {
            let v = view(records, &p2, T0 + D, AsOfMode::Captured);
            assert_eq!(v.events.len(), 1, "{:?}", v.events);
            (v.events[0].confidence, v.events[0].origins.clone())
        };
        // Two copies of one filing from two wires: one origin, a single report.
        let (c, o) = grade(&[wire_a.clone(), wire_b.clone()]);
        assert_eq!((c, o), (Confidence::Single, vec!["sec_edgar".to_string()]));
        // The original itself: confirmed, still one origin.
        let (c, o) = grade(&[filing.clone(), wire_a.clone(), wire_b.clone()]);
        assert_eq!((c, o.len()), (Confidence::Confirmed, 1));
        // An independent outlet (no origin) + a copy: two origins.
        let mut daily = rec(Src::DAILY, "daily-1", T0 + 2 * H, T0 + 2 * H, T0 + 2 * H);
        daily.event_key = filing.event_key.clone();
        let daily = daily.with_identity();
        let (c, o) = grade(&[wire_a.clone(), daily.clone()]);
        assert_eq!(
            (c, o),
            (
                Confidence::Corroborated,
                vec!["sec_edgar".to_string(), "trade_daily".to_string()]
            )
        );
        // A social post confirms nothing.
        let mut post = rec(Src::SOCIAL, "post-1", T0, T0, T0);
        post.event_key = filing.event_key.clone();
        let post = post.with_identity();
        assert_eq!(grade(&[post]).0, Confidence::TriggerOnly);
        assert_eq!(origin_of(&wire_a), "sec_edgar");
        assert_eq!(
            wire_a.origin,
            Some(Origin {
                source_id: "sec_edgar".into(),
                native_id: Some(filing.native_id.clone())
            })
        );
    }

    #[test]
    fn distinct_lots_of_one_procedure_are_not_merged() {
        let p = policies(&[Src::TED]);
        let lot = |native: &str, lot: &str, amount: &str| {
            let mut r = rec(Src::TED, native, T0, T0 + H, T0 + H);
            let n = ted_mut(&mut r);
            n.lot_ids = vec![lot.to_string()];
            n.value = Some(crate::domain::source::NativeAmount::new(amount, "EUR"));
            r.with_identity()
        };
        let a = lot("100-2026", "LOT-0001", "100000.00");
        let b = lot("101-2026", "LOT-0002", "250000.00");
        let all = vec![a.clone(), b.clone()];
        let v = view(&all, &p, T0 + D, AsOfMode::Captured);
        assert_eq!(v.current.len(), 2);
        assert!(v.superseded.is_empty());
        assert!(v.conflicts.is_empty(), "{:?}", v.conflicts);
        assert_eq!(v.events.len(), 1);
        assert_eq!(v.events[0].records.len(), 2);
        // The same lot at another amount is a disagreement.
        let c = lot("102-2026", "LOT-0001", "100500.00");
        let all = vec![a.clone(), b, c.clone()];
        let v = view(&all, &p, T0 + D, AsOfMode::Captured);
        assert_eq!(v.conflicts.len(), 1);
        assert_eq!(v.conflicts[0].lots, vec!["LOT-0001".to_string()]);
        assert_eq!(v.conflicts[0].field, "value");
        let mut ids = vec![a.record_id.clone(), c.record_id.clone()];
        ids.sort();
        assert_eq!(v.conflicts[0].record_ids, ids);
        // The same amount written differently is no disagreement.
        let d = lot("103-2026", "LOT-0001", "100000");
        let same = vec![a, d];
        let v = view(&same, &p, T0 + D, AsOfMode::Captured);
        assert!(v.conflicts.is_empty(), "{:?}", v.conflicts);
    }

    /// Critic U8: an item that later reads as gone or moved shows as
    /// withdrawn from that read on, naming the version it replaced.
    #[test]
    fn withdrawn_upstream_content_is_a_visible_record() {
        let p = policies(&[Src::SEC]);
        let filing = rec(Src::SEC, "0000000001-26-000001", T0, T0 + H, T0 + H);
        let gone = filing.withdrawal(
            Withdrawn {
                how: WithdrawnHow::Gone,
                http_status: Some(404),
                moved_to: None,
                reason: "404".into(),
            },
            crate::domain::canonical::sha256_hex("404"),
            T0 + 5 * D,
            T0 + 5 * D + 1,
        );
        let all = vec![filing.clone(), gone.clone()];
        for mode in [AsOfMode::Captured, AsOfMode::Knowable] {
            // From its parse (captured waits 1 ms past the read).
            let v = view(&all, &p, T0 + 5 * D + 1, mode);
            assert_eq!(v.current.len(), 1, "{mode:?}");
            assert_eq!(v.current[0].state, RecordState::Withdrawn);
            assert_eq!(v.current[0].last_fact.unwrap().record_id, filing.record_id);
            assert_eq!(v.events[0].confidence, Confidence::None);
            // Never back-dated, even for an immutable source in knowable mode.
            let v = view(&all, &p, T0 + 5 * D - 1, mode);
            assert_eq!(view_ids(&v), vec![filing.record_id.clone()], "{mode:?}");
        }
    }

    #[test]
    fn states_follow_the_validity_window() {
        let p = policies(&[Src::TED]);
        let mut r = rec(Src::TED, "100-2026", T0, T0, T0);
        r.valid_from_ms = T0 + D;
        r.valid_until_ms = Some(T0 + 3 * D);
        let r = r.with_identity();
        let all = vec![r];
        let state = |t| view(&all, &p, t, AsOfMode::Captured).current[0].state;
        assert_eq!(state(T0), RecordState::Pending);
        assert_eq!(state(T0 + D), RecordState::InForce);
        assert_eq!(state(T0 + 3 * D - 1), RecordState::InForce);
        assert_eq!(state(T0 + 3 * D), RecordState::Expired);
        assert_eq!(decimal_key("0150000.50"), "150000.5");
        assert_eq!(decimal_key("-0.00"), "0");
        assert_eq!(decimal_key("-12.30"), "-12.3");
    }
}
