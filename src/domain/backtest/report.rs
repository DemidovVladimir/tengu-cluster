//! Backtest report (`docs/xlab-2026-10-01.md` § 6–8, PRD § 33): one run —
//! the spec, each arm's summary and split halves, refusals and skips by
//! reason, data notes — as `report.json` / `report.md` in the run dir and the
//! typed row `backtest/1:<run id>` the `backtest` tool returns. The
//! application fills the run id, the spec's JSON and its sha256.
//!
//! | Piece | Holds |
//! |---|---|
//! | [`BacktestReport`] | run id, strategy, kind, interval, spec + sha256, from / to, split, instruments, candidates, arms, skips by reason, data notes (incl. applied share splits); when the Jev gate arm ran: its comparisons, calibration and `gate` (`GateSummary`: counts, cache, cost) |
//! | [`ArmReport`] | candidates offered, summary, split halves (in-sample / holdout), refusals by rule, drops by reason |
//! | Drawdown | research arms (`research`, `rules`, `jev`): USD + bps of one trade's notional; capped arms: USD + % of `initial_cash_usd` (`stats.rs`) |
//! | `backtest/1` row | subject = the run id; line 1 ≤ 200 chars, ids whole (figures are dropped first); ≤ 32 scalar features of the primary arm (`research`, else the first) + `capped_*` + split halves + the first comparison + the gate's `jev_*` in the slots left; `partial` with an error per data gap kind (missing prices, missing exits, funding hours without a row) |
//! | [`render_markdown`](BacktestReport::render_markdown) | `report.md`: run table, summary per arm, split comparison, per-instrument top / bottom 10, refusals and skips, the Jev gate, Jev vs rules, calibration, data notes, limits (§ 11) |
//! | [`render_compact`](BacktestReport::render_compact) | CLI / tool text ≤ [`COMPACT_MAX_CHARS`]: line 1, one line per arm, the gate's two lines, the split, the best and worst instruments, skips |

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::backtest::engine::{count_skips, ArmResult, CandidateSet, RunParams, Trade};
use crate::domain::backtest::gate::GateSummary;
use crate::domain::backtest::spec::{SplitSpec, StrategySpec, Universe};
use crate::domain::backtest::stats::{paired_diff_ci, Calibration, DiffCi, InstrumentRow, Summary};
use crate::domain::marketdata::fmt_time;
use crate::domain::observation::{
    set_int, set_num, set_str, ErrorClass, Features, ObsStatus, Observed, ReadError,
    MAX_LINE1_CHARS,
};

/// The arm the row's features and line 1 describe, when present.
pub const PRIMARY_ARM: &str = "research";
/// The capped arm's name (`capped_*` features).
pub const CAPPED_ARM: &str = "capped";
/// [`BacktestReport::render_compact`]'s budget.
pub const COMPACT_MAX_CHARS: usize = 3_000;
/// Per-instrument rows shown at each end of `report.md`.
const MD_INSTRUMENTS: usize = 10;

/// What § 11 says a backtest cannot see.
const LIMITS: [&str; 4] = [
    "Fills at bar closes ± modelled costs, not an order-book fill engine: no historical xyz books exist; the thin-name spread is a model (fixed / Abdi–Ranaldo / archive ctx).",
    "Hyperliquid keeps the newest 5 000 bars per interval: 1 h reaches ~208 days; older or finer windows need recording or 4 h / 1 d bars.",
    "LLM time integrity: a model proposing specs over a period inside its training data leaks the future — judge on the holdout.",
    "Small samples (~30 weekends): read the confidence intervals, not the point estimates.",
];

/// In-sample and holdout summaries of one arm.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SplitHalves {
    pub in_sample: Summary,
    pub holdout: Summary,
}

/// One arm of a run (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArmReport {
    /// Candidates offered to the arm (the Jev arm: the ones it took).
    pub n_candidates: usize,
    pub summary: Summary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub split: Option<SplitHalves>,
    /// Refusals by `[risk]` rule (capped arms).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub refusals: BTreeMap<String, usize>,
    /// Candidates dropped while filling, by reason.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub skipped: BTreeMap<String, usize>,
}

/// Arm `a` − arm `b` (mean net bps, paired bootstrap CI).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArmComparison {
    pub a: String,
    pub b: String,
    pub diff: DiffCi,
}

/// One run (module table); `backtest/1:<run id>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BacktestReport {
    pub run_id: String,
    pub strategy: String,
    pub kind: String,
    pub interval: String,
    /// The spec as run (JSON).
    pub spec: Value,
    /// sha256 hex of the canonical spec JSON (the application's).
    pub spec_sha256: String,
    pub from_ms: i64,
    pub to_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub split: Option<SplitSpec>,
    pub n_instruments: usize,
    pub n_candidates: usize,
    pub arms: BTreeMap<String, ArmReport>,
    /// Candidate-level skips by reason.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub skipped: BTreeMap<String, usize>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub data_notes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub comparisons: Vec<ArmComparison>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calibration: Option<Calibration>,
    /// The Jev gate arm's counts, cache and cost, when it ran
    /// (`application/backtest/gate.rs`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate: Option<GateSummary>,
}

fn opt(x: Option<f64>, decimals: usize) -> String {
    x.map_or_else(|| "—".to_string(), |v| format!("{v:.decimals$}"))
}

fn signed(x: Option<f64>, decimals: usize) -> String {
    x.map_or_else(|| "—".to_string(), |v| format!("{v:+.decimals$}"))
}

fn ci(s: &Summary) -> String {
    match (s.ci95_lo_bps, s.ci95_hi_bps) {
        (Some(lo), Some(hi)) => format!("[{lo:+.1}, {hi:+.1}]"),
        _ => "—".to_string(),
    }
}

fn counts(m: &BTreeMap<String, usize>) -> String {
    m.iter()
        .map(|(k, n)| format!("{k} {n}"))
        .collect::<Vec<_>>()
        .join(", ")
}

impl BacktestReport {
    /// A report of `spec` over `params` with `set`'s candidates; arms come
    /// with [`add_arm`](Self::add_arm).
    pub fn new(
        run_id: impl Into<String>,
        spec: &StrategySpec,
        spec_value: Value,
        spec_sha256: impl Into<String>,
        params: &RunParams,
        split: Option<SplitSpec>,
        set: &CandidateSet,
    ) -> Self {
        let n_instruments = if spec.kind.names_instruments() {
            spec.named_instruments().len()
        } else if !params.universe.is_empty() {
            params.universe.len()
        } else {
            match &spec.universe {
                Some(Universe::Ids(ids)) => ids.len(),
                _ => 0,
            }
        };
        Self {
            run_id: run_id.into(),
            strategy: spec.name.clone(),
            kind: spec.kind_name().to_string(),
            interval: spec.interval.as_str().to_string(),
            spec: spec_value,
            spec_sha256: spec_sha256.into(),
            from_ms: params.from_ms,
            to_ms: params.to_ms,
            split,
            n_instruments,
            n_candidates: set.candidates.len(),
            arms: BTreeMap::new(),
            skipped: set.skip_counts(),
            data_notes: set.notes.clone(),
            comparisons: Vec::new(),
            calibration: None,
            gate: None,
        }
    }

    /// Record arm `name`: its summary, the split halves (with the arm's own
    /// statistics parameters), refusals and drops.
    pub fn add_arm(&mut self, name: &str, n_candidates: usize, result: &ArmResult) {
        let split = self.split.as_ref().map(|s| {
            let (holdout, in_sample): (Vec<Trade>, Vec<Trade>) =
                result.trades.iter().cloned().partition(|t| {
                    let legs: Vec<&str> = t.legs.iter().map(|l| l.instrument.as_str()).collect();
                    s.is_holdout(t.decided_at_ms, &legs)
                });
            SplitHalves {
                in_sample: Summary::compute(&in_sample, &result.stats),
                holdout: Summary::compute(&holdout, &result.stats),
            }
        });
        let mut refusals = BTreeMap::new();
        for r in &result.refusals {
            *refusals.entry(r.rule.clone()).or_insert(0) += 1;
        }
        self.arms.insert(
            name.to_string(),
            ArmReport {
                n_candidates,
                summary: result.summary.clone(),
                split,
                refusals,
                skipped: count_skips(&result.skipped),
            },
        );
    }

    /// Record arm `a` − arm `b` (`paired_diff_ci`); `None` when it cannot
    /// be computed.
    pub fn compare(
        &mut self,
        (a, a_trades): (&str, &[Trade]),
        (b, b_trades): (&str, &[Trade]),
        bootstrap: u32,
        seed: u64,
    ) -> Option<&DiffCi> {
        let diff = paired_diff_ci(a_trades, b_trades, bootstrap, seed)?;
        self.comparisons.push(ArmComparison {
            a: a.to_string(),
            b: b.to_string(),
            diff,
        });
        self.comparisons.last().map(|c| &c.diff)
    }

    /// The arm the row describes: [`PRIMARY_ARM`], else the first.
    pub fn primary(&self) -> Option<(&str, &ArmReport)> {
        self.arms
            .get_key_value(PRIMARY_ARM)
            .or_else(|| self.arms.iter().next())
            .map(|(k, v)| (k.as_str(), v))
    }

    fn arm_line(name: &str, a: &ArmReport) -> String {
        let s = &a.summary;
        // Research arms: bps of a trade; capped arms: % of their cash.
        let dd_unit = match (s.max_drawdown_bps, s.max_drawdown_pct) {
            (Some(bps), _) => format!(" dd_bps={bps:.0}"),
            (None, Some(pct)) => format!(" dd_pct={pct:.1}"),
            (None, None) => String::new(),
        };
        let mut line = format!(
            "{name} n={} mean_net_bps={} ci95={} hit={} net_usd={:+.2} dd_usd={:.2}{dd_unit} sharpe={}",
            s.n,
            signed(s.mean_net_bps, 2),
            ci(s),
            opt(s.hit_rate, 2),
            s.net_usd,
            s.max_drawdown_usd,
            opt(s.sharpe, 2)
        );
        if !a.refusals.is_empty() {
            let _ = write!(line, " refused: {}", counts(&a.refusals));
        }
        if !a.skipped.is_empty() {
            let _ = write!(line, " dropped: {}", counts(&a.skipped));
        }
        line
    }

    /// `report.md` (module table).
    pub fn render_markdown(&self) -> String {
        let mut m = String::new();
        let _ = writeln!(m, "# Backtest `{}`\n", self.run_id);
        let _ = writeln!(m, "| Run | |\n|---|---|");
        let split = self
            .split
            .as_ref()
            .map_or("—".to_string(), |s| format!("`{s}`"));
        for (k, v) in [
            (
                "Strategy",
                format!("`{}` ({}, {})", self.strategy, self.kind, self.interval),
            ),
            (
                "Decisions",
                format!("{} → {}", fmt_time(self.from_ms), fmt_time(self.to_ms)),
            ),
            ("Split", split),
            (
                "Instruments · candidates",
                format!("{} · {}", self.n_instruments, self.n_candidates),
            ),
            ("Spec sha256", format!("`{}`", self.spec_sha256)),
        ] {
            let _ = writeln!(m, "| {k} | {v} |");
        }
        let _ = writeln!(m, "\n## Summary\n");
        let _ = writeln!(
            m,
            "| Arm | n | periods | mean net bps | 95 % CI | median | t | hit | Σ net USD | costs USD | funding USD | max DD USD | max DD bps of a trade | max DD % of cash | Sharpe | mean ex best 5 | best-2 share |"
        );
        let _ = writeln!(
            m,
            "|---|---:|---:|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|"
        );
        for (name, a) in &self.arms {
            let s = &a.summary;
            let _ = writeln!(
                m,
                "| {name} | {} | {} | {} | {} | {} | {} | {} | {:+.2} | {:.2} | {:+.2} | {:.2} | {} | {} | {} | {} | {} |",
                s.n,
                s.n_periods,
                signed(s.mean_net_bps, 2),
                ci(s),
                signed(s.median_net_bps, 2),
                opt(s.t_stat, 2),
                opt(s.hit_rate, 2),
                s.net_usd,
                s.fees_usd + s.spread_usd + s.slippage_usd,
                s.funding_usd,
                s.max_drawdown_usd,
                opt(s.max_drawdown_bps, 0),
                opt(s.max_drawdown_pct, 1),
                opt(s.sharpe, 2),
                signed(s.mean_ex_best5_bps, 2),
                opt(s.best2_periods_share, 2)
            );
        }
        let _ = writeln!(
            m,
            "\nMax drawdown on realized equity: research arms in bps of one trade's notional \
             (they take every candidate at `notional_usd` and keep no cash book); capped arms in \
             % of `[paper] initial_cash_usd`."
        );
        if let Some(split) = &self.split {
            let _ = writeln!(m, "\n## Split `{split}`\n");
            let _ = writeln!(m, "| Arm | half | n | mean net bps | 95 % CI | hit | Σ net USD |\n|---|---|---:|---:|---|---:|---:|");
            for (name, a) in &self.arms {
                if let Some(h) = &a.split {
                    for (half, s) in [("in-sample", &h.in_sample), ("holdout", &h.holdout)] {
                        let _ = writeln!(
                            m,
                            "| {name} | {half} | {} | {} | {} | {} | {:+.2} |",
                            s.n,
                            signed(s.mean_net_bps, 2),
                            ci(s),
                            opt(s.hit_rate, 2),
                            s.net_usd
                        );
                    }
                }
            }
        }
        if let Some((name, a)) = self.primary() {
            let mut rows: Vec<&InstrumentRow> = a.summary.per_instrument.iter().collect();
            if !rows.is_empty() {
                rows.sort_by(|x, y| {
                    y.net_usd
                        .total_cmp(&x.net_usd)
                        .then_with(|| x.instrument.cmp(&y.instrument))
                });
                let _ = writeln!(m, "\n## Per instrument ({name}, by Σ net USD)\n");
                let _ = writeln!(m, "| | Instrument | n | mean net bps | Σ net USD | hit |\n|---|---|---:|---:|---:|---:|");
                let n = rows.len();
                let top: Vec<usize> = (0..n.min(MD_INSTRUMENTS)).collect();
                let bottom: Vec<usize> =
                    (n.saturating_sub(MD_INSTRUMENTS).max(top.len())..n).collect();
                for (label, idx) in [("top", top), ("bottom", bottom)] {
                    for i in idx {
                        let r = rows[i];
                        let _ = writeln!(
                            m,
                            "| {label} | `{}` | {} | {:+.2} | {:+.2} | {:.2} |",
                            r.instrument, r.n, r.mean_net_bps, r.net_usd, r.hit_rate
                        );
                    }
                }
            }
        }
        let arm_drops: Vec<String> = self
            .arms
            .iter()
            .filter(|(_, a)| !a.refusals.is_empty() || !a.skipped.is_empty())
            .map(|(name, a)| {
                format!(
                    "| {name} | {} | {} |",
                    if a.refusals.is_empty() {
                        "—".to_string()
                    } else {
                        counts(&a.refusals)
                    },
                    if a.skipped.is_empty() {
                        "—".to_string()
                    } else {
                        counts(&a.skipped)
                    }
                )
            })
            .collect();
        if !self.skipped.is_empty() || !arm_drops.is_empty() {
            let _ = writeln!(m, "\n## Refusals and skips\n");
            if !self.skipped.is_empty() {
                let _ = writeln!(m, "Candidates skipped: {}.\n", counts(&self.skipped));
            }
            if !arm_drops.is_empty() {
                let _ = writeln!(
                    m,
                    "| Arm | refused (rule) | dropped (reason) |\n|---|---|---|"
                );
                for line in arm_drops {
                    let _ = writeln!(m, "{line}");
                }
            }
        }
        if let Some(g) = &self.gate {
            m.push_str(&g.render_markdown());
        }
        if !self.comparisons.is_empty() {
            let _ = writeln!(
                m,
                "\n## Arm differences (mean net bps, paired bootstrap over periods)\n"
            );
            let _ = writeln!(
                m,
                "| A − B | diff | 95 % CI | periods |\n|---|---:|---|---:|"
            );
            for c in &self.comparisons {
                let _ = writeln!(
                    m,
                    "| {} − {} | {:+.2} | [{:+.1}, {:+.1}] | {} |",
                    c.a, c.b, c.diff.diff_bps, c.diff.lo_bps, c.diff.hi_bps, c.diff.n_periods
                );
            }
        }
        if let Some(cal) = &self.calibration {
            let _ = writeln!(
                m,
                "\n## Calibration (n {}, Brier {})\n",
                cal.n,
                opt(cal.brier, 4)
            );
            let _ = writeln!(m, "| p bin | n | mean p | hit rate |\n|---|---:|---:|---:|");
            for b in &cal.bins {
                let _ = writeln!(
                    m,
                    "| {:.1}–{:.1} | {} | {} | {} |",
                    b.lo,
                    b.hi,
                    b.n,
                    opt(b.mean_p, 3),
                    opt(b.hit_rate, 3)
                );
            }
        }
        if !self.data_notes.is_empty() {
            let _ = writeln!(m, "\n## Data notes\n");
            for n in &self.data_notes {
                let _ = writeln!(m, "- {n}");
            }
        }
        let _ = writeln!(m, "\n## Limits\n");
        for l in LIMITS {
            let _ = writeln!(m, "- {l}");
        }
        m
    }

    /// CLI / tool text (module table), at most [`COMPACT_MAX_CHARS`]: lines
    /// that would pass the budget are left out whole.
    pub fn render_compact(&self) -> String {
        let mut lines = vec![self.headline()];
        for (name, a) in &self.arms {
            lines.push(Self::arm_line(name, a));
        }
        if let Some(g) = &self.gate {
            lines.extend(g.render_compact().lines().map(str::to_string));
        }
        if let Some(split) = &self.split {
            for (name, a) in &self.arms {
                if let Some(h) = &a.split {
                    lines.push(format!(
                        "split {split} {name}: in-sample n={} mean_net_bps={} · holdout n={} mean_net_bps={} ci95={}",
                        h.in_sample.n,
                        signed(h.in_sample.mean_net_bps, 2),
                        h.holdout.n,
                        signed(h.holdout.mean_net_bps, 2),
                        ci(&h.holdout)
                    ));
                }
            }
        }
        for c in &self.comparisons {
            lines.push(format!(
                "{} − {}: {:+.2} bps ci95=[{:+.1},{:+.1}] over {} periods",
                c.a, c.b, c.diff.diff_bps, c.diff.lo_bps, c.diff.hi_bps, c.diff.n_periods
            ));
        }
        if let Some((_, a)) = self.primary() {
            let mut rows: Vec<&InstrumentRow> = a.summary.per_instrument.iter().collect();
            rows.sort_by(|x, y| {
                y.net_usd
                    .total_cmp(&x.net_usd)
                    .then_with(|| x.instrument.cmp(&y.instrument))
            });
            let show = |rs: &[&InstrumentRow]| {
                rs.iter()
                    .map(|r| format!("{} {:+.2} usd (n {})", r.instrument, r.net_usd, r.n))
                    .collect::<Vec<_>>()
                    .join(" · ")
            };
            if rows.len() > 1 {
                let k = rows.len().min(3);
                lines.push(format!("best: {}", show(&rows[..k])));
                let mut worst: Vec<&InstrumentRow> = rows[rows.len() - k..].to_vec();
                worst.reverse();
                lines.push(format!("worst: {}", show(&worst)));
            }
        }
        if !self.skipped.is_empty() {
            lines.push(format!("skipped: {}", counts(&self.skipped)));
        }
        if !self.data_notes.is_empty() {
            lines.push(format!("data notes: {} (report.md)", self.data_notes.len()));
        }
        let mut out = String::new();
        for line in lines {
            let extra = line.chars().count() + usize::from(!out.is_empty());
            if out.chars().count() + extra > COMPACT_MAX_CHARS {
                continue;
            }
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&line);
        }
        out
    }
}

impl Observed for BacktestReport {
    const SCHEMA: &'static str = "backtest/1";

    fn subject(&self) -> String {
        self.run_id.clone()
    }

    /// `backtest <run id> <strategy> <kind> <interval>` + the primary
    /// arm's figures while they fit in 200 chars (the ids never cut).
    fn headline(&self) -> String {
        let mut h = format!(
            "backtest {} {} {} {}",
            self.run_id, self.strategy, self.kind, self.interval
        );
        let Some((name, arm)) = self.primary() else {
            h.push_str(" no_arms");
            return h;
        };
        let s = &arm.summary;
        let mut parts = vec![format!("{name} n={}", s.n)];
        if let Some(m) = s.mean_net_bps {
            parts.push(format!("mean_net_bps={m:+.2}"));
        }
        if let (Some(lo), Some(hi)) = (s.ci95_lo_bps, s.ci95_hi_bps) {
            parts.push(format!("ci95=[{lo:+.1},{hi:+.1}]"));
        }
        if let Some(hit) = s.hit_rate {
            parts.push(format!("hit={hit:.2}"));
        }
        parts.push(format!("net_usd={:+.2}", s.net_usd));
        if let Some(sh) = s.sharpe {
            parts.push(format!("sharpe={sh:.2}"));
        }
        for p in parts {
            if h.chars().count() + 1 + p.chars().count() > MAX_LINE1_CHARS {
                break;
            }
            h.push(' ');
            h.push_str(&p);
        }
        h
    }

    fn features(&self) -> Features {
        let mut f = Features::new();
        set_str(&mut f, "kind", Some(&self.kind));
        set_str(&mut f, "interval", Some(&self.interval));
        set_int(&mut f, "n_candidates", Some(self.n_candidates as i64));
        set_int(
            &mut f,
            "n_skipped",
            Some(self.skipped.values().sum::<usize>() as i64),
        );
        if let Some((_, arm)) = self.primary() {
            let s = &arm.summary;
            set_int(&mut f, "n_trades", Some(s.n as i64));
            set_int(&mut f, "n_periods", Some(s.n_periods as i64));
            for (key, v) in [
                ("mean_net_bps", s.mean_net_bps),
                ("median_net_bps", s.median_net_bps),
                ("sd_net_bps", s.sd_net_bps),
                ("ci95_lo_bps", s.ci95_lo_bps),
                ("ci95_hi_bps", s.ci95_hi_bps),
                ("t_stat", s.t_stat),
                ("hit_rate", s.hit_rate),
                ("net_usd", Some(s.net_usd)),
                (
                    "costs_usd",
                    Some(s.fees_usd + s.spread_usd + s.slippage_usd),
                ),
                ("funding_usd", Some(s.funding_usd)),
                ("max_drawdown_usd", Some(s.max_drawdown_usd)),
                // Research: bps of a trade; a capped primary: % of its cash.
                ("max_drawdown_bps", s.max_drawdown_bps),
                ("max_drawdown_pct", s.max_drawdown_pct),
                ("sharpe", s.sharpe),
                ("mean_ex_best5_bps", s.mean_ex_best5_bps),
                ("best2_periods_share", s.best2_periods_share),
            ] {
                set_num(&mut f, key, v);
            }
            if let Some(h) = &arm.split {
                set_num(&mut f, "in_sample_mean_net_bps", h.in_sample.mean_net_bps);
                set_num(&mut f, "holdout_mean_net_bps", h.holdout.mean_net_bps);
                set_int(&mut f, "holdout_n", Some(h.holdout.n as i64));
            }
        }
        if let Some(c) = self.arms.get(CAPPED_ARM) {
            set_int(&mut f, "capped_n", Some(c.summary.n as i64));
            set_num(&mut f, "capped_mean_net_bps", c.summary.mean_net_bps);
            set_num(&mut f, "capped_net_usd", Some(c.summary.net_usd));
            set_num(
                &mut f,
                "capped_max_drawdown_usd",
                Some(c.summary.max_drawdown_usd),
            );
            set_num(
                &mut f,
                "capped_max_drawdown_pct",
                c.summary.max_drawdown_pct,
            );
            set_int(
                &mut f,
                "capped_refusals",
                Some(c.refusals.values().sum::<usize>() as i64),
            );
        }
        if let Some(c) = self.comparisons.first() {
            set_num(&mut f, "diff_bps", Some(c.diff.diff_bps));
        }
        // Last: the gate's jev_* fill the slots left (≤ 32 keys).
        if let Some(g) = &self.gate {
            g.add_features(&mut f);
        }
        f
    }

    fn status(&self) -> ObsStatus {
        if self.errors().is_empty() {
            ObsStatus::Ok
        } else {
            ObsStatus::Partial
        }
    }

    /// One error per data gap kind (never a rule's choice).
    fn errors(&self) -> Vec<ReadError> {
        let mut out = Vec::new();
        let gaps: BTreeMap<String, usize> = self
            .skipped
            .iter()
            .filter(|(k, _)| k.starts_with("missing_"))
            .map(|(k, n)| (k.clone(), *n))
            .collect();
        if !gaps.is_empty() {
            out.push(ReadError::new(
                "prices",
                ErrorClass::NotApplicable,
                format!(
                    "decisions without a price at their instant: {}",
                    counts(&gaps)
                ),
            ));
        }
        for (name, a) in &self.arms {
            if let Some(n) = a.skipped.get("missing_exit") {
                out.push(ReadError::new(
                    format!("{name}.exits"),
                    ErrorClass::NotApplicable,
                    format!("{n} trades dropped: no bar at their exit instant"),
                ));
            }
            if a.summary.funding_incomplete > 0 {
                out.push(ReadError::new(
                    format!("{name}.funding"),
                    ErrorClass::NotApplicable,
                    format!(
                        "{} trades held an hour without a funding row (booked as 0, not guessed)",
                        a.summary.funding_incomplete
                    ),
                ));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::domain::backtest::engine::{candidates, simulate, Arm, MarketData, RiskCaps};
    use crate::domain::backtest::stats::calibration;
    use crate::domain::backtest::testkit::{market, run_params, sparse, spec, utc, H};
    use crate::domain::marketdata::Interval;
    use crate::domain::observation::{assert_features_ok, ObsSource, Observation, MAX_FEATURES};

    const IDS: [&str; 4] = [
        "hyperliquid:xyz:AAA",
        "hyperliquid:xyz:BBB",
        "hyperliquid:xyz:CCC",
        "hyperliquid:xyz:DDD",
    ];

    /// Two weekends of four names (one without bars), research + capped,
    /// split at the second weekend.
    fn run(run_id: &str) -> (BacktestReport, MarketData) {
        let windows = [
            (
                utc("2026-09-19 00:00"),
                utc("2026-09-20 22:00"),
                utc("2026-09-21 13:00"),
            ),
            (
                utc("2026-09-26 00:00"),
                utc("2026-09-27 22:00"),
                utc("2026-09-28 13:00"),
            ),
        ];
        let mut bars = Vec::new();
        for (k, id) in IDS.iter().take(3).enumerate() {
            let mut pts = Vec::new();
            for (w, (a, e, x)) in windows.iter().enumerate() {
                let base = 100.0 + 10.0 * k as f64;
                let up = if (k + w) % 2 == 0 { 1.02 } else { 0.97 };
                pts.extend([
                    (a - H, base),
                    (e - H, base * up),
                    (x - H, base * (1.0 + (up - 1.0) / 3.0)),
                ]);
            }
            bars.push(sparse(id, Interval::H1, &pts));
        }
        let md = market(bars);
        let s = spec(
            json!({"kind": "weekend_window", "universe": IDS, "interval": "1h",
            "calendar": "us_equity", "direction": "fade",
            "costs": {"taker_fee_bps": 1, "funding": true}}),
        );
        let p = run_params(utc("2026-09-14 00:00"), utc("2026-09-29 00:00"));
        let set = candidates(&s, &md, &p).unwrap();
        let split = SplitSpec::parse("time:2026-09-24").ok();
        let mut r = BacktestReport::new(run_id, &s, s.to_value(), "ab".repeat(32), &p, split, &set);
        let research = simulate(&s, &md, &p, &set.candidates, Arm::Research);
        let caps = RiskCaps {
            initial_cash_usd: 100.0,
            max_order_notional_usd: 50.0,
            max_gross_exposure_usd: 100.0,
            max_net_exposure_usd: 100.0,
            daily_loss_limit_usd: 10.0,
            total_loss_limit_usd: 25.0,
        };
        let capped = simulate(&s, &md, &p, &set.candidates, Arm::Capped(caps));
        r.add_arm("research", set.candidates.len(), &research);
        r.add_arm("capped", set.candidates.len(), &capped);
        let jev: Vec<Trade> = research
            .trades
            .iter()
            .filter(|t| t.net_bps > 0.0)
            .cloned()
            .collect();
        r.compare(("jev", &jev), ("research", &research.trades), 500, 7);
        r.calibration = Some(calibration(&[(0.8, true), (0.3, false)], 5));
        (r, md)
    }

    #[test]
    fn the_row_keeps_ids_whole_and_features_scalar() {
        let id = "20261001T120000Z-weekend_fade-0123456789abcdef";
        let (r, _) = run(id);
        assert_eq!(r.n_candidates, 6);
        assert_eq!(r.n_instruments, 4);
        assert_eq!(r.skipped["missing_anchor"], 2, "DDD has no bars");
        let o = Observation::of("backtest", &r, 0, 0, ObsSource::Live);
        assert_eq!(o.key, format!("backtest/1:{id}"));
        assert!(
            o.headline
                .starts_with(&format!("backtest {id} t weekend_window 1h research n=6")),
            "{}",
            o.headline
        );
        assert!(o.headline.chars().count() <= MAX_LINE1_CHARS);
        assert_features_ok(&o.features);
        assert!(o.features.len() <= MAX_FEATURES);
        for key in [
            "mean_net_bps",
            "ci95_lo_bps",
            "holdout_mean_net_bps",
            "capped_n",
            "capped_refusals",
            "diff_bps",
            "n_trades",
            "max_drawdown_usd",
            "max_drawdown_bps",
            "capped_max_drawdown_pct",
        ] {
            assert!(
                o.features.contains_key(key),
                "{key}: {:?}",
                o.features.keys()
            );
        }
        // The research arm keeps no cash book: no drawdown %.
        assert!(!o.features.contains_key("max_drawdown_pct"));
        let research = &r.arms["research"].summary;
        let dd_bps = research.max_drawdown_usd / 100.0 * 1e4;
        assert!((research.max_drawdown_bps.unwrap() - dd_bps).abs() < 1e-9);
        assert_eq!(research.max_drawdown_pct, None);
        let capped = &r.arms["capped"].summary;
        assert_eq!(capped.max_drawdown_bps, None);
        assert!(
            (capped.max_drawdown_pct.unwrap() - capped.max_drawdown_usd).abs() < 1e-9,
            "of $100"
        );
        assert_eq!(o.features["holdout_n"], 3);
        // Missing anchors are a data gap: partial, said once.
        assert_eq!(o.status, ObsStatus::Partial);
        assert!(
            o.errors[0].message.contains("missing_anchor 2"),
            "{:?}",
            o.errors
        );
        let back: BacktestReport = o.typed().unwrap();
        assert_eq!(back, r);
        // A very long run id stays whole; the figures make room.
        let long = format!("run-{}", "x".repeat(170));
        let (r, _) = run(&long);
        let h = r.headline();
        assert!(h.contains(&long), "{h}");
        assert!(!h.contains("net_usd"), "{h}");
    }

    #[test]
    fn markdown_and_compact_renders() {
        let (r, _) = run("run-1");
        let md = r.render_markdown();
        for want in [
            "# Backtest `run-1`",
            "| Strategy | `t` (weekend_window, 1h) |",
            "## Summary",
            "| research | 6 |",
            "| capped |",
            "## Split `time:2026-09-24T00:00:00Z`",
            "| research | holdout | 3 |",
            "## Per instrument (research, by Σ net USD)",
            "`hyperliquid:xyz:AAA`",
            "## Refusals and skips",
            "missing_anchor 2",
            "## Arm differences",
            "| jev − research |",
            "## Calibration (n 2",
            "## Limits",
            &format!("`{}`", "ab".repeat(32)),
            "| max DD USD | max DD bps of a trade | max DD % of cash |",
            "research arms in bps of one trade's notional",
        ] {
            assert!(md.contains(want), "missing `{want}` in:\n{md}");
        }
        assert!(!md.contains("## Jev gate"), "no gate ran");
        let c = r.render_compact();
        assert!(c.chars().count() <= COMPACT_MAX_CHARS);
        assert_eq!(c.lines().next().unwrap(), r.headline());
        assert!(
            c.contains("\nresearch n=6 ") && c.contains("\ncapped n="),
            "{c}"
        );
        let line = |arm: &str| {
            c.lines()
                .find(|l| l.starts_with(&format!("{arm} n=")))
                .unwrap()
                .to_string()
        };
        assert!(line("research").contains(" dd_bps="), "{c}");
        assert!(!line("research").contains("dd_pct"), "{c}");
        assert!(line("capped").contains(" dd_pct="), "{c}");
        assert!(
            c.contains("split time:2026-09-24T00:00:00Z research: in-sample n=3"),
            "{c}"
        );
        assert!(c.contains("best: hyperliquid:xyz:"), "{c}");
        assert!(c.contains("skipped: missing_anchor 2"), "{c}");
        // A crowded report drops whole lines to stay in budget.
        let mut big = r.clone();
        big.data_notes = (0..500).map(|i| format!("note {i}")).collect();
        for i in 0..200 {
            big.arms
                .insert(format!("arm_{i:03}"), r.arms["research"].clone());
        }
        let c = big.render_compact();
        assert!(c.chars().count() <= COMPACT_MAX_CHARS);
        assert!(c
            .lines()
            .all(|l| l.starts_with("backtest") || l.contains(" n=") || l.contains(':')));
        assert!(BacktestReport::arm_line("x", &r.arms["capped"]).starts_with("x n="));
    }
}
