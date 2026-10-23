//! Decisions per strategy kind — the first step of the backtest engine
//! (`engine::candidates`, `docs/xlab-2026-10-01.md` § 5; the per-kind
//! table is `engine.rs`'s). Pure, and as-of: each decision reads only rows
//! observable at its instant.
//!
//! | Piece | Rule |
//! |---|---|
//! | Window kinds | `weekend_window` (instants from `weekend_fade::fade_window` + offsets) and `daily_window` (local `HH:MM` per day of `days`), each window judged by rule W's `signal_of` + `select_capped`; a window whose anchor, entry and exit are not in that order in UTC is skipped with a note — offsets, or a `daily_window` time in a DST gap (`domain::tz` reads it with the standard offset: New York 02:30 on the spring-forward Sunday is 03:30 EDT, after a 03:00 entry); `data_asof_ms` records the anchor read too |
//! | Bar kinds | `move_trigger` per bar close (cooldown), `funding_carry` per funding row, `pair_spread` per close both legs have, `event_window` per event; `funding_carry` / `pair_spread` hold one position at a time (re-entry after the rule's own exit) |
//! | A candidate | legs, side, signal, exit plan, period; the first leg's features as-of the decision (`features.rs`, with the cost model's half-spread at t); `data_asof_ms` = the latest observation read |
//! | `min_entry_trades` | a would-be candidate whose entry bar (any leg's: the bar ending at the decision, observable then) counts fewer trades is a `thin_entry` skip — windows: before `top_n` ranking (the next liquid name moves up); `move_trigger`: no cooldown starts; `funding_carry` / `pair_spread`: no position opens; a bar without `n` passes |
//! | `labels` (`weekend_window`, `labels.rs`) | after the price, cost and `thin_entry` checks each name is labelled over [anchor − `lookback_mins`, entry] (`MarketData::info`; first bar = the store's, else the series' first); a class in `skip` ⇒ `label_skipped` before `top_n` ranks the rest (the next name moves up); candidates and later skips (`below_min_signal`, `not_top_n`) carry `info_label`; one data note counts the classes (`labels::count_note`) |
//! | Skips | per window and name (window / event kinds), per instrument (excluded, no costs) for the bar kinds; an instrument without bars or funding rows is a data note |
//! | `max_candidates` | a run whose candidates pass `RunParams::max_candidates` stops with an error naming the guard and the kind's knobs to tighten — before features of the rest are computed |

use std::collections::{BTreeMap, BTreeSet};

use chrono::{Datelike, NaiveDate, Weekday};

use crate::domain::backtest::costs::{cost_for, CostSpec};
use crate::domain::backtest::engine::{
    utc_day, Candidate, CandidateSet, ExitPlan, Leg, MarketData, RunParams, Skip, SkipReason,
};
use crate::domain::backtest::features::{features_traced, HOURS_PER_YEAR};
use crate::domain::backtest::fills::{
    apr_pct, ceil_grid, floor_grid, half_spread_bps, ln_bps, side_cost, spread_points,
    walk_funding_exit, walk_spread_exit, HOUR_MS,
};
use crate::domain::backtest::labels::{count_note, InfoLabel, LabelSpec};
use crate::domain::backtest::spec::{
    parse_rfc3339, DailyWindowParams, Days, Direction, EventWindowParams, FundingCarryParams,
    MoveTriggerParams, PairSpreadParams, RankBy, StrategyKind, StrategySpec, Universe,
    WeekendWindowParams,
};
use crate::domain::book::Side;
use crate::domain::calendar::{parse_hm, Calendar, ExchangeCalendar};
use crate::domain::marketdata::{fmt_time, Bar, BarSeries};
use crate::domain::tz::Zone;
use crate::domain::xm::weekend_fade::{
    anchor_date, fade_window, select_capped, signal_of, FadeRule, Signal,
};

const MIN_MS: i64 = 60_000;
const DAY_MS: i64 = 86_400_000;
/// Windows / days one run walks at most (a guard).
const MAX_STEPS: usize = 200_000;

/// The next `minute` (local, `zone`) strictly after `after_ms`.
fn next_local(zone: Zone, after_ms: i64, minute: u32) -> i64 {
    let (h, m) = (minute / 60, minute % 60);
    let day = zone.local_date(after_ms);
    let t = zone.at(day, h, m);
    if t > after_ms {
        return t;
    }
    day.succ_opt().map_or(t + DAY_MS, |d| zone.at(d, h, m))
}

/// A decision before features are attached.
struct Draft {
    legs: Vec<Leg>,
    side: Side,
    signal_bps: f64,
    t: i64,
    period: String,
    anchor_px: Option<f64>,
    exit: ExitPlan,
    label: Option<String>,
    /// A labelled `weekend_window`'s class (`labels.rs`).
    info_label: Option<InfoLabel>,
    /// The latest price / row time read for it.
    used_ms: i64,
}

/// One window's judging inputs (window kinds).
struct WindowRule<'r> {
    direction: Direction,
    min_abs_bps: f64,
    top_n: Option<usize>,
    /// `weekend_window`'s `labels` (module table).
    labels: Option<&'r LabelSpec>,
    /// `weekend_window`'s `stop_loss_bps`: the exit becomes a bar-close stop walk.
    stop_loss_bps: Option<f64>,
    /// `weekend_window`'s `rank_by = "net_of_cost"`.
    net_of_cost: bool,
}

struct Builder<'a> {
    spec: &'a StrategySpec,
    md: &'a MarketData,
    p: &'a RunParams,
    iv: i64,
    excluded: BTreeSet<&'a str>,
    out: CandidateSet,
    /// Names labelled per class (the `labels` data note).
    label_counts: BTreeMap<InfoLabel, usize>,
}

impl<'a> Builder<'a> {
    fn cost(&self, id: &str) -> Option<&'a CostSpec> {
        self.spec
            .costs
            .as_ref()
            .or_else(|| cost_for(&self.p.costs, id))
    }

    /// `rank_by = "net_of_cost"` (module table): the `top_n` names with
    /// \|s\| ≥ `min_abs` by \|s\| − 2 × the side cost at `t` (ties by id); a
    /// name without a cost or bars ranks last.
    fn net_of_cost_top(
        &self,
        signals: &[Signal],
        n: usize,
        min_abs: f64,
        t: i64,
    ) -> BTreeSet<String> {
        let key = |s: &Signal| {
            let cost = self
                .cost(&s.instrument)
                .zip(self.md.bars.get(&s.instrument));
            match cost {
                Some((c, bars)) => {
                    let ctx = self.md.ctx.get(&s.instrument);
                    s.s_bps.abs() - 2.0 * side_cost(c, bars, ctx, t).total_bps()
                }
                None => f64::NEG_INFINITY,
            }
        };
        let mut picks: Vec<(&Signal, f64)> = signals
            .iter()
            .filter(|s| s.s_bps.abs() >= min_abs)
            .map(|s| (s, key(s)))
            .collect();
        picks.sort_by(|a, b| {
            b.1.total_cmp(&a.1)
                .then_with(|| a.0.instrument.cmp(&b.0.instrument))
        });
        picks
            .into_iter()
            .take(n)
            .map(|(s, _)| s.instrument.clone())
            .collect()
    }

    fn is_excluded(&self, id: &str) -> bool {
        self.excluded.contains(id)
    }

    /// `min_entry_trades` (module table): the bar of `id` ending at `t` counts
    /// fewer trades. No such bar, or one without `n`, is not thin (a missing
    /// bar is the price checks' skip).
    fn thin_entry(&self, id: &str, t: i64) -> bool {
        let Some(min) = self.spec.min_entry_trades else {
            return false;
        };
        self.md
            .bars
            .get(id)
            .and_then(|s| s.bar_ending_at(t))
            .and_then(|b| b.n)
            .is_some_and(|n| n < min)
    }

    fn universe(&self) -> Result<Vec<String>, String> {
        let mut ids = if self.p.universe.is_empty() {
            match &self.spec.universe {
                Some(Universe::Ids(ids)) => ids.clone(),
                Some(Universe::Named(text)) => {
                    return Err(format!(
                        "universe {text} is not resolved: pass its ids in RunParams.universe"
                    ))
                }
                None => Vec::new(),
            }
        } else {
            self.p.universe.clone()
        };
        ids.sort();
        ids.dedup();
        if ids.is_empty() {
            return Err("the universe holds no instrument".to_string());
        }
        Ok(ids)
    }

    fn exchange(&self, name: &str) -> Result<&'a ExchangeCalendar, String> {
        self.p
            .calendars
            .get(name)
            .and_then(Calendar::exchange)
            .ok_or_else(|| {
                format!("calendar `{name}` is not an exchange [xmarket.calendars.{name}] row")
            })
    }

    fn skip(&mut self, instrument: &str, decided_at_ms: i64, period: &str, reason: SkipReason) {
        self.skip_labelled(instrument, decided_at_ms, period, reason, None);
    }

    /// A skip after labelling: it carries the name's class.
    fn skip_labelled(
        &mut self,
        instrument: &str,
        decided_at_ms: i64,
        period: &str,
        reason: SkipReason,
        info_label: Option<InfoLabel>,
    ) {
        self.out.skipped.push(Skip {
            instrument: instrument.to_string(),
            decided_at_ms,
            period: period.to_string(),
            reason,
            seq: None,
            info_label,
        });
    }

    /// The class of `id` at `entry` on the window anchored at `anchor`
    /// (`labels.rs`), counted for the data note.
    fn label(&mut self, labels: &LabelSpec, id: &str, anchor: i64, entry: i64) -> InfoLabel {
        let first_bar = self
            .md
            .bars
            .get(id)
            .and_then(|s| s.bars.first())
            .map(|b| b.t_open_ms);
        let l = labels.label_of(&self.md.info, id, anchor, entry, first_bar);
        *self.label_counts.entry(l).or_insert(0) += 1;
        l
    }

    fn note(&mut self, note: String) {
        if !self.out.notes.contains(&note) {
            self.out.notes.push(note);
        }
    }

    /// Attach features and record the decision; `Err` once the run passes
    /// `max_candidates` (module table) — the run stops there.
    fn push(&mut self, d: Draft) -> Result<(), String> {
        if self.out.candidates.len() >= self.p.max_candidates {
            return Err(too_many(self.spec, self.p.max_candidates));
        }
        let first = d.legs[0].instrument.as_str();
        let Some(series) = self.md.bars.get(first) else {
            return Ok(());
        };
        let ctx = self.md.ctx.get(first);
        let half = self
            .cost(first)
            .map(|c| half_spread_bps(&c.half_spread, series, ctx, d.t));
        let traced = features_traced(series, self.md.funding.get(first), ctx, d.t, half);
        let data_asof_ms = traced.asof_ms.map_or(d.used_ms, |a| a.max(d.used_ms));
        let instrument = d
            .legs
            .iter()
            .map(|l| l.instrument.as_str())
            .collect::<Vec<_>>()
            .join("/");
        self.out.candidates.push(Candidate {
            seq: 0,
            instrument,
            side: d.side,
            legs: d.legs,
            signal_bps: d.signal_bps,
            decided_at_ms: d.t,
            data_asof_ms,
            period: d.period,
            anchor_px: d.anchor_px,
            exit: d.exit,
            features: traced.features,
            label: d.label,
            info_label: d.info_label,
        });
        Ok(())
    }

    fn finish(mut self) -> CandidateSet {
        self.out.candidates.sort_by(|a, b| {
            a.decided_at_ms
                .cmp(&b.decided_at_ms)
                .then_with(|| a.instrument.cmp(&b.instrument))
        });
        for (i, c) in self.out.candidates.iter_mut().enumerate() {
            c.seq = i;
        }
        self.out.skipped.sort_by(|a, b| {
            a.decided_at_ms
                .cmp(&b.decided_at_ms)
                .then_with(|| a.instrument.cmp(&b.instrument))
        });
        self.out
    }

    /// Rule W's judging of `ids` over one window (module table); the
    /// caller keeps `anchor < entry < exit` (UTC).
    fn judge_window(
        &mut self,
        ids: &[String],
        period: &str,
        (anchor, entry, exit): (i64, i64, i64),
        rule: &WindowRule,
    ) -> Result<(), String> {
        let mut signals = Vec::new();
        let mut labels: BTreeMap<String, InfoLabel> = BTreeMap::new();
        for id in ids {
            let series = self.md.bars.get(id);
            let signal = signal_of(
                id,
                self.is_excluded(id),
                series.and_then(|s| s.close_at(anchor)),
                series.and_then(|s| s.close_at(entry)),
            );
            match signal {
                Err(skip) => self.skip(id, entry, period, SkipReason::of_fade(skip)),
                Ok(_) if self.cost(id).is_none() => {
                    self.skip(id, entry, period, SkipReason::NoCosts)
                }
                // Before the ranking: a thin name never takes a top_n slot.
                Ok(_) if self.thin_entry(id, entry) => {
                    self.skip(id, entry, period, SkipReason::ThinEntry)
                }
                Ok(s) => {
                    // Labels before the ranking too: a skipped class never
                    // takes a top_n slot.
                    let label = rule.labels.map(|l| (l, self.label(l, id, anchor, entry)));
                    match label {
                        Some((l, class)) if l.skips(class) => self.skip_labelled(
                            id,
                            entry,
                            period,
                            SkipReason::LabelSkipped,
                            Some(class),
                        ),
                        _ => {
                            if let Some((_, class)) = label {
                                labels.insert(id.clone(), class);
                            }
                            signals.push(s)
                        }
                    }
                }
            }
        }
        let chosen: BTreeSet<String> = match rule.top_n {
            Some(n) if rule.net_of_cost => {
                self.net_of_cost_top(&signals, n, rule.min_abs_bps, entry)
            }
            Some(n) => select_capped(
                &signals,
                &FadeRule {
                    capped_top_n: n,
                    min_abs_signal_bps: rule.min_abs_bps,
                },
            )
            .into_iter()
            .collect(),
            None => signals
                .iter()
                .filter(|s| s.s_bps.abs() >= rule.min_abs_bps)
                .map(|s| s.instrument.clone())
                .collect(),
        };
        for s in signals {
            if !chosen.contains(&s.instrument) {
                let reason = if s.s_bps.abs() < rule.min_abs_bps {
                    SkipReason::BelowMinSignal
                } else {
                    SkipReason::NotTopN
                };
                let class = labels.get(&s.instrument).copied();
                self.skip_labelled(&s.instrument, entry, period, reason, class);
                continue;
            }
            let side = match rule.direction {
                Direction::Fade => s.side,
                Direction::Follow => s.side.opposite(),
            };
            self.push(Draft {
                legs: vec![Leg {
                    instrument: s.instrument.clone(),
                    side,
                    entry_px: s.entry_px,
                }],
                side,
                signal_bps: s.s_bps,
                t: entry,
                period: period.to_string(),
                anchor_px: Some(s.anchor_px),
                exit: match rule.stop_loss_bps {
                    Some(sl) => ExitPlan::Bars {
                        max_exit_ms: exit,
                        take_profit_bps: None,
                        stop_loss_bps: Some(sl),
                    },
                    None => ExitPlan::At { exit_ms: exit },
                },
                label: None,
                info_label: labels.get(&s.instrument).copied(),
                // Both reads: the anchor's close and the entry's (a label
                // reads events up to the entry, never past it).
                used_ms: anchor.max(entry),
            })?;
        }
        Ok(())
    }

    fn weekend(&mut self, p: &WeekendWindowParams) -> Result<(), String> {
        let cal = self.exchange(&p.calendar)?;
        let ids = self.universe()?;
        let (from, to) = (self.p.from_ms, self.p.to_ms);
        let rule = WindowRule {
            direction: p.direction,
            min_abs_bps: p.min_abs_signal_bps,
            top_n: p.top_n,
            labels: p.labels.as_ref(),
            stop_loss_bps: p.stop_loss_bps,
            net_of_cost: p.rank_by == Some(RankBy::NetOfCost),
        };
        let mut t = from;
        for _ in 0..MAX_STEPS {
            let Some(w) = fade_window(cal, t) else {
                break;
            };
            t = w.exit_ms;
            let anchor = w.anchor_ms + p.anchor_offset_mins * MIN_MS;
            let entry = w.entry_ms + p.entry_offset_mins * MIN_MS;
            let exit = w.exit_ms + p.exit_offset_mins * MIN_MS;
            if entry >= to {
                break;
            }
            if entry < from {
                continue;
            }
            let period = anchor_date(&w);
            if !(anchor < entry && entry < exit) {
                self.note(format!(
                    "window {period}: the offsets put anchor, entry and exit out of order — skipped"
                ));
                continue;
            }
            self.judge_window(&ids, &period, (anchor, entry, exit), &rule)?;
        }
        if let Some(labels) = &p.labels {
            self.note(count_note(labels, &self.label_counts));
        }
        Ok(())
    }

    fn daily(&mut self, p: &DailyWindowParams) -> Result<(), String> {
        let zone = Zone::parse(&p.tz).ok_or_else(|| format!("tz `{}` is not supported", p.tz))?;
        let cal = match p.days {
            Days::Trading => Some(self.exchange(p.calendar.as_deref().unwrap_or_default())?),
            Days::All | Days::Weekdays => None,
        };
        let ids = self.universe()?;
        let hm = |s: &str| parse_hm(s).ok_or_else(|| format!("`{s}` is not HH:MM"));
        let (am, em, xm) = (hm(&p.anchor)?, hm(&p.entry)?, hm(&p.exit)?);
        let at = |d: NaiveDate, m: u32| zone.at(d, m / 60, m % 60);
        let trades_on = |d: NaiveDate| match (p.days, cal) {
            (Days::All, _) => true,
            (Days::Weekdays, _) => !matches!(d.weekday(), Weekday::Sat | Weekday::Sun),
            (Days::Trading, c) => c.is_some_and(|c| c.is_trading_day(d)),
        };
        let prev = |d: NaiveDate| cal.map_or_else(|| d.pred_opt(), |c| c.prev_trading_day(d));
        let next = |d: NaiveDate| cal.map_or_else(|| d.succ_opt(), |c| c.next_trading_day(d));
        let rule = WindowRule {
            direction: p.direction,
            min_abs_bps: p.min_abs_signal_bps,
            top_n: p.top_n,
            labels: None,
            stop_loss_bps: None,
            net_of_cost: false,
        };
        let (from, to) = (self.p.from_ms, self.p.to_ms);
        let (Some(mut d), Some(last)) = (
            zone.local_date(from).pred_opt(),
            zone.local_date(to).succ_opt(),
        ) else {
            return Ok(());
        };
        for _ in 0..MAX_STEPS {
            if d > last {
                break;
            }
            let entry = at(d, em);
            if trades_on(d) && (from..to).contains(&entry) {
                let anchor_day = if am < em { Some(d) } else { prev(d) };
                let exit_day = if xm > em { Some(d) } else { next(d) };
                if let (Some(ad), Some(xd)) = (anchor_day, exit_day) {
                    let period = d.format("%Y-%m-%d").to_string();
                    let (anchor, exit) = (at(ad, am), at(xd, xm));
                    // A wall time in a DST gap reads with the standard
                    // offset (`domain::tz`): the anchor can land after the
                    // entry (New York 02:30 → 03:30 EDT on the spring-forward
                    // Sunday) — reading it would read the future.
                    if anchor < entry && entry < exit {
                        self.judge_window(&ids, &period, (anchor, entry, exit), &rule)?;
                    } else {
                        self.note(format!(
                            "day {period}: anchor {}, entry {} and exit {} are out of order in UTC \
                             (a DST switch in {}) — skipped",
                            fmt_time(anchor),
                            fmt_time(entry),
                            fmt_time(exit),
                            zone.name()
                        ));
                    }
                }
            }
            let Some(n) = d.succ_opt() else {
                break;
            };
            d = n;
        }
        Ok(())
    }

    /// Per-instrument checks of the bar kinds: excluded, costs, bars.
    fn tradable(&mut self, id: &str) -> Option<&'a BarSeries> {
        let from = self.p.from_ms;
        if self.is_excluded(id) {
            self.skip(id, from, "", SkipReason::Excluded);
            return None;
        }
        if self.cost(id).is_none() {
            self.skip(id, from, "", SkipReason::NoCosts);
            return None;
        }
        let series = self.md.bars.get(id);
        if series.is_none() {
            self.note(format!("no {} bars for {id}", self.spec.interval));
        }
        series
    }

    fn move_trigger(&mut self, p: &MoveTriggerParams) -> Result<(), String> {
        let ids = self.universe()?;
        let (iv, from, to) = (self.iv, self.p.from_ms, self.p.to_ms);
        let lookback = i64::from(p.lookback_bars) * iv;
        let cooldown = i64::from(p.cooldown()) * iv;
        for id in &ids {
            let Some(series) = self.tradable(id) else {
                continue;
            };
            let bars = &series.bars;
            let start = bars.partition_point(|b| b.t_open_ms + iv < from);
            let mut last: Option<i64> = None;
            for i in start..bars.len() {
                let b = bars[i];
                let t = b.t_open_ms + iv;
                if t >= to {
                    break;
                }
                if last.is_some_and(|l| t - l < cooldown) {
                    continue;
                }
                let Ok(j) =
                    bars[..i].binary_search_by_key(&(b.t_open_ms - lookback), |x| x.t_open_ms)
                else {
                    continue;
                };
                let Some(r) = ln_bps(b.c, bars[j].c) else {
                    continue;
                };
                if r.abs() < p.threshold_bps {
                    continue;
                }
                if let (Some(ratio), Some(base)) = (p.min_volume_ratio, p.volume_baseline_bars) {
                    if !volume_ok(bars, i, iv, p.lookback_bars, base, ratio) {
                        continue;
                    }
                }
                let Some(side) = p.direction.side(r) else {
                    continue;
                };
                // A thin trigger is skipped and starts no cooldown.
                if self.thin_entry(id, t) {
                    self.skip(id, t, &utc_day(t), SkipReason::ThinEntry);
                    continue;
                }
                last = Some(t);
                self.push(Draft {
                    legs: vec![Leg {
                        instrument: id.clone(),
                        side,
                        entry_px: b.c,
                    }],
                    side,
                    signal_bps: r,
                    t,
                    period: utc_day(t),
                    anchor_px: Some(bars[j].c),
                    exit: ExitPlan::Bars {
                        max_exit_ms: t + i64::from(p.hold_bars) * iv,
                        take_profit_bps: p.take_profit_bps,
                        stop_loss_bps: p.stop_loss_bps,
                    },
                    label: None,
                    info_label: None,
                    used_ms: t,
                })?;
            }
        }
        Ok(())
    }

    fn funding_carry(&mut self, p: &FundingCarryParams) -> Result<(), String> {
        let ids = self.universe()?;
        let (iv, from, to) = (self.iv, self.p.from_ms, self.p.to_ms);
        for id in &ids {
            let series = self.tradable(id);
            if self.is_excluded(id) || self.cost(id).is_none() {
                continue;
            }
            let Some(f) = self.md.funding.get(id) else {
                self.note(format!("no funding rows for {id}"));
                continue;
            };
            let mut open_until = i64::MIN;
            for pt in &f.points {
                let d = ceil_grid(pt.t_ms, iv);
                if d < from || d < open_until {
                    continue;
                }
                if d >= to {
                    break;
                }
                if !(apr_pct(pt.rate_1h) >= p.min_apr_pct) {
                    continue;
                }
                let side = if pt.rate_1h > 0.0 {
                    Side::Sell
                } else {
                    Side::Buy
                };
                let Some(px) = series.and_then(|s| s.close_at(d)) else {
                    self.skip(id, d, &utc_day(d), SkipReason::MissingPrice);
                    continue;
                };
                if self.thin_entry(id, d) {
                    self.skip(id, d, &utc_day(d), SkipReason::ThinEntry);
                    continue;
                }
                let max_exit = ceil_grid(d + i64::from(p.hold_hours) * HOUR_MS, iv);
                open_until = walk_funding_exit(f, d, max_exit, p.exit_apr_pct, iv).0;
                self.push(Draft {
                    legs: vec![Leg {
                        instrument: id.clone(),
                        side,
                        entry_px: px,
                    }],
                    side,
                    signal_bps: pt.rate_1h * HOURS_PER_YEAR * 10_000.0,
                    t: d,
                    period: utc_day(d),
                    anchor_px: None,
                    exit: ExitPlan::Funding {
                        max_exit_ms: max_exit,
                        exit_apr_pct: p.exit_apr_pct,
                    },
                    label: None,
                    info_label: None,
                    used_ms: d.max(pt.t_ms),
                })?;
            }
        }
        Ok(())
    }

    fn pair(&mut self, p: &PairSpreadParams) -> Result<(), String> {
        let [a, b] = p.legs.as_slice() else {
            return Err(format!(
                "pair_spread has {} legs; a pair has 2",
                p.legs.len()
            ));
        };
        let key = format!("{a}/{b}");
        let (iv, from, to) = (self.iv, self.p.from_ms, self.p.to_ms);
        if self.is_excluded(a) || self.is_excluded(b) {
            self.skip(&key, from, "", SkipReason::Excluded);
            return Ok(());
        }
        if self.cost(a).is_none() || self.cost(b).is_none() {
            self.skip(&key, from, "", SkipReason::NoCosts);
            return Ok(());
        }
        let (Some(sa), Some(sb)) = (self.md.bars.get(a), self.md.bars.get(b)) else {
            self.note(format!(
                "no {} bars for both legs of {key}",
                self.spec.interval
            ));
            return Ok(());
        };
        let pts = spread_points(sa, sb, p.lookback_bars as usize);
        let mut open_until = i64::MIN;
        for (k, pt) in pts.iter().enumerate() {
            if pt.t_ms < from || pt.t_ms < open_until {
                continue;
            }
            if pt.t_ms >= to {
                break;
            }
            let (Some(z), Some(mean)) = (pt.z, pt.mean) else {
                continue;
            };
            if z.abs() < p.entry_z {
                continue;
            }
            if self.thin_entry(a, pt.t_ms) || self.thin_entry(b, pt.t_ms) {
                self.skip(&key, pt.t_ms, &utc_day(pt.t_ms), SkipReason::ThinEntry);
                continue;
            }
            let side = if z > 0.0 { Side::Sell } else { Side::Buy };
            let max_exit = pt.t_ms + i64::from(p.max_hold_bars) * iv;
            open_until = walk_spread_exit(&pts[k + 1..], pt.t_ms, max_exit, p.exit_z).0;
            self.push(Draft {
                legs: vec![
                    Leg {
                        instrument: a.clone(),
                        side,
                        entry_px: pt.a_px,
                    },
                    Leg {
                        instrument: b.clone(),
                        side: side.opposite(),
                        entry_px: pt.b_px,
                    },
                ],
                side,
                signal_bps: (pt.s - mean) * 10_000.0,
                t: pt.t_ms,
                period: utc_day(pt.t_ms),
                anchor_px: None,
                exit: ExitPlan::Spread {
                    max_exit_ms: max_exit,
                    exit_z: p.exit_z,
                },
                label: None,
                info_label: None,
                used_ms: pt.t_ms,
            })?;
        }
        Ok(())
    }

    fn events(&mut self, p: &EventWindowParams) -> Result<(), String> {
        let (iv, from, to) = (self.iv, self.p.from_ms, self.p.to_ms);
        let exit_at = match (&p.exit_at, &p.tz) {
            (Some(hm), Some(tz)) => Some((
                parse_hm(hm).ok_or_else(|| format!("exit_at `{hm}` is not HH:MM"))?,
                Zone::parse(tz).ok_or_else(|| format!("tz `{tz}` is not supported"))?,
            )),
            _ => None,
        };
        let mut events: Vec<(i64, usize)> = p
            .events
            .iter()
            .enumerate()
            .filter_map(|(i, e)| parse_rfc3339(&e.t).map(|t| (t, i)))
            .collect();
        events.sort_by(|x, y| {
            x.0.cmp(&y.0)
                .then_with(|| p.events[x.1].instrument.cmp(&p.events[y.1].instrument))
        });
        for (t_ev, i) in events {
            let ev = &p.events[i];
            let id = ev.instrument.as_str();
            let entry = ceil_grid(t_ev + i64::from(p.entry_delay_mins) * MIN_MS, iv);
            if entry < from || entry >= to {
                continue;
            }
            let period = utc_day(entry);
            if self.is_excluded(id) {
                self.skip(id, entry, &period, SkipReason::Excluded);
                continue;
            }
            if self.cost(id).is_none() {
                self.skip(id, entry, &period, SkipReason::NoCosts);
                continue;
            }
            let series = self.md.bars.get(id);
            let Some(anchor_px) = series.and_then(|s| s.close_at(floor_grid(t_ev, iv))) else {
                self.skip(id, entry, &period, SkipReason::MissingAnchor);
                continue;
            };
            let Some(px) = series.and_then(|s| s.close_at(entry)) else {
                self.skip(id, entry, &period, SkipReason::MissingEntry);
                continue;
            };
            let Some(side) = ln_bps(px, anchor_px).and_then(|mv| p.direction.side(mv)) else {
                self.skip(id, entry, &period, SkipReason::Flat);
                continue;
            };
            let mv = ln_bps(px, anchor_px).unwrap_or(0.0);
            if mv.abs() < p.min_abs_move_bps {
                self.skip(id, entry, &period, SkipReason::BelowMinSignal);
                continue;
            }
            if self.thin_entry(id, entry) {
                self.skip(id, entry, &period, SkipReason::ThinEntry);
                continue;
            }
            let exit = match (p.exit_after_mins, exit_at) {
                (Some(m), _) => ceil_grid(entry + i64::from(m) * MIN_MS, iv),
                (None, Some((m, zone))) => ceil_grid(next_local(zone, entry, m), iv),
                (None, None) => {
                    return Err("event_window needs exit_after_mins or exit_at + tz".to_string())
                }
            };
            self.push(Draft {
                legs: vec![Leg {
                    instrument: id.to_string(),
                    side,
                    entry_px: px,
                }],
                side,
                signal_bps: mv,
                t: entry,
                period,
                anchor_px: Some(anchor_px),
                exit: ExitPlan::At { exit_ms: exit },
                label: ev.label.clone(),
                info_label: None,
                used_ms: entry,
            })?;
        }
        Ok(())
    }
}

/// The `max_candidates` error (module table): the cap and how to narrow
/// the spec of this kind.
fn too_many(spec: &StrategySpec, max: usize) -> String {
    let narrow = match &spec.kind {
        StrategyKind::WeekendWindow(_) | StrategyKind::DailyWindow(_) => {
            "raise min_abs_signal_bps or set top_n"
        }
        StrategyKind::MoveTrigger(_) => {
            "raise threshold_bps or cooldown_bars, or add min_volume_ratio"
        }
        StrategyKind::FundingCarry(_) => "raise min_apr_pct",
        StrategyKind::PairSpread(_) => "raise entry_z",
        StrategyKind::EventWindow(_) => "list fewer events",
    };
    format!(
        "more than {max} candidates (the [backtest] max_candidates guard) — nothing was \
         simulated or written: {narrow}, narrow the universe or from / to, or raise [backtest] \
         max_candidates"
    )
}

/// `move_trigger`'s volume filter (module table): bars opening in the
/// lookback vs the baseline before it; missing bars count 0.
fn volume_ok(
    bars: &[Bar],
    i: usize,
    iv: i64,
    lookback_bars: u32,
    baseline_bars: u32,
    ratio: f64,
) -> bool {
    let t = bars[i].t_open_ms;
    let lb_start = t - (i64::from(lookback_bars) - 1) * iv;
    let base_start = lb_start - i64::from(baseline_bars) * iv;
    if bars.first().is_none_or(|b| b.t_open_ms > base_start) {
        return false;
    }
    // Volume of the bars opening in [lo, hi).
    let vol = |lo: i64, hi: i64| -> f64 {
        let a = bars.partition_point(|b| b.t_open_ms < lo);
        let z = bars.partition_point(|b| b.t_open_ms < hi);
        bars[a..z.max(a)].iter().map(|b| b.v).sum()
    };
    let recent = vol(lb_start, t + 1);
    let expected = vol(base_start, lb_start) / f64::from(baseline_bars) * f64::from(lookback_bars);
    expected > 0.0 && recent >= ratio * expected
}

/// The decisions of `spec` over `[params.from_ms, params.to_ms)` —
/// `engine::candidates` after its checks.
pub(crate) fn decide(
    spec: &StrategySpec,
    md: &MarketData,
    params: &RunParams,
) -> Result<CandidateSet, String> {
    let mut b = Builder {
        spec,
        md,
        p: params,
        iv: spec.interval.ms(),
        excluded: spec.exclude.iter().map(String::as_str).collect(),
        out: CandidateSet::default(),
        label_counts: BTreeMap::new(),
    };
    match &spec.kind {
        StrategyKind::WeekendWindow(p) => b.weekend(p)?,
        StrategyKind::DailyWindow(p) => b.daily(p)?,
        StrategyKind::MoveTrigger(p) => b.move_trigger(p)?,
        StrategyKind::FundingCarry(p) => b.funding_carry(p)?,
        StrategyKind::PairSpread(p) => b.pair(p)?,
        StrategyKind::EventWindow(p) => b.events(p)?,
    }
    Ok(b.finish())
}

#[cfg(test)]
#[path = "kinds_tests.rs"]
mod tests;
