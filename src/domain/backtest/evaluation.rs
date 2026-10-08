//! Decision evaluation (roadmap Phase 6, gate G6; `docs/w1-review-2026-10-06.md`
//! § 16): one row per candidate of a gated backtest run — the rule's choice
//! (take every candidate), Jev's (its class, p(take), latency, cost) and
//! HOLD (take none) — joined with the candidate's outcome, then the three
//! policies scored on the same candidates. Pure; the run-dir reader is
//! `adapters/outbound/evidence/run_dir.rs`, the command `tengu evidence
//! evaluate`.
//!
//! | Piece | Rule |
//! |---|---|
//! | Row ([`EvalRow`]) | by `seq`: `candidates.jsonl` (instant, period, instrument, side, signal) · `trades-research.jsonl` (outcome = the candidate's research trade net bps — what taking it earned; none = no trade: `missing_exit` or dropped) · `decisions.jsonl` ([`JevCall::from_audit_line`]: session `backtest:<strategy>:<seq>`, class as the gate's `GateClass::of`, p(take) as `p_take`, `latency_ms`, `usage.cost`) |
//! | Common set | rows with an outcome and an answered decision (class not `error`): every policy is scored on exactly these |
//! | Per candidate | rules = outcome · jev = outcome when `take`, else 0 · hold = 0; bps per candidate, and per trade where a policy trades |
//! | Differences | per candidate: jev − rules, jev − hold, rules − hold (`stats::per_candidate_diff_ci`, paired bootstrap over periods, `[backtest] bootstrap` / `seed`); per trade: jev − rules (`stats::paired_diff_ci_of` — the gate's own `jev − rules`, selection quality) |
//! | Jev status ([`JevStatus`]) | `PROVEN` when jev − rules and jev − hold (per candidate) both have a 95 % CI above 0 · `REJECTED` when jev − hold lies below 0 (its takes lose money) · else `UNPROVEN` — also when Jev is worse than rules per candidate: it skips winners, but taking every candidate is the uncapped research arm, which a capped book cannot do; the reason states every lens |
//! | Calibration | p(take) against outcome > 0 over the common set (`stats::calibration`, the gate's bins), Brier |
//! | Latency · cost | p50 / p95 / max `latency_ms` of the common set's calls (a cached replay reads ~1 ms: the original call's time is not kept); Σ `usage.cost` of every call line, USD |

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use crate::domain::backtest::engine::{Candidate, Trade};
use crate::domain::backtest::gate::{p_take, GateClass, CALIBRATION_BINS};
use crate::domain::backtest::stats::{
    calibration, paired_diff_ci_of, per_candidate_diff_ci, Calibration, DiffCi,
};
use crate::domain::book::Side;
use crate::domain::decision::{Answer, StepOutcome, Verdict};
use crate::domain::marketdata::fmt_time;

/// What Jev answered for one candidate (module table).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct JevCall {
    pub class: GateClass,
    pub p_take: f64,
    /// `next_action.choice`; empty when the call failed.
    pub choice: String,
    pub confidence: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl JevCall {
    /// One `decisions.jsonl` line of a gated run of `strategy`: its `seq`
    /// and the call; `None` for any other line (another session, no
    /// `result`).
    pub fn from_audit_line(strategy: &str, line: &Value) -> Option<(usize, JevCall)> {
        let session = line.get("session_id")?.as_str()?;
        let seq: usize = session
            .strip_prefix("backtest:")?
            .strip_prefix(strategy)?
            .strip_prefix(':')?
            .parse()
            .ok()?;
        let outcome: StepOutcome = serde_json::from_value(line.get("result")?.clone()).ok()?;
        let answer: Answer = line
            .pointer("/answers/next_action")
            .and_then(|a| serde_json::from_value(a.clone()).ok())
            .unwrap_or_default();
        let act_at = line.get("act_at").and_then(Value::as_f64).unwrap_or(0.0);
        let confidence = answer.gate_confidence();
        let verdict = Verdict {
            action: answer.choice.clone().unwrap_or_default(),
            confidence,
            probabilities: answer.probabilities.clone(),
            below_act_at: confidence < act_at,
            outcome,
        };
        Some((
            seq,
            JevCall {
                class: GateClass::of(&verdict),
                p_take: p_take(&verdict),
                choice: verdict.action,
                confidence,
                latency_ms: line.get("latency_ms").and_then(Value::as_u64),
                cost_usd: line.pointer("/usage/cost").and_then(Value::as_f64),
                model: line
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            },
        ))
    }

    fn answered(&self) -> bool {
        self.class != GateClass::Error
    }
}

/// One candidate of the run (module table).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EvalRow {
    pub seq: usize,
    pub decided_at: String,
    pub period: String,
    pub instrument: String,
    pub side: Side,
    pub signal_bps: f64,
    /// The research trade's net bps: what taking the candidate earned.
    pub outcome_net_bps: Option<f64>,
    pub jev: Option<JevCall>,
}

impl EvalRow {
    /// In the common set: an outcome and an answered decision.
    fn common(&self) -> Option<(f64, &JevCall)> {
        let jev = self.jev.as_ref().filter(|j| j.answered())?;
        Some((self.outcome_net_bps?, jev))
    }
}

/// The rows of a run, by `seq` (module table).
pub fn join(
    candidates: &[Candidate],
    research: &[Trade],
    calls: &BTreeMap<usize, JevCall>,
) -> Vec<EvalRow> {
    let outcome: BTreeMap<usize, f64> = research.iter().map(|t| (t.seq, t.net_bps)).collect();
    let mut rows: Vec<EvalRow> = candidates
        .iter()
        .map(|c| EvalRow {
            seq: c.seq,
            decided_at: fmt_time(c.decided_at_ms),
            period: c.period.clone(),
            instrument: c.instrument.clone(),
            side: c.side,
            signal_bps: c.signal_bps,
            outcome_net_bps: outcome.get(&c.seq).copied(),
            jev: calls.get(&c.seq).cloned(),
        })
        .collect();
    rows.sort_by_key(|r| r.seq);
    rows
}

/// One policy on the common set.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PolicyScore {
    pub policy: &'static str,
    pub trades: usize,
    pub bps_per_candidate: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bps_per_trade: Option<f64>,
    pub sum_bps: f64,
}

/// Jev against the baselines (module table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum JevStatus {
    Proven,
    Unproven,
    Rejected,
}

impl JevStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            JevStatus::Proven => "PROVEN",
            JevStatus::Unproven => "UNPROVEN",
            JevStatus::Rejected => "REJECTED",
        }
    }
}

/// `latency_ms` of the common set's calls.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Latency {
    pub n: usize,
    pub p50_ms: u64,
    pub p95_ms: u64,
    pub max_ms: u64,
}

/// The evaluation of one run (module table).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EvalSummary {
    pub candidates: usize,
    pub with_outcome: usize,
    /// Rows with a decision line (answered or failed).
    pub decided: usize,
    pub errors: usize,
    /// Outcome + answered decision: the rows every policy is scored on.
    pub common: usize,
    /// Calls per class over the decided rows.
    pub classes: BTreeMap<&'static str, usize>,
    /// Jev's takes ÷ the common set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub take_rate: Option<f64>,
    pub policies: Vec<PolicyScore>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jev_minus_rules: Option<DiffCi>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jev_minus_hold: Option<DiffCi>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rules_minus_hold: Option<DiffCi>,
    /// Selection quality: Jev's taken trades against every candidate's, mean
    /// per trade (`stats::paired_diff_ci` — the gate's `jev − rules`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jev_minus_rules_per_trade: Option<DiffCi>,
    pub jev_status: JevStatus,
    pub jev_status_reason: String,
    pub calibration: Calibration,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency: Option<Latency>,
    pub cost_usd: f64,
}

/// The module table's Jev status, with every lens in the reason.
fn status(
    minus_rules: Option<&DiffCi>,
    minus_hold: Option<&DiffCi>,
    per_trade: Option<&DiffCi>,
) -> (JevStatus, String) {
    let ci = |d: Option<&DiffCi>| {
        d.map_or("no CI".to_string(), |d| {
            format!("{:+.2} [{:+.1}, {:+.1}]", d.diff_bps, d.lo_bps, d.hi_bps)
        })
    };
    let lenses = format!(
        "per candidate: jev − rules {}, jev − hold {}; per trade: jev − rules {}",
        ci(minus_rules),
        ci(minus_hold),
        ci(per_trade)
    );
    match (minus_rules, minus_hold) {
        (Some(r), Some(h)) if r.lo_bps > 0.0 && h.lo_bps > 0.0 => (
            JevStatus::Proven,
            format!("beats rules and hold per candidate — {lenses}"),
        ),
        (_, Some(h)) if h.hi_bps < 0.0 => (
            JevStatus::Rejected,
            format!("its takes lose money (below hold) — {lenses}"),
        ),
        (Some(r), _) if r.hi_bps < 0.0 => (
            JevStatus::Unproven,
            format!(
                "it skips winners: worse than taking every candidate, which a capped book \
                 cannot do — {lenses}"
            ),
        ),
        (Some(_), Some(_)) => (JevStatus::Unproven, format!("a CI spans 0 — {lenses}")),
        _ => (
            JevStatus::Unproven,
            format!("no CI: the common set covers fewer than 2 periods — {lenses}"),
        ),
    }
}

fn percentile(sorted: &[u64], q: f64) -> u64 {
    let i = ((sorted.len() - 1) as f64 * q).round() as usize;
    sorted[i.min(sorted.len() - 1)]
}

/// The module table's summary of `rows`.
pub fn summarize(rows: &[EvalRow], bootstrap: u32, seed: u64) -> EvalSummary {
    let common: Vec<(&EvalRow, f64, &JevCall)> = rows
        .iter()
        .filter_map(|r| r.common().map(|(o, j)| (r, o, j)))
        .collect();
    let n = common.len();
    let score = |policy: &'static str, earned: &dyn Fn(f64, &JevCall) -> Option<f64>| {
        let taken: Vec<f64> = common
            .iter()
            .filter_map(|(_, o, j)| earned(*o, j))
            .collect();
        // `+ 0.0`: an empty f64 sum is −0.0.
        let sum: f64 = taken.iter().sum::<f64>() + 0.0;
        PolicyScore {
            policy,
            trades: taken.len(),
            bps_per_candidate: if n == 0 { 0.0 } else { sum / n as f64 },
            bps_per_trade: (!taken.is_empty()).then(|| sum / taken.len() as f64),
            sum_bps: sum,
        }
    };
    let rules = |o: f64, _: &JevCall| Some(o);
    let jev = |o: f64, j: &JevCall| j.class.trades().then_some(o);
    let hold = |_: f64, _: &JevCall| None;
    let policies = vec![
        score("rules", &rules),
        score("jev", &jev),
        score("hold", &hold),
    ];
    let diff = |a: &dyn Fn(f64, &JevCall) -> Option<f64>,
                b: &dyn Fn(f64, &JevCall) -> Option<f64>| {
        let cells: Vec<(&str, f64, f64)> = common
            .iter()
            .map(|(r, o, j)| {
                (
                    r.period.as_str(),
                    a(*o, j).unwrap_or(0.0),
                    b(*o, j).unwrap_or(0.0),
                )
            })
            .collect();
        per_candidate_diff_ci(&cells, bootstrap, seed)
    };
    let jev_minus_rules = diff(&jev, &rules);
    let jev_minus_hold = diff(&jev, &hold);
    let rules_minus_hold = diff(&rules, &hold);
    let trades_of = |earned: &dyn Fn(f64, &JevCall) -> Option<f64>| -> Vec<(String, f64)> {
        common
            .iter()
            .filter_map(|(r, o, j)| earned(*o, j).map(|net| (r.period.clone(), net)))
            .collect()
    };
    let jev_minus_rules_per_trade =
        paired_diff_ci_of(&trades_of(&jev), &trades_of(&rules), bootstrap, seed);
    let (jev_status, jev_status_reason) = status(
        jev_minus_rules.as_ref(),
        jev_minus_hold.as_ref(),
        jev_minus_rules_per_trade.as_ref(),
    );
    let points: Vec<(f64, bool)> = common
        .iter()
        .map(|(_, o, j)| (j.p_take, *o > 0.0))
        .collect();
    let mut lat: Vec<u64> = common.iter().filter_map(|(_, _, j)| j.latency_ms).collect();
    lat.sort_unstable();
    let latency = (!lat.is_empty()).then(|| Latency {
        n: lat.len(),
        p50_ms: percentile(&lat, 0.5),
        p95_ms: percentile(&lat, 0.95),
        max_ms: *lat.last().unwrap_or(&0),
    });
    let decided: Vec<&JevCall> = rows.iter().filter_map(|r| r.jev.as_ref()).collect();
    let mut classes: BTreeMap<&'static str, usize> = BTreeMap::new();
    for j in &decided {
        *classes.entry(j.class.as_str()).or_default() += 1;
    }
    let takes = policies[1].trades;
    EvalSummary {
        candidates: rows.len(),
        with_outcome: rows.iter().filter(|r| r.outcome_net_bps.is_some()).count(),
        decided: decided.len(),
        errors: decided.iter().filter(|j| !j.answered()).count(),
        common: n,
        classes,
        take_rate: (n > 0).then(|| takes as f64 / n as f64),
        policies,
        jev_minus_rules,
        jev_minus_hold,
        rules_minus_hold,
        jev_minus_rules_per_trade,
        jev_status,
        jev_status_reason,
        calibration: calibration(&points, CALIBRATION_BINS),
        latency,
        cost_usd: decided.iter().filter_map(|j| j.cost_usd).sum(),
    }
}

impl EvalSummary {
    /// The CLI's text: counts, one line per policy, the differences, the
    /// calls, the verdict.
    pub fn render_text(&self, run: &str) -> String {
        let mut out = vec![format!(
            "evaluation {run}: {} candidates · {} with an outcome · {} decided ({} errors) · \
             common set {}",
            self.candidates, self.with_outcome, self.decided, self.errors, self.common
        )];
        out.push("policy  trades  bps/candidate  bps/trade  Σ bps".to_string());
        for p in &self.policies {
            out.push(format!(
                "{:<6}  {:>6}  {:>13.2}  {:>9}  {:>10.1}",
                p.policy,
                p.trades,
                p.bps_per_candidate,
                p.bps_per_trade
                    .map_or("—".to_string(), |v| format!("{v:.2}")),
                p.sum_bps
            ));
        }
        for (name, d) in [
            ("jev − rules", &self.jev_minus_rules),
            ("jev − hold", &self.jev_minus_hold),
            ("rules − hold", &self.rules_minus_hold),
        ] {
            out.push(match d {
                Some(d) => format!(
                    "{name}: {:+.2} bps per candidate, ci95 [{:+.1}, {:+.1}] over {} periods",
                    d.diff_bps, d.lo_bps, d.hi_bps, d.n_periods
                ),
                None => format!("{name}: no CI (fewer than 2 periods)"),
            });
        }
        out.push(match &self.jev_minus_rules_per_trade {
            Some(d) => format!(
                "jev − rules per trade: {:+.2} bps, ci95 [{:+.1}, {:+.1}] over {} periods (selection quality)",
                d.diff_bps, d.lo_bps, d.hi_bps, d.n_periods
            ),
            None => "jev − rules per trade: no CI".to_string(),
        });
        let classes: Vec<String> = self
            .classes
            .iter()
            .map(|(k, v)| format!("{k} {v}"))
            .collect();
        let latency = self.latency.as_ref().map_or("—".to_string(), |l| {
            format!(
                "p50 {} ms · p95 {} ms · max {} ms",
                l.p50_ms, l.p95_ms, l.max_ms
            )
        });
        out.push(format!(
            "calls: {} · take rate {} · Brier {} · latency {latency} · cost ${:.6}",
            classes.join(" · "),
            self.take_rate
                .map_or("—".to_string(), |r| format!("{r:.3}")),
            self.calibration
                .brier
                .map_or("—".to_string(), |b| format!("{b:.3}")),
            self.cost_usd
        ));
        out.push(format!(
            "Jev: {} — {}",
            self.jev_status.as_str(),
            self.jev_status_reason
        ));
        out.join("\n") + "\n"
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn line(seq: usize, action: &str, p_take: f64, latency: u64) -> Value {
        json!({"session_id": format!("backtest:w:{seq}"), "act_at": 0.01, "latency_ms": latency,
               "model": "typesafe/jev-1.13-20260917", "usage": {"cost": 0.00002},
               "answers": {"next_action": {"type": "choice", "choice": action,
                   "probabilities": {"take": p_take, "skip": 1.0 - p_take}}},
               "result": {"outcome": "stopped", "action": action}})
    }

    fn call(action: &str, p: f64) -> JevCall {
        JevCall::from_audit_line("w", &line(0, action, p, 5))
            .unwrap()
            .1
    }

    fn row(seq: usize, period: &str, outcome: Option<f64>, jev: Option<JevCall>) -> EvalRow {
        EvalRow {
            seq,
            decided_at: String::new(),
            period: period.into(),
            instrument: "hyperliquid:xyz:AAPL".into(),
            side: Side::Sell,
            signal_bps: 50.0,
            outcome_net_bps: outcome,
            jev,
        }
    }

    /// The audit line of a gated run classifies as the gate does; another
    /// strategy's or another session's line is not this run's.
    #[test]
    fn audit_lines_parse_into_calls_by_seq() {
        let (seq, c) = JevCall::from_audit_line("w", &line(17, "take", 0.62, 840)).unwrap();
        assert_eq!(seq, 17);
        assert_eq!(
            (c.class, c.p_take, c.latency_ms),
            (GateClass::Take, 0.62, Some(840))
        );
        assert_eq!(c.cost_usd, Some(0.00002));
        let skip = JevCall::from_audit_line("w", &line(3, "skip", 0.2, 1))
            .unwrap()
            .1;
        assert_eq!(skip.class, GateClass::Skip);
        let failed = json!({"session_id": "backtest:w:4", "result": {"outcome": "error",
                            "reason": "jev 503"}, "latency_ms": 30000});
        let (_, f) = JevCall::from_audit_line("w", &failed).unwrap();
        assert_eq!((f.class, f.p_take), (GateClass::Error, 0.0));
        assert!(JevCall::from_audit_line("w2", &line(1, "take", 0.5, 1)).is_none());
        assert!(JevCall::from_audit_line("w", &json!({"session_id": "webhook-a"})).is_none());
    }

    /// Every policy is scored on the same rows: a row without an outcome or
    /// with a failed call is left out of all three; HOLD earns 0; Jev's
    /// skipped candidates count as 0 per candidate.
    #[test]
    fn policies_are_scored_on_the_same_candidates() {
        let rows = vec![
            row(0, "p1", Some(100.0), Some(call("take", 0.7))),
            row(1, "p1", Some(-40.0), Some(call("skip", 0.2))),
            row(2, "p2", Some(20.0), Some(call("take", 0.6))),
            row(3, "p2", Some(-10.0), Some(call("skip", 0.3))),
            row(4, "p2", None, Some(call("take", 0.9))),
            row(5, "p3", Some(500.0), None),
        ];
        let s = summarize(&rows, 200, 7);
        assert_eq!(
            (s.candidates, s.with_outcome, s.decided, s.common),
            (6, 5, 5, 4)
        );
        let by: BTreeMap<&str, &PolicyScore> = s.policies.iter().map(|p| (p.policy, p)).collect();
        assert_eq!((by["rules"].trades, by["rules"].sum_bps), (4, 70.0));
        assert_eq!((by["jev"].trades, by["jev"].sum_bps), (2, 120.0));
        assert_eq!(by["jev"].bps_per_candidate, 30.0);
        assert_eq!(by["jev"].bps_per_trade, Some(60.0));
        assert_eq!((by["hold"].trades, by["hold"].bps_per_candidate), (0, 0.0));
        assert_eq!(s.take_rate, Some(0.5));
        let d = s.jev_minus_rules.as_ref().unwrap();
        assert_eq!((d.diff_bps, d.n_periods), (12.5, 2));
        assert_eq!(s.calibration.n, 4);
        assert_eq!(s.latency.as_ref().map(|l| l.p50_ms), Some(5));
        assert!(s.render_text("run-1").contains("Jev: "));
    }

    /// PROVEN needs both per-candidate CIs above 0; REJECTED only when the
    /// takes lose money (jev − hold below 0); worse than rules per
    /// candidate — skipping winners, which a capped book must do — stays
    /// UNPROVEN, as does a CI across 0 or none.
    #[test]
    fn jev_is_proven_only_when_both_cis_clear_zero() {
        let ci = |lo: f64, hi: f64| DiffCi {
            diff_bps: (lo + hi) / 2.0,
            lo_bps: lo,
            hi_bps: hi,
            n_periods: 10,
            resamples: 100,
        };
        let (a, b) = (ci(1.0, 5.0), ci(2.0, 9.0));
        let st = |r: Option<&DiffCi>, h: Option<&DiffCi>| status(r, h, None).0;
        assert_eq!(st(Some(&a), Some(&b)), JevStatus::Proven);
        assert_eq!(st(Some(&ci(-3.0, 4.0)), Some(&b)), JevStatus::Unproven);
        let (s, why) = status(Some(&ci(-9.0, -1.0)), Some(&b), Some(&ci(-2.0, 7.0)));
        assert_eq!(s, JevStatus::Unproven);
        assert!(why.starts_with("it skips winners"), "{why}");
        assert!(
            why.contains("per trade: jev − rules +2.50 [-2.0, +7.0]"),
            "{why}"
        );
        assert_eq!(st(Some(&a), Some(&ci(-9.0, -1.0))), JevStatus::Rejected);
        assert_eq!(st(None, Some(&b)), JevStatus::Unproven);
    }

    /// The per-candidate difference pairs a period's candidates: one period
    /// alone gives no CI.
    #[test]
    fn a_single_period_gives_no_ci() {
        let rows = vec![
            row(0, "p1", Some(10.0), Some(call("take", 0.7))),
            row(1, "p1", Some(-5.0), Some(call("skip", 0.2))),
        ];
        let s = summarize(&rows, 100, 7);
        assert!(s.jev_minus_rules.is_none());
        assert_eq!(s.jev_status, JevStatus::Unproven);
    }
}
