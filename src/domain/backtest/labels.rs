//! Point-in-time information labels of `weekend_window` candidates (roadmap
//! Phase 7, `docs/xlab-2026-10-01.md` § 5–6): was there news about the name
//! between the weekend anchor and the decision, or is its move noise? Pure;
//! the application loads [`InfoData`] (`market.db` events + event coverage,
//! the earliest stored bar, `[backtest.splits]`) only for a spec with
//! `labels`, and `kinds.rs` labels each name before `top_n` ranking.
//!
//! | `labels` field ([`LabelSpec`], `deny_unknown_fields`) | Default | Bounds / rule |
//! |---|---|---|
//! | `skip` | `[]` (label only) | classes whose names are skipped (`label_skipped`, the class in the skip) before `top_n` ranks the rest; distinct, never all three |
//! | `lookback_mins` | `240` | 0..=10080: the window opens this long before the anchor. 240 reaches back from rule W's Fri 20:00 New York anchor to the 16:00 close, so releases after the close (earnings 8-Ks at 16:00–16:30), which the after-hours and weekend perps then price, fall inside |
//! | `new_listing_days` | `14` | 0..=365 (0 = off): a name whose first stored bar opens less than this before the decision is UNCERTAIN — two weekly windows: with at most one earlier weekend on the venue its move is price discovery, not noise around a settled price |
//! | `forms` | every filing | 1..=32 distinct forms (≤ 16 chars) as the source writes them (SEC: `8-K`, `10-Q/A`): only those filings are material; the SEC backfill keeps 8-K, 6-K, 10-Q, 10-K, 20-F, 40-F and their /A. `split` / `listing` events are always material |
//!
//! | Class ([`InfoLabel`]) | When (window = [anchor − `lookback_mins`, decision], both ends in) |
//! |---|---|
//! | `NEWS` | a material event of the name published inside the window (`published_ms` ≤ the decision: observable at its publication, never after the decision), or a `[backtest.splits]` split of it inside the window |
//! | `UNCERTAIN` | no NEWS, and no single `EventCoverage` row with `covered = true` spans the window (`from_ms` ≤ its start, `to_ms` > the decision — coverage is `[from, to)`): silence there is no evidence; or the name is a new listing (`new_listing_days`) |
//! | `NOISE` | covered over the whole window, no material event, not a new listing |
//!
//! | Rule | Value |
//! |---|---|
//! | Precedence | NEWS > UNCERTAIN > NOISE: an event the store holds is a fact whatever the coverage; only without one does the silence's quality decide, and NOISE needs it trusted |
//! | First bar ([`InfoData::listed_ms`]) | the earliest stored bar at any interval (store coverage, `t_open_ms`), else the run's first loaded bar; the earlier of the two. HL keeps the newest 5 000 bars per interval — a name stored only at 1h (≈ 208 days) reads as listed at that horizon: UNCERTAIN near it, never NOISE (backfill 1d bars to reach the listing) |
//! | `--data-through` ([`InfoData::cut_after`]) | events published after the bound cut like rows (counted), coverage `to_ms` clipped to the bound + 1 ms and a row starting after it dropped, a first bar opening after it dropped; splits are config, kept |
//! | Jev gate | the label stays out of the gate event (`gate::gate_event`) for now: the gate sees what it saw before Phase 7 |
//! | Report | every candidate of a labelled spec carries `info_label` (`candidates.jsonl`; the older `label` field is `event_window`'s operator text); a skip after labelling (`label_skipped`, `below_min_signal`, `not_top_n`) and an arm's drop of a labelled candidate carry its class; skip counts key `label_skipped:<CLASS>`; one data note counts the classes ([`count_note`]) |

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::domain::marketdata::{EventCoverage, MarketEvent, StockSplit};

/// Window minutes before the anchor (module table).
pub const DEFAULT_LOOKBACK_MINS: u32 = 240;
/// New-listing days (module table).
pub const DEFAULT_NEW_LISTING_DAYS: u32 = 14;
const MAX_LOOKBACK_MINS: u32 = 10_080;
const MAX_NEW_LISTING_DAYS: u32 = 365;
const MAX_FORMS: usize = 32;
const MAX_FORM_CHARS: usize = 16;
const MIN_MS: i64 = 60_000;
const DAY_MS: i64 = 86_400_000;

/// A candidate's information class (module table); wire `NEWS` ·
/// `UNCERTAIN` · `NOISE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum InfoLabel {
    News,
    Uncertain,
    Noise,
}

impl InfoLabel {
    /// In precedence order.
    pub const ALL: [InfoLabel; 3] = [InfoLabel::News, InfoLabel::Uncertain, InfoLabel::Noise];

    pub fn as_str(self) -> &'static str {
        match self {
            InfoLabel::News => "NEWS",
            InfoLabel::Uncertain => "UNCERTAIN",
            InfoLabel::Noise => "NOISE",
        }
    }
}

fn default_lookback_mins() -> u32 {
    DEFAULT_LOOKBACK_MINS
}

fn default_new_listing_days() -> u32 {
    DEFAULT_NEW_LISTING_DAYS
}

/// `weekend_window`'s `labels` table (module table); absent ⇒ no label,
/// and the spec's canonical JSON (its `spec_sha256`) is what it was before
/// Phase 7.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LabelSpec {
    #[serde(default)]
    pub skip: Vec<InfoLabel>,
    #[serde(default = "default_lookback_mins")]
    pub lookback_mins: u32,
    #[serde(default = "default_new_listing_days")]
    pub new_listing_days: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forms: Option<Vec<String>>,
}

impl LabelSpec {
    /// Every problem, each naming its field (`labels.<field>`).
    pub fn validation_errors(&self) -> Vec<String> {
        let mut e = Vec::new();
        let mut seen = Vec::new();
        for l in &self.skip {
            if seen.contains(l) {
                e.push(format!("labels.skip: `{}` is listed twice", l.as_str()));
            } else {
                seen.push(*l);
            }
        }
        if InfoLabel::ALL.iter().all(|l| seen.contains(l)) {
            e.push("labels.skip names every class: no candidate could trade".to_string());
        }
        if self.lookback_mins > MAX_LOOKBACK_MINS {
            e.push(format!(
                "labels.lookback_mins must be within 0..={MAX_LOOKBACK_MINS}"
            ));
        }
        if self.new_listing_days > MAX_NEW_LISTING_DAYS {
            e.push(format!(
                "labels.new_listing_days must be within 0..={MAX_NEW_LISTING_DAYS} (0 = off)"
            ));
        }
        if let Some(forms) = &self.forms {
            if forms.is_empty() || forms.len() > MAX_FORMS {
                e.push(format!(
                    "labels.forms lists {}; give 1..={MAX_FORMS} (leave it out for every filing)",
                    forms.len()
                ));
            }
            let mut seen = Vec::new();
            for f in forms {
                if f.trim().is_empty() || f.trim() != f || f.chars().count() > MAX_FORM_CHARS {
                    e.push(format!(
                        "labels.forms: `{f}` is not a form (1-{MAX_FORM_CHARS} chars, no surrounding space)"
                    ));
                } else if seen.contains(&f) {
                    e.push(format!("labels.forms: `{f}` is listed twice"));
                } else {
                    seen.push(f);
                }
            }
        }
        e
    }

    /// Names of this class are skipped.
    pub fn skips(&self, label: InfoLabel) -> bool {
        self.skip.contains(&label)
    }

    pub fn lookback_ms(&self) -> i64 {
        i64::from(self.lookback_mins) * MIN_MS
    }

    /// `split` / `listing` always; a filing when `forms` lists its form (or
    /// is absent).
    fn material(&self, e: &MarketEvent) -> bool {
        match e.kind.as_str() {
            "filing" => self
                .forms
                .as_ref()
                .is_none_or(|forms| forms.iter().any(|f| *f == e.form)),
            _ => true,
        }
    }

    /// The class of `id` decided at `t_ms` on the window anchored at
    /// `anchor_ms` (module tables); `first_bar_ms` = the run's first loaded
    /// bar of `id` (the fallback of [`InfoData::listed_ms`]).
    pub fn label_of(
        &self,
        info: &InfoData,
        id: &str,
        anchor_ms: i64,
        t_ms: i64,
        first_bar_ms: Option<i64>,
    ) -> InfoLabel {
        let lo = anchor_ms.saturating_sub(self.lookback_ms());
        let inside = |p: i64| lo <= p && p <= t_ms;
        let event = info.events.get(id).is_some_and(|evs| {
            evs.iter()
                .any(|e| inside(e.published_ms) && self.material(e))
        });
        let split = info
            .splits
            .get(id)
            .is_some_and(|ss| ss.iter().any(|s| inside(s.at_ms)));
        if event || split {
            return InfoLabel::News;
        }
        let covered = info.coverage.get(id).is_some_and(|cs| {
            cs.iter()
                .any(|c| c.covered && c.from_ms <= lo && t_ms < c.to_ms)
        });
        let listed = match (info.listed_ms.get(id).copied(), first_bar_ms) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let since = t_ms.saturating_sub(i64::from(self.new_listing_days) * DAY_MS);
        let new_listing = self.new_listing_days > 0 && listed.is_none_or(|l| l > since);
        if !covered || new_listing {
            InfoLabel::Uncertain
        } else {
            InfoLabel::Noise
        }
    }
}

/// What the labels read (module tables), by full instrument id; empty
/// unless the spec has `labels`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InfoData {
    /// Events ascending by `published_ms` (`MarketDataStore::events`).
    pub events: BTreeMap<String, Vec<MarketEvent>>,
    /// Every source's coverage (`MarketDataStore::event_coverage`).
    pub coverage: BTreeMap<String, Vec<EventCoverage>>,
    /// The earliest stored bar's open, any interval (store coverage).
    pub listed_ms: BTreeMap<String, i64>,
    /// `[backtest.splits]`.
    pub splits: BTreeMap<String, Vec<StockSplit>>,
}

impl InfoData {
    /// The newest event loaded (an observation, at its publication).
    pub fn newest_ms(&self) -> Option<i64> {
        self.events
            .values()
            .flat_map(|evs| evs.iter().map(|e| e.published_ms))
            .max()
    }

    /// Keep what was known at `through_ms` (module table); returns the
    /// events removed.
    pub fn cut_after(&mut self, through_ms: i64) -> usize {
        let mut removed = 0;
        for evs in self.events.values_mut() {
            let before = evs.len();
            evs.retain(|e| e.published_ms <= through_ms);
            removed += before - evs.len();
        }
        self.events.retain(|_, evs| !evs.is_empty());
        let end = through_ms.saturating_add(1);
        for cs in self.coverage.values_mut() {
            cs.retain(|c| c.from_ms <= through_ms);
            for c in cs.iter_mut() {
                c.to_ms = c.to_ms.min(end);
            }
        }
        self.coverage.retain(|_, cs| !cs.is_empty());
        self.listed_ms.retain(|_, t| *t <= through_ms);
        removed
    }
}

/// The run's label data note: the classes counted over every name labelled
/// (candidates and the skips after labelling), the knobs, what was skipped.
pub fn count_note(spec: &LabelSpec, counts: &BTreeMap<InfoLabel, usize>) -> String {
    let n = |l: InfoLabel| counts.get(&l).copied().unwrap_or(0);
    let classes: Vec<String> = InfoLabel::ALL
        .iter()
        .map(|l| format!("{} {}", l.as_str(), n(*l)))
        .collect();
    let forms = spec
        .forms
        .as_ref()
        .map_or_else(|| "every filing".to_string(), |f| f.join(", "));
    let listing = if spec.new_listing_days == 0 {
        "new listings off".to_string()
    } else {
        format!("new listing < {} d", spec.new_listing_days)
    };
    let skipped = if spec.skip.is_empty() {
        "nothing skipped".to_string()
    } else {
        let parts: Vec<String> = InfoLabel::ALL
            .iter()
            .filter(|l| spec.skips(**l))
            .map(|l| format!("{} {}", l.as_str(), n(*l)))
            .collect();
        format!("label_skipped: {}", parts.join(", "))
    };
    format!(
        "labels: {} (window anchor − {} min → decision; {listing}; {forms}) — {skipped}",
        classes.join(" · "),
        spec.lookback_mins
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const A: &str = "hyperliquid:xyz:AAA";
    const ANCHOR: i64 = 1_790_000_000_000;
    const T: i64 = ANCHOR + 46 * 3_600_000;

    fn spec(v: serde_json::Value) -> LabelSpec {
        serde_json::from_value(v).unwrap()
    }

    fn filing(published_ms: i64, form: &str) -> MarketEvent {
        MarketEvent {
            instrument: A.into(),
            published_ms,
            kind: "filing".into(),
            id: format!("0000320193-26-{published_ms}"),
            form: form.into(),
            title: None,
        }
    }

    fn cov(from_ms: i64, to_ms: i64, covered: bool) -> EventCoverage {
        EventCoverage {
            instrument: A.into(),
            source: "sec".into(),
            from_ms,
            to_ms,
            covered,
            note: None,
            fetched_at_ms: to_ms,
        }
    }

    /// Covered from long before to long after, listed a year earlier.
    fn covered() -> InfoData {
        InfoData {
            coverage: BTreeMap::from([(A.into(), vec![cov(0, T + DAY_MS, true)])]),
            listed_ms: BTreeMap::from([(A.into(), T - 365 * DAY_MS)]),
            ..Default::default()
        }
    }

    fn with_events(mut info: InfoData, evs: Vec<MarketEvent>) -> InfoData {
        info.events.insert(A.into(), evs);
        info
    }

    #[test]
    fn defaults_fill_and_absent_fields_stay_out() {
        let s = spec(json!({}));
        assert_eq!(
            (
                s.skip.clone(),
                s.lookback_mins,
                s.new_listing_days,
                s.forms.clone()
            ),
            (vec![], 240, 14, None)
        );
        assert_eq!(
            serde_json::to_value(&s).unwrap(),
            json!({"skip": [], "lookback_mins": 240, "new_listing_days": 14})
        );
        let s = spec(json!({"skip": ["NEWS", "UNCERTAIN"], "forms": ["8-K"]}));
        assert!(s.skips(InfoLabel::News) && s.skips(InfoLabel::Uncertain));
        assert!(!s.skips(InfoLabel::Noise));
        assert!(serde_json::from_value::<LabelSpec>(json!({"skip": ["news"]})).is_err());
        let e = serde_json::from_value::<LabelSpec>(json!({"lookback": 5}))
            .unwrap_err()
            .to_string();
        assert!(e.contains("unknown field `lookback`"), "{e}");
    }

    #[test]
    fn bounds_name_their_field() {
        let e = spec(json!({"skip": ["NEWS", "NEWS", "UNCERTAIN", "NOISE"],
            "lookback_mins": 10_081, "new_listing_days": 366,
            "forms": ["8-K", "8-K", " 10-Q", "", "X".repeat(17)]}))
        .validation_errors()
        .join("\n");
        for want in [
            "labels.skip: `NEWS` is listed twice",
            "labels.skip names every class",
            "labels.lookback_mins must be within 0..=10080",
            "labels.new_listing_days must be within 0..=365 (0 = off)",
            "labels.forms: `8-K` is listed twice",
            "labels.forms: ` 10-Q` is not a form",
            "labels.forms: `` is not a form",
            "labels.forms: `XXXXXXXXXXXXXXXXX` is not a form",
        ] {
            assert!(e.contains(want), "want `{want}`:\n{e}");
        }
        let e = spec(json!({"forms": []})).validation_errors().join("\n");
        assert!(e.contains("labels.forms lists 0; give 1..=32"), "{e}");
        assert!(
            spec(json!({"skip": ["NEWS", "UNCERTAIN"], "lookback_mins": 0,
            "new_listing_days": 0}))
            .validation_errors()
            .is_empty()
        );
    }

    /// NEWS > UNCERTAIN > NOISE.
    #[test]
    fn precedence_news_then_uncertain_then_noise() {
        let s = spec(json!({}));
        let news = |info: InfoData| with_events(info, vec![filing(ANCHOR, "8-K")]);
        assert_eq!(s.label_of(&covered(), A, ANCHOR, T, None), InfoLabel::Noise);
        assert_eq!(
            s.label_of(&news(covered()), A, ANCHOR, T, None),
            InfoLabel::News
        );
        // No coverage: an event is still NEWS, its absence UNCERTAIN.
        let bare = InfoData::default();
        assert_eq!(
            s.label_of(&news(bare.clone()), A, ANCHOR, T, Some(0)),
            InfoLabel::News
        );
        assert_eq!(
            s.label_of(&bare, A, ANCHOR, T, Some(0)),
            InfoLabel::Uncertain
        );
        // A new listing: UNCERTAIN unless an event says NEWS.
        let mut fresh = covered();
        fresh.listed_ms.insert(A.into(), T - 13 * DAY_MS);
        assert_eq!(s.label_of(&fresh, A, ANCHOR, T, None), InfoLabel::Uncertain);
        assert_eq!(
            s.label_of(&news(fresh), A, ANCHOR, T, None),
            InfoLabel::News
        );
    }

    /// The window is [anchor − lookback, decision], both ends in.
    #[test]
    fn window_bounds_are_inclusive_and_never_pass_the_decision() {
        let s = spec(json!({"lookback_mins": 240}));
        let lo = ANCHOR - 240 * MIN_MS;
        let at = |p: i64| {
            s.label_of(
                &with_events(covered(), vec![filing(p, "8-K")]),
                A,
                ANCHOR,
                T,
                None,
            )
        };
        assert_eq!(at(lo), InfoLabel::News);
        assert_eq!(at(lo - 1), InfoLabel::Noise);
        assert_eq!(at(T), InfoLabel::News, "observable at its publication");
        assert_eq!(at(T + 1), InfoLabel::Noise, "after the decision");
        // lookback 0: the window opens at the anchor.
        let s0 = spec(json!({"lookback_mins": 0}));
        let ev = with_events(covered(), vec![filing(ANCHOR - 1, "8-K")]);
        assert_eq!(s0.label_of(&ev, A, ANCHOR, T, None), InfoLabel::Noise);
    }

    /// One covered row must span the window: a gap, a late start, an end at
    /// the decision ([from, to)), an uncovered row or two half rows are
    /// UNCERTAIN.
    #[test]
    fn coverage_gaps_are_uncertain() {
        let s = spec(json!({}));
        let lo = ANCHOR - 240 * MIN_MS;
        let with = |rows: Vec<EventCoverage>| {
            let mut info = covered();
            info.coverage.insert(A.into(), rows);
            s.label_of(&info, A, ANCHOR, T, None)
        };
        assert_eq!(with(vec![cov(lo, T + 1, true)]), InfoLabel::Noise);
        assert_eq!(with(vec![cov(lo + 1, T + 1, true)]), InfoLabel::Uncertain);
        assert_eq!(with(vec![cov(lo, T, true)]), InfoLabel::Uncertain);
        assert_eq!(with(vec![cov(0, T + DAY_MS, false)]), InfoLabel::Uncertain);
        let mut other = cov(ANCHOR, T + 1, true);
        other.source = "other".into();
        assert_eq!(
            with(vec![cov(lo, ANCHOR, true), other]),
            InfoLabel::Uncertain
        );
        assert_eq!(with(vec![]), InfoLabel::Uncertain);
    }

    /// The first stored bar (else the first loaded one, the earlier of the
    /// two) newer than `new_listing_days` before the decision; 0 = off.
    #[test]
    fn a_new_listing_is_uncertain() {
        let s = spec(json!({"new_listing_days": 14}));
        let mut info = covered();
        info.listed_ms.clear();
        let first = |t: Option<i64>| s.label_of(&info, A, ANCHOR, T, t);
        assert_eq!(first(Some(T - 14 * DAY_MS)), InfoLabel::Noise);
        assert_eq!(first(Some(T - 14 * DAY_MS + 1)), InfoLabel::Uncertain);
        assert_eq!(first(None), InfoLabel::Uncertain, "no bar known");
        info.listed_ms.insert(A.into(), T - 30 * DAY_MS);
        assert_eq!(
            s.label_of(&info, A, ANCHOR, T, Some(T - DAY_MS)),
            InfoLabel::Noise,
            "the store's earlier bar wins"
        );
        let off = spec(json!({"new_listing_days": 0}));
        info.listed_ms.clear();
        assert_eq!(off.label_of(&info, A, ANCHOR, T, None), InfoLabel::Noise);
    }

    /// `forms` filters filings only; a split event or a configured split in
    /// the window is NEWS.
    #[test]
    fn forms_filter_filings_and_splits_are_news() {
        let only_8k = spec(json!({"forms": ["8-K", "8-K/A"]}));
        let ev = |form: &str| with_events(covered(), vec![filing(ANCHOR, form)]);
        assert_eq!(
            only_8k.label_of(&ev("10-Q"), A, ANCHOR, T, None),
            InfoLabel::Noise
        );
        assert_eq!(
            only_8k.label_of(&ev("8-K/A"), A, ANCHOR, T, None),
            InfoLabel::News
        );
        assert_eq!(
            spec(json!({})).label_of(&ev("10-Q"), A, ANCHOR, T, None),
            InfoLabel::News
        );
        let mut split_event = filing(ANCHOR, "split");
        split_event.kind = "split".into();
        let info = with_events(covered(), vec![split_event]);
        assert_eq!(only_8k.label_of(&info, A, ANCHOR, T, None), InfoLabel::News);
        let mut info = covered();
        info.splits.insert(
            A.into(),
            vec![StockSplit {
                at_ms: T,
                ratio: 3.0,
            }],
        );
        assert_eq!(only_8k.label_of(&info, A, ANCHOR, T, None), InfoLabel::News);
        info.splits.get_mut(A).unwrap()[0].at_ms = T + 1;
        assert_eq!(
            only_8k.label_of(&info, A, ANCHOR, T, None),
            InfoLabel::Noise,
            "a split after the decision is not in the window"
        );
    }

    /// `--data-through`: events after the bound cut and counted, coverage
    /// clipped, a later first bar dropped.
    #[test]
    fn cut_after_keeps_what_was_known_then() {
        let mut info = with_events(
            covered(),
            vec![filing(T - 5, "8-K"), filing(T, "8-K"), filing(T + 1, "8-K")],
        );
        info.coverage
            .get_mut(A)
            .unwrap()
            .push(cov(T + 10, T + DAY_MS, true));
        info.listed_ms.insert("hyperliquid:xyz:NEW".into(), T + 1);
        assert_eq!(info.newest_ms(), Some(T + 1));
        assert_eq!(info.cut_after(T), 1);
        assert_eq!(info.events[A].len(), 2);
        let mut clipped = cov(0, T + DAY_MS, true);
        clipped.to_ms = T + 1;
        assert_eq!(
            info.coverage[A],
            vec![clipped],
            "clipped; the later row gone"
        );
        assert!(!info.listed_ms.contains_key("hyperliquid:xyz:NEW"));
        assert_eq!(info.newest_ms(), Some(T));
        // Still covered at the bound itself.
        let s = spec(json!({}));
        let quiet = InfoData {
            events: BTreeMap::new(),
            ..info
        };
        assert_eq!(s.label_of(&quiet, A, ANCHOR, T, None), InfoLabel::Noise);
        assert_eq!(InfoData::default().newest_ms(), None);
    }

    #[test]
    fn the_count_note_names_classes_knobs_and_skips() {
        let counts = BTreeMap::from([(InfoLabel::News, 3), (InfoLabel::Noise, 5)]);
        assert_eq!(
            count_note(&spec(json!({"skip": ["NEWS", "UNCERTAIN"]})), &counts),
            "labels: NEWS 3 · UNCERTAIN 0 · NOISE 5 (window anchor − 240 min → decision; \
             new listing < 14 d; every filing) — label_skipped: NEWS 3, UNCERTAIN 0"
        );
        assert_eq!(
            count_note(
                &spec(
                    json!({"new_listing_days": 0, "forms": ["8-K", "10-Q"], "lookback_mins": 60})
                ),
                &counts
            ),
            "labels: NEWS 3 · UNCERTAIN 0 · NOISE 5 (window anchor − 60 min → decision; \
             new listings off; 8-K, 10-Q) — nothing skipped"
        );
    }
}
