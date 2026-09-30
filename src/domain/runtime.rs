//! `tengu run` — pure data of the long-running process: the single-runner
//! lease (one runner per sandbox), the heartbeat file, the `loop/1` /
//! `feed/1` health rows and the `tengu doctor --live` verdict. No IO;
//! `now_ms` is always an input. Composition `bootstrap/runtime.rs`; operator
//! doc `docs/runtime-2026-09-30.md`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::observation::{
    set_bool, set_int, set_num, set_str, ErrorClass, Features, Observation, Observed,
};

/// Lease TTL: a crashed runner's lease frees after this.
pub const LEASE_TTL_MS: i64 = 30_000;
/// Renewal period of a live runner (a third of the TTL).
pub const LEASE_RENEW_MS: u64 = 10_000;

/// `runtime:<sandbox>` — the lease one `tengu run` of a sandbox holds.
pub fn lease_resource(sandbox: &str) -> String {
    format!("runtime:{sandbox}")
}

/// `run-<sandbox>.json` — the heartbeat file in the runtime state dir.
pub fn heartbeat_file(sandbox: &str) -> String {
    format!("run-{sandbox}.json")
}

/// One acquire / renew of a lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerLease {
    pub resource: String,
    pub holder: String,
    pub granted: bool,
    /// Who holds it now (`holder` when granted).
    pub current_holder: String,
    pub acquired_at_ms: i64,
    pub expires_at_ms: i64,
}

impl RunnerLease {
    /// Whole seconds until expiry at `now_ms` (0 once expired).
    pub fn remaining_secs(&self, now_ms: i64) -> u64 {
        (self.expires_at_ms.saturating_sub(now_ms).max(0) as u64).div_ceil(1000)
    }
}

/// Lifecycle of the process as the heartbeat reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Running,
    Stopping,
    Stopped,
}

impl RunState {
    pub fn as_str(self) -> &'static str {
        match self {
            RunState::Running => "running",
            RunState::Stopping => "stopping",
            RunState::Stopped => "stopped",
        }
    }
}

/// `<state dir>/run-<sandbox>.json`: rewritten every `[runtime]
/// heartbeat_secs`, and on stop (`stopping`, then `stopped`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Heartbeat {
    pub sandbox: String,
    pub pid: u32,
    /// Lease holder `<host>:<pid>:<uuid>`.
    pub holder: String,
    pub state: RunState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    pub started_at_ms: i64,
    pub ts_ms: i64,
    pub heartbeat_secs: u64,
    #[serde(default)]
    pub loops: BTreeMap<String, LoopHealth>,
    #[serde(default)]
    pub feeds: BTreeMap<String, FeedHealth>,
}

/// `loop/1:<name>` — one decision loop of `tengu run`, written to the loop
/// agent's observation store every heartbeat.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoopHealth {
    pub name: String,
    /// Accepted events waiting for the loop or an in-flight slot.
    pub queue_depth: u32,
    /// Events running now (0 or 1).
    pub in_flight: u32,
    pub accepted: u64,
    pub completed: u64,
    pub failed: u64,
    /// Refused / dropped at shutdown or aborted at the deadline.
    pub dropped: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_event_at_ms: Option<i64>,
    /// When the last event finished — the loop's last decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_decision_at_ms: Option<i64>,
    /// Age of `last_decision_at_ms` at `updated_at_ms`; `None` = none yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_decision_age_s: Option<f64>,
    /// Never a URL (`scrub_urls`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub updated_at_ms: i64,
}

impl Observed for LoopHealth {
    const SCHEMA: &'static str = "loop/1";
    fn subject(&self) -> String {
        self.name.clone()
    }
    fn headline(&self) -> String {
        format!(
            "loop {} in_flight={} queue={} done={} failed={} dropped={} last_decision={}",
            self.name,
            self.in_flight,
            self.queue_depth,
            self.completed,
            self.failed,
            self.dropped,
            age_text(self.last_decision_age_s)
        )
    }
    fn features(&self) -> Features {
        let mut f = Features::new();
        set_int(&mut f, "queue_depth", Some(self.queue_depth.into()));
        set_int(&mut f, "in_flight", Some(self.in_flight.into()));
        set_int(&mut f, "accepted", Some(clamp_i64(self.accepted)));
        set_int(&mut f, "completed", Some(clamp_i64(self.completed)));
        set_int(&mut f, "failed", Some(clamp_i64(self.failed)));
        set_int(&mut f, "dropped", Some(clamp_i64(self.dropped)));
        set_num(&mut f, "last_decision_age_s", self.last_decision_age_s);
        f
    }
}

/// State of a feed (`[feeds.<n>]`, scheduler next wave).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedState {
    Connecting,
    Live,
    Backoff,
    Stalled,
    Down,
}

impl FeedState {
    pub fn as_str(self) -> &'static str {
        match self {
            FeedState::Connecting => "connecting",
            FeedState::Live => "live",
            FeedState::Backoff => "backoff",
            FeedState::Stalled => "stalled",
            FeedState::Down => "down",
        }
    }
}

/// `feed/1:<name>` — one feed of `tengu run`. The row is stamped with the
/// feed's last item (`observed_at_ms = last_item_at_ms`), so a loop's
/// `requires = { feed = 10 }` means "delivered within 10 s"; no row before
/// the first item (the heartbeat still lists the feed). Transitions are the
/// `on_*` methods; `refresh` turns a quiet `live` feed into `stalled`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeedHealth {
    pub name: String,
    pub state: FeedState,
    /// `tengu doctor --live` fails when a required feed is down or stale.
    pub required: bool,
    /// No item for this long (since start while none) ⇒ stale.
    pub stale_after_s: u64,
    pub started_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_item_at_ms: Option<i64>,
    /// Age of the last item at `updated_at_ms`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_item_age_s: Option<f64>,
    pub items: u64,
    pub reconnects: u64,
    pub dropped: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error_class: Option<ErrorClass>,
    /// Never a URL (`scrub_urls`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub updated_at_ms: i64,
}

impl FeedHealth {
    /// A registered feed that has not answered yet: `connecting`.
    pub fn new(name: &str, required: bool, stale_after_s: u64, now_ms: i64) -> Self {
        Self {
            name: name.to_string(),
            state: FeedState::Connecting,
            required,
            stale_after_s,
            started_at_ms: now_ms,
            last_item_at_ms: None,
            last_item_age_s: None,
            items: 0,
            reconnects: 0,
            dropped: 0,
            last_error_class: None,
            last_error: None,
            updated_at_ms: now_ms,
        }
    }

    /// ms without an item at `now_ms` (since start while none).
    pub fn quiet_ms(&self, now_ms: i64) -> i64 {
        now_ms
            .saturating_sub(self.last_item_at_ms.unwrap_or(self.started_at_ms))
            .max(0)
    }

    /// No item within `stale_after_s`.
    pub fn is_stale(&self, now_ms: i64) -> bool {
        self.quiet_ms(now_ms) > (self.stale_after_s as i64).saturating_mul(1000)
    }

    /// Timestamp of the `feed/1` row: the last item (`None` = no row yet).
    pub fn row_at_ms(&self) -> Option<i64> {
        self.last_item_at_ms
    }

    /// An item arrived: `live`.
    pub fn on_item(&mut self, now_ms: i64) {
        self.state = FeedState::Live;
        self.items += 1;
        self.last_item_at_ms = Some(now_ms);
        self.refresh(now_ms);
    }

    /// A read failed: `backoff` while the feed retries, else `down`.
    pub fn on_error(&mut self, class: ErrorClass, message: &str, retrying: bool, now_ms: i64) {
        self.state = if retrying {
            FeedState::Backoff
        } else {
            FeedState::Down
        };
        self.last_error_class = Some(class);
        self.last_error = Some(scrub_urls(message));
        self.refresh(now_ms);
    }

    /// (Re)connecting: `connecting`, one more reconnect.
    pub fn on_reconnect(&mut self, now_ms: i64) {
        self.state = FeedState::Connecting;
        self.reconnects += 1;
        self.refresh(now_ms);
    }

    /// Items dropped (queue full, coalesced away).
    pub fn on_dropped(&mut self, n: u64, now_ms: i64) {
        self.dropped += n;
        self.refresh(now_ms);
    }

    /// Ages at `now_ms`; a quiet `live` feed becomes `stalled`.
    pub fn refresh(&mut self, now_ms: i64) {
        self.updated_at_ms = now_ms;
        self.last_item_age_s = self.last_item_at_ms.map(|t| secs(now_ms.saturating_sub(t)));
        if self.state == FeedState::Live && self.is_stale(now_ms) {
            self.state = FeedState::Stalled;
        }
    }
}

impl Observed for FeedHealth {
    const SCHEMA: &'static str = "feed/1";
    fn subject(&self) -> String {
        self.name.clone()
    }
    fn headline(&self) -> String {
        let mut h = format!(
            "feed {} {}{} items={} last_item={} reconnects={} dropped={}",
            self.name,
            self.state.as_str(),
            if self.required { " required" } else { "" },
            self.items,
            age_text(self.last_item_age_s),
            self.reconnects,
            self.dropped
        );
        if let Some(class) = self.last_error_class {
            h.push_str(&format!(" error={}", class.as_str()));
        }
        h
    }
    fn features(&self) -> Features {
        let mut f = Features::new();
        set_str(&mut f, "state", Some(self.state.as_str()));
        set_bool(&mut f, "required", Some(self.required));
        set_int(&mut f, "stale_after_s", Some(clamp_i64(self.stale_after_s)));
        set_num(&mut f, "last_item_age_s", self.last_item_age_s);
        set_int(&mut f, "items", Some(clamp_i64(self.items)));
        set_int(&mut f, "reconnects", Some(clamp_i64(self.reconnects)));
        set_int(&mut f, "dropped", Some(clamp_i64(self.dropped)));
        set_str(
            &mut f,
            "last_error_class",
            self.last_error_class.map(ErrorClass::as_str),
        );
        f
    }
}

/// Replace every `http(s)://` / `ws(s)://` run with `<url>` — health rows and
/// heartbeats never carry URLs (RPC URLs embed API keys).
pub fn scrub_urls(s: &str) -> String {
    const SCHEMES: [&str; 4] = ["https://", "http://", "wss://", "ws://"];
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = SCHEMES.iter().filter_map(|p| rest.find(p)).min() {
        out.push_str(&rest[..i]);
        out.push_str("<url>");
        let tail = &rest[i..];
        let end = tail
            .find(|c: char| c.is_whitespace() || matches!(c, ')' | '"' | '\'' | '>' | ','))
            .unwrap_or(tail.len());
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// What `tengu doctor --live` found at `run-<sandbox>.json`.
#[derive(Debug, Clone, PartialEq)]
pub enum HeartbeatRead {
    Missing,
    Unreadable(String),
    Found(Heartbeat),
}

/// Knobs of `tengu doctor --live`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveKnobs {
    /// `[runtime] heartbeat_stale_secs`.
    pub heartbeat_stale_secs: u64,
}

/// One line of `tengu doctor --live`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveCheck {
    pub subject: String,
    pub ok: bool,
    pub detail: String,
}

/// Every check; the run is live only when all pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LiveReport {
    pub checks: Vec<LiveCheck>,
}

impl LiveReport {
    pub fn ok(&self) -> bool {
        self.checks.iter().all(|c| c.ok)
    }

    fn push(&mut self, subject: String, ok: bool, detail: String) {
        self.checks.push(LiveCheck {
            subject,
            ok,
            detail,
        });
    }
}

/// The `tengu doctor --live` verdict over the heartbeat file and the
/// `loop/1` / `feed/1` rows read from the stores (per name the newest
/// `updated_at_ms` wins). Fails when the heartbeat is missing, unreadable,
/// older than `heartbeat_stale_secs` or not `running`; when a required feed
/// is `down`, stale (no item within `stale_after_s`) or not updated for
/// longer than `max(stale_after_s, heartbeat_stale_secs)`; when a health row
/// does not decode. Loops are reported, never failed.
pub fn live_verdict(
    sandbox: &str,
    heartbeat: &HeartbeatRead,
    rows: &[Observation],
    now_ms: i64,
    knobs: LiveKnobs,
) -> LiveReport {
    let mut report = LiveReport::default();
    let limit_ms = (knobs.heartbeat_stale_secs as i64).saturating_mul(1000);
    let (mut loops, mut feeds) = (BTreeMap::new(), BTreeMap::new());
    match heartbeat {
        HeartbeatRead::Missing => report.push(
            "heartbeat".into(),
            false,
            format!(
                "missing: no {} — is `tengu run --sandbox {sandbox}` running?",
                heartbeat_file(sandbox)
            ),
        ),
        HeartbeatRead::Unreadable(e) => report.push(
            "heartbeat".into(),
            false,
            format!("{} unreadable: {e}", heartbeat_file(sandbox)),
        ),
        HeartbeatRead::Found(hb) => {
            let age_ms = now_ms.saturating_sub(hb.ts_ms);
            let who = format!("pid {} · holder {}", hb.pid, hb.holder);
            let (ok, what) = match hb.state {
                RunState::Running if age_ms <= limit_ms => (
                    true,
                    format!(
                        "running · up {} · last beat {} ago (limit {} s)",
                        dur(now_ms.saturating_sub(hb.started_at_ms)),
                        dur(age_ms),
                        knobs.heartbeat_stale_secs
                    ),
                ),
                RunState::Running => (
                    false,
                    format!(
                        "stale: last beat {} ago (limit {} s)",
                        dur(age_ms),
                        knobs.heartbeat_stale_secs
                    ),
                ),
                state => (
                    false,
                    format!(
                        "{} {} ago: {}",
                        state.as_str(),
                        dur(age_ms),
                        hb.stop_reason.as_deref().unwrap_or("no reason recorded")
                    ),
                ),
            };
            report.push("heartbeat".into(), ok, format!("{what} · {who}"));
            loops.extend(hb.loops.clone());
            feeds.extend(hb.feeds.clone());
        }
    }
    for row in rows {
        let decoded = match row.schema.as_str() {
            s if s == LoopHealth::SCHEMA => row.typed::<LoopHealth>().map(|l| {
                newest(&mut loops, l.name.clone(), l, |x| x.updated_at_ms);
            }),
            s if s == FeedHealth::SCHEMA => row.typed::<FeedHealth>().map(|f| {
                newest(&mut feeds, f.name.clone(), f, |x| x.updated_at_ms);
            }),
            _ => Ok(()),
        };
        if let Err(e) = decoded {
            report.push(format!("row {}", row.key), false, format!("{e:#}"));
        }
    }
    for (name, l) in &loops {
        report.push(format!("loop {name}"), true, loop_detail(l, now_ms));
    }
    for (name, f) in &feeds {
        let (ok, detail) = feed_check(f, now_ms, limit_ms);
        let kind = if f.required { "required" } else { "optional" };
        report.push(format!("feed {name} ({kind})"), ok, detail);
    }
    report
}

fn newest<T>(map: &mut BTreeMap<String, T>, name: String, v: T, at: impl Fn(&T) -> i64) {
    match map.get(&name) {
        Some(old) if at(old) >= at(&v) => {}
        _ => {
            map.insert(name, v);
        }
    }
}

fn loop_detail(l: &LoopHealth, now_ms: i64) -> String {
    let mut d = format!(
        "in_flight {} · queue {} · done {} · failed {} · dropped {} · last decision {}",
        l.in_flight,
        l.queue_depth,
        l.completed,
        l.failed,
        l.dropped,
        match l.last_decision_at_ms {
            Some(t) => format!("{} ago", dur(now_ms.saturating_sub(t))),
            None => "never".into(),
        }
    );
    if let Some(e) = &l.last_error {
        d.push_str(&format!(" · last error: {e}"));
    }
    d
}

/// (ok, detail) for one feed; only a required feed can fail.
fn feed_check(f: &FeedHealth, now_ms: i64, heartbeat_limit_ms: i64) -> (bool, String) {
    let stale_ms = (f.stale_after_s as i64).saturating_mul(1000);
    let quiet = match f.last_item_at_ms {
        Some(_) => format!("last item {} ago", dur(f.quiet_ms(now_ms))),
        None => format!("no item in {} since start", dur(f.quiet_ms(now_ms))),
    };
    let mut detail = format!(
        "{} · items {} · {quiet} (stale after {} s) · reconnects {} · dropped {}",
        f.state.as_str(),
        f.items,
        f.stale_after_s,
        f.reconnects,
        f.dropped
    );
    if let Some(class) = f.last_error_class {
        detail.push_str(&format!(
            " · last error {}: {}",
            class.as_str(),
            f.last_error.as_deref().unwrap_or("")
        ));
    }
    let not_updated_ms = now_ms.saturating_sub(f.updated_at_ms);
    let problem = if !f.required {
        None
    } else if f.state == FeedState::Down {
        Some("down".to_string())
    } else if f.is_stale(now_ms) {
        Some("stale".to_string())
    } else if not_updated_ms > stale_ms.max(heartbeat_limit_ms) {
        Some(format!("not updated for {}", dur(not_updated_ms)))
    } else {
        None
    };
    match problem {
        Some(p) => (false, format!("{p}: {detail}")),
        None => (true, detail),
    }
}

/// `12 s` · `4 min` · `3 h 5 min`.
fn dur(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    match s {
        0..=119 => format!("{s} s"),
        120..=7199 => format!("{} min", s / 60),
        _ => format!("{} h {} min", s / 3600, (s % 3600) / 60),
    }
}

fn age_text(age_s: Option<f64>) -> String {
    match age_s {
        Some(a) => format!("{a:.0}s"),
        None => "never".into(),
    }
}

fn secs(ms: i64) -> f64 {
    (ms.max(0) as f64 / 100.0).round() / 10.0
}

fn clamp_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::observation::{assert_features_ok, ObsSource, MAX_LINE1_CHARS};

    const T0: i64 = 1_790_000_000_000;
    const HOLDER: &str = "vps-1:4242:0f8c6a52-5d0e-4c8e-9a71-3b2f1c9d7e44";

    #[test]
    fn resource_and_file_keep_the_full_sandbox_name() {
        assert_eq!(lease_resource("xmarket-weekend"), "runtime:xmarket-weekend");
        assert_eq!(
            heartbeat_file("xmarket-weekend"),
            "run-xmarket-weekend.json"
        );
    }

    #[test]
    fn remaining_secs_rounds_up_and_saturates() {
        let l = RunnerLease {
            resource: lease_resource("s"),
            holder: "a".into(),
            granted: true,
            current_holder: "a".into(),
            acquired_at_ms: 0,
            expires_at_ms: 30_000,
        };
        assert_eq!(l.remaining_secs(0), 30);
        assert_eq!(l.remaining_secs(29_001), 1);
        assert_eq!(l.remaining_secs(30_000), 0);
        assert_eq!(l.remaining_secs(99_000), 0);
    }

    #[test]
    fn scrub_urls_hides_every_url_and_keeps_the_rest() {
        let e = "decisions request failed: error sending request for url \
                 (https://openrouter.ai/api/alpha/decisions?key=sk-or-1): client error";
        assert_eq!(
            scrub_urls(e),
            "decisions request failed: error sending request for url (<url>): client error"
        );
        assert_eq!(
            scrub_urls("ws://a.b/x and http://c.d, done"),
            "<url> and <url>, done"
        );
        assert_eq!(scrub_urls("no links"), "no links");
    }

    fn feed(required: bool) -> FeedHealth {
        FeedHealth::new("hl_ctx", required, 60, T0)
    }

    #[test]
    fn feed_transitions() {
        let mut f = feed(true);
        assert_eq!((f.state, f.row_at_ms()), (FeedState::Connecting, None));
        f.on_item(T0 + 1_000);
        assert_eq!((f.state, f.items), (FeedState::Live, 1));
        assert_eq!(f.row_at_ms(), Some(T0 + 1_000));
        f.on_error(
            ErrorClass::RateLimited,
            "HTTP 429 from https://api.hyperliquid.xyz/info",
            true,
            T0 + 2_000,
        );
        assert_eq!(f.state, FeedState::Backoff);
        assert_eq!(
            f.last_error.as_deref(),
            Some("HTTP 429 from <url>"),
            "no URL in health rows"
        );
        f.on_reconnect(T0 + 3_000);
        assert_eq!((f.state, f.reconnects), (FeedState::Connecting, 1));
        f.on_item(T0 + 4_000);
        f.on_dropped(3, T0 + 5_000);
        assert_eq!(
            (f.state, f.dropped, f.last_item_age_s),
            (FeedState::Live, 3, Some(1.0))
        );
        // Quiet past stale_after_s ⇒ stalled; an item brings it back.
        f.refresh(T0 + 64_001);
        assert_eq!(f.state, FeedState::Stalled);
        f.on_item(T0 + 65_000);
        assert_eq!(f.state, FeedState::Live);
        f.on_error(ErrorClass::AuthRequired, "403", false, T0 + 66_000);
        assert_eq!(f.state, FeedState::Down);
        f.refresh(T0 + 999_000);
        assert_eq!(f.state, FeedState::Down, "down stays down");
    }

    #[test]
    fn health_rows_have_full_keys_and_valid_features() {
        let mut f = feed(true);
        f.on_item(T0);
        f.on_error(ErrorClass::Timeout, "slow", true, T0 + 500);
        let o = Observation::of("runtime", &f, T0, 15_000, ObsSource::Live);
        assert_eq!(o.key, "feed/1:hl_ctx");
        assert_features_ok(&o.features);
        assert_eq!(o.features["state"], serde_json::json!("backoff"));
        assert_eq!(o.features["last_error_class"], serde_json::json!("timeout"));
        assert!(
            o.headline
                .starts_with("feed hl_ctx backoff required items=1"),
            "{}",
            o.headline
        );
        assert_eq!(o.typed::<FeedHealth>().unwrap(), f);

        let l = loop_health("xm_main", Some(T0 - 14_000));
        let o = Observation::of("runtime", &l, T0, 15_000, ObsSource::Live);
        assert_eq!(o.key, "loop/1:xm_main");
        assert_features_ok(&o.features);
        assert_eq!(o.features["last_decision_age_s"], serde_json::json!(14.0));
        assert!(o.headline.chars().count() <= MAX_LINE1_CHARS);
        assert!(o.headline.ends_with("last_decision=14s"), "{}", o.headline);
        let never = loop_health("xm_main", None);
        assert!(!never.features().contains_key("last_decision_age_s"));
    }

    fn loop_health(name: &str, last: Option<i64>) -> LoopHealth {
        LoopHealth {
            name: name.into(),
            queue_depth: 0,
            in_flight: 1,
            accepted: 13,
            completed: 11,
            failed: 1,
            dropped: 0,
            last_event_at_ms: Some(T0 - 1_000),
            last_decision_at_ms: last,
            last_decision_age_s: last.map(|t| secs(T0 - t)),
            last_error: Some("jev 503".into()),
            updated_at_ms: T0,
        }
    }

    fn beat(state: RunState, ts_ms: i64, feeds: Vec<FeedHealth>) -> HeartbeatRead {
        HeartbeatRead::Found(Heartbeat {
            sandbox: "xmarket".into(),
            pid: 4242,
            holder: HOLDER.into(),
            state,
            stop_reason: (state != RunState::Running).then(|| "SIGTERM".into()),
            started_at_ms: T0 - 3 * 3_600_000,
            ts_ms,
            heartbeat_secs: 5,
            loops: BTreeMap::from([("xm_main".into(), loop_health("xm_main", Some(T0 - 14_000)))]),
            feeds: feeds.into_iter().map(|f| (f.name.clone(), f)).collect(),
        })
    }

    /// A feed as the heartbeat at `T0` would carry it.
    fn feed_at(required: bool, state: FeedState, last_item_ago_ms: Option<i64>) -> FeedHealth {
        let mut f = FeedHealth::new("hl_ctx", required, 60, T0 - 600_000);
        f.last_item_at_ms = last_item_ago_ms.map(|a| T0 - a);
        f.items = u64::from(last_item_ago_ms.is_some());
        f.state = state;
        if state == FeedState::Down {
            f.last_error_class = Some(ErrorClass::AuthRequired);
            f.last_error = Some("blocked (geo/WAF/Tor exit?)".into());
        }
        f.refresh(T0);
        f.state = state;
        f
    }

    const KNOBS: LiveKnobs = LiveKnobs {
        heartbeat_stale_secs: 30,
    };

    /// The `feed/1` row of `f` as a store would return it.
    fn row_of(f: &FeedHealth) -> Observation {
        Observation::of("runtime", f, T0, 15_000, ObsSource::Live)
    }

    #[test]
    fn live_verdict_table() {
        use FeedState::*;
        use RunState::*;
        let fresh = T0 - 2_000;
        // (case, heartbeat, rows, live?, text every failing line must hold)
        let down_row = feed_at(true, Down, Some(1_000));
        let mut older_live = feed_at(true, Live, Some(1_000));
        older_live.updated_at_ms = T0 - 5_000;
        let mut undecodable = row_of(&down_row);
        undecodable.data = serde_json::json!({"name": 7});
        let mut quiet_since_start = FeedHealth::new("hl_ctx", true, 60, T0 - 90_000);
        quiet_since_start.refresh(T0);
        let mut young = FeedHealth::new("hl_ctx", true, 60, T0 - 10_000);
        young.refresh(T0);
        let mut not_updated = feed_at(true, Live, Some(1_000));
        not_updated.updated_at_ms = T0 - 120_000;
        not_updated.last_item_at_ms = Some(T0 - 1_000);
        let cases: Vec<(&str, HeartbeatRead, Vec<Observation>, bool, &str)> = vec![
            (
                "no heartbeat",
                HeartbeatRead::Missing,
                vec![],
                false,
                "`tengu run --sandbox xmarket`",
            ),
            (
                "unreadable",
                HeartbeatRead::Unreadable("EOF".into()),
                vec![],
                false,
                "run-xmarket.json unreadable: EOF",
            ),
            ("fresh", beat(Running, fresh, vec![]), vec![], true, ""),
            (
                "30 s is still fresh",
                beat(Running, T0 - 30_000, vec![]),
                vec![],
                true,
                "",
            ),
            (
                "stale",
                beat(Running, T0 - 31_000, vec![]),
                vec![],
                false,
                "stale: last beat 31 s ago (limit 30 s)",
            ),
            (
                "stopped",
                beat(Stopped, fresh, vec![]),
                vec![],
                false,
                "stopped 2 s ago: SIGTERM",
            ),
            (
                "stopping",
                beat(Stopping, fresh, vec![]),
                vec![],
                false,
                "stopping",
            ),
            (
                "required live",
                beat(Running, fresh, vec![feed_at(true, Live, Some(1_000))]),
                vec![],
                true,
                "",
            ),
            (
                "required down",
                beat(Running, fresh, vec![feed_at(true, Down, Some(1_000))]),
                vec![],
                false,
                "down: down · items 1 · last item 1 s ago",
            ),
            (
                "required stale",
                beat(Running, fresh, vec![feed_at(true, Live, Some(61_000))]),
                vec![],
                false,
                "stale: live · items 1 · last item 61 s ago (stale after 60 s)",
            ),
            (
                "optional down",
                beat(Running, fresh, vec![feed_at(false, Down, Some(1_000))]),
                vec![],
                true,
                "",
            ),
            (
                "optional stale",
                beat(Running, fresh, vec![feed_at(false, Stalled, Some(900_000))]),
                vec![],
                true,
                "",
            ),
            (
                "newer row wins",
                beat(Running, fresh, vec![older_live.clone()]),
                vec![row_of(&down_row)],
                false,
                "down: down",
            ),
            (
                "older row loses",
                beat(Running, fresh, vec![feed_at(true, Live, Some(1_000))]),
                vec![row_of(&older_live)],
                true,
                "",
            ),
            (
                "no item yet, young",
                beat(Running, fresh, vec![young]),
                vec![],
                true,
                "",
            ),
            (
                "no item since start",
                beat(Running, fresh, vec![quiet_since_start]),
                vec![],
                false,
                "stale: connecting · items 0 · no item in 90 s since start",
            ),
            (
                "not updated",
                beat(Running, fresh, vec![not_updated]),
                vec![],
                false,
                "not updated for 2 min",
            ),
            (
                "row does not decode",
                beat(Running, fresh, vec![]),
                vec![undecodable],
                false,
                "feed/1:hl_ctx data",
            ),
            (
                "rows without a heartbeat",
                HeartbeatRead::Missing,
                vec![row_of(&feed_at(true, Live, Some(1_000)))],
                false,
                "missing",
            ),
        ];
        for (case, hb, rows, live, needle) in cases {
            let r = live_verdict("xmarket", &hb, &rows, T0, KNOBS);
            assert_eq!(r.ok(), live, "{case}: {r:#?}");
            for c in r.checks.iter().filter(|c| !c.ok) {
                assert!(
                    c.detail.contains(needle),
                    "{case}: `{}` lacks `{needle}`",
                    c.detail
                );
            }
        }
    }

    #[test]
    fn live_report_lines_are_readable_with_full_ids() {
        let hb = beat(
            RunState::Running,
            T0 - 2_000,
            vec![feed_at(true, FeedState::Down, Some(1_000))],
        );
        let r = live_verdict("xmarket", &hb, &[], T0, KNOBS);
        let lines: Vec<String> = r
            .checks
            .iter()
            .map(|c| {
                format!(
                    "{} {} {}",
                    if c.ok { "ok" } else { "FAIL" },
                    c.subject,
                    c.detail
                )
            })
            .collect();
        assert_eq!(
            lines[0],
            format!("ok heartbeat running · up 3 h 0 min · last beat 2 s ago (limit 30 s) · pid 4242 · holder {HOLDER}")
        );
        assert_eq!(
            lines[1],
            "ok loop xm_main in_flight 1 · queue 0 · done 11 · failed 1 · dropped 0 · last decision 14 s ago · last error: jev 503"
        );
        assert_eq!(
            lines[2],
            "FAIL feed hl_ctx (required) down: down · items 1 · last item 1 s ago (stale after 60 s) · reconnects 0 · dropped 0 · last error auth_required: blocked (geo/WAF/Tor exit?)"
        );
    }
}
