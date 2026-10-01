//! `backtest` — the Architect's second tool (xlab, `docs/xlab-2026-10-01.md`
//! § 8): one strategy — a `[backtest.strategies]` name, or an inline spec
//! (the Architect's level-2 capability, § 5) — run on the sandbox's
//! warehouse `<state dir>/market.db` by the use case `application/backtest/`
//! as `tengu backtest` runs it, without the Jev gate (it spends money: the
//! CLI's `--gate`) and without a fetch (the Architect backfills first with
//! `market_history` `fetch = true`). No network, no LLM.
//!
//! | Step | Rule |
//! |---|---|
//! | Refuse | no `[xmarket]` ⇒ `state_dir_missing`; `market.db` not openable ⇒ `market_data_unavailable`; no `[backtest]` ⇒ `backtest_config_missing` |
//! | Args (strict) | `strategy` xor `spec`; `from` / `to` (epoch ms, RFC 3339 or a UTC date; default the earliest stored bar of the run's instruments / now); `split` (`time:<t>` \| `instruments:<id,…>`) — an unknown key, a wrong type or a bad split is an error: nothing runs |
//! | Spec | an object (or a string holding one), named by its `name`, else [`ARCHITECT_SPEC`]; refused before any read with every problem at once — `StrategySpec::from_value`'s, then an unknown `@universe` and a calendar that is no exchange `[xmarket.calendars]` row — one line each, so the Architect fixes them in one go; an unknown `strategy` lists the library |
//! | Run | `prepare` (series from `market.db`, read only; candidates) → `evaluate` (`research`, + `capped` with `[risk]` + `[paper]`) → `write_run_dir` (`<state dir>/backtests/<run id>/`), the last two on a blocking thread (the bootstrap is CPU) |
//! | Row | `backtest/1:<run id>` (`domain/backtest/report.rs`), ttl 0: every call is a new run, never cached (recorded when `[recorder]` takes `backtest/1`) |
//! | Text | [`render`]: line 1, features and errors as `render_text` gives them (no `data`: the report is in the run dir), the arm / split / instrument / skip lines of `render_compact`, the decision window, the run's instruments without a cost entry (`no_costs`: full ids, at most [`NO_COST_CHARS`] of them, then a count), `spec_sha256`, the run dir — ≈ 2 KB on the 75-name library, ≈ 4.5 KB crowded (the cap test below), whole under a 16k local model's 8 192-char cap |
//!
//! The run id embeds the UTC second of the call (`run_dir.rs`): two runs
//! never share a dir, and the conformance harness normalises the stamp like
//! any other time (`tests/bridge_conformance.rs::normalize`).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use serde_json::Value;

use super::{defs, field, object_args, opt_str, opt_time, XlabShared};
use crate::adapters::outbound::market_data::market_state_dir;
use crate::adapters::outbound::tools::hyperliquid::store_live;
use crate::application::backtest::{
    evaluate, prepare, spec_of, write_run_dir, BacktestEnv, BacktestJob, BacktestRun, Prepared,
    SpecSource,
};
use crate::config::backtest::BacktestConfig;
use crate::config::sections::SandboxSections;
use crate::config::xmarket::backtests_dir;
use crate::domain::backtest::costs::cost_for;
use crate::domain::backtest::spec::SplitSpec;
use crate::domain::calendar::Calendar;
use crate::domain::marketdata::fmt_time;
use crate::domain::message::ToolDef;
use crate::domain::observation::{now_ms, ObsSource, Observation};
use crate::domain::tools as names;
use crate::ports::tool::{Tool, ToolCtx, ToolOutput};

/// The name an inline spec without its own `name` runs under.
pub(crate) const ARCHITECT_SPEC: &str = "architect_spec";
/// Refusal without `[backtest]`.
pub(crate) const BACKTEST_CONFIG_MISSING: &str = "backtest_config_missing";
/// Chars of ids the `no_costs` line lists before "… and N more".
pub(crate) const NO_COST_CHARS: usize = 1_200;

const ARGS: &[&str] = &["strategy", "spec", "from", "to", "split"];

pub(crate) fn tools(shared: &XlabShared) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(BacktestTool {
        def: defs::def(names::BACKTEST),
        shared: shared.clone(),
    })]
}

pub(crate) struct BacktestTool {
    def: ToolDef,
    shared: XlabShared,
}

#[async_trait]
impl Tool for BacktestTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        ctx.scope.check_fs_write(ctx.workspace)?;
        let store = self.shared.market_arc()?;
        let sections = Arc::clone(&self.shared.sandbox);
        let bt = backtest_section(&sections)?;
        let job = parse_args(args, bt)?;
        check_spec(bt, &sections, &job.spec)?;
        let now = now_ms();
        let env = BacktestEnv {
            store,
            backtests_dir: backtests_dir(market_state_dir(&sections)?),
            sections: Arc::clone(&sections),
            now_ms: now,
        };
        let ran = run(&env, job).await?;
        let obs = Observation::of(names::BACKTEST, &ran.run.report, now, 0, ObsSource::Live);
        store_live(self.shared.store.as_deref(), &obs).await;
        Ok(ToolOutput {
            text: render(&obs, &ran, bt, now),
            observation: Some(obs),
        })
    }
}

/// `[backtest]`, or the refusal (module table).
fn backtest_section(sections: &SandboxSections) -> Result<&BacktestConfig> {
    sections.backtest.as_ref().ok_or_else(|| {
        anyhow!(
            "{BACKTEST_CONFIG_MISSING}: backtests unavailable: no [backtest] section — add \
             [backtest] to the sandbox config: costs per venue prefix, universes and the \
             strategy library (docs/xlab-2026-10-01.md § 5, § 9)"
        )
    })
}

/// The library's strategy names, for messages.
fn library(bt: &BacktestConfig) -> String {
    if bt.strategies.is_empty() {
        "no strategy".to_string()
    } else {
        bt.strategies
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

// ── Arguments (strict) ─────────────────────────────────────────────

/// The job `args` asks for (module table); `bt` names the library in errors.
pub(crate) fn parse_args(args: &Value, bt: &BacktestConfig) -> Result<BacktestJob> {
    let tool = names::BACKTEST;
    let o = object_args(tool, args, ARGS)?;
    let strategy = opt_str(tool, o, "strategy")?;
    let spec = match field(o, "spec") {
        None => None,
        Some(v @ Value::Object(_)) => Some(v.clone()),
        // A model may send the object as JSON text.
        Some(Value::String(s)) => match serde_json::from_str::<Value>(s) {
            Ok(v @ Value::Object(_)) => Some(v),
            _ => bail!(
                "{tool}: 'spec' must be a strategy spec object (kind, universe, interval, the \
                 kind's parameters); a library name goes in 'strategy'"
            ),
        },
        Some(v) => bail!("{tool}: 'spec' must be a strategy spec object, got {v}"),
    };
    let spec = match (strategy, spec) {
        (Some(_), Some(_)) => bail!("{tool}: give 'strategy' or 'spec', not both"),
        (None, None) => bail!(
            "{tool}: give 'strategy' (the sandbox has: {}) or 'spec' (a strategy spec object)",
            library(bt)
        ),
        (Some(name), None) => SpecSource::Strategy(name.to_string()),
        (None, Some(value)) => SpecSource::Json {
            value,
            fallback_name: Some(ARCHITECT_SPEC.to_string()),
        },
    };
    let split = opt_str(tool, o, "split")?
        .map(|s| SplitSpec::parse(s).map_err(|e| anyhow!("{tool}: 'split': {e}")))
        .transpose()?;
    Ok(BacktestJob {
        spec,
        from_ms: opt_time(tool, o, "from")?,
        to_ms: opt_time(tool, o, "to")?,
        split,
    })
}

/// Every problem of the job's spec before any read (module table):
/// `spec_of`'s, then — for a spec that parses — its universe and calendar.
pub(crate) fn spec_problems(
    bt: &BacktestConfig,
    sections: &SandboxSections,
    src: &SpecSource,
) -> Vec<String> {
    let spec = match spec_of(bt, src) {
        Ok(spec) => spec,
        Err(problems) => return problems,
    };
    let at = format!("strategy `{}`", spec.name);
    let mut out = Vec::new();
    if let Err(e) = bt.spec_instruments(&spec) {
        out.push(format!("{at}: universe: {e}"));
    }
    if let Some(cal) = spec.calendar() {
        if sections
            .calendars
            .get(cal)
            .and_then(Calendar::exchange)
            .is_none()
        {
            let known: Vec<&str> = sections
                .calendars
                .iter()
                .filter(|(_, c)| c.exchange().is_some())
                .map(|(id, _)| id.as_str())
                .collect();
            out.push(format!(
                "{at}: calendar `{cal}` is not an exchange [xmarket.calendars.{cal}] row (the \
                 sandbox has: {})",
                if known.is_empty() {
                    "none".to_string()
                } else {
                    known.join(", ")
                }
            ));
        }
    }
    out
}

/// [`spec_problems`] as the tool's error, or nothing.
fn check_spec(bt: &BacktestConfig, sections: &SandboxSections, src: &SpecSource) -> Result<()> {
    let tool = names::BACKTEST;
    let problems = spec_problems(bt, sections, src);
    match (problems.is_empty(), src) {
        (true, _) => Ok(()),
        (false, SpecSource::Strategy(_)) => bail!("{tool}: {}", problems.join("\n")),
        (false, SpecSource::Json { .. }) => bail!(
            "{tool}: spec refused — {} problem(s), fix each and call again:\n{}",
            problems.len(),
            problems.join("\n")
        ),
    }
}

// ── Run ────────────────────────────────────────────────────────────

/// One run of the tool: the report and arms, its dir, and the ids it read
/// that have no cost entry.
#[derive(Debug)]
pub(crate) struct Ran {
    pub run: BacktestRun,
    pub dir: PathBuf,
    /// Loaded ids without a cost (spec `costs` or a `[backtest.costs]`
    /// prefix): skipped as `no_costs`, never traded. Sorted, in full.
    pub no_costs: Vec<String>,
}

/// The run's ids no cost covers (module table).
fn no_cost_ids(p: &Prepared) -> Vec<String> {
    if p.spec.costs.is_some() {
        return Vec::new();
    }
    p.instruments
        .iter()
        .filter(|id| cost_for(&p.params.costs, id).is_none())
        .cloned()
        .collect()
}

/// `prepare` → `evaluate` → `write_run_dir` (module table); every error
/// starts `backtest: `.
pub(crate) async fn run(env: &BacktestEnv, job: BacktestJob) -> Result<Ran> {
    let tool = names::BACKTEST;
    let prepared = prepare(env, job)
        .await
        .map_err(|e| anyhow!("{tool}: {e:#}"))?;
    let no_costs = no_cost_ids(&prepared);
    let done = tokio::task::spawn_blocking(move || -> Result<(BacktestRun, PathBuf)> {
        let mut run = evaluate(&prepared, Vec::new())?;
        let dir = write_run_dir(&prepared, &mut run)?;
        Ok((run, dir))
    })
    .await
    .map_err(|e| anyhow!("{tool}: the run stopped: {e}"))?;
    let (run, dir) = done.map_err(|e| anyhow!("{tool}: {e:#}"))?;
    Ok(Ran { run, dir, no_costs })
}

// ── Text ───────────────────────────────────────────────────────────

/// The `no_costs` line: ids in full, as many as fit [`NO_COST_CHARS`], then
/// how many more.
fn no_costs_line(ids: &[String], bt: &BacktestConfig) -> String {
    let mut shown = String::new();
    let mut listed = 0;
    for id in ids {
        if listed > 0 && shown.len() + 2 + id.len() > NO_COST_CHARS {
            break;
        }
        if listed > 0 {
            shown.push_str(", ");
        }
        shown.push_str(id);
        listed += 1;
    }
    if listed < ids.len() {
        shown.push_str(&format!(
            " … and {} more (skips.json in the run dir)",
            ids.len() - listed
        ));
    }
    let prefixes: Vec<&str> = bt.costs.keys().map(String::as_str).collect();
    format!(
        "no_costs: {} instrument(s) have no cost entry and never trade — no [backtest.costs] \
         prefix matches (prefixes: {}) and the spec sets no costs: {shown} — give the spec its \
         own costs (taker_fee_bps, half_spread, slippage_bps, funding)",
        ids.len(),
        if prefixes.is_empty() {
            "none".to_string()
        } else {
            prefixes.join(", ")
        }
    )
}

/// The tool's text (module table).
pub(crate) fn render(obs: &Observation, ran: &Ran, bt: &BacktestConfig, now_ms: i64) -> String {
    let mut head = obs.clone();
    head.data = Value::Null;
    let mut lines = vec![head.render_text(now_ms)];
    let report = &ran.run.report;
    // Its line 1 is the row's headline, already in line 1 above.
    let compact = report.render_compact();
    lines.extend(compact.lines().skip(1).map(str::to_string));
    lines.push(format!(
        "decisions {} → {} (to exclusive) · {} instrument(s) · {} candidate(s)",
        fmt_time(report.from_ms),
        fmt_time(report.to_ms),
        report.n_instruments,
        report.n_candidates
    ));
    if !ran.no_costs.is_empty() {
        lines.push(no_costs_line(&ran.no_costs, bt));
    }
    lines.push(format!("spec_sha256 {}", report.spec_sha256));
    lines.push(format!("run dir: {}", ran.dir.display()));
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;

    use serde_json::json;

    use super::*;
    use crate::adapters::outbound::market_data::SqliteMarketData;
    use crate::adapters::outbound::tools::workspace::test_support::TestHarness;
    use crate::adapters::outbound::tools::xm::exec_common::tests::{paper, risk};
    use crate::domain::backtest::report::BacktestReport;
    use crate::domain::backtest::testkit::{nyse, random_market, utc};
    use crate::domain::marketdata::Interval;
    use crate::domain::observation::{assert_features_ok, MAX_FEATURES};
    use crate::domain::scope::ToolScope;
    use crate::domain::token::tool_result_char_budget;
    use crate::ports::market_data::MarketDataStore;

    const AAA: &str = "hyperliquid:xyz:AAA";
    const BBB: &str = "hyperliquid:xyz:BBB";
    const MINT: &str = "So11111111111111111111111111111111111111112";

    /// `[backtest]`: xyz costs, a 2-name universe, rule W.
    const BACKTEST_TOML: &str = r#"
        notional_usd = 100
        bootstrap = 200
        seed = 7
        [costs."hyperliquid:xyz:"]
        taker_fee_bps = 0.9
        half_spread = { model = "fixed", bps = 1.0 }
        [universes]
        xyz = ["hyperliquid:xyz:AAA", "hyperliquid:xyz:BBB"]
        [strategies.weekend_fade]
        kind = "weekend_window"
        universe = "@xyz"
        interval = "1h"
        calendar = "us_equity"
        direction = "fade"
    "#;

    fn backtest_config() -> BacktestConfig {
        toml::from_str(BACKTEST_TOML).unwrap()
    }

    /// `[xmarket]` (state dir `state`), NYSE as `us_equity`, `[backtest]`,
    /// the $100 `[risk]` / `[paper]` book ($25 orders).
    fn sections(state: &Path) -> SandboxSections {
        SandboxSections {
            xm_state_dir: Some(state.to_path_buf()),
            risk: Some(risk(&state.join("KILL"), 25)),
            paper: Some(paper()),
            calendars: BTreeMap::from([("us_equity".to_string(), Calendar::Exchange(nyse()))]),
            backtest: Some(backtest_config()),
            ..Default::default()
        }
    }

    /// 30 days of seeded random hourly bars + funding for `AAA` / `BBB`
    /// from 2026-09-01 (four weekends, jumps a move trigger sees).
    async fn seeded(state: &Path) -> Arc<dyn MarketDataStore> {
        let store = SqliteMarketData::open(state).unwrap();
        let md = random_market(&[AAA, BBB], utc("2026-09-01 00:00"), 30 * 24, 11);
        for (id, s) in &md.bars {
            store
                .put_bars(id, Interval::H1, "test", &s.bars)
                .await
                .unwrap();
        }
        for (id, f) in &md.funding {
            store.put_funding(id, "test", &f.points).await.unwrap();
        }
        Arc::new(store)
    }

    fn tool(market: Result<Arc<dyn MarketDataStore>, String>, s: SandboxSections) -> BacktestTool {
        BacktestTool {
            def: defs::def(names::BACKTEST),
            shared: XlabShared {
                market,
                store: None,
                sandbox: Arc::new(s),
            },
        }
    }

    fn job(v: Value) -> Result<BacktestJob> {
        parse_args(&v, &backtest_config())
    }

    #[test]
    fn arguments_parse_strictly() {
        let j = job(json!({"strategy": "weekend_fade", "from": "2026-09-01",
                           "to": 1_790_000_000_000_i64, "split": "time:2026-09-15"}))
        .unwrap();
        assert_eq!(j.spec, SpecSource::Strategy("weekend_fade".into()));
        assert_eq!(
            (j.from_ms, j.to_ms),
            (Some(utc("2026-09-01 00:00")), Some(1_790_000_000_000))
        );
        assert_eq!(j.split, Some(SplitSpec::Time(utc("2026-09-15 00:00"))));
        // A spec object, or JSON text holding one; named architect_spec
        // when it has no name.
        let spec = json!({"kind": "move_trigger", "universe": [AAA], "interval": "1h"});
        for v in [spec.clone(), Value::String(spec.to_string())] {
            let j = job(json!({"spec": v})).unwrap();
            assert_eq!(
                j.spec,
                SpecSource::Json {
                    value: spec.clone(),
                    fallback_name: Some(ARCHITECT_SPEC.into())
                }
            );
            assert_eq!((j.from_ms, j.to_ms, j.split), (None, None, None));
        }
        let split = job(json!({"strategy": "x", "split": format!("instruments:{AAA},{BBB}")}))
            .unwrap()
            .split;
        assert_eq!(
            split,
            Some(SplitSpec::Instruments(vec![AAA.into(), BBB.into()]))
        );
        for (v, needle) in [
            (json!([]), "arguments must be a JSON object"),
            (
                json!({"strategy": "x", "universe": "@xyz"}),
                "unknown argument(s) [\"universe\"]",
            ),
            (json!({}), "give 'strategy' (the sandbox has: weekend_fade)"),
            (json!({"strategy": "x", "spec": {"kind": "x"}}), "not both"),
            (
                json!({"strategy": 5}),
                "'strategy' must be a non-empty string",
            ),
            (
                json!({"spec": [1]}),
                "'spec' must be a strategy spec object",
            ),
            (
                json!({"spec": "weekend_fade"}),
                "a library name goes in 'strategy'",
            ),
            (
                json!({"strategy": "x", "from": "friday"}),
                "'from': `friday` is not epoch ms",
            ),
            (
                json!({"strategy": "x", "to": true}),
                "'to' must be epoch ms",
            ),
            (
                json!({"strategy": "x", "split": "holdout:x"}),
                "'split': split `holdout:x`",
            ),
        ] {
            let e = job(v.clone()).unwrap_err().to_string();
            assert!(e.starts_with("backtest: "), "{e}");
            assert!(e.contains(needle), "{v}: {e}");
        }
    }

    /// A bad spec is refused before any read with every problem at once.
    #[test]
    fn spec_problems_are_all_named() {
        let dir = tempfile::tempdir().unwrap();
        let s = sections(dir.path());
        let bt = backtest_config();
        let src = |v: Value| SpecSource::Json {
            value: v,
            fallback_name: Some(ARCHITECT_SPEC.into()),
        };
        let bad = src(
            json!({"kind": "move_trigger", "universe": "@xyz", "interval": "1h",
            "lookback_bars": 0, "threshold_bps": 0, "direction": "fade", "hold_bars": 0}),
        );
        let p = spec_problems(&bt, &s, &bad);
        assert_eq!(p.len(), 3, "{p:?}");
        for want in [
            "strategy `architect_spec`: lookback_bars must be within 1..=10000",
            "strategy `architect_spec`: threshold_bps must be finite, > 0",
            "strategy `architect_spec`: hold_bars must be within 1..=10000",
        ] {
            assert!(p.iter().any(|x| x.starts_with(want)), "{want}: {p:?}");
        }
        let e = check_spec(&bt, &s, &bad).unwrap_err().to_string();
        assert!(
            e.starts_with("backtest: spec refused — 3 problem(s), fix each and call again:\n"),
            "{e}"
        );
        // A spec that parses: an unknown universe and a calendar that is no
        // exchange row, both named; its own name is kept.
        let lost = src(
            json!({"name": "lost", "kind": "weekend_window", "universe": "@nope",
            "interval": "1h", "calendar": "lse", "direction": "fade"}),
        );
        let p = spec_problems(&bt, &s, &lost);
        assert_eq!(
            p,
            vec![
                "strategy `lost`: universe: no [backtest.universes] entry `nope`".to_string(),
                "strategy `lost`: calendar `lse` is not an exchange [xmarket.calendars.lse] row \
                 (the sandbox has: us_equity)"
                    .to_string(),
            ]
        );
        // Unknown fields are refused by name (deny_unknown_fields).
        let typo = src(
            json!({"kind": "funding_carry", "universe": [AAA], "interval": "1h",
            "min_apr": 50, "hold_hours": 24}),
        );
        let p = spec_problems(&bt, &s, &typo).join("\n");
        assert!(p.contains("unknown field `min_apr`"), "{p}");
        // A library name: the library is listed.
        let e = check_spec(&bt, &s, &SpecSource::Strategy("nope".into()))
            .unwrap_err()
            .to_string();
        assert!(
            e.starts_with("backtest: no [backtest.strategies.nope]; the sandbox has: weekend_fade"),
            "{e}"
        );
        assert!(check_spec(&bt, &s, &SpecSource::Strategy("weekend_fade".into())).is_ok());
    }

    /// A named strategy and an inline spec, end to end through `execute`:
    /// the row, the run dir, the text.
    #[tokio::test]
    async fn a_library_strategy_and_an_inline_spec_run() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let t = tool(Ok(seeded(&state).await), sections(&state));
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let h = TestHarness::new(&ws);
        let window = |mut v: Value| {
            v["from"] = json!("2026-09-01");
            v["to"] = json!("2026-09-29T00:00:00Z");
            v
        };

        let out = t
            .execute(&window(json!({"strategy": "weekend_fade"})), &h.ctx())
            .await
            .unwrap();
        let obs = out.observation.unwrap();
        let report: BacktestReport = obs.typed().unwrap();
        assert_eq!(obs.key, format!("backtest/1:{}", report.run_id));
        assert!(
            report.run_id.ends_with("Z-weekend_fade"),
            "{}",
            report.run_id
        );
        assert_eq!(report.run_id.len(), "20261001T120034Z-weekend_fade".len());
        assert_eq!(obs.ttl_ms, 0);
        assert_features_ok(&obs.features);
        assert!(obs.features.len() <= MAX_FEATURES);
        let research = &report.arms["research"].summary;
        assert!(research.n > 0, "{:?}", report.skipped);
        assert_eq!(obs.features["n_trades"], json!(research.n));
        assert!(report.arms.contains_key("capped"));
        let dir = state.join("backtests").join(&report.run_id);
        for f in [
            "report.json",
            "report.md",
            "trades-research.jsonl",
            "trades-capped.jsonl",
        ] {
            assert!(dir.join(f).is_file(), "{f}");
        }
        let text = &out.text;
        let lines: Vec<&str> = text.lines().collect();
        assert!(
            lines[0].starts_with(&format!(
                "backtest {} weekend_fade weekend_window 1h research n={} ",
                report.run_id, research.n
            )),
            "{text}"
        );
        assert!(lines[0].contains(" | "), "status suffix: {}", lines[0]);
        assert!(lines[1].contains("n_trades="), "features: {}", lines[1]);
        assert!(
            text.contains("\nresearch n=") && text.contains("\ncapped n="),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "\ndecisions 2026-09-01T00:00:00Z → 2026-09-29T00:00:00Z (to exclusive) · 2 \
                 instrument(s) · {} candidate(s)",
                report.n_candidates
            )),
            "{text}"
        );
        assert!(
            text.contains(&format!("\nspec_sha256 {}", report.spec_sha256)),
            "{text}"
        );
        assert!(
            text.ends_with(&format!("\nrun dir: {}", dir.display())),
            "{text}"
        );
        assert!(!text.contains("no_costs:"), "{text}");
        assert!(
            !text.contains("\"spec\""),
            "no data JSON in the text: {text}"
        );

        // An inline spec without a name: architect_spec; a move trigger on
        // the same bars, with a split by instrument.
        let spec = json!({"kind": "move_trigger", "universe": "@xyz", "interval": "1h",
            "lookback_bars": 1, "threshold_bps": 150, "direction": "fade", "hold_bars": 3});
        let out = t
            .execute(
                &window(json!({"spec": spec, "split": format!("instruments:{BBB}")})),
                &h.ctx(),
            )
            .await
            .unwrap();
        let report: BacktestReport = out.observation.unwrap().typed().unwrap();
        assert!(
            report.run_id.ends_with("Z-architect_spec"),
            "{}",
            report.run_id
        );
        assert_eq!(report.kind, "move_trigger");
        assert!(report.arms["research"].summary.n > 0);
        assert!(
            out.text
                .contains(&format!("\nsplit instruments:{BBB} research: in-sample n=")),
            "{}",
            out.text
        );
    }

    /// The run's ids without a cost entry are named in full in the text.
    #[tokio::test]
    async fn instruments_without_costs_are_named() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let t = tool(Ok(seeded(&state).await), sections(&state));
        let h = TestHarness::new(tmp.path());
        let mint = format!("solana:{MINT}");
        let spec = json!({"name": "mixed", "kind": "move_trigger", "universe": [AAA, mint],
            "interval": "1h", "lookback_bars": 1, "threshold_bps": 150, "direction": "fade",
            "hold_bars": 3});
        let out = t
            .execute(
                &json!({"spec": spec, "from": "2026-09-02", "to": "2026-09-20"}),
                &h.ctx(),
            )
            .await
            .unwrap();
        let line = out
            .text
            .lines()
            .find(|l| l.starts_with("no_costs: "))
            .unwrap_or_else(|| panic!("{}", out.text));
        assert_eq!(
            line,
            format!(
                "no_costs: 1 instrument(s) have no cost entry and never trade — no \
                 [backtest.costs] prefix matches (prefixes: hyperliquid:xyz:) and the spec sets \
                 no costs: {mint} — give the spec its own costs (taker_fee_bps, half_spread, \
                 slippage_bps, funding)"
            )
        );
        let report: BacktestReport = out.observation.unwrap().typed().unwrap();
        assert_eq!(report.skipped.get("no_costs"), Some(&1));
        // Its own costs: nothing to name.
        let mut own = spec.clone();
        own["costs"] = json!({"taker_fee_bps": 5.0, "funding": false});
        own["name"] = json!("mixed_costed");
        let out = t
            .execute(
                &json!({"spec": own, "from": "2026-09-02", "to": "2026-09-20"}),
                &h.ctx(),
            )
            .await
            .unwrap();
        assert!(!out.text.contains("no_costs:"), "{}", out.text);
    }

    /// The worst case for a local model: the tool's two arms crowded (a
    /// split, refusals and drops of every kind, every skip reason, data gaps
    /// in both arms, many notes) and 1 000 long ids without costs — the text
    /// stays well inside the 16k window's cap (8 192 chars), ids whole.
    #[tokio::test]
    async fn the_text_fits_a_local_models_result_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let t = tool(Ok(seeded(&state).await), sections(&state));
        let h = TestHarness::new(tmp.path());
        let out = t
            .execute(
                &json!({"strategy": "weekend_fade", "from": "2026-09-01",
                        "to": "2026-09-29", "split": "time:2026-09-15"}),
                &h.ctx(),
            )
            .await
            .unwrap();
        let obs = out.observation.unwrap();
        let mut report: BacktestReport = obs.typed().unwrap();
        assert_eq!(report.arms.len(), 2, "the tool runs research + capped");
        report.data_notes = (0..500).map(|i| format!("note {i}")).collect();
        let reasons = [
            "excluded",
            "missing_anchor",
            "missing_entry",
            "missing_price",
            "flat",
            "below_min_signal",
            "not_top_n",
            "no_costs",
            "missing_exit",
            "future_data",
        ];
        report.skipped = reasons.iter().map(|r| (r.to_string(), 12_345)).collect();
        for arm in report.arms.values_mut() {
            arm.skipped = reasons.iter().map(|r| (r.to_string(), 1_234)).collect();
            arm.refusals = [
                "max_gross_exposure_usd",
                "max_net_exposure_usd",
                "daily_loss_limit_usd",
                "total_loss_limit_usd",
            ]
            .iter()
            .map(|r| (r.to_string(), 999))
            .collect();
            arm.summary.funding_incomplete = 77;
        }
        let ids: Vec<String> = (0..1_000)
            .map(|i| format!("robinhood:0x{i:040x}"))
            .collect();
        let dir = state
            .join("a-long-tengu-home-path-of-an-operator/state/xlab/backtests")
            .join(&report.run_id);
        let obs = Observation::of(names::BACKTEST, &report, 0, 0, ObsSource::Live);
        let ran = Ran {
            run: BacktestRun {
                report,
                arms: BTreeMap::new(),
                extra_files: BTreeMap::new(),
            },
            dir,
            no_costs: ids.clone(),
        };
        let text = render(&obs, &ran, &backtest_config(), 0);
        let cap = tool_result_char_budget(16_384);
        assert!(
            text.chars().count() < cap * 3 / 4,
            "{} chars of a {cap} cap:\n{text}",
            text.chars().count()
        );
        let line = text.lines().find(|l| l.starts_with("no_costs: ")).unwrap();
        assert!(line.contains(&ids[0]) && line.contains(&ids[20]), "{line}");
        assert!(line.contains(" more (skips.json in the run dir)"), "{line}");
        // Every id the line shows is whole.
        for id in line
            .split([',', ' '])
            .filter(|w| w.starts_with("robinhood:"))
        {
            assert!(ids.contains(&id.to_string()), "cut id {id}");
        }
        assert!(text.ends_with(&format!("run dir: {}", ran.dir.display())));
    }

    #[tokio::test]
    async fn execute_gates_scope_and_refuses_without_its_sections() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let store = seeded(&state).await;
        let call = json!({"strategy": "weekend_fade", "from": "2026-09-01", "to": "2026-09-29"});
        // The workspace is the scope's gate (the observation store).
        let t = tool(Ok(Arc::clone(&store)), sections(&state));
        let denied = TestHarness::with_scope(tmp.path(), ToolScope::default());
        assert!(t.execute(&call, &denied.ctx()).await.is_err());
        let h = TestHarness::new(tmp.path());
        // No [xmarket]: refused before anything else.
        let refused = tool(
            super::super::open_market(&SandboxSections::default()),
            SandboxSections::default(),
        );
        let e = refused.execute(&call, &h.ctx()).await.unwrap_err();
        assert!(e.to_string().starts_with("state_dir_missing: "), "{e}");
        // No [backtest]: refused, nothing written.
        let mut no_bt = sections(&state);
        no_bt.backtest = None;
        let e = tool(Ok(Arc::clone(&store)), no_bt)
            .execute(&call, &h.ctx())
            .await
            .unwrap_err()
            .to_string();
        assert!(
            e.starts_with("backtest_config_missing: backtests unavailable: no [backtest] section"),
            "{e}"
        );
        assert!(!state.join("backtests").exists());
        // A run that cannot start says why (no 4h bars), nothing written.
        let spec = json!({"name": "slow", "kind": "move_trigger", "universe": "@xyz",
            "interval": "4h", "lookback_bars": 1, "threshold_bps": 150, "direction": "fade",
            "hold_bars": 3});
        let e = t
            .execute(&json!({"spec": spec}), &h.ctx())
            .await
            .unwrap_err()
            .to_string();
        assert!(
            e.starts_with("backtest: market.db holds no 4h bars for the 2 instrument(s) of `slow`"),
            "{e}"
        );
        assert!(!state.join("backtests").exists());
        // A bad spec: refused before any read.
        let e = t
            .execute(&json!({"spec": {"kind": "grid_bot"}}), &h.ctx())
            .await
            .unwrap_err()
            .to_string();
        assert!(e.starts_with("backtest: spec refused — "), "{e}");
        assert!(!state.join("backtests").exists());
    }
}
