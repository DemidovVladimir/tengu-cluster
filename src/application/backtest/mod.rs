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
//! | 1 | [`resolve`] | the spec (`[backtest.strategies.<name>]`, or a JSON object: its `name`, else the caller's fallback) parsed and validated ([`spec_of`]: every problem, one each — the `backtest` tool's spec check); universe resolved (`@<name>`); instruments read = the universe or the ids the spec names, minus `exclude`; `spec_sha256` |
//! | 2 | [`prepare`] | `from` default = the earliest stored bar of those instruments at the spec's interval, `to` default = now; series over [`Resolved::data_window`]: bars at the interval, funding when the instrument's cost books it (always for `funding_carry`), ctx when its cost is `half_spread = ctx`; `[backtest.splits]` applied to them (`MarketData::adjust_for_splits`: bars closed before each split ÷ ratio, volume × ratio, a bar straddling it dropped; a data note each, listed first); `RunParams` from `[backtest]` (incl. `max_candidates`), `[xmarket.calendars]`; `engine::candidates` — a run past `max_candidates` stops here, before any arm or file; `RiskCaps` from `[risk]` + `[paper]`; the run id proposed |
//! | — | the Jev gate arm (`gate.rs`) | between `prepare` and `evaluate`: `run_gate` reads [`Prepared::set`] (candidates in decision order, features as-of) and decides them |
//! | 3 | [`evaluate`] · `gate::evaluate_gated` | arm `research` always; `capped` when the sandbox has `[risk]` + `[paper]`; then every extra `(name, candidates, Arm)` simulated over its own candidates, reported with `n_candidates` = their count and compared with the base arm of its kind (`research` / `capped`: mean net bps difference, paired bootstrap over periods); split halves when the job has a split. With the gate: `evaluate_gated` = `evaluate` + the gate's `rules` / `jev` arms (research + capped) over the decided candidates, comparisons, calibration, summary, `decisions.jsonl` |
//! | 4 | [`write_run_dir`] | `<backtests dir>/<run id>/` (`run_dir.rs`): `report.json`, `report.md`, `trades-<arm>.jsonl`, `candidates.jsonl`, `skips.json` + [`BacktestRun::extra_files`] (the gate's `decisions.jsonl`); then the run dirs beyond `[backtest] keep_runs` pruned, oldest first (never the decision cache) |
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
use crate::domain::backtest::spec::{
    spec_sha256, valid_name, SplitSpec, StrategyKind, StrategySpec,
};
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
    /// `[backtest] keep_runs`: [`write_run_dir`] prunes the oldest run dirs
    /// beyond it (0 = keep all).
    pub keep_runs: usize,
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
    let spec_sha256 = spec_sha256(&spec_value);
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
    let mut md = load(env.store.as_ref(), &r, &bt.costs, window).await?;
    // Share splits before any decision reads a price.
    let split_notes = md.adjust_for_splits(&bt.stock_splits());
    let params = RunParams {
        from_ms: from,
        to_ms: to,
        universe: r.universe.clone(),
        notional_usd: bt.notional_usd,
        costs: bt.costs.clone(),
        calendars: env.sections.calendars.clone(),
        bootstrap: bt.bootstrap,
        seed: bt.seed,
        max_candidates: bt.max_candidates,
    };
    let mut set = candidates(&r.spec, &md, &params)
        .map_err(|e| anyhow!("strategy `{}`: {e}", r.spec.name))?;
    set.notes.splice(0..0, split_notes);
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
        keep_runs: bt.keep_runs,
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
    use std::collections::{BTreeMap, BTreeSet};

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

    /// A generation's `spec:` pin (`domain/lineage/pins.rs`, read from the
    /// raw config text) is the run's `spec_sha256` for every strategy of the
    /// xlab library: the hashes in existing run dirs stay matchable.
    #[test]
    fn a_spec_pin_is_the_runs_spec_sha256() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("sandboxes/xlab/config.toml");
        let text = std::fs::read_to_string(path).unwrap();
        let table: toml::Value = toml::from_str(&text).unwrap();
        let bt: BacktestConfig = table["backtest"].clone().try_into().unwrap();
        assert!(bt.strategies.len() >= 5);
        for name in bt.strategies.keys() {
            let run = resolve(&bt, &SpecSource::Strategy(name.clone())).unwrap();
            let pin = crate::domain::lineage::pins::spec_pin(&text, name).unwrap();
            assert_eq!(run.spec_sha256, pin, "{name}");
        }
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
        // smallest move of each window is refused (not the alphabetically
        // last name).
        let capped = &run.arms["capped"];
        assert!(capped.trades.iter().all(|t| t.notional_usd == 25.0));
        assert_eq!(capped.trades.len() + capped.refusals.len(), 12);
        assert!(capped
            .refusals
            .iter()
            .all(|r| r.rule == "max_gross_exposure_usd"));
        assert_eq!(capped.refusals.len(), 4);
        for r in &capped.refusals {
            let refused = p.set.candidates[r.seq].signal_bps.abs();
            let window: Vec<f64> = p
                .set
                .candidates
                .iter()
                .filter(|c| c.decided_at_ms == r.decided_at_ms)
                .map(|c| c.signal_bps.abs())
                .collect();
            assert_eq!(window.len(), 3);
            assert!(window.iter().all(|s| *s >= refused), "{r:?}: {window:?}");
        }
        // Drawdown units: research in bps of a $100 trade, capped in % of
        // its $100 cash.
        let rs = &run.report.arms["research"].summary;
        assert_eq!(rs.max_drawdown_pct, None);
        assert!((rs.max_drawdown_bps.unwrap() - rs.max_drawdown_usd * 100.0).abs() < 1e-9);
        let cs = &run.report.arms["capped"].summary;
        assert_eq!(cs.max_drawdown_bps, None);
        assert!((cs.max_drawdown_pct.unwrap() - cs.max_drawdown_usd).abs() < 1e-9);
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

    /// `[backtest.splits]`: CCC stored with a 2-for-1 split inside the
    /// 09-13 weekend's hold (pre-split prices doubled) trades like the
    /// unsplit series once the split is configured; the note leads the data
    /// notes of the report and `skips.json`.
    #[tokio::test]
    async fn a_configured_split_adjusts_the_loaded_bars_and_is_noted() {
        let tmp = tempfile::tempdir().unwrap();
        let unsplit = seeded(&tmp.path().join("unsplit")).await;
        let at = utc("2026-09-14 08:00");
        let raw = SqliteMarketData::open(&tmp.path().join("raw")).unwrap();
        let all = unsplit
            .bars(CCC, Interval::H1, 0, utc("2026-10-02 00:00"))
            .await
            .unwrap();
        let doubled: Vec<Bar> = all
            .bars
            .iter()
            .map(|b| match b.t_open_ms < at {
                true => Bar {
                    o: b.o * 2.0,
                    h: b.h * 2.0,
                    l: b.l * 2.0,
                    c: b.c * 2.0,
                    v: b.v / 2.0,
                    ..*b
                },
                false => *b,
            })
            .collect();
        raw.put_bars(CCC, Interval::H1, "test", &doubled)
            .await
            .unwrap();
        let raw: Arc<dyn MarketDataStore> = Arc::new(raw);
        let spec = SpecSource::Json {
            value: json!({"name": "w", "kind": "weekend_window", "universe": [CCC],
                "interval": "1h", "calendar": "us_equity", "direction": "fade",
                "costs": {"taker_fee_bps": 0.9, "funding": false}}),
            fallback_name: None,
        };
        let mut j = job(spec);
        j.from_ms = Some(utc("2026-09-10 00:00"));
        let window_trade = |run: &BacktestRun| {
            run.arms["research"]
                .trades
                .iter()
                .find(|t| t.decided_at_ms == utc("2026-09-13 22:00"))
                .unwrap()
                .clone()
        };
        let base = env(unsplit, &tmp.path().join("b1"));
        let p0 = prepare(&base, j.clone()).await.unwrap();
        let want = window_trade(&evaluate(&p0, Vec::new()).unwrap());
        assert!(
            want.entry_ms < at && at < want.exit_ms,
            "the split is mid-hold"
        );

        // Not configured: the split books as a fall to half — the raw entry
        // is the doubled pre-split price.
        let mut e = env(raw.clone(), &tmp.path().join("b2"));
        let p = prepare(&e, j.clone()).await.unwrap();
        let t = window_trade(&evaluate(&p, Vec::new()).unwrap());
        let (entry, exit) = (want.legs[0].entry_px, want.legs[0].exit_px);
        assert_eq!(t.legs[0].entry_px, entry * 2.0);
        let raw_gross = t.side.sign() * (exit / (2.0 * entry) - 1.0) * 1e4;
        assert!((t.gross_bps - raw_gross).abs() < 1e-6);
        assert!(
            t.gross_bps > 4_000.0,
            "the short books the split: {}",
            t.gross_bps
        );
        assert!(p.set.notes.iter().all(|n| !n.contains("split-adjusted")));

        // Configured: the same trade, and said.
        let mut s = sections();
        let bt = s.backtest.as_mut().unwrap();
        bt.splits = toml::from_str::<BTreeMap<String, Vec<crate::config::backtest::SplitEntry>>>(
            r#""hyperliquid:xyz:CCC" = [{ at = "2026-09-14T08:00:00Z", ratio = 2.0 }]"#,
        )
        .unwrap();
        e.sections = Arc::new(s);
        let p = prepare(&e, j).await.unwrap();
        let mut run = evaluate(&p, Vec::new()).unwrap();
        let t = window_trade(&run);
        for (a, b) in [
            (t.gross_bps, want.gross_bps),
            (t.net_bps, want.net_bps),
            (t.signal_bps, want.signal_bps),
            (t.legs[0].entry_px, want.legs[0].entry_px),
        ] {
            assert!((a - b).abs() < 1e-9 * a.abs().max(1.0), "{a} vs {b}");
        }
        let note =
            format!("split-adjusted {CCC}: ratio 2 (new shares per old) at 2026-09-14T08:00:00Z");
        assert!(p.set.notes[0].starts_with(&note), "{:?}", p.set.notes);
        assert!(run.report.data_notes[0].starts_with(&note));
        let dir = write_run_dir(&p, &mut run).unwrap();
        let skips: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("skips.json")).unwrap())
                .unwrap();
        assert!(skips["data_notes"][0].as_str().unwrap().starts_with(&note));
        let md = std::fs::read_to_string(dir.join("report.md")).unwrap();
        assert!(md.contains(&format!("- {note}")), "{md}");
    }

    /// Takes every candidate but BBB's; p(take) 0.8 / 0.2.
    struct AllButB;

    #[async_trait::async_trait]
    impl crate::ports::decision::DecisionEngine for AllButB {
        fn model(&self) -> &str {
            "typesafe/jev-1.13-20260917"
        }
        async fn decide(
            &self,
            state: &Value,
            _q: &BTreeMap<String, crate::domain::decision::Question>,
        ) -> Result<crate::domain::decision::Decision> {
            use crate::domain::decision::{Answer, Decision};
            let take = state["event"]["instrument"] != json!(BBB);
            let (choice, p) = if take { ("take", 0.8) } else { ("skip", 0.2) };
            Ok(Decision {
                id: format!("gen-dec-{}", state["event"]["instrument"]),
                model: self.model().into(),
                answers: BTreeMap::from([(
                    "next_action".to_string(),
                    Answer {
                        kind: "choice".into(),
                        choice: Some(choice.into()),
                        probabilities: BTreeMap::from([("take".to_string(), p)]),
                        confidence: Some(0.9),
                        ..Default::default()
                    },
                )]),
                ..Default::default()
            })
        }
    }

    /// `tengu backtest --gate`'s path on a temp store: build (K = 3 replay
    /// loops auditing to a temp file) → `run_gate` (9 of 12 decided) →
    /// `evaluate_gated` → `write_run_dir`: the gate's arms, comparisons,
    /// calibration and summary in the report, `decisions.jsonl` by seq in the
    /// claimed dir, nothing left outside it.
    #[tokio::test]
    async fn the_gate_arm_end_to_end() {
        use crate::application::backtest::gate::{
            evaluate_gated, run_gate, Gate, GateAudit, DECISIONS_FILE,
        };
        use crate::bootstrap::decision::build_replay_loop;
        use crate::ports::clock::SimClock;

        let tmp = tempfile::tempdir().unwrap();
        let store = seeded(&tmp.path().join("state")).await;
        let backtests = tmp.path().join("state/backtests");
        let env = env(store, &backtests);
        let p = prepare(&env, job(SpecSource::Strategy("weekend_fade".into())))
            .await
            .unwrap();
        assert_eq!(p.set.candidates.len(), 12);
        let config: crate::config::Config = toml::from_str(
            r#"
            [agents.xl_jev]
            engine = "openrouter"
            model = "m"
            tools = ["read_file"]
            [decision_loops.xl_gate]
            goal = "Gate the trades a rule proposes"
            agent = "xl_jev"
            act_at = 0.01
            max_steps = 1
            escalate = false
            [decision_loops.xl_gate.actions.take]
            description = "Trade it"
            [decision_loops.xl_gate.actions.skip]
            description = "Do not trade it"
            "#,
        )
        .unwrap();
        let audit = GateAudit::new().unwrap();
        let audit_path = audit.path().to_path_buf();
        let engine: Arc<dyn crate::ports::decision::DecisionEngine> = Arc::new(AllButB);
        let workers = (0..3)
            .map(|_| {
                let clock = Arc::new(SimClock::at(0));
                let l = build_replay_loop(
                    &config,
                    "xl_gate",
                    Arc::clone(&engine),
                    clock.clone(),
                    audit.path(),
                )
                .unwrap();
                (clock, l)
            })
            .collect();
        let gate = Gate {
            loop_name: "xl_gate".into(),
            engine,
            workers,
        };
        let decided = run_gate(&p.set.candidates, &p.spec.name, &gate, 9)
            .await
            .unwrap();
        assert_eq!((decided.decided, decided.cut), (9, 3));
        let mut run = evaluate_gated(&p, &decided, &audit, 0.00004).unwrap();
        drop(audit);
        assert!(!audit_path.exists(), "the temp audit is gone");

        // The report: base arms over all 12, the gate's over the 9 decided.
        let r = &run.report;
        assert_eq!(
            r.arms.keys().map(String::as_str).collect::<Vec<_>>(),
            [
                "capped",
                "jev",
                "jev_capped",
                "research",
                "rules",
                "rules_capped"
            ]
        );
        assert_eq!(
            (
                r.arms["research"].n_candidates,
                r.arms["rules"].n_candidates
            ),
            (12, 9)
        );
        assert_eq!(r.arms["jev"].n_candidates, 6, "AAA and CCC, three windows");
        let rules_seqs: BTreeSet<usize> = run.arms["rules"].trades.iter().map(|t| t.seq).collect();
        assert_eq!(rules_seqs, (0..9).collect());
        assert!(run.arms["jev"]
            .trades
            .iter()
            .all(|t| p.set.candidates[t.seq].instrument != BBB));
        let pairs: Vec<(&str, &str)> = r
            .comparisons
            .iter()
            .map(|c| (c.a.as_str(), c.b.as_str()))
            .collect();
        assert_eq!(pairs, [("jev", "rules"), ("jev_capped", "rules_capped")]);
        let g = r.gate.clone().unwrap();
        assert_eq!((g.decided, g.cut, g.take, g.skip), (9, 3, 6, 3));
        assert_eq!(g.model, "typesafe/jev-1.13-20260917");
        assert_eq!(r.calibration.as_ref().unwrap().n, 9);
        assert!(r.render_compact().contains(
            "jev gate xl_gate (typesafe/jev-1.13-20260917): decided 9 (cut 3 past --max-decisions)"
        ));

        let dir = write_run_dir(&p, &mut run).unwrap();
        // decisions.jsonl: one line per decision, by seq, trigger backtest.
        let lines = lines(&dir.join(DECISIONS_FILE));
        let sessions: Vec<String> = lines
            .iter()
            .map(|l| l["session_id"].as_str().unwrap().to_string())
            .collect();
        let want: Vec<String> = (0..9)
            .map(|i| format!("backtest:weekend_fade:{i}"))
            .collect();
        assert_eq!(sessions, want);
        assert!(lines.iter().all(|l| l["trigger"] == json!("backtest")));
        for arm in ["jev", "jev_capped", "rules", "rules_capped"] {
            assert!(dir.join(format!("trades-{arm}.jsonl")).exists(), "{arm}");
        }
        let md = std::fs::read_to_string(dir.join("report.md")).unwrap();
        for want in [
            "## Jev gate `xl_gate`",
            "| Decided · cut | 9 of 12 · 3 past `--max-decisions`",
            "| jev − rules |",
            "## Calibration (n 9",
        ] {
            assert!(md.contains(want), "missing `{want}` in:\n{md}");
        }
        // report.json carries the summary (floats may move a last digit on
        // the way back: compare the counts).
        let back: BacktestReport =
            serde_json::from_str(&std::fs::read_to_string(dir.join("report.json")).unwrap())
                .unwrap();
        let bg = back.gate.unwrap();
        assert_eq!(
            (
                bg.loop_name.as_str(),
                bg.model.as_str(),
                bg.decided,
                bg.cut,
                bg.take,
                bg.skip
            ),
            ("xl_gate", g.model.as_str(), 9, 3, 6, 3)
        );
        assert_eq!(bg.calibration.n, g.calibration.n);
        // Nothing outside the claimed run dir.
        let entries: Vec<String> = std::fs::read_dir(&backtests)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(entries, vec![run.report.run_id.clone()]);
    }

    /// `sections()` with `[backtest]` = `extra` lines + [`BACKTEST`].
    fn sections_with(extra: &str) -> SandboxSections {
        let mut s = sections();
        s.backtest = Some(toml::from_str(&format!("{extra}\n{BACKTEST}")).unwrap());
        s
    }

    /// Regression (review: unbounded output): a run whose candidates pass
    /// `[backtest] max_candidates` stops in `prepare` — before any arm, run
    /// dir or file — with a message naming the guard and how to narrow it.
    #[tokio::test]
    async fn a_run_past_max_candidates_fails_before_any_arm() {
        let tmp = tempfile::tempdir().unwrap();
        let store = seeded(&tmp.path().join("state")).await;
        let backtests = tmp.path().join("state/backtests");
        let mut e = env(store, &backtests);
        e.sections = Arc::new(sections_with("max_candidates = 11"));
        let err = prepare(&e, job(SpecSource::Strategy("weekend_fade".into())))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("strategy `weekend_fade`: more than 11 candidates")
                && err.contains("[backtest] max_candidates")
                && err.contains("min_abs_signal_bps"),
            "{err}"
        );
        assert!(!backtests.exists(), "nothing written");
        // At the cap: runs.
        e.sections = Arc::new(sections_with("max_candidates = 12"));
        let p = prepare(&e, job(SpecSource::Strategy("weekend_fade".into())))
            .await
            .unwrap();
        assert_eq!(p.set.candidates.len(), 12);
    }

    /// Regression (review: run dirs never pruned): with `keep_runs` the
    /// oldest run dirs beyond the count go when a run is written — never
    /// the run just written (even when the clock reads older than a kept
    /// one), never the decision cache or anything not named like a run.
    #[tokio::test]
    async fn old_run_dirs_are_pruned_on_write_never_the_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let store = seeded(&tmp.path().join("state")).await;
        let backtests = tmp.path().join("state/backtests");
        std::fs::create_dir_all(&backtests).unwrap();
        for f in ["decision-cache.db", "decision-cache.db-wal", "notes.txt"] {
            std::fs::write(backtests.join(f), "x").unwrap();
        }
        let old: Vec<String> = (1..=11)
            .map(|d| format!("202609{d:02}T120000Z-old_run"))
            .collect();
        for (i, id) in old.iter().enumerate() {
            std::fs::create_dir(backtests.join(id)).unwrap();
            std::fs::write(backtests.join(id).join("report.json"), i.to_string()).unwrap();
        }
        // Same second, suffixed: -2 is newer than the plain id.
        std::fs::create_dir(backtests.join("20260911T120000Z-old_run-2")).unwrap();
        // A run from a clock ahead of this one, and a dir an operator pinned.
        std::fs::create_dir(backtests.join("20991231T000000Z-future")).unwrap();
        std::fs::create_dir(backtests.join("keep-20260901T120000Z-old_run")).unwrap();
        let mut e = env(store, &backtests);
        e.sections = Arc::new(sections_with("keep_runs = 10"));
        let p = prepare(&e, job(SpecSource::Strategy("weekend_fade".into())))
            .await
            .unwrap();
        let mut run = evaluate(&p, Vec::new()).unwrap();
        let dir = write_run_dir(&p, &mut run).unwrap();
        assert!(dir.join("report.json").is_file(), "the new run stays");
        let mut left: Vec<String> = std::fs::read_dir(&backtests)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        left.sort();
        let mut want: Vec<String> = vec![
            "20260905T120000Z-old_run".into(),
            "20260906T120000Z-old_run".into(),
            "20260907T120000Z-old_run".into(),
            "20260908T120000Z-old_run".into(),
            "20260909T120000Z-old_run".into(),
            "20260910T120000Z-old_run".into(),
            "20260911T120000Z-old_run".into(),
            "20260911T120000Z-old_run-2".into(),
            "20991231T000000Z-future".into(),
            run.report.run_id.clone(),
            "decision-cache.db".into(),
            "decision-cache.db-wal".into(),
            "keep-20260901T120000Z-old_run".into(),
            "notes.txt".into(),
        ];
        want.sort();
        // 10 run dirs: the future one, the new one and the 8 newest old ones.
        assert_eq!(left, want);
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
