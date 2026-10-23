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
//! | 2 | [`prepare`] | `from` default = the earliest stored bar of those instruments at the spec's interval, `to` default = now; series over [`Resolved::data_window`]: bars at the interval, funding when the instrument's cost books it (always for `funding_carry`), ctx when its cost is `half_spread = ctx`; with `data_through_ms` (`--data-through`) the rows after it cut (`MarketData::cut_after`, a data note), else the newest row loaded recorded as the run's `data_through_ms` (lineage D1); `[backtest.splits]` applied to them (`MarketData::adjust_for_splits`: bars closed before each split ÷ ratio, volume × ratio, a bar straddling it dropped; a data note each, listed first); a spec with `labels` (`weekend_window`, Phase 7) also loads [`InfoData`] (`load_info`: each instrument's events over the window − `lookback_mins`, its event coverage, its earliest stored bar at any interval, its `[backtest.splits]`) before the cut — `--data-through` cuts events like rows and clips coverage — and a data note names the instruments no source covers (ids in full); `RunParams` from `[backtest]` (incl. `max_candidates`), `[xmarket.calendars]`; `engine::candidates` — a run past `max_candidates` stops here, before any arm or file; `RiskCaps` from `[risk]` + `[paper]`; the run id proposed |
//! | — | the Jev gate arm (`gate.rs`) | between `prepare` and `evaluate`: `run_gate` reads [`Prepared::set`] (candidates in decision order, features as-of) and decides them |
//! | 3 | [`evaluate`] · `gate::evaluate_gated` | arm `research` always; `capped` when the sandbox has `[risk]` + `[paper]`; then every extra `(name, candidates, Arm)` simulated over its own candidates, reported with `n_candidates` = their count and compared with the base arm of its kind (`research` / `capped`: mean net bps difference, paired bootstrap over periods); split halves when the job has a split. With the gate: `evaluate_gated` = `evaluate` + the gate's `rules` / `jev` arms (research + capped) over the decided candidates, comparisons, calibration, summary, `decisions.jsonl` |
//! | 4 | [`write_run_dir`] | `<backtests dir>/<run id>/` (`run_dir.rs`): `report.json`, `report.md`, `trades-<arm>.jsonl`, `candidates.jsonl`, `skips.json` + [`BacktestRun::extra_files`] (the gate's `decisions.jsonl`); then the run dirs beyond `[backtest] keep_runs` pruned, oldest first (never the decision cache or a cited run — `keep_cited`: [`cited_runs`]) |
//!
//! | Rule | Value |
//! |---|---|
//! | Run id | `<YYYYMMDDTHHMMSSZ>-<strategy>` from now (UTC); a taken one ⇒ `-2`, `-3`, … — proposed by `prepare`, claimed by `write_run_dir` (`create_dir`: a run that took it meanwhile moves this one to the next free suffix) |
//! | `spec_sha256` | sha256 hex (64 chars) of the canonical JSON of `StrategySpec::to_value` (defaults filled, keys sorted at every depth — `domain/canonical.rs`; `spec::spec_sha256`, also a generation's `spec:` pin) |
//! | Cohort identity ([`RunIdentity`], in `report.json`) | `generation` = the bound `[generation]` id (none when unbound); `instruments_sha256` = canonical sha256 of the sorted ids read ([`Resolved::instruments`]); `costs_sha256` = canonical sha256 of `{id: its resolved cost (the spec's, else the longest [backtest.costs] prefix) or null}` — costs sit outside `spec_sha256`; what a strategy ranking's cohort compares |
//! | `exclude` | never loaded; the engine still sees the whole universe and counts each excluded name as a skip (`excluded`) |
//! | Store | read only: a run never writes `market.db` |
//! | Generation | a `[generation]`-bound sandbox (`SandboxSections::generation`) runs only the kinds its capabilities bind: else `capability_unavailable: …` ([`capability_refusal`]) from `prepare`, before any read; the `backtest` tool lists it among the spec's problems |

pub(crate) mod gate;
pub(crate) mod run_dir;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use serde_json::Value;

pub(crate) use self::run_dir::write_run_dir;
use crate::config::backtest::BacktestConfig;
use crate::config::sections::SandboxSections;
use crate::config::xmarket::rankings_dir;
use crate::domain::backtest::costs::{cost_for, CostSpec, HalfSpread};
use crate::domain::backtest::engine::{
    candidates, simulate, Arm, ArmResult, Candidate, CandidateSet, MarketData, RiskCaps, RunParams,
};
use crate::domain::backtest::labels::{InfoData, LabelSpec};
use crate::domain::backtest::report::{BacktestReport, CAPPED_ARM, PRIMARY_ARM};
use crate::domain::backtest::spec::{
    spec_sha256, valid_name, SplitSpec, StrategyKind, StrategySpec,
};
use crate::domain::canonical::canonical_sha256;
use crate::domain::lineage::value::Locator;
use crate::domain::marketdata::{fmt_time, Interval, StockSplit};
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
    /// Read only what was known then (`MarketData::cut_after`); `None` =
    /// everything stored (lineage D1).
    pub data_through_ms: Option<i64>,
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

    /// The run's cohort identity (module table) under `sections`.
    pub(crate) fn identity(
        &self,
        sections: &SandboxSections,
        costs: &BTreeMap<String, CostSpec>,
    ) -> RunIdentity {
        let resolved: serde_json::Map<String, Value> = self
            .instruments
            .iter()
            .map(|id| {
                let cost = self
                    .cost(costs, id)
                    .and_then(|c| serde_json::to_value(c).ok())
                    .unwrap_or(Value::Null);
                (id.clone(), cost)
            })
            .collect();
        RunIdentity {
            generation: sections.generation.as_ref().map(|g| g.id.clone()),
            instruments_sha256: canonical_sha256(&serde_json::json!(self.instruments)),
            costs_sha256: canonical_sha256(&Value::Object(resolved)),
        }
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

/// What a strategy ranking compares two runs on, beside the spec, window and
/// data bound (module table): recorded in `report.json`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RunIdentity {
    pub generation: Option<String>,
    pub instruments_sha256: String,
    pub costs_sha256: String,
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
    /// [`Resolved::identity`]: recorded in the report.
    pub identity: RunIdentity,
    pub md: MarketData,
    pub params: RunParams,
    pub set: CandidateSet,
    /// `[risk]` + `[paper]` ⇒ the capped arm's caps.
    pub caps: Option<RiskCaps>,
    pub backtests_dir: PathBuf,
    /// `[backtest] keep_runs`: [`write_run_dir`] prunes the oldest run dirs
    /// beyond it (0 = keep all).
    pub keep_runs: usize,
    /// Run ids of this state dir a registry or a published ranking cites
    /// ([`cited_runs`]): never pruned, not counted in `keep_runs` (lineage D3).
    pub keep_cited: BTreeSet<String>,
    /// [`BacktestReport::data_through_ms`]: the `--data-through` bound, else
    /// the newest row loaded.
    pub data_through_ms: Option<i64>,
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

/// A sandbox bound to a generation (`[generation]`, `config/lineage.rs`)
/// runs only the strategy kinds its capabilities bind: the refusal, starting
/// `capability_unavailable:` and naming kind, capability and generation —
/// for a named, inline (`backtest` tool) or `--spec` spec alike.
pub(crate) fn capability_refusal(
    sections: &SandboxSections,
    spec: &StrategySpec,
) -> Option<String> {
    let why = sections
        .generation
        .as_ref()?
        .kind_refusal(spec.kind_name())?;
    Some(format!(
        "capability_unavailable: strategy `{}`: {why}",
        spec.name
    ))
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

/// What a labelled spec's labels read (module table, step 2;
/// `domain/backtest/labels.rs`): per instrument its events over
/// `[lo − lookback, hi)`, every source's event coverage, its earliest stored
/// bar at any interval; `[backtest.splits]` of the run's instruments.
async fn load_info(
    store: &dyn MarketDataStore,
    r: &Resolved,
    labels: &LabelSpec,
    splits: &BTreeMap<String, Vec<StockSplit>>,
    (lo, hi): (i64, i64),
) -> Result<InfoData> {
    let mut info = InfoData::default();
    let from = lo.saturating_sub(labels.lookback_ms());
    for id in &r.instruments {
        let events = store.events(id, from, hi).await?;
        if !events.is_empty() {
            info.events.insert(id.clone(), events);
        }
        let coverage = store.event_coverage(id).await?;
        if !coverage.is_empty() {
            info.coverage.insert(id.clone(), coverage);
        }
        let first = store
            .coverage(Some(id))
            .await?
            .into_iter()
            .filter(|row| row.kind == "bars")
            .map(|row| row.first_ms)
            .min();
        if let Some(first) = first {
            info.listed_ms.insert(id.clone(), first);
        }
        if let Some(list) = splits.get(id) {
            info.splits.insert(id.clone(), list.clone());
        }
    }
    Ok(info)
}

/// The labels' data note on coverage (module table): which instruments no
/// source covers at all — every candidate of theirs is UNCERTAIN unless an
/// event says NEWS. Ids in full.
fn coverage_note(r: &Resolved, info: &InfoData) -> Option<String> {
    let uncovered: Vec<&str> = r
        .instruments
        .iter()
        .filter(|id| {
            !info
                .coverage
                .get(id.as_str())
                .is_some_and(|cs| cs.iter().any(|c| c.covered))
        })
        .map(String::as_str)
        .collect();
    if uncovered.is_empty() {
        return None;
    }
    let n = r.instruments.len();
    Some(if uncovered.len() == n {
        format!(
            "labels: no event source covers any of the {n} instrument(s) — every candidate is \
             UNCERTAIN unless an event says NEWS (backfill the events first: tengu history events)"
        )
    } else {
        format!(
            "labels: no event source covers {} of the {n} instruments — their candidates are \
             UNCERTAIN unless an event says NEWS: {}",
            uncovered.len(),
            uncovered.join(", ")
        )
    })
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
    if let Some(refusal) = capability_refusal(&env.sections, &r.spec) {
        bail!("{refusal}");
    }
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
    if let Some(labels) = r.spec.labels() {
        md.info = load_info(env.store.as_ref(), &r, labels, &bt.stock_splits(), window).await?;
    }
    // The data this run reads, recorded so a rerun can read the same.
    let mut through_notes = Vec::new();
    if let Some(t) = job.data_through_ms {
        let cut = md.cut_after(t);
        if cut > 0 {
            through_notes.push(format!(
                "data through {} (--data-through): {cut} row(s) stored after it left out",
                fmt_time(t)
            ));
        }
    }
    let data_through_ms = job.data_through_ms.or_else(|| md.newest_ms());
    // Share splits before any decision reads a price.
    let mut split_notes = md.adjust_for_splits(&bt.stock_splits());
    if r.spec.labels().is_some() {
        split_notes.extend(coverage_note(&r, &md.info));
    }
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
    set.notes
        .splice(0..0, through_notes.into_iter().chain(split_notes));
    let run_id_base = run_dir::run_id_base(env.now_ms, &r.spec.name);
    let run_id = run_dir::propose_run_id(&env.backtests_dir, &run_id_base);
    let identity = r.identity(&env.sections, &bt.costs);
    Ok(Prepared {
        run_id,
        run_id_base,
        spec: r.spec,
        spec_value: r.spec_value,
        spec_sha256: r.spec_sha256,
        split: job.split,
        instruments: r.instruments,
        identity,
        md,
        params,
        set,
        caps: risk_caps(&env.sections),
        backtests_dir: env.backtests_dir.clone(),
        keep_runs: bt.keep_runs,
        keep_cited: cited_runs(&env.sections, &env.backtests_dir),
        data_through_ms,
    })
}

/// The runs of `backtests_dir` (`<state dir>/backtests`) retention never
/// prunes (lineage D3): those the bound generation's registry cites
/// (`GenerationScope::cited_runs`), those the `[strategy_ranking]` registry
/// cites (`RankingSection::cited_runs` — an unbound sandbox sharing the state
/// dir too), and those a published ranking of the state dir cites
/// ([`published_ranking_runs`]).
fn cited_runs(s: &SandboxSections, backtests_dir: &Path) -> BTreeSet<String> {
    let Some(state_dir) = backtests_dir.parent() else {
        return BTreeSet::new();
    };
    let Some(state) = state_dir.file_name().and_then(|n| n.to_str()) else {
        return BTreeSet::new();
    };
    let registries = [
        s.generation.as_ref().map(|g| &g.cited_runs),
        s.ranking.as_ref().map(|r| &r.cited_runs),
    ];
    let mut out: BTreeSet<String> = registries
        .into_iter()
        .flatten()
        .filter_map(|by_state| by_state.get(state))
        .flatten()
        .cloned()
        .collect();
    out.extend(published_ranking_runs(state_dir, state));
    out
}

/// Every run of `state` a published ranking cites: each
/// `run:<state>/<run id>` string anywhere in `<state dir>/strategy-rankings/
/// <contract id>/latest.json`. An unreadable file is a warning (its runs are
/// then not kept by it).
fn published_ranking_runs(state_dir: &Path, state: &str) -> BTreeSet<String> {
    fn walk(v: &Value, state: &str, out: &mut BTreeSet<String>) {
        match v {
            Value::String(s) if s.starts_with("run:") => {
                if let Ok(Locator::Run {
                    state: st, run_id, ..
                }) = s.parse::<Locator>()
                {
                    if st == state {
                        out.insert(run_id);
                    }
                }
            }
            Value::Array(a) => a.iter().for_each(|x| walk(x, state, out)),
            Value::Object(o) => o.values().for_each(|x| walk(x, state, out)),
            _ => {}
        }
    }
    let mut out = BTreeSet::new();
    let Ok(entries) = std::fs::read_dir(rankings_dir(state_dir)) else {
        return out;
    };
    for entry in entries.flatten() {
        let latest = entry.path().join("latest.json");
        if !latest.is_file() {
            continue;
        }
        let parsed = std::fs::read_to_string(&latest)
            .map_err(|e| e.to_string())
            .and_then(|t| serde_json::from_str::<Value>(&t).map_err(|e| e.to_string()));
        match parsed {
            Ok(v) => walk(&v, state, &mut out),
            Err(e) => tracing::warn!(
                file = %latest.display(),
                "run-dir retention: a published ranking does not read ({e}) — its runs are not kept by it"
            ),
        }
    }
    out
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
    report.data_through_ms = p.data_through_ms;
    report.generation = p.identity.generation.clone();
    report.instruments_sha256 = Some(p.identity.instruments_sha256.clone());
    report.costs_sha256 = Some(p.identity.costs_sha256.clone());
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
mod tests;
