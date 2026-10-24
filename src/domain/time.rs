//! Time helpers every layer shares — core, no vertical owns them: the CLI's
//! `--from` / `--to`, feed schedules, calendars, reports, source records.
//! Instants are epoch ms; reports print RFC 3339 UTC.

/// Minutes in a day.
pub const DAY_MIN: u32 = 1440;

/// `"HH:MM"` (00:00–23:59) → minutes after midnight.
pub fn parse_hm(s: &str) -> Option<u32> {
    let (h, m) = s.trim().split_once(':')?;
    if h.len() != 2 || m.len() != 2 {
        return None;
    }
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

/// Epoch ms, RFC 3339 (`2026-10-02T20:00:00-04:00`) or a UTC date
/// (`2026-10-02` = its midnight UTC).
pub fn parse_time(s: &str) -> Result<i64, String> {
    let s = s.trim();
    if let Ok(ms) = s.parse::<i64>() {
        return Ok(ms);
    }
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(t.timestamp_millis());
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        if let Some(t) = d.and_hms_opt(0, 0, 0) {
            return Ok(t.and_utc().timestamp_millis());
        }
    }
    Err(format!(
        "`{s}` is not epoch ms, RFC 3339 (2026-10-02T20:00:00-04:00) or a date (2026-10-02)"
    ))
}

/// RFC 3339 in UTC with seconds (`2026-10-03T00:00:00Z`) — how reports show
/// instants.
pub fn fmt_time(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_else(|| ms.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_parse_and_format() {
        assert_eq!(parse_time("1759449600000").unwrap(), 1_759_449_600_000);
        assert_eq!(
            parse_time("2026-10-02T20:00:00-04:00").unwrap(),
            parse_time("2026-10-03").unwrap()
        );
        assert_eq!(
            fmt_time(parse_time("2026-10-03").unwrap()),
            "2026-10-03T00:00:00Z"
        );
        assert!(parse_time("friday").is_err());
    }

    #[test]
    fn hours_and_minutes() {
        assert_eq!(parse_hm("09:30"), Some(570));
        assert_eq!(parse_hm("23:59"), Some(1439));
        for bad in ["24:00", "9:30", "09:60", "0930", "", "ab:cd"] {
            assert_eq!(parse_hm(bad), None, "{bad:?}");
        }
    }
}
