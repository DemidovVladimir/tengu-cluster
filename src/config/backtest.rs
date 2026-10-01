//! `[backtest]` — history-first research knobs of a sandbox (xlab,
//! `docs/xlab-2026-10-01.md` § 5–9). Read through `SandboxSections.backtest`
//! by `tengu backtest` (`application/backtest/`) and the `backtest` /
//! `market_history` tools. Needs `[xmarket]`: `market.db` and the run dirs
//! live in its state dir. `deny_unknown_fields`.
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
//!
//! | Load rule (`validation_errors`; a violation fails `Config::load`) | The error starts |
//! |---|---|
//! | every `strategies.<name>` parses and validates (`StrategySpec::from_value`: kind, fields, bounds) | `backtest.strategies.<name>: <field> …` |
//! | its `@<universe>` is a `universes` entry | `backtest.strategies.<name>: universe: …` |
//! | its `calendar` (`weekend_window`; `daily_window` with `days = "trading"`) is an exchange `[xmarket.calendars.<id>]` | `backtest.strategies.<name>: calendar …` |
//! | every instrument it trades (universe or named ids, minus `exclude`) has a `costs` prefix, unless the spec sets `costs` | `backtest.strategies.<name>: no [backtest.costs] prefix matches <ids, in full>` |

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::backtest::costs::{cost_for, CostSpec};
use crate::domain::backtest::spec::{valid_name, StrategySpec, Universe};
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
    /// Raw spec tables; [`BacktestConfig::strategy`] parses and validates one
    /// (`domain::backtest::spec`), `validation_errors` all of them at load.
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

impl BacktestConfig {
    /// Section-local problems (module tables); `has_xmarket` = the sandbox
    /// has `[xmarket]`, `loops` = its `[decision_loops]` names, `calendars`
    /// = its exchange `[xmarket.calendars]` ids.
    pub fn validation_errors(
        &self,
        has_xmarket: bool,
        loops: &[&str],
        calendars: &[&str],
    ) -> Vec<String> {
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
            errors.extend(self.strategy_errors(name, calendars));
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

    /// One library strategy's problems (module table), each starting
    /// `backtest.strategies.<name>: `.
    fn strategy_errors(&self, name: &str, calendars: &[&str]) -> Vec<String> {
        let at = format!("backtest.strategies.{name}");
        let spec = match self.strategy(name) {
            Ok(spec) => spec,
            Err(errors) => {
                // The parser says "strategy `<name>`: …"; the path says it here.
                let own = format!("strategy `{name}`: ");
                return errors
                    .into_iter()
                    .map(|e| format!("{at}: {}", e.strip_prefix(&own).unwrap_or(&e)))
                    .collect();
            }
        };
        let mut errors = Vec::new();
        match self.spec_instruments(&spec) {
            Err(e) => errors.push(format!("{at}: universe: {e}")),
            Ok(ids) if spec.costs.is_none() => {
                let missing: Vec<&str> = ids
                    .iter()
                    .filter(|id| !spec.exclude.contains(*id) && cost_for(&self.costs, id).is_none())
                    .map(String::as_str)
                    .collect();
                if !missing.is_empty() {
                    errors.push(format!(
                        "{at}: no [backtest.costs] prefix matches {} — add [backtest.costs.\"<prefix>\"] or the strategy's own costs",
                        missing.join(", ")
                    ));
                }
            }
            Ok(_) => {}
        }
        if let Some(cal) = spec.calendar() {
            if !calendars.contains(&cal) {
                errors.push(format!(
                    "{at}: calendar `{cal}` is not an exchange [xmarket.calendars.{cal}] row"
                ));
            }
        }
        errors
    }

    /// `[backtest.strategies.<name>]` parsed and validated
    /// (`StrategySpec::from_value`; every error names the strategy).
    pub fn strategy(&self, name: &str) -> Result<StrategySpec, Vec<String>> {
        let value = self.strategies.get(name).ok_or_else(|| {
            let known: Vec<&str> = self.strategies.keys().map(String::as_str).collect();
            vec![if known.is_empty() {
                format!("no [backtest.strategies.{name}]: the sandbox defines no strategy")
            } else {
                format!(
                    "no [backtest.strategies.{name}]; the sandbox has: {}",
                    known.join(", ")
                )
            }]
        })?;
        StrategySpec::from_value(name, value)
    }

    /// The ids `spec` trades: its universe (`@<name>` resolved) or the ids it
    /// names (`pair_spread`, `event_window`), `exclude` still in (the engine
    /// skips those) — sorted, distinct.
    pub fn spec_instruments(&self, spec: &StrategySpec) -> Result<Vec<String>, String> {
        let mut ids = match &spec.universe {
            Some(Universe::Named(text)) => self.resolve_universe(text)?,
            Some(Universe::Ids(ids)) => ids.clone(),
            None => spec.named_instruments(),
        };
        ids.sort();
        ids.dedup();
        Ok(ids)
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

    /// A library that loads: a weekend rule on a named universe, a pair
    /// naming its legs, a spec with its own costs.
    const LIBRARY: &str = r#"
        [costs."hyperliquid:xyz:"]
        taker_fee_bps = 0.9
        half_spread = { model = "fixed", bps = 1.0 }
        [costs."hyperliquid:"]
        taker_fee_bps = 4.5
        [universes]
        crypto = ["hyperliquid:BTC", "hyperliquid:SOL"]
        [strategies.weekend_fade]
        kind = "weekend_window"
        universe = "@crypto"
        interval = "1h"
        calendar = "us_equity"
        direction = "fade"
        [strategies.sol_eth_spread]
        kind = "pair_spread"
        legs = ["hyperliquid:SOL", "hyperliquid:ETH"]
        interval = "1h"
        lookback_bars = 168
        entry_z = 2.0
        exit_z = 0.5
        max_hold_bars = 72
        [strategies.dex_move]
        kind = "move_trigger"
        universe = ["solana:So11111111111111111111111111111111111111112"]
        interval = "1h"
        lookback_bars = 4
        threshold_bps = 300
        direction = "fade"
        hold_bars = 12
        costs = { taker_fee_bps = 5.0, funding = false }
    "#;

    #[test]
    fn defaults_and_tables_load() {
        let c = parse(LIBRARY);
        assert_eq!((c.notional_usd, c.bootstrap, c.seed), (100.0, 2_000, 7));
        assert_eq!(c.universes["crypto"].len(), 2);
        assert_eq!(c.strategies["weekend_fade"]["kind"], "weekend_window");
        let e = c.validation_errors(true, &[], &["us_equity"]);
        assert!(e.is_empty(), "{e:?}");
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
        // One strategy by name; its instruments, universe or legs.
        let w = c.strategy("weekend_fade").unwrap();
        assert_eq!(
            c.spec_instruments(&w).unwrap(),
            vec!["hyperliquid:BTC", "hyperliquid:SOL"]
        );
        let pair = c.strategy("sol_eth_spread").unwrap();
        assert_eq!(
            c.spec_instruments(&pair).unwrap(),
            vec!["hyperliquid:ETH", "hyperliquid:SOL"]
        );
        let e = c.strategy("nope").unwrap_err().join("\n");
        assert!(
            e.contains("no [backtest.strategies.nope]; the sandbox has: dex_move, sol_eth_spread, weekend_fade"),
            "{e}"
        );
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
        let e = c.validation_errors(false, &["xl_gate"], &[]);
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

    /// Every library strategy is checked at load: a bad spec, a missing
    /// universe, a calendar that is not an exchange row and an instrument
    /// without costs — each error names the strategy.
    #[test]
    fn strategies_are_checked_at_load() {
        let mut c = parse(LIBRARY);
        let table = |src: &str| -> Value { toml::from_str(src).unwrap() };
        c.strategies.insert(
            "bad_kind".into(),
            table("kind = \"grid_bot\"\nuniverse = \"@crypto\"\ninterval = \"1h\""),
        );
        c.strategies.insert(
            "bad_field".into(),
            table(
                "kind = \"funding_carry\"\nuniverse = \"@crypto\"\ninterval = \"1h\"\n\
                 min_apr_pct = 10\nhold_hours = 0",
            ),
        );
        c.strategies.insert(
            "lost_universe".into(),
            table(
                "kind = \"funding_carry\"\nuniverse = \"@xyz_stocks\"\ninterval = \"1h\"\n\
                 min_apr_pct = 10\nhold_hours = 24",
            ),
        );
        c.strategies.insert(
            "no_costs".into(),
            table(
                "kind = \"move_trigger\"\nuniverse = [\"hyperliquid:BTC\", \
                 \"robinhood:0x1234567890abcdef1234567890abcdef12345678\", \
                 \"solana:So11111111111111111111111111111111111111112\"]\n\
                 exclude = [\"hyperliquid:BTC\"]\ninterval = \"1h\"\nlookback_bars = 4\n\
                 threshold_bps = 300\ndirection = \"fade\"\nhold_bars = 12",
            ),
        );
        c.strategies.insert(
            "Bad-Key".into(),
            table("kind = \"funding_carry\"\nuniverse = \"@crypto\"\ninterval = \"1h\"\nmin_apr_pct = 10\nhold_hours = 1"),
        );
        let e = c.validation_errors(true, &[], &["nyse"]);
        let has = |want: &str| e.iter().any(|m| m.contains(want));
        assert!(
            has("backtest.strategies.bad_kind: kind `grid_bot` is not one of"),
            "{e:?}"
        );
        assert!(
            has("backtest.strategies.bad_field: hold_hours must be within 1..=8760"),
            "{e:?}"
        );
        assert!(
            has("backtest.strategies.lost_universe: universe: no [backtest.universes] entry `xyz_stocks`"),
            "{e:?}"
        );
        // Every id in full; the excluded one needs no costs.
        assert!(
            has(
                "backtest.strategies.no_costs: no [backtest.costs] prefix matches \
                 robinhood:0x1234567890abcdef1234567890abcdef12345678, \
                 solana:So11111111111111111111111111111111111111112"
            ),
            "{e:?}"
        );
        assert!(
            has("backtest.strategies.weekend_fade: calendar `us_equity` is not an exchange [xmarket.calendars.us_equity] row"),
            "{e:?}"
        );
        assert!(has("backtest.strategies.Bad-Key: name `Bad-Key`"), "{e:?}");
        // The parser's own "strategy `<name>`:" prefix is not repeated.
        assert!(!has("strategy `bad_kind`"), "{e:?}");
        // The spec with its own costs and the pair with legs pass.
        assert!(!has("dex_move") && !has("sol_eth_spread"), "{e:?}");
    }
}
