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
//! | `splits."<full id>"` | — | share splits `[{ at = "<RFC 3339>", ratio = <new shares per old> }]`: a run adjusts that instrument's loaded bars closed before `at` (prices ÷ ratio, volume × ratio; ctx prices too; funding never), drops a bar straddling `at` (a day bar around an intraday split) and says so in the report's data notes |
//! | `gate` | — | the `[decision_loops.<name>]` the Jev gate arm runs by default |
//! | `max_candidates` | `50000` | a run whose candidates pass it stops before any arm or file, with an error naming the guard and the spec's knobs to narrow (1–1 000 000). 50 000 ≈ 4× the library's largest run (`xyz_funding_carry`, 13 000) and room for an hourly 50 bps fade on 75 names (30 330 candidates: a 47 MB run dir, 120 MB RSS); it refuses the always-in specs (0.01 bps hourly on 75 names: 244 688 candidates, a 392 MiB run dir, 876 MB RSS — now an error in under 2 s at 113 MB); a run at the cap writes ≈ 80 MB |
//! | `keep_runs` | `100` | run dirs kept under `<state dir>/backtests/`: writing a run prunes the oldest beyond it (by the id's UTC stamp, then suffix); never the run just written, the decision cache, a run the bound generation's lineage registry cites (`run:<state>/<run id>` — kept, not counted) or anything not named like a run id (rename a dir — `keep-<run id>` — to pin it); 0 = keep all, else ≥ 10. 100 ≈ 2.5 forty-round Architect turns or ≈ 2.5 days at the 2026-10-01 pace (41 runs a day): ≈ 0.3 GB at a typical 2.9 MB a run, ≤ ≈ 8 GB if every run sat at the `max_candidates` cap |
//!
//! | Load rule (`validation_errors`; a violation fails `Config::load`) | The error starts |
//! |---|---|
//! | every `strategies.<name>` parses and validates (`StrategySpec::from_value`: kind, fields, bounds) | `backtest.strategies.<name>: <field> …` |
//! | its `@<universe>` is a `universes` entry | `backtest.strategies.<name>: universe: …` |
//! | its `calendar` (`weekend_window`; `daily_window` with `days = "trading"`) is an exchange `[xmarket.calendars.<id>]` | `backtest.strategies.<name>: calendar …` |
//! | every instrument it trades (universe or named ids, minus `exclude`) has a `costs` prefix, unless the spec sets `costs` | `backtest.strategies.<name>: no [backtest.costs] prefix matches <ids, in full>` |
//! | every `splits` key is a full instrument id; each entry's `at` is RFC 3339, its `ratio` finite, > 0 and ≠ 1; entries sorted by `at`, each instant once (unknown fields refused) | `backtest.splits."<id>"` |
//! | `max_candidates` within 1–1 000 000; `keep_runs` 0 or ≥ 10 | `backtest.max_candidates` · `backtest.keep_runs` |
//! | a cost's `half_spread` takes only its model's keys (`domain/backtest/costs.rs`): a knob nested there by mistake fails the parse, naming it | the TOML error |

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::backtest::costs::{cost_for, CostSpec};
use crate::domain::backtest::spec::{parse_rfc3339, valid_name, StrategySpec, Universe};
use crate::domain::market::InstrumentId;
use crate::domain::marketdata::StockSplit;

/// One `[backtest.splits."<id>"]` entry (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SplitEntry {
    /// RFC 3339: the first instant at the new share count (bars opening
    /// before it are adjusted).
    pub at: String,
    /// New shares per old share: 3.0 = 3-for-1, 0.1 = 1-for-10 reverse.
    pub ratio: f64,
}

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
    /// Share splits by full instrument id (module table).
    #[serde(default)]
    pub splits: BTreeMap<String, Vec<SplitEntry>>,
    #[serde(default)]
    pub gate: Option<String>,
    /// A run whose candidates pass this stops before any arm or file
    /// (module table).
    #[serde(default = "default_max_candidates")]
    pub max_candidates: usize,
    /// Run dirs kept under `<state dir>/backtests/`: writing one prunes the
    /// oldest beyond it (0 = keep all); never the decision cache or a run the
    /// bound generation's lineage registry cites.
    #[serde(default = "default_keep_runs")]
    pub keep_runs: usize,
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
            splits: BTreeMap::new(),
            gate: None,
            max_candidates: default_max_candidates(),
            keep_runs: default_keep_runs(),
        }
    }
}

/// `max_candidates` bounds (module table).
pub const MAX_CANDIDATES_RANGE: std::ops::RangeInclusive<usize> = 1..=1_000_000;
/// The least non-zero `keep_runs`: a concurrent run's fresh dir is never
/// among the oldest.
pub const MIN_KEEP_RUNS: usize = 10;

/// 50 000: ≈ 4× the library's largest run (`xyz_funding_carry`, 13 000),
/// room for an hourly 50 bps fade on 75 names (30 330: a 47 MB run dir,
/// 120 MB RSS); refuses the always-in specs (0.01 bps hourly: 244 688
/// candidates, a 392 MiB run dir, 876 MB RSS — now an error in under 2 s at
/// 113 MB). A run at the cap writes ≈ 80 MB.
fn default_max_candidates() -> usize {
    50_000
}

/// 100: 2.5 forty-round Architect turns, or ≈ 2.5 days at the 2026-10-01
/// pace (41 runs a day); ≈ 0.3 GB at a typical 2.9 MB a run, ≤ ≈ 8 GB if
/// every run sat at the `max_candidates` cap (≈ 80 MB).
fn default_keep_runs() -> usize {
    100
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
        if !MAX_CANDIDATES_RANGE.contains(&self.max_candidates) {
            errors.push(format!(
                "backtest.max_candidates must be within {}..={}",
                MAX_CANDIDATES_RANGE.start(),
                MAX_CANDIDATES_RANGE.end()
            ));
        }
        if self.keep_runs != 0 && self.keep_runs < MIN_KEEP_RUNS {
            errors.push(format!(
                "backtest.keep_runs must be 0 (keep every run dir) or ≥ {MIN_KEEP_RUNS}"
            ));
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
        for (id, entries) in &self.splits {
            errors.extend(split_errors(id, entries));
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

    /// `[backtest.splits]` as `StockSplit`s by full id — the entries that
    /// pass the load rule (`Config::load` refuses the rest).
    pub fn stock_splits(&self) -> BTreeMap<String, Vec<StockSplit>> {
        self.splits
            .iter()
            .map(|(id, entries)| {
                let splits = entries
                    .iter()
                    .filter_map(|e| {
                        let s = StockSplit {
                            at_ms: parse_rfc3339(&e.at)?,
                            ratio: e.ratio,
                        };
                        s.is_valid().then_some(s)
                    })
                    .collect();
                (id.clone(), splits)
            })
            .collect()
    }
}

/// The load rule of one `splits."<id>"` list (module table).
fn split_errors(id: &str, entries: &[SplitEntry]) -> Vec<String> {
    let at = format!("backtest.splits.\"{id}\"");
    let mut errors = Vec::new();
    if let Err(e) = InstrumentId::parse(id) {
        errors.push(format!("{at}: {e}"));
    }
    if entries.is_empty() {
        errors.push(format!(
            "{at} lists no split: [{{ at = \"<RFC 3339>\", ratio = <new shares per old> }}]"
        ));
    }
    let mut last: Option<i64> = None;
    for (i, e) in entries.iter().enumerate() {
        match parse_rfc3339(&e.at) {
            None => errors.push(format!(
                "{at}[{i}].at `{}` is not RFC 3339 (2026-09-28T08:00:00Z)",
                e.at
            )),
            Some(t) => {
                if last.is_some_and(|l| t <= l) {
                    errors.push(format!(
                        "{at}[{i}].at `{}`: list the splits by `at`, each instant once",
                        e.at
                    ));
                }
                last = Some(t);
            }
        }
        let valid = StockSplit {
            at_ms: 0,
            ratio: e.ratio,
        }
        .is_valid();
        if !valid {
            errors.push(format!(
                "{at}[{i}].ratio must be finite, > 0 and ≠ 1 (new shares per old share: 3.0 = 3-for-1)"
            ));
        }
    }
    errors
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

    /// Regression (review: `[backtest.costs]` dropped a knob nested in
    /// `half_spread`): the load refuses it, naming the key.
    #[test]
    fn a_cost_knob_nested_in_half_spread_fails_the_load() {
        let e = toml::from_str::<BacktestConfig>(
            "[costs.\"hyperliquid:xyz:\"]\ntaker_fee_bps = 0.9\n\
             half_spread = { model = \"fixed\", bps = 1.0, slippage_bps = 500 }",
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("slippage_bps"), "{e}");
    }

    /// Regression (review: unbounded run output): the run guards load with
    /// their defaults — `max_candidates` 50 000, `keep_runs` 100 — and a
    /// value out of range is a load error naming the field.
    #[test]
    fn the_run_guards_load_with_their_defaults() {
        let c = parse("");
        assert_eq!((c.max_candidates, c.keep_runs), (50_000, 100));
        assert_eq!(BacktestConfig::default(), c);
        let c = parse("max_candidates = 1000\nkeep_runs = 0");
        assert_eq!((c.max_candidates, c.keep_runs), (1_000, 0));
        assert!(c.validation_errors(true, &[], &[]).is_empty());
        let c = parse("max_candidates = 0\nkeep_runs = 3");
        let e = c.validation_errors(true, &[], &[]);
        assert!(
            e.iter()
                .any(|m| m.starts_with("backtest.max_candidates must be within")),
            "{e:?}"
        );
        assert!(
            e.iter()
                .any(|m| m.starts_with("backtest.keep_runs must be 0")),
            "{e:?}"
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

    /// `[backtest.splits]`: the KIOXIA entry loads into a `StockSplit`; each
    /// load rule names the id (in full) and the entry.
    #[test]
    fn splits_load_and_their_rules_name_the_entry() {
        let c = parse(
            r#"
            [splits]
            "hyperliquid:xyz:KIOXIA" = [{ at = "2026-09-28T08:00:00Z", ratio = 3.0 }]
            "#,
        );
        assert!(c.validation_errors(true, &[], &[]).is_empty());
        assert_eq!(
            c.stock_splits(),
            BTreeMap::from([(
                "hyperliquid:xyz:KIOXIA".to_string(),
                vec![StockSplit {
                    at_ms: parse_rfc3339("2026-09-28T08:00:00Z").unwrap(),
                    ratio: 3.0
                }]
            )])
        );
        assert!(
            toml::from_str::<BacktestConfig>(
                "[splits]\n\"hyperliquid:xyz:KIOXIA\" = [{ at = \"2026-09-28T08:00:00Z\", ratio = 3.0, note = \"x\" }]"
            )
            .is_err(),
            "unknown fields are refused"
        );
        // (the list, the errors it must give)
        let cases: [(&str, &[&str]); 7] = [
            (
                r#""KIOXIA" = [{ at = "2026-09-28T08:00:00Z", ratio = 3.0 }]"#,
                &[r#"backtest.splits."KIOXIA": "#],
            ),
            (
                r#""hyperliquid:xyz:KIOXIA" = [{ at = "2026-09-28", ratio = 3.0 }]"#,
                &[r#"backtest.splits."hyperliquid:xyz:KIOXIA"[0].at `2026-09-28` is not RFC 3339"#],
            ),
            (
                r#""hyperliquid:xyz:KIOXIA" = [{ at = "2026-09-28T08:00:00Z", ratio = 1.0 }]"#,
                &[
                    r#"backtest.splits."hyperliquid:xyz:KIOXIA"[0].ratio must be finite, > 0 and ≠ 1"#,
                ],
            ),
            (
                r#""hyperliquid:xyz:KIOXIA" = [{ at = "2026-09-28T08:00:00Z", ratio = -3.0 }]"#,
                &["[0].ratio must be finite, > 0"],
            ),
            (
                r#""hyperliquid:xyz:KIOXIA" = [{ at = "2026-09-28T08:00:00Z", ratio = nan }]"#,
                &["[0].ratio must be finite"],
            ),
            (
                r#""hyperliquid:xyz:KIOXIA" = [
                    { at = "2026-09-28T08:00:00Z", ratio = 3.0 },
                    { at = "2026-03-02T14:30:00Z", ratio = 2.0 },
                    { at = "2026-03-02T14:30:00Z", ratio = 2.0 }]"#,
                &[
                    "[1].at `2026-03-02T14:30:00Z`: list the splits by `at`, each instant once",
                    "[2].at `2026-03-02T14:30:00Z`: list the splits by `at`",
                ],
            ),
            (
                r#""hyperliquid:xyz:KIOXIA" = []"#,
                &[r#"backtest.splits."hyperliquid:xyz:KIOXIA" lists no split"#],
            ),
        ];
        for (list, wants) in cases {
            let c = parse(&format!("[splits]\n{list}"));
            let e = c.validation_errors(true, &[], &[]);
            for want in wants {
                assert!(
                    e.iter().any(|m| m.contains(want)),
                    "want `{want}` for {list}: {e:?}"
                );
            }
            // What load refuses never reaches a run.
            assert!(c
                .stock_splits()
                .values()
                .flatten()
                .all(|s| s.is_valid() && s.at_ms > 0));
        }
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

    /// `[backtest]` of `sandboxes/<name>/config.toml`.
    fn sandbox_backtest(name: &str) -> BacktestConfig {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let path = root.join(format!("sandboxes/{name}/config.toml"));
        let text = std::fs::read_to_string(&path).unwrap();
        let v: toml::Value = toml::from_str(&text).unwrap();
        v["backtest"].clone().try_into().unwrap()
    }

    /// Phase 7 (`labels`, 2026-10-08) changes no existing spec's hash: every
    /// `[backtest.strategies]` spec of the frozen `sandboxes/xlab` keeps the
    /// `spec_sha256` it had before (computed at 3ba881791442698b556449fd00f3350f1edcb4f2;
    /// the nine W1 pins of `lineage/generations/W1.toml` among them), and so
    /// do xlab-w2's copies; its four labelled specs differ from their bases
    /// only in the `labels` table.
    #[test]
    fn spec_hashes_survive_the_labels_knob() {
        use crate::domain::backtest::spec::spec_sha256;
        let pinned = [
            (
                "crypto_funding_carry",
                "b75ab5262ffca5dbad4cfd4d5ce873417ccc7da134722a8a9f42bd9f3291748e",
            ),
            (
                "crypto_move_fade",
                "90b61efa4a5e1fa016e4155c154a086c01bb5151012af47556f0893a27d937ad",
            ),
            (
                "sol_eth_spread",
                "c499e65d940cb5ddde04e660b066f67c4ad4e1841a3862940602f17c75f7b174",
            ),
            (
                "weekend_fade",
                "e2b361ac710107d7317ed8223bf7f1da4cf74818cb0e9323a363014e35c30d14",
            ),
            (
                "weekend_fade_liquid",
                "dabdf28465aeae58204da189d435cf5dd2840308050330932eb40f9be4881a7f",
            ),
            (
                "weekend_fade_top4",
                "aee630b88c13216bb1c812eb53e71dbc2249331f58fa6a9471ab78bb9b4bdf56",
            ),
            (
                "weekend_follow",
                "61b7c7b896b1e9444821a796754065024879ce5a92639facd243d2e461cbcb4f",
            ),
            (
                "xyz_funding_carry",
                "3a95e2149ac0551064ff778a5f9545b5155e0e06dc35c63af30520a304d29165",
            ),
            (
                "xyz_overnight_follow",
                "051791688d07b49900bf10d8a43aad668f735a7bae1f266f155ae13dc9ec711b",
            ),
            (
                "xyz_weeknight_fade_top4",
                "ad0981422f850136d5e072460d8b2f46f91cfa92c2359fcf902218c2556542d2",
            ),
            (
                "xyz_weeknight_fade_top4_0400",
                "0a429b9a4e41b02ab4ba329d15f2848deee90d72f4b1d4ca7494d36c35ef07c2",
            ),
        ];
        let hash = |bt: &BacktestConfig, name: &str| {
            let s = bt
                .strategy(name)
                .unwrap_or_else(|e| panic!("{name}: {e:?}"));
            spec_sha256(&s.to_value())
        };
        let xlab = sandbox_backtest("xlab");
        assert_eq!(
            xlab.strategies
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            pinned.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            "every xlab strategy is pinned"
        );
        let w2 = sandbox_backtest("xlab-w2");
        for (name, sha) in pinned {
            assert_eq!(hash(&xlab, name), sha, "xlab {name}");
            assert_eq!(hash(&w2, name), sha, "xlab-w2's copy of {name}");
        }
        // The labelled four: the base's value + `labels`, nothing else.
        for (labelled, base, skip) in [
            ("weekend_fade_skip_news", "weekend_fade", vec!["NEWS"]),
            (
                "weekend_fade_noise_only",
                "weekend_fade",
                vec!["NEWS", "UNCERTAIN"],
            ),
            (
                "weekend_fade_top4_skip_news",
                "weekend_fade_top4",
                vec!["NEWS"],
            ),
            (
                "weekend_fade_top4_noise_only",
                "weekend_fade_top4",
                vec!["NEWS", "UNCERTAIN"],
            ),
        ] {
            let mut v = w2.strategy(labelled).unwrap().to_value();
            let labels = v.as_object_mut().unwrap().remove("labels").unwrap();
            assert_eq!(
                labels,
                serde_json::json!({"skip": skip, "lookback_mins": 240, "new_listing_days": 14}),
                "{labelled}"
            );
            v["name"] = Value::from(base);
            assert_eq!(v, w2.strategy(base).unwrap().to_value(), "{labelled}");
            assert_ne!(hash(&w2, labelled), hash(&w2, base));
        }
    }
}
