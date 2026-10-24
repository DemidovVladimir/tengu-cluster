//! Feed schedules (`[feeds.<n>]`, xmarket `rt-scheduler`) — when a feed
//! fires. Pure: `now_ms` is an input; civil time and DST come from
//! `domain/tz.rs`. Config `config/feeds.rs`; runner
//! `application/runtime/feeds.rs`.
//!
//! | Source | Fires | Grid |
//! |---|---|---|
//! | `every_ms` | outside every window | multiples of `every_ms` since the Unix epoch (UTC) |
//! | [`Window`] | inside `[from, to)` local time, on the `days` it starts (`to <= from` ends the next day; `24:00` = midnight) | `start + k · every_ms` |
//! | [`AtTick`] | at its local wall time on its `days` (`"Sun 18:00"`, `"daily 09:00"`) | — |
//!
//! [`next_fire`] = the earliest fire at or after `now_ms` and strictly after
//! `last_fire_ms`: a slot in the past is skipped, never replayed. Ties go to
//! the at-tick (never jittered), then the finer grid. Overlapping windows
//! fire on the union of their grids. A `last_fire_ms` more than
//! [`CLOCK_STEP_GRACE_MS`] after `now_ms` means the wall clock stepped back:
//! it is clamped to `now_ms`, so the feed fires on the new clock's grid
//! instead of stalling for the step (a slot before the step may fire again
//! under its call ids — exec tools deduplicate).
//!
//! DST (`Zone::to_utc_ms`): a wall time in the spring-forward gap reads with
//! the standard offset (02:30 → 03:30 EDT); one in the fall-back overlap is
//! the earlier instant. So an at-tick fires once on each day it names, and a
//! window keeps its wall-clock bounds (one spanning the fall-back hour runs an
//! hour longer, one spanning the gap an hour shorter; one whose end resolves
//! at or before its start is skipped that day).

use chrono::{Datelike, Duration, NaiveDate, Weekday};

use crate::domain::time::{parse_hm, DAY_MIN};
use crate::domain::tz::Zone;

/// Local dates [`next_fire`] looks at around an instant: one back (a window
/// from yesterday still open after midnight), eight ahead (a weekly at-tick
/// already past today).
const SCAN_BACK_DAYS: i64 = 1;
const SCAN_AHEAD_DAYS: i64 = 8;
const DAY_MS: i64 = 86_400_000;
/// Bound on the base-grid walk past windows.
const MAX_BASE_STEPS: usize = 1_000;
/// Shortest grace of a late fire ([`late_grace_ms`]).
const MIN_GRACE_MS: i64 = 60_000;
/// How far the last fire may sit after `now_ms` before [`next_fire`] reads
/// it as a backward step of the wall clock.
pub const CLOCK_STEP_GRACE_MS: i64 = 1_000;
/// Largest `jitter_pct`.
pub const MAX_JITTER_PCT: u32 = 50;
/// Longest interval: fires are looked up at most [`SCAN_AHEAD_DAYS`] ahead.
pub const MAX_EVERY_SECS: u64 = 7 * 86_400;

/// A set of weekdays (bit 0 = Monday … bit 6 = Sunday).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Days(u8);

impl Days {
    pub const ALL: Days = Days(0x7f);
    pub const NONE: Days = Days(0);

    pub fn with(self, day: Weekday) -> Days {
        Days(self.0 | (1 << day.num_days_from_monday()))
    }

    pub fn contains(self, day: Weekday) -> bool {
        self.0 & (1 << day.num_days_from_monday()) != 0
    }
}

/// A local-time span with its own interval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    /// Days the window starts on.
    pub days: Days,
    /// Minutes after local midnight, `[from, to)`; `to <= from` ends the
    /// next day; `to` may be 1440 (midnight).
    pub from: u32,
    pub to: u32,
    pub every_ms: u64,
}

impl Window {
    /// `[start, end)` in UTC ms of the occurrence starting on local `date`;
    /// `None` when `date` is not one of `days` or the span resolves empty
    /// (a spring-forward gap edge case).
    pub fn span(&self, zone: Zone, date: NaiveDate) -> Option<(i64, i64)> {
        if !self.days.contains(date.weekday()) {
            return None;
        }
        let start = local_ms(zone, date, self.from)?;
        let end_date = if self.to > self.from {
            date
        } else {
            date.succ_opt()?
        };
        let end = local_ms(zone, end_date, self.to)?;
        (end > start).then_some((start, end))
    }
}

/// A clock tick: local wall time `minute` on `days`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AtTick {
    pub days: Days,
    /// Minutes after local midnight (0–1439).
    pub minute: u32,
}

/// When a feed fires (see the module table).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schedule {
    /// Zone of `windows` and `at`.
    pub zone: Zone,
    /// Base interval outside the windows; `None` = no fires there.
    pub every_ms: Option<u64>,
    pub windows: Vec<Window>,
    pub at: Vec<AtTick>,
}

impl Schedule {
    /// Longest grid interval (base or window); `None` = at-ticks only.
    pub fn longest_interval_ms(&self) -> Option<u64> {
        self.every_ms
            .into_iter()
            .chain(self.windows.iter().map(|w| w.every_ms))
            .max()
    }
}

/// One fire time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fire {
    pub at_ms: i64,
    /// Interval of the grid that produced it; `None` = an at-tick.
    pub every_ms: Option<u64>,
}

/// The earliest fire at or after `now_ms` and strictly after `last_fire_ms`
/// (see the module doc: a last fire past the clock-step grace counts as
/// `now_ms`); `None` only for an empty schedule.
pub fn next_fire(s: &Schedule, now_ms: i64, last_fire_ms: Option<i64>) -> Option<Fire> {
    let t = match last_fire_ms {
        Some(last) => {
            let stepped_back = last > now_ms.saturating_add(CLOCK_STEP_GRACE_MS);
            let last = if stepped_back { now_ms } else { last };
            now_ms.max(last.saturating_add(1))
        }
        None => now_ms,
    };
    let mut best = None;
    for tick in &s.at {
        if let Some(at_ms) = next_at_tick(s.zone, tick, t) {
            offer(
                &mut best,
                Fire {
                    at_ms,
                    every_ms: None,
                },
            );
        }
    }
    for w in &s.windows {
        if let Some(at_ms) = next_in_window(s.zone, w, t) {
            offer(
                &mut best,
                Fire {
                    at_ms,
                    every_ms: Some(w.every_ms),
                },
            );
        }
    }
    if let Some(every) = s.every_ms {
        if let Some(at_ms) = next_on_base(s, every, t) {
            offer(
                &mut best,
                Fire {
                    at_ms,
                    every_ms: Some(every),
                },
            );
        }
    }
    best
}

/// `fire.at_ms` delayed by `rand01 · jitter_pct % · every_ms` for a grid
/// fire; an at-tick keeps its instant. `jitter_pct` above
/// [`MAX_JITTER_PCT`] counts as the max; `rand01` outside [0, 1] or NaN as 0.
pub fn jittered_ms(fire: Fire, jitter_pct: u32, rand01: f64) -> i64 {
    let Some(every) = fire.every_ms else {
        return fire.at_ms;
    };
    let r = if (0.0..=1.0).contains(&rand01) {
        rand01
    } else {
        0.0
    };
    let span = every as f64 * f64::from(jitter_pct.min(MAX_JITTER_PCT)) / 100.0;
    fire.at_ms.saturating_add((span * r).round() as i64)
}

/// How late a fire may still run: its interval, at least a minute; an
/// at-tick a minute. Later ⇒ the runner skips it (system sleep, stalled
/// clock) — never a burst of stale fires.
pub fn late_grace_ms(fire: Fire) -> i64 {
    fire.every_ms.map_or(MIN_GRACE_MS, |e| {
        (e.min(i64::MAX as u64) as i64).max(MIN_GRACE_MS)
    })
}

/// `"Mon"` … `"Sun"` (any case).
pub fn parse_day(s: &str) -> Option<Weekday> {
    Some(match s.trim().to_ascii_lowercase().as_str() {
        "mon" => Weekday::Mon,
        "tue" => Weekday::Tue,
        "wed" => Weekday::Wed,
        "thu" => Weekday::Thu,
        "fri" => Weekday::Fri,
        "sat" => Weekday::Sat,
        "sun" => Weekday::Sun,
        _ => return None,
    })
}

/// `"HH:MM"` (00:00–23:59) → minutes after midnight; with `allow_end` also
/// `"24:00"` (1440 — a window ending at midnight).
pub fn parse_clock(s: &str, allow_end: bool) -> Option<u32> {
    if allow_end && s.trim() == "24:00" {
        return Some(DAY_MIN);
    }
    parse_hm(s)
}

/// `"Sun 18:00"` (one day, any case) or `"daily 09:00"` (every day).
pub fn parse_at(s: &str) -> Option<AtTick> {
    let (day, hm) = s.trim().split_once(' ')?;
    let days = if day.eq_ignore_ascii_case("daily") {
        Days::ALL
    } else {
        Days::NONE.with(parse_day(day)?)
    };
    Some(AtTick {
        days,
        minute: parse_clock(hm.trim(), false)?,
    })
}

/// Keep the earlier fire; on a tie the at-tick, then the finer grid.
fn offer(best: &mut Option<Fire>, f: Fire) {
    let rank = |x: &Fire| (x.at_ms, x.every_ms.map_or(0, |e| e.saturating_add(1)));
    if best.as_ref().is_none_or(|b| rank(&f) < rank(b)) {
        *best = Some(f);
    }
}

/// UTC ms of `minute` minutes after local midnight of `date` (1440 = the
/// next midnight).
fn local_ms(zone: Zone, date: NaiveDate, minute: u32) -> Option<i64> {
    if minute >= DAY_MIN {
        return local_ms(zone, date.succ_opt()?, minute - DAY_MIN);
    }
    Some(zone.at(date, minute / 60, minute % 60))
}

/// Local dates from [`SCAN_BACK_DAYS`] before `t`'s to [`SCAN_AHEAD_DAYS`]
/// after it.
fn dates_around(zone: Zone, t: i64) -> impl Iterator<Item = NaiveDate> {
    let today = zone.local_date(t);
    (-SCAN_BACK_DAYS..=SCAN_AHEAD_DAYS)
        .filter_map(move |k| today.checked_add_signed(Duration::days(k)))
}

fn next_at_tick(zone: Zone, tick: &AtTick, t: i64) -> Option<i64> {
    dates_around(zone, t)
        .filter(|d| tick.days.contains(d.weekday()))
        .filter_map(|d| local_ms(zone, d, tick.minute))
        .filter(|&at| at >= t)
        .min()
}

fn next_in_window(zone: Zone, w: &Window, t: i64) -> Option<i64> {
    let every = i64::try_from(w.every_ms).ok()?.max(1);
    dates_around(zone, t)
        .filter_map(|d| w.span(zone, d))
        .filter_map(|(start, end)| {
            let at = if t <= start {
                start
            } else {
                start.saturating_add((t - start + every - 1) / every * every)
            };
            (at < end).then_some(at)
        })
        .min()
}

/// First base-grid instant at or after `t` outside every window.
fn next_on_base(s: &Schedule, every_ms: u64, t: i64) -> Option<i64> {
    let every = i64::try_from(every_ms).ok()?.max(1);
    let horizon = t.saturating_add(SCAN_AHEAD_DAYS * DAY_MS);
    let mut at = align_up(t, every);
    for _ in 0..MAX_BASE_STEPS {
        if at > horizon {
            return None;
        }
        match window_end_at(s, at) {
            None => return Some(at),
            Some(end) => at = align_up(end, every),
        }
    }
    None
}

/// End of the latest-ending window occurrence that contains `t`.
fn window_end_at(s: &Schedule, t: i64) -> Option<i64> {
    let today = s.zone.local_date(t);
    let days = [today.pred_opt(), Some(today)];
    s.windows
        .iter()
        .flat_map(|w| days.iter().flatten().filter_map(|d| w.span(s.zone, *d)))
        .filter(|&(start, end)| start <= t && t < end)
        .map(|(_, end)| end)
        .max()
}

/// Smallest multiple of `every` (> 0) at or after `t`.
fn align_up(t: i64, every: i64) -> i64 {
    let rem = t.rem_euclid(every);
    if rem == 0 {
        t
    } else {
        t.saturating_add(every - rem)
    }
}

#[cfg(test)]
#[path = "schedule_tests.rs"]
mod tests;
