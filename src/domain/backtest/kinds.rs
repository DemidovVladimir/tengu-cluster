//! Decisions per strategy kind — the first step of the backtest engine
//! (`engine::candidates`, `docs/xlab-2026-10-01.md` § 5; the per-kind
//! table is `engine.rs`'s). Pure, and as-of: each decision reads only rows
//! observable at its instant.
//!
//! | Piece | Rule |
//! |---|---|
//! | Window kinds | `weekend_window` (instants from `weekend_fade::fade_window` + offsets) and `daily_window` (local `HH:MM` per day of `days`), each window judged by rule W's `signal_of` + `select_capped` |
//! | Bar kinds | `move_trigger` per bar close (cooldown), `funding_carry` per funding row, `pair_spread` per close both legs have, `event_window` per event; `funding_carry` / `pair_spread` hold one position at a time (re-entry after the rule's own exit) |
//! | A candidate | legs, side, signal, exit plan, period; the first leg's features as-of the decision (`features.rs`, with the cost model's half-spread at t); `data_asof_ms` = the latest observation read |
//! | Skips | per window and name (window / event kinds), per instrument (excluded, no costs) for the bar kinds; an instrument without bars or funding rows is a data note |

use std::collections::BTreeSet;

use chrono::{Datelike, NaiveDate, Weekday};

use crate::domain::backtest::costs::{cost_for, CostSpec};
use crate::domain::backtest::engine::{
    utc_day, Candidate, CandidateSet, ExitPlan, Leg, MarketData, RunParams, Skip, SkipReason,
};
use crate::domain::backtest::features::{features_traced, HOURS_PER_YEAR};
use crate::domain::backtest::fills::{
    apr_pct, ceil_grid, floor_grid, half_spread_bps, ln_bps, spread_points, walk_funding_exit,
    walk_spread_exit, HOUR_MS,
};
use crate::domain::backtest::spec::{
    parse_rfc3339, DailyWindowParams, Days, Direction, EventWindowParams, FundingCarryParams,
    MoveTriggerParams, PairSpreadParams, StrategyKind, StrategySpec, Universe, WeekendWindowParams,
};
use crate::domain::book::Side;
use crate::domain::calendar::{parse_hm, Calendar, ExchangeCalendar};
use crate::domain::marketdata::{Bar, BarSeries};
use crate::domain::tz::Zone;
use crate::domain::xm::weekend_fade::{
    anchor_date, fade_window, select_capped, signal_of, FadeRule,
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
    /// The latest price / row time read for it.
    used_ms: i64,
}

/// One window's judging inputs (window kinds).
struct WindowRule {
    direction: Direction,
    min_abs_bps: f64,
    top_n: Option<usize>,
}

struct Builder<'a> {
    spec: &'a StrategySpec,
    md: &'a MarketData,
    p: &'a RunParams,
    iv: i64,
    excluded: BTreeSet<&'a str>,
    out: CandidateSet,
}

impl<'a> Builder<'a> {
    fn cost(&self, id: &str) -> Option<&'a CostSpec> {
        self.spec
            .costs
            .as_ref()
            .or_else(|| cost_for(&self.p.costs, id))
    }

    fn is_excluded(&self, id: &str) -> bool {
        self.excluded.contains(id)
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
        self.out.skipped.push(Skip {
            instrument: instrument.to_string(),
            decided_at_ms,
            period: period.to_string(),
            reason,
        });
    }

    fn note(&mut self, note: String) {
        if !self.out.notes.contains(&note) {
            self.out.notes.push(note);
        }
    }

    fn push(&mut self, d: Draft) {
        let first = d.legs[0].instrument.as_str();
        let Some(series) = self.md.bars.get(first) else {
            return;
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
        });
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

    /// Rule W's judging of `ids` over one window (module table).
    fn judge_window(
        &mut self,
        ids: &[String],
        period: &str,
        (anchor, entry, exit): (i64, i64, i64),
        rule: &WindowRule,
    ) {
        let mut signals = Vec::new();
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
                Ok(s) => signals.push(s),
            }
        }
        let chosen: BTreeSet<String> = match rule.top_n {
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
                self.skip(&s.instrument, entry, period, reason);
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
                exit: ExitPlan::At { exit_ms: exit },
                label: None,
                used_ms: entry,
            });
        }
    }

    fn weekend(&mut self, p: &WeekendWindowParams) -> Result<(), String> {
        let cal = self.exchange(&p.calendar)?;
        let ids = self.universe()?;
        let (from, to) = (self.p.from_ms, self.p.to_ms);
        let rule = WindowRule {
            direction: p.direction,
            min_abs_bps: p.min_abs_signal_bps,
            top_n: p.top_n,
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
            self.judge_window(&ids, &period, (anchor, entry, exit), &rule);
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
                    self.judge_window(&ids, &period, (at(ad, am), entry, at(xd, xm)), &rule);
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
                    used_ms: t,
                });
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
                    used_ms: d.max(pt.t_ms),
                });
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
                used_ms: pt.t_ms,
            });
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
                used_ms: entry,
            });
        }
        Ok(())
    }
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
mod tests {
    use serde_json::json;

    use super::*;
    use crate::domain::backtest::engine::{candidates, simulate, Arm};
    use crate::domain::backtest::fills::ExitReason;
    use crate::domain::backtest::testkit::{
        et, funding, market, run_params, series, sparse, spec, utc, H,
    };
    use crate::domain::marketdata::Interval;

    const A: &str = "hyperliquid:xyz:AAA";
    const B: &str = "hyperliquid:xyz:BBB";
    const C: &str = "hyperliquid:xyz:CCC";
    const D: &str = "hyperliquid:xyz:DDD";

    fn bps(x: f64) -> f64 {
        x.ln() * 10_000.0
    }

    /// The 2026-09-26 → 28 window on 1 h bars: A rose (faded short), B fell
    /// (faded long), C excluded, D without bars.
    fn weekend_case(extra: serde_json::Value) -> (StrategySpec, MarketData, RunParams) {
        let (anchor, entry, exit) = (
            utc("2026-09-26 00:00"),
            utc("2026-09-27 22:00"),
            utc("2026-09-28 13:00"),
        );
        let px = |a: f64, e: f64, x: f64| [(anchor - H, a), (entry - H, e), (exit - H, x)];
        let md = market(vec![
            sparse(A, Interval::H1, &px(100.0, 102.0, 101.0)),
            sparse(B, Interval::H1, &px(50.0, 49.5, 50.0)),
            sparse(C, Interval::H1, &px(10.0, 11.0, 10.0)),
        ]);
        let mut v = json!({"kind": "weekend_window", "universe": [A, B, C, D], "interval": "1h",
            "calendar": "us_equity", "direction": "fade", "exclude": [C], "notional_usd": 250,
            "costs": {"taker_fee_bps": 2, "half_spread": {"model": "fixed", "bps": 1}, "slippage_bps": 0.5, "funding": false}});
        for (k, x) in extra.as_object().unwrap() {
            v[k] = x.clone();
        }
        (
            spec(v),
            md,
            run_params(utc("2026-09-21 00:00"), utc("2026-09-29 00:00")),
        )
    }

    #[test]
    fn weekend_window_trades_by_hand() {
        let (s, md, p) = weekend_case(json!({}));
        let set = candidates(&s, &md, &p).unwrap();
        let entry = utc("2026-09-27 22:00");
        assert_eq!(set.candidates.len(), 2);
        let (a, b) = (&set.candidates[0], &set.candidates[1]);
        assert_eq!((a.instrument.as_str(), a.side, a.seq), (A, Side::Sell, 0));
        assert_eq!((b.instrument.as_str(), b.side, b.seq), (B, Side::Buy, 1));
        assert_eq!(a.signal_bps, (102.0f64 / 100.0).ln() * 10_000.0);
        assert_eq!((a.decided_at_ms, a.data_asof_ms), (entry, entry));
        assert_eq!(a.period, "2026-09-25");
        assert_eq!(a.anchor_px, Some(100.0));
        assert_eq!(
            a.exit,
            ExitPlan::At {
                exit_ms: utc("2026-09-28 13:00")
            }
        );
        assert_eq!(a.features["half_spread_bps"], 1.0);
        let skips: Vec<(&str, SkipReason)> = set
            .skipped
            .iter()
            .map(|k| (k.instrument.as_str(), k.reason))
            .collect();
        assert_eq!(
            skips,
            vec![(C, SkipReason::Excluded), (D, SkipReason::MissingAnchor)]
        );
        assert_eq!(set.skip_counts()["excluded"], 1);

        let r = simulate(&s, &md, &p, &set.candidates, Arm::Research);
        assert_eq!(r.trades.len(), 2);
        let ta = &r.trades[0];
        assert_eq!(ta.gross_bps, -1.0 * (101.0f64 / 102.0).ln() * 10_000.0);
        assert_eq!(
            (ta.fee_bps, ta.spread_bps, ta.slippage_bps),
            (4.0, 2.0, 1.0)
        );
        assert!((ta.net_bps - (ta.gross_bps - 7.0)).abs() < 1e-12);
        assert_eq!(ta.notional_usd, 250.0);
        assert!((ta.net_usd - ta.net_bps * 250.0 / 10_000.0).abs() < 1e-12);
        assert_eq!((ta.funding_bps, ta.funding_complete), (0.0, true));
        assert_eq!(
            (ta.entry_ms, ta.exit_ms, ta.exit_reason),
            (entry, utc("2026-09-28 13:00"), ExitReason::Window)
        );
        assert_eq!((ta.legs[0].entry_px, ta.legs[0].exit_px), (102.0, 101.0));
        let tb = &r.trades[1];
        assert!((tb.gross_bps - bps(50.0 / 49.5)).abs() < 1e-9);
        assert_eq!(r.summary.n, 2);
        assert_eq!(r.summary.n_periods, 1);
        assert_eq!(r.summary.periods_per_year, 52.0);

        // follow flips the sides; top_n / min keep the larger move.
        let (s, md, p) = weekend_case(json!({"direction": "follow"}));
        let set = candidates(&s, &md, &p).unwrap();
        assert_eq!(
            (set.candidates[0].side, set.candidates[1].side),
            (Side::Buy, Side::Sell)
        );
        for extra in [json!({"top_n": 1}), json!({"min_abs_signal_bps": 150})] {
            let (s, md, p) = weekend_case(extra.clone());
            let set = candidates(&s, &md, &p).unwrap();
            assert_eq!(set.candidates.len(), 1, "{extra}");
            assert_eq!(set.candidates[0].instrument, A);
            let why = set
                .skipped
                .iter()
                .find(|k| k.instrument == B)
                .unwrap()
                .reason;
            let want = if extra.get("top_n").is_some() {
                SkipReason::NotTopN
            } else {
                SkipReason::BelowMinSignal
            };
            assert_eq!(why, want);
        }
        // Offsets move the instants (whole bars); the bar there is missing.
        let (s, md, p) = weekend_case(json!({"entry_offset_mins": 60}));
        let set = candidates(&s, &md, &p).unwrap();
        assert!(set.candidates.is_empty());
        assert!(set
            .skipped
            .iter()
            .any(|k| k.reason == SkipReason::MissingEntry && k.decided_at_ms == entry + H));
    }

    #[test]
    fn daily_window_instants_and_trades() {
        // Weekdays: Mon 2026-09-28 20:00 EDT, anchor 16:00 the same day,
        // exit 10:00 the next day.
        let md = market(vec![sparse(
            A,
            Interval::H1,
            &[
                (et("2026-09-28 15:00"), 100.0),
                (et("2026-09-28 19:00"), 103.0),
                (et("2026-09-29 09:00"), 104.0),
            ],
        )]);
        let s = spec(
            json!({"kind": "daily_window", "universe": [A], "interval": "1h", "days": "weekdays",
            "tz": "America/New_York", "anchor": "16:00", "entry": "20:00", "exit": "10:00", "direction": "follow"}),
        );
        let p = run_params(utc("2026-09-28 12:00"), utc("2026-09-29 12:00"));
        let set = candidates(&s, &md, &p).unwrap();
        assert_eq!(set.candidates.len(), 1, "{:?}", set.skipped);
        let c = &set.candidates[0];
        assert_eq!(
            (c.side, c.decided_at_ms, c.period.as_str()),
            (Side::Buy, et("2026-09-28 20:00"), "2026-09-28")
        );
        assert_eq!(
            c.exit,
            ExitPlan::At {
                exit_ms: et("2026-09-29 10:00")
            }
        );
        let r = simulate(&s, &md, &p, &set.candidates, Arm::Research);
        assert!((r.trades[0].gross_bps - bps(104.0 / 103.0)).abs() < 1e-9);
        assert_eq!(r.summary.periods_per_year, 261.0);

        // Trading days around Labor Day (Mon 2026-09-07): anchor 21:00 ≥ the
        // entry time ⇒ the previous trading day; exit ⇒ the next one.
        let t0 = et("2026-09-02 00:00");
        let closes: Vec<f64> = (0..240).map(|i| 100.0 + i as f64 * 0.01).collect();
        let md = market(vec![series(A, Interval::H1, t0, &closes)]);
        let s = spec(
            json!({"kind": "daily_window", "universe": [A], "interval": "1h", "days": "trading",
            "calendar": "us_equity", "tz": "America/New_York", "anchor": "21:00", "entry": "20:00",
            "exit": "10:00", "direction": "fade"}),
        );
        let p = run_params(et("2026-09-04 00:00"), et("2026-09-09 00:00"));
        let set = candidates(&s, &md, &p).unwrap();
        let got: Vec<(i64, Option<f64>, ExitPlan)> = set
            .candidates
            .iter()
            .map(|c| (c.decided_at_ms, c.anchor_px, c.exit.clone()))
            .collect();
        let close_at = |t: i64| md.bars[A].close_at(t);
        assert_eq!(
            got,
            vec![
                (
                    et("2026-09-04 20:00"),
                    close_at(et("2026-09-03 21:00")),
                    ExitPlan::At {
                        exit_ms: et("2026-09-08 10:00")
                    }
                ),
                (
                    et("2026-09-08 20:00"),
                    close_at(et("2026-09-04 21:00")),
                    ExitPlan::At {
                        exit_ms: et("2026-09-09 10:00")
                    }
                ),
            ]
        );
        assert!(
            set.candidates.iter().all(|c| c.side == Side::Sell),
            "rising: faded"
        );
    }

    #[test]
    fn move_trigger_take_profit_hold_stop_loss_and_cooldown() {
        let t0 = utc("2026-09-28 00:00");
        let closes = [
            100.0, 100.0, 100.0, 100.0, 103.0, 104.0, 106.5, 101.0, 100.0, 100.0, 100.0, 100.0,
            96.0, 98.0, 98.0, 98.0,
        ];
        let md = market(vec![series(A, Interval::H1, t0, &closes)]);
        let v = json!({"kind": "move_trigger", "universe": [A], "interval": "1h", "lookback_bars": 1,
            "threshold_bps": 200, "direction": "follow", "hold_bars": 3, "take_profit_bps": 250, "stop_loss_bps": 150});
        let s = spec(v.clone());
        let p = run_params(t0, t0 + 20 * H);
        let set = candidates(&s, &md, &p).unwrap();
        let got: Vec<(i64, Side)> = set
            .candidates
            .iter()
            .map(|c| (c.decided_at_ms, c.side))
            .collect();
        // Bars 4 (+295 bps), 7 (−530, after the 3-bar cooldown), 12 (−408).
        assert_eq!(
            got,
            vec![
                (t0 + 5 * H, Side::Buy),
                (t0 + 8 * H, Side::Sell),
                (t0 + 13 * H, Side::Sell)
            ]
        );
        assert_eq!(set.candidates[0].anchor_px, Some(100.0));
        let r = simulate(&s, &md, &p, &set.candidates, Arm::Research);
        let exits: Vec<(i64, ExitReason)> = r
            .trades
            .iter()
            .map(|t| (t.exit_ms, t.exit_reason))
            .collect();
        assert_eq!(
            exits,
            vec![
                (t0 + 7 * H, ExitReason::TakeProfit),
                (t0 + 11 * H, ExitReason::Hold),
                (t0 + 14 * H, ExitReason::StopLoss)
            ]
        );
        assert!((r.trades[0].gross_bps - bps(106.5 / 103.0)).abs() < 1e-9);
        assert!((r.trades[1].gross_bps - bps(101.0 / 100.0)).abs() < 1e-9);
        assert!((r.trades[2].gross_bps + bps(98.0 / 96.0)).abs() < 1e-9);
        assert_eq!(r.trades[0].period, "2026-09-28");

        // Volume: only bar 4 trades 3× its 3-bar baseline.
        let mut vol = md.clone();
        vol.bars.get_mut(A).unwrap().bars[4].v = 3.0;
        let mut v2 = v.clone();
        v2["min_volume_ratio"] = json!(2);
        v2["volume_baseline_bars"] = json!(3);
        let set = candidates(&spec(v2), &vol, &p).unwrap();
        assert_eq!(set.candidates.len(), 1);
        assert_eq!(set.candidates[0].decided_at_ms, t0 + 5 * H);
        // Cooldown 0: bar 6 triggers too (+237 bps).
        let mut v3 = v;
        v3["cooldown_bars"] = json!(0);
        let set = candidates(&spec(v3), &md, &p).unwrap();
        assert!(set.candidates.iter().any(|c| c.decided_at_ms == t0 + 7 * H));
    }

    #[test]
    fn funding_carry_enters_the_receiving_side_and_exits_on_a_low_rate() {
        let t0 = utc("2026-09-28 00:00");
        let mut closes = vec![100.0; 30];
        closes[4] = 99.0; // close at t0 + 5 h
        let rates = [0.00001, 0.0001, 0.0001, 0.0001, 0.000001, 0.0001, 0.0001];
        let mut md = market(vec![series(A, Interval::H1, t0, &closes)]);
        let rows: Vec<(i64, f64)> = rates
            .iter()
            .enumerate()
            .map(|(k, r)| (t0 + k as i64 * H + 37, *r))
            .collect();
        md.funding.insert(A.into(), funding(A, &rows));
        let s = spec(
            json!({"kind": "funding_carry", "universe": [A], "interval": "1h", "min_apr_pct": 50,
            "exit_apr_pct": 10, "hold_hours": 24, "costs": {"taker_fee_bps": 1, "funding": true}}),
        );
        let p = run_params(t0, t0 + 24 * H);
        let set = candidates(&s, &md, &p).unwrap();
        // Row 1 (stamped 01:00:00.037) ⇒ the 02:00 close; rows 2–3 held;
        // row 4 is under the exit APR; row 5 re-enters at 06:00.
        let got: Vec<(i64, Side)> = set
            .candidates
            .iter()
            .map(|c| (c.decided_at_ms, c.side))
            .collect();
        assert_eq!(
            got,
            vec![(t0 + 2 * H, Side::Sell), (t0 + 6 * H, Side::Sell)]
        );
        let c = &set.candidates[0];
        assert!((c.signal_bps - 0.0001 * 8760.0 * 10_000.0).abs() < 1e-6);
        assert_eq!(c.data_asof_ms, t0 + 2 * H);
        assert!((c.features["funding_apr_pct"] - 87.6).abs() < 1e-9);
        let r = simulate(&s, &md, &p, &set.candidates, Arm::Research);
        let t = &r.trades[0];
        assert_eq!(
            (t.exit_ms, t.exit_reason),
            (t0 + 5 * H, ExitReason::FundingBelowExit)
        );
        assert!(
            (t.gross_bps - bps(100.0 / 99.0)).abs() < 1e-9,
            "short from 100 to 99"
        );
        // Settlements at 03, 04, 05 h: the short receives.
        assert!(
            (t.funding_bps - (1.0 + 0.01 + 1.0)).abs() < 1e-9,
            "{}",
            t.funding_bps
        );
        assert!(t.funding_complete);
        assert!((t.net_bps - (t.gross_bps - 2.0 + t.funding_bps)).abs() < 1e-9);
        // The second runs out of rows after 06:00: funding incomplete.
        assert!(!r.trades[1].funding_complete);
        assert_eq!(r.summary.funding_incomplete, 1);
    }

    #[test]
    fn pair_spread_by_hand() {
        let t0 = utc("2026-09-28 00:00");
        let d = 0.01f64;
        let spreads = [d, -d, d, -d, 5.0 * d, d, d];
        let a: Vec<f64> = spreads.iter().map(|s| 100.0 * s.exp()).collect();
        let md = market(vec![
            series(A, Interval::H1, t0, &a),
            series(B, Interval::H1, t0, &[100.0; 7]),
        ]);
        let s = spec(
            json!({"kind": "pair_spread", "interval": "1h", "legs": [A, B], "lookback_bars": 4,
            "entry_z": 2, "exit_z": 0.5, "max_hold_bars": 10,
            "costs": {"taker_fee_bps": 1, "funding": false}}),
        );
        let p = run_params(t0, t0 + 10 * H);
        let set = candidates(&s, &md, &p).unwrap();
        assert_eq!(set.candidates.len(), 1);
        let c = &set.candidates[0];
        assert_eq!(c.instrument, format!("{A}/{B}"));
        assert_eq!(
            (c.side, c.legs[0].side, c.legs[1].side),
            (Side::Sell, Side::Sell, Side::Buy)
        );
        assert!((c.signal_bps - 500.0).abs() < 1e-6, "5d over a mean of 0");
        let r = simulate(&s, &md, &p, &set.candidates, Arm::Research);
        let t = &r.trades[0];
        assert_eq!((t.exit_ms, t.exit_reason), (t0 + 6 * H, ExitReason::ExitZ));
        assert_eq!(t.legs[0].notional_usd, 50.0);
        assert!(
            (t.legs[0].gross_bps - 400.0).abs() < 1e-6,
            "{}",
            t.legs[0].gross_bps
        );
        assert!(t.legs[1].gross_bps.abs() < 1e-9);
        assert!((t.gross_bps - 200.0).abs() < 1e-6);
        assert!(
            (t.net_bps - (200.0 - 2.0)).abs() < 1e-6,
            "each leg pays 2 bps"
        );
        assert!((t.net_usd - 1.98).abs() < 1e-6);
    }

    #[test]
    fn event_window_by_hand() {
        let day = |h: &str| utc(&format!("2026-09-29 {h}"));
        let bars = sparse(
            A,
            Interval::H1,
            &[
                (day("12:00"), 100.0),
                (day("13:00"), 101.0),
                (day("15:00"), 102.0),
            ],
        );
        let md = market(vec![bars, sparse(C, Interval::H1, &[(day("12:00"), 5.0)])]);
        let base = json!({"kind": "event_window", "interval": "1h", "direction": "follow",
            "events": [
                {"instrument": A, "t": "2026-09-29T13:30:00Z", "label": "cpi"},
                {"instrument": B, "t": "2026-09-29T13:10:00Z"},
                {"instrument": C, "t": "2026-09-29T13:20:00Z"},
                {"instrument": D, "t": "2026-09-30T13:20:00Z"}
            ],
            "entry_delay_mins": 30, "exit_after_mins": 120, "exclude": [D]});
        let p = run_params(utc("2026-09-29 00:00"), utc("2026-09-30 00:00"));
        let set = candidates(&spec(base.clone()), &md, &p).unwrap();
        assert_eq!(set.candidates.len(), 1);
        let c = &set.candidates[0];
        // 13:30 + 30 min ⇒ the 14:00 close; the anchor = the 13:00 close.
        assert_eq!(
            (c.decided_at_ms, c.anchor_px, c.side),
            (day("14:00"), Some(100.0), Side::Buy)
        );
        assert_eq!(
            c.exit,
            ExitPlan::At {
                exit_ms: day("16:00")
            }
        );
        assert_eq!(c.label.as_deref(), Some("cpi"));
        let skips: Vec<(&str, SkipReason)> = set
            .skipped
            .iter()
            .map(|k| (k.instrument.as_str(), k.reason))
            .collect();
        assert_eq!(
            skips,
            vec![
                (B, SkipReason::MissingAnchor),
                (C, SkipReason::MissingEntry)
            ],
            "D is out of range"
        );
        let r = simulate(&spec(base.clone()), &md, &p, &set.candidates, Arm::Research);
        assert!((r.trades[0].gross_bps - bps(102.0 / 101.0)).abs() < 1e-9);
        assert_eq!(r.trades[0].label.as_deref(), Some("cpi"));
        // exit_at 12:00 New York (EDT) after 10:00 EDT = 16:00 UTC too.
        let mut at = base.clone();
        at.as_object_mut().unwrap().remove("exit_after_mins");
        at["exit_at"] = json!("12:00");
        at["tz"] = json!("America/New_York");
        let set = candidates(&spec(at), &md, &p).unwrap();
        assert_eq!(
            set.candidates[0].exit,
            ExitPlan::At {
                exit_ms: day("16:00")
            }
        );
        // A move under the minimum.
        let mut min = base;
        min["min_abs_move_bps"] = json!(150);
        let set = candidates(&spec(min), &md, &p).unwrap();
        assert!(set.candidates.is_empty());
        assert_eq!(set.skipped[0].reason, SkipReason::BelowMinSignal);
    }
}
