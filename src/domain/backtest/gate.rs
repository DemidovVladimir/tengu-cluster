//! The Jev gate arm's pure half (`docs/xlab-2026-10-01.md` § 7; PRD § 38
//! baseline C, § 37 calibration): what Jev sees of a candidate, how its
//! verdict classifies, p(take), and the gate summary for `report.md` and the
//! `backtest/1` row. The loops, the cache and the workers:
//! `application/backtest/gate.rs`.
//!
//! | Piece | Rule |
//! |---|---|
//! | [`gate_event`] | `{strategy, instrument, side, signal_bps, decided_at, features}` — `decided_at` RFC 3339 UTC; numbers rounded to 0.01, whole ones as integers (a short, stable cache key); nothing known only after the decision: no exit, period, anchor, label (free text an operator may write with hindsight), data time or outcome |
//! | [`GateClass::of`] | `Stopped` `take` ⇒ take · `Stopped` `ask_architect` ⇒ ask_architect · any other `Stopped` ⇒ skip · `Escalated` (below `act_at`) ⇒ unsure · `Rejected` ⇒ rejected · a failed call ⇒ error ([`GateDecision::failed`]); a tool outcome (never from a terminal-only loop) ⇒ rejected. Only take trades |
//! | [`p_take`] | `probabilities["take"]`, else the confidence when the action is `take`, else 0 |
//! | [`GateSummary`] | counts per class; take rate = take ÷ answered (decided − errors); the run's cache hits / misses / errors; est. cost = misses × the price per decision ([`COST_PER_DECISION_USD`]; the caller passes 0 offline); calibration — [`CALIBRATION_BINS`] bins of p(take) against the candidate's research trade winning (net bps > 0), Brier — over answered verdicts but rejected ones, joined by `seq`; jev − rules = `paired_diff_ci` of the taken candidates' trades against every decided candidate's (research; capped too when given) — none when an arm traded in fewer than 2 periods or misses over 1 % of the resamples (`stats.rs`) |
//! | Renders | [`GateSummary::render_markdown`] — the gate section of `report.md` (counts, cache, cost, differences, Brier; the bins are the report's `## Calibration`); [`GateSummary::render_compact`] — two CLI lines; [`GateSummary::add_features`] — `jev_*` keys of the `backtest/1` row, into free slots only (≤ 32 keys). `BacktestReport` carries the summary (`gate`) and calls all three |

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::domain::backtest::engine::{Candidate, Trade};
use crate::domain::backtest::stats::{calibration, paired_diff_ci, Calibration, DiffCi};
use crate::domain::decision::{StepOutcome, Verdict};
use crate::domain::marketdata::fmt_time;
use crate::domain::observation::{Features, MAX_FEATURES};

/// The terminal action that trades.
pub const TAKE: &str = "take";
/// The terminal action that hands a candidate to the Architect (counted).
pub const ASK_ARCHITECT: &str = "ask_architect";
/// Arm names a gated run adds to the report: every decided candidate (PRD
/// baseline A on the same decisions) and the ones Jev took (baseline C).
pub const RULES_ARM: &str = "rules";
pub const JEV_ARM: &str = "jev";
pub const RULES_CAPPED_ARM: &str = "rules_capped";
pub const JEV_CAPPED_ARM: &str = "jev_capped";
/// Spend estimate per live decision (USD), `docs/xlab-2026-10-01.md` § 7:
/// measured 2026-10-01, Σ `usage.cost` $0.000321 over 10 live decisions.
pub const COST_PER_DECISION_USD: f64 = 0.00004;
/// Reliability bins of p(take) over [0, 1].
pub const CALIBRATION_BINS: usize = 5;

/// A number as Jev reads it: rounded to 0.01, a whole value as an integer;
/// not finite ⇒ null.
fn num(x: f64) -> Value {
    let r = (x * 100.0).round() / 100.0;
    if r.is_finite() && r == r.trunc() && r.abs() < 9.0e15 {
        json!(r as i64)
    } else {
        json!(r)
    }
}

/// What the gate loop decides on (module table): the candidate as of its
/// decision, under `strategy`'s name.
pub fn gate_event(strategy: &str, c: &Candidate) -> Value {
    let features: Map<String, Value> = c
        .features
        .iter()
        .map(|(k, v)| (k.clone(), num(*v)))
        .collect();
    json!({
        "strategy": strategy,
        "instrument": c.instrument,
        "side": c.side.as_str(),
        "signal_bps": num(c.signal_bps),
        "decided_at": fmt_time(c.decided_at_ms),
        "features": features,
    })
}

/// What one gate decision means for the trade (module table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateClass {
    Take,
    Skip,
    AskArchitect,
    Unsure,
    Rejected,
    Error,
}

impl GateClass {
    #[cfg(test)]
    pub const ALL: [GateClass; 6] = [
        GateClass::Take,
        GateClass::Skip,
        GateClass::AskArchitect,
        GateClass::Unsure,
        GateClass::Rejected,
        GateClass::Error,
    ];

    /// The class of an answered verdict (module table).
    pub(crate) fn of(v: &Verdict) -> GateClass {
        match &v.outcome {
            StepOutcome::Stopped { action } => match action.as_str() {
                TAKE => GateClass::Take,
                ASK_ARCHITECT => GateClass::AskArchitect,
                _ => GateClass::Skip,
            },
            StepOutcome::Escalated { .. } => GateClass::Unsure,
            StepOutcome::Error { .. } => GateClass::Error,
            StepOutcome::Rejected { .. }
            | StepOutcome::Executed { .. }
            | StepOutcome::Refused { .. }
            | StepOutcome::DryRun { .. } => GateClass::Rejected,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            GateClass::Take => "take",
            GateClass::Skip => "skip",
            GateClass::AskArchitect => "ask_architect",
            GateClass::Unsure => "unsure",
            GateClass::Rejected => "rejected",
            GateClass::Error => "error",
        }
    }

    /// Only `take` trades.
    pub fn trades(self) -> bool {
        self == GateClass::Take
    }
}

/// p(take) of a verdict (module table).
pub(crate) fn p_take(v: &Verdict) -> f64 {
    match v.probabilities.get(TAKE) {
        Some(p) => *p,
        None if v.action == TAKE => v.confidence,
        None => 0.0,
    }
}

/// One candidate through the gate: its class and the verdict, or the failed
/// call's error in full.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct GateDecision {
    /// The candidate's `seq`.
    pub seq: usize,
    pub class: GateClass,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<Verdict>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl GateDecision {
    pub(crate) fn decided(seq: usize, verdict: Verdict) -> Self {
        Self {
            seq,
            class: GateClass::of(&verdict),
            verdict: Some(verdict),
            error: None,
        }
    }

    /// A call that failed (offline cache miss, Jev error): class `error`.
    pub(crate) fn failed(seq: usize, error: impl Into<String>) -> Self {
        Self {
            seq,
            class: GateClass::Error,
            verdict: None,
            error: Some(error.into()),
        }
    }

    /// p(take) of an answered verdict; `None` for a failed call.
    pub(crate) fn p_take(&self) -> Option<f64> {
        self.verdict.as_ref().map(p_take)
    }
}

/// What [`GateSummary::new`] reads.
pub(crate) struct GateInputs<'a> {
    pub loop_name: &'a str,
    pub model: &'a str,
    /// One per decided candidate.
    pub decisions: &'a [GateDecision],
    /// Candidates past `max_decisions`, never asked.
    pub cut: usize,
    /// The run's `CacheStats` (0 for an engine without a cache).
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub cache_errors: u64,
    /// Price of one live call; 0 offline.
    pub cost_per_decision_usd: f64,
    /// Research trades of every decided candidate (the rules arm) and of the
    /// taken ones (the jev arm); the calibration joins `rules` by `seq`.
    pub rules: &'a [Trade],
    pub jev: &'a [Trade],
    /// The capped arms, when the sandbox has caps.
    pub capped: Option<(&'a [Trade], &'a [Trade])>,
    /// `[backtest] bootstrap` / `seed`.
    pub bootstrap: u32,
    pub seed: u64,
}

/// The gate arm of one run (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct GateSummary {
    #[serde(rename = "loop")]
    pub loop_name: String,
    pub model: String,
    pub decided: usize,
    pub cut: usize,
    pub take: usize,
    pub skip: usize,
    pub ask_architect: usize,
    pub unsure: usize,
    pub rejected: usize,
    pub errors: usize,
    /// take ÷ answered (decided − errors).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub take_rate: Option<f64>,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub cache_errors: u64,
    pub cost_per_decision_usd: f64,
    pub est_cost_usd: f64,
    /// Research trades of the rules / jev arms and their means.
    pub rules_n: usize,
    pub jev_n: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules_mean_net_bps: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jev_mean_net_bps: Option<f64>,
    pub calibration: Calibration,
    /// jev − rules, research arms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff_ci: Option<DiffCi>,
    /// The capped arms ran (the sandbox has caps).
    pub capped: bool,
    /// jev − rules, capped arms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff_ci_capped: Option<DiffCi>,
}

fn mean_net(trades: &[Trade]) -> Option<f64> {
    (!trades.is_empty())
        .then(|| trades.iter().map(|t| t.net_bps).sum::<f64>() / trades.len() as f64)
}

fn opt(x: Option<f64>, decimals: usize) -> String {
    x.map_or_else(|| "—".to_string(), |v| format!("{v:.decimals$}"))
}

fn signed(x: Option<f64>, decimals: usize) -> String {
    x.map_or_else(|| "—".to_string(), |v| format!("{v:+.decimals$}"))
}

fn diff_text(d: Option<&DiffCi>) -> String {
    d.map_or_else(
        || "— (an arm with trades in fewer than 2 periods, or missing from over 1 % of the resamples: no CI)".to_string(),
        |d| {
            format!(
                "{:+.2} bps, 95 % CI [{:+.1}, {:+.1}], {} periods",
                d.diff_bps, d.lo_bps, d.hi_bps, d.n_periods
            )
        },
    )
}

impl GateSummary {
    /// The module table's figures of `i`.
    pub(crate) fn new(i: &GateInputs<'_>) -> GateSummary {
        let mut counts: BTreeMap<GateClass, usize> = BTreeMap::new();
        for d in i.decisions {
            *counts.entry(d.class).or_insert(0) += 1;
        }
        let n = |c: GateClass| counts.get(&c).copied().unwrap_or(0);
        let decided = i.decisions.len();
        let answered = decided - n(GateClass::Error);
        let won: BTreeMap<usize, bool> = i.rules.iter().map(|t| (t.seq, t.net_bps > 0.0)).collect();
        let points: Vec<(f64, bool)> = i
            .decisions
            .iter()
            .filter(|d| d.class != GateClass::Rejected)
            .filter_map(|d| Some((d.p_take()?, *won.get(&d.seq)?)))
            .collect();
        GateSummary {
            loop_name: i.loop_name.to_string(),
            model: i.model.to_string(),
            decided,
            cut: i.cut,
            take: n(GateClass::Take),
            skip: n(GateClass::Skip),
            ask_architect: n(GateClass::AskArchitect),
            unsure: n(GateClass::Unsure),
            rejected: n(GateClass::Rejected),
            errors: n(GateClass::Error),
            take_rate: (answered > 0).then(|| n(GateClass::Take) as f64 / answered as f64),
            cache_hits: i.cache_hits,
            cache_misses: i.cache_misses,
            cache_errors: i.cache_errors,
            cost_per_decision_usd: i.cost_per_decision_usd,
            est_cost_usd: i.cache_misses as f64 * i.cost_per_decision_usd,
            rules_n: i.rules.len(),
            jev_n: i.jev.len(),
            rules_mean_net_bps: mean_net(i.rules),
            jev_mean_net_bps: mean_net(i.jev),
            calibration: calibration(&points, CALIBRATION_BINS),
            diff_ci: paired_diff_ci(i.jev, i.rules, i.bootstrap, i.seed),
            capped: i.capped.is_some(),
            diff_ci_capped: i
                .capped
                .and_then(|(rules, jev)| paired_diff_ci(jev, rules, i.bootstrap, i.seed)),
        }
    }

    /// Answered decisions (not failed calls).
    pub(crate) fn answered(&self) -> usize {
        self.decided - self.errors
    }

    fn classes_line(&self) -> String {
        format!(
            "take {} · skip {} · ask_architect {} · unsure {} · rejected {} · error {}",
            self.take, self.skip, self.ask_architect, self.unsure, self.rejected, self.errors
        )
    }

    /// The gate section of `report.md` (module table); starts with a blank
    /// line, so it appends to `BacktestReport::render_markdown`.
    pub(crate) fn render_markdown(&self) -> String {
        let mut m = String::new();
        let _ = writeln!(m, "\n## Jev gate `{}`\n", self.loop_name);
        let _ = writeln!(m, "| Gate | |\n|---|---|");
        let cut = if self.cut == 0 {
            format!("{} · 0", self.decided)
        } else {
            format!(
                "{} of {} · {} past `--max-decisions` (the latest; never asked)",
                self.decided,
                self.decided + self.cut,
                self.cut
            )
        };
        let mut rows = vec![
            ("Model", format!("`{}`", self.model)),
            ("Decided · cut", cut),
            ("Classes", self.classes_line()),
            (
                "Take rate",
                format!("{} of {} answered", opt(self.take_rate, 2), self.answered()),
            ),
            (
                "Cache",
                format!(
                    "{} hits · {} misses · {} errors",
                    self.cache_hits, self.cache_misses, self.cache_errors
                ),
            ),
            (
                "Est. cost",
                format!(
                    "${:.4} ({} misses × ${})",
                    self.est_cost_usd, self.cache_misses, self.cost_per_decision_usd
                ),
            ),
            (
                "Arms (research)",
                format!(
                    "jev n {} mean {} bps · rules n {} mean {} bps",
                    self.jev_n,
                    signed(self.jev_mean_net_bps, 2),
                    self.rules_n,
                    signed(self.rules_mean_net_bps, 2)
                ),
            ),
            ("jev − rules (research)", diff_text(self.diff_ci.as_ref())),
        ];
        if self.capped {
            rows.push((
                "jev − rules (capped)",
                diff_text(self.diff_ci_capped.as_ref()),
            ));
        }
        rows.push((
            "Calibration",
            format!(
                "n {} · Brier {} — p(take) against the research trade winning; bins: `## Calibration`",
                self.calibration.n,
                opt(self.calibration.brier, 4)
            ),
        ));
        for (k, v) in rows {
            let _ = writeln!(m, "| {k} | {v} |");
        }
        m
    }

    /// Two CLI lines: decisions and classes; cache, cost, difference, Brier.
    pub(crate) fn render_compact(&self) -> String {
        let cut = if self.cut > 0 {
            format!(" (cut {} past --max-decisions)", self.cut)
        } else {
            String::new()
        };
        let diff = self.diff_ci.as_ref().map_or_else(
            || "jev − rules —".to_string(),
            |d| {
                format!(
                    "jev − rules {:+.2} bps ci95=[{:+.1},{:+.1}]",
                    d.diff_bps, d.lo_bps, d.hi_bps
                )
            },
        );
        format!(
            "jev gate {} ({}): decided {}{cut} · {} · take rate {}\n\
             cache {} hits · {} misses · est ${:.4} · {diff} · Brier {}",
            self.loop_name,
            self.model,
            self.decided,
            self.classes_line(),
            opt(self.take_rate, 2),
            self.cache_hits,
            self.cache_misses,
            self.est_cost_usd,
            opt(self.calibration.brier, 3)
        )
    }

    /// `jev_*` features of the `backtest/1` row, in this order, each only
    /// while the row has a free slot (≤ `MAX_FEATURES` keys; an existing key
    /// is replaced): take rate, Brier, decided, cut, errors, unsure, est.
    /// cost. The jev − rules difference is the row's `diff_bps` once it is
    /// the report's first comparison (`GateArms::add_to_report`).
    pub(crate) fn add_features(&self, f: &mut Features) {
        let finite = |x: Option<f64>| x.filter(|v| v.is_finite()).map(|v| json!(v));
        let count = |n: usize| Some(json!(n));
        for (key, value) in [
            ("jev_take_rate", finite(self.take_rate)),
            ("jev_brier", finite(self.calibration.brier)),
            ("jev_decided", count(self.decided)),
            ("jev_cut", count(self.cut)),
            ("jev_errors", count(self.errors)),
            ("jev_unsure", count(self.unsure)),
            ("jev_est_cost_usd", finite(Some(self.est_cost_usd))),
        ] {
            let Some(value) = value else { continue };
            if f.contains_key(key) || f.len() < MAX_FEATURES {
                f.insert(key.to_string(), value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::*;
    use crate::domain::backtest::engine::{ExitPlan, Leg};
    use crate::domain::backtest::features::FEATURE_KEYS;
    use crate::domain::backtest::testkit::{trade, utc, H};
    use crate::domain::book::Side;
    use crate::domain::observation::{assert_features_ok, Features};

    const TSLA: &str = "hyperliquid:xyz:TSLA";

    fn verdict(outcome: StepOutcome, action: &str, confidence: f64, p: &[(&str, f64)]) -> Verdict {
        Verdict {
            action: action.into(),
            confidence,
            probabilities: p.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            below_act_at: matches!(outcome, StepOutcome::Escalated { .. }),
            outcome,
        }
    }

    fn stopped(action: &str) -> StepOutcome {
        StepOutcome::Stopped {
            action: action.into(),
        }
    }

    /// Rule W at Sun 22:00 UTC, exit Mon 13:00, with a hindsight label.
    fn candidate() -> Candidate {
        let t = utc("2026-09-27 22:00");
        Candidate {
            seq: 3,
            instrument: TSLA.into(),
            side: Side::Sell,
            legs: vec![Leg {
                instrument: TSLA.into(),
                side: Side::Sell,
                entry_px: 431.27,
            }],
            signal_bps: 182.536_189_7,
            decided_at_ms: t,
            data_asof_ms: t - 1,
            period: "2026-09-25".into(),
            anchor_px: Some(423.47),
            exit: ExitPlan::At {
                exit_ms: utc("2026-09-28 13:00"),
            },
            features: BTreeMap::from([
                ("ret_24h_bps".into(), -150.004),
                ("vol_24h_bps".into(), 45.678_9),
                ("hour_of_week".into(), 166.0),
                ("trades_last_bar".into(), 1_234.0),
                ("volume_ratio_24h".into(), 0.004),
            ]),
            label: Some("earnings beat, +10 % by Monday".into()),
        }
    }

    #[test]
    fn the_event_is_the_decision_instant_only() {
        let c = candidate();
        let e = gate_event("weekend_fade", &c);
        assert_eq!(
            e,
            json!({
                "strategy": "weekend_fade",
                "instrument": TSLA,
                "side": "sell",
                "signal_bps": 182.54,
                "decided_at": "2026-09-27T22:00:00Z",
                "features": {"hour_of_week": 166, "ret_24h_bps": -150, "trades_last_bar": 1234,
                             "vol_24h_bps": 45.68, "volume_ratio_24h": 0}
            })
        );
        let keys: Vec<&str> = e.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "decided_at",
                "features",
                "instrument",
                "side",
                "signal_bps",
                "strategy"
            ]
        );
        // Nothing from after the decision: no exit (field or time), outcome,
        // period, anchor, data time or the hindsight label.
        let text = e.to_string();
        for banned in [
            "exit",
            "2026-09-28",
            &utc("2026-09-28 13:00").to_string(),
            "net",
            "gross",
            "outcome",
            "period",
            "anchor",
            "asof",
            "label",
            "earnings",
            "legs",
            "431.27",
        ] {
            assert!(!text.contains(banned), "`{banned}` in {text}");
        }
        assert!(e["features"]
            .as_object()
            .unwrap()
            .keys()
            .all(|k| FEATURE_KEYS.contains(&k.as_str())));
        // A non-finite number is null, never a made-up value.
        assert_eq!(num(f64::NAN), Value::Null);
        assert_eq!(num(f64::INFINITY), Value::Null);
        assert_eq!(num(-0.004), json!(0));
        assert_eq!(num(-1.25), json!(-1.25));
    }

    #[test]
    fn classes_follow_the_verdict_outcome() {
        let cases = [
            (stopped("take"), GateClass::Take),
            (stopped("skip"), GateClass::Skip),
            (stopped("ask_architect"), GateClass::AskArchitect),
            (stopped("hold"), GateClass::Skip),
            (
                StepOutcome::Escalated {
                    action: "take".into(),
                    confidence: 0.4,
                },
                GateClass::Unsure,
            ),
            (
                StepOutcome::Rejected {
                    action: "hold".into(),
                    reason: "not a legal action this step".into(),
                },
                GateClass::Rejected,
            ),
            (
                StepOutcome::Error {
                    reason: "HTTP 402".into(),
                },
                GateClass::Error,
            ),
            (
                StepOutcome::DryRun {
                    action: "open".into(),
                },
                GateClass::Rejected,
            ),
        ];
        for (outcome, want) in cases {
            let v = verdict(outcome.clone(), "x", 0.9, &[]);
            assert_eq!(GateClass::of(&v), want, "{outcome:?}");
            assert_eq!(GateDecision::decided(1, v).class, want);
        }
        let failed = GateDecision::failed(2, "decision cache miss (offline)");
        assert_eq!((failed.class, failed.p_take()), (GateClass::Error, None));
        assert!(GateClass::Take.trades() && !GateClass::Unsure.trades());
        let names: Vec<&str> = GateClass::ALL.iter().map(|c| c.as_str()).collect();
        assert_eq!(
            names,
            [
                "take",
                "skip",
                "ask_architect",
                "unsure",
                "rejected",
                "error"
            ]
        );
        assert_eq!(
            serde_json::to_value(GateClass::AskArchitect).unwrap(),
            json!("ask_architect")
        );
    }

    #[test]
    fn p_take_prefers_the_probability_then_the_take_confidence() {
        let with_p = verdict(
            stopped("skip"),
            "skip",
            0.8,
            &[("take", 0.15), ("skip", 0.8)],
        );
        assert_eq!(p_take(&with_p), 0.15);
        let take_only = verdict(stopped("take"), "take", 0.7, &[]);
        assert_eq!(p_take(&take_only), 0.7);
        let other = verdict(stopped("skip"), "skip", 0.9, &[]);
        assert_eq!(p_take(&other), 0.0);
    }

    fn seq_trade(seq: usize, period: &str, net_bps: f64, exit_ms: i64) -> Trade {
        let mut t = trade(period, TSLA, net_bps, 100.0, exit_ms);
        t.seq = seq;
        t
    }

    /// Seven decisions, hand-computed: classes, take rate, the calibration
    /// join (rejected, errors and candidates without a trade left out),
    /// Brier, the paired difference, the cost estimate.
    #[test]
    fn summary_by_hand() {
        let decisions = vec![
            GateDecision::decided(
                0,
                verdict(
                    stopped("take"),
                    "take",
                    0.8,
                    &[("take", 0.9), ("skip", 0.1)],
                ),
            ),
            GateDecision::decided(
                1,
                verdict(
                    stopped("skip"),
                    "skip",
                    0.8,
                    &[("take", 0.15), ("skip", 0.85)],
                ),
            ),
            GateDecision::decided(
                2,
                verdict(
                    StepOutcome::Escalated {
                        action: "take".into(),
                        confidence: 0.5,
                    },
                    "take",
                    0.5,
                    &[("take", 0.5)],
                ),
            ),
            GateDecision::decided(
                3,
                verdict(stopped("ask_architect"), "ask_architect", 0.9, &[]),
            ),
            GateDecision::decided(
                4,
                verdict(
                    StepOutcome::Rejected {
                        action: "hold".into(),
                        reason: "not a legal action this step".into(),
                    },
                    "hold",
                    0.99,
                    &[("take", 0.99)],
                ),
            ),
            GateDecision::failed(5, "decisions endpoint HTTP 503: unavailable"),
            // Taken, but its trade was dropped (no exit bar): no point.
            GateDecision::decided(6, verdict(stopped("take"), "take", 0.7, &[])),
        ];
        let t0 = utc("2026-09-28 00:00");
        let rules = vec![
            seq_trade(0, "p0", 10.0, t0 + H),
            seq_trade(1, "p0", -5.0, t0 + 2 * H),
            seq_trade(2, "p1", 3.0, t0 + 3 * H),
            seq_trade(3, "p1", -2.0, t0 + 4 * H),
            seq_trade(4, "p1", 7.0, t0 + 5 * H),
            seq_trade(5, "p1", 7.0, t0 + 6 * H),
        ];
        let jev = vec![rules[0].clone()];
        let summary = |jev: &[Trade]| {
            GateSummary::new(&GateInputs {
                loop_name: "xl_gate",
                model: "~typesafe/jev-latest",
                decisions: &decisions,
                cut: 25,
                cache_hits: 4,
                cache_misses: 3,
                cache_errors: 1,
                cost_per_decision_usd: COST_PER_DECISION_USD,
                rules: &rules,
                jev,
                capped: None,
                bootstrap: 500,
                seed: 7,
            })
        };
        let s = summary(&jev);
        assert_eq!(
            (s.decided, s.cut, s.take, s.skip, s.ask_architect),
            (7, 25, 2, 1, 1)
        );
        assert_eq!((s.unsure, s.rejected, s.errors, s.answered()), (1, 1, 1, 6));
        assert_eq!(s.take_rate, Some(2.0 / 6.0));
        assert!((s.est_cost_usd - 3.0 * 0.00004).abs() < 1e-15);
        // Points: (0.9 won) (0.15 lost) (0.5 won) (0.0 lost).
        let c = &s.calibration;
        assert_eq!(c.n, 4);
        assert_eq!(c.bins.len(), CALIBRATION_BINS);
        assert_eq!((c.bins[0].n, c.bins[0].hit_rate), (2, Some(0.0)));
        assert!((c.bins[0].mean_p.unwrap() - 0.075).abs() < 1e-12);
        assert_eq!((c.bins[2].n, c.bins[2].hit_rate), (1, Some(1.0)));
        assert_eq!((c.bins[4].n, c.bins[4].mean_p), (1, Some(0.9)));
        let brier = (0.01 + 0.0225 + 0.25 + 0.0) / 4.0;
        assert!((c.brier.unwrap() - brier).abs() < 1e-12);
        // jev − rules: the jev arm traded in one period — no CI (it would be
        // the rules arm's spread alone); over two periods: 6.5 − mean(10,
        // −5, 3, −2, 7, 7).
        assert_eq!(s.diff_ci, None);
        let two = vec![rules[0].clone(), rules[2].clone()];
        let s2 = summary(&two);
        let d = s2.diff_ci.as_ref().unwrap();
        assert!((d.diff_bps - (6.5 - 20.0 / 6.0)).abs() < 1e-12, "{d:?}");
        assert_eq!((d.n_periods, d.resamples), (2, 500));
        assert_eq!((s.capped, &s.diff_ci_capped), (false, &None));
        assert_eq!((s.rules_n, s.jev_n), (6, 1));
        assert_eq!(s.jev_mean_net_bps, Some(10.0));
        // Round trip (report.json / gate.json).
        let back: GateSummary = serde_json::from_value(serde_json::to_value(&s).unwrap()).unwrap();
        assert_eq!(back, s);
        assert_eq!(serde_json::to_value(&s).unwrap()["loop"], json!("xl_gate"));
    }

    fn small_summary() -> GateSummary {
        let decisions = vec![
            GateDecision::decided(0, verdict(stopped("take"), "take", 0.8, &[("take", 0.8)])),
            GateDecision::failed(1, "offline"),
        ];
        let rules = vec![seq_trade(0, "p0", 4.0, utc("2026-09-28 01:00"))];
        GateSummary::new(&GateInputs {
            loop_name: "xl_gate",
            model: "typesafe/jev-1.13-20260917",
            decisions: &decisions,
            cut: 0,
            cache_hits: 1,
            cache_misses: 1,
            cache_errors: 1,
            cost_per_decision_usd: 0.0,
            rules: &rules,
            jev: &rules,
            capped: Some((&rules, &rules)),
            bootstrap: 200,
            seed: 7,
        })
    }

    #[test]
    fn renders_and_row_features() {
        let s = small_summary();
        let md = s.render_markdown();
        for want in [
            "## Jev gate `xl_gate`",
            "| Model | `typesafe/jev-1.13-20260917` |",
            "| Decided · cut | 2 · 0 |",
            "| Classes | take 1 · skip 0 · ask_architect 0 · unsure 0 · rejected 0 · error 1 |",
            "| Take rate | 1.00 of 1 answered |",
            "| Cache | 1 hits · 1 misses · 1 errors |",
            "| Est. cost | $0.0000 (1 misses × $0) |",
            "| jev − rules (research) | — (an arm with trades in fewer than 2 periods, or missing from over 1 % of the resamples: no CI) |",
            "| jev − rules (capped) | — ",
            "| Calibration | n 1 · Brier 0.0400",
        ] {
            assert!(md.contains(want), "missing `{want}` in:\n{md}");
        }
        assert!(md.starts_with("\n## "), "appends to the report");
        let compact = s.render_compact();
        assert_eq!(compact.lines().count(), 2, "{compact}");
        assert!(
            compact.starts_with(
                "jev gate xl_gate (typesafe/jev-1.13-20260917): decided 2 · take 1 · skip 0"
            ),
            "{compact}"
        );
        assert!(compact.contains("est $0.0000 · jev − rules — · Brier 0.040"));

        // Features: all seven into an empty row, scalar.
        let mut f = Features::new();
        s.add_features(&mut f);
        assert_eq!(f.len(), 7, "{f:?}");
        assert_features_ok(&f);
        assert_eq!(f["jev_take_rate"], json!(1.0));
        assert_eq!(f["jev_errors"], json!(1));
        // A crowded row: only the free slots, in priority order; an existing
        // key is replaced, never dropped.
        let mut full: Features = (0..MAX_FEATURES - 2)
            .map(|i| (format!("k{i:02}"), json!(i)))
            .collect();
        full.insert("jev_cut".into(), json!(99));
        s.add_features(&mut full);
        assert_eq!(full.len(), MAX_FEATURES);
        assert!(full.contains_key("jev_take_rate") && full["jev_cut"] == json!(0));
        assert!(!full.contains_key("jev_brier"), "{full:?}");
        // A cut is said, never silent.
        let mut cut = s.clone();
        cut.cut = 25;
        assert!(cut
            .render_markdown()
            .contains("| Decided · cut | 2 of 27 · 25 past `--max-decisions`"));
        assert!(cut
            .render_compact()
            .contains("(cut 25 past --max-decisions)"));
    }
}
