//! Backtest statistics (`docs/xlab-2026-10-01.md` § 6–7): one arm's trades
//! → [`Summary`]; two arms → [`paired_diff_ci`]; Jev's p(take) → [`calibration`].
//! Pure and deterministic: the bootstrap draws from a hand-rolled splitmix64
//! seeded by `[backtest] seed`, so a rerun prints the same numbers.
//!
//! | Figure | Rule |
//! |---|---|
//! | n · mean · median · sd · t | over the trades' net bps (sd sample, n − 1; t = mean / (sd / √n)); mean summed in trade order |
//! | hit rate | share of trades with net bps > 0 |
//! | Σ net / gross / fees / spread / slippage / funding USD | bps × filled notional / 10⁴, summed |
//! | max drawdown | realized equity in exit order (exit, then decision, then trade order): the deepest fall from a running peak, USD; + % of the start equity when the arm keeps one (capped: `initial_cash_usd`); + bps of one trade's notional when it trades a fixed one (research: every candidate at `notional_usd`, no cash book — a % would be of cash it never holds) |
//! | Sharpe | per period (Σ net USD of its trades), mean / sd × √(periods per year): weekend 52 · trading day 252 · weekday 261 · day 365; periods with trades only |
//! | 95 % CI of the mean | cluster bootstrap over periods (B = `[backtest] bootstrap`): each resample draws as many periods with replacement and takes the pooled mean net bps; the 2.5 / 97.5 % quantiles (linear); ≥ 2 periods |
//! | mean without the best 5 | mean net bps after dropping the 5 largest (n > 5) |
//! | best-2-period share | Σ net USD of the 2 best periods ÷ Σ net USD (total > 0) |
//! | per instrument | n, mean net bps, Σ net USD, hit rate per instrument key |
//! | [`paired_diff_ci`] | mean net bps of arm A − arm B, resampling the union of their periods (a resample without trades in an arm is dropped) |
//! | [`calibration`] | equal-width bins of p over [0, 1]: n, mean p, hit rate; Brier = mean (p − win)² |
//!
//! A figure that cannot be computed (too few trades / periods, sd 0) is
//! left out, never 0.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::domain::backtest::engine::Trade;

/// The statistics' bucket of a trade (module table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeriodKind {
    /// A weekend window (52 a year).
    Weekend,
    /// A trading day (252).
    TradingDay,
    /// Mon–Fri (261).
    Weekday,
    /// A calendar day — `daily_window` with `days = "all"`, or the UTC day
    /// of entry of every other kind (365).
    Day,
}

impl PeriodKind {
    pub fn per_year(self) -> f64 {
        match self {
            PeriodKind::Weekend => 52.0,
            PeriodKind::TradingDay => 252.0,
            PeriodKind::Weekday => 261.0,
            PeriodKind::Day => 365.0,
        }
    }

    #[cfg_attr(not(test), allow(dead_code))] // serde names it in report.json; text renderers may
    pub fn as_str(self) -> &'static str {
        match self {
            PeriodKind::Weekend => "weekend",
            PeriodKind::TradingDay => "trading_day",
            PeriodKind::Weekday => "weekday",
            PeriodKind::Day => "day",
        }
    }
}

/// splitmix64 (Steele, Lea, Flood 2014) — the bootstrap's generator.
#[derive(Debug, Clone)]
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n` (`n > 0`): the high word of a 128-bit product.
    pub fn below(&mut self, n: usize) -> usize {
        ((u128::from(self.next_u64()) * n as u128) >> 64) as usize
    }

    /// Uniform in `[0, 1)`, 53 bits.
    #[cfg_attr(not(test), allow(dead_code))] // readers: the test fixtures (`testkit::random_market`)
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// How an arm's statistics are computed (`[backtest]` + the arm).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StatsParams {
    pub bootstrap: u32,
    pub seed: u64,
    pub periods_per_year: f64,
    /// The capped arm's `initial_cash_usd` (drawdown %); `None` for a
    /// research arm.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_equity_usd: Option<f64>,
    /// A research arm's per-trade notional (drawdown bps); `None` for the
    /// capped arm.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trade_notional_usd: Option<f64>,
}

/// One instrument key's trades.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstrumentRow {
    pub instrument: String,
    pub n: usize,
    pub mean_net_bps: f64,
    pub net_usd: f64,
    pub hit_rate: f64,
}

/// One arm's figures (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub n: usize,
    pub n_periods: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mean_net_bps: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub median_net_bps: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sd_net_bps: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t_stat: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hit_rate: Option<f64>,
    pub net_usd: f64,
    pub gross_usd: f64,
    pub fees_usd: f64,
    pub spread_usd: f64,
    pub slippage_usd: f64,
    pub funding_usd: f64,
    /// Trades with a settlement hour of their hold missing.
    pub funding_incomplete: usize,
    pub max_drawdown_usd: f64,
    /// % of the start equity (capped arms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_drawdown_pct: Option<f64>,
    /// bps of one trade's notional (research arms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_drawdown_bps: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_equity_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sharpe: Option<f64>,
    pub periods_per_year: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ci95_lo_bps: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ci95_hi_bps: Option<f64>,
    pub bootstrap: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mean_ex_best5_bps: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub best2_periods_share: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub per_instrument: Vec<InstrumentRow>,
}

/// Mean in slice order; `None` when empty.
fn mean(xs: &[f64]) -> Option<f64> {
    (!xs.is_empty()).then(|| xs.iter().sum::<f64>() / xs.len() as f64)
}

/// Sample stdev around `m`; `None` under 2 values.
fn sample_sd(xs: &[f64], m: f64) -> Option<f64> {
    (xs.len() >= 2)
        .then(|| (xs.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (xs.len() - 1) as f64).sqrt())
}

fn median(xs: &[f64]) -> Option<f64> {
    let mut v = xs.to_vec();
    v.sort_by(f64::total_cmp);
    let n = v.len();
    match n {
        0 => None,
        _ if n % 2 == 1 => Some(v[n / 2]),
        _ => Some((v[n / 2 - 1] + v[n / 2]) / 2.0),
    }
}

/// Linear-interpolated quantile of an ascending slice.
fn quantile(sorted: &[f64], q: f64) -> Option<f64> {
    let n = sorted.len();
    if n == 0 {
        return None;
    }
    let pos = q.clamp(0.0, 1.0) * (n - 1) as f64;
    let (lo, hi) = (pos.floor() as usize, pos.ceil() as usize);
    Some(sorted[lo] + (pos - lo as f64) * (sorted[hi] - sorted[lo]))
}

/// Σ net bps and count per period, by period key.
#[derive(Debug, Clone, Copy, Default)]
struct Cell {
    sum_bps: f64,
    n: usize,
    usd: f64,
}

/// 2.5 / 97.5 % of the pooled mean over `b` cluster resamples of `cells`.
fn bootstrap_ci(cells: &[Cell], b: u32, seed: u64) -> Option<(f64, f64)> {
    if cells.len() < 2 || b == 0 {
        return None;
    }
    let mut rng = SplitMix64::new(seed);
    let k = cells.len();
    let mut means = Vec::with_capacity(b as usize);
    for _ in 0..b {
        let (mut s, mut n) = (0.0, 0usize);
        for _ in 0..k {
            let c = cells[rng.below(k)];
            s += c.sum_bps;
            n += c.n;
        }
        if n > 0 {
            means.push(s / n as f64);
        }
    }
    means.sort_by(f64::total_cmp);
    Some((quantile(&means, 0.025)?, quantile(&means, 0.975)?))
}

impl Summary {
    /// The module table's figures of `trades` (an arm's, in trade order).
    pub fn compute(trades: &[Trade], p: &StatsParams) -> Summary {
        let n = trades.len();
        let nets: Vec<f64> = trades.iter().map(|t| t.net_bps).collect();
        let mean_net = mean(&nets);
        let sd = mean_net.and_then(|m| sample_sd(&nets, m));
        let usd = |bps: fn(&Trade) -> f64| -> f64 {
            trades
                .iter()
                .map(|t| bps(t) * t.notional_usd / 10_000.0)
                .sum()
        };
        // Realized equity in exit order.
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by(|&a, &b| {
            let (x, y) = (&trades[a], &trades[b]);
            x.exit_ms
                .cmp(&y.exit_ms)
                .then(x.decided_at_ms.cmp(&y.decided_at_ms))
                .then(a.cmp(&b))
        });
        let start = p.start_equity_usd.unwrap_or(0.0);
        let (mut equity, mut peak, mut dd) = (start, start, 0.0f64);
        for i in order {
            equity += trades[i].net_usd;
            peak = peak.max(equity);
            dd = dd.max(peak - equity);
        }
        // Periods.
        let mut cells: BTreeMap<&str, Cell> = BTreeMap::new();
        for t in trades {
            let c = cells.entry(t.period.as_str()).or_default();
            c.sum_bps += t.net_bps;
            c.n += 1;
            c.usd += t.net_usd;
        }
        let cells: Vec<Cell> = cells.into_values().collect();
        let period_usd: Vec<f64> = cells.iter().map(|c| c.usd).collect();
        let sharpe = mean(&period_usd).and_then(|m| {
            sample_sd(&period_usd, m)
                .filter(|sd| *sd > 0.0)
                .map(|sd| m / sd * p.periods_per_year.sqrt())
        });
        let ci = bootstrap_ci(&cells, p.bootstrap, p.seed);
        let mean_ex_best5 = (n > 5).then(|| {
            let mut v = nets.clone();
            v.sort_by(|a, b| b.total_cmp(a));
            mean(&v[5..])
        });
        let net_usd: f64 = trades.iter().map(|t| t.net_usd).sum();
        let best2 = (net_usd > 0.0).then(|| {
            let mut v = period_usd.clone();
            v.sort_by(|a, b| b.total_cmp(a));
            v.iter().take(2).sum::<f64>() / net_usd
        });
        // Per instrument key.
        let mut per: BTreeMap<&str, (usize, f64, f64, usize)> = BTreeMap::new();
        for t in trades {
            let e = per.entry(t.instrument.as_str()).or_default();
            e.0 += 1;
            e.1 += t.net_bps;
            e.2 += t.net_usd;
            e.3 += usize::from(t.net_bps > 0.0);
        }
        Summary {
            n,
            n_periods: cells.len(),
            mean_net_bps: mean_net,
            median_net_bps: median(&nets),
            sd_net_bps: sd,
            t_stat: mean_net
                .zip(sd)
                .and_then(|(m, sd)| (sd > 0.0).then(|| m / (sd / (n as f64).sqrt()))),
            hit_rate: (n > 0).then(|| nets.iter().filter(|x| **x > 0.0).count() as f64 / n as f64),
            net_usd,
            gross_usd: usd(|t| t.gross_bps),
            fees_usd: usd(|t| t.fee_bps),
            spread_usd: usd(|t| t.spread_bps),
            slippage_usd: usd(|t| t.slippage_bps),
            funding_usd: usd(|t| t.funding_bps),
            funding_incomplete: trades.iter().filter(|t| !t.funding_complete).count(),
            max_drawdown_usd: dd,
            max_drawdown_pct: p
                .start_equity_usd
                .filter(|s| *s > 0.0)
                .map(|s| dd / s * 100.0),
            max_drawdown_bps: p
                .trade_notional_usd
                .filter(|x| *x > 0.0)
                .map(|x| dd / x * 10_000.0),
            start_equity_usd: p.start_equity_usd,
            sharpe,
            periods_per_year: p.periods_per_year,
            ci95_lo_bps: ci.map(|c| c.0),
            ci95_hi_bps: ci.map(|c| c.1),
            bootstrap: p.bootstrap,
            mean_ex_best5_bps: mean_ex_best5.flatten(),
            best2_periods_share: best2,
            per_instrument: per
                .into_iter()
                .map(|(id, (n, sum, usd, wins))| InstrumentRow {
                    instrument: id.to_string(),
                    n,
                    mean_net_bps: sum / n as f64,
                    net_usd: usd,
                    hit_rate: wins as f64 / n as f64,
                })
                .collect(),
        }
    }
}

/// Mean net bps of arm A − arm B with a paired bootstrap CI over periods.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiffCi {
    pub diff_bps: f64,
    pub lo_bps: f64,
    pub hi_bps: f64,
    /// Periods in the union of both arms.
    pub n_periods: usize,
    /// Resamples with trades in both arms (the CI's sample).
    pub resamples: usize,
}

/// The module table's paired difference; `None` when an arm has no trade,
/// the union has < 2 periods or no resample holds both arms.
pub fn paired_diff_ci(a: &[Trade], b: &[Trade], bootstrap: u32, seed: u64) -> Option<DiffCi> {
    let net = |ts: &[Trade]| ts.iter().map(|t| t.net_bps).collect::<Vec<_>>();
    let diff = mean(&net(a))? - mean(&net(b))?;
    let mut per: BTreeMap<&str, [Cell; 2]> = BTreeMap::new();
    for (arm, trades) in [a, b].into_iter().enumerate() {
        for t in trades {
            let c = &mut per.entry(t.period.as_str()).or_default()[arm];
            c.sum_bps += t.net_bps;
            c.n += 1;
        }
    }
    let cells: Vec<[Cell; 2]> = per.into_values().collect();
    if cells.len() < 2 {
        return None;
    }
    let mut rng = SplitMix64::new(seed);
    let k = cells.len();
    let mut diffs = Vec::with_capacity(bootstrap as usize);
    for _ in 0..bootstrap {
        let mut acc = [Cell::default(); 2];
        for _ in 0..k {
            let pick = cells[rng.below(k)];
            for arm in 0..2 {
                acc[arm].sum_bps += pick[arm].sum_bps;
                acc[arm].n += pick[arm].n;
            }
        }
        if acc[0].n > 0 && acc[1].n > 0 {
            diffs.push(acc[0].sum_bps / acc[0].n as f64 - acc[1].sum_bps / acc[1].n as f64);
        }
    }
    diffs.sort_by(f64::total_cmp);
    Some(DiffCi {
        diff_bps: diff,
        lo_bps: quantile(&diffs, 0.025)?,
        hi_bps: quantile(&diffs, 0.975)?,
        n_periods: k,
        resamples: diffs.len(),
    })
}

/// One reliability bin `[lo, hi)` (the last one closed).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalBin {
    pub lo: f64,
    pub hi: f64,
    pub n: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mean_p: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hit_rate: Option<f64>,
}

/// Reliability of a probability against outcomes (PRD § 37).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Calibration {
    pub n: usize,
    pub bins: Vec<CalBin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brier: Option<f64>,
}

/// `points` = (p, won) per decision; `bins` equal-width bins over [0, 1]
/// (at least 1); p is clamped to [0, 1], a non-finite p is left out.
pub fn calibration(points: &[(f64, bool)], bins: usize) -> Calibration {
    let k = bins.max(1);
    let mut acc = vec![(0usize, 0.0f64, 0usize); k];
    let (mut n, mut sq) = (0usize, 0.0f64);
    for &(p, won) in points {
        if !p.is_finite() {
            continue;
        }
        let p = p.clamp(0.0, 1.0);
        let i = ((p * k as f64) as usize).min(k - 1);
        acc[i].0 += 1;
        acc[i].1 += p;
        acc[i].2 += usize::from(won);
        sq += (p - if won { 1.0 } else { 0.0 }).powi(2);
        n += 1;
    }
    Calibration {
        n,
        bins: acc
            .into_iter()
            .enumerate()
            .map(|(i, (m, sum_p, wins))| CalBin {
                lo: i as f64 / k as f64,
                hi: (i + 1) as f64 / k as f64,
                n: m,
                mean_p: (m > 0).then(|| sum_p / m as f64),
                hit_rate: (m > 0).then(|| wins as f64 / m as f64),
            })
            .collect(),
        brier: (n > 0).then(|| sq / n as f64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::backtest::testkit::trade;

    fn params(seed: u64) -> StatsParams {
        StatsParams {
            bootstrap: 2_000,
            seed,
            periods_per_year: 52.0,
            start_equity_usd: Some(100.0),
            trade_notional_usd: None,
        }
    }

    #[test]
    fn splitmix_is_the_reference_sequence() {
        // Reference values of splitmix64 seeded with 0 (Vigna's C code).
        let mut r = SplitMix64::new(0);
        assert_eq!(r.next_u64(), 0xE220_A839_7B1D_CDAF);
        assert_eq!(r.next_u64(), 0x6E78_9E6A_A1B9_65F4);
        let mut r = SplitMix64::new(7);
        let draws: Vec<usize> = (0..1000).map(|_| r.below(10)).collect();
        assert!(draws.iter().all(|d| *d < 10));
        for v in 0..10 {
            assert!(draws.contains(&v), "{v} never drawn");
        }
        let u = SplitMix64::new(1).unit();
        assert!((0.0..1.0).contains(&u));
    }

    /// Hand-computed: 6 trades over 3 periods.
    #[test]
    fn summary_figures_by_hand() {
        let ts = vec![
            trade("w1", "a", 10.0, 100.0, 1),
            trade("w1", "b", -20.0, 100.0, 2),
            trade("w2", "a", 30.0, 100.0, 3),
            trade("w2", "a", 5.0, 100.0, 4),
            trade("w3", "b", -5.0, 100.0, 5),
            trade("w3", "c", 40.0, 100.0, 6),
        ];
        let s = Summary::compute(&ts, &params(7));
        assert_eq!((s.n, s.n_periods), (6, 3));
        let m = 60.0 / 6.0;
        assert_eq!(s.mean_net_bps, Some(m));
        assert_eq!(s.median_net_bps, Some(7.5));
        let var = [10.0f64, -20.0, 30.0, 5.0, -5.0, 40.0]
            .iter()
            .map(|x| (x - m).powi(2))
            .sum::<f64>()
            / 5.0;
        assert!((s.sd_net_bps.unwrap() - var.sqrt()).abs() < 1e-12);
        assert!((s.t_stat.unwrap() - m / (var.sqrt() / 6f64.sqrt())).abs() < 1e-12);
        assert_eq!(s.hit_rate, Some(4.0 / 6.0));
        // $100 each: 1 bps = $0.01.
        assert!((s.net_usd - 0.60).abs() < 1e-12);
        assert!((s.fees_usd - 6.0 * 0.01).abs() < 1e-12, "1 bps fee each");
        // Mean without the best 5 = the worst one.
        assert_eq!(s.mean_ex_best5_bps, Some(-20.0));
        // Periods: w1 −0.10, w2 +0.35, w3 +0.35 ⇒ best two 0.70 / 0.60.
        assert!((s.best2_periods_share.unwrap() - 0.70 / 0.60).abs() < 1e-12);
        let pu = [-0.10f64, 0.35, 0.35];
        let pm = pu.iter().sum::<f64>() / 3.0;
        let psd = (pu.iter().map(|x| (x - pm).powi(2)).sum::<f64>() / 2.0).sqrt();
        assert!((s.sharpe.unwrap() - pm / psd * 52f64.sqrt()).abs() < 1e-9);
        // Per instrument.
        let a = &s.per_instrument[0];
        assert_eq!((a.instrument.as_str(), a.n), ("a", 3));
        assert!((a.mean_net_bps - 15.0).abs() < 1e-12);
        assert_eq!(a.hit_rate, 1.0);
        assert_eq!(s.per_instrument.len(), 3);
        let (lo, hi) = (s.ci95_lo_bps.unwrap(), s.ci95_hi_bps.unwrap());
        assert!(lo < m && m < hi, "{lo} {m} {hi}");
        // Empty: counts 0, figures left out.
        let e = Summary::compute(&[], &params(7));
        assert_eq!(
            (e.n, e.mean_net_bps, e.sharpe, e.ci95_lo_bps),
            (0, None, None, None)
        );
        assert_eq!(e.max_drawdown_pct, Some(0.0));
    }

    /// Exit order, not trade order: +10, −5, −10, +3, −20 USD from 100.
    #[test]
    fn drawdown_on_realized_equity_in_exit_order() {
        // (net usd as bps of $10 000 notional: 1 bps = $1), exit ms.
        let ts = vec![
            trade("p", "x", -20.0, 10_000.0, 50),
            trade("p", "x", 10.0, 10_000.0, 10),
            trade("p", "x", -10.0, 10_000.0, 30),
            trade("p", "x", -5.0, 10_000.0, 20),
            trade("p", "x", 3.0, 10_000.0, 40),
        ];
        let s = Summary::compute(&ts, &params(7));
        // Equity 110, 105, 95, 98, 78: peak 110, trough 78.
        assert!((s.max_drawdown_usd - 32.0).abs() < 1e-9);
        assert!((s.max_drawdown_pct.unwrap() - 32.0).abs() < 1e-9);
        assert_eq!(s.ci95_lo_bps, None, "one period: no CI");
        assert_eq!(s.sharpe, None);
        let mut no_start = params(7);
        no_start.start_equity_usd = None;
        let s = Summary::compute(&ts, &no_start);
        assert_eq!(s.max_drawdown_pct, None);
        assert!((s.max_drawdown_usd - 32.0).abs() < 1e-9, "from 0");
    }

    /// The drawdown's units by arm: a capped arm's % of its cash, a research
    /// arm's bps of one trade's notional — never a % of cash it does not
    /// hold (the old research % read 161 % on rule W).
    #[test]
    fn drawdown_units_follow_the_arm() {
        // −$161 realized over three trades of $100 (1 bps = $0.01).
        let ts = vec![
            trade("p", "x", -10_000.0, 100.0, 10),
            trade("p", "x", -6_100.0, 100.0, 20),
            trade("p", "x", 500.0, 100.0, 30),
        ];
        // (start equity, trade notional) → (pct, bps)
        let cases = [
            ((Some(100.0), None), (Some(161.0), None)),
            ((None, Some(100.0)), (None, Some(16_100.0))),
            ((None, Some(25.0)), (None, Some(64_400.0))),
            ((Some(0.0), Some(0.0)), (None, None)),
            ((None, None), (None, None)),
        ];
        for ((start, notional), (pct, bps)) in cases {
            let mut p = params(7);
            p.start_equity_usd = start;
            p.trade_notional_usd = notional;
            let s = Summary::compute(&ts, &p);
            assert!(
                (s.max_drawdown_usd - 161.0).abs() < 1e-9,
                "{start:?} {notional:?}"
            );
            let close = |a: Option<f64>, b: Option<f64>| match (a, b) {
                (Some(a), Some(b)) => (a - b).abs() < 1e-6,
                (a, b) => a == b,
            };
            assert!(
                close(s.max_drawdown_pct, pct),
                "{start:?}: {:?}",
                s.max_drawdown_pct
            );
            assert!(
                close(s.max_drawdown_bps, bps),
                "{notional:?}: {:?}",
                s.max_drawdown_bps
            );
            assert_eq!(s.start_equity_usd, start);
        }
    }

    fn many_periods() -> Vec<Trade> {
        (0..40)
            .map(|i| {
                let net = ((i * 37) % 23) as f64 - 9.0;
                trade(&format!("p{:02}", i / 2), "x", net, 100.0, i)
            })
            .collect()
    }

    #[test]
    fn the_bootstrap_is_seeded() {
        let ts = many_periods();
        let a = Summary::compute(&ts, &params(7));
        let b = Summary::compute(&ts, &params(7));
        assert_eq!(
            (a.ci95_lo_bps, a.ci95_hi_bps),
            (b.ci95_lo_bps, b.ci95_hi_bps),
            "same seed, same CI"
        );
        let c = Summary::compute(&ts, &params(8));
        assert_ne!(
            (a.ci95_lo_bps, a.ci95_hi_bps),
            (c.ci95_lo_bps, c.ci95_hi_bps),
            "another seed, another CI"
        );
        let m = a.mean_net_bps.unwrap();
        assert!(a.ci95_lo_bps.unwrap() < m && m < a.ci95_hi_bps.unwrap());
    }

    #[test]
    fn paired_difference_over_the_union_of_periods() {
        let a = many_periods();
        let b: Vec<Trade> = a
            .iter()
            .filter(|t| t.net_bps > 0.0)
            .cloned()
            .map(|mut t| {
                t.net_bps -= 4.0;
                t
            })
            .collect();
        let d = paired_diff_ci(&a, &b, 2_000, 7).unwrap();
        let mean = |ts: &[Trade]| ts.iter().map(|t| t.net_bps).sum::<f64>() / ts.len() as f64;
        assert!((d.diff_bps - (mean(&a) - mean(&b))).abs() < 1e-12);
        assert!(d.lo_bps <= d.diff_bps && d.diff_bps <= d.hi_bps, "{d:?}");
        assert_eq!(d.n_periods, 20);
        assert!(d.resamples > 1_900, "{d:?}");
        assert_eq!(paired_diff_ci(&a, &b, 2_000, 7), Some(d));
        assert_eq!(paired_diff_ci(&a, &[], 2_000, 7), None);
    }

    #[test]
    fn calibration_bins_and_brier_by_hand() {
        let pts = [
            (0.1, false),
            (0.15, true),
            (0.9, true),
            (1.0, true),
            (0.5, false),
            (f64::NAN, true),
        ];
        let c = calibration(&pts, 5);
        assert_eq!(c.n, 5);
        assert_eq!(c.bins.len(), 5);
        assert_eq!(c.bins[0].n, 2);
        assert!((c.bins[0].mean_p.unwrap() - 0.125).abs() < 1e-12);
        assert_eq!(c.bins[0].hit_rate, Some(0.5));
        assert_eq!((c.bins[1].n, c.bins[1].mean_p), (0, None));
        assert_eq!(c.bins[2].n, 1);
        assert_eq!(c.bins[4].n, 2, "1.0 lands in the last bin");
        let brier = (0.01 + 0.7225 + 0.01 + 0.0 + 0.25) / 5.0;
        assert!((c.brier.unwrap() - brier).abs() < 1e-12);
        assert_eq!(calibration(&[], 5).brier, None);
        assert_eq!(PeriodKind::TradingDay.per_year(), 252.0);
        assert_eq!(PeriodKind::Weekend.as_str(), "weekend");
    }
}
