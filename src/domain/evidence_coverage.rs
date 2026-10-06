//! Evidence coverage (`tengu evidence coverage`, `docs/lineage-2026-10-06.md`
//! § 3): which cadence slots of a window a recorded stream holds, its gaps,
//! and which gaps a second source (backfilled 1 m bars) covers — every
//! interval labelled [`Provenance`] `LIVE_RECORDED` / `BACKFILLED` /
//! `MISSING`. Pure: instants and bars are inputs (`application/evidence.rs`
//! reads them from recorder day files and `market.db`, read-only).
//!
//! | Piece | Rule |
//! |---|---|
//! | Slots ([`Window`]) | `[k·c, (k+1)·c)` on the epoch grid of cadence `c`, every slot inside `[from, to)` (a slot cut by either end is left out) |
//! | Covered | a key covers a slot when one of its rows with status `ok` / `partial` was observed in it; `absent` / `error` rows cover nothing |
//! | Key | `never_covered` = rows in the window but no covered slot (an `absent` instrument); else its gaps = maximal runs of uncovered slots |
//! | Sweep gap | a run of slots no key covers (the recorder recorded nothing of the stream) |
//! | Second source ([`Bars`]) | per key, bar open times (+ trades `n`); a gap minute `[m, m + bar)` is `BACKFILLED` when the key has a bar opening at `m`, else `MISSING`; `n = 0` bars are counted `flat` (a stale price, not an observation) |
//! | Timeline | runs of slots with a sweep → `LIVE_RECORDED`; each sweep gap → `BACKFILLED` for the keys whose every gap minute has a bar, `MISSING` for the rest (a key with only some minutes counts `MISSING`, its backfilled minutes in the note) |
//! | Keys | full observation keys; a bar's key = the observation key's instrument (`mkt_ctx/1:<instrument>`) |

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::domain::evidence::Provenance;

/// The slot grid of a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Window {
    pub from_ms: i64,
    pub to_ms: i64,
    pub cadence_ms: i64,
}

impl Window {
    pub fn new(from_ms: i64, to_ms: i64, cadence_ms: i64) -> Result<Self, String> {
        if cadence_ms <= 0 {
            return Err(format!("cadence {cadence_ms} ms is not > 0"));
        }
        if to_ms <= from_ms {
            return Err(format!("window to {to_ms} is not after from {from_ms}"));
        }
        let w = Self {
            from_ms,
            to_ms,
            cadence_ms,
        };
        if w.n_slots() == 0 {
            return Err("the window holds no whole slot".into());
        }
        Ok(w)
    }

    /// Start of the first slot (the module table).
    pub fn first_slot_ms(&self) -> i64 {
        let c = self.cadence_ms;
        self.from_ms.div_euclid(c) * c
            + if self.from_ms.rem_euclid(c) == 0 {
                0
            } else {
                c
            }
    }

    pub fn n_slots(&self) -> usize {
        let first = self.first_slot_ms();
        if first + self.cadence_ms > self.to_ms {
            return 0;
        }
        ((self.to_ms - first) / self.cadence_ms) as usize
    }

    /// The slot holding `t_ms`, when it is one of the window's.
    pub fn slot_of(&self, t_ms: i64) -> Option<usize> {
        let first = self.first_slot_ms();
        if t_ms < first {
            return None;
        }
        let i = ((t_ms - first) / self.cadence_ms) as usize;
        (i < self.n_slots()).then_some(i)
    }

    pub fn slot_start(&self, i: usize) -> i64 {
        self.first_slot_ms() + i as i64 * self.cadence_ms
    }
}

/// One key's covered instants (rows `ok` / `partial`) and whether it had
/// any row at all in the window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stream {
    pub key: String,
    /// `observed_at_ms` of the key's `ok` / `partial` rows.
    pub covered_ms: Vec<i64>,
    /// Rows of any status in the window.
    pub rows: usize,
}

/// One backfilled bar's open time and trade count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bar {
    pub t_open_ms: i64,
    /// `None` when the source does not say.
    pub trades: Option<i64>,
}

/// The second source: bars by instrument (module table).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Bars {
    /// Bar length, ms (1 m = 60 000).
    pub bar_ms: i64,
    /// Short name of the source, e.g. `market.db 1m`.
    pub source: String,
    pub by_instrument: BTreeMap<String, Vec<Bar>>,
}

/// The instrument of an observation key: what follows `<schema>:`, where
/// the schema is `<name>/<version>`.
pub fn key_instrument(key: &str) -> &str {
    match key.find('/') {
        Some(slash) => match key[slash..].find(':') {
            Some(colon) => &key[slash + colon + 1..],
            None => key,
        },
        None => key,
    }
}

/// A run of slots `[from_ms, to_ms)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Gap {
    pub from_ms: i64,
    pub to_ms: i64,
    pub slots: usize,
}

/// How the second source covers one key over one gap.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GapFill {
    pub minutes: usize,
    pub backfilled: usize,
    /// Backfilled minutes whose bar had no trade.
    pub flat: usize,
}

impl GapFill {
    pub fn label(&self) -> Provenance {
        if self.minutes > 0 && self.backfilled == self.minutes {
            Provenance::Backfilled
        } else {
            Provenance::Missing
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeyGap {
    #[serde(flatten)]
    pub gap: Gap,
    pub label: Provenance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill: Option<GapFill>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeyCoverage {
    pub key: String,
    pub covered_slots: usize,
    /// Gaps that are not sweep gaps (the key missing while others were
    /// recorded).
    pub own_gaps: Vec<KeyGap>,
}

/// A slot run the recorder recorded nothing of the stream in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SweepGap {
    #[serde(flatten)]
    pub gap: Gap,
    /// Live keys (≥ 1 covered slot) whose every gap minute has a bar.
    pub backfilled_keys: usize,
    /// Live keys with no bar for some gap minute, full ids.
    pub missing_keys: Vec<String>,
    /// Bars found over the gap, and those with no trade.
    pub bars: usize,
    pub flat_bars: usize,
}

/// One labelled stretch of the window (module table: Timeline).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Interval {
    pub from_ms: i64,
    pub to_ms: i64,
    pub label: Provenance,
    /// Keys the label holds for.
    pub keys: usize,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

/// The coverage of one stream set over a window (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Coverage {
    pub window: Window,
    pub slots: usize,
    /// Slots at least one key covers.
    pub swept_slots: usize,
    pub keys: usize,
    pub live_keys: usize,
    /// Keys with rows in the window but no covered slot.
    pub never_covered: Vec<String>,
    /// Live keys covering every swept slot.
    pub complete_keys: usize,
    pub sweep_gaps: Vec<SweepGap>,
    pub per_key: Vec<KeyCoverage>,
    pub timeline: Vec<Interval>,
    /// The second source, when one was given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub second_source: Option<String>,
}

fn runs(uncovered: impl Iterator<Item = (usize, bool)>) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    let mut last = 0;
    for (i, gap) in uncovered {
        last = i;
        match (gap, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                out.push((s, i));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        out.push((s, last + 1));
    }
    out
}

/// The second source over `[from, to)` for one instrument.
pub fn gap_fill(bars: &Bars, instrument: &str, from_ms: i64, to_ms: i64) -> GapFill {
    let step = bars.bar_ms.max(1);
    let have: BTreeMap<i64, Option<i64>> = bars
        .by_instrument
        .get(instrument)
        .map(|v| v.iter().map(|b| (b.t_open_ms, b.trades)).collect())
        .unwrap_or_default();
    let (mut minutes, mut backfilled, mut flat) = (0, 0, 0);
    let mut m = from_ms.div_euclid(step) * step;
    while m < to_ms {
        minutes += 1;
        if let Some(trades) = have.get(&m) {
            backfilled += 1;
            flat += usize::from(*trades == Some(0));
        }
        m += step;
    }
    GapFill {
        minutes,
        backfilled,
        flat,
    }
}

/// The coverage of `streams` over `window`, gaps labelled by `bars` when
/// given (module table).
pub fn coverage(window: &Window, streams: &[Stream], bars: Option<&Bars>) -> Coverage {
    let n = window.n_slots();
    let mut swept = vec![false; n];
    let mut per_key_slots: Vec<(String, Vec<bool>)> = Vec::new();
    let mut never_covered = Vec::new();
    for s in streams {
        let mut cov = vec![false; n];
        for t in &s.covered_ms {
            if let Some(i) = window.slot_of(*t) {
                cov[i] = true;
                swept[i] = true;
            }
        }
        if cov.iter().any(|c| *c) {
            per_key_slots.push((s.key.clone(), cov));
        } else if s.rows > 0 || !s.covered_ms.is_empty() {
            never_covered.push(s.key.clone());
        }
    }
    per_key_slots.sort_by(|a, b| a.0.cmp(&b.0));
    never_covered.sort();
    let gap_of = |(a, b): (usize, usize)| Gap {
        from_ms: window.slot_start(a),
        to_ms: window.slot_start(b),
        slots: b - a,
    };
    let sweep_runs = runs(swept.iter().enumerate().map(|(i, s)| (i, !*s)));
    let mut sweep_gaps = Vec::new();
    for r in &sweep_runs {
        let gap = gap_of(*r);
        let (mut backfilled, mut missing, mut nbars, mut flat) = (0, Vec::new(), 0, 0);
        if let Some(b) = bars {
            for (key, _) in &per_key_slots {
                let f = gap_fill(b, key_instrument(key), gap.from_ms, gap.to_ms);
                nbars += f.backfilled;
                flat += f.flat;
                match f.label() {
                    Provenance::Backfilled => backfilled += 1,
                    _ => missing.push(key.clone()),
                }
            }
        } else {
            missing = per_key_slots.iter().map(|(k, _)| k.clone()).collect();
        }
        sweep_gaps.push(SweepGap {
            gap,
            backfilled_keys: backfilled,
            missing_keys: missing,
            bars: nbars,
            flat_bars: flat,
        });
    }
    let sweep_slot: BTreeSet<usize> = sweep_runs.iter().flat_map(|(a, b)| *a..*b).collect();
    let mut per_key = Vec::new();
    let mut complete_keys = 0;
    for (key, cov) in &per_key_slots {
        let own = runs(
            cov.iter()
                .enumerate()
                .map(|(i, c)| (i, !*c && !sweep_slot.contains(&i))),
        );
        if own.is_empty() {
            complete_keys += 1;
        }
        let own_gaps = own
            .into_iter()
            .map(|r| {
                let gap = gap_of(r);
                let fill = bars.map(|b| gap_fill(b, key_instrument(key), gap.from_ms, gap.to_ms));
                KeyGap {
                    label: fill.as_ref().map_or(Provenance::Missing, GapFill::label),
                    gap,
                    fill,
                }
            })
            .collect();
        per_key.push(KeyCoverage {
            key: key.clone(),
            covered_slots: cov.iter().filter(|c| **c).count(),
            own_gaps,
        });
    }
    // Timeline.
    let live = per_key_slots.len();
    let mut timeline = Vec::new();
    let mut cursor = 0usize;
    let push_live = |a: usize, b: usize, timeline: &mut Vec<Interval>| {
        if b > a {
            timeline.push(Interval {
                from_ms: window.slot_start(a),
                to_ms: window.slot_start(b),
                label: Provenance::LiveRecorded,
                keys: live,
                note: String::new(),
            });
        }
    };
    for (r, g) in sweep_runs.iter().zip(&sweep_gaps) {
        push_live(cursor, r.0, &mut timeline);
        let source = bars.map_or("no second source".to_string(), |b| b.source.clone());
        if g.backfilled_keys > 0 {
            timeline.push(Interval {
                from_ms: g.gap.from_ms,
                to_ms: g.gap.to_ms,
                label: Provenance::Backfilled,
                keys: g.backfilled_keys,
                note: format!("{source}: {} bars, {} flat (n = 0)", g.bars, g.flat_bars),
            });
        }
        if !g.missing_keys.is_empty() {
            timeline.push(Interval {
                from_ms: g.gap.from_ms,
                to_ms: g.gap.to_ms,
                label: Provenance::Missing,
                keys: g.missing_keys.len(),
                note: if bars.is_some() {
                    format!("no {source} bar for every minute")
                } else {
                    source
                },
            });
        }
        cursor = r.1;
    }
    push_live(cursor, n, &mut timeline);
    Coverage {
        window: *window,
        slots: n,
        swept_slots: swept.iter().filter(|s| **s).count(),
        keys: per_key_slots.len() + never_covered.len(),
        live_keys: live,
        never_covered,
        complete_keys,
        sweep_gaps,
        per_key,
        timeline,
        second_source: bars.map(|b| b.source.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: i64 = 60_000;

    fn stream(key: &str, at: &[i64]) -> Stream {
        Stream {
            key: key.into(),
            covered_ms: at.to_vec(),
            rows: at.len(),
        }
    }

    #[test]
    fn slots_sit_on_the_epoch_grid_inside_the_window() {
        let w = Window::new(10 * M + 55_000, 20 * M + 30_000, M).unwrap();
        assert_eq!(w.first_slot_ms(), 11 * M);
        assert_eq!(w.n_slots(), 9); // 11..20, the cut 20:00–20:30 left out
        assert_eq!(w.slot_of(11 * M + 5), Some(0));
        assert_eq!(w.slot_of(10 * M + 59_000), None);
        assert_eq!(w.slot_of(20 * M), None);
        assert!(Window::new(0, M - 1, M).is_err());
        assert!(Window::new(0, M, 0).is_err());
        assert_eq!(
            key_instrument("mkt_ctx/1:hyperliquid:xyz:CBRS"),
            "hyperliquid:xyz:CBRS"
        );
    }

    #[test]
    fn sweep_gaps_key_gaps_and_the_second_source() {
        let w = Window::new(0, 10 * M, M).unwrap();
        // Both keys skip minutes 3–4 (a sweep gap); B also skips 7; C is absent.
        let a: Vec<i64> = [0, 1, 2, 5, 6, 7, 8, 9]
            .iter()
            .map(|m| m * M + 2_000)
            .collect();
        let b: Vec<i64> = [0, 1, 2, 5, 6, 8, 9]
            .iter()
            .map(|m| m * M + 3_000)
            .collect();
        let streams = vec![
            stream("mkt_ctx/1:x:A", &a),
            stream("mkt_ctx/1:x:B", &b),
            Stream {
                key: "mkt_ctx/1:x:C".into(),
                covered_ms: vec![],
                rows: 10,
            },
        ];
        let mut bars = Bars {
            bar_ms: M,
            source: "market.db 1m".into(),
            by_instrument: BTreeMap::new(),
        };
        bars.by_instrument.insert(
            "x:A".into(),
            (0..10)
                .map(|m| Bar {
                    t_open_ms: m * M,
                    trades: Some(if m == 3 { 0 } else { 4 }),
                })
                .collect(),
        );
        bars.by_instrument.insert(
            "x:B".into(),
            vec![Bar {
                t_open_ms: 3 * M,
                trades: Some(1),
            }],
        );
        let c = coverage(&w, &streams, Some(&bars));
        assert_eq!((c.slots, c.swept_slots, c.keys, c.live_keys), (10, 8, 3, 2));
        assert_eq!(c.never_covered, vec!["mkt_ctx/1:x:C".to_string()]);
        assert_eq!(c.sweep_gaps.len(), 1);
        let g = &c.sweep_gaps[0];
        assert_eq!((g.gap.from_ms, g.gap.to_ms, g.gap.slots), (3 * M, 5 * M, 2));
        assert_eq!(g.backfilled_keys, 1);
        assert_eq!(g.missing_keys, vec!["mkt_ctx/1:x:B".to_string()]);
        assert_eq!((g.bars, g.flat_bars), (3, 1));
        assert_eq!(c.complete_keys, 1);
        let kb = c.per_key.iter().find(|k| k.key.ends_with(":B")).unwrap();
        assert_eq!(kb.own_gaps.len(), 1);
        assert_eq!(kb.own_gaps[0].gap.from_ms, 7 * M);
        assert_eq!(kb.own_gaps[0].label, Provenance::Missing);
        let labels: Vec<(i64, Provenance, usize)> = c
            .timeline
            .iter()
            .map(|i| (i.from_ms / M, i.label, i.keys))
            .collect();
        assert_eq!(
            labels,
            vec![
                (0, Provenance::LiveRecorded, 2),
                (3, Provenance::Backfilled, 1),
                (3, Provenance::Missing, 1),
                (5, Provenance::LiveRecorded, 2),
            ]
        );
        // Without a second source every gap is MISSING.
        let bare = coverage(&w, &streams, None);
        assert_eq!(bare.timeline[1].label, Provenance::Missing);
        assert_eq!(bare.timeline[1].keys, 2);
    }

    #[test]
    fn a_trailing_gap_closes_at_the_window_end() {
        let w = Window::new(0, 5 * 300_000, 300_000).unwrap();
        let c = coverage(&w, &[stream("hl_book/1:x:A", &[1_000, 300_500])], None);
        assert_eq!(c.sweep_gaps.len(), 1);
        assert_eq!(c.sweep_gaps[0].gap.from_ms, 600_000);
        assert_eq!(c.sweep_gaps[0].gap.to_ms, 1_500_000);
        assert_eq!(c.sweep_gaps[0].gap.slots, 3);
    }
}
