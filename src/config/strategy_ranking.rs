//! `[strategy_ranking]` — the strategy-ranking contracts a sandbox runs
//! (`lineage/rankings/<id>.toml`, `domain/lineage/ranking.rs`) and the
//! lineage registry they live in. Resolved once at `Config::load` into
//! `SandboxSections::ranking` ([`RankingSection`]); `deny_unknown_fields`.
//!
//! ```toml
//! [strategy_ranking]
//! registry = "../../lineage"              # relative to this config file
//! contracts = ["rank.xlab-w2.daily.v1"]   # ids under <registry>/rankings/
//! ```
//!
//! | Load rule ([`section_errors`]; a violation fails `Config::load`) | The error names |
//! |---|---|
//! | `contracts` not empty, no id twice | `strategy_ranking.contracts` |
//! | the registry loads | `strategy_ranking.registry`, each load problem |
//! | each contract exists | the id |
//! | the config is a `sandboxes/<name>/config.toml` and each contract's `sandbox` is `<name>` | the id, both sandboxes |
//! | `[backtest]` present and each contract strategy a `[backtest.strategies.<name>]` | the id, the strategy |
//! | no Error finding of the registry on the contract (its shape: `invalid_field` …) — `seal_mismatch` excepted: an unsealed (Warn) or changed contract is the publisher's refusal at run time, never a load failure of the sandbox | the code and message |
//!
//! [`RankingSection::cited_runs`]: every run dir the registry cites, by
//! state — run-dir retention keeps them in a sandbox bound to no generation
//! too (lineage D3, `application/backtest/mod.rs::cited_runs`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::lineage::{load_registry, relative_to_config};
use super::{paths, Config};
use crate::domain::lineage::registry::label;
use crate::domain::lineage::value::RecordKind;
use crate::domain::lineage::Severity;

/// `[strategy_ranking]` (module example).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StrategyRankingConfig {
    /// The lineage registry dir, relative to the config file's dir.
    pub registry: PathBuf,
    /// Ranking contract ids (`<registry>/rankings/<id>.toml`).
    pub contracts: Vec<String>,
}

/// `[strategy_ranking]` resolved (runtime only, never in TOML).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RankingSection {
    /// The registry dir, absolute: the publisher reloads it at run time (a
    /// seal appended after the process started counts).
    #[cfg_attr(not(test), allow(dead_code))] // read by the ranking coordinator
    pub registry_dir: PathBuf,
    /// The contract ids, as listed.
    #[cfg_attr(not(test), allow(dead_code))] // read by the ranking coordinator
    pub contracts: Vec<String>,
    /// Every run dir the registry cites (`Registry::cited_runs`), by state.
    pub cited_runs: BTreeMap<String, BTreeSet<String>>,
}

/// The module's load rules for `cfg` read from `path`, and the section when
/// the registry loads.
pub(crate) fn section_errors(cfg: &Config, path: &Path) -> (Vec<String>, Option<RankingSection>) {
    let Some(sr) = &cfg.strategy_ranking else {
        return (Vec::new(), None);
    };
    let mut errs = Vec::new();
    if sr.contracts.is_empty() {
        errs.push(
            "strategy_ranking.contracts: empty — list the ranking contracts (ids under \
             <registry>/rankings/) this sandbox runs"
                .to_string(),
        );
    }
    let mut seen = BTreeSet::new();
    for id in sr.contracts.iter().filter(|id| !seen.insert(id.as_str())) {
        errs.push(format!("strategy_ranking.contracts: `{id}` listed twice"));
    }
    let dir = relative_to_config(path, &sr.registry);
    let reg = match load_registry(&dir) {
        Ok(r) => r,
        Err(es) => {
            errs.extend(
                es.into_iter()
                    .map(|e| format!("strategy_ranking.registry {}: {e}", dir.display())),
            );
            return (errs, None);
        }
    };
    let sandbox = paths::sandbox_of_config_file(path);
    if sandbox.is_none() && !sr.contracts.is_empty() {
        errs.push(format!(
            "strategy_ranking: {} is no sandboxes/<name>/config.toml — a ranking contract names \
             its sandbox",
            path.display()
        ));
    }
    let findings = reg.validate();
    for id in &sr.contracts {
        let at = format!("strategy_ranking.contracts `{id}`");
        let Some(c) = reg.rankings.get(id) else {
            errs.push(format!(
                "{at}: no rankings/{id}.toml in the registry {}",
                dir.display()
            ));
            continue;
        };
        if let Some(s) = sandbox.as_deref().filter(|s| *s != c.sandbox) {
            errs.push(format!(
                "{at}: the contract is for sandbox `{}`, this config is sandbox `{s}`",
                c.sandbox
            ));
        }
        match &cfg.backtest {
            None => errs.push(format!(
                "{at}: no [backtest] here — the contract ranks [backtest.strategies.<name>]"
            )),
            Some(bt) => {
                for s in c
                    .strategies
                    .iter()
                    .filter(|s| !bt.strategies.contains_key(*s))
                {
                    errs.push(format!(
                        "{at}: strategy `{s}` is not in [backtest.strategies]"
                    ));
                }
            }
        }
        let mine = label(RecordKind::Ranking, id);
        for f in findings.iter().filter(|f| {
            f.severity == Severity::Error && f.record == mine && f.code != "seal_mismatch"
        }) {
            errs.push(format!("{at}: {} {}", f.code, f.message));
        }
    }
    let section = RankingSection {
        registry_dir: paths::absolute_path(&dir),
        contracts: sr.contracts.clone(),
        cited_runs: reg.cited_runs(),
    };
    (errs, Some(section))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use crate::config::Config;

    /// A ranked sandbox: rule W and its top-4 cut in `[backtest]`, the
    /// fixture contract `rank.fixture.v1` listed.
    const CONFIG: &str = r#"
runtime_profile = "auto"

[egress]
network = "open"
allow_hosts = ["api.hyperliquid.xyz"]

[memory]
enabled = false

[xmarket]
state = "strategy-ranking-test"

[xmarket.calendars.us_equity]
kind = "exchange"
tz = "America/New_York"
core = ["09:30", "16:00"]
pre = "04:00"
post = "20:00"
overnight = true
early_close = "13:00"
early_post = "17:00"
holidays = ["2026-11-26", "2026-12-25"]
early_closes = ["2026-11-27", "2026-12-24"]

[backtest]
notional_usd = 100
bootstrap = 200
seed = 7

[backtest.costs."hyperliquid:xyz:"]
taker_fee_bps = 0.9
half_spread = { model = "fixed", bps = 1.0 }

[backtest.strategies.rule_w]
kind = "weekend_window"
universe = ["hyperliquid:xyz:AAPL", "hyperliquid:xyz:TSLA"]
interval = "1h"
calendar = "us_equity"
direction = "fade"

[backtest.strategies.rule_w_top4]
kind = "weekend_window"
universe = ["hyperliquid:xyz:AAPL"]
interval = "1h"
calendar = "us_equity"
direction = "fade"

[agents.architect]
default = true
engine = "openrouter"
model = "anthropic/claude-opus-4.7"
tools = ["backtest", "read_file"]
description = "Ranks the fixture strategies."

[default_scopes.backtest]
fs_roots = ["~/strategy-ranking-test-ws"]

[strategy_ranking]
registry = "../../registry"
contracts = ["rank.fixture.v1"]
"#;

    fn copy_dir(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for e in std::fs::read_dir(from).unwrap().flatten() {
            let target = to.join(e.file_name());
            if e.path().is_dir() {
                copy_dir(&e.path(), &target);
            } else {
                std::fs::copy(e.path(), &target).unwrap();
            }
        }
    }

    /// `<tmp>/registry` = the lineage fixture registry, `<tmp>/sandboxes/
    /// <sandbox>/config.toml` = `edit(CONFIG)`.
    fn setup(sandbox: &str, edit: impl Fn(String) -> String) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/lineage/registry");
        copy_dir(&fixture, &tmp.path().join("registry"));
        let dir = tmp.path().join("sandboxes").join(sandbox);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, edit(CONFIG.to_string())).unwrap();
        (tmp, path)
    }

    fn load_err(path: &Path) -> String {
        match Config::load(path) {
            Ok(_) => panic!("{} loaded", path.display()),
            Err(e) => format!("{e:#}"),
        }
    }

    #[test]
    fn loads_and_names_an_unknown_contract() {
        let (tmp, path) = setup("ranked", |t| t);
        let cfg = Config::load(&path).unwrap_or_else(|e| panic!("{e:#}"));
        let section = cfg.ranking_section.clone().expect("resolved");
        assert_eq!(section.contracts, ["rank.fixture.v1"]);
        assert_eq!(
            section.registry_dir,
            std::fs::canonicalize(tmp.path().join("registry")).unwrap()
        );
        // The fixture registry's cited runs, for retention.
        assert!(
            section.cited_runs["xlab"].contains("20261001T120034Z-rule_w")
                && section.cited_runs["xlab"].contains("20261002T100000Z-rule_w_top4"),
            "{:?}",
            section.cited_runs
        );
        // Every agent's tools see the same section.
        let agent = &cfg.agents["architect"];
        assert!(Arc::ptr_eq(
            agent.sandbox.ranking.as_ref().unwrap(),
            &section
        ));
        // Unknown and listed twice.
        let (_tmp, path) = setup("ranked", |t| {
            t.replace(
                "contracts = [\"rank.fixture.v1\"]",
                "contracts = [\"rank.nope\", \"rank.fixture.v1\", \"rank.fixture.v1\"]",
            )
        });
        let e = load_err(&path);
        assert!(
            e.contains("strategy_ranking.contracts `rank.nope`: no rankings/rank.nope.toml"),
            "{e}"
        );
        assert!(
            e.contains("strategy_ranking.contracts: `rank.fixture.v1` listed twice"),
            "{e}"
        );
        // A registry that does not load.
        let (_tmp, path) = setup("ranked", |t| t.replace("../../registry", "../../none"));
        let e = load_err(&path);
        assert!(
            e.contains("strategy_ranking.registry") && e.contains("no registry directory"),
            "{e}"
        );
        // An unknown key in the section.
        let (_tmp, path) = setup("ranked", |t| format!("{t}cadence = \"daily\"\n"));
        assert!(load_err(&path).contains("cadence"));
    }

    #[test]
    fn refuses_a_contract_of_another_sandbox() {
        let (_tmp, path) = setup("other", |t| t);
        let e = load_err(&path);
        assert!(
            e.contains(
                "strategy_ranking.contracts `rank.fixture.v1`: the contract is for sandbox \
                 `ranked`, this config is sandbox `other`"
            ),
            "{e}"
        );
    }

    #[test]
    fn refuses_a_strategy_missing_from_the_library() {
        let (_tmp, path) = setup("ranked", |t| {
            t.replace(
                "[backtest.strategies.rule_w_top4]",
                "[backtest.strategies.top4]",
            )
        });
        let e = load_err(&path);
        assert!(
            e.contains(
                "strategy_ranking.contracts `rank.fixture.v1`: strategy `rule_w_top4` is not in \
                 [backtest.strategies]"
            ),
            "{e}"
        );
    }

    /// The commented `[strategy_ranking]` block of `config.example.toml`
    /// parses as the section.
    #[test]
    fn example_block_parses() {
        let text = include_str!("../../config.example.toml");
        let block: Vec<&str> = text
            .lines()
            .skip_while(|l| *l != "# [strategy_ranking]")
            .take_while(|l| l.starts_with('#'))
            .map(|l| l.strip_prefix("# ").unwrap_or(l.trim_start_matches('#')))
            .collect();
        #[derive(serde::Deserialize)]
        struct T {
            strategy_ranking: super::StrategyRankingConfig,
        }
        let t: T = toml::from_str(&block.join("\n")).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(t.strategy_ranking.registry, Path::new("../../lineage"));
        assert_eq!(t.strategy_ranking.contracts, ["rank.xlab-w2.daily.v1"]);
    }

    /// A contract's shape error fails the load; an edit after its seal
    /// (`seal_mismatch`) does not — the publisher refuses that one.
    #[test]
    fn a_shape_error_fails_the_load_a_changed_seal_does_not() {
        let (tmp, path) = setup("ranked", |t| t);
        let contract = tmp.path().join("registry/rankings/rank.fixture.v1.toml");
        let text = std::fs::read_to_string(&contract).unwrap();
        std::fs::write(&contract, text.replace("min_trades = 20", "min_trades = 5")).unwrap();
        Config::load(&path).unwrap_or_else(|e| panic!("{e:#}"));
        std::fs::write(&contract, text.replace("America/New_York", "Asia/Tokyo")).unwrap();
        let e = load_err(&path);
        assert!(
            e.contains(
                "strategy_ranking.contracts `rank.fixture.v1`: invalid_field tz: `Asia/Tokyo`"
            ),
            "{e}"
        );
    }
}
