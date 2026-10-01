//! `[xmarket]` — sandbox-wide settings of the xmarket runtime: the
//! install-wide state directory (tracker convention 3), the session
//! calendars `[xmarket.calendars.<id>]` (convention 18, evaluated by
//! `domain/calendar.rs`) and the weekend fade `[xmarket.weekend_fade]`
//! ([`WeekendFadeConfig`], its own load rules). Later milestones add venues
//! and lifecycle knobs. `deny_unknown_fields`. Operator doc:
//! `docs/runtime-2026-09-30.md` § State layout.
//!
//! State layout — one state dir per sandbox, `<TENGU_HOME>/state/<state>`,
//! shared by every process of the install; `tengu prune` never deletes it.
//! Paths: [`ledger_db`], [`runtime_db`], [`history_dir`].
//!
//! | Path | Holds | Opened by |
//! |---|---|---|
//! | `<state dir>/ledger.db` | paper ledger + risk verdicts | `outbound/paper_store.rs` |
//! | `<state dir>/runtime.db` · `run-<sandbox>.json` | `tengu run` lease · heartbeat | `outbound/runtime_store.rs` |
//! | `<state dir>/history/<YYYYMMDD>.db` | recorder day files (UTC) | `outbound/history_sqlite.rs` |
//! | `<state dir>/{catalog,events,audit,spend}.db` | reserved: `kg-catalog-store` (M1), `info-store` (M4), `ops-audit-store` (M5), `ops-cost-guard` (M4) | — |
//! | `<workspace>/.tengu/observations.db` | the workspace's only xmarket file: hot rows (`world`, `requires`) | `outbound/observations.rs` |
//!
//! | Load rule (sandbox with `[feeds]`, `[risk]` or `[xmarket]`; any violation fails `Config::load`) | Why |
//! |---|---|
//! | `[risk]` needs `[xmarket]` | the ledger lives in the state dir |
//! | Every agent named by `[feeds.*].agent` or `[decision_loops.*].agent`, or holding an xmarket tool (`domain::tools::XM_TOOLS`), sets the same `workspace`, absolute or `~/…` | a loop reads `world` rows from its agent's `<workspace>/.tengu/observations.db`, where feeds and exec tools write them — one store for every process |
//! | With `[risk]`: every agent sets `workspace`, absolute or `~/…` | without one an agent works in the process cwd (the Docker image's `/opt/tengu` holds `TENGU_HOME`), which no rule can keep off the state dir |
//! | The state dir outside every `fs_roots` and agent `workspace`, no overlap either way (symlinks resolved); a hardened sandbox checks all of `<TENGU_HOME>/state` instead (`config/hardening.rs`) | `read_file` / `write_file` must not reach the stores |
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

use super::hardening;
use super::paths::{expand_tilde, resolve_tengu_home};
use super::risk::HL_MIN_ORDER_USD;
use super::Config;
use crate::domain::calendar::{
    parse_date, parse_hm, parse_week_hm, Calendar, ExchangeCalendar, WeeklyWindow,
};
use crate::domain::market::MarketCtx;
use crate::domain::observation::Observed;
use crate::domain::tools::{XM_TOOLS, XM_WEEKEND_FADE};
use crate::domain::tz::Zone;
use crate::domain::xm::risk::permission_error;
use crate::domain::xm::weekend_fade::{FadeRule, FADE_STRATEGY};

/// The rows the weekend fade's anchor price comes from.
const MKT_CTX_SCHEMA: &str = <MarketCtx as Observed>::SCHEMA;

/// The paper ledger (`outbound/paper_store.rs`).
pub(crate) const LEDGER_DB: &str = "ledger.db";
/// The `tengu run` lease (`outbound/runtime_store.rs`).
pub(crate) const RUNTIME_DB: &str = "runtime.db";
/// The recorder's `<YYYYMMDD>.db` day files (`outbound/history_sqlite.rs`).
pub(crate) const HISTORY_DIR: &str = "history";
/// Reserved: instruments, edges, lifecycle, approvals (`kg-catalog-store`).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const CATALOG_DB: &str = "catalog.db";
/// Reserved: the event store (`info-store`).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const EVENTS_DB: &str = "events.db";
/// Reserved: decision audit v2 (`ops-audit-store`).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const AUDIT_DB: &str = "audit.db";
/// Reserved: `[spend]` caps (`ops-cost-guard`).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const SPEND_DB: &str = "spend.db";

/// `<state_dir>/ledger.db`.
pub(crate) fn ledger_db(state_dir: &Path) -> PathBuf {
    state_dir.join(LEDGER_DB)
}

/// `<state_dir>/runtime.db` — `state_dir` as `config::runtime::state_dir`
/// picks it (the xmarket state dir, else `<TENGU_HOME>/state`).
pub(crate) fn runtime_db(state_dir: &Path) -> PathBuf {
    state_dir.join(RUNTIME_DB)
}

/// `<state_dir>/history`.
pub(crate) fn history_dir(state_dir: &Path) -> PathBuf {
    state_dir.join(HISTORY_DIR)
}

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
    /// `[xmarket.weekend_fade]` — rule W for the exec tool
    /// `xm_weekend_fade` ([`WeekendFadeConfig`]).
    #[serde(default)]
    pub weekend_fade: Option<WeekendFadeConfig>,
}

impl Default for XmarketConfig {
    fn default() -> Self {
        Self {
            state: default_state(),
            calendars: BTreeMap::new(),
            weekend_fade: None,
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

    /// `<tengu_home>/state/<state>/history` — the recorder's day files.
    pub fn history_dir(&self, tengu_home: &Path) -> PathBuf {
        history_dir(&self.state_dir(tengu_home))
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
        if let Some(fade) = &self.weekend_fade {
            errors.extend(fade.validation_errors(self));
        }
        errors
    }
}

/// `[xmarket.weekend_fade]` — the weekend-fade rule W, run by the exec tool
/// `xm_weekend_fade` (`domain/xm/weekend_fade.rs`,
/// `docs/xmarket-risk-paper-2026-09-30.md` § Weekend fade). Every key is
/// required, unknown keys are refused.
///
/// | Load rule | Why |
/// |---|---|
/// | `calendar` names an `exchange` `[xmarket.calendars.<id>]` | the weekend clock needs trading days |
/// | `universe` non-empty, unique `hyperliquid:<coin>` ids in full; `exclude` ⊆ `universe` | the paper engine fills Hyperliquid perps only; a short id never matches a row |
/// | `capped_top_n` ≤ the names not excluded; `min_abs_signal_bps` ≥ 0 | the capped selection |
/// | `capped_notional_usd` ≥ $10 (HL minimum), ≤ `[risk] max_order_notional_usd`; `capped_top_n` × it ≤ `[risk] max_gross_exposure_usd` | a fade the gate always denies is a typo |
/// | `shadow_account` a `[A-Za-z0-9._-]` name, not the `[risk] account`; `shadow_initial_cash_usd` > 0; `shadow_notional_usd` ≥ $10 | the shadow ledger is its own account |
/// | `expected_edge_bps` ≥ `[risk] min_edge_bps` | else every capped fade is denied `min_edge` |
/// | `anchor_max_age_secs`, `entry_max_age_secs` > 0; 0 < `entry_lateness_max_secs` < 54 000 (15 h: before any exit); 0 < `max_slippage_bps` < 10 000 | — |
/// | `[risk]` + `[paper]` present | an exec tool; the capped ledger is the `[risk]` account |
/// | `[recorder]` records `mkt_ctx/1`; `anchor_max_age_secs` ≥ its `heartbeat_secs` (with `change_only`) and ≥ its `min_interval_secs` for `mkt_ctx/1` | the anchor price comes from the history |
/// | every universe name not excluded permitted by `[risk]` (`instruments_allow`, not `instruments_deny`) | the capped ledger is rule W exactly; leave a name out with `exclude` |
/// | `[risk] require_hedge_for` does not list `overreaction` | the fades carry that strategy and have no hedge leg |
/// | an agent holding `xm_weekend_fade` ⇒ the section exists | the tool would refuse every call |
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WeekendFadeConfig {
    /// Id of an `exchange` `[xmarket.calendars.<id>]` — the weekend clock
    /// (NYSE: `us_equity`).
    pub calendar: String,
    /// Names faded, full ids `hyperliquid:<coin>` (`hyperliquid:xyz:TSLA`).
    pub universe: Vec<String>,
    /// Universe ids neither ledger trades (a split, a halt).
    pub exclude: Vec<String>,
    /// Capped ledger: this many names with the largest |s| …
    pub capped_top_n: usize,
    /// … and |s| at least this, bps.
    pub min_abs_signal_bps: f64,
    /// USD per capped fade, through the `[risk]` gate.
    pub capped_notional_usd: f64,
    /// The shadow ledger's account (every eligible name, no caps).
    pub shadow_account: String,
    /// Its cash when it opens.
    pub shadow_initial_cash_usd: f64,
    /// USD per shadow fade.
    pub shadow_notional_usd: f64,
    /// `edge_after_costs_bps` of the per-name opportunity row a capped fade
    /// names (the gate's `min_edge_bps`). Example 23: half the in-sample
    /// +46 bps net per trade (the feasibility study cut every edge in half).
    pub expected_edge_bps: f64,
    /// Oldest recorded `mkt_ctx/1` row the anchor price may come from.
    pub anchor_max_age_secs: u64,
    /// Oldest stored `mkt_ctx/1` row the entry price may come from.
    pub entry_max_age_secs: u64,
    /// A first call later than this after the entry instant enters nothing
    /// (`missed_entry`); failed fades are placed again until then.
    pub entry_lateness_max_secs: u64,
    /// IOC bound of every fade order (entries and shadow exits) vs the book
    /// mid, bps.
    pub max_slippage_bps: f64,
}

/// Longest `entry_lateness_max_secs`: every window's exit is at least 15 h
/// after its entry (Sun 18:00 → Mon 09:00).
const MAX_LATENESS_SECS: u64 = 15 * 3600;

impl WeekendFadeConfig {
    /// The selection knobs.
    pub fn rule(&self) -> FadeRule {
        FadeRule {
            capped_top_n: self.capped_top_n,
            min_abs_signal_bps: self.min_abs_signal_bps,
        }
    }

    /// The section's own rules (the module-table rows that need only
    /// `[xmarket]`).
    fn validation_errors(&self, x: &XmarketConfig) -> Vec<String> {
        let mut errors = Vec::new();
        let mut err = |e: String| errors.push(format!("xmarket.weekend_fade: {e}"));
        match x.calendars.get(&self.calendar) {
            None => err(format!(
                "calendar `{}` is not an [xmarket.calendars.<id>] row",
                self.calendar
            )),
            Some(c) if c.kind != CalendarKind::Exchange => err(format!(
                "calendar `{}` must be kind = \"exchange\" (the weekend clock needs trading days)",
                self.calendar
            )),
            Some(_) => {}
        }
        if self.universe.is_empty() {
            err("universe is empty".into());
        }
        let mut seen = BTreeSet::new();
        for id in &self.universe {
            let full = id
                .strip_prefix("hyperliquid:")
                .is_some_and(|coin| !coin.is_empty() && !coin.contains(char::is_whitespace));
            if !full {
                err(format!(
                    "universe id `{id}` is not a full `hyperliquid:<coin>` id (e.g. hyperliquid:xyz:TSLA)"
                ));
            }
            if !seen.insert(id.as_str()) {
                err(format!("universe lists `{id}` twice"));
            }
        }
        let mut excluded = BTreeSet::new();
        for id in &self.exclude {
            if !seen.contains(id.as_str()) {
                err(format!("exclude id `{id}` is not in universe"));
            }
            if !excluded.insert(id.as_str()) {
                err(format!("exclude lists `{id}` twice"));
            }
        }
        let tradable = seen.iter().filter(|id| !excluded.contains(*id)).count();
        if self.capped_top_n > tradable {
            err(format!(
                "capped_top_n {} exceeds the {tradable} universe names not excluded",
                self.capped_top_n
            ));
        }
        let finite = |v: f64| v.is_finite();
        if !(finite(self.min_abs_signal_bps) && self.min_abs_signal_bps >= 0.0) {
            err(format!(
                "min_abs_signal_bps must be ≥ 0, got {}",
                self.min_abs_signal_bps
            ));
        }
        for (key, v) in [
            ("capped_notional_usd", self.capped_notional_usd),
            ("shadow_notional_usd", self.shadow_notional_usd),
        ] {
            if !(finite(v) && v >= HL_MIN_ORDER_USD) {
                err(format!(
                    "{key} must be ≥ {HL_MIN_ORDER_USD} (Hyperliquid's minimum order), got {v}"
                ));
            }
        }
        let a = self.shadow_account.as_str();
        if a.is_empty()
            || !a
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            err(format!(
                "shadow_account `{a}` must be a [A-Za-z0-9._-] ledger account"
            ));
        }
        if !(finite(self.shadow_initial_cash_usd) && self.shadow_initial_cash_usd > 0.0) {
            err(format!(
                "shadow_initial_cash_usd must be > 0, got {}",
                self.shadow_initial_cash_usd
            ));
        }
        if !finite(self.expected_edge_bps) {
            err(format!(
                "expected_edge_bps must be a number, got {}",
                self.expected_edge_bps
            ));
        }
        for (key, v) in [
            ("anchor_max_age_secs", self.anchor_max_age_secs),
            ("entry_max_age_secs", self.entry_max_age_secs),
        ] {
            if v == 0 {
                err(format!("{key} must be > 0"));
            }
        }
        if !(1..MAX_LATENESS_SECS).contains(&self.entry_lateness_max_secs) {
            err(format!(
                "entry_lateness_max_secs must be > 0 and < {MAX_LATENESS_SECS} (15 h: before any \
                 exit), got {}",
                self.entry_lateness_max_secs
            ));
        }
        if !(finite(self.max_slippage_bps)
            && self.max_slippage_bps > 0.0
            && self.max_slippage_bps < 10_000.0)
        {
            err(format!(
                "max_slippage_bps must be > 0 and < 10000, got {}",
                self.max_slippage_bps
            ));
        }
        errors
    }
}

/// `[xmarket.weekend_fade]` against the other sections (the struct's
/// module-table rows that need `[risk]`, `[recorder]` or the agents).
fn weekend_fade_errors(cfg: &Config, errors: &mut Vec<String>) {
    let fade = cfg.xmarket.as_ref().and_then(|x| x.weekend_fade.as_ref());
    let Some(fade) = fade else {
        let holders: Vec<&str> = cfg
            .agents
            .iter()
            .filter(|(_, a)| {
                [&a.tools, &a.workspace_tools]
                    .iter()
                    .any(|list| list.iter().any(|t| t == XM_WEEKEND_FADE))
            })
            .map(|(id, _)| id.as_str())
            .collect();
        for id in holders {
            errors.push(format!(
                "agents.{id} holds {XM_WEEKEND_FADE}, but the sandbox has no \
                 [xmarket.weekend_fade] — the tool would refuse every call"
            ));
        }
        return;
    };
    let mut err = |e: String| errors.push(format!("xmarket.weekend_fade: {e}"));
    let (Some(risk), Some(_)) = (&cfg.risk, &cfg.paper) else {
        err(
            "needs [risk] + [paper]: xm_weekend_fade is an exec tool and the capped ledger is \
             the [risk] account"
                .into(),
        );
        return;
    };
    let limits = risk.limits();
    if fade.shadow_account == risk.account {
        err(format!(
            "shadow_account `{}` is the [risk] account — the shadow ledger needs its own",
            fade.shadow_account
        ));
    }
    if fade.capped_notional_usd > risk.max_order_notional_usd {
        err(format!(
            "capped_notional_usd {} exceeds [risk] max_order_notional_usd {}",
            fade.capped_notional_usd, risk.max_order_notional_usd
        ));
    }
    let capped_total = fade.capped_top_n as f64 * fade.capped_notional_usd;
    if capped_total > risk.max_gross_exposure_usd {
        err(format!(
            "capped_top_n × capped_notional_usd = {capped_total} exceeds [risk] \
             max_gross_exposure_usd {}",
            risk.max_gross_exposure_usd
        ));
    }
    if fade.expected_edge_bps < risk.min_edge_bps {
        err(format!(
            "expected_edge_bps {} is below [risk] min_edge_bps {}: every capped fade would be \
             denied min_edge",
            fade.expected_edge_bps, risk.min_edge_bps
        ));
    }
    if risk.require_hedge_for.iter().any(|s| s == FADE_STRATEGY) {
        err(format!(
            "[risk] require_hedge_for lists `{FADE_STRATEGY}`, the fades' strategy — they have \
             no hedge leg"
        ));
    }
    let blocked: Vec<String> = fade
        .universe
        .iter()
        .filter(|id| !fade.exclude.contains(id))
        .filter_map(|id| permission_error(&limits, id))
        .collect();
    if !blocked.is_empty() {
        err(format!(
            "every universe name not excluded must be tradable under [risk] (the capped ledger \
             is rule W exactly; leave a name out with `exclude`): {}",
            blocked.join("; ")
        ));
    }
    let rec = &cfg.recorder;
    if !rec.records(MKT_CTX_SCHEMA) {
        err(format!(
            "the anchor price comes from the history: [recorder] enabled = true with \
             `{MKT_CTX_SCHEMA}` in schemas"
        ));
    } else {
        let mut floor = rec.min_interval_ms(MKT_CTX_SCHEMA) / 1000;
        if rec.change_only {
            floor = floor.max(rec.heartbeat_secs);
        }
        if fade.anchor_max_age_secs < floor {
            err(format!(
                "anchor_max_age_secs {} is below {floor} s, the longest the recorder may go \
                 without a {MKT_CTX_SCHEMA} row ([recorder] heartbeat_secs / min_interval_secs)",
                fade.anchor_max_age_secs
            ));
        }
    }
}

/// `[xmarket]` itself, then the load rules of an xmarket sandbox — one with
/// `[feeds]`, `[risk]` or `[xmarket]` (module table).
pub(crate) fn validation_errors(cfg: &Config) -> Vec<String> {
    rules_at(cfg, &resolve_tengu_home())
}

/// [`validation_errors`] with the state dir under `tengu_home`.
fn rules_at(cfg: &Config, tengu_home: &Path) -> Vec<String> {
    let mut errors = cfg
        .xmarket
        .as_ref()
        .map(XmarketConfig::validation_errors)
        .unwrap_or_default();
    if cfg.feeds.is_empty() && cfg.risk.is_none() && cfg.xmarket.is_none() {
        return errors;
    }
    if cfg.risk.is_some() && cfg.xmarket.is_none() {
        errors.push(
            "[risk] needs [xmarket]: the paper ledger lives in \
             <TENGU_HOME>/state/<xmarket.state>/ledger.db — add [xmarket] (state = \"<name>\")"
                .into(),
        );
    }
    workspace_errors(cfg, &mut errors);
    weekend_fade_errors(cfg, &mut errors);
    // A hardened sandbox keeps all of `<TENGU_HOME>/state` out of reach
    // (`config/hardening.rs`); an invalid `state` name is reported above.
    match &cfg.xmarket {
        Some(x)
            if !hardening::requires_hardened_claude_code(cfg)
                && x.validation_errors().is_empty() =>
        {
            errors.extend(hardening::dir_reach_errors(
                cfg,
                "the [xmarket] state dir",
                &x.state_dir(tengu_home),
                "xmarket: no tool may reach the install-wide stores",
            ));
        }
        _ => {}
    }
    errors
}

/// Agents that share the xmarket workspace, each with the keys that make it
/// one: `feeds.<n>.agent`, `decision_loops.<n>.agent`, `holds <xm tools>`.
fn members(cfg: &Config) -> BTreeMap<&str, Vec<String>> {
    let mut out: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for (name, feed) in &cfg.feeds {
        if let Some(agent) = feed.agent.as_deref() {
            out.entry(agent.trim())
                .or_default()
                .push(format!("feeds.{name}.agent"));
        }
    }
    let mut loops: Vec<_> = cfg.decision_loops.iter().collect();
    loops.sort_by(|a, b| a.0.cmp(b.0));
    for (name, dl) in loops {
        out.entry(dl.agent.as_str())
            .or_default()
            .push(format!("decision_loops.{name}.agent"));
    }
    for (id, agent) in &cfg.agents {
        let held: Vec<&str> = XM_TOOLS
            .iter()
            .copied()
            .filter(|t| {
                agent.tools.iter().any(|x| x == t) || agent.workspace_tools.iter().any(|x| x == t)
            })
            .collect();
        if !held.is_empty() {
            out.entry(id.as_str())
                .or_default()
                .push(format!("holds {}", held.join(", ")));
        }
    }
    out
}

/// The shared workspace of the [`members`]; with `[risk]` an explicit
/// workspace on every agent.
fn workspace_errors(cfg: &Config, errors: &mut Vec<String>) {
    let members = members(cfg);
    let mut agents: Vec<_> = cfg.agents.iter().collect();
    agents.sort_by(|a, b| a.0.cmp(b.0));
    let mut shared: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();
    for (id, agent) in agents {
        let why = members.get(id.as_str()).map(|keys| keys.join(", "));
        if why.is_none() && cfg.risk.is_none() {
            continue;
        }
        let Some(ws) = &agent.workspace else {
            errors.push(match &why {
                Some(why) => format!(
                    "agents.{id} ({why}) has no `workspace` — the agents of feeds, decision \
                     loops and xmarket tools share one: a loop reads `world` rows from its \
                     agent's <workspace>/.tengu/observations.db, where feeds and exec tools \
                     write them (without one the agent works in the process cwd)"
                ),
                None => format!(
                    "agents.{id} has no `workspace` — in a [risk] sandbox every agent sets \
                     one: without it the agent works in the process cwd (the Docker image's \
                     /opt/tengu holds TENGU_HOME), which no load rule can keep away from \
                     <TENGU_HOME>/state"
                ),
            });
            continue;
        };
        let expanded = expand_tilde(ws);
        if !expanded.is_absolute() {
            errors.push(format!(
                "agents.{id}.workspace `{}` must be absolute or start with `~/` — tengu run, \
                 run-agent children and the bridge must all resolve it to one directory",
                ws.display()
            ));
            continue;
        }
        if let Some(why) = why {
            shared
                .entry(hardening::resolved(&expanded))
                .or_default()
                .push(format!("agents.{id} ({why})"));
        }
    }
    if shared.len() > 1 {
        let found: Vec<String> = shared
            .iter()
            .map(|(ws, ids)| format!("`{}`: {}", ws.display(), ids.join(", ")))
            .collect();
        errors.push(format!(
            "the agents of feeds, decision loops and xmarket tools must share one `workspace` \
             (a loop reads `world` rows from its agent's .tengu/observations.db; feeds and exec \
             tools write them there) — found {}: {}",
            shared.len(),
            found.join(" · ")
        ));
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
        assert_eq!(
            x.history_dir(Path::new("/h")),
            PathBuf::from("/h/state/xmarket/history")
        );
        assert!(x.validation_errors().is_empty());
        assert!(x.calendars().is_empty());
    }

    /// One layout: every store under the state dir, names distinct.
    #[test]
    fn state_layout_paths() {
        let dir = Path::new("/h/state/xmarket-weekend");
        assert_eq!(
            ledger_db(dir),
            PathBuf::from("/h/state/xmarket-weekend/ledger.db")
        );
        assert_eq!(
            runtime_db(dir),
            PathBuf::from("/h/state/xmarket-weekend/runtime.db")
        );
        assert_eq!(
            history_dir(dir),
            PathBuf::from("/h/state/xmarket-weekend/history")
        );
        let names = [
            LEDGER_DB,
            RUNTIME_DB,
            HISTORY_DIR,
            CATALOG_DB,
            EVENTS_DB,
            AUDIT_DB,
            SPEND_DB,
        ];
        assert_eq!(names.iter().collect::<BTreeSet<_>>().len(), names.len());
    }

    /// `<TENGU_HOME>` of the rule tests.
    const HOME: &str = "/srv/tengu-home";

    /// Chat agent (no workspace) + the xmarket agents sharing `/srv/xm-ws`:
    /// a loop agent, a feed agent holding `hl_ctx` (the feed: [`FEED`]), an
    /// exec agent opting into `paper_order`.
    const AGENTS: &str = r#"
[agents.chat]
default = true
engine = "openrouter"
model = "m"

[agents.xm_feeds]
engine = "openrouter"
model = "m"
workspace = "/srv/xm-ws"
tools = ["hl_ctx"]

[agents.xm_loop]
engine = "openrouter"
model = "m"
workspace = "/srv/xm-ws"

[agents.xm_exec]
engine = "openrouter"
model = "m"
workspace = "/srv/xm-ws"
workspace_tools = ["paper_order"]

[decision_loops.xm_main]
goal = "g"
agent = "xm_loop"
[decision_loops.xm_main.actions.hold]
description = "Nothing to do"
"#;

    const FEED: &str = r#"
[feeds.hl_ctx]
kind = "tool"
agent = "xm_feeds"
tool = "hl_ctx"
every_secs = 60
"#;

    const XM: &str = "[xmarket]\nstate = \"xm-test\"\n";

    /// [`AGENTS`] with `agents.<agent>`'s workspace line replaced (`None` =
    /// removed).
    fn ws(agent: &str, to: Option<&str>) -> String {
        let block = format!("[agents.{agent}]\nengine = \"openrouter\"\nmodel = \"m\"\n");
        let old = format!("{block}workspace = \"/srv/xm-ws\"\n");
        assert!(AGENTS.contains(&old), "{agent}");
        let new = match to {
            Some(w) => format!("{block}workspace = \"{w}\"\n"),
            None => block,
        };
        AGENTS.replacen(&old, &new, 1)
    }

    /// The xmarket rules of a whole sandbox, `<TENGU_HOME>` = [`HOME`].
    fn rules(toml: &str) -> Vec<String> {
        let cfg: Config = toml::from_str(toml).unwrap_or_else(|e| panic!("{e}\n{toml}"));
        rules_at(&cfg, Path::new(HOME))
    }

    /// Every rule, table-driven: each error matches a `want` substring and
    /// each `want` an error (no `want` = the sandbox passes).
    #[test]
    fn load_rules() {
        use crate::config::risk::tests::RISK_100;
        let user_ws = dirs_next::home_dir().unwrap().join("xm-ws");
        let user_ws = user_ws.display().to_string();
        let all_at = |w: &str| {
            AGENTS.replace(
                "workspace = \"/srv/xm-ws\"",
                &format!("workspace = \"{w}\""),
            )
        };
        let chat_ws = AGENTS.replacen(
            "model = \"m\"\n",
            "model = \"m\"\nworkspace = \"/srv/chat-ws\"\n",
            1,
        );
        let cases: Vec<(&str, String, Vec<&str>)> = vec![
            ("shared workspace", format!("{XM}{AGENTS}{FEED}"), vec![]),
            ("[feeds] alone", format!("{AGENTS}{FEED}"), vec![]),
            (
                "neither [feeds], [risk] nor [xmarket]: not checked",
                ws("xm_loop", Some("/srv/other")),
                vec![],
            ),
            (
                "a feed agent without workspace",
                format!("{XM}{}{FEED}", ws("xm_feeds", None)),
                vec!["agents.xm_feeds (feeds.hl_ctx.agent, holds hl_ctx) has no `workspace` — the agents of feeds, decision loops and xmarket tools share one"],
            ),
            (
                "a loop agent elsewhere",
                format!("{XM}{}{FEED}", ws("xm_loop", Some("/srv/other"))),
                vec!["must share one `workspace` (a loop reads `world` rows from its agent's .tengu/observations.db; feeds and exec tools write them there) — found 2: `/srv/other`: agents.xm_loop (decision_loops.xm_main.agent) · `/srv/xm-ws`: agents.xm_exec (holds paper_order), agents.xm_feeds (feeds.hl_ctx.agent, holds hl_ctx)"],
            ),
            (
                "an exec agent on a relative workspace",
                format!("{XM}{}", ws("xm_exec", Some("xm-ws"))),
                vec!["agents.xm_exec.workspace `xm-ws` must be absolute or start with `~/`"],
            ),
            (
                "`~/` is its absolute spelling",
                format!(
                    "{XM}{}{FEED}",
                    all_at(&user_ws).replacen(
                        &format!("workspace = \"{user_ws}\""),
                        "workspace = \"~/xm-ws\"",
                        1
                    )
                ),
                vec![],
            ),
            (
                "[risk] without [xmarket], a chat agent without workspace",
                format!("{RISK_100}{AGENTS}{FEED}"),
                vec![
                    "[risk] needs [xmarket]: the paper ledger lives in <TENGU_HOME>/state/<xmarket.state>/ledger.db",
                    "agents.chat has no `workspace` — in a [risk] sandbox every agent sets one",
                ],
            ),
            (
                "[risk] + [xmarket], every agent a workspace (the chat agent its own)",
                format!("{XM}{RISK_100}{chat_ws}{FEED}"),
                vec![],
            ),
            (
                "the state dir inside the workspace",
                format!("{XM}{}{FEED}", all_at(HOME)),
                vec![
                    "the [xmarket] state dir `/srv/tengu-home/state/xm-test` is inside agents.xm_exec.workspace `/srv/tengu-home` — keep it outside every fs root and workspace (xmarket: no tool may reach the install-wide stores)",
                    "is inside agents.xm_feeds.workspace",
                    "is inside agents.xm_loop.workspace",
                ],
            ),
            (
                "an fs root inside the state dir",
                format!("{XM}{AGENTS}[default_scopes.read_file]\nfs_roots = [\"{HOME}/state/xm-test/notes\"]\n"),
                vec!["the [xmarket] state dir `/srv/tengu-home/state/xm-test` is around default_scopes.read_file.fs_roots `/srv/tengu-home/state/xm-test/notes`"],
            ),
        ];
        for (label, toml, want) in cases {
            let got = rules(&toml);
            for e in &got {
                assert!(
                    want.iter().any(|w| e.contains(w)),
                    "{label}: unexpected {e}"
                );
            }
            for w in want {
                assert!(
                    got.iter().any(|e| e.contains(w)),
                    "{label}: want {w}\ngot {got:#?}"
                );
            }
        }
    }

    /// A hardened (`[risk]`) sandbox leaves the state dir to the broader
    /// `<TENGU_HOME>/state` rule, which reports a workspace holding it
    /// (`hardening::tests::state_dir_and_kill_switch_stay_outside_every_root`):
    /// not reported twice.
    #[test]
    fn a_hardened_sandbox_leaves_the_state_dir_to_hardening() {
        use crate::config::risk::tests::RISK_100;
        let all_home = AGENTS
            .replace(
                "workspace = \"/srv/xm-ws\"",
                &format!("workspace = \"{HOME}\""),
            )
            .replacen(
                "model = \"m\"\n",
                &format!("model = \"m\"\nworkspace = \"{HOME}\"\n"),
                1,
            );
        let got = rules(&format!("{XM}{RISK_100}{all_home}"));
        assert!(got.is_empty(), "{got:#?}");
    }

    /// A weekend-fade sandbox: an exchange calendar, `[xmarket.weekend_fade]`,
    /// the recorder, the $100 `[risk]`, a private exec agent holding the tool.
    fn fade_sandbox() -> String {
        use crate::config::risk::tests::RISK_100;
        format!(
            r#"{XM}
[xmarket.calendars.us]
kind = "exchange"
tz = "America/New_York"
core = ["09:30", "16:00"]

[xmarket.calendars.crypto]
kind = "24x7"

[xmarket.weekend_fade]
calendar = "us"
universe = ["hyperliquid:xyz:TSLA"]
exclude = []
capped_top_n = 1
min_abs_signal_bps = 50
capped_notional_usd = 25
shadow_account = "xmarket-shadow"
shadow_initial_cash_usd = 10000
shadow_notional_usd = 100
expected_edge_bps = 23
anchor_max_age_secs = 600
entry_max_age_secs = 120
entry_lateness_max_secs = 600
max_slippage_bps = 50

[recorder]
enabled = true
schemas = ["mkt_ctx/1"]
{RISK_100}
[agents.main]
default = true
engine = "openrouter"
model = "m"
workspace = "/srv/xm-ws"

[agents.xm_exec]
engine = "openrouter"
model = "m"
workspace = "/srv/xm-ws"
tools = ["xm_weekend_fade"]
"#
        )
    }

    /// Every `[xmarket.weekend_fade]` rule, table-driven (each error matches
    /// a `want`, each `want` an error).
    #[test]
    fn weekend_fade_rules() {
        let base = fade_sandbox();
        let with = |from: &str, to: &str| {
            assert!(base.contains(from), "{from}");
            base.replacen(from, to, 1)
        };
        let cases: Vec<(&str, String, Vec<&str>)> = vec![
            ("valid", base.clone(), vec![]),
            (
                "a 24x7 calendar",
                with("calendar = \"us\"", "calendar = \"crypto\""),
                vec!["xmarket.weekend_fade: calendar `crypto` must be kind = \"exchange\""],
            ),
            (
                "an unknown calendar",
                with("calendar = \"us\"", "calendar = \"nyse\""),
                vec!["calendar `nyse` is not an [xmarket.calendars.<id>] row"],
            ),
            (
                "a short id",
                with(
                    "universe = [\"hyperliquid:xyz:TSLA\"]",
                    "universe = [\"xyz:TSLA\"]",
                ),
                vec![
                    "universe id `xyz:TSLA` is not a full `hyperliquid:<coin>` id",
                    "every universe name not excluded must be tradable under [risk]",
                ],
            ),
            (
                "a twice-listed id, an exclusion outside the universe",
                with(
                    "universe = [\"hyperliquid:xyz:TSLA\"]\nexclude = []",
                    "universe = [\"hyperliquid:xyz:TSLA\", \"hyperliquid:xyz:TSLA\"]\n\
                     exclude = [\"hyperliquid:xyz:NVDA\"]",
                ),
                vec![
                    "universe lists `hyperliquid:xyz:TSLA` twice",
                    "exclude id `hyperliquid:xyz:NVDA` is not in universe",
                ],
            ),
            (
                "more capped names than tradable ones",
                with("capped_top_n = 1", "capped_top_n = 2"),
                vec!["capped_top_n 2 exceeds the 1 universe names not excluded"],
            ),
            (
                "under the HL minimum",
                with("capped_notional_usd = 25", "capped_notional_usd = 5"),
                vec!["capped_notional_usd must be ≥ 10"],
            ),
            (
                "over the [risk] order cap",
                with("capped_notional_usd = 25", "capped_notional_usd = 30"),
                vec!["capped_notional_usd 30 exceeds [risk] max_order_notional_usd 25"],
            ),
            (
                "over the gross budget",
                with(
                    "max_gross_exposure_usd = 100",
                    "max_gross_exposure_usd = 20",
                ),
                vec!["capped_top_n × capped_notional_usd = 25 exceeds [risk] max_gross_exposure_usd 20"],
            ),
            (
                "the [risk] account as the shadow",
                with(
                    "shadow_account = \"xmarket-shadow\"",
                    "shadow_account = \"xmarket\"",
                ),
                vec!["shadow_account `xmarket` is the [risk] account"],
            ),
            (
                "a shadow account with a space",
                with(
                    "shadow_account = \"xmarket-shadow\"",
                    "shadow_account = \"x shadow\"",
                ),
                vec!["shadow_account `x shadow` must be a [A-Za-z0-9._-] ledger account"],
            ),
            (
                "an edge under min_edge_bps",
                with("expected_edge_bps = 23", "expected_edge_bps = 5"),
                vec!["expected_edge_bps 5 is below [risk] min_edge_bps 10"],
            ),
            (
                "no lateness",
                with(
                    "entry_lateness_max_secs = 600",
                    "entry_lateness_max_secs = 0",
                ),
                vec!["entry_lateness_max_secs must be > 0 and < 54000"],
            ),
            (
                "an anchor age under the recorder heartbeat",
                with("anchor_max_age_secs = 600", "anchor_max_age_secs = 100"),
                vec!["anchor_max_age_secs 100 is below 300 s"],
            ),
            (
                "mkt_ctx/1 not recorded",
                with(
                    "schemas = [\"mkt_ctx/1\"]",
                    "schemas = [\"hl_book/1\"]",
                ),
                vec!["the anchor price comes from the history: [recorder] enabled = true with `mkt_ctx/1`"],
            ),
            (
                "the fades' strategy needs a hedge",
                with(
                    "require_hedge_for = [\"convergence\"]",
                    "require_hedge_for = [\"overreaction\"]",
                ),
                vec!["[risk] require_hedge_for lists `overreaction`"],
            ),
            (
                "a denied universe name",
                with("instruments_deny = []", "instruments_deny = [\"hyperliquid:xyz:TSLA\"]"),
                vec!["hyperliquid:xyz:TSLA is in instruments_deny"],
            ),
            (
                "excluded, it need not be tradable",
                with(
                    "universe = [\"hyperliquid:xyz:TSLA\"]\nexclude = []\ncapped_top_n = 1",
                    "universe = [\"hyperliquid:xyz:TSLA\", \"hyperliquid:xyz:KIOXIA\"]\n\
                     exclude = [\"hyperliquid:xyz:KIOXIA\"]\ncapped_top_n = 1",
                ),
                vec![],
            ),
            (
                "an agent holds the tool, no section",
                format!(
                    "{XM}{}\n[agents.x]\nengine = \"openrouter\"\nmodel = \"m\"\nworkspace = \"/srv/xm-ws\"\ntools = [\"xm_weekend_fade\"]\n",
                    crate::config::risk::tests::RISK_100
                ),
                vec!["agents.x holds xm_weekend_fade, but the sandbox has no [xmarket.weekend_fade]"],
            ),
        ];
        for (label, toml, want) in cases {
            let got = rules(&toml);
            for e in &got {
                assert!(
                    want.iter().any(|w| e.contains(w)),
                    "{label}: unexpected {e}"
                );
            }
            for w in want {
                assert!(
                    got.iter().any(|e| e.contains(w)),
                    "{label}: want {w}\ngot {got:#?}"
                );
            }
        }
        // Without [risk] + [paper].
        let no_risk = base.replacen(crate::config::risk::tests::RISK_100, "", 1);
        let got = rules(&no_risk);
        assert!(
            got.iter().any(|e| e.contains("needs [risk] + [paper]")),
            "{got:#?}"
        );
        // Every key required, unknown keys refused.
        let missing = with("max_slippage_bps = 50\n", "");
        assert!(toml::from_str::<Config>(&missing).is_err());
        let typo = with(
            "max_slippage_bps = 50",
            "max_slippage_bps = 50\nmax_slippage = 50",
        );
        assert!(toml::from_str::<Config>(&typo).is_err());
        // The rules run in `Config::validate`; the section reaches every
        // agent's tools.
        let mut cfg: Config = toml::from_str(&base).unwrap();
        cfg.validate().expect("valid");
        cfg.fold_default_scopes();
        let fade = cfg.agents["xm_exec"].sandbox.weekend_fade.clone().unwrap();
        assert_eq!(fade.universe, ["hyperliquid:xyz:TSLA"]);
        assert_eq!(fade.rule().capped_top_n, 1);
        let bad: Config =
            toml::from_str(&with("expected_edge_bps = 23", "expected_edge_bps = 5")).unwrap();
        assert!(bad.validate().is_err());
    }

    /// The commented `[xmarket.weekend_fade]` block of `config.example.toml`,
    /// uncommented under the calendar fixture, passes the section's rules:
    /// the 75-name universe in full ids, NYSE as the clock.
    #[test]
    fn example_weekend_fade_block_is_valid() {
        let text = include_str!("../../config.example.toml");
        let block: Vec<&str> = text
            .lines()
            .skip_while(|l| *l != "# [xmarket.weekend_fade]")
            .take_while(|l| l.starts_with('#'))
            .map(|l| l.strip_prefix("# ").unwrap_or(l.trim_start_matches('#')))
            .collect();
        assert!(block.len() > 20, "{block:?}");
        let x = toml::from_str::<Fixture>(&format!("{FIXTURE}\n{}", block.join("\n")))
            .unwrap_or_else(|e| panic!("{e}"))
            .xmarket;
        assert_eq!(x.validation_errors(), Vec::<String>::new());
        let fade = x.weekend_fade.unwrap();
        assert_eq!(fade.universe.len(), 75);
        assert!(fade
            .universe
            .iter()
            .all(|id| id.starts_with("hyperliquid:xyz:")));
        assert!(fade.universe.contains(&"hyperliquid:xyz:TSLA".to_string()));
        assert_eq!(
            (
                fade.capped_top_n,
                fade.min_abs_signal_bps,
                fade.capped_notional_usd
            ),
            (4, 50.0, 25.0)
        );
        assert_eq!(fade.calendar, "us_equity");
        assert_eq!(fade.expected_edge_bps, 23.0);
    }

    /// `sandboxes/xmarket-weekend/config.toml` (`x-weekend-sandbox`) trades
    /// rule W as the replay golden computed it: the 75 names with the
    /// golden's knobs, books of exactly those names, and the replay of the
    /// 2026-09-26 → 09-28 weekend through its own calendar, universe and
    /// rule reproduces the golden bit for bit (KIOXIA excluded here, as the
    /// fixture's provenance says: split halt). Also what rule W needs from
    /// the rest of the file, and its first window = the runbook's.
    #[test]
    fn weekend_sandbox_replays_the_golden() {
        use crate::domain::xm::weekend_fade::{fade_window, golden, replay};

        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("sandboxes")
            .join("xmarket-weekend")
            .join("config.toml");
        let cfg = Config::load(&path).unwrap_or_else(|e| panic!("{}: {e:#}", path.display()));
        let x = cfg.xmarket.as_ref().unwrap();
        assert_eq!(x.state, "xmarket-weekend");
        let fade = x.weekend_fade.as_ref().unwrap();
        let candles = golden::candles();
        assert_eq!(candles.len(), 75);
        let missing: Vec<&String> = candles
            .keys()
            .filter(|id| !fade.universe.contains(id))
            .collect();
        assert!(missing.is_empty(), "universe lacks {missing:?}");
        assert_eq!(
            (
                fade.capped_top_n,
                fade.min_abs_signal_bps,
                fade.capped_notional_usd
            ),
            (4, 50.0, 25.0)
        );
        let coins: Vec<&str> = fade
            .universe
            .iter()
            .map(|id| id.strip_prefix("hyperliquid:").unwrap())
            .collect();
        let books: Vec<&str> = cfg.feeds["hl_book"].each["coin"]
            .iter()
            .map(|c| c.as_str().unwrap())
            .collect();
        assert_eq!(books, coins);

        let Some(Calendar::Exchange(cal)) = x.calendars().remove(&fade.calendar) else {
            panic!("calendar `{}` is not an exchange row", fade.calendar)
        };
        let w = fade_window(&cal, et("2026-09-25 12:00")).unwrap();
        assert_eq!(
            (w.anchor_ms, w.entry_ms, w.exit_ms),
            (
                utc("2026-09-26 00:00"),
                utc("2026-09-27 22:00"),
                utc("2026-09-28 13:00")
            )
        );
        let universe: BTreeMap<_, _> = candles
            .into_iter()
            .filter(|(id, _)| fade.universe.contains(id))
            .collect();
        let mut exclude: BTreeSet<String> = fade.exclude.iter().cloned().collect();
        exclude.insert("hyperliquid:xyz:KIOXIA".to_string());
        assert_eq!(exclude, golden::excluded());
        let r = replay(&universe, &w, &exclude, golden::cost_rt_bps(), &fade.rule());
        golden::assert_replay(&r);

        // Rule W's needs beyond the load rules (wave D notes).
        let (risk, paper) = (cfg.risk.as_ref().unwrap(), cfg.paper.as_ref().unwrap());
        let capped_usd = fade.capped_top_n as f64 * fade.capped_notional_usd;
        assert!(risk.max_leverage * paper.initial_cash_usd > capped_usd);
        assert!(risk.max_net_exposure_usd >= capped_usd);
        assert!(risk.max_orders_per_min as usize >= 2 * fade.capped_top_n);
        assert!(risk.exits.max_hold_secs > 15 * 3600);
        let every = |f: &str| cfg.feeds[f].every_secs;
        assert_eq!(
            (every("hl_ctx"), every("xm_weekend_fade"), every("xm_exits")),
            (Some(60), Some(60), Some(15))
        );
        let ctx = &cfg.feeds["hl_ctx"];
        assert_eq!(ctx.args["dex"], "xyz");
        let ctx_late_ms = ctx.every_secs.unwrap() * 1000 * (100 + u64::from(ctx.jitter_pct)) / 100;
        assert!(risk.max_data_age_ms.ctx >= ctx_late_ms);
        assert!(fade.entry_max_age_secs * 1000 >= ctx_late_ms);
        for schema in ["mkt_ctx/1", "hl_book/1", "mkt_instrument/1"] {
            assert!(cfg.recorder.records(schema), "{schema}");
        }
        assert!(cfg.recorder.retention_days == 0 || cfg.recorder.retention_days >= 30);

        // This weekend: anchor Fri 2026-10-02 20:00, entry Sun 18:00, exit
        // Mon 09:00 New York (the runbook's timeline).
        let first = fade_window(&cal, et("2026-10-01 12:00")).unwrap();
        assert_eq!(
            (first.anchor_ms, first.entry_ms, first.exit_ms),
            (
                et("2026-10-02 20:00"),
                et("2026-10-04 18:00"),
                et("2026-10-05 09:00")
            )
        );
        assert_eq!(first.entry_ms, utc("2026-10-04 22:00"));
    }

    /// The rules run inside `Config::validate` (and so `Config::load`).
    #[test]
    fn validate_runs_the_rules() {
        let mut cfg: Config =
            toml::from_str(&format!("{XM}{}{FEED}", ws("xm_loop", Some("/srv/other")))).unwrap();
        let e = cfg.validate().unwrap_err().to_string();
        assert!(e.contains("must share one `workspace`"), "{e}");
        cfg.agents.get_mut("xm_loop").unwrap().workspace = Some("/srv/xm-ws".into());
        cfg.validate().expect("one workspace");
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
