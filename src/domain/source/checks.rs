//! Split-world no-lookahead checks of the source layer (compiled for tests
//! only), modelled on `domain/backtest/checks.rs`.
//!
//! | Check | Holds |
//! |---|---|
//! | Worlds | `testkit`: `sec` (immutable filings, amendments, copies, an outlet that disagrees, reparses, a parse fixed by a reparse, a withdrawal, coverage with failed fetches, a raw purge), `ted` (immutable notices with lots, a notice per lot, a duplicate, corrigenda, awards, a notice in force later, a buyer without jurisdiction, demand posts, an uncovered day), `in_place` (a registry listing edited every 3 days, gone, moved, a 3-day coverage hole) |
//! | Instants t | every 9 h over each world, plus the published / read / parsed instants of every 5th record and 1 ms before each |
//! | What moves | per mode, the records not visible at t; in knowable mode not the later reads of an item first read after t (knowable mode back-dates them by design) |
//! | Moves after t | `delete` them, and coverage and purges after t · `next_ms` — each of their clocks after t set to t + 1 ms (ids re-derived, corrections re-pointed) · `flood` — one record an hour for 48 h from t + 1 ms on the items, events and entities visible at t: corrections, edits (new bytes), copies, withdrawals, new items · `coverage` — complete and failed fetches after t · `reparse` — every record parsed again by the next parser after t · `purge` — raw purges after t (records keep their hashes) |
//! | Captured | `canonical_json(packet(t))` byte-identical under every move |
//! | Knowable | the same under every move but `reparse` (it re-reads bytes that existed at t: what was knowable changes, by design) |
//! | Boundary | every record is visible from its visible instant on, and not 1 ms before |
//! | Append-only | corrections after t add records and rewrite none; from their read on, each shows as `old → new` (`correction`) with both cited |
//! | Knowable reads no edit early | an edit — new bytes for an item, in place or under a declared-immutable id — is in no knowable view before its read |
//! | Not vacuous | across the worlds the packets show corrections, revisions, withdrawn, unparsed, pending and expired rows, conflicts, `jurisdiction_missing` and `listing_stale`, every confidence, single and aggregate demand, gaps, failed fetches and purges; the two modes differ somewhere |

use std::collections::{BTreeMap, BTreeSet};

use super::asof::{as_of, visible_ms, AsOfMode, Confidence, Coverage, Purge, SupersededBy};
use super::packet::EvidencePacket;
use super::record::SourceRecord;
use super::rules::{IssueCode, Revision};
use super::testkit::{
    copy_of, correction_of, edited, gone, reparsed, retext, worlds, Src, World, D, H,
};
use crate::domain::canonical::canonical_json;
use crate::domain::marketdata::fmt_time;
use crate::domain::source::record::WithdrawnHow;

const MODES: [AsOfMode; 2] = [AsOfMode::Captured, AsOfMode::Knowable];
const FLOOD_HOURS: i64 = 48;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Move {
    Delete,
    NextMs,
    Flood,
    Coverage,
    Reparse,
    Purge,
}

const MOVES: [Move; 6] = [
    Move::Delete,
    Move::NextMs,
    Move::Flood,
    Move::Coverage,
    Move::Reparse,
    Move::Purge,
];

/// Module table: knowable mode is not invariant to a reparse after t.
fn allowed(mode: AsOfMode, how: Move) -> bool {
    !(mode == AsOfMode::Knowable && how == Move::Reparse)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flood {
    Correction,
    Edit,
    Copy,
    Withdrawal,
    NewItem,
}

const FLOOD_ALL: [Flood; 5] = [
    Flood::Correction,
    Flood::Edit,
    Flood::Copy,
    Flood::Withdrawal,
    Flood::NewItem,
];

fn canon(p: &EvidencePacket) -> String {
    canonical_json(&serde_json::to_value(p).expect("packet serializes"))
}

/// `a == b`, else a panic naming where they part.
fn assert_same(a: &str, b: &str, what: &str) {
    if a == b {
        return;
    }
    let at = a
        .bytes()
        .zip(b.bytes())
        .position(|(x, y)| x != y)
        .unwrap_or(a.len().min(b.len()));
    let from = at.saturating_sub(200);
    let window = |s: &str| {
        s.get(from..(at + 200).min(s.len()))
            .unwrap_or("")
            .to_string()
    };
    panic!(
        "{what}: packets differ at byte {at}\n moved: …{}…\n  base: …{}…",
        window(a),
        window(b)
    );
}

/// Per record (not deduplicated), when it becomes visible in `mode` — the
/// `asof.rs` rule, applied to each copy.
fn record_visible(w: &World, mode: AsOfMode) -> Vec<i64> {
    let mut base: BTreeMap<(&str, &str), &SourceRecord> = BTreeMap::new();
    for r in &w.records {
        let e = base
            .entry((r.source_id.as_str(), r.native_id.as_str()))
            .or_insert(r);
        if (r.observed_ms, r.parsed_ms, r.record_id.as_str())
            < (e.observed_ms, e.parsed_ms, e.record_id.as_str())
        {
            *e = r;
        }
    }
    w.records
        .iter()
        .map(|r| {
            let b = base[&(r.source_id.as_str(), r.native_id.as_str())];
            let reads_base =
                r.record_id == b.record_id || r.snapshots.iter().any(|s| b.snapshots.contains(s));
            let revision = w
                .policies
                .get(&r.source_id)
                .map_or(Revision::InPlace, |p| p.revision);
            visible_ms(r, revision, mode, reads_base)
        })
        .collect()
}

/// Module table: what moves.
fn in_scope(w: &World, t: i64, mode: AsOfMode) -> Vec<bool> {
    let mut first_read: BTreeMap<(&str, &str), i64> = BTreeMap::new();
    for r in &w.records {
        let e = first_read
            .entry((r.source_id.as_str(), r.native_id.as_str()))
            .or_insert(r.observed_ms);
        *e = (*e).min(r.observed_ms);
    }
    w.records
        .iter()
        .zip(record_visible(w, mode))
        .map(|(r, v)| {
            v > t
                && (mode == AsOfMode::Captured
                    || r.published_ms > t
                    || first_read[&(r.source_id.as_str(), r.native_id.as_str())] <= t)
        })
        .collect()
}

/// `w` with what comes after `t` moved (module table).
fn moved(w: &World, t: i64, mode: AsOfMode, how: Move) -> World {
    let mut out = w.clone();
    match how {
        Move::Delete => {
            let scope = in_scope(w, t, mode);
            out.records = w
                .records
                .iter()
                .zip(&scope)
                .filter(|(_, s)| !**s)
                .map(|(r, _)| r.clone())
                .collect();
            out.coverage.retain(|c| c.fetched_ms <= t);
            out.purges.retain(|p| p.purged_ms <= t);
        }
        Move::NextMs => next_ms(&mut out, &in_scope(w, t, mode), t),
        Move::Flood => out.records.extend(flood(w, t, &FLOOD_ALL)),
        Move::Coverage => out.coverage.extend(later_coverage(w, t)),
        Move::Reparse => out.records.extend(
            w.records
                .iter()
                .enumerate()
                .map(|(i, r)| reparsed(r, t + 1 + (i as i64 % 50) * 60_000)),
        ),
        Move::Purge => out.purges.extend(later_purges(w, t)),
    }
    out
}

/// Every clock after `t` of the records in `scope` → t + 1 ms; ids
/// re-derived and corrections re-pointed at them.
fn next_ms(w: &mut World, scope: &[bool], t: i64) {
    let later = |c: i64| if c > t { t + 1 } else { c };
    let mut renamed: BTreeMap<String, String> = BTreeMap::new();
    for (r, moves) in w.records.iter_mut().zip(scope) {
        if !*moves {
            continue;
        }
        let old = r.record_id.clone();
        r.published_ms = later(r.published_ms);
        r.observed_ms = later(r.observed_ms);
        r.parsed_ms = later(r.parsed_ms);
        r.valid_from_ms = later(r.valid_from_ms);
        *r = r.clone().with_identity();
        if r.record_id != old {
            renamed.insert(old, r.record_id.clone());
        }
    }
    loop {
        let mut changed = false;
        for (r, moves) in w.records.iter_mut().zip(scope) {
            let Some(new) = r.supersedes.as_ref().and_then(|x| renamed.get(x)).cloned() else {
                continue;
            };
            assert!(
                *moves,
                "{} (visible at t) names a moved record",
                r.record_id
            );
            let old = r.record_id.clone();
            r.supersedes = Some(new);
            *r = r.clone().with_identity();
            renamed.insert(old, r.record_id.clone());
            changed = true;
        }
        if !changed {
            break;
        }
    }
    for c in &mut w.coverage {
        c.fetched_ms = later(c.fetched_ms);
    }
    for p in &mut w.purges {
        p.purged_ms = later(p.purged_ms);
    }
}

/// One record an hour from t + 1 ms on the items read at or before t
/// (module table: `flood`), cycling through `kinds`.
fn flood(w: &World, t: i64, kinds: &[Flood]) -> Vec<SourceRecord> {
    let visible = record_visible(w, AsOfMode::Captured);
    let mut items = BTreeSet::new();
    let templates: Vec<&SourceRecord> = w
        .records
        .iter()
        .zip(&visible)
        .filter(|(_, v)| **v <= t)
        .map(|(r, _)| r)
        .filter(|r| items.insert((r.source_id.as_str(), r.native_id.as_str())))
        .collect();
    if templates.is_empty() {
        return Vec::new();
    }
    (0..FLOOD_HOURS)
        .map(|k| {
            let at = t + 1 + k * H;
            let tpl = templates[k as usize % templates.len()];
            let r = match kinds[k as usize % kinds.len()] {
                Flood::Correction => correction_of(tpl, &format!("flood-{k}-c"), at, at, |r| {
                    retext(r, &format!("flood correction {k}"))
                }),
                Flood::Edit => edited(tpl, at, &format!("flood edit {k}")),
                Flood::Copy => copy_of(tpl, Src::WIRE, &format!("flood-{k}-copy"), at, at),
                Flood::Withdrawal => gone(tpl, at, WithdrawnHow::Gone),
                Flood::NewItem => {
                    let mut n = tpl.clone();
                    n.native_id = format!("flood-{k}-n");
                    n.url = format!("https://example.org/{}/flood-{k}-n", tpl.source_id);
                    n.published_ms = at;
                    n.observed_ms = at;
                    n.parsed_ms = at;
                    n.valid_from_ms = at;
                    n.valid_until_ms = n.valid_until_ms.map(|u| u.max(at + D));
                    n.snapshots = vec![crate::domain::canonical::sha256_hex(&format!("flood {k}"))];
                    n.supersedes = None;
                    n.with_identity()
                }
            };
            assert_eq!(r.validate(), Ok(()), "flood {k}");
            r
        })
        .collect()
}

/// Complete and failed fetches of every query after t.
fn later_coverage(w: &World, t: i64) -> Vec<Coverage> {
    let queries: BTreeSet<(&str, &str)> = w
        .coverage
        .iter()
        .map(|c| (c.source_id.as_str(), c.query_key.as_str()))
        .collect();
    queries
        .into_iter()
        .flat_map(|(s, q)| {
            let row = |fetched: i64, complete| Coverage {
                source_id: s.into(),
                query_key: q.into(),
                fetched_ms: fetched,
                from_ms: t - 30 * D,
                to_ms: fetched,
                complete,
                error_class: (!complete).then(|| "transient".to_string()),
            };
            [row(t + 1, true), row(t + 2 * H, false), row(t + D, true)]
        })
        .collect()
}

/// A raw purge of every source after t.
fn later_purges(w: &World, t: i64) -> Vec<Purge> {
    w.policies
        .keys()
        .map(|s| Purge {
            source_id: s.clone(),
            purged_ms: t + 1,
            raw_before_ms: Some(t),
            records_before_ms: None,
            snapshots: 5,
            records: 0,
            reason: "raw_retention_days".into(),
        })
        .collect()
}

/// Module table: instants.
fn instants(w: &World) -> Vec<i64> {
    let mut out: BTreeSet<i64> = (0..)
        .map(|k| w.span.0 + k * 9 * H)
        .take_while(|t| *t <= w.span.1)
        .collect();
    for r in w.records.iter().step_by(5) {
        for c in [r.published_ms, r.observed_ms, r.parsed_ms] {
            out.insert(c);
            out.insert(c - 1);
        }
    }
    out.into_iter().collect()
}

/// What a packet shows, for the not-vacuous row.
fn shown(p: &EvidencePacket, seen: &mut BTreeSet<String>) {
    let mut labels: Vec<String> = [
        (
            "correction",
            p.superseded
                .iter()
                .any(|s| s.by == SupersededBy::Correction),
        ),
        (
            "revision",
            p.superseded.iter().any(|s| s.by == SupersededBy::Revision),
        ),
        ("withdrawn", !p.withdrawn.is_empty()),
        ("unparsed", !p.unparsed.is_empty()),
        ("pending", !p.pending.is_empty()),
        ("expired", !p.expired.is_empty()),
        ("conflict", !p.conflicts.is_empty()),
    ]
    .into_iter()
    .filter(|(_, yes)| *yes)
    .map(|(k, _)| k.to_string())
    .collect();
    labels.extend(
        p.issues
            .iter()
            .map(|i| format!("issue:{}", i.code.as_str())),
    );
    labels.extend(
        p.events
            .iter()
            .map(|e| format!("confidence:{}", e.confidence.as_str())),
    );
    labels.push(format!("demand:{}", p.demand.status.as_str()));
    for f in &p.freshness {
        for (k, yes) in [
            ("gap", f.queries.iter().any(|q| !q.gaps.is_empty())),
            ("failed_fetch", !f.failed.is_empty()),
            ("purge", !f.purges.is_empty()),
        ] {
            if yes {
                labels.push(k.to_string());
            }
        }
    }
    seen.extend(labels);
}

#[test]
fn packets_at_t_ignore_everything_after_t() {
    let mut seen = BTreeSet::new();
    let mut modes_differ = false;
    for w in worlds() {
        let ts = instants(&w);
        assert!(ts.len() > 30, "{}: {} instants", w.name, ts.len());
        let mut distinct = BTreeSet::new();
        for &t in &ts {
            let mut by_mode = Vec::new();
            for mode in MODES {
                let base = w.packet(t, mode);
                shown(&base, &mut seen);
                let base = canon(&base);
                for how in MOVES.into_iter().filter(|m| allowed(mode, *m)) {
                    let again = moved(&w, t, mode, how).packet(t, mode);
                    assert_same(
                        &canon(&again),
                        &base,
                        &format!("{} {mode:?} {how:?} at {} ({t})", w.name, fmt_time(t)),
                    );
                }
                distinct.insert(base.clone());
                by_mode.push(base);
            }
            modes_differ |=
                by_mode[0].replace("\"captured\"", "") != by_mode[1].replace("\"knowable\"", "");
        }
        assert!(
            distinct.len() > 20,
            "{}: only {} distinct packets",
            w.name,
            distinct.len()
        );

        // Boundary: visible from its instant on, not 1 ms before.
        for mode in MODES {
            let input = w.input();
            for (_, versions) in super::asof::visibility(&w.records, &w.policies, mode) {
                for (r, v) in versions {
                    assert!(
                        as_of(&input, v, mode)
                            .visible
                            .contains_key(r.record_id.as_str()),
                        "{} {mode:?}: {} not visible at {v}",
                        w.name,
                        r.record_id
                    );
                    assert!(
                        !as_of(&input, v - 1, mode)
                            .visible
                            .contains_key(r.record_id.as_str()),
                        "{} {mode:?}: {} visible 1 ms early",
                        w.name,
                        r.record_id
                    );
                }
            }
        }
    }
    assert!(modes_differ, "captured and knowable never differ");
    for want in [
        "correction",
        "revision",
        "withdrawn",
        "unparsed",
        "pending",
        "expired",
        "conflict",
        "gap",
        "failed_fetch",
        "purge",
        "demand:single_signal",
        "demand:aggregate",
    ] {
        assert!(seen.contains(want), "no packet shows {want}: {seen:?}");
    }
    for code in [IssueCode::JurisdictionMissing, IssueCode::ListingStale] {
        assert!(
            seen.contains(&format!("issue:{}", code.as_str())),
            "{seen:?}"
        );
    }
    for c in [
        Confidence::Confirmed,
        Confidence::Corroborated,
        Confidence::Single,
        Confidence::TriggerOnly,
        Confidence::None,
    ] {
        assert!(
            seen.contains(&format!("confidence:{}", c.as_str())),
            "{seen:?}"
        );
    }
}

#[test]
fn corrections_after_t_append_and_never_rewrite() {
    for w in worlds() {
        for t in instants(&w).into_iter().step_by(4) {
            let fixes = flood(&w, t, &[Flood::Correction]);
            if fixes.is_empty() {
                continue;
            }
            let mut after = w.clone();
            after.records.extend(fixes.iter().cloned());
            // Appended, never rewritten: the earlier records are byte-identical.
            assert_eq!(&after.records[..w.records.len()], &w.records[..]);
            for mode in MODES {
                assert_same(
                    &canon(&after.packet(t, mode)),
                    &canon(&w.packet(t, mode)),
                    &format!("{} {mode:?} corrections after {}", w.name, fmt_time(t)),
                );
            }
            // From their read on, each correction shows, both ends cited.
            let later = t + 1 + FLOOD_HOURS * H;
            let p = after.packet(later, AsOfMode::Captured);
            let cited: BTreeSet<&str> = p.citations.iter().map(|c| c.record_id.as_str()).collect();
            for fix in &fixes {
                let old = fix.supersedes.as_deref().expect("a correction");
                assert!(
                    p.superseded.iter().any(|s| s.by == SupersededBy::Correction
                        && s.old == old
                        && s.new == fix.record_id),
                    "{}: {} → {} missing at {}",
                    w.name,
                    old,
                    fix.record_id,
                    fmt_time(later)
                );
                assert!(cited.contains(old) && cited.contains(fix.record_id.as_str()));
            }
        }
    }
}

#[test]
fn coverage_and_purges_after_t_change_nothing_at_t() {
    for w in worlds() {
        for t in instants(&w).into_iter().step_by(3) {
            for mode in MODES {
                let base = canon(&w.packet(t, mode));
                for how in [Move::Coverage, Move::Purge] {
                    let m = moved(&w, t, mode, how);
                    assert_same(
                        &canon(&m.packet(t, mode)),
                        &base,
                        &format!("{} {mode:?} {how:?} at {}", w.name, fmt_time(t)),
                    );
                    // Visible from their own instant: the loss and the new reads show.
                    let p = m.packet(t + D, mode);
                    match how {
                        Move::Purge => assert!(
                            p.freshness
                                .iter()
                                .all(|f| f.purges.iter().any(|x| x.purged_ms == t + 1)),
                            "{}: purge after t not shown later",
                            w.name
                        ),
                        _ => assert!(
                            p.freshness
                                .iter()
                                .filter(|f| !f.queries.is_empty())
                                .all(|f| f.queries.iter().all(|q| q.last_fetch_ms == t + D)),
                            "{}: fetches after t not shown later",
                            w.name
                        ),
                    }
                }
            }
        }
    }
}

#[test]
fn knowable_mode_never_reads_in_place_edits_early() {
    for w in worlds() {
        // Also with every source declared immutable (a mis-declared row).
        let mut immutable = w.clone();
        for p in immutable.policies.values_mut() {
            p.revision = Revision::Immutable;
        }
        for world in [&w, &immutable] {
            let mut first: BTreeMap<(&str, &str), &SourceRecord> = BTreeMap::new();
            for r in &world.records {
                let e = first
                    .entry((r.source_id.as_str(), r.native_id.as_str()))
                    .or_insert(r);
                if (r.observed_ms, r.parsed_ms) < (e.observed_ms, e.parsed_ms) {
                    *e = r;
                }
            }
            // Each record id at its earliest read (a re-read of the same
            // content is the same record).
            let mut earliest: BTreeMap<&str, &SourceRecord> = BTreeMap::new();
            for r in &world.records {
                let e = earliest.entry(r.record_id.as_str()).or_insert(r);
                if (r.observed_ms, r.parsed_ms) < (e.observed_ms, e.parsed_ms) {
                    *e = r;
                }
            }
            let input = world.input();
            let mut edits = 0;
            for r in earliest.into_values() {
                let base = first[&(r.source_id.as_str(), r.native_id.as_str())];
                let new_bytes = !r.snapshots.iter().any(|s| base.snapshots.contains(s));
                if !new_bytes || r.record_id == base.record_id {
                    continue;
                }
                edits += 1;
                for t in [
                    r.published_ms,
                    (r.published_ms + r.observed_ms) / 2,
                    r.observed_ms - 1,
                ] {
                    let v = as_of(&input, t, AsOfMode::Knowable);
                    assert!(
                        !v.visible.contains_key(r.record_id.as_str()),
                        "{}: edit {} read at {} visible at {}",
                        world.name,
                        r.record_id,
                        fmt_time(r.observed_ms),
                        fmt_time(t)
                    );
                }
            }
            assert!(
                edits > 0 || world.name == "ted",
                "{}: no edit checked",
                world.name
            );
        }
    }
}
