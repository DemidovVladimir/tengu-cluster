//! `backtest` — the Architect's second tool (xlab, `docs/xlab-2026-10-01.md`
//! § 8): one strategy — a `[backtest.strategies]` name, or an inline spec
//! (the Architect's level-2 capability, § 5) — run on the sandbox's
//! warehouse `<state dir>/market.db` by the use case `application/backtest/`
//! as `tengu backtest` runs it, without the Jev gate (it spends money: the
//! CLI's `--gate`) and without a fetch (the Architect backfills first with
//! `market_history` `fetch = true`). No network, no LLM. With `run_id` it runs
//! nothing and reads a stored run's rows instead (`rows.rs`).
//!
//! | Step | Rule |
//! |---|---|
//! | Refuse | no `[xmarket]` ⇒ `state_dir_missing`; `market.db` not openable ⇒ `market_data_unavailable`; no `[backtest]` ⇒ `backtest_config_missing` |
//! | Args (strict) | a run: `strategy` xor `spec`; `from` / `to` (epoch ms, RFC 3339 or a UTC date; default the earliest stored bar of the run's instruments / now); `split` (`time:<t>` \| `instruments:<id,…>`); `holdout` (bool, needs `split`). A read: `run_id` + `view` / `arm` / `limit` / `holdout`, no run argument. An unknown key, a wrong type, a bad split or a mix is an error: nothing runs |
//! | Spec | an object (or a string holding one), named by its `name`, else [`ARCHITECT_SPEC`]; refused before any read with every problem at once — `StrategySpec::from_value`'s, then an unknown `@universe` and a calendar that is no exchange `[xmarket.calendars]` row — one line each, so the Architect fixes them in one go; an unknown `strategy` lists the library |
//! | Holdout (`holdout.rs`) | a `split` without `holdout` runs the in-sample half only (hidden: a time split ends the decisions at it, an instruments split leaves its ids' candidates out); `holdout: true` runs both halves and is recorded in `<state dir>/backtests/holdout-reads.jsonl` (`holdout read #n for this spec`); a read with no holdout candidate is refused |
//! | Size | after `prepare`, a run of more than [`MAX_TOOL_ROWS`] candidates or candidate skips is refused (every one would be written to the run dir: candidates, trades and refusals per arm, skips) — name the knobs that make the rule pickier; the operator's CLI has no such limit |
//! | Run | `prepare` (series from `market.db`, read only; candidates) → `evaluate` (`research`, + `capped` with `[risk]` + `[paper]`) → `write_run_dir` (`<state dir>/backtests/<run id>/`), the last two on a blocking thread (the bootstrap is CPU) |
//! | Row | `backtest/1:<run id>` (`domain/backtest/report.rs`), ttl 0: every call is a new run, never cached (recorded when `[recorder]` takes `backtest/1`); a holdout read adds the feature `holdout_reads` (this spec's #n) |
//! | Text | [`render`]: line 1, features and errors as `render_text` gives them (no `data`: the report is in the run dir), the arm / split / instrument / skip lines of `render_compact`, the best and worst periods, the hidden-holdout or holdout-read line, the decision window, the run's instruments without a cost entry (`no_costs`: full ids, at most [`NO_COST_CHARS`] of them, then a count), `spec_sha256`, how to read the run's rows (`run_id` + `view`) — never the run dir's path (outside every fs root: the operator's) — ≈ 2 KB on the 75-name library, ≤ [`TEXT_MAX_CHARS`] crowded (the cap test below), whole under a 16k local model's 8 192-char cap |
//!
//! The run id embeds the UTC second of the call (`run_dir.rs`): two runs
//! never share a dir, and the conformance harness normalises the stamp like
//! any other time (`tests/bridge_conformance.rs::normalize`).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use serde_json::Value;
use tracing::info;

use super::holdout::{self, HoldoutRead, ReadCount};
use super::rows::{self, RowsRequest};
use super::{defs, field, object_args, opt_str, opt_time, XlabShared};
use crate::adapters::outbound::market_data::market_state_dir;
use crate::adapters::outbound::tools::hyperliquid::store_live;
use crate::application::backtest::{
    capability_refusal, evaluate, prepare, spec_of, write_run_dir, BacktestEnv, BacktestJob,
    BacktestRun, Prepared, SpecSource,
};
use crate::config::backtest::BacktestConfig;
use crate::config::sections::SandboxSections;
use crate::config::xmarket::backtests_dir;
use crate::domain::backtest::costs::cost_for;
use crate::domain::backtest::report::PRIMARY_ARM;
use crate::domain::backtest::spec::SplitSpec;
use crate::domain::calendar::Calendar;
use crate::domain::message::ToolDef;
use crate::domain::observation::{now_ms, set_int, ObsSource, Observation, MAX_FEATURES};
use crate::domain::time::fmt_time;
use crate::domain::tools as names;
use crate::ports::tool::{Tool, ToolCtx, ToolOutput};

/// The name an inline spec without its own `name` runs under.
pub(crate) const ARCHITECT_SPEC: &str = "architect_spec";
/// Refusal without `[backtest]`.
pub(crate) const BACKTEST_CONFIG_MISSING: &str = "backtest_config_missing";
/// Chars of ids the `no_costs` line lists before "… and N more".
pub(crate) const NO_COST_CHARS: usize = 800;
/// Candidates — and, apart, candidate skips — a tool run may hold (module
/// table): every one is a line of the run dir. The library's largest run
/// (`xyz_funding_carry`, 13 016 candidates) writes ≈ 22 MB.
pub(crate) const MAX_TOOL_ROWS: usize = 50_000;
/// The text's budget on a crowded run, by construction: `render_compact` ≤
/// 3 000, [`NO_COST_CHARS`], the fixed lines (the cap test holds it).
#[cfg(test)]
pub(crate) const TEXT_MAX_CHARS: usize = 5_120;
/// Periods each of the text's best / worst lines names.
const TEXT_PERIODS: usize = 3;

const ARGS: &[&str] = &[
    "strategy", "spec", "from", "to", "split", "holdout", "run_id", "view", "arm", "limit",
];
/// A run's own arguments: never with `run_id`.
const RUN_ARGS: &[&str] = &["strategy", "spec", "from", "to", "split"];
/// A read's own arguments: only with `run_id`.
const ROWS_ARGS: &[&str] = &["view", "arm", "limit"];

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
        let request = parse_args(args, bt)?;
        let now = now_ms();
        let backtests = backtests_dir(market_state_dir(&sections)?);
        let (job, holdout) = match request {
            Request::Rows(req) => {
                let call_id = ctx.call_id.map(str::to_string);
                // Streams a run's trades file: off the async workers.
                let text = tokio::task::spawn_blocking(move || {
                    rows::read(&backtests, &req, call_id.as_deref(), now)
                })
                .await
                .map_err(|e| anyhow!("{}: the read stopped: {e}", names::BACKTEST))??;
                return Ok(ToolOutput {
                    text,
                    observation: None,
                });
            }
            Request::Run { job, holdout } => (job, holdout),
        };
        check_spec(bt, &sections, &job.spec)?;
        let env = BacktestEnv {
            store,
            backtests_dir: backtests.clone(),
            sections: Arc::clone(&sections),
            now_ms: now,
        };
        let ran = run(&env, job, holdout).await?;
        // The path is the operator's (logs), never the model's text.
        info!(
            run_id = %ran.run.report.run_id,
            run_dir = %ran.dir.display(),
            "backtest tool: run written"
        );
        // A holdout read is recorded before anything shows it.
        let read = match &ran.halves {
            Halves::Both(split) => {
                let r = &ran.run.report;
                let read = HoldoutRead::new(
                    "backtest",
                    &r.run_id,
                    &r.spec_sha256,
                    &r.strategy,
                    split,
                    ctx.call_id,
                    now,
                );
                let count = holdout::record(&backtests, &read).map_err(|e| {
                    anyhow!(
                        "{}: run {} finished, but its holdout read could not be recorded \
                         ({e:#}) — not shown",
                        names::BACKTEST,
                        r.run_id
                    )
                })?;
                Some(count)
            }
            _ => None,
        };
        let mut obs = Observation::of(names::BACKTEST, &ran.run.report, now, 0, ObsSource::Live);
        if let Some(count) = read {
            if obs.features.len() < MAX_FEATURES {
                set_int(&mut obs.features, "holdout_reads", Some(count.spec as i64));
            }
        }
        store_live(self.shared.store.as_deref(), &obs).await;
        Ok(ToolOutput {
            text: render(&obs, &ran, bt, now, read),
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

/// What a call asks for (module table).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Request {
    /// Run a spec; `holdout` = show both halves of its split (a read).
    Run { job: BacktestJob, holdout: bool },
    /// A stored run's rows (`rows.rs`).
    Rows(RowsRequest),
}

/// The request `args` make (module table); `bt` names the library in errors.
pub(crate) fn parse_args(args: &Value, bt: &BacktestConfig) -> Result<Request> {
    let tool = names::BACKTEST;
    let o = object_args(tool, args, ARGS)?;
    let holdout = match field(o, "holdout") {
        None => false,
        Some(Value::Bool(b)) => *b,
        Some(v) => bail!("{tool}: 'holdout' must be a boolean, got {v}"),
    };
    let given = |keys: &[&'static str]| -> Vec<&'static str> {
        keys.iter()
            .copied()
            .filter(|k| field(o, k).is_some())
            .collect()
    };
    if field(o, "run_id").is_some() {
        let mixed = given(RUN_ARGS);
        if !mixed.is_empty() {
            bail!(
                "{tool}: 'run_id' reads a stored run's rows — give it with view / arm / limit / \
                 holdout only, not {}",
                mixed.join(", ")
            );
        }
        return rows::parse(o, holdout).map(Request::Rows);
    }
    let stray = given(ROWS_ARGS);
    if !stray.is_empty() {
        bail!(
            "{tool}: {} go(es) with 'run_id' (reading a stored run's rows by its run id)",
            stray.join(", ")
        );
    }
    let job = parse_job(o, bt)?;
    if holdout && job.split.is_none() {
        bail!(
            "{tool}: \"holdout\": true reads a split's holdout half — give 'split' too \
             (time:<t> or instruments:<id,…>)"
        );
    }
    Ok(Request::Run { job, holdout })
}

/// A run's job (module table).
fn parse_job(o: &serde_json::Map<String, Value>, bt: &BacktestConfig) -> Result<BacktestJob> {
    let tool = names::BACKTEST;
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
        // Not a tool argument: W1 pins the tool's schema (`tool_schema:backtest`).
        data_through_ms: None,
    })
}

/// Every problem of the job's spec before any read (module table):
/// `spec_of`'s, then — for a spec that parses — its kind against a bound
/// `[generation]` (`capability_unavailable`), its universe and calendar.
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
    // `[generation]`: a kind outside the bound generation (`capability_unavailable`).
    out.extend(capability_refusal(sections, &spec));
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

/// How a run treats its split (`holdout.rs`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Halves {
    /// No split.
    Whole,
    /// The in-sample half only; `left_out` = candidates on a listed id
    /// left out (an instruments split).
    Hidden { split: SplitSpec, left_out: usize },
    /// Both halves side by side: a holdout read.
    Both(SplitSpec),
}

/// One run of the tool: the report and arms, its dir, the ids it read that
/// have no cost entry, and its halves.
#[derive(Debug)]
pub(crate) struct Ran {
    pub run: BacktestRun,
    pub dir: PathBuf,
    /// Loaded ids without a cost (spec `costs` or a `[backtest.costs]`
    /// prefix): skipped as `no_costs`, never traded. Sorted, in full.
    pub no_costs: Vec<String>,
    pub halves: Halves,
}

/// The size refusal (module table): a run of more than [`MAX_TOOL_ROWS`]
/// candidates or candidate skips.
fn check_size(p: &Prepared) -> Result<()> {
    let (c, s) = (p.set.candidates.len(), p.set.skipped.len());
    if c <= MAX_TOOL_ROWS && s <= MAX_TOOL_ROWS {
        return Ok(());
    }
    bail!(
        "{}: strategy `{}` makes {c} candidate(s) and {s} candidate skip(s) over {} → {} — a \
         tool run holds at most {MAX_TOOL_ROWS} of each (every one is a line of the run dir: \
         candidates, trades and refusals per arm, skips). Make the rule pickier (threshold_bps, \
         min_abs_signal_bps, cooldown_bars, top_n, min_entry_trades, min_volume_ratio) or \
         narrow the universe or from / to, then call again",
        names::BACKTEST,
        p.spec.name,
        fmt_time(p.params.from_ms),
        fmt_time(p.params.to_ms)
    )
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

/// `prepare` → [the holdout left out | checked readable] → the size check →
/// `evaluate` → `write_run_dir` (module table); `holdout` = run both halves
/// of the job's split. Every error starts `backtest: `.
pub(crate) async fn run(env: &BacktestEnv, mut job: BacktestJob, holdout: bool) -> Result<Ran> {
    let tool = names::BACKTEST;
    let hidden = if holdout {
        None
    } else {
        holdout::hide(&mut job, env.now_ms)?
    };
    let read = job.split.clone();
    let mut prepared = prepare(env, job).await.map_err(|e| {
        let context = hidden.as_ref().map(holdout::hidden_context);
        anyhow!("{tool}: {e:#}{}", context.unwrap_or_default())
    })?;
    let halves = match (hidden, read) {
        (Some(split), _) => {
            let left_out = holdout::leave_out(&mut prepared, &split);
            Halves::Hidden { split, left_out }
        }
        (None, Some(split)) => {
            holdout::check_readable(&prepared, &split)?;
            Halves::Both(split)
        }
        (None, None) => Halves::Whole,
    };
    check_size(&prepared)?;
    let no_costs = no_cost_ids(&prepared);
    let done = tokio::task::spawn_blocking(move || -> Result<(BacktestRun, PathBuf)> {
        let mut run = evaluate(&prepared, Vec::new())?;
        let dir = write_run_dir(&prepared, &mut run)?;
        Ok((run, dir))
    })
    .await
    .map_err(|e| anyhow!("{tool}: the run stopped: {e}"))?;
    let (run, dir) = done.map_err(|e| anyhow!("{tool}: {e:#}"))?;
    Ok(Ran {
        run,
        dir,
        no_costs,
        halves,
    })
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
        shown.push_str(&format!(" … and {} more", ids.len() - listed));
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

/// The best and worst [`TEXT_PERIODS`] periods of the primary arm by Σ net
/// USD (more than one period).
fn period_lines(ran: &Ran) -> Vec<String> {
    let Some(arm) = ran.run.arms.get(PRIMARY_ARM) else {
        return Vec::new();
    };
    let rows = rows::per_key(
        arm.trades
            .iter()
            .map(|t| (t.period.as_str(), t.net_usd, t.net_bps)),
    );
    if rows.len() < 2 {
        return Vec::new();
    }
    let k = rows.len().min(TEXT_PERIODS);
    let show = |rs: &mut dyn Iterator<Item = &(String, usize, f64, f64)>| {
        rs.map(|(p, n, usd, _)| format!("{p} {usd:+.2} usd (n {n})"))
            .collect::<Vec<_>>()
            .join(" · ")
    };
    vec![
        format!("best periods: {}", show(&mut rows[..k].iter())),
        format!(
            "worst periods: {} ({} periods: view periods)",
            show(&mut rows[rows.len() - k..].iter().rev()),
            rows.len()
        ),
    ]
}

/// The tool's text (module table); `read` = a recorded holdout read.
/// One arm half: `n=… mean_net_bps=… ci95=[…] hit=… net_usd=… sharpe=…`.
fn half_line(s: &crate::domain::backtest::stats::Summary) -> String {
    let mean = s
        .mean_net_bps
        .map_or_else(|| "—".to_string(), |m| format!("{m:+.2}"));
    let ci = match (s.ci95_lo_bps, s.ci95_hi_bps) {
        (Some(lo), Some(hi)) => format!("[{lo:+.1}, {hi:+.1}]"),
        _ => "—".to_string(),
    };
    let hit = s
        .hit_rate
        .map_or_else(|| "—".to_string(), |h| format!("{h:.2}"));
    let sharpe = s
        .sharpe
        .map_or_else(|| "—".to_string(), |x| format!("{x:.2}"));
    format!(
        "n={} mean_net_bps={mean} ci95={ci} hit={hit} net_usd={:+.2} sharpe={sharpe}",
        s.n, s.net_usd
    )
}

/// A holdout read's text (module table): out-of-sample figures only up
/// front — line 1 is the holdout half of the research arm, then its capped
/// half, the in-sample half as a labelled reference, the holdout's best and
/// worst periods, the read count, data gaps, the window, `spec_sha256` and
/// the rows hint. No whole-window figure and no features dump (the typed row
/// keeps them for programs): a live Architect quoted a whole-window line 1
/// as the holdout result twice, also with a labelled holdout line under it.
fn holdout_text(
    obs: &Observation,
    ran: &Ran,
    split: &SplitSpec,
    read: Option<ReadCount>,
) -> String {
    let report = &ran.run.report;
    let halves = |arm: &str| report.arms.get(arm).and_then(|a| a.split.as_ref());
    let mut lines = Vec::new();
    let head = format!(
        "backtest {} {} {} {} HOLDOUT of {split} (out-of-sample)",
        report.run_id, report.strategy, report.kind, report.interval
    );
    match halves(PRIMARY_ARM) {
        Some(h) => lines.push(format!(
            "{head} research: {} | {}",
            half_line(&h.holdout),
            obs.status.as_str()
        )),
        None => lines.push(format!("{head}: no split halves | {}", obs.status.as_str())),
    }
    for (name, a) in &report.arms {
        if name.as_str() == PRIMARY_ARM {
            continue;
        }
        if let Some(h) = &a.split {
            lines.push(format!("HOLDOUT {name}: {}", half_line(&h.holdout)));
        }
    }
    if let Some(h) = halves(PRIMARY_ARM) {
        lines.push(format!(
            "in-sample (already seen while tuning — reference only) research: {}",
            half_line(&h.in_sample)
        ));
    }
    if let Some(arm) = ran.run.arms.get(PRIMARY_ARM) {
        let rows = rows::per_key(
            arm.trades
                .iter()
                .filter(|t| {
                    let legs: Vec<&str> = t.legs.iter().map(|l| l.instrument.as_str()).collect();
                    split.is_holdout(t.decided_at_ms, &legs)
                })
                .map(|t| (t.period.as_str(), t.net_usd, t.net_bps)),
        );
        if !rows.is_empty() {
            let k = rows.len().min(TEXT_PERIODS);
            let show = |rs: &mut dyn Iterator<Item = &(String, usize, f64, f64)>| {
                rs.map(|(p, n, usd, _)| format!("{p} {usd:+.2} usd (n {n})"))
                    .collect::<Vec<_>>()
                    .join(" · ")
            };
            lines.push(format!(
                "holdout best periods: {}",
                show(&mut rows[..k].iter())
            ));
            lines.push(format!(
                "holdout worst periods: {} ({} holdout periods)",
                show(&mut rows[rows.len() - k..].iter().rev()),
                rows.len()
            ));
        }
    }
    if let Some(count) = read {
        lines.push(holdout::read_line(count, split));
    }
    for e in &obs.errors {
        lines.push(format!(
            "error {}: {} {}",
            e.field,
            e.class.as_str(),
            e.message
        ));
    }
    lines.push(format!(
        "decisions {} → {} (to exclusive) · {} instrument(s) · {} candidate(s)",
        fmt_time(report.from_ms),
        fmt_time(report.to_ms),
        report.n_instruments,
        report.n_candidates
    ));
    lines.push(format!("spec_sha256 {}", report.spec_sha256));
    lines.push(format!(
        "rows: backtest {{\"run_id\": \"{}\", \"view\": \"periods\"}} shows the in-sample rows; \
         + \"holdout\": true shows the holdout rows too (another counted read)",
        report.run_id
    ));
    lines.join("\n")
}

pub(crate) fn render(
    obs: &Observation,
    ran: &Ran,
    bt: &BacktestConfig,
    now_ms: i64,
    read: Option<ReadCount>,
) -> String {
    if let Halves::Both(split) = &ran.halves {
        return holdout_text(obs, ran, split, read);
    }
    let mut head = obs.clone();
    head.data = Value::Null;
    let report = &ran.run.report;
    let mut lines = vec![head.render_text(now_ms)];
    // Its line 1 is the row's headline, already in line 1 above; its data
    // notes are read with the notes view, not in report.md.
    let compact = report.render_compact();
    lines.extend(
        compact
            .lines()
            .skip(1)
            .map(|l| match l.strip_prefix("data notes: ") {
                Some(rest) => format!(
                    "data notes: {} (view notes)",
                    rest.split(' ').next().unwrap_or(rest)
                ),
                None => l.to_string(),
            }),
    );
    lines.extend(period_lines(ran));
    match (&ran.halves, read) {
        (Halves::Hidden { split, left_out }, _) => {
            lines.push(holdout::hidden_line(split, *left_out));
        }
        _ => {}
    }
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
    lines.push(format!(
        "rows: backtest {{\"run_id\": \"{}\", \"view\": \"periods\"}} (or \"instruments\", \"trades\", \
         \"notes\") — the run's files are outside your workspace",
        report.run_id
    ));
    lines.join("\n")
}

#[cfg(test)]
#[path = "run_tests.rs"]
mod tests;
