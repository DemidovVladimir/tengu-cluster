//! `[feeds.<name>]` — scheduled work of `tengu run` (xmarket `rt-scheduler`;
//! fire times `domain/schedule.rs`, runner `application/runtime/feeds.rs`,
//! operator doc `docs/runtime-2026-09-30.md` § Feeds). `deny_unknown_fields`.
//!
//! | Key | Kind | Default | Meaning |
//! |---|---|---|---|
//! | `kind` | — | required | `"tool"` calls a tool · `"tick"` sends a loop event; `"poll"` (`info-fetch`, W2) and `"stream"` / `"ws"` / `"rows"` are refused until built |
//! | `every_secs` | both | — | base interval (1 s – 7 d) on the UTC epoch grid |
//! | `windows` | both | `[]` | `{ days = ["Sun"], from = "17:00", to = "19:00", every_secs = 60 }`: local time in `tz`, replaces `every_secs` inside; `to <= from` ends the next day, `"24:00"` = midnight; no `days` = every day |
//! | `at` | both | `[]` | clock ticks in `tz`: `"Sun 18:00"`, `"daily 09:00"` |
//! | `tz` | both | `UTC` | `America/New_York` · `Europe/Paris` · `UTC` |
//! | `jitter_pct` | both | 0 | a grid fire starts up to this % (0–50) of its interval late; at-ticks are exact |
//! | `run_on_start` | both | `false` | one run as soon as `tengu run` starts |
//! | `agent`, `tool` | tool | required | `[agents.<agent>]` runs `tool` — listed in its `tools` (or opted in via `workspace_tools`) — with the executor a decision loop of that agent gets |
//! | `args` | tool | `{}` | the tool's arguments |
//! | `each` | tool | `{}` | fan-out, `{ coin = ["xyz:TSLA", "xyz:NVDA"] }` = one call per value; several keys = every combination (keys in name order, the last varying fastest); ≤ 500 calls per run |
//! | `concurrency` | tool | 1 | calls of one run in flight at once (1–32) |
//! | `target` | tick | required | the `[decision_loops.<target>]` the event goes to |
//! | `event` | tick | `{}` | the event table; the scheduler adds `ts_ms` (the slot time) |
//! | `required` | both | `false` | `tengu doctor --live` fails when the feed is down or stale |
//! | `stale_after_secs` | both | 3 × the longest interval (≥ 60); no `every_secs` (idle between windows / at-ticks): 8 days | no item for this long ⇒ stale |
//!
//! At least one of `every_secs`, `windows`, `at`. No `budget` key: tools
//! budget themselves against `[rate_limits.<name>]` (`hl-info-client`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::{AgentConfig, Config};
use crate::domain::schedule::{
    parse_at, parse_clock, parse_day, Days, Schedule, Window, MAX_EVERY_SECS, MAX_JITTER_PCT,
};
use crate::domain::tools::WORKSPACE_TOOLS;
use crate::domain::tz::Zone;

/// Calls one run may make (`each` combinations).
pub const MAX_CALLS: usize = 500;
/// Largest `concurrency`.
pub const MAX_CONCURRENCY: usize = 32;
/// Windows / at-ticks per feed.
const MAX_WINDOWS: usize = 32;
const MAX_AT: usize = 64;
/// `stale_after_secs` default of a feed without `every_secs`.
const IDLE_STALE_SECS: u64 = 8 * 86_400;

/// One `[feeds.<name>]` block (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedConfig {
    /// `"tool"` or `"tick"` (a string, so `"poll"` gets a validation error
    /// naming its item instead of a parse error).
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub windows: Vec<FeedWindow>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub at: Vec<String>,
    /// Zone of `windows` and `at`. Default `UTC`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tz: Option<String>,
    #[serde(default)]
    pub jitter_pct: u32,
    #[serde(default)]
    pub run_on_start: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub args: Map<String, Value>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub each: BTreeMap<String, Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub event: Map<String, Value>,
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_after_secs: Option<u64>,
}

/// One `windows` entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedWindow {
    /// Days the window starts on (`"Mon"` … `"Sun"`); empty = every day.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub days: Vec<String>,
    /// `"HH:MM"` local time.
    pub from: String,
    /// `"HH:MM"` or `"24:00"`; `<= from` ends the next day.
    pub to: String,
    pub every_secs: u64,
}

/// What a feed does when it fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedKind {
    Tool,
    Tick,
}

impl FeedConfig {
    /// `kind` parsed; the error is relative to the feed (`kind = …`).
    pub fn kind(&self) -> Result<FeedKind, String> {
        match self.kind.as_str() {
            "tool" => Ok(FeedKind::Tool),
            "tick" => Ok(FeedKind::Tick),
            "poll" => Err(
                "kind = \"poll\" is reserved for `info-fetch` (xmarket wave W2) and \
                           not built yet — use \"tool\" or \"tick\""
                    .to_string(),
            ),
            k @ ("stream" | "ws" | "rows") => Err(format!(
                "kind = \"{k}\" is reserved for a later xmarket milestone and not built yet — \
                 use \"tool\" or \"tick\""
            )),
            k => Err(format!(
                "kind = \"{k}\" is unknown — use \"tool\" or \"tick\""
            )),
        }
    }

    /// The schedule, built; errors are relative to the feed (`tz: …`).
    pub fn schedule(&self) -> Result<Schedule, Vec<String>> {
        let mut errs = Vec::new();
        let tz = self.tz.as_deref().unwrap_or("UTC");
        let zone = Zone::parse(tz).unwrap_or_else(|| {
            errs.push(format!(
                "tz: unknown zone `{tz}` (America/New_York, Europe/Paris or UTC)"
            ));
            Zone::Utc
        });
        let every_ms = self.every_secs.map(|s| {
            if !(1..=MAX_EVERY_SECS).contains(&s) {
                errs.push(format!("every_secs {s} must be 1–{MAX_EVERY_SECS}"));
            }
            s.saturating_mul(1000)
        });
        if self.windows.len() > MAX_WINDOWS {
            errs.push(format!("windows: at most {MAX_WINDOWS}"));
        }
        let windows = (self.windows.iter().enumerate())
            .filter_map(|(i, w)| w.build(&format!("windows[{i}]"), &mut errs))
            .collect();
        if self.at.len() > MAX_AT {
            errs.push(format!("at: at most {MAX_AT}"));
        }
        let at = (self.at.iter().enumerate())
            .filter_map(|(i, s)| {
                let tick = parse_at(s);
                if tick.is_none() {
                    errs.push(format!(
                        "at[{i}] `{s}` is not \"<Mon…Sun> HH:MM\" or \"daily HH:MM\""
                    ));
                }
                tick
            })
            .collect();
        let s = Schedule {
            zone,
            every_ms,
            windows,
            at,
        };
        if self.every_secs.is_none() && self.windows.is_empty() && self.at.is_empty() {
            errs.push("every_secs: set every_secs, windows or at — the feed never fires".into());
        }
        if errs.is_empty() {
            Ok(s)
        } else {
            Err(errs)
        }
    }

    /// One argument object per call of a run, in fan-out order: `args` plus
    /// each combination of `each` values (keys in name order, the last
    /// varying fastest). No `each` ⇒ one call with `args`.
    pub fn calls(&self) -> Vec<Value> {
        let mut out = vec![self.args.clone()];
        for (key, values) in &self.each {
            out = out
                .into_iter()
                .flat_map(|base| {
                    values.iter().map(move |v| {
                        let mut args = base.clone();
                        args.insert(key.clone(), v.clone());
                        args
                    })
                })
                .collect();
        }
        out.into_iter().map(Value::Object).collect()
    }

    /// Calls in flight at once (default 1).
    pub fn concurrency(&self) -> usize {
        self.concurrency.unwrap_or(1).clamp(1, MAX_CONCURRENCY)
    }

    /// `stale_after_secs`, else 3 × the longest interval (at least 60 s); a
    /// feed without `every_secs` (idle between its windows / at-ticks): 8
    /// days.
    pub fn stale_after_secs(&self) -> u64 {
        if let Some(s) = self.stale_after_secs {
            return s;
        }
        let longest = self.schedule().ok().and_then(|s| s.longest_interval_ms());
        match (self.every_secs, longest) {
            (Some(_), Some(ms)) => (ms / 1000).saturating_mul(3).max(60),
            _ => IDLE_STALE_SECS,
        }
    }
}

impl FeedWindow {
    fn build(&self, at: &str, errs: &mut Vec<String>) -> Option<Window> {
        let n = errs.len();
        let from = parse_clock(&self.from, false);
        if from.is_none() {
            errs.push(format!("{at}.from `{}` is not HH:MM", self.from));
        }
        let to = parse_clock(&self.to, true);
        if to.is_none() {
            errs.push(format!("{at}.to `{}` is not HH:MM or 24:00", self.to));
        }
        if from.is_some() && from == to {
            errs.push(format!("{at}: from and to are both {} (empty)", self.from));
        }
        if !(1..=MAX_EVERY_SECS).contains(&self.every_secs) {
            errs.push(format!(
                "{at}.every_secs {} must be 1–{MAX_EVERY_SECS}",
                self.every_secs
            ));
        }
        let mut days = if self.days.is_empty() {
            Days::ALL
        } else {
            Days::NONE
        };
        for d in &self.days {
            match parse_day(d) {
                Some(day) => days = days.with(day),
                None => errs.push(format!("{at}.days: `{d}` is not a day (Mon … Sun)")),
            }
        }
        (errs.len() == n).then(|| Window {
            days,
            from: from.unwrap_or(0),
            to: to.unwrap_or(0),
            every_ms: self.every_secs.saturating_mul(1000),
        })
    }
}

/// Problems in `[feeds]` (feeds in name order), including references to
/// `[agents]` and `[decision_loops]`.
pub(crate) fn validation_errors(cfg: &Config) -> Vec<String> {
    let mut out = Vec::new();
    for (name, feed) in &cfg.feeds {
        let p = format!("feeds.{name}");
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-'))
        {
            out.push(format!(
                "{p}: name must be [a-z0-9_-]+ (it is part of call ids and session ids)"
            ));
        }
        if let Err(errs) = feed.schedule() {
            out.extend(errs.into_iter().map(|e| format!("{p}.{e}")));
        }
        if feed.jitter_pct > MAX_JITTER_PCT {
            out.push(format!(
                "{p}.jitter_pct {} must be 0–{MAX_JITTER_PCT}",
                feed.jitter_pct
            ));
        }
        if feed.stale_after_secs == Some(0) {
            out.push(format!("{p}.stale_after_secs must be at least 1"));
        }
        match feed.kind() {
            Err(e) => out.push(format!("{p}.{e}")),
            Ok(FeedKind::Tool) => tool_errors(cfg, &p, feed, &mut out),
            Ok(FeedKind::Tick) => tick_errors(cfg, &p, feed, &mut out),
        }
    }
    out
}

fn tool_errors(cfg: &Config, p: &str, feed: &FeedConfig, out: &mut Vec<String>) {
    for (key, set) in [
        ("target", feed.target.is_some()),
        ("event", !feed.event.is_empty()),
    ] {
        if set {
            out.push(format!("{p}.{key} is only for kind = \"tick\""));
        }
    }
    let agent = required(feed.agent.as_deref(), p, "agent", "tool", out);
    let tool = required(feed.tool.as_deref(), p, "tool", "tool", out);
    if let Some(a) = agent {
        match cfg.agents.get(a) {
            None => out.push(format!("{p}.agent: no [agents.{a}] block")),
            Some(agent_cfg) => {
                if let Some(e) = tool.and_then(|t| tool_access(agent_cfg, a, t)) {
                    out.push(format!("{p}.tool: {e}"));
                }
            }
        }
    }
    for (key, values) in &feed.each {
        if feed.args.contains_key(key) {
            out.push(format!("{p}.each.{key}: also set in args"));
        }
        if values.is_empty() {
            out.push(format!(
                "{p}.each.{key}: empty list — the feed would make no call"
            ));
        }
    }
    let calls = (feed.each.values()).fold(1usize, |n, v| n.saturating_mul(v.len()));
    if calls > MAX_CALLS {
        out.push(format!(
            "{p}.each: {calls} calls per run (at most {MAX_CALLS})"
        ));
    }
    if let Some(c) = feed.concurrency {
        if !(1..=MAX_CONCURRENCY).contains(&c) {
            out.push(format!("{p}.concurrency {c} must be 1–{MAX_CONCURRENCY}"));
        }
    }
}

fn tick_errors(cfg: &Config, p: &str, feed: &FeedConfig, out: &mut Vec<String>) {
    for (key, set) in [
        ("agent", feed.agent.is_some()),
        ("tool", feed.tool.is_some()),
        ("args", !feed.args.is_empty()),
        ("each", !feed.each.is_empty()),
        ("concurrency", feed.concurrency.is_some()),
    ] {
        if set {
            out.push(format!("{p}.{key} is only for kind = \"tool\""));
        }
    }
    if let Some(t) = required(feed.target.as_deref(), p, "target", "tick", out) {
        if !cfg.decision_loops.contains_key(t) {
            out.push(format!("{p}.target: no [decision_loops.{t}] block"));
        }
    }
    if feed.event.contains_key("ts_ms") {
        out.push(format!(
            "{p}.event.ts_ms is set by the scheduler (the slot time)"
        ));
    }
}

/// A key `kind` needs; pushes an error when absent or blank.
fn required<'a>(
    v: Option<&'a str>,
    p: &str,
    key: &str,
    kind: &str,
    out: &mut Vec<String>,
) -> Option<&'a str> {
    let v = v.map(str::trim).filter(|s| !s.is_empty());
    if v.is_none() {
        out.push(format!("{p}.{key} is required for kind = \"{kind}\""));
    }
    v
}

/// Why `agent` (id `name`) cannot call `tool`: `tools` is the allow-list
/// (empty = every always-on tool); an opt-in tool also counts when it is in
/// `workspace_tools`.
fn tool_access(agent: &AgentConfig, name: &str, tool: &str) -> Option<String> {
    let opted_in = agent.workspace_tools.iter().any(|t| t == tool);
    if !agent.tools.is_empty() {
        (!opted_in && !agent.tools.iter().any(|t| t == tool))
            .then(|| format!("`{tool}` is not in [agents.{name}].tools"))
    } else if WORKSPACE_TOOLS.contains(&tool) && !opted_in {
        Some(format!(
            "`{tool}` is an opt-in tool: list it in [agents.{name}].tools or workspace_tools"
        ))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// An agent with an allow-list and one decision loop to point at; the
    /// xmarket agents share one workspace (`config/xmarket.rs`).
    const BASE: &str = r#"
        [agents.xm_feeds]
        engine = "openrouter"
        model = "m"
        workspace = "/srv/xm-ws"
        tools = ["hl_ctx", "hl_book", "risk_status"]

        [agents.xm_exec]
        engine = "openrouter"
        model = "m"
        workspace = "/srv/xm-ws"
        tools = ["xm_exits"]

        [agents.open]
        engine = "openrouter"
        model = "m"

        [decision_loops.xm_main]
        goal = "g"
        agent = "xm_feeds"
        [decision_loops.xm_main.actions.hold]
        description = "Nothing to do"
    "#;

    fn load(feeds: &str) -> Config {
        toml::from_str(&format!("{BASE}\n{feeds}")).unwrap_or_else(|e| panic!("{e}"))
    }

    fn errors(feeds: &str) -> Vec<String> {
        validation_errors(&load(feeds))
    }

    const WEEKEND_BOOK: &str = r#"
        [feeds.hl_book]
        kind = "tool"
        agent = "xm_feeds"
        tool = "hl_book"
        each = { coin = ["xyz:TSLA", "xyz:NVDA"] }
        every_secs = 300
        tz = "America/New_York"
        windows = [
          { days = ["Sun"], from = "17:00", to = "19:00", every_secs = 60 },
          { days = ["Mon"], from = "08:30", to = "09:30", every_secs = 60 },
        ]
        required = true
    "#;

    #[test]
    fn weekend_book_feed_builds_its_schedule_and_calls() {
        let cfg = load(WEEKEND_BOOK);
        assert!(validation_errors(&cfg).is_empty());
        cfg.validate().expect("the whole config validates");
        let f = &cfg.feeds["hl_book"];
        assert_eq!(f.kind(), Ok(FeedKind::Tool));
        let s = f.schedule().unwrap();
        assert_eq!((s.zone, s.every_ms), (Zone::NewYork, Some(300_000)));
        let sun = Days::NONE.with(chrono::Weekday::Sun);
        assert_eq!(
            s.windows[0],
            Window {
                days: sun,
                from: 17 * 60,
                to: 19 * 60,
                every_ms: 60_000
            }
        );
        assert_eq!((s.windows[1].from, s.windows[1].to), (510, 570));
        assert_eq!(
            f.calls(),
            vec![json!({"coin": "xyz:TSLA"}), json!({"coin": "xyz:NVDA"})]
        );
        assert_eq!((f.concurrency(), f.stale_after_secs()), (1, 900));
        assert!(f.required && !f.run_on_start && f.jitter_pct == 0);
    }

    #[test]
    fn fan_out_is_every_combination_in_key_order() {
        let f: FeedConfig = toml::from_str(
            r#"
            kind = "tool"
            args = { max_age_secs = 0 }
            each = { interval = ["1m", "5m"], coin = ["ETH", "xyz:TSLA"] }
        "#,
        )
        .unwrap();
        assert_eq!(
            f.calls(),
            vec![
                json!({"max_age_secs": 0, "coin": "ETH", "interval": "1m"}),
                json!({"max_age_secs": 0, "coin": "ETH", "interval": "5m"}),
                json!({"max_age_secs": 0, "coin": "xyz:TSLA", "interval": "1m"}),
                json!({"max_age_secs": 0, "coin": "xyz:TSLA", "interval": "5m"}),
            ]
        );
        let plain: FeedConfig = toml::from_str("kind = \"tool\"").unwrap();
        assert_eq!(plain.calls(), vec![json!({})]);
    }

    #[test]
    fn tick_feed_with_at_ticks_validates() {
        let cfg = load(
            r#"
            [feeds.weekend_entry]
            kind = "tick"
            target = "xm_main"
            tz = "America/New_York"
            at = ["Sun 18:00", "daily 09:00"]
            event = { phase = "entry" }
        "#,
        );
        assert_eq!(validation_errors(&cfg), Vec::<String>::new());
        let f = &cfg.feeds["weekend_entry"];
        assert_eq!(f.kind(), Ok(FeedKind::Tick));
        assert_eq!(f.schedule().unwrap().at.len(), 2);
        assert_eq!(f.stale_after_secs(), 8 * 86_400);
        // Windows only: idle between them, so not stale within the week.
        let w: FeedConfig = toml::from_str(
            "kind = \"tick\"\nwindows = [{ days = [\"Sun\"], from = \"17:00\", to = \"19:00\", every_secs = 60 }]",
        )
        .unwrap();
        assert_eq!(w.stale_after_secs(), 8 * 86_400);
        let fast: FeedConfig = toml::from_str("kind = \"tick\"\nevery_secs = 5").unwrap();
        assert_eq!(fast.stale_after_secs(), 60);
    }

    #[test]
    fn validation_names_every_problem() {
        for (feeds, want) in [
            (
                "[feeds.news]\nkind = \"poll\"\nevery_secs = 60",
                "feeds.news.kind = \"poll\" is reserved for `info-fetch` (xmarket wave W2)",
            ),
            (
                "[feeds.x]\nkind = \"ws\"\nevery_secs = 60",
                "feeds.x.kind = \"ws\" is reserved for a later xmarket milestone",
            ),
            (
                "[feeds.x]\nkind = \"cron\"\nevery_secs = 60",
                "feeds.x.kind = \"cron\" is unknown",
            ),
            (
                "[feeds.x]\nkind = \"tool\"\nevery_secs = 60",
                "feeds.x.agent is required for kind = \"tool\"",
            ),
            (
                "[feeds.x]\nkind = \"tool\"\nagent = \"xm_feeds\"\nevery_secs = 60",
                "feeds.x.tool is required for kind = \"tool\"",
            ),
            (
                "[feeds.x]\nkind = \"tool\"\nagent = \"nobody\"\ntool = \"hl_ctx\"\nevery_secs = 60",
                "feeds.x.agent: no [agents.nobody] block",
            ),
            (
                "[feeds.x]\nkind = \"tool\"\nagent = \"xm_feeds\"\ntool = \"read_file\"\nevery_secs = 60",
                "feeds.x.tool: `read_file` is not in [agents.xm_feeds].tools",
            ),
            (
                "[feeds.x]\nkind = \"tool\"\nagent = \"open\"\ntool = \"sol_price\"\nevery_secs = 60",
                "feeds.x.tool: `sol_price` is an opt-in tool",
            ),
            (
                "[feeds.x]\nkind = \"tick\"\nevery_secs = 60",
                "feeds.x.target is required for kind = \"tick\"",
            ),
            (
                "[feeds.x]\nkind = \"tick\"\ntarget = \"nope\"\nevery_secs = 60",
                "feeds.x.target: no [decision_loops.nope] block",
            ),
            (
                "[feeds.x]\nkind = \"tick\"\ntarget = \"xm_main\"\nevery_secs = 60\nevent = { ts_ms = 1 }",
                "feeds.x.event.ts_ms is set by the scheduler",
            ),
            (
                "[feeds.x]\nkind = \"tick\"\ntarget = \"xm_main\"\nevery_secs = 60\ntool = \"hl_ctx\"",
                "feeds.x.tool is only for kind = \"tool\"",
            ),
            (
                "[feeds.x]\nkind = \"tool\"\nagent = \"xm_feeds\"\ntool = \"hl_ctx\"\nevery_secs = 60\ntarget = \"xm_main\"",
                "feeds.x.target is only for kind = \"tick\"",
            ),
            (
                "[feeds.x]\nkind = \"tick\"\ntarget = \"xm_main\"",
                "feeds.x.every_secs: set every_secs, windows or at",
            ),
            (
                "[feeds.x]\nkind = \"tick\"\ntarget = \"xm_main\"\nevery_secs = 0",
                "feeds.x.every_secs 0 must be 1–604800",
            ),
            (
                "[feeds.x]\nkind = \"tick\"\ntarget = \"xm_main\"\nevery_secs = 60\ntz = \"Asia/Almaty\"",
                "feeds.x.tz: unknown zone `Asia/Almaty`",
            ),
            (
                "[feeds.x]\nkind = \"tick\"\ntarget = \"xm_main\"\nat = [\"Sunday 18:00\"]",
                "feeds.x.at[0] `Sunday 18:00` is not",
            ),
            (
                "[feeds.x]\nkind = \"tick\"\ntarget = \"xm_main\"\nwindows = [{ from = \"17:00\", to = \"17:00\", every_secs = 60 }]",
                "feeds.x.windows[0]: from and to are both 17:00 (empty)",
            ),
            (
                "[feeds.x]\nkind = \"tick\"\ntarget = \"xm_main\"\nwindows = [{ days = [\"Sun\", \"Funday\"], from = \"25:00\", to = \"19:00\", every_secs = 0 }]",
                "feeds.x.windows[0].from `25:00` is not HH:MM",
            ),
            (
                "[feeds.x]\nkind = \"tick\"\ntarget = \"xm_main\"\nwindows = [{ days = [\"Funday\"], from = \"17:00\", to = \"19:00\", every_secs = 60 }]",
                "feeds.x.windows[0].days: `Funday` is not a day",
            ),
            (
                "[feeds.x]\nkind = \"tick\"\ntarget = \"xm_main\"\nevery_secs = 60\njitter_pct = 60",
                "feeds.x.jitter_pct 60 must be 0–50",
            ),
            (
                "[feeds.x]\nkind = \"tick\"\ntarget = \"xm_main\"\nevery_secs = 60\nstale_after_secs = 0",
                "feeds.x.stale_after_secs must be at least 1",
            ),
            (
                "[feeds.x]\nkind = \"tool\"\nagent = \"xm_feeds\"\ntool = \"hl_book\"\nevery_secs = 60\nargs = { coin = \"ETH\" }\neach = { coin = [\"BTC\"] }",
                "feeds.x.each.coin: also set in args",
            ),
            (
                "[feeds.x]\nkind = \"tool\"\nagent = \"xm_feeds\"\ntool = \"hl_book\"\nevery_secs = 60\neach = { coin = [] }",
                "feeds.x.each.coin: empty list",
            ),
            (
                "[feeds.x]\nkind = \"tool\"\nagent = \"xm_feeds\"\ntool = \"hl_book\"\nevery_secs = 60\nconcurrency = 0",
                "feeds.x.concurrency 0 must be 1–32",
            ),
            (
                "[feeds.\"hl:book\"]\nkind = \"tick\"\ntarget = \"xm_main\"\nevery_secs = 60",
                "feeds.hl:book: name must be [a-z0-9_-]+",
            ),
        ] {
            let errs = errors(feeds);
            assert!(
                errs.iter().any(|e| e.starts_with(want)),
                "{feeds}\nwant: {want}\ngot: {errs:#?}"
            );
        }
        // Opted in via workspace_tools, or no allow-list and not opt-in: fine.
        let mut cfg = load(
            "[feeds.x]\nkind = \"tool\"\nagent = \"open\"\ntool = \"read_file\"\nevery_secs = 60",
        );
        assert!(validation_errors(&cfg).is_empty());
        cfg.feeds.get_mut("x").unwrap().tool = Some("sol_price".into());
        cfg.agents.get_mut("open").unwrap().workspace_tools = vec!["sol_price".into()];
        assert!(validation_errors(&cfg).is_empty());
        // The whole config refuses to load with a bad feed.
        let bad = load("[feeds.news]\nkind = \"poll\"\nevery_secs = 60");
        let e = format!("{:#}", bad.validate().unwrap_err());
        assert!(e.contains("info-fetch"), "{e}");
    }

    #[test]
    fn unknown_keys_are_parse_errors() {
        let e = toml::from_str::<Config>(&format!(
            "{BASE}\n[feeds.x]\nkind = \"tick\"\ntarget = \"xm_main\"\nevery_sec = 60\n"
        ))
        .unwrap_err();
        assert!(e.to_string().contains("every_sec"), "{e}");
        let e = toml::from_str::<Config>(&format!(
            "{BASE}\n[feeds.x]\nkind = \"tick\"\ntarget = \"xm_main\"\n\
             windows = [{{ from = \"17:00\", to = \"19:00\", every = 60 }}]\n"
        ))
        .unwrap_err();
        assert!(e.to_string().contains("every"), "{e}");
    }

    /// The commented `[feeds.*]` block of `config.example.toml`, uncommented
    /// (with its agent and loop), is valid.
    #[test]
    fn example_block_uncommented_is_valid() {
        let text = include_str!("../../config.example.toml");
        let block: Vec<&str> = text
            .lines()
            .skip_while(|l| *l != "# [feeds.hl_ctx]")
            .take_while(|l| l.starts_with('#'))
            .map(|l| l.strip_prefix("# ").unwrap_or(l.trim_start_matches('#')))
            .collect();
        assert!(block.len() > 20, "{block:?}");
        // The exec agent holds the weekend fade too (as in the example).
        let base = BASE.replace(
            "tools = [\"xm_exits\"]",
            "tools = [\"xm_exits\", \"xm_weekend_fade\"]",
        );
        let cfg: Config = toml::from_str(&format!("{base}\n{}", block.join("\n")))
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(validation_errors(&cfg), Vec::<String>::new());
        let names: Vec<&str> = cfg.feeds.keys().map(String::as_str).collect();
        assert_eq!(
            names,
            [
                "hl_book",
                "hl_ctx",
                "risk_day",
                "weekend_review",
                "xm_exits",
                "xm_weekend_fade"
            ]
        );
        assert_eq!(
            cfg.feeds["hl_book"].schedule().unwrap(),
            load(WEEKEND_BOOK).feeds["hl_book"].schedule().unwrap()
        );
        // risk_status once a day at 00:01 UTC, no arguments.
        let day = &cfg.feeds["risk_day"];
        let s = day.schedule().unwrap();
        assert_eq!((s.zone, s.every_ms, s.at.len()), (Zone::Utc, None, 1));
        assert_eq!((s.at[0].days, s.at[0].minute), (Days::ALL, 1));
        assert_eq!(day.calls(), vec![json!({})]);
        assert_eq!(day.stale_after_secs(), 90_000);
        let review = &cfg.feeds["weekend_review"];
        assert_eq!(review.kind(), Ok(FeedKind::Tick));
        assert_eq!(review.schedule().unwrap().at.len(), 2);
        // The exit rules every 15 s on the private exec agent, no arguments.
        let exits = &cfg.feeds["xm_exits"];
        assert_eq!(
            (
                exits.kind(),
                exits.schedule().unwrap().every_ms,
                exits.required
            ),
            (Ok(FeedKind::Tool), Some(15_000), true)
        );
        assert_eq!(exits.calls(), vec![json!({})]);
        // The weekend fade every 60 s on the same agent, no arguments.
        let fade = &cfg.feeds["xm_weekend_fade"];
        assert_eq!(
            (
                fade.kind(),
                fade.schedule().unwrap().every_ms,
                fade.required,
                fade.agent.as_deref()
            ),
            (Ok(FeedKind::Tool), Some(60_000), true, Some("xm_exec"))
        );
        assert_eq!(fade.calls(), vec![json!({})]);
    }
}
