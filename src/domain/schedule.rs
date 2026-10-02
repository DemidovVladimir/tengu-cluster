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

use crate::domain::calendar::{parse_hm, DAY_MIN};
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
mod tests {
    use super::*;
    use chrono::NaiveDateTime;

    const MIN: u64 = 60_000;

    fn utc(s: &str) -> i64 {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
            .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M"))
            .unwrap()
            .and_utc()
            .timestamp_millis()
    }

    /// Wall-clock time `s` in New York → UTC ms.
    fn et(s: &str) -> i64 {
        let local = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
            .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M"))
            .unwrap();
        Zone::NewYork.to_utc_ms(local)
    }

    fn day(d: Weekday) -> Days {
        Days::NONE.with(d)
    }

    fn window(days: Days, from: &str, to: &str, every_ms: u64) -> Window {
        Window {
            days,
            from: parse_clock(from, false).unwrap(),
            to: parse_clock(to, true).unwrap(),
            every_ms,
        }
    }

    fn at_ticks(list: &[&str]) -> Schedule {
        Schedule {
            zone: Zone::NewYork,
            every_ms: None,
            windows: vec![],
            at: list.iter().map(|s| parse_at(s).unwrap()).collect(),
        }
    }

    /// Build plan § weekend sandbox: `hl_book` every 5 min, every 60 s
    /// Sun 17:00–19:00 and Mon 08:30–09:30 ET.
    fn weekend_book() -> Schedule {
        Schedule {
            zone: Zone::NewYork,
            every_ms: Some(5 * MIN),
            windows: vec![
                window(day(Weekday::Sun), "17:00", "19:00", MIN),
                window(day(Weekday::Mon), "08:30", "09:30", MIN),
            ],
            at: vec![],
        }
    }

    fn fire(at_ms: i64, every_ms: Option<u64>) -> Option<Fire> {
        Some(Fire { at_ms, every_ms })
    }

    /// Fires from `now` on, each one fed back as `last_fire_ms`.
    fn sequence(s: &Schedule, now: i64, n: usize) -> Vec<i64> {
        let mut out = Vec::new();
        let mut last = None;
        for _ in 0..n {
            let f = next_fire(s, now.max(last.unwrap_or(now)), last).unwrap();
            out.push(f.at_ms);
            last = Some(f.at_ms);
        }
        out
    }

    #[test]
    fn plain_interval_sits_on_the_epoch_grid_and_skips_missed_slots() {
        let s = Schedule {
            zone: Zone::Utc,
            every_ms: Some(MIN),
            windows: vec![],
            at: vec![],
        };
        let t = utc("2026-10-04 12:00");
        for (now, last, want) in [
            (t + 30_000, None, t + 60_000),
            (t, None, t),                            // on the grid: fire now
            (t, Some(t), t + 60_000),                // never the same slot twice
            (t + 1, Some(t), t + 60_000),            // a run that took 1 ms
            (t + 150_000, Some(t), t + 180_000),     // a 150 s run: 60 s, 120 s skipped
            (t + 3_600_000, Some(t), t + 3_600_000), // back after an hour: no burst
        ] {
            assert_eq!(
                next_fire(&s, now, last),
                fire(want, Some(MIN)),
                "now {now} last {last:?}"
            );
        }
    }

    #[test]
    fn weekend_book_schedule_windows_override_the_base_interval() {
        let s = weekend_book();
        for (now, last, want, every) in [
            ("2026-10-04 16:52:00", None, "2026-10-04 16:55", 5 * MIN),
            ("2026-10-04 16:55:00", None, "2026-10-04 16:55", 5 * MIN),
            // The base slot at 17:00 is inside the window: the window fires it.
            (
                "2026-10-04 16:55:00",
                Some("2026-10-04 16:55"),
                "2026-10-04 17:00",
                MIN,
            ),
            (
                "2026-10-04 17:00:30",
                Some("2026-10-04 17:00"),
                "2026-10-04 17:01",
                MIN,
            ),
            // Missed window slots are skipped.
            (
                "2026-10-04 17:30:30",
                Some("2026-10-04 17:02"),
                "2026-10-04 17:31",
                MIN,
            ),
            // The end is exclusive: 19:00 is a base slot again.
            (
                "2026-10-04 18:59:00",
                Some("2026-10-04 18:59"),
                "2026-10-04 19:00",
                5 * MIN,
            ),
            ("2026-10-04 19:00:01", None, "2026-10-04 19:05", 5 * MIN),
            ("2026-10-05 08:26:00", None, "2026-10-05 08:30", MIN),
            ("2026-10-05 09:29:10", None, "2026-10-05 09:30", 5 * MIN),
            // After the fall-back switch the window follows local time (EST).
            ("2026-11-01 16:59:00", None, "2026-11-01 17:00", MIN),
            // Spring-forward Sunday: EDT from 03:00, the window at 17:00 EDT.
            ("2026-03-08 16:57:30", None, "2026-03-08 17:00", MIN),
        ] {
            assert_eq!(
                next_fire(&s, et(now), last.map(et)),
                fire(et(want), Some(every)),
                "now {now} last {last:?}"
            );
        }
        assert_eq!(et("2026-11-01 17:00"), utc("2026-11-01 22:00"));
        assert_eq!(et("2026-03-08 17:00"), utc("2026-03-08 21:00"));
        // Two hours at 60 s = 120 fires, then the base grid resumes.
        let seq = sequence(&s, et("2026-10-04 17:00"), 122);
        assert_eq!(seq[119], et("2026-10-04 18:59"));
        assert_eq!(seq[120], et("2026-10-04 19:00"));
        assert_eq!(seq[121], et("2026-10-04 19:05"));
    }

    #[test]
    fn at_ticks_follow_new_york_wall_time_across_both_dst_switches() {
        let s = at_ticks(&["Sun 18:00", "Mon 09:00"]);
        for (now, want) in [
            // The weekend-run weekend (EDT).
            (
                "2026-10-02 20:00",
                ["2026-10-04 22:00", "2026-10-05 13:00", "2026-10-11 22:00"],
            ),
            // Fall back on 2026-11-01: EST from then on.
            (
                "2026-10-31 12:00",
                ["2026-11-01 23:00", "2026-11-02 14:00", "2026-11-08 23:00"],
            ),
            // Spring forward on 2026-03-08: EDT from then on.
            (
                "2026-03-07 12:00",
                ["2026-03-08 22:00", "2026-03-09 13:00", "2026-03-15 22:00"],
            ),
        ] {
            let want: Vec<i64> = want.iter().map(|s| utc(s)).collect();
            assert_eq!(sequence(&s, et(now), 3), want, "from {now} ET");
        }
        assert!(
            next_fire(&s, et("2026-10-04 18:00"), None)
                .unwrap()
                .every_ms
                .is_none(),
            "an at-tick is never jittered"
        );
    }

    #[test]
    fn at_ticks_in_the_gap_and_the_overlap_fire_once() {
        // 02:30 does not exist on 2026-03-08: read as 03:30 EDT.
        let gap = at_ticks(&["Sun 02:30"]);
        assert_eq!(
            sequence(&gap, et("2026-03-07 12:00"), 2),
            vec![utc("2026-03-08 07:30"), utc("2026-03-15 06:30")]
        );
        // 01:30 happens twice on 2026-11-01: only the first (EDT) fires.
        let overlap = at_ticks(&["Sun 01:30"]);
        let first = next_fire(&overlap, et("2026-10-31 12:00"), None).unwrap();
        assert_eq!(first.at_ms, utc("2026-11-01 05:30"));
        assert_eq!(
            next_fire(&overlap, utc("2026-11-01 06:00"), Some(first.at_ms))
                .unwrap()
                .at_ms,
            utc("2026-11-08 06:30"),
            "the second 01:30 (06:30 UTC on 11-01) must not fire"
        );
        // daily: 09:00 EDT, then 09:00 EST.
        let daily = at_ticks(&["daily 09:00"]);
        assert_eq!(
            sequence(&daily, utc("2026-10-31 12:00"), 2),
            vec![utc("2026-10-31 13:00"), utc("2026-11-01 14:00")]
        );
    }

    #[test]
    fn windows_keep_wall_clock_bounds_across_dst() {
        // Sun 00:00–03:00 ET hourly: the fall-back night is 4 h long.
        let fall = Schedule {
            zone: Zone::NewYork,
            every_ms: None,
            windows: vec![window(day(Weekday::Sun), "00:00", "03:00", 60 * MIN)],
            at: vec![],
        };
        assert_eq!(
            sequence(&fall, et("2026-10-31 12:00"), 5),
            vec![
                utc("2026-11-01 04:00"), // 00:00 EDT
                utc("2026-11-01 05:00"), // 01:00 EDT
                utc("2026-11-01 06:00"), // 01:00 EST
                utc("2026-11-01 07:00"), // 02:00 EST
                utc("2026-11-08 05:00"), // next Sunday 00:00 EST
            ]
        );
        // Sun 01:00–04:00 ET hourly: the spring-forward night is 2 h long.
        let spring = Schedule {
            zone: Zone::NewYork,
            every_ms: None,
            windows: vec![window(day(Weekday::Sun), "01:00", "04:00", 60 * MIN)],
            at: vec![],
        };
        assert_eq!(
            sequence(&spring, et("2026-03-07 12:00"), 3),
            vec![
                utc("2026-03-08 06:00"), // 01:00 EST
                utc("2026-03-08 07:00"), // 03:00 EDT
                utc("2026-03-15 05:00"), // next Sunday 01:00 EDT
            ]
        );
        // Gap times read with the standard offset: 02:00–02:45 shifts to
        // 03:00–03:45 EDT; 02:30–03:00 resolves backwards and is skipped.
        let d = NaiveDate::from_ymd_opt(2026, 3, 8).unwrap();
        let shifted = window(day(Weekday::Sun), "02:00", "02:45", MIN);
        assert_eq!(
            shifted.span(Zone::NewYork, d),
            Some((utc("2026-03-08 07:00"), utc("2026-03-08 07:45")))
        );
        let empty = window(day(Weekday::Sun), "02:30", "03:00", MIN);
        assert_eq!(empty.span(Zone::NewYork, d), None);
    }

    #[test]
    fn windows_crossing_midnight_and_ending_at_24_00() {
        let s = Schedule {
            zone: Zone::NewYork,
            every_ms: Some(60 * MIN),
            windows: vec![window(day(Weekday::Fri), "22:00", "02:00", 10 * MIN)],
            at: vec![],
        };
        for (now, want, every) in [
            ("2026-10-01 22:30:00", "2026-10-01 23:00", 60 * MIN), // Thu: no window
            ("2026-10-02 21:30:00", "2026-10-02 22:00", 10 * MIN),
            ("2026-10-03 00:05:00", "2026-10-03 00:10", 10 * MIN), // Sat, still Fri's window
            ("2026-10-03 01:55:01", "2026-10-03 02:00", 60 * MIN), // closed at 02:00
        ] {
            assert_eq!(
                next_fire(&s, et(now), None),
                fire(et(want), Some(every)),
                "now {now}"
            );
        }
        // Across the fall-back night: Sat 22:00 EDT → Sun 02:00 EST is 5 h
        // (30 fires); the base grid resumes at 02:00 EST.
        let night = Schedule {
            zone: Zone::NewYork,
            every_ms: Some(60 * MIN),
            windows: vec![window(day(Weekday::Sat), "22:00", "02:00", 10 * MIN)],
            at: vec![],
        };
        let seq = sequence(&night, utc("2026-11-01 02:00"), 32);
        assert_eq!(seq[0], utc("2026-11-01 02:00")); // Sat 22:00 EDT
        assert_eq!(seq[29], utc("2026-11-01 06:50")); // Sun 01:50 EST
        assert_eq!(seq[30], utc("2026-11-01 07:00")); // 02:00 EST: base grid
        assert_eq!(seq[31], utc("2026-11-01 08:00"));
        let to_midnight = Schedule {
            zone: Zone::NewYork,
            every_ms: Some(60 * MIN),
            windows: vec![window(day(Weekday::Sat), "23:00", "24:00", 15 * MIN)],
            at: vec![],
        };
        assert_eq!(
            sequence(&to_midnight, et("2026-10-03 22:30"), 6),
            ["23:00", "23:15", "23:30", "23:45"]
                .iter()
                .map(|t| et(&format!("2026-10-03 {t}")))
                .chain([et("2026-10-04 00:00"), et("2026-10-04 01:00")])
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn windows_only_overlaps_and_full_cover() {
        // No base interval: the next window start.
        let only = Schedule {
            zone: Zone::NewYork,
            every_ms: None,
            windows: vec![window(day(Weekday::Sun), "17:00", "19:00", MIN)],
            at: vec![],
        };
        assert_eq!(
            next_fire(&only, et("2026-09-30 12:00"), None),
            fire(et("2026-10-04 17:00"), Some(MIN))
        );
        // Overlapping windows fire on the union of their grids; ties take
        // the finer grid.
        let overlap = Schedule {
            zone: Zone::Utc,
            every_ms: None,
            windows: vec![
                window(day(Weekday::Mon), "10:00", "12:00", 30 * MIN),
                window(day(Weekday::Mon), "11:00", "11:30", 10 * MIN),
            ],
            at: vec![],
        };
        assert_eq!(
            sequence(&overlap, utc("2026-10-05 10:59"), 5),
            vec![
                utc("2026-10-05 11:00"),
                utc("2026-10-05 11:10"),
                utc("2026-10-05 11:20"),
                utc("2026-10-05 11:30"), // the 10-min window ended; the 30-min grid
                utc("2026-10-12 10:00"), // 12:00 is the end of both: next Monday
            ]
        );
        assert_eq!(
            next_fire(&overlap, utc("2026-10-05 11:00"), None)
                .unwrap()
                .every_ms,
            Some(10 * MIN)
        );
        // A slower window silences the faster base inside it.
        let slow = Schedule {
            zone: Zone::Utc,
            every_ms: Some(MIN),
            windows: vec![window(Days::ALL, "00:00", "06:00", 60 * MIN)],
            at: vec![],
        };
        assert_eq!(
            next_fire(&slow, utc("2026-10-05 00:00:30"), None),
            fire(utc("2026-10-05 01:00"), Some(60 * MIN))
        );
        assert_eq!(
            next_fire(
                &slow,
                utc("2026-10-05 05:30"),
                Some(utc("2026-10-05 05:00"))
            ),
            fire(utc("2026-10-05 06:00"), Some(MIN))
        );
        // Windows covering the whole week leave no base fire (and terminate).
        let cover = Schedule {
            zone: Zone::Utc,
            every_ms: Some(60 * MIN),
            windows: vec![window(Days::ALL, "00:00", "24:00", 7 * MIN)],
            at: vec![],
        };
        let f = next_fire(&cover, utc("2026-10-05 00:01"), None).unwrap();
        assert_eq!(
            f,
            Fire {
                at_ms: utc("2026-10-05 00:07"),
                every_ms: Some(7 * MIN)
            }
        );
        assert_eq!(
            next_on_base(&cover, 60 * MIN, utc("2026-10-05 00:01")),
            None
        );
    }

    #[test]
    fn empty_schedule_and_mixed_sources() {
        let empty = Schedule {
            zone: Zone::Utc,
            every_ms: None,
            windows: vec![],
            at: vec![],
        };
        assert_eq!(next_fire(&empty, 0, None), None);
        assert_eq!(empty.longest_interval_ms(), None);
        // An at-tick on a base slot wins the tie (no jitter).
        let mixed = Schedule {
            zone: Zone::NewYork,
            every_ms: Some(5 * MIN),
            windows: vec![],
            at: vec![parse_at("Sun 18:00").unwrap()],
        };
        assert_eq!(
            next_fire(&mixed, et("2026-10-04 17:58"), None),
            fire(et("2026-10-04 18:00"), None)
        );
        assert_eq!(
            next_fire(&mixed, et("2026-10-04 18:00"), Some(et("2026-10-04 18:00"))),
            fire(et("2026-10-04 18:05"), Some(5 * MIN))
        );
        assert_eq!(weekend_book().longest_interval_ms(), Some(5 * MIN));
    }

    /// The wall clock steps back X after a fire: the next fire comes on the
    /// new clock's grid, not X later; a step within the grace keeps the
    /// order (the last slot never fires twice).
    #[test]
    fn a_backward_clock_step_does_not_stall_the_feed() {
        let s = Schedule {
            zone: Zone::Utc,
            every_ms: Some(MIN),
            windows: vec![],
            at: vec![],
        };
        let t = utc("2026-10-04 22:00");
        for (now, want) in [
            // Stepped back 10 min right after the 22:00 fire: 21:51, not 22:01.
            (t - 10 * MIN as i64 + 30_000, t - 9 * MIN as i64),
            // Back 90 s: 21:59, then the 22:00 slot again on the new clock.
            (t - 90_000, t - MIN as i64),
            // Within the grace: still strictly after the last fire.
            (t - CLOCK_STEP_GRACE_MS, t + MIN as i64),
            (t - 400, t + MIN as i64),
        ] {
            assert_eq!(
                next_fire(&s, now, Some(t)),
                fire(want, Some(MIN)),
                "now {now}"
            );
        }
        // At-ticks too: back across the tick, it fires on the new clock.
        let tick = at_ticks(&["Sun 18:00"]);
        let at = et("2026-10-04 18:00");
        assert_eq!(
            next_fire(&tick, at - 5 * MIN as i64, Some(at)),
            fire(at, None)
        );
        assert_eq!(
            next_fire(&tick, at + 1, Some(at)),
            fire(et("2026-10-11 18:00"), None)
        );
    }

    #[test]
    fn jitter_delays_grid_fires_only() {
        let grid = Fire {
            at_ms: 1_000_000,
            every_ms: Some(MIN),
        };
        for (pct, r, want) in [
            (10, 0.5, 1_003_000),
            (10, 1.0, 1_006_000),
            (0, 1.0, 1_000_000),
            (80, 1.0, 1_030_000), // capped at 50 %
            (10, f64::NAN, 1_000_000),
            (10, -1.0, 1_000_000),
        ] {
            assert_eq!(jittered_ms(grid, pct, r), want, "pct {pct} r {r}");
        }
        let tick = Fire {
            at_ms: 1_000_000,
            every_ms: None,
        };
        assert_eq!(jittered_ms(tick, 50, 1.0), 1_000_000);
        assert_eq!(late_grace_ms(tick), 60_000);
        assert_eq!(late_grace_ms(grid), 60_000);
        assert_eq!(
            late_grace_ms(Fire {
                every_ms: Some(5 * MIN),
                ..grid
            }),
            300_000
        );
        assert_eq!(
            late_grace_ms(Fire {
                every_ms: Some(5_000),
                ..grid
            }),
            60_000
        );
    }

    #[test]
    fn parses_days_clock_times_and_at_ticks() {
        assert_eq!(parse_day("sun"), Some(Weekday::Sun));
        assert_eq!(parse_day(" MON "), Some(Weekday::Mon));
        assert_eq!(parse_day("Sunday"), None);
        assert_eq!(parse_clock("24:00", true), Some(1440));
        assert_eq!(parse_clock("24:00", false), None);
        assert_eq!(parse_clock("08:30", false), Some(510));
        assert_eq!(parse_clock("8:30", false), None);
        let t = parse_at("Sun 18:00").unwrap();
        assert!(t.days.contains(Weekday::Sun) && !t.days.contains(Weekday::Sat));
        assert_eq!(t.minute, 1080);
        assert_eq!(parse_at("DAILY 09:00").unwrap().days, Days::ALL);
        assert_eq!(parse_at("sun  07:05").unwrap().minute, 425);
        for bad in [
            "Sunday 18:00",
            "Sun 24:00",
            "18:00",
            "Sun",
            "weekdays 09:00",
            "",
        ] {
            assert_eq!(parse_at(bad), None, "{bad}");
        }
    }
}
