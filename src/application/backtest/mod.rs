//! Backtest use case (xlab, `docs/xlab-2026-10-01.md` § 6, § 10): a strategy
//! spec + the sandbox's `[backtest]` → series from the market-data store
//! (`ports::market_data`) → the pure engine (`domain/backtest/`) → a run
//! dir. IO is injected ([`BacktestEnv`]: the store, the sandbox sections,
//! the run-dir root, now), so tests run on a temp store. Callers: `tengu
//! backtest` (`adapters/inbound/cli/backtest.rs`), the `backtest` tool
//! (`adapters/outbound/tools/xlab/run.rs`: the rules arms, no gate).
//!
//! | Step | Call | Rule |
//! |---|---|---|
//! | 1 | [`resolve`] | the spec (`[backtest.strategies.<name>]`, or a JSON object: its `name`, else the caller's fallback) parsed and validated ([`spec_of`]: every problem, one each); universe resolved (`@<name>`); instruments read = the universe or the ids the spec names, minus `exclude`; `spec_sha256` |
//! | 2 | [`prepare`] | `from` default = the earliest stored bar of those instruments at the spec's interval, `to` default = now; series over [`Resolved::data_window`]: bars at the interval, funding when the instrument's cost books it (always for `funding_carry`), ctx when its cost is `half_spread = ctx`; `RunParams` from `[backtest]`, `[xmarket.calendars]`, `[paper]`; `engine::candidates`; `RiskCaps` from `[risk]` + `[paper]`; the run id proposed |
//! | — | the Jev gate arm (`gate.rs`) | between `prepare` and `evaluate`: reads [`Prepared::set`] (candidates in decision order, features as-of) and picks the ones Jev takes |
//! | 3 | [`evaluate`] | arm `research` always; `capped` when the sandbox has `[risk]` + `[paper]`; then every extra `(name, candidates, Arm)` — **the gate arm's entry point** — simulated over its own candidates, reported with `n_candidates` = their count and compared with the base arm of its kind (`research` / `capped`: mean net bps difference, paired bootstrap over periods); split halves when the job has a split |
//! | 4 | [`write_run_dir`] | `<backtests dir>/<run id>/` (`run_dir.rs`): `report.json`, `report.md`, `trades-<arm>.jsonl`, `candidates.jsonl`, `skips.json` + [`BacktestRun::extra_files`] (the gate's `decisions.jsonl`) |
//!
//! | Rule | Value |
//! |---|---|
//! | Run id | `<YYYYMMDDTHHMMSSZ>-<strategy>` from now (UTC); a taken one ⇒ `-2`, `-3`, … — proposed by `prepare`, claimed by `write_run_dir` (`create_dir`: a run that took it meanwhile moves this one to the next free suffix) |
//! | `spec_sha256` | sha256 hex (64 chars) of the canonical JSON of `StrategySpec::to_value` (defaults filled, keys sorted at every depth — `domain/canonical.rs`) |
//! | `exclude` | never loaded; the engine still sees the whole universe and counts each excluded name as a skip (`excluded`) |
//! | Store | read only: a run never writes `market.db` |

pub(crate) mod gate;
pub(crate) mod run_dir;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use serde_json::Value;

pub(crate) use self::run_dir::write_run_dir;
use crate::config::backtest::BacktestConfig;
use crate::config::sections::SandboxSections;
use crate::domain::backtest::costs::{cost_for, CostSpec, HalfSpread};
use crate::domain::backtest::engine::{
    candidates, simulate, Arm, ArmResult, Candidate, CandidateSet, MarketData, RiskCaps, RunParams,
};
use crate::domain::backtest::report::{BacktestReport, CAPPED_ARM, PRIMARY_ARM};
use crate::domain::backtest::spec::{valid_name, SplitSpec, StrategyKind, StrategySpec};
use crate::domain::canonical::canonical_sha256;
use crate::domain::marketdata::{fmt_time, Interval};
use crate::ports::market_data::MarketDataStore;

const HOUR_MS: i64 = 3_600_000;

/// What a run reads from outside, injected (module doc).
#[derive(Clone)]
pub(crate) struct BacktestEnv {
    /// `<state dir>/market.db` — read only.
    pub store: Arc<dyn MarketDataStore>,
    /// `[backtest]` (absent = its defaults), `[xmarket.calendars]`,
    /// `[risk]`, `[paper]` of the sandbox.
    pub sections: Arc<SandboxSections>,
    /// `<state dir>/backtests` (`config::xmarket::backtests_dir`): the run dirs.
    pub backtests_dir: PathBuf,
    /// The run's "now": the default `to` and the run id's time.
    pub now_ms: i64,
}

/// Where the spec comes from.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SpecSource {
    /// `[backtest.strategies.<name>]`.
    Strategy(String),
    /// A JSON object (tool `spec`, `tengu backtest --spec`); `fallback_name`
    /// names it when it has no `name` (the CLI: the file stem).
    Json {
        value: Value,
        fallback_name: Option<String>,
    },
}

/// One run's request.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BacktestJob {
    pub spec: SpecSource,
    /// Decisions in `[from, to)`; `None` = the earliest stored bar / now.
    pub from_ms: Option<i64>,
    pub to_ms: Option<i64>,
    pub split: Option<SplitSpec>,
}

/// A spec resolved against the sandbox (module table, step 1).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Resolved {
    pub spec: StrategySpec,
    /// `spec.to_value()` — what the report records and the hash covers.
    pub spec_value: Value,
    pub spec_sha256: String,
    /// `RunParams::universe`: the resolved universe, `exclude` still in;
    /// empty for the kinds that name their ids (`pair_spread`, `event_window`).
    pub universe: Vec<String>,
    /// What the run reads: the universe or the named ids, minus `exclude`;
    /// sorted, distinct, never empty.
    pub instruments: Vec<String>,
}

impl Resolved {
    /// The cost an instrument resolves to: the spec's, else the longest
    /// `[backtest.costs]` prefix.
    fn cost<'a>(&'a self, costs: &'a BTreeMap<String, CostSpec>, id: &str) -> Option<&'a CostSpec> {
        self.spec.costs.as_ref().or_else(|| cost_for(costs, id))
    }

    /// The series range a run over `[from_ms, to_ms)` reads:
    /// `StrategySpec::data_range`, reaching further back for the longest
    /// `abdi_ranaldo` window among the `[backtest.costs]` its instruments
    /// resolve to.
    pub(crate) fn data_window(
        &self,
        costs: &BTreeMap<String, CostSpec>,
        from_ms: i64,
        to_ms: i64,
    ) -> (i64, i64) {
        let (mut lo, hi) = self.spec.data_range(from_ms, to_ms);
        let iv = self.spec.interval.ms();
        for id in &self.instruments {
            if let Some(HalfSpread::AbdiRanaldo { window_bars, .. }) =
                self.cost(costs, id).map(|c| &c.half_spread)
            {
                let back = i64::from(window_bars.saturating_add(1)).saturating_mul(iv);
                lo = lo.min(from_ms.saturating_sub(back));
            }
        }
        (lo, hi)
    }
}

/// What [`prepare`] built: everything a run needs before its arms — the
/// gate arm chooses from `set.candidates` in between.
#[derive(Debug, Clone)]
pub(crate) struct Prepared {
    /// Proposed (free at prepare time); [`write_run_dir`] claims it.
    pub run_id: String,
    /// `<YYYYMMDDTHHMMSSZ>-<strategy>` — the run id without its suffix.
    pub run_id_base: String,
    pub spec: StrategySpec,
    pub spec_value: Value,
    pub spec_sha256: String,
    pub split: Option<SplitSpec>,
    /// The ids loaded ([`Resolved::instruments`]).
    pub instruments: Vec<String>,
    pub md: MarketData,
    pub params: RunParams,
    pub set: CandidateSet,
    /// `[risk]` + `[paper]` ⇒ the capped arm's caps.
    pub caps: Option<RiskCaps>,
    pub backtests_dir: PathBuf,
}

/// What [`evaluate`] produced; [`write_run_dir`] writes it.
#[derive(Debug, Clone)]
pub(crate) struct BacktestRun {
    pub report: BacktestReport,
    /// Every arm's simulation by name: `research`, `capped`, the extra arms.
    pub arms: BTreeMap<String, ArmResult>,
    /// More files for the run dir, name → text (the gate arm's
    /// `decisions.jsonl`); a plain file name (`run_dir::check_file_name`).
    pub extra_files: BTreeMap<String, String>,
}

/// `[backtest]`, or its defaults when the sandbox has none.
fn backtest_config(sections: &SandboxSections) -> BacktestConfig {
    sections.backtest.clone().unwrap_or_default()
}

/// The spec of `src` parsed and validated against `bt` — every problem, one
/// per entry (`StrategySpec::from_value`; an unknown strategy lists the
/// library). The first half of [`resolve`]; the `backtest` tool reports an
/// `Err` as the spec's problems before any read.
pub(crate) fn spec_of(bt: &BacktestConfig, src: &SpecSource) -> Result<StrategySpec, Vec<String>> {
    match src {
        SpecSource::Strategy(name) => bt.strategy(name),
        SpecSource::Json {
            value,
            fallback_name,
        } => {
            // Its own `name` wins; `from_value("", …)` takes it.
            let name = if value.get("name").is_some() {
                ""
            } else {
                fallback_name.as_deref().unwrap_or("")
            };
            StrategySpec::from_value(name, value)
        }
    }
}

/// Step 1 (module table): the spec of `src` against `bt`.
pub(crate) fn resolve(bt: &BacktestConfig, src: &SpecSource) -> Result<Resolved> {
    let spec = spec_of(bt, src).map_err(|errors| anyhow!("{}", errors.join("\n")))?;
    let all = bt
        .spec_instruments(&spec)
        .map_err(|e| anyhow!("strategy `{}`: universe: {e}", spec.name))?;
    let instruments: Vec<String> = all
        .iter()
        .filter(|id| !spec.exclude.contains(*id))
        .cloned()
        .collect();
    if instruments.is_empty() {
        bail!(
            "strategy `{}`: every instrument it trades is excluded",
            spec.name
        );
    }
    let universe = if spec.kind.names_instruments() {
        Vec::new()
    } else {
        all
    };
    let spec_value = spec.to_value();
    let spec_sha256 = canonical_sha256(&spec_value);
    Ok(Resolved {
        spec,
        spec_value,
        spec_sha256,
        universe,
        instruments,
    })
}

/// The earliest stored bar at `interval` among `ids` (store coverage).
async fn earliest_bar(
    store: &dyn MarketDataStore,
    ids: &[String],
    interval: Interval,
) -> Result<Option<i64>> {
    let mut first: Option<i64> = None;
    for id in ids {
        for row in store.coverage(Some(id)).await? {
            if row.kind == "bars" && row.interval == Some(interval) {
                first = Some(first.map_or(row.first_ms, |f| f.min(row.first_ms)));
            }
        }
    }
    Ok(first)
}

/// The series of `r` over `[lo, hi)` (module table, step 2); an instrument
/// with no row of a kind gets no series of it (the engine notes / skips it).
async fn load(
    store: &dyn MarketDataStore,
    r: &Resolved,
    costs: &BTreeMap<String, CostSpec>,
    (lo, hi): (i64, i64),
) -> Result<MarketData> {
    let carry = matches!(r.spec.kind, StrategyKind::FundingCarry(_));
    let mut md = MarketData::default();
    for id in &r.instruments {
        let bars = store.bars(id, r.spec.interval, lo, hi).await?;
        if !bars.bars.is_empty() {
            md.bars.insert(id.clone(), bars);
        }
        // No cost ⇒ the engine skips the instrument (`no_costs`) unread.
        let Some(cost) = r.cost(costs, id) else {
            continue;
        };
        if cost.funding || carry {
            // HL stamps a settlement a few ms after its hour: an hour of margin.
            let f = store
                .funding(id, lo.saturating_sub(HOUR_MS), hi.saturating_add(HOUR_MS))
                .await?;
            if !f.points.is_empty() {
                md.funding.insert(id.clone(), f);
            }
        }
        if matches!(cost.half_spread, HalfSpread::Ctx { .. }) {
            let c = store.ctx(id, lo, hi).await?;
            if !c.points.is_empty() {
                md.ctx.insert(id.clone(), c);
            }
        }
    }
    Ok(md)
}

/// `[risk]` + `[paper]` ⇒ the capped arm's caps.
fn risk_caps(sections: &SandboxSections) -> Option<RiskCaps> {
    let (risk, paper) = (sections.risk.as_ref()?, sections.paper.as_ref()?);
    Some(RiskCaps {
        initial_cash_usd: paper.initial_cash_usd,
        max_order_notional_usd: risk.max_order_notional_usd,
        max_gross_exposure_usd: risk.max_gross_exposure_usd,
        max_net_exposure_usd: risk.max_net_exposure_usd,
        daily_loss_limit_usd: risk.daily_loss_limit_usd,
        total_loss_limit_usd: risk.total_loss_limit_usd,
    })
}

/// Steps 1–2 (module table): series loaded, candidates built, run id
/// proposed. `Err` names what is missing (no stored bars, an empty range,
/// an unknown strategy, a spec error).
pub(crate) async fn prepare(env: &BacktestEnv, job: BacktestJob) -> Result<Prepared> {
    let bt = backtest_config(&env.sections);
    let r = resolve(&bt, &job.spec)?;
    let iv = r.spec.interval;
    let to = job.to_ms.unwrap_or(env.now_ms);
    let from = match job.from_ms {
        Some(from) => from,
        None => earliest_bar(env.store.as_ref(), &r.instruments, iv)
            .await?
            .ok_or_else(|| {
                anyhow!(
                    "market.db holds no {iv} bars for the {} instrument(s) of `{}` — backfill \
                     them first (tengu history backfill --instruments … --interval {iv} \
                     --from <date> --funding) or give a from",
                    r.instruments.len(),
                    r.spec.name
                )
            })?,
    };
    if from >= to {
        bail!("from {} is not before to {}", fmt_time(from), fmt_time(to));
    }
    let window = r.data_window(&bt.costs, from, to);
    let md = load(env.store.as_ref(), &r, &bt.costs, window).await?;
    let params = RunParams {
        from_ms: from,
        to_ms: to,
        universe: r.universe.clone(),
        notional_usd: bt.notional_usd,
        costs: bt.costs.clone(),
        calendars: env.sections.calendars.clone(),
        bootstrap: bt.bootstrap,
        seed: bt.seed,
        start_equity_usd: env.sections.paper.as_ref().map(|p| p.initial_cash_usd),
    };
    let set = candidates(&r.spec, &md, &params)
        .map_err(|e| anyhow!("strategy `{}`: {e}", r.spec.name))?;
    let run_id_base = run_dir::run_id_base(env.now_ms, &r.spec.name);
    let run_id = run_dir::propose_run_id(&env.backtests_dir, &run_id_base);
    Ok(Prepared {
        run_id,
        run_id_base,
        spec: r.spec,
        spec_value: r.spec_value,
        spec_sha256: r.spec_sha256,
        split: job.split,
        instruments: r.instruments,
        md,
        params,
        set,
        caps: risk_caps(&env.sections),
        backtests_dir: env.backtests_dir.clone(),
    })
}

/// Step 3 (module table): the base arms, then each extra arm
/// `(name, candidates, arm)` over its own candidates — a subset of
/// `prepared.set.candidates` (the Jev gate's takes), `Arm::Research` or
/// `Arm::Capped(prepared.caps)` — compared with the base arm of its kind.
/// An extra name is `[a-z0-9_]{1,48}` and not taken.
pub(crate) fn evaluate(
    p: &Prepared,
    extra: Vec<(String, Vec<Candidate>, Arm)>,
) -> Result<BacktestRun> {
    let mut report = BacktestReport::new(
        p.run_id.clone(),
        &p.spec,
        p.spec_value.clone(),
        p.spec_sha256.clone(),
        &p.params,
        p.split.clone(),
        &p.set,
    );
    let mut arms: BTreeMap<String, ArmResult> = BTreeMap::new();
    let mut base: Vec<(&[Candidate], Arm)> = vec![(&p.set.candidates, Arm::Research)];
    if let Some(caps) = &p.caps {
        base.push((&p.set.candidates, Arm::Capped(caps.clone())));
    }
    for (cands, arm) in base {
        let name = arm.name().to_string();
        let result = simulate(&p.spec, &p.md, &p.params, cands, arm);
        report.add_arm(&name, cands.len(), &result);
        arms.insert(name, result);
    }
    for (name, cands, arm) in extra {
        if !valid_name(&name) {
            bail!("arm name `{name}`: [a-z0-9_], 1-48 characters");
        }
        if arms.contains_key(&name) {
            bail!("arm `{name}` is already in the run");
        }
        let base_name = arm.name();
        let result = simulate(&p.spec, &p.md, &p.params, &cands, arm);
        report.add_arm(&name, cands.len(), &result);
        if let Some(b) = arms.get(base_name) {
            report.compare(
                (&name, &result.trades),
                (base_name, &b.trades),
                p.params.bootstrap,
                p.params.seed,
            );
        }
        arms.insert(name, result);
    }
    debug_assert!(arms.contains_key(PRIMARY_ARM));
    debug_assert_eq!(arms.contains_key(CAPPED_ARM), p.caps.is_some());
    Ok(BacktestRun {
        report,
        arms,
        extra_files: BTreeMap::new(),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::*;
    use crate::adapters::outbound::market_data::SqliteMarketData;
    use crate::config::risk::{PaperConfig, RiskConfig};
    use crate::domain::backtest::engine::{SkipReason, Trade};
    use crate::domain::backtest::testkit::{nyse, utc, H};
    use crate::domain::calendar::Calendar;
    use crate::domain::marketdata::{Bar, FundingPoint};
    use crate::domain::observation::{ObsSource, Observation};

    const AAA: &str = "hyperliquid:xyz:AAA";
    const BBB: &str = "hyperliquid:xyz:BBB";
    const CCC: &str = "hyperliquid:xyz:CCC";

    /// `[risk]` + `[paper]` of a $100 book: $25 orders, $60 gross.
    const RISK: &str = r#"
        [risk]
        account = "t"
        mode = "paper"
        venues = ["hyperliquid"]
        min_lifecycle = "paper_tradable"
        instruments_allow = ["hyperliquid:xyz:AAA", "hyperliquid:xyz:BBB", "hyperliquid:xyz:CCC"]
        instruments_deny = []
        max_order_notional_usd = 25
        max_position_notional_usd = 50
        max_asset_exposure_usd = 50
        max_venue_exposure_usd = 110
        max_gross_exposure_usd = 60
        max_net_exposure_usd = 60
        max_leverage = 1.2
        daily_loss_limit_usd = 10
        total_loss_limit_usd = 25
        min_edge_bps = 10
        max_slippage_bps = 30
        min_depth_usd = 250
        require_hedge_for = []
        max_data_age_ms = { book = 5000, ctx = 90000, reference = 60000, quote = 20000 }
        max_skew_ms = 5000
        max_orders_per_min = 12
        max_open_orders = 4
        kill_switch_file = "/tmp/tengu-backtest-test-KILL"
        allow_reduce_degraded = true
        [risk.exits]
        take_profit_bps = 2000
        stop_loss_bps = 1000
        max_hold_secs = 259200
        [paper]
        initial_cash_usd = 100
        latency_ms = 250
        latency_jitter_ms = 100
        fee_tier = 0
        staking_discount_pct = 0
        order_types = ["market", "ioc"]
    "#;

    /// `[backtest]`: xyz costs, a 3-name universe, rule W, a funding carry.
    const BACKTEST: &str = r#"
        notional_usd = 100
        bootstrap = 200
        seed = 7
        [costs."hyperliquid:xyz:"]
        taker_fee_bps = 0.9
        half_spread = { model = "fixed", bps = 1.0 }
        [universes]
        xyz = ["hyperliquid:xyz:AAA", "hyperliquid:xyz:BBB", "hyperliquid:xyz:CCC"]
        [strategies.weekend_fade]
        kind = "weekend_window"
        universe = "@xyz"
        interval = "1h"
        calendar = "us_equity"
        direction = "fade"
        [strategies.carry]
        kind = "funding_carry"
        universe = "@xyz"
        interval = "1h"
        min_apr_pct = 50
        exit_apr_pct = 10
        hold_hours = 24
    "#;

    fn sections() -> SandboxSections {
        #[derive(serde::Deserialize)]
        struct Tables {
            risk: RiskConfig,
            paper: PaperConfig,
        }
        let t: Tables = toml::from_str(RISK).unwrap();
        SandboxSections {
            sandbox: Some("t".into()),
            risk: Some(t.risk),
            paper: Some(t.paper),
            calendars: BTreeMap::from([("us_equity".to_string(), Calendar::Exchange(nyse()))]),
            backtest: Some(toml::from_str(BACKTEST).unwrap()),
            ..Default::default()
        }
    }

    /// Hourly bars 2026-09-01 → 10-01 for three names (each its own ±20 bps
    /// wave; Sunday 12:00–24:00 UTC a jump: +200 bps `AAA` / `CCC`, −150 bps
    /// `BBB`), funding every hour stamped 37 ms late (`AAA`: 175 % APR on
    /// 09-10 and 09-20, else ~1 %).
    async fn seeded(dir: &std::path::Path) -> Arc<dyn MarketDataStore> {
        let store = SqliteMarketData::open(dir).unwrap();
        let t0 = utc("2026-09-01 00:00");
        let hours = 30 * 24;
        for (k, id) in [AAA, BBB, CCC].into_iter().enumerate() {
            let kf = k as f64;
            let mut bars = Vec::new();
            let mut funding = Vec::new();
            for h in 0..hours {
                let t = t0 + h as i64 * H;
                let wave = ((h as f64) / (5.0 + kf) + kf).sin() * 0.002;
                let weekday = chrono::DateTime::from_timestamp_millis(t)
                    .map(|d| chrono::Datelike::weekday(&d).num_days_from_monday())
                    .unwrap();
                let jump = if weekday == 6 && (12..24).contains(&(h % 24)) {
                    if k % 2 == 0 {
                        0.02
                    } else {
                        -0.015
                    }
                } else {
                    0.0
                };
                let c = (100.0 + 10.0 * kf) * (wave + jump).exp();
                bars.push(Bar {
                    t_open_ms: t,
                    o: c,
                    h: c * 1.001,
                    l: c * 0.999,
                    c,
                    v: 10.0 + kf,
                    n: Some(5),
                });
                let day = h / 24;
                let rate = if k == 0 && (day == 9 || day == 19) {
                    2e-4 // 175 % APR: longs pay — the carry shorts
                } else {
                    1e-6
                };
                funding.push(FundingPoint {
                    t_ms: t + 37,
                    rate_1h: rate,
                    premium: None,
                });
            }
            store
                .put_bars(id, Interval::H1, "test", &bars)
                .await
                .unwrap();
            store.put_funding(id, "test", &funding).await.unwrap();
        }
        Arc::new(store)
    }

    fn env(store: Arc<dyn MarketDataStore>, backtests: &std::path::Path) -> BacktestEnv {
        BacktestEnv {
            store,
            sections: Arc::new(sections()),
            backtests_dir: backtests.to_path_buf(),
            now_ms: utc("2026-10-01 12:00") + 34_000,
        }
    }

    fn job(spec: SpecSource) -> BacktestJob {
        BacktestJob {
            spec,
            from_ms: None,
            to_ms: Some(utc("2026-09-29 00:00")),
            split: None,
        }
    }

    fn lines(path: &std::path::Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    /// The whole use case on a temp store: rule W and a funding carry →
    /// candidates, research + capped arms, a time split, the run dir.
    #[tokio::test]
    async fn weekend_and_carry_run_end_to_end() {
        let tmp = tempfile::tempdir().unwrap();
        let store = seeded(&tmp.path().join("state")).await;
        let backtests = tmp.path().join("state/backtests");
        let env = env(store, &backtests);

        let mut w = job(SpecSource::Strategy("weekend_fade".into()));
        w.split = Some(SplitSpec::parse("time:2026-09-15").unwrap());
        let p = prepare(&env, w.clone()).await.unwrap();
        // from defaults to the earliest stored bar.
        assert_eq!(p.params.from_ms, utc("2026-09-01 00:00"));
        assert_eq!(p.params.to_ms, utc("2026-09-29 00:00"));
        assert_eq!(p.instruments, vec![AAA, BBB, CCC]);
        assert_eq!(p.params.universe, vec![AAA, BBB, CCC]);
        assert_eq!(p.params.start_equity_usd, Some(100.0));
        assert_eq!(p.run_id, "20261001T120034Z-weekend_fade");
        assert_eq!(p.spec_sha256.len(), 64);
        assert_eq!(p.spec_value["name"], "weekend_fade");
        // Four windows (Labor Day moves the first exit to Tuesday), 3 names.
        assert_eq!(p.set.candidates.len(), 12, "{:?}", p.set.skip_counts());
        assert!(p
            .set
            .candidates
            .iter()
            .all(|c| c.features.contains_key("ret_24h_bps")));
        assert_eq!(p.caps.as_ref().unwrap().max_order_notional_usd, 25.0);

        let mut run = evaluate(&p, Vec::new()).unwrap();
        let research = &run.arms["research"];
        assert_eq!(research.trades.len(), 12);
        assert!(research.trades.iter().all(|t| t.notional_usd == 100.0));
        // Sun 09-13 18:00 EDT: AAA and CCC jumped up (faded short), BBB
        // down (long); each fade earns its jump back by Monday's exit.
        let at = utc("2026-09-13 22:00");
        let trade = |id: &str| {
            research
                .trades
                .iter()
                .find(|t| t.instrument == id && t.decided_at_ms == at)
                .unwrap()
        };
        use crate::domain::book::Side;
        assert_eq!(
            [trade(AAA).side, trade(BBB).side, trade(CCC).side],
            [Side::Sell, Side::Buy, Side::Sell]
        );
        assert!(trade(AAA).gross_bps > 150.0 && trade(BBB).gross_bps > 100.0);
        // Labor Day: that window enters Monday 18:00 and exits Tuesday 09:00.
        let labor_day = &research.trades[0];
        assert_eq!(
            (labor_day.entry_ms, labor_day.exit_ms),
            (utc("2026-09-07 22:00"), utc("2026-09-08 13:00"))
        );
        // Costs: 2 × (0.9 + 1.0) bps on every trade.
        assert!(research
            .trades
            .iter()
            .all(|t| (t.fee_bps + t.spread_bps - 3.8).abs() < 1e-9));
        assert!(research.trades.iter().all(|t| t.funding_complete));
        // Capped: $25 orders; two at a time fit under $60 gross, so the
        // third name of each window is refused.
        let capped = &run.arms["capped"];
        assert!(capped.trades.iter().all(|t| t.notional_usd == 25.0));
        assert_eq!(capped.trades.len() + capped.refusals.len(), 12);
        assert!(capped
            .refusals
            .iter()
            .all(|r| r.rule == "max_gross_exposure_usd"));
        assert_eq!(capped.refusals.len(), 4);
        // The split: in-sample = the windows decided before 09-15.
        let halves = run.report.arms["research"].split.as_ref().unwrap();
        assert_eq!((halves.in_sample.n, halves.holdout.n), (6, 6));

        let dir = write_run_dir(&p, &mut run).unwrap();
        assert_eq!(dir, backtests.join("20261001T120034Z-weekend_fade"));
        let text = std::fs::read_to_string(dir.join("report.json")).unwrap();
        // The file is the report as serialized (serde_json's default float
        // parser may move a last digit on the way back, so compare text).
        assert_eq!(
            text,
            serde_json::to_string_pretty(&run.report).unwrap() + "\n"
        );
        let report: BacktestReport = serde_json::from_str(&text).unwrap();
        assert_eq!(report.run_id, "20261001T120034Z-weekend_fade");
        assert_eq!(
            (report.n_candidates, report.arms.len()),
            (12, run.report.arms.len())
        );
        let o = Observation::of("backtest", &report, 0, 0, ObsSource::Live);
        assert_eq!(o.key, "backtest/1:20261001T120034Z-weekend_fade");
        let md = std::fs::read_to_string(dir.join("report.md")).unwrap();
        assert!(md.contains("## Split `time:2026-09-15T00:00:00Z`"), "{md}");
        for arm in ["research", "capped"] {
            let path = dir.join(format!("trades-{arm}.jsonl"));
            let text = std::fs::read_to_string(&path).unwrap();
            let want: Vec<String> = run.arms[arm]
                .trades
                .iter()
                .map(|t| serde_json::to_string(t).unwrap())
                .collect();
            assert_eq!(text.lines().collect::<Vec<_>>(), want, "{arm}");
            let first: Trade = serde_json::from_value(lines(&path)[0].clone()).unwrap();
            assert_eq!(first.seq, run.arms[arm].trades[0].seq);
        }
        let cands = lines(&dir.join("candidates.jsonl"));
        assert_eq!(cands.len(), 12);
        assert!(cands[0]["features"]["hour_of_week"].is_number());
        let skips: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("skips.json")).unwrap())
                .unwrap();
        assert_eq!(
            skips["arms"]["capped"]["refusals_by_rule"]["max_gross_exposure_usd"],
            4
        );
        assert!(skips["candidates"].is_object() && skips["data_notes"].is_array());

        // Same second, same strategy: the next run is "-2"; a third "-3".
        let p2 = prepare(&env, w.clone()).await.unwrap();
        assert_eq!(p2.run_id, "20261001T120034Z-weekend_fade-2");
        let mut run2 = evaluate(&p2, Vec::new()).unwrap();
        let dir2 = write_run_dir(&p2, &mut run2).unwrap();
        assert!(dir2.ends_with("20261001T120034Z-weekend_fade-2"));
        assert_eq!(run2.report.run_id, "20261001T120034Z-weekend_fade-2");
        // A proposal another run claimed first moves to the next suffix.
        let p3 = prepare(&env, w).await.unwrap();
        std::fs::create_dir(backtests.join(&p3.run_id)).unwrap();
        let mut run3 = evaluate(&p3, Vec::new()).unwrap();
        let dir3 = write_run_dir(&p3, &mut run3).unwrap();
        assert!(
            dir3.ends_with("20261001T120034Z-weekend_fade-4"),
            "{}",
            dir3.display()
        );
        assert_eq!(run3.report.run_id, "20261001T120034Z-weekend_fade-4");

        // The carry: AAA's two extreme days, short (longs pay), out when
        // the rate normalises; funding is booked to the short.
        let c = prepare(&env, job(SpecSource::Strategy("carry".into())))
            .await
            .unwrap();
        assert_eq!(c.set.candidates.len(), 2, "{:?}", c.set.candidates);
        assert!(c.set.candidates.iter().all(|x| x.instrument == AAA));
        let run = evaluate(&c, Vec::new()).unwrap();
        let trades = &run.arms["research"].trades;
        assert_eq!(trades.len(), 2);
        assert!(trades
            .iter()
            .all(|t| t.funding_bps > 0.0 && t.funding_complete));
        assert!(trades.iter().all(|t| t.side == Side::Sell));
        // Decided at the bar close after the 00:00 settlement, out at the
        // close after the first normal rate a day later.
        assert_eq!(
            (trades[0].decided_at_ms, trades[0].exit_ms),
            (utc("2026-09-10 01:00"), utc("2026-09-11 01:00"))
        );
    }

    /// The gate arm's entry point: an extra arm over a subset, compared with
    /// its base arm; names are checked; extra files land in the run dir.
    #[tokio::test]
    async fn extra_arms_are_simulated_compared_and_written() {
        let tmp = tempfile::tempdir().unwrap();
        let store = seeded(&tmp.path().join("state")).await;
        let backtests = tmp.path().join("state/backtests");
        let env = env(store, &backtests);
        let p = prepare(&env, job(SpecSource::Strategy("weekend_fade".into())))
            .await
            .unwrap();
        let takes: Vec<Candidate> = p
            .set
            .candidates
            .iter()
            .filter(|c| c.instrument != BBB)
            .cloned()
            .collect();
        let caps = p.caps.clone().unwrap();
        let mut run = evaluate(
            &p,
            vec![
                ("jev".into(), takes.clone(), Arm::Research),
                ("jev_capped".into(), takes, Arm::Capped(caps.clone())),
            ],
        )
        .unwrap();
        assert_eq!(run.report.arms["jev"].n_candidates, 8);
        assert_eq!(run.arms["jev"].trades.len(), 8);
        let pairs: Vec<(&str, &str)> = run
            .report
            .comparisons
            .iter()
            .map(|c| (c.a.as_str(), c.b.as_str()))
            .collect();
        assert_eq!(pairs, vec![("jev", "research"), ("jev_capped", "capped")]);
        run.extra_files
            .insert("decisions.jsonl".into(), "{\"a\":1}\n".into());
        let dir = write_run_dir(&p, &mut run).unwrap();
        assert_eq!(lines(&dir.join("decisions.jsonl")), vec![json!({"a": 1})]);
        assert!(dir.join("trades-jev_capped.jsonl").exists());

        for (name, why) in [
            ("research", "already in the run"),
            ("Bad-Name", "[a-z0-9_]"),
        ] {
            let e = evaluate(&p, vec![(name.into(), Vec::new(), Arm::Research)]).unwrap_err();
            assert!(e.to_string().contains(why), "{name}: {e}");
        }
        let mut bad = evaluate(&p, Vec::new()).unwrap();
        bad.extra_files.insert("../x".into(), String::new());
        assert!(write_run_dir(&p, &mut bad).is_err());
    }

    #[tokio::test]
    async fn runs_that_cannot_start_say_why() {
        let tmp = tempfile::tempdir().unwrap();
        let store = seeded(&tmp.path().join("state")).await;
        let env = env(store, &tmp.path().join("state/backtests"));
        let err = |j: BacktestJob| {
            let env = env.clone();
            async move { prepare(&env, j).await.unwrap_err().to_string() }
        };
        let e = err(job(SpecSource::Strategy("nope".into()))).await;
        assert!(e.contains("no [backtest.strategies.nope]"), "{e}");
        // A JSON spec: its own name, else the fallback; errors name fields.
        let spec = json!({"kind": "move_trigger", "universe": [AAA], "interval": "4h",
            "lookback_bars": 2, "threshold_bps": 50, "direction": "fade", "hold_bars": 2});
        let e = err(job(SpecSource::Json {
            value: spec.clone(),
            fallback_name: Some("my_move".into()),
        }))
        .await;
        assert!(e.contains("market.db holds no 4h bars"), "{e}");
        assert!(e.contains("`my_move`"), "{e}");
        let mut bad = spec.clone();
        bad["hold_bars"] = json!(0);
        let e = err(job(SpecSource::Json {
            value: bad,
            fallback_name: Some("my_move".into()),
        }))
        .await;
        assert!(e.contains("hold_bars must be within 1..=10000"), "{e}");
        let mut j = job(SpecSource::Strategy("weekend_fade".into()));
        j.from_ms = Some(utc("2026-09-29 00:00"));
        let e = err(j).await;
        assert!(e.contains("is not before"), "{e}");
        // Everything excluded.
        let mut all_out = spec;
        all_out["interval"] = json!("1h");
        all_out["exclude"] = json!([AAA]);
        let e = err(job(SpecSource::Json {
            value: all_out,
            fallback_name: Some("x".into()),
        }))
        .await;
        assert!(e.contains("every instrument it trades is excluded"), "{e}");
        // An excluded name is not loaded but still counted as a skip.
        let mut j = job(SpecSource::Json {
            value: json!({"name": "w", "kind": "weekend_window", "universe": "@xyz",
                "interval": "1h", "calendar": "us_equity", "direction": "fade",
                "exclude": [CCC]}),
            fallback_name: Some("ignored".into()),
        });
        j.from_ms = Some(utc("2026-09-10 00:00"));
        let p = prepare(&env, j).await.unwrap();
        assert_eq!(p.spec.name, "w");
        assert_eq!(p.instruments, vec![AAA, BBB]);
        assert!(!p.md.bars.contains_key(CCC));
        assert!(p
            .set
            .skipped
            .iter()
            .any(|s| s.instrument == CCC && s.reason == SkipReason::Excluded));
    }
}
