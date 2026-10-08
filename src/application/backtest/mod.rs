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

    /// The series range a run over `[from_ms, to_ms)` reads:
    /// `StrategySpec::data_range`, reaching further back for the longest
    /// `abdi_ranaldo` window among the `[backtest.costs]` its instruments
    /// resolve to.
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
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use serde_json::json;

    use super::*;
    use crate::adapters::outbound::market_data::SqliteMarketData;
    use crate::config::risk::{PaperConfig, RiskConfig};
    use crate::domain::backtest::engine::{SkipReason, Trade};
    use crate::domain::backtest::testkit::{et, nyse, utc, H};
    use crate::domain::calendar::Calendar;
    use crate::domain::lineage::generation::GenerationScope;
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
            data_through_ms: None,
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

    /// `[generation]`: a sandbox bound to W1 (the lineage fixture) refuses
    /// an inline `event_window` spec — W2-SIM's capability — with
    /// `capability_unavailable` before any read; bound to W2-SIM it runs.
    #[tokio::test]
    async fn a_bound_generation_refuses_a_kind_it_lacks() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/lineage/registry");
        let reg = crate::config::lineage::load_registry(&dir).unwrap();
        let bound = |g: &str| {
            let mut s = sections();
            s.generation = Some(Arc::new(
                crate::domain::lineage::generation::GenerationScope::of(&reg, g).unwrap(),
            ));
            Arc::new(s)
        };
        let tmp = tempfile::tempdir().unwrap();
        let store = seeded(&tmp.path().join("state")).await;
        let mut e = env(store, &tmp.path().join("state/backtests"));
        let event = SpecSource::Json {
            value: json!({"name": "news", "kind": "event_window", "interval": "1h",
                "events": [{"instrument": AAA, "t": "2026-09-10T14:00:00Z"}],
                "direction": "follow", "exit_after_mins": 120}),
            fallback_name: None,
        };
        e.sections = bound("W1");
        let err = prepare(&e, job(event.clone()))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.starts_with(
                "capability_unavailable: strategy `news`: strategy kind `event_window` is bound \
                 by capability `cap.event_window`, which generation `W1` does not include"
            ),
            "{err}"
        );
        // W1's own kinds run.
        prepare(&e, job(SpecSource::Strategy("weekend_fade".into())))
            .await
            .unwrap();
        e.sections = bound("W2-SIM");
        let p = prepare(&e, job(event)).await.unwrap();
        assert_eq!(p.spec.kind_name(), "event_window");
        let seen = p.set.candidates.len() + p.set.skip_counts().values().sum::<usize>();
        assert_eq!(seen, 1, "the one event, decided or skipped");
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

    /// Lineage D1: a run cut at `--data-through T` over the full warehouse
    /// is the run over a warehouse that held only what was known at T (the
    /// same arms, the same bound recorded) — so a rerun with a report's
    /// `data_through_ms` survives a grown `market.db`; without the cut the
    /// grown warehouse changes the run.
    #[tokio::test]
    async fn data_through_reruns_a_grown_warehouse_on_the_same_data() {
        let tmp = tempfile::tempdir().unwrap();
        let full = seeded(&tmp.path().join("full")).await;
        let through = utc("2026-09-22 00:00");
        // The warehouse as it was at `through`: bars closed by it, funding
        // stamped by it.
        let grown_later = SqliteMarketData::open(&tmp.path().join("then")).unwrap();
        for id in [AAA, BBB, CCC] {
            let bars = full.bars(id, Interval::H1, 0, i64::MAX).await.unwrap();
            let kept: Vec<Bar> = bars
                .bars
                .into_iter()
                .filter(|b| b.t_close_ms(Interval::H1) <= through)
                .collect();
            grown_later
                .put_bars(id, Interval::H1, "test", &kept)
                .await
                .unwrap();
            let f = full.funding(id, 0, i64::MAX).await.unwrap();
            let kept: Vec<FundingPoint> =
                f.points.into_iter().filter(|p| p.t_ms <= through).collect();
            grown_later.put_funding(id, "test", &kept).await.unwrap();
        }
        let then: Arc<dyn MarketDataStore> = Arc::new(grown_later);
        let backtests = tmp.path().join("backtests");
        let run_on = |store: Arc<dyn MarketDataStore>, data_through_ms: Option<i64>| {
            let e = env(store, &backtests);
            async move {
                let job = BacktestJob {
                    data_through_ms,
                    ..job(SpecSource::Strategy("weekend_fade".into()))
                };
                let p = prepare(&e, job).await.unwrap();
                evaluate(&p, Vec::new()).unwrap().report
            }
        };
        let at_the_time = run_on(Arc::clone(&then), None).await;
        let cut = run_on(Arc::clone(&full), Some(through)).await;
        let uncut = run_on(full, None).await;
        assert_eq!(at_the_time.data_through_ms, Some(through));
        assert_eq!(cut.data_through_ms, Some(through));
        assert_eq!(cut.arms, at_the_time.arms);
        assert_eq!(cut.n_candidates, at_the_time.n_candidates);
        assert!(
            cut.data_notes[0].starts_with("data through 2026-09-22T00:00:00Z (--data-through): ")
        );
        assert_ne!(uncut.arms, cut.arms, "the later data changes the run");
        assert!(uncut.data_through_ms > Some(through));
    }

    /// Phase 7: a spec with `labels` reads `market.db`'s events, event
    /// coverage and first bars — AAA's 8-K of Fri 2026-09-18 16:30 New York
    /// makes its 09-20 decision NEWS (skipped), the uncovered CCC is
    /// UNCERTAIN (and named in a note), the first two weekends are within
    /// 14 days of the first bar (UNCERTAIN); `candidates.jsonl` carries the
    /// labels, `skips.json` counts `label_skipped:NEWS`; `--data-through`
    /// cuts a later event and clips coverage, labels before it unchanged.
    /// The same spec without `labels` loads none of it.
    #[tokio::test]
    async fn labels_read_events_and_coverage_and_respect_data_through() {
        use crate::domain::backtest::labels::InfoLabel;
        use crate::domain::marketdata::{EventCoverage, MarketEvent};
        let tmp = tempfile::tempdir().unwrap();
        let store = seeded(&tmp.path().join("state")).await;
        let filing = |published_ms: i64, id: &str| MarketEvent {
            instrument: AAA.into(),
            published_ms,
            kind: "filing".into(),
            id: id.into(),
            form: "8-K".into(),
            title: None,
        };
        let late = utc("2026-09-28 01:00");
        store
            .put_events(
                "sec",
                &[
                    filing(et("2026-09-18 16:30"), "0000000001-26-000001"),
                    filing(late, "0000000001-26-000002"),
                ],
            )
            .await
            .unwrap();
        for id in [AAA, BBB] {
            store
                .put_event_coverage(&EventCoverage {
                    instrument: id.into(),
                    source: "sec".into(),
                    from_ms: utc("2026-01-01 00:00"),
                    to_ms: utc("2026-10-01 00:00"),
                    covered: true,
                    note: None,
                    fetched_at_ms: utc("2026-10-01 00:00"),
                })
                .await
                .unwrap();
        }
        let spec = |labels: Option<Value>| {
            let mut v = json!({"name": "wl", "kind": "weekend_window", "universe": "@xyz",
                "interval": "1h", "calendar": "us_equity", "direction": "fade"});
            if let Some(l) = labels {
                v["labels"] = l;
            }
            SpecSource::Json {
                value: v,
                fallback_name: None,
            }
        };
        let e = env(store, &tmp.path().join("state/backtests"));
        let p = prepare(&e, job(spec(Some(json!({"skip": ["NEWS"]})))))
            .await
            .unwrap();
        assert_eq!(p.md.info.listed_ms[AAA], utc("2026-09-01 00:00"));
        assert_eq!(p.md.info.events[AAA].len(), 2);
        let label = |id: &str, t: i64| {
            p.set
                .candidates
                .iter()
                .find(|c| c.instrument == id && c.decided_at_ms == t)
                .and_then(|c| c.info_label)
        };
        let sep20 = utc("2026-09-20 22:00");
        assert_eq!(label(BBB, sep20), Some(InfoLabel::Noise));
        assert_eq!(label(CCC, sep20), Some(InfoLabel::Uncertain));
        assert_eq!(label(AAA, sep20), None, "skipped");
        assert!(p.set.skipped.iter().any(|k| k.instrument == AAA
            && k.decided_at_ms == sep20
            && k.reason == SkipReason::LabelSkipped
            && k.info_label == Some(InfoLabel::News)));
        assert_eq!(
            label(AAA, utc("2026-09-13 22:00")),
            Some(InfoLabel::Uncertain),
            "12.9 days after the first bar"
        );
        assert!(p.set.candidates.iter().all(|c| c.info_label.is_some()));
        assert!(
            p.set.notes.contains(&format!(
                "labels: no event source covers 1 of the 3 instruments — their candidates are \
                 UNCERTAIN unless an event says NEWS: {CCC}"
            )),
            "{:?}",
            p.set.notes
        );
        assert!(p
            .set
            .notes
            .iter()
            .any(|n| n.starts_with("labels: NEWS 1 · ")));
        let mut run = evaluate(&p, Vec::new()).unwrap();
        assert_eq!(run.report.skipped["label_skipped:NEWS"], 1);
        let dir = write_run_dir(&p, &mut run).unwrap();
        let rows = lines(&dir.join("candidates.jsonl"));
        assert!(!rows.is_empty() && rows.iter().all(|r| r["info_label"].is_string()));
        let skips: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("skips.json")).unwrap())
                .unwrap();
        assert_eq!(skips["candidates"]["label_skipped:NEWS"], 1);

        // --data-through: the later event is cut (counted), coverage
        // clipped; every decision by then labelled the same.
        let through = utc("2026-09-28 00:00");
        let cut = prepare(
            &e,
            BacktestJob {
                data_through_ms: Some(through),
                ..job(spec(Some(json!({"skip": ["NEWS"]}))))
            },
        )
        .await
        .unwrap();
        assert_eq!(cut.md.info.events[AAA].len(), 1);
        assert!(cut.md.info.coverage[AAA]
            .iter()
            .all(|c| c.to_ms == through + 1));
        let upto = |s: &CandidateSet| -> Vec<Candidate> {
            s.candidates
                .iter()
                .filter(|c| c.decided_at_ms <= through)
                .cloned()
                .collect()
        };
        assert_eq!(upto(&cut.set), upto(&p.set));
        assert!(!upto(&cut.set).is_empty());

        // Without labels: nothing loaded, nothing labelled, no label note.
        let plain = prepare(&e, job(spec(None))).await.unwrap();
        assert_eq!(plain.md.info, InfoData::default());
        assert!(plain.set.candidates.iter().all(|c| c.info_label.is_none()));
        assert!(!plain.set.notes.iter().any(|n| n.starts_with("labels:")));
    }

    /// Lineage D3: a run the bound generation's registry cites is never
    /// pruned and does not count in `keep_runs` — the oldest two of 13 are
    /// cited, so with 10 kept the two uncited runs after them go.
    #[tokio::test]
    async fn retention_never_prunes_a_run_the_registry_cites() {
        let tmp = tempfile::tempdir().unwrap();
        let store = seeded(&tmp.path().join("state")).await;
        let backtests = tmp.path().join("state/backtests");
        std::fs::create_dir_all(&backtests).unwrap();
        let old: Vec<String> = (1..=13)
            .map(|d| format!("202609{d:02}T120000Z-old_run"))
            .collect();
        for id in &old {
            std::fs::create_dir(backtests.join(id)).unwrap();
        }
        let scope = GenerationScope {
            id: "W1".into(),
            bound_kinds: BTreeMap::from([("weekend_window".into(), "cap".into())]),
            available_kinds: BTreeSet::from(["weekend_window".into()]),
            // The state dir is `<tmp>/state`: its name is the locator's state.
            cited_runs: BTreeMap::from([(
                "state".into(),
                BTreeSet::from([old[0].clone(), old[1].clone()]),
            )]),
            ..Default::default()
        };
        let mut e = env(store, &backtests);
        let mut s = sections_with("keep_runs = 10");
        s.generation = Some(Arc::new(scope));
        e.sections = Arc::new(s);
        let p = prepare(&e, job(SpecSource::Strategy("weekend_fade".into())))
            .await
            .unwrap();
        assert_eq!(
            p.keep_cited,
            BTreeSet::from([old[0].clone(), old[1].clone()])
        );
        let mut run = evaluate(&p, Vec::new()).unwrap();
        write_run_dir(&p, &mut run).unwrap();
        let mut left: Vec<String> = std::fs::read_dir(&backtests)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        left.sort();
        // Kept: the 2 cited, the new run and the 9 newest uncited (old[4..]).
        let mut want: Vec<String> = old[..2].iter().chain(&old[4..]).cloned().collect();
        want.push(run.report.run_id.clone());
        want.sort();
        assert_eq!(left, want);
    }

    /// Strategy ranking (SR-2): `report.json` records the cohort identity —
    /// the bound generation, the sorted ids read and every id's resolved
    /// cost; `[backtest.costs]` moves `costs_sha256` and never
    /// `spec_sha256`, an `exclude` moves `instruments_sha256`.
    /// 13 old run dirs of the state `state`, `keep_runs = 10`, `sections`
    /// edited by `with`; one run written. The run dirs left, sorted, and the
    /// new run's id.
    async fn retained(
        with: impl FnOnce(&mut SandboxSections, &std::path::Path, &[String]),
    ) -> (Vec<String>, Vec<String>, String) {
        let tmp = tempfile::tempdir().unwrap();
        let store = seeded(&tmp.path().join("state")).await;
        let backtests = tmp.path().join("state/backtests");
        std::fs::create_dir_all(&backtests).unwrap();
        let old: Vec<String> = (1..=13)
            .map(|d| format!("202609{d:02}T120000Z-old_run"))
            .collect();
        for id in &old {
            std::fs::create_dir(backtests.join(id)).unwrap();
        }
        let mut e = env(store, &backtests);
        let mut s = sections_with("keep_runs = 10");
        with(&mut s, &tmp.path().join("state"), &old);
        e.sections = Arc::new(s);
        let p = prepare(&e, job(SpecSource::Strategy("weekend_fade".into())))
            .await
            .unwrap();
        let mut run = evaluate(&p, Vec::new()).unwrap();
        write_run_dir(&p, &mut run).unwrap();
        let mut left: Vec<String> = std::fs::read_dir(&backtests)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        left.sort();
        (left, old, run.report.run_id)
    }

    /// Lineage D3 for an unbound sandbox (xlab-w2 shares the state dir
    /// holding W1's cited runs): the `[strategy_ranking]` registry's cited
    /// runs are kept and not counted, as a bound generation's are.
    #[tokio::test]
    async fn retention_keeps_runs_an_unbound_sandbox_registry_cites() {
        let (left, old, new) = retained(|s, _, old| {
            assert!(s.generation.is_none());
            s.ranking = Some(Arc::new(crate::config::strategy_ranking::RankingSection {
                cited_runs: BTreeMap::from([
                    (
                        "state".into(),
                        BTreeSet::from([old[0].clone(), old[1].clone()]),
                    ),
                    ("other".into(), BTreeSet::from([old[2].clone()])),
                ]),
                ..Default::default()
            }));
        })
        .await;
        // Kept: the 2 cited of this state, the new run, the 9 newest uncited.
        let mut want: Vec<String> = old[..2].iter().chain(&old[4..]).cloned().collect();
        want.push(new);
        want.sort();
        assert_eq!(left, want);
    }

    /// A published ranking's `latest.json` keeps every run of this state it
    /// cites (`run:<state>/<run id>` at any depth), unbound and without a
    /// `[strategy_ranking]` section — another state's runs and a dated
    /// ranking keep nothing; an unreadable `latest.json` keeps nothing.
    #[tokio::test]
    async fn retention_keeps_runs_the_latest_ranking_cites() {
        let (left, old, new) = retained(|s, state_dir, old| {
            assert!(s.generation.is_none() && s.ranking.is_none());
            let contract = state_dir.join("strategy-rankings/rank.t");
            std::fs::create_dir_all(contract.join("2026-10-08")).unwrap();
            let latest = json!({
                "contract": "rank.t",
                "cohorts": [{"rows": [
                    {"strategy": "a", "run": format!("run:state/{}", old[0])},
                    {"strategy": "b", "run": format!("run:other/{}", old[2])},
                ]}],
                "failed": [{"run": format!("run:state/{}/report.json", old[1])}],
            });
            std::fs::write(contract.join("latest.json"), latest.to_string()).unwrap();
            let dated = json!({"run": format!("run:state/{}", old[3])});
            std::fs::write(contract.join("2026-10-08/ranking.json"), dated.to_string()).unwrap();
            let broken = state_dir.join("strategy-rankings/rank.broken");
            std::fs::create_dir_all(&broken).unwrap();
            std::fs::write(broken.join("latest.json"), "{ not json").unwrap();
        })
        .await;
        let mut want: Vec<String> = old[..2].iter().chain(&old[4..]).cloned().collect();
        want.push(new);
        want.sort();
        assert_eq!(left, want);
    }

    #[tokio::test]
    async fn report_records_its_cohort_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let store = seeded(&tmp.path().join("state")).await;
        let backtests = tmp.path().join("state/backtests");
        let mut e = env(store, &backtests);
        let run = |e: BacktestEnv, spec: SpecSource| async move {
            let p = prepare(&e, job(spec)).await.unwrap();
            let mut run = evaluate(&p, Vec::new()).unwrap();
            let dir = write_run_dir(&p, &mut run).unwrap();
            let on_disk: BacktestReport =
                serde_json::from_str(&std::fs::read_to_string(dir.join("report.json")).unwrap())
                    .unwrap();
            assert_eq!(on_disk.instruments_sha256, run.report.instruments_sha256);
            assert_eq!(on_disk.costs_sha256, run.report.costs_sha256);
            assert_eq!(on_disk.generation, run.report.generation);
            run.report
        };
        let fade = || SpecSource::Strategy("weekend_fade".into());
        let base = run(e.clone(), fade()).await;
        let xyz = sections().backtest.unwrap().costs["hyperliquid:xyz:"].clone();
        let cost = serde_json::to_value(&xyz).unwrap();
        assert_eq!(base.generation, None, "unbound");
        assert_eq!(
            base.instruments_sha256.as_deref(),
            Some(canonical_sha256(&json!([AAA, BBB, CCC])).as_str())
        );
        assert_eq!(
            base.costs_sha256.as_deref(),
            Some(canonical_sha256(&json!({AAA: cost, BBB: cost, CCC: cost})).as_str())
        );
        // A cost for one name: the costs hash moves, the spec hash does not.
        let mut s = sections();
        let bt = s.backtest.as_mut().unwrap();
        let mut dearer = xyz.clone();
        dearer.taker_fee_bps = 4.5;
        bt.costs.insert(AAA.into(), dearer);
        e.sections = Arc::new(s);
        let costly = run(e.clone(), fade()).await;
        assert_eq!(costly.spec_sha256, base.spec_sha256);
        assert_eq!(costly.instruments_sha256, base.instruments_sha256);
        assert_ne!(costly.costs_sha256, base.costs_sha256);
        // An excluded name: the instruments hash moves.
        let narrow = run(
            e.clone(),
            SpecSource::Json {
                value: json!({"name": "wf", "kind": "weekend_window", "universe": "@xyz",
                    "interval": "1h", "calendar": "us_equity", "direction": "fade",
                    "exclude": [CCC]}),
                fallback_name: None,
            },
        )
        .await;
        assert_eq!(
            narrow.instruments_sha256.as_deref(),
            Some(canonical_sha256(&json!([AAA, BBB])).as_str())
        );
        // Bound: the generation id.
        let mut s = sections();
        s.generation = Some(Arc::new(GenerationScope {
            id: "W1".into(),
            bound_kinds: BTreeMap::from([("weekend_window".into(), "cap".into())]),
            available_kinds: BTreeSet::from(["weekend_window".into()]),
            ..Default::default()
        }));
        e.sections = Arc::new(s);
        let bound = run(e, fade()).await;
        assert_eq!(bound.generation.as_deref(), Some("W1"));
        assert_eq!(bound.costs_sha256, base.costs_sha256);
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
