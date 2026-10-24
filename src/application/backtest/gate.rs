//! The Jev gate arm (`docs/xlab-2026-10-01.md` § 7; PRD § 38 baseline C,
//! § 37 calibration): the sandbox's `[decision_loops.<gate>]` decides every
//! rule candidate on history — the real `DecisionLoop` on a `SimClock` set
//! to the decision instant, terminal actions only, Jev behind the decision
//! cache — then the rules arm (every decided candidate) and the jev arm (the
//! taken ones) are simulated side by side. Pure pieces:
//! `domain/backtest/gate.rs`; the cached engine and the loops:
//! `bootstrap::decision::build_gate`.
//!
//! | Step | Rule |
//! |---|---|
//! | Order + cap | candidates by `seq`; the first `max_decisions` are decided, the rest are counted as `cut` and logged — never a silent cap |
//! | Workers | K = [`Gate::workers`]; each owns its loop + clock and pulls the next candidate off one shared queue: `clock.set(decided_at_ms)`, then `decide_terminal(gate_event, "backtest:<strategy>:<seq>")`; results in `seq` order |
//! | Determinism | replay loops keep no history (`build_replay_loop`): a request is a function of its candidate alone, so verdicts, cache keys and their order never depend on K |
//! | Failures | a failed call (offline miss, Jev error, open circuit) ⇒ class `error`, counted, one warn line at the end; the run goes on |
//! | Cache | `DecisionEngine::cache_stats` after − before: the run's hits / misses / errors |
//! | [`gate_arms`] | rules = `simulate` of the decided candidates, jev = of the taken ones — research, and capped when caps are given; with a cut, their decision range ends at the first candidate never asked (what their Sharpe annualises over); calibration (p(take) vs the rules arm's trade winning, by `seq`) + paired differences → `GateSummary` |
//! | [`GateArms::add_to_report`] | arms `rules` · `jev` (+ `rules_capped` · `jev_capped`), comparisons jev − rules put first (the row's `diff_bps`), the calibration, the summary (`BacktestReport::gate`: `report.md`'s gate section, the CLI lines, the row's `jev_*`) |
//! | [`GateArms::add_to_run`] | the same into a run, + the four arms' trades (`trades-<arm>.jsonl`) — one source of truth: `gate_arms`' simulations, never re-run |
//! | [`GateAudit`] | the replay loops audit to a temp file outside the state dir (removed on drop: a failed run leaves nothing behind); [`evaluate_gated`] moves its lines, ordered by seq, into the run dir's `decisions.jsonl` (`write_run_dir` claims the dir) |
//! | [`evaluate_gated`] | `tengu backtest --gate`'s step 3: `evaluate` (research, capped) + [`gate_arms`] → [`GateArms::add_to_run`] + `decisions.jsonl` |

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use futures::future::join_all;
use serde_json::Value;
use tracing::{debug, warn};

use super::{evaluate, BacktestRun, Prepared};
use crate::application::decision_loop::DecisionLoop;
use crate::domain::backtest::engine::{
    simulate, Arm, ArmResult, Candidate, MarketData, RiskCaps, RunParams,
};
use crate::domain::backtest::gate::{
    gate_event, p_take, GateDecision, GateInputs, GateSummary, JEV_ARM, JEV_CAPPED_ARM, RULES_ARM,
    RULES_CAPPED_ARM,
};
use crate::domain::backtest::report::{ArmComparison, BacktestReport};
use crate::domain::backtest::spec::StrategySpec;
use crate::domain::decision::StepOutcome;
use crate::ports::clock::SimClock;
use crate::ports::decision::{CacheStats, DecisionEngine};

/// One worker: its clock (set to each decision instant) and its replay loop
/// on that clock.
pub(crate) type GateWorker = (Arc<SimClock>, Arc<DecisionLoop>);

/// What decides: `bootstrap::decision::build_gate`.
pub(crate) struct Gate {
    /// `[decision_loops.<name>]`.
    pub loop_name: String,
    /// The engine every worker's loop asks: the model (the cache key's slug)
    /// and the cache counters.
    pub engine: Arc<dyn DecisionEngine>,
    /// K ≥ 1; each loop is history-free (`build_replay_loop`).
    pub workers: Vec<GateWorker>,
}

/// [`run_gate`]'s result.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct GateRun {
    pub loop_name: String,
    /// The engine's slug.
    pub model: String,
    pub strategy: String,
    /// One per decided candidate, by `seq`.
    pub decisions: Vec<GateDecision>,
    /// Candidates asked: the first `max_decisions` by `seq`.
    pub decided: usize,
    /// Candidates past `max_decisions`, never asked.
    pub cut: usize,
    /// The run's counters; `None` = an engine without a cache.
    pub cache: Option<CacheStats>,
}

/// `backtest:<strategy>:<seq>` — one gate decision's session (audit line,
/// metrics).
pub(crate) fn session_id(strategy: &str, seq: usize) -> String {
    format!("backtest:{strategy}:{seq}")
}

fn since(after: CacheStats, before: CacheStats) -> CacheStats {
    CacheStats {
        hits: after.hits.saturating_sub(before.hits),
        misses: after.misses.saturating_sub(before.misses),
        errors: after.errors.saturating_sub(before.errors),
    }
}

/// One worker: candidates off the shared queue until it is empty.
async fn drain(
    queue: &[&Candidate],
    next: &AtomicUsize,
    (clock, gate_loop): &GateWorker,
    strategy: &str,
) -> Vec<GateDecision> {
    let mut out = Vec::new();
    while let Some(c) = queue.get(next.fetch_add(1, Ordering::Relaxed)) {
        clock.set(c.decided_at_ms);
        let event = gate_event(strategy, c);
        let decided = gate_loop
            .decide_terminal(&event, &session_id(strategy, c.seq))
            .await;
        out.push(match decided {
            Ok(v) => GateDecision::decided(c.seq, v),
            Err(e) => {
                let error = format!("{e:#}");
                debug!(seq = c.seq, instrument = %c.instrument, %error, "jev gate: call failed");
                GateDecision::failed(c.seq, error)
            }
        });
    }
    out
}

/// Decide `candidates` with `gate` (module table): the first
/// `max_decisions` by `seq`, K workers, results by `seq`. `Err` only for a
/// run that cannot start (no worker, a `seq` twice); a failed call is a
/// class-`error` decision.
pub(crate) async fn run_gate(
    candidates: &[Candidate],
    strategy: &str,
    gate: &Gate,
    max_decisions: usize,
) -> Result<GateRun> {
    if gate.workers.is_empty() {
        bail!("jev gate `{}`: no worker to decide with", gate.loop_name);
    }
    let mut queue: Vec<&Candidate> = candidates.iter().collect();
    queue.sort_by_key(|c| c.seq);
    if let Some(w) = queue.windows(2).find(|w| w[0].seq == w[1].seq) {
        bail!(
            "jev gate `{}`: candidate seq {} appears twice",
            gate.loop_name,
            w[0].seq
        );
    }
    let cut = queue.len().saturating_sub(max_decisions);
    queue.truncate(max_decisions);
    if cut > 0 {
        warn!(
            gate = %gate.loop_name,
            strategy,
            candidates = queue.len() + cut,
            max_decisions,
            cut,
            "jev gate: the latest {cut} candidates are past --max-decisions and are not decided"
        );
    }
    let before = gate.engine.cache_stats();
    let next = AtomicUsize::new(0);
    let workers = gate
        .workers
        .iter()
        .map(|w| drain(&queue, &next, w, strategy));
    let mut decisions: Vec<GateDecision> = join_all(workers).await.into_iter().flatten().collect();
    decisions.sort_by_key(|d| d.seq);
    let cache = gate
        .engine
        .cache_stats()
        .map(|after| since(after, before.unwrap_or_default()));
    let failed: Vec<&GateDecision> = decisions.iter().filter(|d| d.error.is_some()).collect();
    if let Some(first) = failed.first() {
        warn!(
            gate = %gate.loop_name,
            strategy,
            failed = failed.len(),
            decided = decisions.len(),
            first_seq = first.seq,
            first_error = first.error.as_deref().unwrap_or_default(),
            "jev gate: calls failed; those candidates are class error (no trade)"
        );
    }
    Ok(GateRun {
        loop_name: gate.loop_name.clone(),
        model: gate.engine.model().to_string(),
        strategy: strategy.to_string(),
        decided: decisions.len(),
        cut,
        decisions,
        cache,
    })
}

/// The gate's arms and summary (module table).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct GateArms {
    /// Seqs of the decided candidates found in the candidate list, and of
    /// the taken ones.
    pub decided: Vec<usize>,
    pub taken: Vec<usize>,
    /// Research arms: every decided candidate / the taken ones.
    pub rules: ArmResult,
    pub jev: ArmResult,
    /// The same under `[risk]` caps, when given.
    pub rules_capped: Option<ArmResult>,
    pub jev_capped: Option<ArmResult>,
    pub summary: GateSummary,
}

/// Simulate the rules and jev arms of `run` over `candidates` (the list
/// `run_gate` decided) and summarize the gate (module table).
/// `cost_per_decision_usd`: `domain::backtest::gate::COST_PER_DECISION_USD`
/// online, 0 offline (no call is made).
pub(crate) fn gate_arms(
    run: &GateRun,
    candidates: &[Candidate],
    spec: &StrategySpec,
    md: &MarketData,
    params: &RunParams,
    caps: Option<&RiskCaps>,
    cost_per_decision_usd: f64,
) -> GateArms {
    let decided: BTreeSet<usize> = run.decisions.iter().map(|d| d.seq).collect();
    let taken: BTreeSet<usize> = run
        .decisions
        .iter()
        .filter(|d| d.class.trades())
        .map(|d| d.seq)
        .collect();
    let pick = |seqs: &BTreeSet<usize>| -> Vec<Candidate> {
        candidates
            .iter()
            .filter(|c| seqs.contains(&c.seq))
            .cloned()
            .collect()
    };
    let (on_decided, on_taken) = (pick(&decided), pick(&taken));
    // A cut gate decided only the earliest candidates: its arms' decision
    // range ends at the first one it never asked (Sharpe's annualisation).
    let mut params = params.clone();
    if let Some(first_cut) = candidates
        .iter()
        .filter(|c| !decided.contains(&c.seq))
        .map(|c| c.decided_at_ms)
        .min()
    {
        params.to_ms = params.to_ms.min(first_cut);
    }
    let params = &params;
    let arm = |cs: &[Candidate], a: Arm| simulate(spec, md, params, cs, a);
    let rules = arm(&on_decided, Arm::Research);
    let jev = arm(&on_taken, Arm::Research);
    let capped = caps.map(|caps| {
        (
            arm(&on_decided, Arm::Capped(caps.clone())),
            arm(&on_taken, Arm::Capped(caps.clone())),
        )
    });
    let cache = run.cache.unwrap_or_default();
    let summary = GateSummary::new(&GateInputs {
        loop_name: &run.loop_name,
        model: &run.model,
        decisions: &run.decisions,
        cut: run.cut,
        cache_hits: cache.hits,
        cache_misses: cache.misses,
        cache_errors: cache.errors,
        cost_per_decision_usd,
        rules: &rules.trades,
        jev: &jev.trades,
        capped: capped
            .as_ref()
            .map(|(r, j)| (r.trades.as_slice(), j.trades.as_slice())),
        bootstrap: params.bootstrap,
        seed: params.seed,
    });
    let (rules_capped, jev_capped) = match capped {
        Some((r, j)) => (Some(r), Some(j)),
        None => (None, None),
    };
    let seqs = |cs: &[Candidate]| cs.iter().map(|c| c.seq).collect::<Vec<_>>();
    GateArms {
        decided: seqs(&on_decided),
        taken: seqs(&on_taken),
        rules,
        jev,
        rules_capped,
        jev_capped,
        summary,
    }
}

impl GateArms {
    /// Put the gate into `report` (module table): the four arms, jev − rules
    /// first among the comparisons (research, then capped), the calibration
    /// and the summary (`report.gate`: its `report.md` section, CLI lines and
    /// `jev_*` row features).
    pub(crate) fn add_to_report(&self, report: &mut BacktestReport) {
        report.add_arm(RULES_ARM, self.decided.len(), &self.rules);
        report.add_arm(JEV_ARM, self.taken.len(), &self.jev);
        if let (Some(r), Some(j)) = (&self.rules_capped, &self.jev_capped) {
            report.add_arm(RULES_CAPPED_ARM, self.decided.len(), r);
            report.add_arm(JEV_CAPPED_ARM, self.taken.len(), j);
        }
        let diffs = [
            (JEV_ARM, RULES_ARM, &self.summary.diff_ci),
            (
                JEV_CAPPED_ARM,
                RULES_CAPPED_ARM,
                &self.summary.diff_ci_capped,
            ),
        ];
        let earlier = std::mem::take(&mut report.comparisons);
        report.comparisons = diffs
            .into_iter()
            .filter_map(|(a, b, d)| {
                d.as_ref().map(|diff| ArmComparison {
                    a: a.to_string(),
                    b: b.to_string(),
                    diff: diff.clone(),
                })
            })
            .chain(earlier)
            .collect();
        report.calibration = Some(self.summary.calibration.clone());
        report.gate = Some(self.summary.clone());
    }

    /// [`add_to_report`](Self::add_to_report) on `run.report`, and the four
    /// simulations into `run.arms` (their `trades-<arm>.jsonl`, `skips.json`
    /// rows) — the report and the files read the same `ArmResult`s.
    pub(crate) fn add_to_run(self, run: &mut BacktestRun) {
        self.add_to_report(&mut run.report);
        let capped = match (self.rules_capped, self.jev_capped) {
            (Some(r), Some(j)) => vec![(RULES_CAPPED_ARM, r), (JEV_CAPPED_ARM, j)],
            _ => Vec::new(),
        };
        for (name, arm) in [(RULES_ARM, self.rules), (JEV_ARM, self.jev)]
            .into_iter()
            .chain(capped)
        {
            run.arms.insert(name.to_string(), arm);
        }
    }
}

/// `decisions.jsonl` — the gate's audit in the run dir.
pub(crate) const DECISIONS_FILE: &str = "decisions.jsonl";

/// The gate's audit while the run dir is unclaimed (module table): a temp
/// file outside the state dir, deleted on drop.
pub(crate) struct GateAudit {
    file: tempfile::NamedTempFile,
}

impl GateAudit {
    pub(crate) fn new() -> Result<Self> {
        let file = tempfile::Builder::new()
            .prefix("tengu-gate-")
            .suffix(".jsonl")
            .tempfile()
            .context("create the jev gate's temp audit file")?;
        Ok(Self { file })
    }

    /// Where the replay loops append (`build_gate`'s `audit_path`).
    pub(crate) fn path(&self) -> &Path {
        self.file.path()
    }

    /// The audit lines, verbatim, ordered by their decision's seq (session
    /// `backtest:<strategy>:<seq>`; K workers append in completion order); a
    /// line without one keeps its place after them.
    pub(crate) fn lines_by_seq(&self, strategy: &str) -> Result<String> {
        let path = self.path();
        let text =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let prefix = format!("backtest:{strategy}:");
        let seq = |line: &str| -> Option<usize> {
            let v: Value = serde_json::from_str(line).ok()?;
            v.get("session_id")?
                .as_str()?
                .strip_prefix(&prefix)?
                .parse()
                .ok()
        };
        let mut lines: Vec<(Option<usize>, &str)> = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| (seq(l), l))
            .collect();
        // Stable: `None` sorts after every seq, in file order.
        lines.sort_by_key(|(s, _)| s.unwrap_or(usize::MAX));
        Ok(lines.into_iter().map(|(_, l)| format!("{l}\n")).collect())
    }
}

/// `tengu backtest --gate`'s step 3 (module table): `evaluate`'s base arms,
/// then the gate's arms, comparisons, calibration and summary over the
/// candidates `gate` decided, and its audit as `decisions.jsonl`.
/// `cost_per_decision_usd`: `COST_PER_DECISION_USD` online, 0 offline.
pub(crate) fn evaluate_gated(
    p: &Prepared,
    gate: &GateRun,
    audit: &GateAudit,
    cost_per_decision_usd: f64,
) -> Result<BacktestRun> {
    let mut run = evaluate(p, Vec::new())?;
    gate_arms(
        gate,
        &p.set.candidates,
        &p.spec,
        &p.md,
        &p.params,
        p.caps.as_ref(),
        cost_per_decision_usd,
    )
    .add_to_run(&mut run);
    run.extra_files.insert(
        DECISIONS_FILE.to_string(),
        audit.lines_by_seq(&gate.strategy)?,
    );
    Ok(run)
}

/// One decision as a line — seq, class, action, confidence, p(take), a
/// rejection's reason; a failed call's error in full (CLI, live test).
pub(crate) fn describe(d: &GateDecision) -> String {
    match (&d.verdict, &d.error) {
        (Some(v), _) => {
            let reason = match &v.outcome {
                StepOutcome::Rejected { reason, .. } => format!(" ({reason})"),
                _ => String::new(),
            };
            format!(
                "seq {} {} — action `{}` confidence {:.2} p(take) {:.2}{reason}",
                d.seq,
                d.class.as_str(),
                v.action,
                v.confidence,
                p_take(v)
            )
        }
        (None, Some(e)) => format!("seq {} error — {e}", d.seq),
        (None, None) => format!("seq {} {}", d.seq, d.class.as_str()),
    }
}

#[cfg(test)]
#[path = "gate_tests.rs"]
mod tests;
