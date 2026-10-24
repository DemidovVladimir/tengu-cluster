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
//! Paths: [`ledger_db`], [`history_dir`]; `runtime.db` is core
//! (`config::runtime::runtime_db`: every `tengu run` has one, xmarket or not).
//!
//! | Path | Holds | Opened by |
//! |---|---|---|
//! | `<state dir>/ledger.db` | paper ledger + risk verdicts; each account's owner sandbox | `outbound/paper_store.rs` |
//! | `<state dir>/runtime.db` · `run-<sandbox>.json` | the `tengu run` / `tengu webhooks` leases (`runtime:<sandbox>`, `state:<dir>`: one owner per state dir) · heartbeat | `outbound/runtime_store.rs` |
//! | `<state dir>/history/<YYYYMMDD>.db` | recorder day files (UTC) | `outbound/history_sqlite.rs` |
//! | `<state dir>/market.db` · `backtests/<run id>/` | the market-data warehouse · backtest run dirs + the decision cache (xlab) | `outbound/market_data.rs` · `application/backtest/` |
//! | `<state dir>/strategy-rankings/<contract id>/` | published strategy rankings (`<date>/{manifest,ranking}.json`, `ranking.md`, `latest.{json,md}`); retention keeps the runs a `latest.json` cites | `application/ranking/` (writes) · `application/backtest/mod.rs` (`cited_runs`) |
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
    parse_date, parse_week_hm, Calendar, ExchangeCalendar, WeeklyWindow,
};
use crate::domain::market::MarketCtx;
use crate::domain::observation::Observed;
use crate::domain::time::parse_hm;
use crate::domain::tools::{XM_TOOLS, XM_WEEKEND_FADE};
use crate::domain::tz::Zone;
use crate::domain::xm::risk::permission_error;
use crate::domain::xm::weekend_fade::{FadeRule, FADE_STRATEGY};

/// The rows the weekend fade's anchor price comes from.
const MKT_CTX_SCHEMA: &str = <MarketCtx as Observed>::SCHEMA;

/// The paper ledger (`outbound/paper_store.rs`).
pub(crate) const LEDGER_DB: &str = "ledger.db";
/// The recorder's `<YYYYMMDD>.db` day files (`outbound/history_sqlite.rs`).
pub(crate) const HISTORY_DIR: &str = "history";
/// The market-data warehouse: bars, funding, contexts (`outbound/market_data.rs`, xlab).
pub(crate) const MARKET_DB: &str = "market.db";
/// Backtest run dirs + the decision cache (`application/backtest/`, xlab).
pub(crate) const BACKTESTS_DIR: &str = "backtests";
/// Published strategy rankings, one dir per contract (`<contract id>/<date>/`,
/// `<contract id>/latest.{json,md}`); run-dir retention keeps every run a
/// `latest.json` cites (`application/backtest/mod.rs::cited_runs`).
pub(crate) const RANKINGS_DIR: &str = "strategy-rankings";
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

/// `<state_dir>/history`.
pub(crate) fn history_dir(state_dir: &Path) -> PathBuf {
    state_dir.join(HISTORY_DIR)
}

/// `<state_dir>/market.db`.
pub(crate) fn market_db(state_dir: &Path) -> PathBuf {
    state_dir.join(MARKET_DB)
}

/// `<state_dir>/backtests`.
pub(crate) fn backtests_dir(state_dir: &Path) -> PathBuf {
    state_dir.join(BACKTESTS_DIR)
}

/// `<state_dir>/strategy-rankings`.
pub(crate) fn rankings_dir(state_dir: &Path) -> PathBuf {
    state_dir.join(RANKINGS_DIR)
}

/// `[xmarket]` section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct XmarketConfig {
    /// Directory name under `<TENGU_HOME>/state/` that holds the install-wide
    /// stores (`ledger.db`, `runtime.db`, `history/`, …): `xmarket` for the
    /// main sandbox, `xmarket-weekend` for the weekend run. One path segment;
    /// `tengu prune` never deletes `state/`. `flows` stays refused: binaries
    /// built before 2026-10-22 prune `state/flows`.
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
        super::paths::Layout::at(tengu_home).state_dir(&self.state)
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
        // `flows`: an older binary's `tengu prune` deletes `state/flows`.
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
#[path = "xmarket_tests.rs"]
mod tests;
