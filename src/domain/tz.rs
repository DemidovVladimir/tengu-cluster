//! Civil time in the zones xmarket schedules against, DST rules hand-rolled
//! (no tz database). Pure; `chrono` only for calendar arithmetic.
//!
//! | Zone | Standard | Daylight | Switch |
//! |---|---|---|---|
//! | `America/New_York` | EST UTC−5 | EDT UTC−4 | 2nd Sun Mar 02:00 EST → 03:00 EDT; 1st Sun Nov 02:00 EDT → 01:00 EST |
//! | `Europe/Paris` | CET UTC+1 | CEST UTC+2 | last Sun Mar and last Sun Oct, 01:00 UTC |
//! | `UTC` | UTC | — | — |
//!
//! Local → UTC: a wall time inside the spring-forward gap is read with the
//! standard offset (02:30 on the US switch day → 03:30 EDT); one inside the
//! fall-back overlap resolves to the earlier instant (daylight offset).

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, Weekday};

const HOUR_MS: i64 = 3_600_000;

/// A zone with a known DST rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Zone {
    NewYork,
    Paris,
    Utc,
}

impl Zone {
    /// IANA name → zone; only the three supported zones parse.
    pub fn parse(name: &str) -> Option<Zone> {
        match name.trim() {
            "America/New_York" => Some(Zone::NewYork),
            "Europe/Paris" => Some(Zone::Paris),
            "UTC" | "Etc/UTC" => Some(Zone::Utc),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Zone::NewYork => "America/New_York",
            Zone::Paris => "Europe/Paris",
            Zone::Utc => "UTC",
        }
    }

    fn standard_offset_ms(self) -> i64 {
        match self {
            Zone::NewYork => -5 * HOUR_MS,
            Zone::Paris => HOUR_MS,
            Zone::Utc => 0,
        }
    }

    /// Daylight time in force at `utc_ms`.
    pub fn is_dst(self, utc_ms: i64) -> bool {
        let year = naive_utc(utc_ms).year();
        match self {
            Zone::Utc => false,
            Zone::NewYork => {
                // 02:00 EST = 07:00 UTC; 02:00 EDT = 06:00 UTC.
                let start = utc_ms_of(nth_weekday(year, 3, Weekday::Sun, 2), 7);
                let end = utc_ms_of(nth_weekday(year, 11, Weekday::Sun, 1), 6);
                (start..end).contains(&utc_ms)
            }
            Zone::Paris => {
                let start = utc_ms_of(last_weekday(year, 3, Weekday::Sun), 1);
                let end = utc_ms_of(last_weekday(year, 10, Weekday::Sun), 1);
                (start..end).contains(&utc_ms)
            }
        }
    }

    /// Offset from UTC at `utc_ms`, in ms (EDT = −14 400 000).
    pub fn offset_ms(self, utc_ms: i64) -> i64 {
        self.standard_offset_ms() + if self.is_dst(utc_ms) { HOUR_MS } else { 0 }
    }

    /// Local wall-clock time at `utc_ms`.
    pub fn to_local(self, utc_ms: i64) -> NaiveDateTime {
        naive_utc(utc_ms + self.offset_ms(utc_ms))
    }

    /// Local calendar date at `utc_ms`.
    pub fn local_date(self, utc_ms: i64) -> NaiveDate {
        self.to_local(utc_ms).date()
    }

    /// UTC ms of the wall-clock time `local` in this zone (gap → standard
    /// offset, overlap → earlier instant; see the module doc).
    pub fn to_utc_ms(self, local: NaiveDateTime) -> i64 {
        let wall = local.and_utc().timestamp_millis();
        let standard = wall - self.standard_offset_ms();
        let daylight = standard - HOUR_MS;
        if self != Zone::Utc && self.is_dst(daylight) {
            daylight
        } else {
            standard
        }
    }

    /// UTC ms of `date` at `hour:minute` local time.
    pub fn at(self, date: NaiveDate, hour: u32, minute: u32) -> i64 {
        let time =
            NaiveTime::from_hms_opt(hour.min(23), minute.min(59), 0).unwrap_or(NaiveTime::MIN);
        self.to_utc_ms(date.and_time(time))
    }
}

/// `n`-th (1-based) `weekday` of `month`.
pub fn nth_weekday(year: i32, month: u32, weekday: Weekday, n: u32) -> NaiveDate {
    NaiveDate::from_weekday_of_month_opt(year, month, weekday, n as u8)
        .unwrap_or_else(|| NaiveDate::from_ymd_opt(year, month, 1).unwrap_or_default())
}

/// Last `weekday` of `month`.
pub fn last_weekday(year: i32, month: u32, weekday: Weekday) -> NaiveDate {
    let (ny, nm) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let mut d = NaiveDate::from_ymd_opt(ny, nm, 1).unwrap_or_default() - Duration::days(1);
    while d.weekday() != weekday {
        d -= Duration::days(1);
    }
    d
}

fn naive_utc(utc_ms: i64) -> NaiveDateTime {
    chrono::DateTime::from_timestamp_millis(utc_ms)
        .unwrap_or_default()
        .naive_utc()
}

fn utc_ms_of(date: NaiveDate, hour_utc: u32) -> i64 {
    date.and_hms_opt(hour_utc, 0, 0)
        .unwrap_or_default()
        .and_utc()
        .timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(s: &str) -> i64 {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M")
            .unwrap()
            .and_utc()
            .timestamp_millis()
    }

    fn local(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap()
    }

    #[test]
    fn parses_supported_zones_only() {
        assert_eq!(Zone::parse("America/New_York"), Some(Zone::NewYork));
        assert_eq!(Zone::parse("Europe/Paris"), Some(Zone::Paris));
        assert_eq!(Zone::parse("UTC"), Some(Zone::Utc));
        assert_eq!(Zone::parse("Asia/Almaty"), None);
        assert_eq!(Zone::NewYork.name(), "America/New_York");
    }

    #[test]
    fn us_switch_dates_2026_to_2028() {
        // 2nd Sunday of March / 1st Sunday of November.
        for (y, mar, nov) in [(2026, 8, 1), (2027, 14, 7), (2028, 12, 5)] {
            assert_eq!(nth_weekday(y, 3, Weekday::Sun, 2).day(), mar, "{y} March");
            assert_eq!(
                nth_weekday(y, 11, Weekday::Sun, 1).day(),
                nov,
                "{y} November"
            );
        }
    }

    #[test]
    fn eu_switch_dates_2026_to_2028() {
        for (y, mar, oct) in [(2026, 29, 25), (2027, 28, 31), (2028, 26, 29)] {
            assert_eq!(last_weekday(y, 3, Weekday::Sun).day(), mar, "{y} March");
            assert_eq!(last_weekday(y, 10, Weekday::Sun).day(), oct, "{y} October");
        }
    }

    #[test]
    fn new_york_offsets_around_the_switches() {
        let ny = Zone::NewYork;
        // 2026-03-08 06:59 UTC = 01:59 EST; 07:00 UTC = 03:00 EDT.
        assert_eq!(ny.offset_ms(utc("2026-03-08 06:59")), -5 * HOUR_MS);
        assert_eq!(ny.offset_ms(utc("2026-03-08 07:00")), -4 * HOUR_MS);
        assert_eq!(
            ny.to_local(utc("2026-03-08 07:00")),
            local("2026-03-08 03:00")
        );
        // 2026-11-01 05:59 UTC = 01:59 EDT; 06:00 UTC = 01:00 EST.
        assert_eq!(ny.offset_ms(utc("2026-11-01 05:59")), -4 * HOUR_MS);
        assert_eq!(ny.offset_ms(utc("2026-11-01 06:00")), -5 * HOUR_MS);
        assert_eq!(
            ny.to_local(utc("2026-11-01 06:00")),
            local("2026-11-01 01:00")
        );
    }

    #[test]
    fn new_york_local_to_utc_including_gap_and_overlap() {
        let ny = Zone::NewYork;
        // Weekend-fade clock, weekend of 2026-10-02 (EDT): Fri 20:00 = Sat 00:00 UTC.
        assert_eq!(
            ny.to_utc_ms(local("2026-10-02 20:00")),
            utc("2026-10-03 00:00")
        );
        assert_eq!(
            ny.to_utc_ms(local("2026-10-04 18:00")),
            utc("2026-10-04 22:00")
        );
        assert_eq!(
            ny.to_utc_ms(local("2026-10-05 09:00")),
            utc("2026-10-05 13:00")
        );
        // After the November switch (EST): Sun 18:00 = 23:00 UTC.
        assert_eq!(
            ny.to_utc_ms(local("2026-11-01 18:00")),
            utc("2026-11-01 23:00")
        );
        assert_eq!(
            ny.to_utc_ms(local("2026-10-30 20:00")),
            utc("2026-10-31 00:00")
        );
        // Gap: 02:30 on 2026-03-08 does not exist → standard reading = 03:30 EDT.
        assert_eq!(
            ny.to_utc_ms(local("2026-03-08 02:30")),
            utc("2026-03-08 07:30")
        );
        // Overlap: 01:30 on 2026-11-01 happens twice → the earlier (EDT) one.
        assert_eq!(
            ny.to_utc_ms(local("2026-11-01 01:30")),
            utc("2026-11-01 05:30")
        );
        assert_eq!(
            ny.at(NaiveDate::from_ymd_opt(2026, 10, 4).unwrap(), 18, 0),
            utc("2026-10-04 22:00")
        );
    }

    #[test]
    fn round_trip_every_hour_of_2026_in_both_zones() {
        for zone in [Zone::NewYork, Zone::Paris, Zone::Utc] {
            let mut t = utc("2026-01-01 00:00");
            let end = utc("2027-01-01 00:00");
            while t < end {
                let back = zone.to_utc_ms(zone.to_local(t));
                // Only the repeated fall-back hour maps to its earlier twin.
                assert!(back == t || back == t - HOUR_MS, "{zone:?} {t} → {back}");
                t += HOUR_MS;
            }
        }
    }

    #[test]
    fn paris_offsets_around_the_switches() {
        let p = Zone::Paris;
        assert_eq!(p.offset_ms(utc("2026-03-29 00:59")), HOUR_MS);
        assert_eq!(p.offset_ms(utc("2026-03-29 01:00")), 2 * HOUR_MS);
        assert_eq!(p.offset_ms(utc("2026-10-25 00:59")), 2 * HOUR_MS);
        assert_eq!(p.offset_ms(utc("2026-10-25 01:00")), HOUR_MS);
        // RH tokenization window edge: Sat 02:00 Paris (CEST) = Sat 00:00 UTC.
        assert_eq!(
            p.to_utc_ms(local("2026-10-03 02:00")),
            utc("2026-10-03 00:00")
        );
        assert_eq!(
            p.local_date(utc("2026-10-02 22:30")),
            NaiveDate::from_ymd_opt(2026, 10, 3).unwrap()
        );
    }
}
