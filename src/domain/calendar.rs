//! Market-session calendars (`kg-calendars`, tracker convention 18) — one pure
//! evaluator. Data: `[xmarket.calendars.<id>]` (`config/xmarket.rs`); civil
//! time and DST: `domain/tz.rs`; `utc_ms` is always an input.
//!
//! | Kind | Session at an instant | Rows (`config.example.toml`) |
//! |---|---|---|
//! | `exchange` | `pre` `[pre, open)` · `open` `[open, close)` · `post` `[close, post)` on trading days (Mon–Fri minus `holidays`; `early_closes` use `early_close` / `early_post`) · `overnight` from `post` on the calendar day before a trading day to its `pre` · else `closed` | `us_equity` (NYSE) |
//! | `weekly` | `open` inside one weekly window `[open, close)` (wraps across the week), minus an optional daily break · else `closed` | trade[XYZ] external price (stocks = 24x5, indices / commodities, FX), RH tokenization |
//! | `24x7` | always `open` | HL crypto perps |
//!
//! Weekend clock (`ExchangeCalendar::weekend_window`, `x-weekend-fade-strategy`),
//! around a break = a run of non-trading days, all local time:
//!
//! | Instant | Rule | 2026-09-26 → 09-28 |
//! |---|---|---|
//! | anchor | 20:00 on the last trading day before the break (normally Fri) | Fri 20:00 EDT = Sat 00:00 UTC |
//! | entry | 18:00 on the last non-trading day (normally Sun; Mon for a Monday holiday) | Sun 18:00 EDT = 22:00 UTC |
//! | exit | 09:00 on the next trading day | Mon 09:00 EDT = 13:00 UTC |
//!
//! A single mid-week holiday is a break too (`closed_days = 1`).

// Consumers land in the next wave (`x-weekend-fade-strategy`); config builds
// the calendars and `domain/schedule.rs` parses clock times here today.
#![allow(dead_code)]

use std::collections::BTreeSet;

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, Timelike, Weekday};
use serde::{Deserialize, Serialize};

use crate::domain::tz::Zone;

/// Minutes in a day.
pub const DAY_MIN: u32 = 1440;
/// Minutes in a week (weekly windows count from Monday 00:00 local).
pub const WEEK_MIN: u32 = 7 * DAY_MIN;
/// Bound on every day walk (`prev_trading_day`, `next_trading_day`,
/// `weekend_window`): a calendar without a trading day in this span yields
/// `None`.
const MAX_WALK_DAYS: u32 = 31;

/// Session state at an instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Session {
    Open,
    Pre,
    Post,
    Overnight,
    Closed,
}

impl Session {
    pub fn as_str(self) -> &'static str {
        match self {
            Session::Open => "open",
            Session::Pre => "pre",
            Session::Post => "post",
            Session::Overnight => "overnight",
            Session::Closed => "closed",
        }
    }

    pub fn is_closed(self) -> bool {
        self == Session::Closed
    }
}

/// One `[xmarket.calendars.<id>]` row, built.
#[derive(Debug, Clone, PartialEq)]
pub enum Calendar {
    Exchange(ExchangeCalendar),
    Weekly(WeeklyWindow),
    AlwaysOpen,
}

impl Calendar {
    pub fn session(&self, utc_ms: i64) -> Session {
        match self {
            Calendar::Exchange(c) => c.session(utc_ms),
            Calendar::Weekly(w) => w.session(utc_ms),
            Calendar::AlwaysOpen => Session::Open,
        }
    }

    /// Trading days and the weekend clock exist only for `exchange` rows.
    pub fn exchange(&self) -> Option<&ExchangeCalendar> {
        match self {
            Calendar::Exchange(c) => Some(c),
            _ => None,
        }
    }
}

/// Exchange sessions; every time is minutes after local midnight.
#[derive(Debug, Clone, PartialEq)]
pub struct ExchangeCalendar {
    pub zone: Zone,
    /// Core session `[open, close)`.
    pub open: u32,
    pub close: u32,
    /// Pre-market `[pre, open)`.
    pub pre: Option<u32>,
    /// Post-market `[close, post)`.
    pub post: Option<u32>,
    /// Overnight from `post` on the calendar day before a trading day to that
    /// day's `pre` (needs both).
    pub overnight: bool,
    /// Core close on an `early_closes` date.
    pub early_close: Option<u32>,
    /// Post-market end on an `early_closes` date; `None` = no post-market.
    pub early_post: Option<u32>,
    pub holidays: BTreeSet<NaiveDate>,
    pub early_closes: BTreeSet<NaiveDate>,
}

/// Local times of the weekend clock, minutes after midnight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WeekendTimes {
    pub anchor: u32,
    pub entry: u32,
    pub exit: u32,
}

impl WeekendTimes {
    /// 20:00 anchor · 18:00 entry · 09:00 exit (feasibility study rule W).
    pub const DEFAULT: WeekendTimes = WeekendTimes {
        anchor: 20 * 60,
        entry: 18 * 60,
        exit: 9 * 60,
    };
}

/// The weekend clock around one break (see the module doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WeekendWindow {
    /// Last trading day before the break.
    pub last_trading_day: NaiveDate,
    /// First trading day after it.
    pub next_trading_day: NaiveDate,
    /// Non-trading days in the break (2 = a plain weekend).
    pub closed_days: u32,
    pub anchor_ms: i64,
    pub entry_ms: i64,
    pub exit_ms: i64,
}

impl ExchangeCalendar {
    /// Mon–Fri and not a holiday.
    pub fn is_trading_day(&self, date: NaiveDate) -> bool {
        !matches!(date.weekday(), Weekday::Sat | Weekday::Sun) && !self.holidays.contains(&date)
    }

    /// A trading day listed in `early_closes` (with `early_close` set).
    pub fn is_early_close(&self, date: NaiveDate) -> bool {
        self.early_close.is_some() && self.early_closes.contains(&date) && self.is_trading_day(date)
    }

    /// Last trading day strictly before `date`.
    pub fn prev_trading_day(&self, date: NaiveDate) -> Option<NaiveDate> {
        self.walk(date, -1)
    }

    /// First trading day strictly after `date`.
    pub fn next_trading_day(&self, date: NaiveDate) -> Option<NaiveDate> {
        self.walk(date, 1)
    }

    fn walk(&self, date: NaiveDate, step: i64) -> Option<NaiveDate> {
        let mut d = date;
        for _ in 0..MAX_WALK_DAYS {
            d = d.checked_add_signed(Duration::days(step))?;
            if self.is_trading_day(d) {
                return Some(d);
            }
        }
        None
    }

    pub fn session(&self, utc_ms: i64) -> Session {
        let (day, m) = local_minute(self.zone.to_local(utc_ms));
        if self.is_trading_day(day) {
            let (close, post) = if self.is_early_close(day) {
                (self.early_close.unwrap_or(self.close), self.early_post)
            } else {
                (self.close, self.post)
            };
            if (self.open..close).contains(&m) {
                return Session::Open;
            }
            if self.pre.is_some_and(|pre| (pre..self.open).contains(&m)) {
                return Session::Pre;
            }
            if post.is_some_and(|post| (close..post).contains(&m)) {
                return Session::Post;
            }
            if self.overnight && self.pre.is_some_and(|pre| m < pre) {
                return Session::Overnight;
            }
        }
        let evening = self.overnight && self.post.is_some_and(|post| m >= post);
        if evening && day.succ_opt().is_some_and(|d| self.is_trading_day(d)) {
            return Session::Overnight;
        }
        Session::Closed
    }

    /// The weekend clock with the default times ([`WeekendTimes::DEFAULT`]).
    pub fn weekend_window(&self, utc_ms: i64) -> Option<WeekendWindow> {
        self.weekend_window_with(utc_ms, WeekendTimes::DEFAULT)
    }

    /// The window still running at `utc_ms` (`exit_ms > utc_ms`), else the
    /// next one. `None` when no trading day is in reach.
    pub fn weekend_window_with(&self, utc_ms: i64, times: WeekendTimes) -> Option<WeekendWindow> {
        let today = self.zone.local_date(utc_ms);
        let mut day = if self.is_trading_day(today) {
            today
        } else {
            self.next_trading_day(today)?
        };
        for _ in 0..MAX_WALK_DAYS {
            let before = day.pred_opt()?;
            if !self.is_trading_day(before) {
                let last = self.prev_trading_day(day)?;
                let window = WeekendWindow {
                    last_trading_day: last,
                    next_trading_day: day,
                    closed_days: ((day - last).num_days() - 1) as u32,
                    anchor_ms: self.at(last, times.anchor),
                    entry_ms: self.at(before, times.entry),
                    exit_ms: self.at(day, times.exit),
                };
                if window.exit_ms > utc_ms {
                    return Some(window);
                }
            }
            day = self.next_trading_day(day)?;
        }
        None
    }

    fn at(&self, date: NaiveDate, minute: u32) -> i64 {
        self.zone.at(date, minute / 60, minute % 60)
    }
}

/// One weekly window `[open, close)` in minutes after Monday 00:00 local
/// (wraps when `close <= open`), closed daily in `daily_break` (minutes after
/// midnight, may wrap).
#[derive(Debug, Clone, PartialEq)]
pub struct WeeklyWindow {
    pub zone: Zone,
    pub open: u32,
    pub close: u32,
    pub daily_break: Option<(u32, u32)>,
}

impl WeeklyWindow {
    pub fn session(&self, utc_ms: i64) -> Session {
        let local = self.zone.to_local(utc_ms);
        let (_, m) = local_minute(local);
        let week_minute = local.weekday().num_days_from_monday() * DAY_MIN + m;
        let inside = in_cyclic(self.open, self.close, week_minute);
        let on_break = self
            .daily_break
            .is_some_and(|(from, to)| in_cyclic(from, to, m));
        if inside && !on_break {
            Session::Open
        } else {
            Session::Closed
        }
    }
}

/// `x` in the cyclic half-open range `[from, to)`; `to <= from` wraps.
fn in_cyclic(from: u32, to: u32, x: u32) -> bool {
    if from < to {
        (from..to).contains(&x)
    } else {
        x >= from || x < to
    }
}

fn local_minute(local: NaiveDateTime) -> (NaiveDate, u32) {
    (local.date(), local.hour() * 60 + local.minute())
}

/// `"HH:MM"` (00:00–23:59) → minutes after midnight.
pub fn parse_hm(s: &str) -> Option<u32> {
    let (h, m) = s.trim().split_once(':')?;
    if h.len() != 2 || m.len() != 2 {
        return None;
    }
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

/// `"Sun 20:00"` (three-letter day, any case) → minutes after Monday 00:00.
pub fn parse_week_hm(s: &str) -> Option<u32> {
    let (day, hm) = s.trim().split_once(' ')?;
    let day = match day.to_ascii_lowercase().as_str() {
        "mon" => 0,
        "tue" => 1,
        "wed" => 2,
        "thu" => 3,
        "fri" => 4,
        "sat" => 5,
        "sun" => 6,
        _ => return None,
    };
    Some(day * DAY_MIN + parse_hm(hm)?)
}

/// `"YYYY-MM-DD"`.
pub fn parse_date(s: &str) -> Option<NaiveDate> {
    let s = s.trim();
    (s.len() == 10)
        .then(|| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hm(s: &str) -> u32 {
        parse_hm(s).unwrap()
    }

    fn date(s: &str) -> NaiveDate {
        parse_date(s).unwrap()
    }

    fn dates(list: &[&str]) -> BTreeSet<NaiveDate> {
        list.iter().map(|s| date(s)).collect()
    }

    fn utc(s: &str) -> i64 {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M")
            .unwrap()
            .and_utc()
            .timestamp_millis()
    }

    /// Wall-clock time `s` in New York → UTC ms.
    fn et(s: &str) -> i64 {
        Zone::NewYork.to_utc_ms(NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap())
    }

    fn paris(s: &str) -> i64 {
        Zone::Paris.to_utc_ms(NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap())
    }

    /// NYSE with the dates these tests touch (full list: the fixture).
    fn nyse() -> ExchangeCalendar {
        ExchangeCalendar {
            zone: Zone::NewYork,
            open: hm("09:30"),
            close: hm("16:00"),
            pre: Some(hm("04:00")),
            post: Some(hm("20:00")),
            overnight: true,
            early_close: Some(hm("13:00")),
            early_post: Some(hm("17:00")),
            holidays: dates(&[
                "2026-09-07",
                "2026-11-26",
                "2026-12-25",
                "2027-01-01",
                "2027-01-18",
            ]),
            early_closes: dates(&["2026-11-27", "2026-12-24"]),
        }
    }

    fn check(cal: &ExchangeCalendar, cases: &[(&str, Session)]) {
        for (t, want) in cases {
            assert_eq!(cal.session(et(t)), *want, "{t} ET");
        }
    }

    #[test]
    fn sessions_of_a_regular_week() {
        use Session::*;
        check(
            &nyse(),
            &[
                ("2026-09-25 19:59", Post),
                ("2026-09-25 20:00", Closed), // Fri: Sat is no trading day
                ("2026-09-26 12:00", Closed),
                ("2026-09-27 19:59", Closed),
                ("2026-09-27 20:00", Overnight), // Sun night → Mon
                ("2026-09-28 03:59", Overnight),
                ("2026-09-28 04:00", Pre),
                ("2026-09-28 09:29", Pre),
                ("2026-09-28 09:30", Open),
                ("2026-09-28 15:59", Open),
                ("2026-09-28 16:00", Post),
                ("2026-09-28 19:59", Post),
                ("2026-09-28 20:00", Overnight),
                ("2026-09-29 00:00", Overnight),
            ],
        );
        assert_eq!(Session::Overnight.as_str(), "overnight");
        assert!(Session::Closed.is_closed() && !Session::Pre.is_closed());
    }

    #[test]
    fn weekend_window_2026_09_26_to_28() {
        let cal = nyse();
        let want = WeekendWindow {
            last_trading_day: date("2026-09-25"),
            next_trading_day: date("2026-09-28"),
            closed_days: 2,
            anchor_ms: utc("2026-09-26 00:00"),
            entry_ms: utc("2026-09-27 22:00"),
            exit_ms: utc("2026-09-28 13:00"),
        };
        // Same window from before the anchor until just before the exit.
        for t in [
            "2026-09-24 12:00",
            "2026-09-25 10:00",
            "2026-09-25 20:00",
            "2026-09-26 12:00",
            "2026-09-27 18:00",
            "2026-09-28 08:59",
        ] {
            assert_eq!(cal.weekend_window(et(t)), Some(want), "{t} ET");
        }
        // At the exit the next weekend starts counting.
        let next = cal.weekend_window(et("2026-09-28 09:00")).unwrap();
        assert_eq!(next.last_trading_day, date("2026-10-02"));
        assert_eq!(next.anchor_ms, utc("2026-10-03 00:00"));
        assert_eq!(next.entry_ms, utc("2026-10-04 22:00"));
        assert_eq!(next.exit_ms, utc("2026-10-05 13:00"));
    }

    /// US clocks fall back on Sun 2026-11-01: anchor in EDT, entry and exit in EST.
    #[test]
    fn dst_weekend_2026_11_01() {
        let cal = nyse();
        let w = cal.weekend_window(utc("2026-10-31 12:00")).unwrap();
        assert_eq!(w.anchor_ms, utc("2026-10-31 00:00")); // Fri 20:00 EDT
        assert_eq!(w.entry_ms, utc("2026-11-01 23:00")); // Sun 18:00 EST
        assert_eq!(w.exit_ms, utc("2026-11-02 14:00")); // Mon 09:00 EST
        assert_eq!(w.closed_days, 2);
        assert_eq!(cal.session(utc("2026-10-30 20:00")), Session::Post); // 16:00 EDT
        assert_eq!(cal.session(utc("2026-11-02 00:59")), Session::Closed); // Sun 19:59 EST
        assert_eq!(cal.session(utc("2026-11-02 01:00")), Session::Overnight); // Sun 20:00 EST
        assert_eq!(cal.session(utc("2026-11-02 14:30")), Session::Open); // Mon 09:30 EST
                                                                         // Spring forward, Sun 2027-03-14: the Sunday-night overnight starts 20:00 EDT.
        assert_eq!(cal.session(utc("2027-03-14 23:59")), Session::Closed);
        assert_eq!(cal.session(utc("2027-03-15 00:00")), Session::Overnight);
    }

    /// MLK Day 2027-01-18: entry moves to Mon 18:00, exit to Tue 09:00.
    #[test]
    fn three_day_weekend_mlk_2027() {
        let cal = nyse();
        assert!(!cal.is_trading_day(date("2027-01-18")));
        assert_eq!(
            cal.next_trading_day(date("2027-01-15")),
            Some(date("2027-01-19"))
        );
        assert_eq!(
            cal.prev_trading_day(date("2027-01-19")),
            Some(date("2027-01-15"))
        );
        let want = WeekendWindow {
            last_trading_day: date("2027-01-15"),
            next_trading_day: date("2027-01-19"),
            closed_days: 3,
            anchor_ms: utc("2027-01-16 01:00"), // Fri 20:00 EST
            entry_ms: utc("2027-01-18 23:00"),  // Mon 18:00 EST
            exit_ms: utc("2027-01-19 14:00"),   // Tue 09:00 EST
        };
        for t in ["2027-01-15 12:00", "2027-01-17 18:00", "2027-01-19 08:59"] {
            assert_eq!(cal.weekend_window(et(t)), Some(want), "{t} ET");
        }
        use Session::*;
        check(
            &cal,
            &[
                ("2027-01-17 20:00", Closed), // no overnight into a holiday
                ("2027-01-18 10:00", Closed),
                ("2027-01-18 20:00", Overnight),
                ("2027-01-19 04:00", Pre),
            ],
        );
    }

    #[test]
    fn thanksgiving_and_early_close_2026() {
        let cal = nyse();
        assert!(cal.is_early_close(date("2026-11-27")));
        assert!(!cal.is_early_close(date("2026-11-26")));
        use Session::*;
        check(
            &cal,
            &[
                ("2026-11-25 19:59", Post),
                ("2026-11-25 20:00", Closed), // Thu is a holiday
                ("2026-11-26 12:00", Closed),
                ("2026-11-26 20:00", Overnight), // into the early-close Friday
                ("2026-11-27 04:00", Pre),
                ("2026-11-27 12:59", Open),
                ("2026-11-27 13:00", Post),
                ("2026-11-27 16:59", Post),
                ("2026-11-27 17:00", Closed),
                ("2026-11-27 20:00", Closed),
            ],
        );
        // The one-day break around Thanksgiving, then the weekend after it.
        let thu = cal.weekend_window(et("2026-11-25 10:00")).unwrap();
        assert_eq!(thu.closed_days, 1);
        assert_eq!(thu.anchor_ms, utc("2026-11-26 01:00")); // Wed 20:00 EST
        assert_eq!(thu.entry_ms, utc("2026-11-26 23:00")); // Thu 18:00 EST
        assert_eq!(thu.exit_ms, utc("2026-11-27 14:00")); // Fri 09:00 EST
        let sat = cal.weekend_window(et("2026-11-27 10:00")).unwrap();
        assert_eq!(sat.last_trading_day, date("2026-11-27"));
        assert_eq!(sat.anchor_ms, utc("2026-11-28 01:00")); // early-close Fri, 20:00
        assert_eq!(sat.exit_ms, utc("2026-11-30 14:00"));
    }

    #[test]
    fn christmas_eve_early_close_then_a_three_day_break() {
        let cal = nyse();
        use Session::*;
        check(
            &cal,
            &[
                ("2026-12-24 13:00", Post),
                ("2026-12-24 17:00", Closed),
                ("2026-12-24 20:00", Closed),
            ],
        );
        let w = cal.weekend_window(et("2026-12-24 10:00")).unwrap();
        assert_eq!(w.closed_days, 3);
        assert_eq!(w.anchor_ms, utc("2026-12-25 01:00")); // Thu 20:00 EST
        assert_eq!(w.entry_ms, utc("2026-12-27 23:00")); // Sun 18:00 EST
        assert_eq!(w.exit_ms, utc("2026-12-28 14:00"));
    }

    #[test]
    fn weekly_windows_trade_xyz_rh_tokenization_and_24x7() {
        let ny = |open: &str, close: &str, brk: Option<(&str, &str)>| {
            Calendar::Weekly(WeeklyWindow {
                zone: Zone::NewYork,
                open: parse_week_hm(open).unwrap(),
                close: parse_week_hm(close).unwrap(),
                daily_break: brk.map(|(a, b)| (hm(a), hm(b))),
            })
        };
        let stocks = ny("Sun 20:00", "Fri 20:00", None);
        let indices = ny("Sun 18:00", "Fri 17:00", Some(("17:00", "18:00")));
        let fx = ny("Sun 17:00", "Fri 17:00", None);
        use Session::*;
        for (cal, t, want) in [
            (&stocks, "2026-09-27 19:59", Closed),
            (&stocks, "2026-09-27 20:00", Open),
            (&stocks, "2026-09-30 03:00", Open),
            (&stocks, "2026-10-02 19:59", Open),
            (&stocks, "2026-10-02 20:00", Closed),
            (&indices, "2026-09-27 17:59", Closed),
            (&indices, "2026-09-27 18:00", Open),
            (&indices, "2026-09-29 16:59", Open),
            (&indices, "2026-09-29 17:30", Closed), // daily break
            (&indices, "2026-09-29 18:00", Open),
            (&indices, "2026-10-02 17:00", Closed),
            (&fx, "2026-09-27 17:00", Open),
            (&fx, "2026-10-02 16:59", Open),
            (&fx, "2026-10-02 17:00", Closed),
        ] {
            assert_eq!(cal.session(et(t)), want, "{t} ET");
        }
        let rh = Calendar::Weekly(WeeklyWindow {
            zone: Zone::Paris,
            open: parse_week_hm("Mon 02:00").unwrap(),
            close: parse_week_hm("Sat 02:00").unwrap(),
            daily_break: None,
        });
        for (t, want) in [
            ("2026-10-03 01:59", Open),
            ("2026-10-03 02:00", Closed), // Sat 02:00 CEST = 00:00 UTC
            ("2026-10-05 01:59", Closed),
            ("2026-10-05 02:00", Open),
            ("2026-10-26 02:00", Open), // first Monday on CET
        ] {
            assert_eq!(rh.session(paris(t)), want, "{t} Paris");
        }
        assert_eq!(rh.session(utc("2026-10-26 00:59")), Closed);
        assert_eq!(rh.session(utc("2026-10-26 01:00")), Open);
        assert_eq!(
            Calendar::AlwaysOpen.session(utc("2026-10-03 12:00")),
            Session::Open
        );
        assert!(Calendar::AlwaysOpen.exchange().is_none());
        assert!(Calendar::Exchange(nyse()).exchange().is_some());
    }

    #[test]
    fn walks_are_bounded() {
        let mut cal = nyse();
        let start = date("2027-02-01");
        cal.holidays = (0..60).map(|i| start + Duration::days(i)).collect();
        assert_eq!(cal.next_trading_day(start), None);
        assert_eq!(cal.weekend_window(et("2027-02-02 12:00")), None);
        assert_eq!(
            cal.prev_trading_day(start),
            Some(date("2027-01-29")),
            "walks back out of the closed span"
        );
    }

    #[test]
    fn parsers() {
        assert_eq!(parse_hm("09:30"), Some(570));
        assert_eq!(parse_hm("23:59"), Some(1439));
        for bad in ["24:00", "9:30", "09:60", "0930", "", "ab:cd"] {
            assert_eq!(parse_hm(bad), None, "{bad:?}");
        }
        assert_eq!(parse_week_hm("Mon 00:00"), Some(0));
        assert_eq!(parse_week_hm("sun 20:00"), Some(6 * DAY_MIN + 1200));
        assert_eq!(parse_week_hm("FRI 17:00"), Some(4 * DAY_MIN + 1020));
        for bad in ["Sunday 20:00", "Sun", "Sun 25:00", "20:00"] {
            assert_eq!(parse_week_hm(bad), None, "{bad:?}");
        }
        assert_eq!(parse_date("2026-11-26"), Some(date("2026-11-26")));
        for bad in ["2026-02-30", "2026-1-5", "26-11-26", "2026/11/26"] {
            assert_eq!(parse_date(bad), None, "{bad:?}");
        }
        assert!(in_cyclic(10, 20, 10) && !in_cyclic(10, 20, 20));
        assert!(in_cyclic(WEEK_MIN - 10, 5, 0) && !in_cyclic(WEEK_MIN - 10, 5, 5));
    }
}
