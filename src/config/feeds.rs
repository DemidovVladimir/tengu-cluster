//! `[feeds.<name>]` — scheduled work of `tengu run` (xmarket `rt-scheduler`;
//! fire times `domain/schedule.rs`, runner `application/runtime/feeds.rs`,
//! operator doc `docs/runtime-2026-09-30.md` § Feeds). `deny_unknown_fields`.
//!
//! | Key | Kind | Default | Meaning |
//! |---|---|---|---|
//! | `kind` | — | required | `"tool"` calls a tool · `"tick"` sends a loop event · `"job"` runs a named application job; `"poll"` (`info-fetch`, W2) and `"stream"` / `"ws"` / `"rows"` are refused until built |
//! | `every_secs` | all | — | base interval (1 s – 7 d) on the UTC epoch grid |
//! | `windows` | all | `[]` | `{ days = ["Sun"], from = "17:00", to = "19:00", every_secs = 60 }`: local time in `tz`, replaces `every_secs` inside; `to <= from` ends the next day, `"24:00"` = midnight; no `days` = every day |
//! | `at` | all | `[]` | clock ticks in `tz`: `"Sun 18:00"`, `"daily 09:00"` |
//! | `tz` | all | `UTC` | `America/New_York` · `Europe/Paris` · `UTC` |
//! | `jitter_pct` | all | 0 | a grid fire starts up to this % (0–50) of its interval late; at-ticks are exact |
//! | `run_on_start` | all | `false` | one run as soon as `tengu run` starts |
//! | `agent`, `tool` | tool | required | `[agents.<agent>]` runs `tool` — listed in its `tools` (or opted in via `workspace_tools`) — with the executor a decision loop of that agent gets |
//! | `args` | tool | `{}` | the tool's arguments |
//! | `each` | tool | `{}` | fan-out, `{ coin = ["xyz:TSLA", "xyz:NVDA"] }` = one call per value; several keys = every combination (keys in name order, the last varying fastest); ≤ 500 calls per run |
//! | `concurrency` | tool | 1 | calls of one run in flight at once (1–32) |
//! | `target` | tick | required | the `[decision_loops.<target>]` the event goes to |
//! | `event` | tick | `{}` | the event table; the scheduler adds `ts_ms` (the slot time) |
//! | `job` | job | required | one of [`JOBS`] — a closed list of application jobs, never a command from config |
//! | `required` | all | `false` | `tengu doctor --live` fails when the feed is down or stale |
//! | `stale_after_secs` | all | 3 × the longest interval (≥ 60); no `every_secs` (idle between windows / at-ticks): 8 days | no item for this long ⇒ stale |
//!
//! | Job ([`JOBS`]) | Needs | Runs (`bootstrap/runtime.rs::job_for`) |
//! |---|---|---|
//! | `soe_cycle` | `[soe]` (`config/soe.rs`) | the week's SOE cycle (`application/soe/job.rs`): the ISO week of the slot in `tz`, decided at the slot time; a week already frozen is a no-op |
//!
//! At least one of `every_secs`, `windows`, `at`. No `budget` key: tools
//! budget themselves against `[rate_limits.<name>]` (`hl-info-client`).
//! Strategy ranking stays a `kind = "tool"` feed (critic C14).

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
/// The weekly SOE cycle (`application/soe/job.rs`).
pub const JOB_SOE_CYCLE: &str = "soe_cycle";
/// Every job a `kind = "job"` feed may name (module table).
pub const JOBS: &[&str] = &[JOB_SOE_CYCLE];

/// One `[feeds.<name>]` block (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedConfig {
    /// `"tool"`, `"tick"` or `"job"` (a string, so `"poll"` gets a
    /// validation error naming its item instead of a parse error).
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
    /// `kind = "job"`: one of [`JOBS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<String>,
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
    /// A named application job ([`JOBS`]).
    Job,
}

impl FeedConfig {
    /// `kind` parsed; the error is relative to the feed (`kind = …`).
    pub fn kind(&self) -> Result<FeedKind, String> {
        match self.kind.as_str() {
            "tool" => Ok(FeedKind::Tool),
            "tick" => Ok(FeedKind::Tick),
            "job" => Ok(FeedKind::Job),
            "poll" => Err(
                "kind = \"poll\" is reserved for `info-fetch` (xmarket wave W2) and \
                           not built yet — use \"tool\", \"tick\" or \"job\""
                    .to_string(),
            ),
            k @ ("stream" | "ws" | "rows") => Err(format!(
                "kind = \"{k}\" is reserved for a later xmarket milestone and not built yet — \
                 use \"tool\", \"tick\" or \"job\""
            )),
            k => Err(format!(
                "kind = \"{k}\" is unknown — use \"tool\", \"tick\" or \"job\""
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
            Ok(FeedKind::Job) => job_errors(cfg, &p, feed, &mut out),
        }
        if feed.job.is_some() && feed.kind().is_ok_and(|k| k != FeedKind::Job) {
            out.push(format!("{p}.job is only for kind = \"job\""));
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

/// `kind = "job"`: a known job and what it needs (module table).
fn job_errors(cfg: &Config, p: &str, feed: &FeedConfig, out: &mut Vec<String>) {
    for (key, set) in [
        ("agent", feed.agent.is_some()),
        ("tool", feed.tool.is_some()),
        ("args", !feed.args.is_empty()),
        ("each", !feed.each.is_empty()),
        ("concurrency", feed.concurrency.is_some()),
        ("target", feed.target.is_some()),
        ("event", !feed.event.is_empty()),
    ] {
        if set {
            out.push(format!("{p}.{key} is not for kind = \"job\""));
        }
    }
    match required(feed.job.as_deref(), p, "job", "job", out) {
        Some(JOB_SOE_CYCLE) if cfg.soe.is_none() => out.push(format!(
            "{p}.job: `{JOB_SOE_CYCLE}` needs a [soe] section (config/soe.rs)"
        )),
        Some(j) if !JOBS.contains(&j) => out.push(format!(
            "{p}.job: `{j}` is not a known job ({})",
            JOBS.join(", ")
        )),
        _ => {}
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
#[path = "feeds_tests.rs"]
mod tests;
