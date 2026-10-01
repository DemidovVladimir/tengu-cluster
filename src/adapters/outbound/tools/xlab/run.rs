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
    evaluate, prepare, spec_of, write_run_dir, BacktestEnv, BacktestJob, BacktestRun, Prepared,
    SpecSource,
};
use crate::config::backtest::BacktestConfig;
use crate::config::sections::SandboxSections;
use crate::config::xmarket::backtests_dir;
use crate::domain::backtest::costs::cost_for;
use crate::domain::backtest::report::{BacktestReport, PRIMARY_ARM};
use crate::domain::backtest::spec::SplitSpec;
use crate::domain::calendar::Calendar;
use crate::domain::marketdata::fmt_time;
use crate::domain::message::ToolDef;
use crate::domain::observation::{now_ms, set_int, ObsSource, Observation, MAX_FEATURES};
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
/// `OUT-OF-SAMPLE (holdout of <split>) research: n=… mean_net_bps=… ci95=[…]
/// · in-sample n=… mean_net_bps=… ci95=[…]` — the result a holdout read is
/// judged on; `None` when the research arm has no split halves.
fn out_of_sample_line(report: &BacktestReport, split: &SplitSpec) -> Option<String> {
    let halves = report.arms.get(PRIMARY_ARM)?.split.as_ref()?;
    let half = |s: &crate::domain::backtest::stats::Summary| {
        let mean = s
            .mean_net_bps
            .map_or_else(|| "—".to_string(), |m| format!("{m:+.2}"));
        let ci = match (s.ci95_lo_bps, s.ci95_hi_bps) {
            (Some(lo), Some(hi)) => format!("[{lo:+.1}, {hi:+.1}]"),
            _ => "—".to_string(),
        };
        format!("n={} mean_net_bps={mean} ci95={ci}", s.n)
    };
    Some(format!(
        "OUT-OF-SAMPLE (holdout of {split}) research: {} · in-sample {} — judge the holdout on \
         this line; line 1 and the arm lines cover the whole window (in-sample + holdout)",
        half(&halves.holdout),
        half(&halves.in_sample)
    ))
}

pub(crate) fn render(
    obs: &Observation,
    ran: &Ran,
    bt: &BacktestConfig,
    now_ms: i64,
    read: Option<ReadCount>,
) -> String {
    let mut head = obs.clone();
    head.data = Value::Null;
    let report = &ran.run.report;
    let head_text = head.render_text(now_ms);
    let mut head_lines = head_text.lines();
    let mut lines: Vec<String> = head_lines.next().map(str::to_string).into_iter().collect();
    // A holdout read: line 1 and the arm lines cover the whole window; the
    // out-of-sample result goes right under line 1, labelled (a live
    // Architect quoted the whole-window CI as the holdout's).
    if let Halves::Both(split) = &ran.halves {
        if let Some(line) = out_of_sample_line(report, split) {
            lines.push(line);
        }
    }
    lines.extend(head_lines.map(str::to_string));
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
        (Halves::Both(split), Some(count)) => lines.push(holdout::read_line(count, split)),
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

    fn request(v: Value) -> Result<Request> {
        parse_args(&v, &backtest_config())
    }

    fn job(v: Value) -> Result<BacktestJob> {
        match request(v)? {
            Request::Run { job, holdout } => {
                assert!(!holdout);
                Ok(job)
            }
            Request::Rows(r) => panic!("a read: {r:?}"),
        }
    }

    /// A run and a read never mix; `holdout` needs a split (or a run id).
    #[test]
    fn run_and_read_arguments_do_not_mix() {
        let r = request(
            json!({"strategy": "weekend_fade", "split": "time:2026-09-15",
                               "holdout": true}),
        )
        .unwrap();
        assert!(matches!(r, Request::Run { holdout: true, .. }), "{r:?}");
        let r = request(
            json!({"run_id": "20261001T120034Z-weekend_fade", "view": "trades",
                               "limit": 3, "holdout": true}),
        )
        .unwrap();
        let Request::Rows(rows) = r else {
            panic!("{r:?}")
        };
        assert_eq!(
            (rows.run.as_str(), rows.limit, rows.holdout),
            ("20261001T120034Z-weekend_fade", 3, true)
        );
        for (v, needle) in [
            (
                json!({"strategy": "weekend_fade", "holdout": true}),
                "\"holdout\": true reads a split's holdout half — give 'split' too",
            ),
            (
                json!({"strategy": "weekend_fade", "holdout": "yes"}),
                "'holdout' must be a boolean",
            ),
            (
                json!({"run_id": "r", "strategy": "weekend_fade", "from": "2026-09-01"}),
                "'run_id' reads a stored run's rows — give it with view / arm / limit / holdout \
                 only, not strategy, from",
            ),
            (
                json!({"strategy": "weekend_fade", "view": "periods", "limit": 3}),
                "view, limit go(es) with 'run_id'",
            ),
            (json!({"run_id": "../conf/market.db"}), "is not a run id"),
            (json!({"run_id": 7}), "'run_id' must be a non-empty string"),
        ] {
            let e = request(v.clone()).unwrap_err().to_string();
            assert!(e.starts_with("backtest: "), "{e}");
            assert!(e.contains(needle), "{v}: {e}");
        }
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
        // The run's rows through the tool; never the path (the run dir is
        // outside every fs root: the model could not read it).
        assert!(
            text.ends_with(&format!(
                "\nrows: backtest {{\"run_id\": \"{}\", \"view\": \"periods\"}} (or \"instruments\", \
                 \"trades\", \"notes\") — the run's files are outside your workspace",
                report.run_id
            )),
            "{text}"
        );
        assert!(!text.contains(&state.display().to_string()), "{text}");
        assert!(
            !text.contains("report.md") && !text.contains("skips.json"),
            "{text}"
        );
        // Four weekends: the best and worst periods by Σ net USD.
        assert!(text.contains("\nbest periods: 2026-09-"), "{text}");
        assert!(text.contains(" periods: view periods)"), "{text}");
        assert!(!text.contains("no_costs:"), "{text}");
        assert!(
            !text.contains("\"spec\""),
            "no data JSON in the text: {text}"
        );

        // An inline spec without a name: architect_spec; a move trigger on
        // the same bars, with a split by instrument — its holdout hidden.
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
                .contains(&format!("\nholdout hidden: split instruments:{BBB} — the ")),
            "{}",
            out.text
        );
        assert!(!out.text.contains("holdout n="), "{}", out.text);
    }

    /// Runs `args` and returns the report and text.
    async fn exec(t: &BacktestTool, h: &TestHarness, args: Value) -> (BacktestReport, String) {
        let out = t.execute(&args, &h.ctx()).await.unwrap();
        let obs = out.observation.unwrap();
        (obs.typed().unwrap(), out.text)
    }

    fn trades_of(
        state: &Path,
        run_id: &str,
        arm: &str,
    ) -> Vec<crate::domain::backtest::engine::Trade> {
        let path = state
            .join("backtests")
            .join(run_id)
            .join(format!("trades-{arm}.jsonl"));
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn ledger(state: &Path) -> Vec<HoldoutRead> {
        std::fs::read_to_string(holdout::ledger_path(&state.join("backtests")))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    /// The holdout stays hidden while the Architect tunes (review finding:
    /// every split call printed the holdout's n, mean and CI): a time split
    /// runs the in-sample half only — exactly the full run's in-sample half,
    /// both arms — and writes no holdout trade; `holdout: true` shows both
    /// halves and every such read is a ledger line, counted per spec and
    /// per split.
    #[tokio::test]
    async fn a_split_hides_the_holdout_until_it_is_read_and_reads_are_counted() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let t = tool(Ok(seeded(&state).await), sections(&state));
        let h = TestHarness::new(tmp.path());
        let split_at = utc("2026-09-15 00:00");
        let call = json!({"strategy": "weekend_fade", "from": "2026-09-01",
                          "to": "2026-09-29", "split": "time:2026-09-15"});

        let (hidden, text) = exec(&t, &h, call.clone()).await;
        assert_eq!(
            hidden.split, None,
            "no halves: the run is the in-sample half"
        );
        assert_eq!(hidden.to_ms, split_at, "decisions end at the split");
        let research = trades_of(&state, &hidden.run_id, "research");
        assert!(!research.is_empty());
        assert!(research.iter().all(|t| t.decided_at_ms < split_at));
        assert!(trades_of(&state, &hidden.run_id, "capped")
            .iter()
            .all(|t| t.decided_at_ms < split_at));
        assert!(
            text.contains(
                "\nholdout hidden: split time:2026-09-15T00:00:00Z — no decision at or after the split \
                 (2026-09-15T00:00:00Z): no holdout trade was simulated; every figure above is \
                 the in-sample half."
            ),
            "{text}"
        );
        for leak in [
            "holdout n=",
            "holdout_mean_net_bps",
            "holdout_n",
            "holdout read #",
        ] {
            assert!(!text.contains(leak), "{leak}: {text}");
        }
        assert!(hidden.data_notes[0].starts_with(
            "holdout hidden (backtest tool): split time:2026-09-15T00:00:00Z — no decision at or after"
        ));
        assert!(ledger(&state).is_empty(), "tuning reads no holdout");

        // The one read: both halves, the in-sample half = the hidden run.
        let mut read = call.clone();
        read["holdout"] = json!(true);
        let out = t.execute(&read, &h.ctx()).await.unwrap();
        let obs = out.observation.unwrap();
        let full: BacktestReport = obs.typed().unwrap();
        for arm in ["research", "capped"] {
            let halves = full.arms[arm].split.as_ref().unwrap();
            assert_eq!(halves.in_sample, hidden.arms[arm].summary, "{arm}");
            assert!(halves.holdout.n > 0, "{arm}");
        }
        assert_eq!(obs.features["holdout_reads"], 1);
        assert!(obs.features.contains_key("holdout_mean_net_bps"));
        // The out-of-sample result sits right under line 1, labelled, with
        // the holdout half's own n / mean (line 1 covers the whole window).
        let oos = out.text.lines().nth(1).unwrap();
        let holdout = &full.arms["research"].split.as_ref().unwrap().holdout;
        assert!(
            oos.starts_with(&format!(
                "OUT-OF-SAMPLE (holdout of time:2026-09-15T00:00:00Z) research: n={} \
                 mean_net_bps={:+.2} ci95=",
                holdout.n,
                holdout.mean_net_bps.unwrap()
            )),
            "{oos}"
        );
        assert!(oos.contains("judge the holdout on this line"), "{oos}");
        assert!(
            !text.contains("OUT-OF-SAMPLE"),
            "a hidden run has no out-of-sample line: {text}"
        );
        assert!(
            out.text
                .contains("\nsplit time:2026-09-15T00:00:00Z research: in-sample n="),
            "{}",
            out.text
        );
        assert!(
            out.text.contains(
                "\nholdout read #1 for this spec · 1 read(s) of split time:2026-09-15T00:00:00Z \
                 in this sandbox, every spec — this first read is the spec's out-of-sample test\n"
            ),
            "{}",
            out.text
        );
        assert!(
            out.text
                .contains(&format!("\nspec_sha256 {}\n", full.spec_sha256)),
            "{}",
            out.text
        );
        let lines = ledger(&state);
        assert_eq!(lines.len(), 1);
        assert_eq!(
            (
                lines[0].via.as_str(),
                lines[0].run_id.as_str(),
                lines[0].spec_sha256.as_str(),
                lines[0].strategy.as_str(),
                lines[0].split.as_str()
            ),
            (
                "backtest",
                full.run_id.as_str(),
                full.spec_sha256.as_str(),
                "weekend_fade",
                "time:2026-09-15T00:00:00Z"
            )
        );

        // Peeking again is visible: #2 for this spec.
        let out = t.execute(&read, &h.ctx()).await.unwrap();
        assert!(
            out.text.contains("\nholdout read #2 for this spec")
                && out.text.contains("fitted to it: say so"),
            "{}",
            out.text
        );
        // Another spec on the same split: #1 for it, the split's third.
        let mut other = read.clone();
        other.as_object_mut().unwrap().remove("strategy");
        other["spec"] = json!({"name": "w_follow", "kind": "weekend_window", "universe": "@xyz",
            "interval": "1h", "calendar": "us_equity", "direction": "follow"});
        let out = t.execute(&other, &h.ctx()).await.unwrap();
        assert!(
            out.text.contains("\nholdout read #1 for this spec")
                && out
                    .text
                    .contains(" · 3 read(s) of split time:2026-09-15T00:00:00Z"),
            "{}",
            out.text
        );
        assert_eq!(ledger(&state).len(), 3);

        // A window inside the holdout cannot be tuned on; a read of a
        // window with no holdout candidate is refused and not counted.
        let mut late = call.clone();
        late["from"] = json!("2026-09-16");
        let e = t.execute(&late, &h.ctx()).await.unwrap_err().to_string();
        assert!(e.contains("is at or after the split"), "{e}");
        let mut early = read.clone();
        early["split"] = json!("time:2026-09-28T12:00:00Z");
        let e = t.execute(&early, &h.ctx()).await.unwrap_err().to_string();
        assert!(
            e.contains("none of the run's") && e.contains("nothing counted"),
            "{e}"
        );
        assert_eq!(ledger(&state).len(), 3);
    }

    /// An instruments split leaves the listed ids out while hidden: no trade
    /// on them, the same research trades as the full run's in-sample half.
    #[tokio::test]
    async fn an_instruments_split_leaves_the_holdout_ids_out() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let t = tool(Ok(seeded(&state).await), sections(&state));
        let h = TestHarness::new(tmp.path());
        let spec = json!({"name": "mt", "kind": "move_trigger", "universe": "@xyz",
            "interval": "1h", "lookback_bars": 1, "threshold_bps": 150, "direction": "fade",
            "hold_bars": 3});
        let call = json!({"spec": spec, "from": "2026-09-01", "to": "2026-09-29",
                          "split": format!("instruments:{BBB}")});
        let (hidden, text) = exec(&t, &h, call.clone()).await;
        let mut read = call.clone();
        read["holdout"] = json!(true);
        let (full, _) = exec(&t, &h, read).await;
        let key = |t: &crate::domain::backtest::engine::Trade| {
            (t.instrument.clone(), t.decided_at_ms, t.net_bps.to_bits())
        };
        let in_sample: Vec<_> = trades_of(&state, &full.run_id, "research")
            .iter()
            .filter(|t| t.instrument != BBB)
            .map(key)
            .collect();
        let shown: Vec<_> = trades_of(&state, &hidden.run_id, "research")
            .iter()
            .map(key)
            .collect();
        assert!(!shown.is_empty());
        assert_eq!(shown, in_sample);
        assert_eq!(
            hidden.arms["research"].summary,
            full.arms["research"].split.as_ref().unwrap().in_sample
        );
        let left_out = full.n_candidates - hidden.n_candidates;
        assert!(left_out > 0);
        assert!(
            text.contains(&format!(
                "holdout hidden: split instruments:{BBB} — the {left_out} candidate(s) trading a \
                 listed id were left out"
            )),
            "{text}"
        );
        // Candidates renumbered: seq = index.
        let cands: Vec<Value> = std::fs::read_to_string(
            state
                .join("backtests")
                .join(&hidden.run_id)
                .join("candidates.jsonl"),
        )
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
        assert!(cands
            .iter()
            .enumerate()
            .all(|(i, c)| c["seq"] == json!(i) && c["instrument"] != json!(BBB)));
    }

    /// A stored run's rows by run id (review finding: the text pointed at a
    /// run dir no agent path reaches): periods, instruments, trades, notes;
    /// a split run's holdout rows hidden unless read — and that read
    /// counted; ids only (no path, no traversal, no symlink).
    #[tokio::test]
    async fn a_stored_runs_rows_are_read_by_run_id() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let t = tool(Ok(seeded(&state).await), sections(&state));
        let h = TestHarness::new(tmp.path());
        let (whole, _) = exec(
            &t,
            &h,
            json!({"strategy": "weekend_fade", "from": "2026-09-01", "to": "2026-09-29"}),
        )
        .await;
        let rows = |v: Value| {
            let t = &t;
            let h = &h;
            async move {
                let out = t.execute(&v, &h.ctx()).await?;
                assert!(out.observation.is_none(), "a read is no new row");
                anyhow::Ok(out.text)
            }
        };
        let id = whole.run_id.clone();
        let research = &whole.arms["research"].summary;
        let text = rows(json!({"run_id": id, "view": "periods"}))
            .await
            .unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0],
            format!(
                "backtest rows {id} view=periods · weekend_fade weekend_window 1h arm=research · \
                 {} trade(s) · Σ net_usd {:+.2}",
                research.n, research.net_usd
            )
        );
        assert_eq!(
            lines[1],
            format!(
                "all {} period(s) by Σ net USD, best first:",
                research.n_periods
            )
        );
        // Self-describing rows: `#<rank> period=… n=… net_usd=…`.
        assert!(lines[2].starts_with("#1 period=2026-09-"), "{text}");
        assert_eq!(lines.len(), 2 + research.n_periods, "{text}");
        // Each period row: its trades and Σ net USD; together the arm's.
        let field = |l: &str, key: &str| -> f64 {
            l.split(' ')
                .find_map(|w| w.strip_prefix(key))
                .and_then(|v| v.parse().ok())
                .unwrap_or_else(|| panic!("no {key} in {l}"))
        };
        let total: f64 = lines[2..].iter().map(|l| field(l, "net_usd=")).sum();
        assert!(
            (total - research.net_usd).abs() < 0.01 * lines.len() as f64,
            "{text}"
        );
        let n: f64 = lines[2..].iter().map(|l| field(l, "n=")).sum();
        assert_eq!(n as usize, research.n, "{text}");
        let text = rows(json!({"run_id": id, "view": "instruments", "arm": "capped"}))
            .await
            .unwrap();
        assert!(text.contains(" arm=capped · "), "{text}");
        assert!(
            text.contains("\nall 2 instrument(s) by Σ net USD"),
            "{text}"
        );
        for id in [AAA, BBB] {
            assert!(text.contains(&format!(" instrument={id} ")), "{id}: {text}");
        }
        // Two best, two worst trades, the rest counted between them.
        let text = rows(json!({"run_id": id, "view": "trades", "limit": 2}))
            .await
            .unwrap();
        assert!(
            text.contains(&format!(
                "\nthe 2 best and 2 worst of {} trade(s) by Σ net USD:",
                research.n
            )),
            "{text}"
        );
        assert!(
            text.contains(&format!("\n… {} more\n", research.n - 4)),
            "{text}"
        );
        assert!(
            text.ends_with(&format!(
                " period={}",
                trades_of(&state, &id, "research")
                    .iter()
                    .min_by(|a, b| a.net_usd.total_cmp(&b.net_usd))
                    .unwrap()
                    .period
            )),
            "the worst trade last: {text}"
        );
        assert!(
            text.contains(&format!("\n#{} decided_at=", research.n)),
            "the worst's rank: {text}"
        );
        let text = rows(json!({"run_id": id, "view": "notes"})).await.unwrap();
        assert!(
            text.starts_with(&format!(
                "backtest rows {id} view=notes · weekend_fade weekend_window 1h\ndata notes ("
            )),
            "{text}"
        );

        // Ids only: a path, an unknown run, a symlinked run are refused.
        for (run, needle) in [
            ("../state", "is not a run id"),
            ("backtests/x", "is not a run id"),
            ("20200101T000000Z-nope", "no run `20200101T000000Z-nope`"),
        ] {
            let e = rows(json!({"run_id": run})).await.unwrap_err().to_string();
            assert!(e.contains(needle), "{run}: {e}");
        }
        let e = rows(json!({"run_id": "20200101T000000Z-nope"}))
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains(&format!("(the newest: {id})")), "{e}");
        #[cfg(unix)]
        {
            let outside = tmp.path().join("outside");
            std::fs::create_dir_all(&outside).unwrap();
            std::fs::write(outside.join("report.json"), "{}").unwrap();
            std::os::unix::fs::symlink(&outside, state.join("backtests/20200101T000000Z-link"))
                .unwrap();
            let e = rows(json!({"run_id": "20200101T000000Z-link"}))
                .await
                .unwrap_err()
                .to_string();
            assert!(e.contains("is a symlink"), "{e}");
        }
        let e = rows(json!({"run_id": id, "arm": "jev"}))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("has no arm `jev` (it has: capped, research)"),
            "{e}"
        );
        let e = rows(json!({"run_id": id, "holdout": true}))
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("has no split"), "{e}");

        // A split run (both halves, as the operator's CLI writes them):
        // the holdout rows stay hidden unless read, and that read counts.
        let (split, _) = exec(
            &t,
            &h,
            json!({"strategy": "weekend_fade", "from": "2026-09-01", "to": "2026-09-29",
                   "split": "time:2026-09-15", "holdout": true}),
        )
        .await;
        assert_eq!(ledger(&state).len(), 1);
        let halves = split.arms["research"].split.clone().unwrap();
        let text = rows(json!({"run_id": split.run_id, "view": "trades", "limit": 25}))
            .await
            .unwrap();
        assert!(
            text.contains(&format!(
                "\nsplit time:2026-09-15T00:00:00Z: holdout hidden — its {} trade(s) left out",
                halves.holdout.n
            )),
            "{text}"
        );
        assert!(
            text.contains(&format!(" · {} trade(s) · ", halves.in_sample.n)),
            "{text}"
        );
        assert!(!text.contains("2026-09-20T"), "no holdout decision: {text}");
        assert_eq!(ledger(&state).len(), 1, "a hidden read is not counted");
        let text = rows(json!({"run_id": split.run_id, "view": "periods", "holdout": true}))
            .await
            .unwrap();
        assert!(
            text.contains(&format!(
                "\nsplit time:2026-09-15T00:00:00Z: both halves shown ({} holdout trade(s)) · \
                 holdout read #2 for this spec",
                halves.holdout.n
            )),
            "{text}"
        );
        assert!(text.contains("holdout_n"), "{text}");
        let lines = ledger(&state);
        assert_eq!(lines.len(), 2);
        assert_eq!(
            (lines[1].via.as_str(), lines[1].run_id.as_str()),
            ("rows", split.run_id.as_str())
        );
    }

    /// A tool run is bounded (review finding: one in-bounds spec wrote 392
    /// MiB): more than `MAX_TOOL_ROWS` candidates is refused after
    /// `prepare`, before any arm or file, naming the knobs.
    #[tokio::test]
    async fn a_run_past_the_tool_limit_is_refused_before_any_file() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let store = SqliteMarketData::open(&state).unwrap();
        // One minute bars alternating 100 / 100.1: every close moves.
        let t0 = utc("2026-08-01 00:00");
        let n = MAX_TOOL_ROWS as i64 + 100;
        let bars: Vec<crate::domain::marketdata::Bar> = (0..n)
            .map(|i| {
                let c = if i % 2 == 0 { 100.0 } else { 100.1 };
                crate::domain::marketdata::Bar {
                    t_open_ms: t0 + i * 60_000,
                    o: c,
                    h: c,
                    l: c,
                    c,
                    v: 1.0,
                    n: Some(1),
                }
            })
            .collect();
        store
            .put_bars(AAA, Interval::M1, "test", &bars)
            .await
            .unwrap();
        let t = tool(Ok(Arc::new(store)), sections(&state));
        let h = TestHarness::new(tmp.path());
        let dense = json!({"name": "dense", "kind": "move_trigger", "universe": [AAA],
            "interval": "1m", "lookback_bars": 1, "threshold_bps": 0.01, "direction": "fade",
            "hold_bars": 1, "cooldown_bars": 0});
        let e = t
            .execute(&json!({"spec": dense}), &h.ctx())
            .await
            .unwrap_err()
            .to_string();
        // Every bar but the first has its lookback bar: 50 099 candidates.
        // The refusal names the spec, the bound and the knobs (an
        // application-level `[backtest]` cap at the same bound may refuse
        // first, in its own words — the same three facts).
        assert!(e.starts_with("backtest: strategy `dense`"), "{e}");
        assert!(
            e.contains(&MAX_TOOL_ROWS.to_string())
                && e.contains("threshold_bps")
                && e.contains("cooldown_bars"),
            "{e}"
        );
        assert!(
            e.contains(&format!("makes {} candidate(s)", n - 1)) || e.contains("max_candidates"),
            "{e}"
        );
        assert!(!state.join("backtests").exists(), "nothing written");
        // Pickier: the same bars run.
        let mut picky = dense.clone();
        picky["cooldown_bars"] = json!(10);
        let out = t.execute(&json!({"spec": picky}), &h.ctx()).await.unwrap();
        assert!(out.text.starts_with("backtest "), "{}", out.text);
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
    /// split read — both halves and the read line —, refusals and drops of
    /// every kind, every skip reason, data gaps in both arms, many notes,
    /// the period lines) and 1 000 long ids without costs — the text stays
    /// within [`TEXT_MAX_CHARS`], well inside the 16k window's cap (8 192
    /// chars), ids whole.
    #[tokio::test]
    async fn the_text_fits_a_local_models_result_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let env = BacktestEnv {
            store: seeded(&state).await,
            sections: Arc::new(sections(&state)),
            backtests_dir: state.join("backtests"),
            now_ms: utc("2026-10-01 12:00"),
        };
        let j = job(json!({"strategy": "weekend_fade", "from": "2026-09-01",
                           "to": "2026-09-29", "split": "time:2026-09-15"}))
        .unwrap();
        let mut ran = run(&env, j, true).await.unwrap();
        let Halves::Both(split) = ran.halves.clone() else {
            panic!("{:?}", ran.halves)
        };
        let report = &mut ran.run.report;
        assert_eq!(report.arms.len(), 2, "the tool runs research + capped");
        assert!(report.arms["research"].split.is_some());
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
        ran.no_costs = ids.clone();
        let obs = Observation::of(names::BACKTEST, &ran.run.report, 0, 0, ObsSource::Live);
        let count = ReadCount {
            spec: 12_345,
            split: 67_890,
        };
        assert_eq!(split, SplitSpec::Time(utc("2026-09-15 00:00")));
        let text = render(&obs, &ran, &backtest_config(), 0, Some(count));
        let cap = tool_result_char_budget(16_384);
        assert!(
            text.chars().count() <= TEXT_MAX_CHARS && TEXT_MAX_CHARS < cap * 3 / 4,
            "{} chars of a {cap} cap:\n{text}",
            text.chars().count()
        );
        for want in [
            "\nsplit time:2026-09-15T00:00:00Z research: in-sample n=",
            "\nbest periods: ",
            "\nholdout read #12345 for this spec",
            "\ndata notes: 500 (view notes)",
            "\nrows: backtest {\"run_id\": ",
        ] {
            assert!(text.contains(want), "{want}: {text}");
        }
        let line = text.lines().find(|l| l.starts_with("no_costs: ")).unwrap();
        assert!(line.contains(&ids[0]) && line.contains(&ids[10]), "{line}");
        assert!(line.contains(" more — give the spec"), "{line}");
        // Every id the line shows is whole.
        for id in line
            .split([',', ' '])
            .filter(|w| w.starts_with("robinhood:"))
        {
            assert!(ids.contains(&id.to_string()), "cut id {id}");
        }
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
