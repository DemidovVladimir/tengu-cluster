//! `[xmarket]` — sandbox-wide settings of the xmarket runtime: the
//! install-wide state directory (tracker convention 3) and the session
//! calendars `[xmarket.calendars.<id>]` (convention 18, evaluated by
//! `domain/calendar.rs`). Later milestones add venues and lifecycle knobs.
//! `deny_unknown_fields`.
//!
//! | Calendar field | Kind | Value |
//! |---|---|---|
//! | `kind` | all | `exchange` \| `weekly` \| `24x7` |
//! | `tz` | exchange, weekly | `America/New_York` \| `Europe/Paris` \| `UTC` (`domain/tz.rs`) |
//! | `core` | exchange | `["09:30", "16:00"]` — regular session |
//! | `pre` / `post` | exchange | `"04:00"` pre-market from · `"20:00"` post-market until |
//! | `overnight` | exchange | `true` = `post` of the day before a trading day → its `pre` (needs both) |
//! | `early_close` / `early_post` | exchange | `"13:00"` core close · `"17:00"` post-market until, on `early_closes` dates |
//! | `holidays` / `early_closes` | exchange | `YYYY-MM-DD` weekdays (quoted or TOML dates) |
//! | `open` / `close` | weekly | `"Sun 20:00"` / `"Fri 20:00"` — local; the window may wrap the week |
//! | `daily_break` | weekly | `["17:00", "18:00"]` — closed daily inside the window |

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize};

use crate::domain::calendar::{
    parse_date, parse_hm, parse_week_hm, Calendar, ExchangeCalendar, WeeklyWindow,
};
use crate::domain::tz::Zone;

/// `[xmarket]` section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct XmarketConfig {
    /// Directory name under `<TENGU_HOME>/state/` that holds the install-wide
    /// stores (`ledger.db`, `runtime.db`, `history/`, …): `xmarket` for the
    /// main sandbox, `xmarket-weekend` for the weekend run. One path segment;
    /// `tengu prune` never deletes `state/` (only `state/flows`).
    #[serde(default = "default_state")]
    pub state: String,
    /// `[xmarket.calendars.<id>]` — session calendars by id (module doc).
    #[serde(default)]
    pub calendars: BTreeMap<String, CalendarConfig>,
}

impl Default for XmarketConfig {
    fn default() -> Self {
        Self {
            state: default_state(),
            calendars: BTreeMap::new(),
        }
    }
}

fn default_state() -> String {
    "xmarket".to_string()
}

impl XmarketConfig {
    /// `<tengu_home>/state/<state>`.
    pub fn state_dir(&self, tengu_home: &Path) -> PathBuf {
        tengu_home.join("state").join(&self.state)
    }

    /// Every calendar that builds (an invalid one fails `validation_errors`).
    pub fn calendars(&self) -> BTreeMap<String, Calendar> {
        self.calendars
            .iter()
            .filter_map(|(id, c)| c.build().ok().map(|cal| (id.clone(), cal)))
            .collect()
    }

    pub fn validation_errors(&self) -> Vec<String> {
        let mut errors = Vec::new();
        let s = self.state.as_str();
        let ok = !s.is_empty()
            && s != "."
            && s != ".."
            && s != "flows"
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
        if !ok {
            errors.push(format!(
                "xmarket.state `{s}` must be one directory name ([A-Za-z0-9._-], not `.`, `..` or `flows`)"
            ));
        }
        for (id, cal) in &self.calendars {
            if let Err(issues) = cal.build() {
                errors.extend(
                    issues
                        .into_iter()
                        .map(|i| format!("xmarket.calendars.{id}: {i}")),
                );
            }
        }
        errors
    }
}

/// `kind` of a calendar row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CalendarKind {
    #[serde(rename = "exchange")]
    Exchange,
    #[serde(rename = "weekly")]
    Weekly,
    #[serde(rename = "24x7")]
    AlwaysOpen,
}

/// `[xmarket.calendars.<id>]` — one calendar; fields per kind in the module
/// doc (a field of another kind is an error).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalendarConfig {
    pub kind: CalendarKind,
    pub tz: Option<String>,
    pub core: Option<Vec<String>>,
    pub pre: Option<String>,
    pub post: Option<String>,
    #[serde(default)]
    pub overnight: bool,
    pub early_close: Option<String>,
    pub early_post: Option<String>,
    #[serde(default, deserialize_with = "de_dates")]
    pub holidays: Vec<String>,
    #[serde(default, deserialize_with = "de_dates")]
    pub early_closes: Vec<String>,
    pub open: Option<String>,
    pub close: Option<String>,
    pub daily_break: Option<Vec<String>>,
}

/// Dates as strings, whether written `"2026-11-26"` or as a TOML date.
fn de_dates<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    use serde::de::Error;
    Vec::<toml::Value>::deserialize(d)?
        .into_iter()
        .map(|v| match v {
            toml::Value::String(s) => Ok(s),
            toml::Value::Datetime(dt) => Ok(dt.to_string()),
            other => Err(D::Error::custom(format!(
                "expected a YYYY-MM-DD date, got `{other}`"
            ))),
        })
        .collect()
}

impl CalendarConfig {
    /// Build the domain calendar; `Err` lists every problem.
    pub fn build(&self) -> Result<Calendar, Vec<String>> {
        let mut errors = Vec::new();
        let set: [(&str, bool); 12] = [
            ("tz", self.tz.is_some()),
            ("core", self.core.is_some()),
            ("pre", self.pre.is_some()),
            ("post", self.post.is_some()),
            ("overnight", self.overnight),
            ("early_close", self.early_close.is_some()),
            ("early_post", self.early_post.is_some()),
            ("holidays", !self.holidays.is_empty()),
            ("early_closes", !self.early_closes.is_empty()),
            ("open", self.open.is_some()),
            ("close", self.close.is_some()),
            ("daily_break", self.daily_break.is_some()),
        ];
        let (kind, allowed): (&str, &[&str]) = match self.kind {
            CalendarKind::Exchange => (
                "exchange",
                &[
                    "tz",
                    "core",
                    "pre",
                    "post",
                    "overnight",
                    "early_close",
                    "early_post",
                    "holidays",
                    "early_closes",
                ],
            ),
            CalendarKind::Weekly => ("weekly", &["tz", "open", "close", "daily_break"]),
            CalendarKind::AlwaysOpen => ("24x7", &[]),
        };
        for (field, present) in set {
            if present && !allowed.contains(&field) {
                errors.push(format!("`{field}` is not used by kind = \"{kind}\""));
            }
        }
        let built = match self.kind {
            CalendarKind::Exchange => self.exchange(&mut errors).map(Calendar::Exchange),
            CalendarKind::Weekly => self.weekly(&mut errors).map(Calendar::Weekly),
            CalendarKind::AlwaysOpen => Some(Calendar::AlwaysOpen),
        };
        match built {
            Some(cal) if errors.is_empty() => Ok(cal),
            _ => Err(errors),
        }
    }

    fn zone(&self, errors: &mut Vec<String>) -> Option<Zone> {
        match self.tz.as_deref() {
            None => {
                errors.push("`tz` is required".into());
                None
            }
            Some(tz) => Zone::parse(tz).or_else(|| {
                errors.push(format!(
                    "tz `{tz}` is not supported (America/New_York, Europe/Paris, UTC)"
                ));
                None
            }),
        }
    }

    fn exchange(&self, errors: &mut Vec<String>) -> Option<ExchangeCalendar> {
        let zone = self.zone(errors);
        let (open, close) = match self.core.as_deref() {
            Some([a, b]) => match (hm(errors, "core", a), hm(errors, "core", b)) {
                (Some(a), Some(b)) if a < b => (a, b),
                (Some(_), Some(_)) => {
                    errors.push("core must open before it closes".into());
                    return None;
                }
                _ => return None,
            },
            _ => {
                errors.push("`core` is required: [\"HH:MM\", \"HH:MM\"]".into());
                return None;
            }
        };
        let pre = self.pre.as_deref().and_then(|s| hm(errors, "pre", s));
        let post = self.post.as_deref().and_then(|s| hm(errors, "post", s));
        let early_close = self
            .early_close
            .as_deref()
            .and_then(|s| hm(errors, "early_close", s));
        let early_post = self
            .early_post
            .as_deref()
            .and_then(|s| hm(errors, "early_post", s));
        if pre.is_some_and(|p| p >= open) {
            errors.push("pre must be before the core open".into());
        }
        if post.is_some_and(|p| p <= close) {
            errors.push("post must be after the core close".into());
        }
        if self.overnight && (self.pre.is_none() || self.post.is_none()) {
            errors.push("overnight needs pre and post".into());
        }
        if early_close.is_some_and(|e| e <= open || e > close) {
            errors.push("early_close must be after the core open and not after its close".into());
        }
        if let Some(ep) = early_post {
            if early_close.is_none_or(|e| ep <= e) || post.is_none_or(|p| ep > p) {
                errors.push("early_post needs early_close < early_post <= post".into());
            }
        }
        let holidays = dates(errors, "holidays", &self.holidays);
        let early_closes = dates(errors, "early_closes", &self.early_closes);
        if !early_closes.is_empty() && self.early_close.is_none() {
            errors.push("early_closes needs early_close".into());
        }
        for d in holidays.intersection(&early_closes) {
            errors.push(format!("{d} is both a holiday and an early close"));
        }
        Some(ExchangeCalendar {
            zone: zone?,
            open,
            close,
            pre,
            post,
            overnight: self.overnight,
            early_close,
            early_post,
            holidays,
            early_closes,
        })
    }

    fn weekly(&self, errors: &mut Vec<String>) -> Option<WeeklyWindow> {
        let zone = self.zone(errors);
        let week_hm = |errors: &mut Vec<String>, field: &str, v: &Option<String>| match v {
            None => {
                errors.push(format!("`{field}` is required: \"Sun 20:00\""));
                None
            }
            Some(s) => parse_week_hm(s).or_else(|| {
                errors.push(format!("{field} `{s}` is not \"Ddd HH:MM\" (Mon … Sun)"));
                None
            }),
        };
        let open = week_hm(errors, "open", &self.open);
        let close = week_hm(errors, "close", &self.close);
        if open.is_some() && open == close {
            errors.push("open equals close (use kind = \"24x7\")".into());
        }
        let daily_break = match self.daily_break.as_deref() {
            None => None,
            Some([a, b]) => match (hm(errors, "daily_break", a), hm(errors, "daily_break", b)) {
                (Some(a), Some(b)) if a != b => Some((a, b)),
                (Some(_), Some(_)) => {
                    errors.push("daily_break must not be empty".into());
                    None
                }
                _ => None,
            },
            Some(_) => {
                errors.push("daily_break is [\"HH:MM\", \"HH:MM\"]".into());
                None
            }
        };
        Some(WeeklyWindow {
            zone: zone?,
            open: open?,
            close: close?,
            daily_break,
        })
    }
}

fn hm(errors: &mut Vec<String>, field: &str, s: &str) -> Option<u32> {
    parse_hm(s).or_else(|| {
        errors.push(format!("{field} `{s}` is not HH:MM"));
        None
    })
}

/// Parsed dates; a bad or weekend date is an error (NYSE lists weekday
/// closures only — a weekend entry hides the observed weekday).
fn dates(errors: &mut Vec<String>, field: &str, list: &[String]) -> BTreeSet<chrono::NaiveDate> {
    use chrono::Datelike;
    let mut out = BTreeSet::new();
    for s in list {
        match parse_date(s) {
            None => errors.push(format!("{field}: `{s}` is not a YYYY-MM-DD date")),
            Some(d) if d.weekday().number_from_monday() > 5 => {
                errors.push(format!("{field}: {d} is a {}", d.weekday()))
            }
            Some(d) => {
                out.insert(d);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::calendar::Session;
    use chrono::{NaiveDate, NaiveDateTime};

    /// Tracker fixture: the `[xmarket]` block of `config.example.toml`.
    const FIXTURE: &str = include_str!("../../tests/fixtures/xmarket/calendars.toml");

    #[derive(Deserialize)]
    struct Fixture {
        xmarket: XmarketConfig,
    }

    fn fixture() -> XmarketConfig {
        toml::from_str::<Fixture>(FIXTURE).unwrap().xmarket
    }

    fn et(s: &str) -> i64 {
        Zone::NewYork.to_utc_ms(NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap())
    }

    fn utc(s: &str) -> i64 {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M")
            .unwrap()
            .and_utc()
            .timestamp_millis()
    }

    fn day(s: &str) -> NaiveDate {
        parse_date(s).unwrap()
    }

    #[test]
    fn defaults_and_state_dir() {
        let x: XmarketConfig = toml::from_str("").unwrap();
        assert_eq!(x.state, "xmarket");
        assert_eq!(
            x.state_dir(Path::new("/h")),
            PathBuf::from("/h/state/xmarket")
        );
        assert!(x.validation_errors().is_empty());
        assert!(x.calendars().is_empty());
    }

    #[test]
    fn rejects_paths_and_unknown_keys() {
        for bad in ["", "..", "a/b", "flows", "x y"] {
            let x = XmarketConfig {
                state: bad.to_string(),
                ..XmarketConfig::default()
            };
            assert_eq!(x.validation_errors().len(), 1, "{bad:?}");
        }
        assert!(toml::from_str::<XmarketConfig>("stat = \"x\"").is_err());
        let typo = "[calendars.x]\nkind = \"24x7\"\nholiday = [\"2026-01-01\"]\n";
        assert!(toml::from_str::<XmarketConfig>(typo).is_err());
    }

    /// The fixture (= `config.example.toml`) builds, and its NYSE data drives
    /// the weekend clock and sessions end to end.
    #[test]
    fn fixture_calendars_build_and_evaluate() {
        let x = fixture();
        assert_eq!(x.validation_errors(), Vec::<String>::new());
        let cals = x.calendars();
        assert_eq!(
            cals.keys().map(String::as_str).collect::<Vec<_>>(),
            [
                "crypto",
                "rh_tokenization",
                "us_equity",
                "xyz_fx",
                "xyz_indices",
                "xyz_stocks"
            ]
        );
        let nyse = cals["us_equity"].exchange().unwrap();
        assert_eq!(nyse.holidays.len(), 29);
        assert_eq!(nyse.early_closes.len(), 5);
        // Verified on nyse.com 2026-09-30: Christmas 2027 observed Fri 12-24,
        // no New Year holiday for Sat 2028-01-01.
        assert!(!nyse.is_trading_day(day("2027-12-24")));
        assert!(nyse.is_trading_day(day("2027-12-31")));
        assert!(nyse.is_early_close(day("2028-07-03")));

        let w = nyse.weekend_window(et("2026-09-26 12:00")).unwrap();
        assert_eq!(
            (w.anchor_ms, w.entry_ms, w.exit_ms),
            (
                utc("2026-09-26 00:00"),
                utc("2026-09-27 22:00"),
                utc("2026-09-28 13:00")
            )
        );
        let mlk = nyse.weekend_window(et("2027-01-16 12:00")).unwrap();
        assert_eq!(mlk.entry_ms, utc("2027-01-18 23:00"));
        assert_eq!(mlk.exit_ms, utc("2027-01-19 14:00"));
        for (t, want) in [
            ("2026-11-26 12:00", Session::Closed),
            ("2026-11-27 13:00", Session::Post),
            ("2026-11-27 17:00", Session::Closed),
            ("2026-09-29 03:00", Session::Overnight),
        ] {
            assert_eq!(cals["us_equity"].session(et(t)), want, "{t} ET");
        }
        assert_eq!(
            cals["xyz_indices"].session(et("2026-09-29 17:30")),
            Session::Closed
        );
        assert_eq!(
            cals["rh_tokenization"].session(utc("2026-10-03 00:00")),
            Session::Closed
        );
        assert_eq!(
            cals["crypto"].session(utc("2026-10-03 00:00")),
            Session::Open
        );
    }

    /// Every agent's tools see the built calendars through `AgentConfig::sandbox`.
    #[test]
    fn calendars_reach_every_agent_through_sandbox_sections() {
        let mut config: crate::config::Config = toml::from_str(&format!(
            "{FIXTURE}\n[agents.main]\ndefault = true\nengine = \"openrouter\"\nmodel = \"m\"\n"
        ))
        .unwrap();
        config.validate().unwrap();
        config.fold_default_scopes();
        let cals = &config.agents["main"].sandbox.calendars;
        assert_eq!(cals.len(), 6);
        assert_eq!(
            cals["us_equity"].session(et("2026-09-28 10:00")),
            Session::Open
        );

        let mut bad: crate::config::Config = toml::from_str(
            "[xmarket.calendars.us]\nkind = \"exchange\"\ntz = \"UTC\"\n\
             [agents.main]\nengine = \"openrouter\"\nmodel = \"m\"\n",
        )
        .unwrap();
        let e = bad.validate().unwrap_err().to_string();
        assert!(
            e.contains("xmarket.calendars.us: `core` is required"),
            "{e}"
        );
        bad.fold_default_scopes();
        assert!(bad.agents["main"].sandbox.calendars.is_empty());
    }

    #[test]
    fn toml_dates_are_accepted_unquoted() {
        let x: XmarketConfig = toml::from_str(
            "[calendars.us]\nkind = \"exchange\"\ntz = \"America/New_York\"\n\
             core = [\"09:30\", \"16:00\"]\nholidays = [2026-11-26, \"2026-12-25\"]\n",
        )
        .unwrap();
        assert_eq!(x.calendars["us"].holidays, ["2026-11-26", "2026-12-25"]);
        assert!(x.validation_errors().is_empty());
    }

    #[test]
    fn calendar_validation_errors() {
        let errors = |toml_row: &str| {
            let x: XmarketConfig = toml::from_str(&format!("[calendars.c]\n{toml_row}")).unwrap();
            x.validation_errors()
        };
        let base =
            "kind = \"exchange\"\ntz = \"America/New_York\"\ncore = [\"09:30\", \"16:00\"]\n";
        assert!(errors(base).is_empty());
        for (extra, want) in [
            ("holidays = [\"2026-02-30\"]", "is not a YYYY-MM-DD date"),
            ("holidays = [\"2026-07-04\"]", "is a Sat"),
            ("early_closes = [\"2026-11-27\"]", "early_closes needs early_close"),
            ("overnight = true", "overnight needs pre and post"),
            ("pre = \"10:00\"", "pre must be before the core open"),
            ("post = \"15:00\"", "post must be after the core close"),
            ("early_close = \"17:00\"", "early_close must be after"),
            ("early_close = \"13:00\"\nearly_post = \"17:00\"", "early_post needs"),
            ("open = \"Sun 20:00\"", "`open` is not used by kind = \"exchange\""),
            (
                "early_close = \"13:00\"\nholidays = [\"2026-11-27\"]\nearly_closes = [\"2026-11-27\"]",
                "both a holiday and an early close",
            ),
        ] {
            let got = errors(&format!("{base}{extra}\n"));
            assert!(
                got.iter().any(|e| e.starts_with("xmarket.calendars.c: ") && e.contains(want)),
                "{extra:?} → {got:?}"
            );
        }
        let e =
            errors("kind = \"exchange\"\ntz = \"Asia/Almaty\"\ncore = [\"16:00\", \"09:30\"]\n");
        assert!(
            e.iter()
                .any(|m| m.contains("tz `Asia/Almaty` is not supported")),
            "{e:?}"
        );
        assert!(
            e.iter().any(|m| m.contains("core must open before")),
            "{e:?}"
        );
        let e = errors("kind = \"weekly\"\ntz = \"UTC\"\nopen = \"Sun 20:00\"\n");
        assert!(e.iter().any(|m| m.contains("`close` is required")), "{e:?}");
        let e = errors(
            "kind = \"weekly\"\ntz = \"UTC\"\nopen = \"Sunday 20:00\"\nclose = \"Fri 20:00\"\n",
        );
        assert!(
            e.iter().any(|m| m.contains("is not \"Ddd HH:MM\"")),
            "{e:?}"
        );
        let e = errors(
            "kind = \"weekly\"\ntz = \"UTC\"\nopen = \"Fri 20:00\"\nclose = \"Fri 20:00\"\n",
        );
        assert!(e.iter().any(|m| m.contains("use kind = \"24x7\"")), "{e:?}");
        let e = errors("kind = \"24x7\"\ntz = \"UTC\"\n");
        assert!(
            e.iter()
                .any(|m| m.contains("`tz` is not used by kind = \"24x7\"")),
            "{e:?}"
        );
        let e = errors("kind = \"weekly\"\nopen = \"Sun 18:00\"\nclose = \"Fri 17:00\"\ndaily_break = [\"17:00\"]\n");
        assert!(e.iter().any(|m| m.contains("`tz` is required")), "{e:?}");
        assert!(e.iter().any(|m| m.contains("daily_break is [")), "{e:?}");
        assert!(toml::from_str::<XmarketConfig>("[calendars.c]\nkind = \"24x5\"\n").is_err());
    }
}
