//! `[risk]` + `[paper]` — the limits of the xmarket paper budget (tracker
//! § 7 #3: $100) and the paper fill engine's knobs. Every field is required
//! (no serde defaults) and every table is `deny_unknown_fields`, so a missing
//! or misspelled limit fails `Config::load` instead of trading without it;
//! `Config` itself rejects unknown top-level keys, so `[rsik]` fails too
//! (convention 6). Tools read both sections through `AgentConfig::sandbox`
//! (`config/sections.rs`); no `[risk]` ⇒ every exec tool refuses
//! (`risk_config_missing`, convention 9).
//!
//! ```toml
//! [risk]
//! account = "xmarket"
//! mode = "paper"
//! venues = ["hyperliquid"]
//! min_lifecycle = "paper_tradable"
//! instruments_allow = ["hyperliquid:xyz:TSLA"]
//! instruments_deny = []
//! max_order_notional_usd = 25
//! max_position_notional_usd = 50
//! max_asset_exposure_usd = 50
//! max_venue_exposure_usd = 100
//! max_gross_exposure_usd = 100
//! max_net_exposure_usd = 100
//! max_leverage = 1
//! daily_loss_limit_usd = 10
//! total_loss_limit_usd = 25
//! min_edge_bps = 10
//! max_slippage_bps = 30
//! min_depth_usd = 250
//! require_hedge_for = ["convergence"]
//! max_data_age_ms = { book = 5000, ctx = 20000, reference = 60000, quote = 20000 }
//! max_skew_ms = 5000
//! max_orders_per_min = 6
//! max_open_orders = 4
//! kill_switch_file = "~/.tengu/state/xmarket/KILL"
//! allow_reduce_degraded = true
//!
//! [paper]
//! initial_cash_usd = 100
//! latency_ms = 250
//! latency_jitter_ms = 100
//! fee_tier = 0
//! staking_discount_pct = 0
//! order_types = ["market", "ioc"]
//! ```
//!
//! | Load rule (any violation fails `Config::load`) | Why |
//! |---|---|
//! | `mode = "paper"`; `"live"` is refused | live orders start in M3b, after an M3 "go" (§ 7 #19) |
//! | `[risk]` and `[paper]` come together | the paper engine has no limits without `[risk]`, the gate no cash without `[paper]` |
//! | Numbers finite and > 0 (`min_edge_bps` ≥ 0, `staking_discount_pct` 0–40, `fee_tier` 0–6, `latency_jitter_ms` ≤ `latency_ms`) | a 0 / NaN / inf cap is a typo, never a policy |
//! | Instrument ids `<venue>:<native id>` in full, venue in [`VENUES`] (convention 1); an allow-listed id's venue is in `venues`; no id both allowed and denied | a short or misspelled id never matches a row — a denied id would silently stay tradable |
//! | `max_order ≤ max_position ≤ max_gross_exposure`, `daily_loss ≤ total_loss` | catches a cap typed one digit too long |
//! | `hyperliquid` in `venues` ⇒ `max_order_notional_usd ≥ 10` | HL rejects orders under $10 (`MinTradeNtl`) |
//! | `min_lifecycle` ≥ `mapped` | §29: discovered / identified instruments are never tradable |
//! | `kill_switch_file` absolute or `~/…` | loops, the bridge and the operator CLI must see one file |
//! | `require_hedge_for` names [`STRATEGIES`] only | a misspelled strategy would silently drop its hedge rule |
//!
//! Halts (§ 7 #8): `daily_loss_limit_usd` trips a halt that clears at 00:00
//! UTC; `total_loss_limit_usd`, an operator halt and the kill-switch file
//! clear only through `tengu risk resume` (TTY). Enforcement lives in the
//! gate (`risk-gate-domain`, `risk-kill-switch`); this module is the schema.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::paths::expand_tilde;
use super::Config;
use crate::domain::xm::risk::{MaxAges, RiskLimits};

/// Venues an order may target (tracker convention 1). `ref:<MIC>` ids are
/// reference prices, never tradable.
pub(crate) const VENUES: &[&str] = &[
    "hyperliquid",
    "robinhood",
    "binance-usdm",
    "bybit-linear",
    "okx-swap",
    "coinbase",
];

/// §21 opportunity types — the `strategy` names `require_hedge_for` accepts.
pub(crate) const STRATEGIES: &[&str] = &[
    "convergence",
    "information_latency",
    "overreaction",
    "underreaction",
    "related_asset",
];

/// Hyperliquid's minimum order notional (USD); smaller orders are rejected
/// with `MinTradeNtl`.
pub(crate) const HL_MIN_ORDER_USD: f64 = 10.0;

/// `[risk]` — every §28 limit the gate checks inside each exec tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RiskConfig {
    /// Ledger account (`<state dir>/ledger.db`), e.g. `xmarket`. `[A-Za-z0-9._-]`.
    pub account: String,
    /// `paper` only until the live pilot (M3b).
    pub mode: RiskMode,
    /// Venues orders may target ([`VENUES`]).
    pub venues: Vec<String>,
    /// Lowest §29 state an instrument needs. M0 permits by `instruments_allow`
    /// only (convention 10); the gate reads this once the catalog exists
    /// (`kg-lifecycle`, M1: `mapped`; from M2: `paper_tradable`).
    pub min_lifecycle: Lifecycle,
    /// Full instrument ids that may be traded, e.g. `hyperliquid:xyz:TSLA`
    /// (the interim §29 gate, convention 10). Empty = nothing is tradable.
    pub instruments_allow: Vec<String>,
    /// Full instrument ids never traded; wins over every other permission.
    pub instruments_deny: Vec<String>,
    /// Notional of one order.
    pub max_order_notional_usd: f64,
    /// Notional of one position after the fill.
    pub max_position_notional_usd: f64,
    /// Net notional per underlying across venues after the fill.
    pub max_asset_exposure_usd: f64,
    /// Gross notional per venue after the fill.
    pub max_venue_exposure_usd: f64,
    /// Σ |position notional| after the fill — the $100 budget.
    pub max_gross_exposure_usd: f64,
    /// |Σ signed position notional| after the fill.
    pub max_net_exposure_usd: f64,
    /// Gross exposure / equity after the fill; 1 = no leverage.
    pub max_leverage: f64,
    /// Loss since 00:00 UTC that halts entries until the next 00:00 UTC.
    pub daily_loss_limit_usd: f64,
    /// Loss since inception that halts entries until `tengu risk resume`.
    pub total_loss_limit_usd: f64,
    /// Edge after costs an entry needs (fresh `xm_compare/1` row); exits skip it.
    pub min_edge_bps: f64,
    /// Worst fill vs mid the book walk may reach.
    pub max_slippage_bps: f64,
    /// Book depth on the order's side within `max_slippage_bps`.
    pub min_depth_usd: f64,
    /// Strategies ([`STRATEGIES`]) that need an available hedge leg.
    pub require_hedge_for: Vec<String>,
    /// Max age of each input row an order is checked against.
    pub max_data_age_ms: DataAgeLimits,
    /// Max time between the observations of the legs of one decision.
    pub max_skew_ms: u64,
    /// Orders accepted per rolling minute, per account.
    pub max_orders_per_min: u32,
    /// Resting orders per account.
    pub max_open_orders: u32,
    /// Present ⇒ halted; every gate call checks it. Absolute or `~/…`.
    pub kill_switch_file: PathBuf,
    /// § 7 #7: with stale data or a halted account a reduce-only / close
    /// order still passes, and the verdict records `allow_reduce_degraded`.
    pub allow_reduce_degraded: bool,
}

/// `[risk] mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskMode {
    /// Fills from the paper engine (`[paper]`).
    Paper,
    /// Venue orders — refused at load until M3b.
    Live,
}

/// §29 lifecycle states, in order — defined with the gate
/// (`domain/xm/risk.rs`); `kg-lifecycle` owns the transitions.
pub use crate::domain::xm::risk::Lifecycle;

/// `[risk.max_data_age_ms]` — per input kind, milliseconds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DataAgeLimits {
    /// L2 book row (`hl_book/1`).
    pub book: u64,
    /// Market context row (`mkt_ctx/1`).
    pub ctx: u64,
    /// Reference price (`ref:<MIC>` instruments).
    pub reference: u64,
    /// Venue quote (`rh_quote/1`, `rh_dex_quote/1`).
    pub quote: u64,
}

/// `[paper]` — the paper fill engine (`risk-paper-fill-engine`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaperConfig {
    /// Starting cash of the `[risk] account` ledger.
    pub initial_cash_usd: f64,
    /// Simulated order latency: sleep, re-read the book, fill against it.
    pub latency_ms: u64,
    /// ± uniform jitter on `latency_ms`; ≤ `latency_ms`.
    pub latency_jitter_ms: u64,
    /// Hyperliquid volume fee tier, 0–6.
    pub fee_tier: u8,
    /// HYPE staking fee discount in percent, 0–40 (HL tiers 5 … 40).
    pub staking_discount_pct: f64,
    /// Order types the engine fills. M0: `market`, `ioc`.
    pub order_types: Vec<OrderType>,
}

/// `[paper] order_types` entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderType {
    /// IOC bounded by `max_slippage_bps` from mid.
    Market,
    /// Limit IOC: walks to the limit, cancels the rest.
    Ioc,
}

impl RiskConfig {
    /// Copy with `kill_switch_file` `~`-expanded — what `AgentConfig::sandbox`
    /// carries.
    pub(crate) fn resolved(&self) -> Self {
        Self {
            kill_switch_file: expand_tilde(&self.kill_switch_file),
            ..self.clone()
        }
    }

    /// The gate's limits (`domain/xm/risk.rs`). `min_lifecycle` maps to none
    /// until the catalog exists: the M0 permission is `instruments_allow` /
    /// `instruments_deny` only (convention 10; `kg-lifecycle` turns it on).
    pub(crate) fn limits(&self) -> RiskLimits {
        let a = &self.max_data_age_ms;
        RiskLimits {
            account: self.account.clone(),
            venues: self.venues.clone(),
            min_lifecycle: None,
            instruments_allow: self.instruments_allow.clone(),
            instruments_deny: self.instruments_deny.clone(),
            max_order_notional_usd: self.max_order_notional_usd,
            max_position_notional_usd: self.max_position_notional_usd,
            max_asset_exposure_usd: self.max_asset_exposure_usd,
            max_venue_exposure_usd: self.max_venue_exposure_usd,
            max_gross_exposure_usd: self.max_gross_exposure_usd,
            max_net_exposure_usd: self.max_net_exposure_usd,
            max_leverage: self.max_leverage,
            daily_loss_limit_usd: self.daily_loss_limit_usd,
            total_loss_limit_usd: self.total_loss_limit_usd,
            min_edge_bps: self.min_edge_bps,
            max_slippage_bps: self.max_slippage_bps,
            min_depth_usd: self.min_depth_usd,
            require_hedge_for: self.require_hedge_for.clone(),
            max_data_age_ms: MaxAges {
                book: a.book,
                ctx: a.ctx,
                reference: a.reference,
                quote: a.quote,
            },
            max_skew_ms: self.max_skew_ms,
            max_orders_per_min: self.max_orders_per_min,
            max_open_orders: self.max_open_orders,
            allow_reduce_degraded: self.allow_reduce_degraded,
        }
    }
}

pub(crate) fn validation_errors(cfg: &Config) -> Vec<String> {
    let mut errs = Vec::new();
    if let Some(r) = &cfg.risk {
        risk_rules(r, &mut errs);
    }
    if let Some(p) = &cfg.paper {
        paper_rules(p, &mut errs);
    }
    match (cfg.risk.is_some(), cfg.paper.is_some()) {
        (true, false) => errs.push(
            "[risk] mode = \"paper\" needs a [paper] section (initial_cash_usd, latency_ms, …)"
                .into(),
        ),
        (false, true) => errs.push(
            "[paper] without [risk]: the paper tools refuse every order without limits — add [risk]"
                .into(),
        ),
        _ => {}
    }
    errs
}

fn positive(errs: &mut Vec<String>, path: &str, v: f64) {
    if !(v.is_finite() && v > 0.0) {
        errs.push(format!("{path} must be a finite number > 0 (got {v})"));
    }
}

fn non_negative(errs: &mut Vec<String>, path: &str, v: f64) {
    if !(v.is_finite() && v >= 0.0) {
        errs.push(format!("{path} must be a finite number >= 0 (got {v})"));
    }
}

fn nonzero(errs: &mut Vec<String>, path: &str, v: u64) {
    if v == 0 {
        errs.push(format!("{path} must be > 0"));
    }
}

fn risk_rules(r: &RiskConfig, errs: &mut Vec<String>) {
    if r.mode == RiskMode::Live {
        errs.push(
            "risk.mode = \"live\" is refused until the live pilot (M3b, after an M3 \"go\") — \
             use \"paper\""
                .into(),
        );
    }
    let a = r.account.as_str();
    if a.is_empty()
        || !a
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        errs.push(format!(
            "risk.account `{a}` must be a non-empty [A-Za-z0-9._-] name"
        ));
    }
    if r.venues.is_empty() {
        errs.push("risk.venues: list at least one venue".into());
    }
    for v in &r.venues {
        if !VENUES.contains(&v.as_str()) {
            errs.push(format!(
                "risk.venues: unknown venue `{v}` (one of {})",
                VENUES.join(", ")
            ));
        }
    }
    if r.min_lifecycle < Lifecycle::Mapped {
        errs.push(
            "risk.min_lifecycle below \"mapped\" would permit unmapped instruments (§29) — use \
             \"mapped\" or later"
                .into(),
        );
    }
    for (list, ids) in [
        ("instruments_allow", &r.instruments_allow),
        ("instruments_deny", &r.instruments_deny),
    ] {
        for id in ids {
            if let Some(why) = instrument_id_error(id) {
                errs.push(format!("risk.{list}: `{id}` — {why}"));
            }
        }
    }
    for id in &r.instruments_allow {
        if let Some((venue, _)) = id.split_once(':') {
            if VENUES.contains(&venue) && !r.venues.iter().any(|v| v == venue) {
                errs.push(format!(
                    "risk.instruments_allow: `{id}` — venue `{venue}` is not in risk.venues"
                ));
            }
        }
        if r.instruments_deny.contains(id) {
            errs.push(format!(
                "risk: `{id}` is in both instruments_allow and instruments_deny"
            ));
        }
    }
    for (path, v) in [
        ("max_order_notional_usd", r.max_order_notional_usd),
        ("max_position_notional_usd", r.max_position_notional_usd),
        ("max_asset_exposure_usd", r.max_asset_exposure_usd),
        ("max_venue_exposure_usd", r.max_venue_exposure_usd),
        ("max_gross_exposure_usd", r.max_gross_exposure_usd),
        ("max_net_exposure_usd", r.max_net_exposure_usd),
        ("max_leverage", r.max_leverage),
        ("daily_loss_limit_usd", r.daily_loss_limit_usd),
        ("total_loss_limit_usd", r.total_loss_limit_usd),
        ("max_slippage_bps", r.max_slippage_bps),
        ("min_depth_usd", r.min_depth_usd),
    ] {
        positive(errs, &format!("risk.{path}"), v);
    }
    non_negative(errs, "risk.min_edge_bps", r.min_edge_bps);
    if r.max_order_notional_usd > r.max_position_notional_usd {
        errs.push(format!(
            "risk.max_order_notional_usd ({}) exceeds max_position_notional_usd ({})",
            r.max_order_notional_usd, r.max_position_notional_usd
        ));
    }
    if r.max_position_notional_usd > r.max_gross_exposure_usd {
        errs.push(format!(
            "risk.max_position_notional_usd ({}) exceeds max_gross_exposure_usd ({})",
            r.max_position_notional_usd, r.max_gross_exposure_usd
        ));
    }
    if r.daily_loss_limit_usd > r.total_loss_limit_usd {
        errs.push(format!(
            "risk.daily_loss_limit_usd ({}) exceeds total_loss_limit_usd ({})",
            r.daily_loss_limit_usd, r.total_loss_limit_usd
        ));
    }
    let on_hl = r.venues.iter().any(|v| v == "hyperliquid");
    if on_hl && r.max_order_notional_usd < HL_MIN_ORDER_USD {
        errs.push(format!(
            "risk.max_order_notional_usd ({}) is below Hyperliquid's ${HL_MIN_ORDER_USD} \
             minimum order — every order would be rejected",
            r.max_order_notional_usd
        ));
    }
    for s in &r.require_hedge_for {
        if !STRATEGIES.contains(&s.as_str()) {
            errs.push(format!(
                "risk.require_hedge_for: unknown strategy `{s}` (one of {})",
                STRATEGIES.join(", ")
            ));
        }
    }
    let ages = &r.max_data_age_ms;
    for (k, v) in [
        ("book", ages.book),
        ("ctx", ages.ctx),
        ("reference", ages.reference),
        ("quote", ages.quote),
    ] {
        nonzero(errs, &format!("risk.max_data_age_ms.{k}"), v);
    }
    nonzero(errs, "risk.max_skew_ms", r.max_skew_ms);
    nonzero(
        errs,
        "risk.max_orders_per_min",
        u64::from(r.max_orders_per_min),
    );
    nonzero(errs, "risk.max_open_orders", u64::from(r.max_open_orders));
    let kill = r.kill_switch_file.as_path();
    if kill.as_os_str().is_empty() || !expand_tilde(kill).is_absolute() {
        errs.push(format!(
            "risk.kill_switch_file `{}` must be absolute or start with `~/`",
            kill.display()
        ));
    }
}

fn paper_rules(p: &PaperConfig, errs: &mut Vec<String>) {
    positive(errs, "paper.initial_cash_usd", p.initial_cash_usd);
    if p.latency_jitter_ms > p.latency_ms {
        errs.push(format!(
            "paper.latency_jitter_ms ({}) exceeds latency_ms ({})",
            p.latency_jitter_ms, p.latency_ms
        ));
    }
    if p.fee_tier > 6 {
        errs.push(format!(
            "paper.fee_tier = {} — Hyperliquid tiers are 0–6",
            p.fee_tier
        ));
    }
    let pct = p.staking_discount_pct;
    if !(pct.is_finite() && (0.0..=40.0).contains(&pct)) {
        errs.push(format!(
            "paper.staking_discount_pct = {pct} — must be within 0–40 (HL staking tiers)"
        ));
    }
    if p.order_types.is_empty() {
        errs.push("paper.order_types: list at least one order type (market, ioc)".into());
    }
}

/// Why `id` is not a full tradable instrument id (`<venue>:<native id
/// verbatim>`, convention 1); `None` when it is.
fn instrument_id_error(id: &str) -> Option<String> {
    let Some((venue, native)) = id.split_once(':') else {
        return Some("not a full `<venue>:<native id>` instrument id".into());
    };
    if venue == "ref" {
        return Some("a reference instrument (`ref:<MIC>:<symbol>`) is not tradable".into());
    }
    if !VENUES.contains(&venue) {
        return Some(format!(
            "unknown venue `{venue}` (one of {})",
            VENUES.join(", ")
        ));
    }
    if native.split(':').any(str::is_empty)
        || native.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Some("the native id must be verbatim: no empty `:` segment, no whitespace".into());
    }
    if venue == "robinhood" && !is_evm_address(native) {
        return Some("a robinhood id is the token contract, `0x` + 40 hex digits".into());
    }
    None
}

fn is_evm_address(s: &str) -> bool {
    s.strip_prefix("0x")
        .is_some_and(|h| h.len() == 40 && h.chars().all(|c| c.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const AGENT: &str = "[agents.main]\nengine = \"openrouter\"\nmodel = \"m\"\n";

    /// The $100 budget (tracker § 7 #3), one key per line so every required
    /// key can be dropped in turn.
    const RISK_100: &str = r#"
[risk]
account = "xmarket"
mode = "paper"
venues = ["hyperliquid"]
min_lifecycle = "paper_tradable"
instruments_allow = ["hyperliquid:xyz:TSLA"]
instruments_deny = []
max_order_notional_usd = 25
max_position_notional_usd = 50
max_asset_exposure_usd = 50
max_venue_exposure_usd = 100
max_gross_exposure_usd = 100
max_net_exposure_usd = 100
max_leverage = 1
daily_loss_limit_usd = 10
total_loss_limit_usd = 25
min_edge_bps = 10
max_slippage_bps = 30
min_depth_usd = 250
require_hedge_for = ["convergence"]
max_skew_ms = 5000
max_orders_per_min = 6
max_open_orders = 4
kill_switch_file = "~/.tengu/state/xmarket/KILL"
allow_reduce_degraded = true

[risk.max_data_age_ms]
book = 5000
ctx = 20000
reference = 60000
quote = 20000

[paper]
initial_cash_usd = 100
latency_ms = 250
latency_jitter_ms = 100
fee_tier = 0
staking_discount_pct = 0
order_types = ["market", "ioc"]
"#;

    fn parse(sections: &str) -> Result<Config, String> {
        toml::from_str::<Config>(&format!("{AGENT}{sections}")).map_err(|e| e.to_string())
    }

    fn errs_of(sections: &str) -> String {
        validation_errors(&parse(sections).expect("parses")).join("\n")
    }

    /// `RISK_100` with the first line starting `<key> =` replaced.
    fn with(key: &str, line: &str) -> String {
        let prefix = format!("{key} =");
        let mut done = false;
        RISK_100
            .lines()
            .map(|l| {
                if !done && l.starts_with(&prefix) {
                    done = true;
                    line.to_string()
                } else {
                    l.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn budget_100_parses_and_validates() {
        let cfg = parse(RISK_100).unwrap();
        cfg.validate().expect("valid");
        let r = cfg.risk.as_ref().unwrap();
        assert_eq!(r.mode, RiskMode::Paper);
        assert_eq!(r.instruments_allow, vec!["hyperliquid:xyz:TSLA"]);
        assert_eq!(r.max_gross_exposure_usd, 100.0, "TOML integer → f64");
        assert_eq!(r.max_order_notional_usd, 25.0);
        assert_eq!(r.max_position_notional_usd, 50.0);
        assert_eq!(r.max_leverage, 1.0);
        assert_eq!(r.daily_loss_limit_usd, 10.0);
        assert_eq!(r.total_loss_limit_usd, 25.0);
        assert_eq!(r.max_data_age_ms.quote, 20_000);
        assert_eq!(r.min_lifecycle, Lifecycle::PaperTradable);
        let p = cfg.paper.as_ref().unwrap();
        assert_eq!(p.initial_cash_usd, 100.0);
        assert_eq!(p.order_types, vec![OrderType::Market, OrderType::Ioc]);
    }

    /// Every `key = value` line of `RISK_100` is required: dropping any one
    /// is a parse error naming it (`[risk]`, `[risk.max_data_age_ms]`,
    /// `[paper]` alike).
    #[test]
    fn every_field_is_required() {
        let keys: Vec<&str> = RISK_100
            .lines()
            .filter_map(|l| l.split_once(" = ").map(|(k, _)| k))
            .collect();
        assert_eq!(keys.len(), 24 + 4 + 6, "{keys:?}");
        for key in keys {
            let toml = with(key, "");
            let err = parse(&toml).expect_err(key);
            assert!(
                err.contains(&format!("missing field `{key}`")),
                "{key}: {err}"
            );
        }
        let (head, tail) = RISK_100.split_once("[risk.max_data_age_ms]").unwrap();
        let no_ages = format!("{head}{}", &tail[tail.find("[paper]").unwrap()..]);
        let err = parse(&no_ages).unwrap_err();
        assert!(err.contains("missing field `max_data_age_ms`"), "{err}");
    }

    #[test]
    fn unknown_keys_are_errors() {
        for (after, bad) in [
            ("[risk]", "max_exposure_usd = 100"),
            ("[risk.max_data_age_ms]", "trade = 1"),
            ("[paper]", "fee_bps = 4.5"),
        ] {
            let toml = RISK_100.replacen(after, &format!("{after}\n{bad}"), 1);
            let err = parse(&toml).expect_err(bad);
            let key = bad.split(' ').next().unwrap();
            assert!(
                err.contains(&format!("unknown field `{key}`")),
                "{bad}: {err}"
            );
        }
        for bad in [
            r#"mode = "backtest""#,
            r#"min_lifecycle = "tradable""#,
            r#"order_types = ["alo"]"#,
        ] {
            let key = bad.split(' ').next().unwrap();
            let err = parse(&with(key, bad)).expect_err(bad);
            assert!(err.contains("unknown variant"), "{bad}: {err}");
        }
    }

    /// Convention 6: a misspelled top-level table is an error, not an
    /// absent section (`[rsik]` would otherwise mean "no limits").
    #[test]
    fn misspelled_top_level_table_is_an_error() {
        let err = parse(&RISK_100.replace("[risk]", "[rsik]")).unwrap_err();
        assert!(err.contains("unknown field `rsik`"), "{err}");
        // Bare top-level keys too — a runtime-only field is not a TOML key.
        for key in ["typo_key", "sandbox_name"] {
            let err = toml::from_str::<Config>(&format!("{key} = \"x\"\n{AGENT}")).unwrap_err();
            assert!(
                err.to_string().contains(&format!("unknown field `{key}`")),
                "{err}"
            );
        }
    }

    #[test]
    fn load_rules() {
        for (key, line, want) in [
            ("mode", r#"mode = "live""#, "refused until the live pilot"),
            ("account", r#"account = "x market""#, "risk.account"),
            ("venues", "venues = []", "at least one venue"),
            ("venues", r#"venues = ["hyperliqiud"]"#, "unknown venue `hyperliqiud`"),
            ("min_lifecycle", r#"min_lifecycle = "discovered""#, "unmapped"),
            ("instruments_allow", r#"instruments_allow = ["TSLA"]"#, "`TSLA` — not a full"),
            ("instruments_allow", r#"instruments_allow = ["hl:xyz:TSLA"]"#, "unknown venue `hl`"),
            ("instruments_allow", r#"instruments_allow = ["ref:XNAS:TSLA"]"#, "not tradable"),
            ("instruments_allow", r#"instruments_allow = ["hyperliquid:xyz:"]"#, "empty `:` segment"),
            ("instruments_allow", r#"instruments_allow = ["hyperliquid:xyz:TSLA "]"#, "no whitespace"),
            (
                "instruments_allow",
                r#"instruments_allow = ["robinhood:0x322F0929c4625eD5bAd873c95208D54E1c003b2d"]"#,
                "`robinhood:0x322F0929c4625eD5bAd873c95208D54E1c003b2d` — venue `robinhood` is not in risk.venues",
            ),
            ("instruments_deny", r#"instruments_deny = ["robinhood:TSLA"]"#, "0x` + 40 hex"),
            ("instruments_deny", r#"instruments_deny = ["hyperliquid:xyz:TSLA"]"#, "in both"),
            ("max_order_notional_usd", "max_order_notional_usd = 0", "max_order_notional_usd must be a finite number > 0"),
            ("max_leverage", "max_leverage = nan", "max_leverage must be a finite number > 0"),
            ("max_gross_exposure_usd", "max_gross_exposure_usd = inf", "max_gross_exposure_usd must be a finite number > 0"),
            ("min_edge_bps", "min_edge_bps = -1", "min_edge_bps must be a finite number >= 0"),
            ("max_order_notional_usd", "max_order_notional_usd = 60", "exceeds max_position_notional_usd"),
            ("max_position_notional_usd", "max_position_notional_usd = 150", "exceeds max_gross_exposure_usd"),
            ("daily_loss_limit_usd", "daily_loss_limit_usd = 30", "exceeds total_loss_limit_usd"),
            ("max_order_notional_usd", "max_order_notional_usd = 5", "minimum order"),
            ("require_hedge_for", r#"require_hedge_for = ["convergance"]"#, "unknown strategy `convergance`"),
            ("book", "book = 0", "risk.max_data_age_ms.book must be > 0"),
            ("max_open_orders", "max_open_orders = 0", "risk.max_open_orders must be > 0"),
            ("kill_switch_file", r#"kill_switch_file = "KILL""#, "must be absolute"),
            ("initial_cash_usd", "initial_cash_usd = 0", "paper.initial_cash_usd"),
            ("latency_jitter_ms", "latency_jitter_ms = 300", "exceeds latency_ms"),
            ("fee_tier", "fee_tier = 7", "tiers are 0–6"),
            ("staking_discount_pct", "staking_discount_pct = 50", "0–40"),
            ("order_types", "order_types = []", "at least one order type"),
        ] {
            let e = errs_of(&with(key, line));
            assert!(e.contains(want), "{line}: {e}");
        }
        assert_eq!(errs_of(RISK_100), "");
        // A robinhood contract id in full is fine once the venue is permitted.
        let rh = with("venues", r#"venues = ["hyperliquid", "robinhood"]"#).replace(
            r#"instruments_deny = []"#,
            r#"instruments_deny = ["robinhood:0x322F0929c4625eD5bAd873c95208D54E1c003b2d"]"#,
        );
        assert_eq!(errs_of(&rh), "");
    }

    #[test]
    fn risk_and_paper_come_together() {
        let risk_only = RISK_100.split("[paper]").next().unwrap();
        assert_eq!(
            errs_of(risk_only),
            "[risk] mode = \"paper\" needs a [paper] section (initial_cash_usd, latency_ms, …)"
        );
        let paper_only = format!("[paper]{}", RISK_100.split("[paper]").nth(1).unwrap());
        assert!(errs_of(&paper_only).contains("[paper] without [risk]"));
        assert_eq!(errs_of(""), "", "neither = not a trading sandbox");
    }

    /// Both sections reach every agent's tools through the one shared
    /// `AgentConfig::sandbox`, with the kill-switch path expanded.
    #[test]
    fn sections_carry_resolved_risk_and_paper() {
        let mut cfg = parse(&format!(
            "{RISK_100}\n[agents.exec]\nengine = \"openrouter\"\nmodel = \"m\"\n"
        ))
        .unwrap();
        cfg.fold_default_scopes();
        let s = &cfg.agents["main"].sandbox;
        assert!(std::sync::Arc::ptr_eq(s, &cfg.agents["exec"].sandbox));
        let r = s.risk.as_ref().unwrap();
        assert!(
            r.kill_switch_file.is_absolute(),
            "{}",
            r.kill_switch_file.display()
        );
        assert!(r.kill_switch_file.ends_with(".tengu/state/xmarket/KILL"));
        assert_eq!(s.paper.as_ref().unwrap().initial_cash_usd, 100.0);

        let mut plain = Config::default();
        plain.fold_default_scopes();
        assert!(plain.agents["main"].sandbox.risk.is_none());
        assert!(plain.agents["main"].sandbox.paper.is_none());
    }

    /// Convention 6 audit, kept: every sandbox config and the example still
    /// load with unknown top-level keys rejected.
    #[test]
    fn every_sandbox_and_the_example_load() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut n = 0;
        for entry in std::fs::read_dir(root.join("sandboxes")).unwrap() {
            let path = entry.unwrap().path().join("config.toml");
            if path.exists() {
                Config::load(&path).unwrap_or_else(|e| panic!("{}: {e:#}", path.display()));
                n += 1;
            }
        }
        assert!(n >= 6, "found {n} sandbox configs");
        Config::load(&root.join("config.example.toml")).expect("config.example.toml");
    }

    /// `[risk]` maps field by field into the gate's limits; no lifecycle
    /// rule before the catalog (M0).
    #[test]
    fn limits_map_every_field() {
        let cfg = parse(RISK_100).unwrap();
        let r = cfg.risk.unwrap().resolved();
        let l = r.limits();
        assert_eq!(l.account, "xmarket");
        assert_eq!(l.instruments_allow, vec!["hyperliquid:xyz:TSLA"]);
        assert_eq!(l.min_lifecycle, None);
        assert_eq!(
            (
                l.max_order_notional_usd,
                l.max_position_notional_usd,
                l.max_asset_exposure_usd,
                l.max_venue_exposure_usd,
                l.max_gross_exposure_usd,
                l.max_net_exposure_usd,
                l.max_leverage,
                l.daily_loss_limit_usd,
                l.total_loss_limit_usd,
                l.min_edge_bps,
                l.max_slippage_bps,
                l.min_depth_usd,
            ),
            (25.0, 50.0, 50.0, 100.0, 100.0, 100.0, 1.0, 10.0, 25.0, 10.0, 30.0, 250.0)
        );
        assert_eq!(
            (
                l.max_data_age_ms.book,
                l.max_data_age_ms.ctx,
                l.max_data_age_ms.reference
            ),
            (5_000, 20_000, 60_000)
        );
        assert_eq!(l.max_data_age_ms.quote, 20_000);
        assert_eq!(
            (l.max_skew_ms, l.max_orders_per_min, l.max_open_orders),
            (5_000, 6, 4)
        );
        assert_eq!(l.require_hedge_for, vec!["convergence"]);
        assert!(l.allow_reduce_degraded);
    }

    /// The commented `[risk]` / `[paper]` block of `config.example.toml`,
    /// uncommented, is the valid $100 budget.
    #[test]
    fn example_block_uncommented_is_valid() {
        let text = include_str!("../../config.example.toml");
        let block: Vec<&str> = text
            .lines()
            .skip_while(|l| *l != "# [risk]")
            .take_while(|l| l.starts_with('#'))
            .map(|l| l.strip_prefix("# ").unwrap_or(l.trim_start_matches('#')))
            .collect();
        assert!(block.len() > 30, "{block:?}");
        let cfg = parse(&block.join("\n")).unwrap_or_else(|e| panic!("{e}"));
        cfg.validate().expect("valid");
        let r = cfg.risk.unwrap();
        assert_eq!(
            (
                r.max_gross_exposure_usd,
                r.max_order_notional_usd,
                r.max_position_notional_usd,
                r.max_leverage,
                r.daily_loss_limit_usd,
                r.total_loss_limit_usd
            ),
            (100.0, 25.0, 50.0, 1.0, 10.0, 25.0)
        );
        assert_eq!(cfg.paper.unwrap().initial_cash_usd, 100.0);
    }
}
