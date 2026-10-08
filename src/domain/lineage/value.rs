//! Shared values of lineage records (`docs/lineage-2026-10-06.md` § 1): ids,
//! times, counts, locators, evidence refs, pin targets, capability bindings.
//! Every value parses from the record's TOML string and prints back as
//! written; a missing fact is the string `"UNKNOWN"`, never a guess.
//!
//! | Value | TOML | Rule |
//! |---|---|---|
//! | id | `rule_w.top4` | `^[A-Za-z0-9][A-Za-z0-9._-]{0,79}$` ([`valid_id`]) |
//! | [`Time`] | `"2026-10-01T18:28:09Z"` · `"2026-09-30"` · `"UNKNOWN"` | RFC 3339 in UTC (`Z` or `+00:00`) or a UTC day = [00:00, 24:00); compares known values only ([`Time::order`]) |
//! | [`Count`] | `27` · `"UNKNOWN"` | a [`Precision`] may sit beside it where the schema says so |
//! | [`Locator`] | `repo:` · `run:` · `vault:` · `state:` · `git:` · `record:` · `url:` · `"UNKNOWN"` | segments relative, never `..`; `git:` 40 lowercase hex; `record:<kind>/<id>`; `url:https://…` |
//! | [`PinTarget`] | `config:<sandbox>/<dotted path>` · `spec:<sandbox>/<strategy>` · `tool_schema:<tool>` · `skill:<name>` · `repo:<path>` | a dotted path segment is bare (`[A-Za-z0-9_-]+`) or quoted (`"hyperliquid:xyz:"`, `\"` and `\\` escaped) |
//! | [`Binding`] | `tool:<name>` · `strategy_kind:<kind>` | what a capability makes available |
//! | [`RecordKind`] | `family` `variant` `experiment` `episode` `incident` `capability` `generation` `evidence` `ranking` | one dir each under `lineage/`; a seal names `variant:` · `experiment:` · `ranking:<id>` ([`parse_seal_record`]) |

use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, NaiveDate, SecondsFormat, Utc};
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::domain::evidence::{valid_item_path, EvidenceClass, Provenance};

/// The missing-fact marker.
pub const UNKNOWN: &str = "UNKNOWN";
/// Longest `title`.
pub const MAX_TITLE_CHARS: usize = 160;
const DAY_MS: i64 = 86_400_000;

/// Module table: a record id.
pub fn valid_id(id: &str) -> bool {
    let b = id.as_bytes();
    !b.is_empty()
        && b.len() <= 80
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

/// 40 lowercase hex chars (a git commit).
pub fn valid_commit(s: &str) -> bool {
    s.len() == 40
        && s.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

/// Implements `Serialize` / `Deserialize` through `Display` / `FromStr`.
macro_rules! string_serde {
    ($ty:ty, $expecting:literal) => {
        impl Serialize for $ty {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.collect_str(self)
            }
        }
        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                struct V;
                impl Visitor<'_> for V {
                    type Value = $ty;
                    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                        f.write_str($expecting)
                    }
                    fn visit_str<E: de::Error>(self, v: &str) -> Result<$ty, E> {
                        v.parse().map_err(E::custom)
                    }
                }
                d.deserialize_str(V)
            }
        }
    };
}

// ---------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------

/// A record time (module table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Time {
    /// An instant, ms since the epoch (UTC).
    At(i64),
    /// A UTC day: its 00:00 in ms; covers [00:00, 24:00).
    Day(i64),
    Unknown,
}

/// How two times compare when either may be a day or unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeOrder {
    /// Every instant `a` may stand for is ≤ every one `b` may.
    NotAfter,
    /// Every instant of `a` is > every one of `b`.
    After,
    /// They overlap (a day against an instant inside it, or two equal days).
    Ambiguous,
    /// Either is `UNKNOWN`.
    Unknown,
}

impl Time {
    pub fn is_known(&self) -> bool {
        !matches!(self, Time::Unknown)
    }

    /// The earliest instant it may stand for.
    pub fn earliest(&self) -> Option<i64> {
        match *self {
            Time::At(t) | Time::Day(t) => Some(t),
            Time::Unknown => None,
        }
    }

    /// The latest instant it may stand for.
    pub fn latest(&self) -> Option<i64> {
        match *self {
            Time::At(t) => Some(t),
            Time::Day(d) => Some(d + DAY_MS - 1),
            Time::Unknown => None,
        }
    }

    /// The end of a window bounded by it: an instant is exclusive, a day
    /// ends at the next 00:00.
    pub fn window_end(&self) -> Option<i64> {
        match *self {
            Time::At(t) => Some(t),
            Time::Day(d) => Some(d + DAY_MS),
            Time::Unknown => None,
        }
    }

    /// `self` against `other` (enum doc).
    pub fn order(&self, other: &Time) -> TimeOrder {
        match (
            self.earliest(),
            self.latest(),
            other.earliest(),
            other.latest(),
        ) {
            (Some(_), Some(a_hi), Some(b_lo), Some(_)) if a_hi <= b_lo => TimeOrder::NotAfter,
            (Some(a_lo), Some(_), Some(_), Some(b_hi)) if a_lo > b_hi => TimeOrder::After,
            (Some(_), _, Some(_), _) => TimeOrder::Ambiguous,
            _ => TimeOrder::Unknown,
        }
    }

    /// For sorting: unknown last, then by earliest instant.
    pub fn sort_key(&self) -> (bool, i64) {
        (!self.is_known(), self.earliest().unwrap_or(i64::MAX))
    }
}

impl FromStr for Time {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        if s == UNKNOWN {
            return Ok(Time::Unknown);
        }
        if s.len() == 10 {
            let d = NaiveDate::parse_from_str(s, "%Y-%m-%d")
                .map_err(|_| format!("time `{s}`: not a YYYY-MM-DD day"))?;
            let ms = d
                .and_hms_opt(0, 0, 0)
                .map(|t| t.and_utc().timestamp_millis())
                .ok_or_else(|| format!("time `{s}`: out of range"))?;
            return Ok(Time::Day(ms));
        }
        let t = DateTime::parse_from_rfc3339(s).map_err(|_| {
            format!(
                "time `{s}`: not RFC 3339 (2026-10-01T18:28:09Z), a day (2026-09-30) or UNKNOWN"
            )
        })?;
        if t.offset().local_minus_utc() != 0 {
            return Err(format!("time `{s}`: give it in UTC (…Z)"));
        }
        Ok(Time::At(t.timestamp_millis()))
    }
}

impl fmt::Display for Time {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Time::Unknown => f.write_str(UNKNOWN),
            Time::Day(d) => match DateTime::<Utc>::from_timestamp_millis(d) {
                Some(t) => write!(f, "{}", t.format("%Y-%m-%d")),
                None => f.write_str(UNKNOWN),
            },
            Time::At(t) => match DateTime::<Utc>::from_timestamp_millis(t) {
                Some(dt) => {
                    let fmt = if t % 1000 == 0 {
                        SecondsFormat::Secs
                    } else {
                        SecondsFormat::Millis
                    };
                    f.write_str(&dt.to_rfc3339_opts(fmt, true))
                }
                None => f.write_str(UNKNOWN),
            },
        }
    }
}

string_serde!(
    Time,
    "a quoted time: \"2026-10-01T18:28:09Z\", \"2026-09-30\" or \"UNKNOWN\""
);

/// `[from, to)` of two bounds overlaps `[from, to)` of two others; `None`
/// when a bound is unknown (module table: known values only).
pub fn windows_overlap(a: (&Time, &Time), b: (&Time, &Time)) -> Option<bool> {
    let (a_lo, a_hi) = (a.0.earliest()?, a.1.window_end()?);
    let (b_lo, b_hi) = (b.0.earliest()?, b.1.window_end()?);
    Some(a_lo < b_hi && b_lo < a_hi)
}

// ---------------------------------------------------------------------------
// Count, precision, integrity
// ---------------------------------------------------------------------------

/// A count (module table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Count {
    Known(u64),
    Unknown,
}

impl Count {
    pub fn known(&self) -> Option<u64> {
        match self {
            Count::Known(n) => Some(*n),
            Count::Unknown => None,
        }
    }
}

impl fmt::Display for Count {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Count::Known(n) => write!(f, "{n}"),
            Count::Unknown => f.write_str(UNKNOWN),
        }
    }
}

impl Serialize for Count {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Count::Known(n) => s.serialize_u64(*n),
            Count::Unknown => s.serialize_str(UNKNOWN),
        }
    }
}

impl<'de> Deserialize<'de> for Count {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = Count;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a count ≥ 0 or \"UNKNOWN\"")
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Count, E> {
                Ok(Count::Known(v))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Count, E> {
                u64::try_from(v)
                    .map(Count::Known)
                    .map_err(|_| E::custom(format!("count {v} is negative")))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Count, E> {
                if v == UNKNOWN {
                    Ok(Count::Unknown)
                } else {
                    Err(E::custom(format!("count `{v}`: a number or \"UNKNOWN\"")))
                }
            }
        }
        d.deserialize_any(V)
    }
}

/// How exact a [`Count`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Precision {
    Exact,
    Approx,
    LowerBound,
}

/// Of a window: could the hypothesis-forming study have seen it?
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Integrity {
    Clean,
    Contaminated,
    Unknown,
}

// ---------------------------------------------------------------------------
// Record kinds and locators
// ---------------------------------------------------------------------------

/// The record kinds, one dir each under `lineage/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecordKind {
    Family,
    Variant,
    Experiment,
    Episode,
    Incident,
    Capability,
    Generation,
    Evidence,
    Ranking,
}

impl RecordKind {
    pub const ALL: [RecordKind; 9] = [
        RecordKind::Family,
        RecordKind::Variant,
        RecordKind::Experiment,
        RecordKind::Episode,
        RecordKind::Incident,
        RecordKind::Capability,
        RecordKind::Generation,
        RecordKind::Evidence,
        RecordKind::Ranking,
    ];

    /// `family`, `variant`, …
    pub fn name(&self) -> &'static str {
        match self {
            RecordKind::Family => "family",
            RecordKind::Variant => "variant",
            RecordKind::Experiment => "experiment",
            RecordKind::Episode => "episode",
            RecordKind::Incident => "incident",
            RecordKind::Capability => "capability",
            RecordKind::Generation => "generation",
            RecordKind::Evidence => "evidence",
            RecordKind::Ranking => "ranking",
        }
    }

    /// The dir under `lineage/`.
    pub fn dir(&self) -> &'static str {
        match self {
            RecordKind::Family => "families",
            RecordKind::Variant => "variants",
            RecordKind::Experiment => "experiments",
            RecordKind::Episode => "episodes",
            RecordKind::Incident => "incidents",
            RecordKind::Capability => "capabilities",
            RecordKind::Generation => "generations",
            RecordKind::Evidence => "evidence",
            RecordKind::Ranking => "rankings",
        }
    }

    pub fn parse(s: &str) -> Option<RecordKind> {
        RecordKind::ALL.into_iter().find(|k| k.name() == s)
    }
}

impl fmt::Display for RecordKind {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Where a piece of evidence is (module table; resolved by
/// `ports::lineage::EvidenceResolver`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Locator {
    /// A repo file, relative to the repo root (the registry's parent).
    Repo(String),
    /// A backtest run dir (`report.json` when `file` is none).
    Run {
        state: String,
        run_id: String,
        file: Option<String>,
    },
    /// A read-only vault copy: `<TENGU_HOME>/state/evidence/<snapshot>/<path>`.
    Vault {
        snapshot: String,
        path: String,
    },
    /// A live state file — mutable.
    State {
        state: String,
        path: String,
    },
    Git(String),
    Record {
        kind: RecordKind,
        id: String,
    },
    Url(String),
    Unknown,
}

/// `<first segment>/<the rest>` with both valid.
fn split_first(rest: &str, what: &str) -> Result<(String, String), String> {
    let (a, b) = rest
        .split_once('/')
        .ok_or_else(|| format!("{what}: `<{what}>/<path>` expected"))?;
    if !valid_item_path(a) || !valid_item_path(b) {
        return Err(format!(
            "{what}: `{rest}` — relative segments, no `.` / `..`"
        ));
    }
    Ok((a.to_string(), b.to_string()))
}

impl FromStr for Locator {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        if s == UNKNOWN {
            return Ok(Locator::Unknown);
        }
        let (scheme, rest) = s
            .split_once(':')
            .ok_or_else(|| format!("locator `{s}`: no `<scheme>:`"))?;
        let bad = |why: String| format!("locator `{s}`: {why}");
        match scheme {
            "repo" if valid_item_path(rest) => Ok(Locator::Repo(rest.to_string())),
            "repo" => Err(bad("a relative repo path, no `.` / `..`".into())),
            "run" => {
                let (state, tail) = split_first(rest, "state").map_err(bad)?;
                let (run_id, file) = match tail.split_once('/') {
                    Some((id, file)) => (id.to_string(), Some(file.to_string())),
                    None => (tail, None),
                };
                Ok(Locator::Run {
                    state,
                    run_id,
                    file,
                })
            }
            "vault" => {
                let (snapshot, path) = split_first(rest, "snapshot").map_err(bad)?;
                Ok(Locator::Vault { snapshot, path })
            }
            "state" => {
                let (state, path) = split_first(rest, "state").map_err(bad)?;
                Ok(Locator::State { state, path })
            }
            "git" if valid_commit(rest) => Ok(Locator::Git(rest.to_string())),
            "git" => Err(bad("a full 40-hex lowercase commit".into())),
            "record" => {
                let (kind, id) = rest
                    .split_once('/')
                    .ok_or_else(|| bad("`record:<kind>/<id>`".into()))?;
                let kind = RecordKind::parse(kind).ok_or_else(|| {
                    bad(format!(
                        "kind `{kind}` is not one of {}",
                        RecordKind::ALL.map(|k| k.name()).join(", ")
                    ))
                })?;
                if !valid_id(id) {
                    return Err(bad(format!("id `{id}` is not a record id")));
                }
                Ok(Locator::Record {
                    kind,
                    id: id.to_string(),
                })
            }
            "url" if rest.starts_with("https://") && rest.len() > 8 => {
                Ok(Locator::Url(rest.to_string()))
            }
            "url" => Err(bad("`url:https://…`".into())),
            other => Err(bad(format!(
                "scheme `{other}` is not repo, run, vault, state, git, record or url"
            ))),
        }
    }
}

impl fmt::Display for Locator {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Locator::Repo(p) => write!(f, "repo:{p}"),
            Locator::Run {
                state,
                run_id,
                file,
            } => match file {
                Some(file) => write!(f, "run:{state}/{run_id}/{file}"),
                None => write!(f, "run:{state}/{run_id}"),
            },
            Locator::Vault { snapshot, path } => write!(f, "vault:{snapshot}/{path}"),
            Locator::State { state, path } => write!(f, "state:{state}/{path}"),
            Locator::Git(c) => write!(f, "git:{c}"),
            Locator::Record { kind, id } => write!(f, "record:{kind}/{id}"),
            Locator::Url(u) => write!(f, "url:{u}"),
            Locator::Unknown => f.write_str(UNKNOWN),
        }
    }
}

string_serde!(
    Locator,
    "a locator: repo:, run:, vault:, state:, git:, record:, url: or \"UNKNOWN\""
);

/// `[[evidence]]` on any record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceRef {
    #[serde(rename = "ref")]
    pub locator: Locator,
    /// Short label: `report`, `prereg`, `ledger`, `doc`, …
    pub role: String,
    pub class: EvidenceClass,
    pub provenance: Provenance,
    /// Of the file (a dir: its tree hash) when recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

// ---------------------------------------------------------------------------
// Pin targets and bindings
// ---------------------------------------------------------------------------

/// What a pin hashes (module table; `domain/lineage/pins.rs`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PinTarget {
    /// A value of `sandboxes/<sandbox>/config.toml`; `path` = its segments.
    Config {
        sandbox: String,
        path: Vec<String>,
        raw_path: String,
    },
    /// `[backtest.strategies.<strategy>]` normalized = a run's `spec_sha256`.
    Spec {
        sandbox: String,
        strategy: String,
    },
    ToolSchema(String),
    Skill(String),
    Repo(String),
}

/// `a.b."c:d".e` → `["a", "b", "c:d", "e"]` (module table).
pub fn parse_dotted(s: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    loop {
        let mut seg = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            let mut closed = false;
            while let Some(c) = chars.next() {
                match c {
                    '\\' => match chars.next() {
                        Some(e @ ('"' | '\\')) => seg.push(e),
                        _ => return Err(format!("path `{s}`: only \\\" and \\\\ escape")),
                    },
                    '"' => {
                        closed = true;
                        break;
                    }
                    c => seg.push(c),
                }
            }
            if !closed {
                return Err(format!("path `{s}`: an unclosed quote"));
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c == '.' {
                    break;
                }
                if !(c.is_ascii_alphanumeric() || c == '_' || c == '-') {
                    return Err(format!(
                        "path `{s}`: `{c}` in a bare segment — quote it (\"…\")"
                    ));
                }
                seg.push(c);
                chars.next();
            }
            if seg.is_empty() {
                return Err(format!("path `{s}`: an empty segment"));
            }
        }
        out.push(seg);
        match chars.next() {
            None => return Ok(out),
            Some('.') => {}
            Some(c) => return Err(format!("path `{s}`: `{c}` after a quoted segment")),
        }
    }
}

impl FromStr for PinTarget {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let (scheme, rest) = s
            .split_once(':')
            .ok_or_else(|| format!("pin target `{s}`: no `<scheme>:`"))?;
        let name_ok = |n: &str| valid_id(n);
        match scheme {
            "config" => {
                let (sandbox, raw_path) = rest
                    .split_once('/')
                    .ok_or_else(|| format!("pin target `{s}`: `config:<sandbox>/<dotted path>`"))?;
                if !name_ok(sandbox) {
                    return Err(format!("pin target `{s}`: sandbox `{sandbox}`"));
                }
                Ok(PinTarget::Config {
                    sandbox: sandbox.to_string(),
                    path: parse_dotted(raw_path)?,
                    raw_path: raw_path.to_string(),
                })
            }
            "spec" => match rest.split_once('/') {
                Some((sandbox, strategy)) if name_ok(sandbox) && name_ok(strategy) => {
                    Ok(PinTarget::Spec {
                        sandbox: sandbox.to_string(),
                        strategy: strategy.to_string(),
                    })
                }
                _ => Err(format!("pin target `{s}`: `spec:<sandbox>/<strategy>`")),
            },
            "tool_schema" if name_ok(rest) => Ok(PinTarget::ToolSchema(rest.to_string())),
            "skill" if name_ok(rest) => Ok(PinTarget::Skill(rest.to_string())),
            "repo" if valid_item_path(rest) => Ok(PinTarget::Repo(rest.to_string())),
            _ => Err(format!(
                "pin target `{s}`: config:<sandbox>/<path>, spec:<sandbox>/<strategy>, \
                 tool_schema:<tool>, skill:<name> or repo:<relative path>"
            )),
        }
    }
}

impl fmt::Display for PinTarget {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            PinTarget::Config {
                sandbox, raw_path, ..
            } => write!(f, "config:{sandbox}/{raw_path}"),
            PinTarget::Spec { sandbox, strategy } => write!(f, "spec:{sandbox}/{strategy}"),
            PinTarget::ToolSchema(t) => write!(f, "tool_schema:{t}"),
            PinTarget::Skill(n) => write!(f, "skill:{n}"),
            PinTarget::Repo(p) => write!(f, "repo:{p}"),
        }
    }
}

string_serde!(
    PinTarget,
    "a pin target: config:, spec:, tool_schema:, skill: or repo:"
);

/// What a capability makes available (module table).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Binding {
    Tool(String),
    StrategyKind(String),
}

impl FromStr for Binding {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.split_once(':') {
            Some(("tool", t)) if valid_id(t) => Ok(Binding::Tool(t.to_string())),
            Some(("strategy_kind", k)) if valid_id(k) => Ok(Binding::StrategyKind(k.to_string())),
            _ => Err(format!(
                "binding `{s}`: `tool:<name>` or `strategy_kind:<kind>`"
            )),
        }
    }
}

impl fmt::Display for Binding {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Binding::Tool(t) => write!(f, "tool:{t}"),
            Binding::StrategyKind(k) => write!(f, "strategy_kind:{k}"),
        }
    }
}

string_serde!(Binding, "a binding: tool:<name> or strategy_kind:<kind>");

/// A sealed record name: `variant:<id>`, `experiment:<id>` or `ranking:<id>`.
pub fn parse_seal_record(s: &str) -> Result<(RecordKind, String), String> {
    match s.split_once(':') {
        Some(("variant", id)) if valid_id(id) => Ok((RecordKind::Variant, id.to_string())),
        Some(("experiment", id)) if valid_id(id) => Ok((RecordKind::Experiment, id.to_string())),
        Some(("ranking", id)) if valid_id(id) => Ok((RecordKind::Ranking, id.to_string())),
        _ => Err(format!(
            "`{s}`: `variant:<id>`, `experiment:<id>` or `ranking:<id>`"
        )),
    }
}

/// Times in `items` sorted (unknown last); for stable views.
pub fn cmp_time(a: &Time, b: &Time) -> Ordering {
    a.sort_key().cmp(&b.sort_key())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_follow_the_pattern() {
        for ok in ["W1", "rule_w.top4", "W2-SIM", "a", &"x".repeat(80)] {
            assert!(valid_id(ok), "{ok}");
        }
        for bad in ["", "_a", ".a", "a b", "a/b", &"x".repeat(81), "é"] {
            assert!(!valid_id(bad), "{bad}");
        }
    }

    #[test]
    fn times_parse_print_and_compare_known_values_only() {
        let at: Time = "2026-10-01T18:28:09Z".parse().unwrap();
        let day: Time = "2026-10-01".parse().unwrap();
        let unknown: Time = UNKNOWN.parse().unwrap();
        assert_eq!(at.to_string(), "2026-10-01T18:28:09Z");
        assert_eq!(day.to_string(), "2026-10-01");
        assert_eq!(unknown, Time::Unknown);
        assert_eq!(
            "2026-10-01T18:28:09.250+00:00"
                .parse::<Time>()
                .unwrap()
                .to_string(),
            "2026-10-01T18:28:09.250Z"
        );
        assert!("2026-10-01T18:28:09+02:00".parse::<Time>().is_err());
        assert!("yesterday".parse::<Time>().is_err());
        // An instant inside the day is ambiguous against it; the next day is after.
        assert_eq!(at.order(&day), TimeOrder::Ambiguous);
        assert_eq!(day.order(&at), TimeOrder::Ambiguous);
        let next: Time = "2026-10-02".parse().unwrap();
        assert_eq!(at.order(&next), TimeOrder::NotAfter);
        assert_eq!(next.order(&at), TimeOrder::After);
        assert_eq!(at.order(&unknown), TimeOrder::Unknown);
        assert_eq!(at.order(&at), TimeOrder::NotAfter);
        // Day windows [Mar 1, Jun 30] and [Jul 1, Sep 30] touch, never overlap.
        let t = |s: &str| s.parse::<Time>().unwrap();
        let (d0, d1, h0, h1) = (
            t("2026-03-01"),
            t("2026-06-30"),
            t("2026-07-01"),
            t("2026-09-30"),
        );
        assert_eq!(windows_overlap((&d0, &d1), (&h0, &h1)), Some(false));
        assert_eq!(windows_overlap((&d0, &h0), (&h0, &h1)), Some(true));
        let (i0, i1) = (t("2026-07-01T00:00:00Z"), t("2026-09-30T00:00:00Z"));
        assert_eq!(windows_overlap((&d0, &i0), (&i0, &i1)), Some(false));
        assert_eq!(windows_overlap((&d0, &unknown), (&i0, &i1)), None);
    }

    #[test]
    fn counts_take_a_number_or_unknown() {
        #[derive(Deserialize)]
        struct T {
            n: Count,
        }
        assert_eq!(toml::from_str::<T>("n = 27").unwrap().n, Count::Known(27));
        assert_eq!(
            toml::from_str::<T>("n = \"UNKNOWN\"").unwrap().n,
            Count::Unknown
        );
        assert!(toml::from_str::<T>("n = -1").is_err());
        assert!(toml::from_str::<T>("n = \"27\"").is_err());
    }

    #[test]
    fn locators_round_trip_and_refuse_climbing() {
        for s in [
            "repo:docs/lineage-2026-10-06.md",
            "run:xlab/20261001T182809Z-weekend_fade",
            "run:xlab/20261001T182809Z-weekend_fade/trades-research.jsonl",
            "vault:w1-2026-10-06/xmarket-weekend/ledger.db",
            "state:xmarket-weekend/ledger.db",
            "git:c80d5beb724ff7eaf2fc61a4928ff15aa9a7cd16",
            "record:experiment/rule_w.forward",
            "url:https://example.com/a",
            "UNKNOWN",
        ] {
            let l: Locator = s.parse().unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(l.to_string(), s);
        }
        for bad in [
            "repo:../x",
            "repo:/abs",
            "run:xlab",
            "vault:w1",
            "git:c80d5be",
            "record:thing/x",
            "url:http://x",
            "ftp:x",
            "nothing",
        ] {
            assert!(bad.parse::<Locator>().is_err(), "{bad}");
        }
    }

    #[test]
    fn pin_targets_take_quoted_segments() {
        let p: PinTarget = "config:xlab/backtest.costs.\"hyperliquid:xyz:\""
            .parse()
            .unwrap();
        match &p {
            PinTarget::Config { sandbox, path, .. } => {
                assert_eq!(sandbox, "xlab");
                assert_eq!(path, &["backtest", "costs", "hyperliquid:xyz:"]);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            p.to_string(),
            "config:xlab/backtest.costs.\"hyperliquid:xyz:\""
        );
        assert_eq!(
            parse_dotted(r#"a."b\"c".d"#).unwrap(),
            vec!["a", "b\"c", "d"]
        );
        for bad in ["a..b", "a.\"b", "a.b:c", ""] {
            assert!(parse_dotted(bad).is_err(), "{bad}");
        }
        for ok in [
            "spec:xlab/weekend_fade",
            "tool_schema:backtest",
            "skill:xlab-research",
            "repo:src/domain/backtest/spec.rs",
        ] {
            assert_eq!(ok.parse::<PinTarget>().unwrap().to_string(), ok);
        }
        assert!("spec:xlab".parse::<PinTarget>().is_err());
        assert!("thing:x".parse::<PinTarget>().is_err());
    }

    #[test]
    fn bindings_and_seal_records_parse() {
        assert_eq!(
            "tool:backtest".parse::<Binding>().unwrap(),
            Binding::Tool("backtest".into())
        );
        assert_eq!(
            "strategy_kind:weekend_window".parse::<Binding>().unwrap(),
            Binding::StrategyKind("weekend_window".into())
        );
        assert!("kind:x".parse::<Binding>().is_err());
        assert_eq!(
            parse_seal_record("variant:rule_w.sat").unwrap(),
            (RecordKind::Variant, "rule_w.sat".to_string())
        );
        assert!(parse_seal_record("family:x").is_err());
    }

    #[test]
    fn seal_records_parse_ranking() {
        assert_eq!(
            parse_seal_record("ranking:rank.xlab-w2.daily.v1").unwrap(),
            (RecordKind::Ranking, "rank.xlab-w2.daily.v1".to_string())
        );
        assert!(parse_seal_record("ranking:").is_err());
        assert!(parse_seal_record("rankings:rank.x").is_err());
        let e = parse_seal_record("generation:W1").unwrap_err();
        assert!(e.contains("`ranking:<id>`"), "{e}");
        assert_eq!(RecordKind::parse("ranking"), Some(RecordKind::Ranking));
        assert_eq!(RecordKind::Ranking.dir(), "rankings");
        assert_eq!(
            "record:ranking/rank.t".parse::<Locator>().unwrap(),
            Locator::Record {
                kind: RecordKind::Ranking,
                id: "rank.t".into()
            }
        );
    }
}
