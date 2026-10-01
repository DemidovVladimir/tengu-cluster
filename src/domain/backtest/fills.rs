//! Fills, costs and exits of the backtest engine (`docs/xlab-2026-10-01.md`
//! § 6), pure. A side is filled at the close of the bar ending at its
//! instant; everything here reads only rows observable at the instant it
//! judges.
//!
//! | Piece | Rule |
//! |---|---|
//! | Cost per side ([`side_cost`]) | `taker_fee_bps` + half-spread + `slippage_bps`; half-spread `fixed` · `abdi_ranaldo` over the last `window_bars` bars closed before the instant (none ⇒ the floor) · `ctx`: the latest ctx row ≤ the instant (`impact_half_spread_bps`), else the fallback |
//! | Abdi–Ranaldo ([`abdi_ranaldo_half_bps`]) | c = ln close, η = (ln high + ln low) / 2 over adjacent bars; s² = max(0, 4 · mean[(c_t − η_t)(c_t − η_{t+1})]); half = √s² / 2 × 10⁴ bps |
//! | Funding ([`funding_over`]) | a row's settlement hour = its `t_ms` to the nearest hour (HL stamps a few ms late); Σ over hours in (entry, exit] of −side × rate_1h × 10⁴ bps; complete = every hour of the grid in (entry, exit] has a row; `funding = false` ⇒ 0, complete |
//! | `move_trigger` exit ([`walk_bars_exit`]) | each bar close after the entry up to `hold_bars`: take-profit (gross ≥ tp) or stop-loss (gross ≤ −sl), else the hold |
//! | `funding_carry` exit ([`walk_funding_exit`]) | the first settlement after the entry with \|rate\| APR < `exit_apr_pct` → the next bar close, else `hold_hours` |
//! | `pair_spread` ([`spread_points`], [`walk_spread_exit`]) | s = ln(a / b) at every close both legs have; z against the mean and sample sd of the `lookback_bars` previous points (sd ≈ 0 ⇒ no z); exit at the first point with \|z\| ≤ `exit_z`, else `max_hold_bars` |

// Consumers land with the xlab application wave (docs/xlab-2026-10-01.md); drop this then.
#![cfg_attr(not(test), allow(dead_code))]

use std::collections::{BTreeMap, VecDeque};

use serde::{Deserialize, Serialize};

use crate::domain::backtest::costs::{CostSpec, HalfSpread};
use crate::domain::backtest::features::HOURS_PER_YEAR;
use crate::domain::book::Side;
use crate::domain::marketdata::{Bar, BarSeries, CtxPoint, CtxSeries, FundingSeries};

pub(crate) const HOUR_MS: i64 = 3_600_000;
/// A spread sd at or under this (log units) is flat: no z.
const MIN_SPREAD_SD: f64 = 1e-12;

/// Why a trade closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    /// The planned instant of a window / event kind.
    Window,
    /// `hold_bars` / `hold_hours` ran out.
    Hold,
    TakeProfit,
    StopLoss,
    /// A settlement under `exit_apr_pct`.
    FundingBelowExit,
    /// \|z\| ≤ `exit_z`.
    ExitZ,
    /// `max_hold_bars` ran out.
    MaxHold,
}

impl ExitReason {
    pub fn as_str(self) -> &'static str {
        match self {
            ExitReason::Window => "window",
            ExitReason::Hold => "hold",
            ExitReason::TakeProfit => "take_profit",
            ExitReason::StopLoss => "stop_loss",
            ExitReason::FundingBelowExit => "funding_below_exit",
            ExitReason::ExitZ => "exit_z",
            ExitReason::MaxHold => "max_hold",
        }
    }
}

/// What one side of a fill costs, bps of its notional.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct SideCost {
    pub fee_bps: f64,
    pub spread_bps: f64,
    pub slippage_bps: f64,
}

impl SideCost {
    pub fn total_bps(&self) -> f64 {
        self.fee_bps + self.spread_bps + self.slippage_bps
    }
}

/// ln(a / b) × 10⁴ for prices finite and > 0.
pub(crate) fn ln_bps(a: f64, b: f64) -> Option<f64> {
    (a.is_finite() && b.is_finite() && a > 0.0 && b > 0.0).then(|| (a / b).ln() * 10_000.0)
}

/// side × ln(exit / entry) × 10⁴ — the trade's gross, bps of its notional.
pub fn gross_bps(side: Side, entry_px: f64, exit_px: f64) -> Option<f64> {
    (entry_px.is_finite() && exit_px.is_finite() && entry_px > 0.0 && exit_px > 0.0)
        .then(|| side.sign() * (exit_px / entry_px).ln() * 10_000.0)
}

/// |rate_1h| annualised, percent.
pub fn apr_pct(rate_1h: f64) -> f64 {
    rate_1h.abs() * HOURS_PER_YEAR * 100.0
}

/// The first multiple of `iv` at or after `t`.
pub(crate) fn ceil_grid(t: i64, iv: i64) -> i64 {
    let r = t.rem_euclid(iv);
    if r == 0 {
        t
    } else {
        t + (iv - r)
    }
}

/// The last multiple of `iv` at or before `t`.
pub(crate) fn floor_grid(t: i64, iv: i64) -> i64 {
    t - t.rem_euclid(iv)
}

/// The module table's cost of one side at `instant_ms`.
pub fn side_cost(
    cost: &CostSpec,
    bars: &BarSeries,
    ctx: Option<&CtxSeries>,
    instant_ms: i64,
) -> SideCost {
    SideCost {
        fee_bps: cost.taker_fee_bps,
        spread_bps: half_spread_bps(&cost.half_spread, bars, ctx, instant_ms),
        slippage_bps: cost.slippage_bps,
    }
}

/// The half-spread `model` charges at `instant_ms`, bps.
pub fn half_spread_bps(
    model: &HalfSpread,
    bars: &BarSeries,
    ctx: Option<&CtxSeries>,
    instant_ms: i64,
) -> f64 {
    match model {
        HalfSpread::Fixed { bps } => *bps,
        HalfSpread::AbdiRanaldo {
            window_bars,
            floor_bps,
        } => {
            let before = bars.observable_at(instant_ms.saturating_sub(1));
            let window = &before[before.len().saturating_sub(*window_bars as usize)..];
            abdi_ranaldo_half_bps(window, bars.interval.ms())
                .map_or(*floor_bps, |h| h.max(*floor_bps))
        }
        HalfSpread::Ctx { fallback_bps } => ctx
            .and_then(|c| c.last_at(instant_ms))
            .and_then(CtxPoint::impact_half_spread_bps)
            .filter(|h| h.is_finite())
            .unwrap_or(*fallback_bps),
    }
}

/// Abdi & Ranaldo (2017) half-spread of `bars` (oldest first), bps; `None`
/// without a pair of adjacent bars with valid prices.
pub fn abdi_ranaldo_half_bps(bars: &[Bar], interval_ms: i64) -> Option<f64> {
    let eta = |b: &Bar| (b.h.ln() + b.l.ln()) / 2.0;
    let (mut sum, mut n) = (0.0, 0usize);
    for w in bars.windows(2) {
        let (b0, b1) = (&w[0], &w[1]);
        if b1.t_open_ms - b0.t_open_ms != interval_ms {
            continue;
        }
        let c = b0.c.ln();
        let term = (c - eta(b0)) * (c - eta(b1));
        if term.is_finite() {
            sum += term;
            n += 1;
        }
    }
    (n > 0).then(|| (4.0 * sum / n as f64).max(0.0).sqrt() / 2.0 * 10_000.0)
}

fn nearest_hour(t_ms: i64) -> i64 {
    (t_ms + HOUR_MS / 2).div_euclid(HOUR_MS) * HOUR_MS
}

/// Funding over a hold of `side` from `entry_ms` to `exit_ms`: (bps of the
/// notional, every settlement hour has a row) — module table.
pub fn funding_over(
    funding: Option<&FundingSeries>,
    side: Side,
    entry_ms: i64,
    exit_ms: i64,
    enabled: bool,
) -> (f64, bool) {
    if !enabled {
        return (0.0, true);
    }
    let mut hours: BTreeMap<i64, f64> = BTreeMap::new();
    if let Some(f) = funding {
        for p in f.between(entry_ms - HOUR_MS / 2, exit_ms + HOUR_MS / 2) {
            let h = nearest_hour(p.t_ms);
            if h > entry_ms && h <= exit_ms && p.rate_1h.is_finite() {
                hours.insert(h, p.rate_1h);
            }
        }
    }
    let bps = hours
        .values()
        .map(|r| -side.sign() * r * 10_000.0)
        .sum::<f64>();
    let first = (entry_ms.div_euclid(HOUR_MS) + 1) * HOUR_MS;
    let expected = if exit_ms >= first {
        (exit_ms - first) / HOUR_MS + 1
    } else {
        0
    };
    (bps, hours.len() as i64 == expected)
}

/// `move_trigger`'s exit (module table): (instant, why).
pub fn walk_bars_exit(
    bars: &BarSeries,
    entry_ms: i64,
    entry_px: f64,
    side: Side,
    max_exit_ms: i64,
    take_profit_bps: Option<f64>,
    stop_loss_bps: Option<f64>,
) -> (i64, ExitReason) {
    let iv = bars.interval.ms();
    let start = bars.bars.partition_point(|b| b.t_open_ms + iv <= entry_ms);
    for b in &bars.bars[start..] {
        let t = b.t_open_ms + iv;
        if t > max_exit_ms {
            break;
        }
        let Some(g) = gross_bps(side, entry_px, b.c) else {
            continue;
        };
        if take_profit_bps.is_some_and(|tp| g >= tp) {
            return (t, ExitReason::TakeProfit);
        }
        if stop_loss_bps.is_some_and(|sl| g <= -sl) {
            return (t, ExitReason::StopLoss);
        }
    }
    (max_exit_ms, ExitReason::Hold)
}

/// `funding_carry`'s exit (module table): (instant, why).
pub fn walk_funding_exit(
    funding: &FundingSeries,
    entry_ms: i64,
    max_exit_ms: i64,
    exit_apr_pct: Option<f64>,
    interval_ms: i64,
) -> (i64, ExitReason) {
    if let Some(floor) = exit_apr_pct {
        for p in funding.between(entry_ms, max_exit_ms) {
            if apr_pct(p.rate_1h) < floor {
                let t = ceil_grid(p.t_ms, interval_ms);
                return if t <= max_exit_ms {
                    (t, ExitReason::FundingBelowExit)
                } else {
                    (max_exit_ms, ExitReason::Hold)
                };
            }
        }
    }
    (max_exit_ms, ExitReason::Hold)
}

/// One close both legs of a pair have.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpreadPoint {
    pub t_ms: i64,
    pub a_px: f64,
    pub b_px: f64,
    /// ln(a / b).
    pub s: f64,
    /// Mean of the `lookback` previous points, once there are that many.
    pub mean: Option<f64>,
    pub z: Option<f64>,
}

/// The module table's spread series of legs `a` / `b` (same interval).
pub fn spread_points(a: &BarSeries, b: &BarSeries, lookback: usize) -> Vec<SpreadPoint> {
    let iv = a.interval.ms();
    let mut window: VecDeque<f64> = VecDeque::with_capacity(lookback + 1);
    let mut out = Vec::new();
    for bar in &a.bars {
        let t = bar.t_open_ms + iv;
        let Some(b_px) = b.close_at(t) else {
            continue;
        };
        let Some(s) = ln_bps(bar.c, b_px).map(|x| x / 10_000.0) else {
            continue;
        };
        let (mean, z) = if lookback >= 2 && window.len() == lookback {
            let m = window.iter().sum::<f64>() / lookback as f64;
            let var = window.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (lookback - 1) as f64;
            let sd = var.sqrt();
            (Some(m), (sd > MIN_SPREAD_SD).then(|| (s - m) / sd))
        } else {
            (None, None)
        };
        out.push(SpreadPoint {
            t_ms: t,
            a_px: bar.c,
            b_px,
            s,
            mean,
            z,
        });
        window.push_back(s);
        if window.len() > lookback {
            window.pop_front();
        }
    }
    out
}

/// `pair_spread`'s exit over `points` (module table): (instant, why).
pub fn walk_spread_exit(
    points: &[SpreadPoint],
    entry_ms: i64,
    max_exit_ms: i64,
    exit_z: f64,
) -> (i64, ExitReason) {
    let start = points.partition_point(|p| p.t_ms <= entry_ms);
    for p in &points[start..] {
        if p.t_ms > max_exit_ms {
            break;
        }
        if p.z.is_some_and(|z| z.abs() <= exit_z) {
            return (p.t_ms, ExitReason::ExitZ);
        }
    }
    (max_exit_ms, ExitReason::MaxHold)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::backtest::testkit::{bar, ohlc, utc, H};
    use crate::domain::marketdata::{FundingPoint, Interval};

    const ID: &str = "hyperliquid:BTC";

    /// Closes bouncing between bid 99 and ask 101 around a fixed 99–101
    /// range: every term is d², d = ln(101 / 99) / 2, so the half-spread is
    /// exactly d — about 100 bps.
    #[test]
    fn abdi_ranaldo_on_a_known_input() {
        let t0 = utc("2026-09-28 00:00");
        let bars: Vec<Bar> = (0..6)
            .map(|i| {
                ohlc(
                    t0 + i * H,
                    100.0,
                    101.0,
                    99.0,
                    if i % 2 == 0 { 101.0 } else { 99.0 },
                )
            })
            .collect();
        let d = (101.0f64 / 99.0).ln() / 2.0;
        let half = abdi_ranaldo_half_bps(&bars, H).unwrap();
        assert!((half - d * 10_000.0).abs() < 1e-9, "{half}");
        assert!((half - 100.0033).abs() < 1e-3);
        // Closing at the high of a 5 % staircase: c − η_t > 0 > c − η_{t+1},
        // the covariance is negative and s² is floored at 0.
        let up: Vec<Bar> = (0..4)
            .map(|i| {
                let m = 100.0 * 1.05f64.powi(i);
                ohlc(t0 + i as i64 * H, m, m * 1.001, m * 0.999, m * 1.001)
            })
            .collect();
        assert_eq!(abdi_ranaldo_half_bps(&up, H), Some(0.0));
        // Not adjacent (a gap), or a single bar: no estimate.
        assert_eq!(abdi_ranaldo_half_bps(&[bars[0], bars[2]], H), None);
        assert_eq!(abdi_ranaldo_half_bps(&bars[..1], H), None);
        // Through the cost model: only bars closed before the instant, floored.
        let s = BarSeries::new(ID, Interval::H1, bars.clone());
        let model = HalfSpread::AbdiRanaldo {
            window_bars: 4,
            floor_bps: 1.0,
        };
        // Bar 5 closes at the instant (not before it): the window = bars 1..=4.
        let at = t0 + 6 * H;
        let want = abdi_ranaldo_half_bps(&bars[1..5], H).unwrap();
        assert_eq!(half_spread_bps(&model, &s, None, at), want);
        // At t0 + 2h only bar 0 closed before: no pair, the floor.
        assert_eq!(half_spread_bps(&model, &s, None, t0 + 2 * H), 1.0);
        let floor = HalfSpread::AbdiRanaldo {
            window_bars: 4,
            floor_bps: 500.0,
        };
        assert_eq!(half_spread_bps(&floor, &s, None, at), 500.0);
    }

    #[test]
    fn side_costs_by_model() {
        let t = utc("2026-09-28 12:00");
        let s = BarSeries::new(ID, Interval::H1, vec![bar(t - H, 100.0)]);
        let cost = CostSpec {
            taker_fee_bps: 4.5,
            half_spread: HalfSpread::Fixed { bps: 1.25 },
            slippage_bps: 0.5,
            funding: true,
        };
        let c = side_cost(&cost, &s, None, t);
        assert_eq!((c.fee_bps, c.spread_bps, c.slippage_bps), (4.5, 1.25, 0.5));
        assert_eq!(c.total_bps(), 6.25);
        let ctx = CtxSeries::new(
            ID,
            vec![CtxPoint {
                t_ms: t - 60_000,
                mid: Some(100.0),
                impact_bid: Some(99.98),
                impact_ask: Some(100.02),
                ..Default::default()
            }],
        );
        let model = HalfSpread::Ctx { fallback_bps: 9.0 };
        assert!((half_spread_bps(&model, &s, Some(&ctx), t) - 2.0).abs() < 1e-9);
        assert_eq!(
            half_spread_bps(&model, &s, Some(&ctx), t - 120_000),
            9.0,
            "no row yet"
        );
        assert_eq!(half_spread_bps(&model, &s, None, t), 9.0);
    }

    fn funding(points: &[(i64, f64)]) -> FundingSeries {
        FundingSeries::new(
            ID,
            points
                .iter()
                .map(|&(t_ms, rate_1h)| FundingPoint {
                    t_ms,
                    rate_1h,
                    premium: None,
                })
                .collect(),
        )
    }

    #[test]
    fn funding_is_booked_per_settlement_hour() {
        let e = utc("2026-09-28 10:00");
        // Rows stamped a few ms after the hour, as HL does.
        let f = funding(&[
            (e + 37, 0.0001),
            (e + H + 41, 0.0001),
            (e + 2 * H + 12, -0.00005),
            (e + 3 * H + 9, 0.0002),
        ]);
        // Short over (10:00, 13:00]: the 11, 12 and 13 h rows (the 10 h row
        // settled at the entry instant — not held over it).
        let (bps, complete) = funding_over(Some(&f), Side::Sell, e, e + 3 * H, true);
        assert!(
            (bps - (1.0 + -0.5 + 2.0)).abs() < 1e-12,
            "short receives: {bps}"
        );
        assert!(complete);
        let (long, _) = funding_over(Some(&f), Side::Buy, e, e + 3 * H, true);
        assert!((long + bps).abs() < 1e-12, "a long pays the same");
        // An hour without a row: counted, not guessed.
        let gap = funding(&[(e + H, 0.0001), (e + 3 * H, 0.0001)]);
        let (bps, complete) = funding_over(Some(&gap), Side::Sell, e, e + 3 * H, true);
        assert!((bps - 2.0).abs() < 1e-12);
        assert!(!complete);
        assert_eq!(
            funding_over(None, Side::Buy, e, e + 3 * H, true),
            (0.0, false)
        );
        // Within one hour: no settlement, complete; funding off: 0, complete.
        assert_eq!(
            funding_over(None, Side::Buy, e + 60_000, e + 50 * 60_000, true),
            (0.0, true)
        );
        assert_eq!(
            funding_over(Some(&f), Side::Sell, e, e + 3 * H, false),
            (0.0, true)
        );
    }

    #[test]
    fn exits_walk_only_closes_after_the_entry() {
        let t0 = utc("2026-09-28 00:00");
        let closes = [100.0, 101.0, 103.0, 99.0, 98.0, 104.0];
        let s = BarSeries::new(
            ID,
            Interval::H1,
            closes
                .iter()
                .enumerate()
                .map(|(i, c)| bar(t0 + i as i64 * H, *c))
                .collect(),
        );
        let entry = t0 + H; // the close of bar 0: 100
                            // Long, TP 250: bar 2 closes at 103 (+295.6 bps).
        assert_eq!(
            walk_bars_exit(
                &s,
                entry,
                100.0,
                Side::Buy,
                entry + 5 * H,
                Some(250.0),
                None
            ),
            (t0 + 3 * H, ExitReason::TakeProfit)
        );
        // Short, SL 250: the same bar.
        assert_eq!(
            walk_bars_exit(
                &s,
                entry,
                100.0,
                Side::Sell,
                entry + 5 * H,
                None,
                Some(250.0)
            ),
            (t0 + 3 * H, ExitReason::StopLoss)
        );
        // Neither within 2 bars: the hold.
        assert_eq!(
            walk_bars_exit(
                &s,
                entry,
                100.0,
                Side::Buy,
                entry + H,
                Some(500.0),
                Some(500.0)
            ),
            (entry + H, ExitReason::Hold)
        );
        // Funding: the first row under the exit APR, at the next bar close.
        let f = funding(&[
            (t0, 0.0001),
            (t0 + H + 30, 0.0001),
            (t0 + 2 * H + 30, 0.000001),
        ]);
        assert_eq!(
            walk_funding_exit(&f, t0, t0 + 10 * H, Some(5.0), H),
            (t0 + 3 * H, ExitReason::FundingBelowExit)
        );
        assert_eq!(
            walk_funding_exit(&f, t0, t0 + 10 * H, None, H),
            (t0 + 10 * H, ExitReason::Hold)
        );
        assert_eq!(
            walk_funding_exit(&f, t0, t0 + 2 * H, Some(5.0), H),
            (t0 + 2 * H, ExitReason::Hold),
            "the low row comes after the hold"
        );
        assert!((apr_pct(-0.0001) - 87.6).abs() < 1e-9);
        assert_eq!(
            (ceil_grid(7, 5), ceil_grid(10, 5), floor_grid(7, 5)),
            (10, 10, 5)
        );
        assert_eq!(gross_bps(Side::Sell, 100.0, 0.0), None);
        assert_eq!(ln_bps(-1.0, 1.0), None);
        for r in [
            ExitReason::Window,
            ExitReason::Hold,
            ExitReason::TakeProfit,
            ExitReason::StopLoss,
            ExitReason::FundingBelowExit,
            ExitReason::ExitZ,
            ExitReason::MaxHold,
        ] {
            assert_eq!(serde_json::to_value(r).unwrap(), r.as_str());
        }
    }

    #[test]
    fn spread_points_z_against_the_previous_lookback() {
        let t0 = utc("2026-09-28 00:00");
        let d = 0.01f64;
        let spreads = [d, -d, d, -d, 5.0 * d, d, 0.0];
        let a = BarSeries::new(
            "hyperliquid:ETH",
            Interval::H1,
            spreads
                .iter()
                .enumerate()
                .map(|(i, s)| bar(t0 + i as i64 * H, 100.0 * s.exp()))
                .collect(),
        );
        // Leg b misses the last close: that point is skipped.
        let b = BarSeries::new(
            ID,
            Interval::H1,
            (0..6).map(|i| bar(t0 + i * H, 100.0)).collect(),
        );
        let pts = spread_points(&a, &b, 4);
        assert_eq!(pts.len(), 6);
        assert!(pts[..4].iter().all(|p| p.z.is_none()));
        // Point 4: previous [d, −d, d, −d], mean 0, sd 2d/√3 ⇒ z = 5√3/2.
        assert!((pts[4].z.unwrap() - 5.0 * 3f64.sqrt() / 2.0).abs() < 1e-9);
        assert!(pts[4].mean.unwrap().abs() < 1e-15);
        // Point 5: previous [−d, d, −d, 5d], mean d ⇒ z = 0: the exit.
        assert!(pts[5].z.unwrap().abs() < 1e-9);
        assert_eq!(
            walk_spread_exit(&pts, pts[4].t_ms, pts[4].t_ms + 10 * H, 0.5),
            (pts[5].t_ms, ExitReason::ExitZ)
        );
        assert_eq!(
            walk_spread_exit(&pts, pts[4].t_ms, pts[4].t_ms + 10 * H, -1.0),
            (pts[4].t_ms + 10 * H, ExitReason::MaxHold)
        );
        // A flat spread has no z.
        let flat = spread_points(&b, &b, 2);
        assert!(flat.iter().all(|p| p.z.is_none()));
    }
}
