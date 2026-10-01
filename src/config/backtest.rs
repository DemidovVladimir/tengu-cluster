//! `[backtest]` — history-first research knobs of a sandbox (xlab,
//! `docs/xlab-2026-10-01.md` § 5–9). Read through `SandboxSections.backtest`
//! by `tengu backtest` and the `backtest` / `market_history` tools. Needs
//! `[xmarket]`: `market.db` and the run dirs live in its state dir.
//! `deny_unknown_fields`.
//!
//! | Field | Default | Effect |
//! |---|---|---|
//! | `notional_usd` | `100` | per trade, when a spec sets none |
//! | `bootstrap` | `2000` | resamples of every confidence interval (100–100 000) |
//! | `seed` | `7` | the bootstrap generator's seed: a rerun prints the same numbers |
//! | `costs."<prefix>"` | — | `CostSpec` (`domain/backtest/costs.rs`) for ids starting with the prefix; the longest prefix wins; an instrument without one is refused |
//! | `universes.<name>` | — | full instrument ids, `@<name>` in specs and on the CLI |
//! | `strategies.<name>` | — | strategy specs (`domain/backtest/spec.rs`), the operator's named capabilities |
//! | `gate` | — | the `[decision_loops.<name>]` the Jev gate arm runs by default |

// Consumers land with the xlab wave (docs/xlab-2026-10-01.md); drop this then.
#![cfg_attr(not(test), allow(dead_code))]

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::backtest::costs::CostSpec;
use crate::domain::market::InstrumentId;

/// `[backtest]` section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BacktestConfig {
    #[serde(default = "default_notional_usd")]
    pub notional_usd: f64,
    #[serde(default = "default_bootstrap")]
    pub bootstrap: u32,
    #[serde(default = "default_seed")]
    pub seed: u64,
    #[serde(default)]
    pub costs: BTreeMap<String, CostSpec>,
    #[serde(default)]
    pub universes: BTreeMap<String, Vec<String>>,
    /// Raw spec tables; `domain::backtest::spec` parses and validates them.
    #[serde(default)]
    pub strategies: BTreeMap<String, Value>,
    #[serde(default)]
    pub gate: Option<String>,
}

impl Default for BacktestConfig {
    fn default() -> Self {
        Self {
            notional_usd: default_notional_usd(),
            bootstrap: default_bootstrap(),
            seed: default_seed(),
            costs: BTreeMap::new(),
            universes: BTreeMap::new(),
            strategies: BTreeMap::new(),
            gate: None,
        }
    }
}

fn default_notional_usd() -> f64 {
    100.0
}

fn default_bootstrap() -> u32 {
    2_000
}

fn default_seed() -> u64 {
    7
}

/// `[a-z0-9_]{1,48}` — universe and strategy names.
pub fn valid_name(name: &str) -> bool {
    (1..=48).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

impl BacktestConfig {
    /// Section-local problems; `has_xmarket` = the sandbox has `[xmarket]`,
    /// `loops` = its `[decision_loops]` names. Strategy specs are checked by
    /// the spec parser (`domain::backtest::spec`) where it is wired.
    pub fn validation_errors(&self, has_xmarket: bool, loops: &[&str]) -> Vec<String> {
        let mut errors = Vec::new();
        if !has_xmarket {
            errors.push(
                "[backtest] needs [xmarket]: market.db and backtest runs live in its state dir"
                    .to_string(),
            );
        }
        if !(self.notional_usd.is_finite() && self.notional_usd > 0.0) {
            errors.push("backtest.notional_usd must be finite and > 0".to_string());
        }
        if !(100..=100_000).contains(&self.bootstrap) {
            errors.push("backtest.bootstrap must be within 100..=100000".to_string());
        }
        for (prefix, cost) in &self.costs {
            if prefix.trim().is_empty() {
                errors.push("backtest.costs has an empty prefix".to_string());
            }
            errors.extend(cost.validation_errors(&format!("backtest.costs.\"{prefix}\"")));
        }
        for (name, ids) in &self.universes {
            if !valid_name(name) {
                errors.push(format!(
                    "backtest.universes.{name}: names are [a-z0-9_], 1-48 characters"
                ));
            }
            if ids.is_empty() {
                errors.push(format!("backtest.universes.{name} is empty"));
            }
            for id in ids {
                if let Err(e) = InstrumentId::parse(id) {
                    errors.push(format!("backtest.universes.{name}: {e}"));
                }
            }
        }
        for name in self.strategies.keys() {
            if !valid_name(name) {
                errors.push(format!(
                    "backtest.strategies.{name}: names are [a-z0-9_], 1-48 characters"
                ));
            }
        }
        if let Some(gate) = &self.gate {
            if !loops.contains(&gate.as_str()) {
                errors.push(format!(
                    "backtest.gate = \"{gate}\" names no [decision_loops.{gate}]"
                ));
            }
        }
        errors
    }

    /// The ids of `@<name>` (a universe) or a single full id.
    pub fn resolve_universe(&self, item: &str) -> Result<Vec<String>, String> {
        match item.strip_prefix('@') {
            Some(name) => self
                .universes
                .get(name)
                .cloned()
                .ok_or_else(|| format!("no [backtest.universes] entry `{name}`")),
            None => InstrumentId::parse(item).map(|_| vec![item.to_string()]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml_src: &str) -> BacktestConfig {
        toml::from_str(toml_src).unwrap()
    }

    #[test]
    fn defaults_and_tables_load() {
        let c = parse(
            r#"
            [costs."hyperliquid:xyz:"]
            taker_fee_bps = 0.9
            half_spread = { model = "fixed", bps = 1.0 }
            [universes]
            crypto = ["hyperliquid:BTC", "hyperliquid:SOL"]
            [strategies.weekend_fade]
            kind = "weekend_window"
            universe = "@crypto"
            "#,
        );
        assert_eq!((c.notional_usd, c.bootstrap, c.seed), (100.0, 2_000, 7));
        assert_eq!(c.universes["crypto"].len(), 2);
        assert_eq!(c.strategies["weekend_fade"]["kind"], "weekend_window");
        assert!(c.validation_errors(true, &[]).is_empty());
        assert_eq!(
            c.resolve_universe("@crypto").unwrap(),
            vec!["hyperliquid:BTC", "hyperliquid:SOL"]
        );
        assert_eq!(
            c.resolve_universe("hyperliquid:xyz:TSLA").unwrap(),
            vec!["hyperliquid:xyz:TSLA"]
        );
        assert!(c.resolve_universe("@nope").is_err());
        assert!(toml::from_str::<BacktestConfig>("notional = 5").is_err());
    }

    #[test]
    fn problems_are_named() {
        let c = parse(
            r#"
            notional_usd = 0
            bootstrap = 5
            gate = "missing_loop"
            [universes]
            Bad-Name = ["nowhere:X"]
            "#,
        );
        let e = c.validation_errors(false, &["xl_gate"]);
        assert!(e.iter().any(|m| m.contains("needs [xmarket]")), "{e:?}");
        assert!(e.iter().any(|m| m.contains("notional_usd")), "{e:?}");
        assert!(e.iter().any(|m| m.contains("bootstrap")), "{e:?}");
        assert!(e.iter().any(|m| m.contains("Bad-Name")), "{e:?}");
        assert!(
            e.iter().any(|m| m.contains("unknown venue `nowhere`")),
            "{e:?}"
        );
        assert!(e.iter().any(|m| m.contains("missing_loop")), "{e:?}");
    }
}
