//! Features as-of t (`docs/xlab-2026-10-01.md` § 7): what the Jev gate and
//! the report see of an instrument at a decision. Only rows observable at t
//! are read — a bar from its close, funding and ctx rows from their `t_ms`
//! (§ 39). A missing input leaves its key out, never 0.
//!
//! | Key | Value |
//! |---|---|
//! | `ret_1h_bps` · `ret_24h_bps` · `ret_168h_bps` | ln(close of the last bar observable / close of the bar ending that many hours earlier) × 10⁴; the span must be a whole number of bars |
//! | `vol_24h_bps` | sample stdev of the log returns between adjacent bars closing in the last 24 h × 10⁴ (≥ 2 returns) |
//! | `volume_ratio_24h` | volume of the last 24 h ÷ the mean daily volume of the 7 days before (the series must reach back 8 days; missing bars count 0) |
//! | `trades_last_bar` | `n` of the last bar observable |
//! | `funding_apr_pct` | the latest funding rate observable × 24 × 365 × 100, else the latest ctx `funding_1h` |
//! | `half_spread_bps` | the cost model's half-spread at t (given), else the latest ctx impact half-spread |
//! | `hour_of_week` | UTC hour of t, Monday 00:00 = 0 |
//!
//! The windows run back from the last bar observable (its close may be
//! before t when bars are missing — the as-of view, not a guess).

use std::collections::BTreeMap;

use crate::domain::backtest::fills::ln_bps;
use crate::domain::marketdata::{Bar, BarSeries, CtxSeries, FundingSeries};

/// Every key [`features_asof`] may set.
#[cfg_attr(not(test), allow(dead_code))] // readers: the Jev gate arm (`application/backtest/gate.rs`)
pub const FEATURE_KEYS: [&str; 9] = [
    "ret_1h_bps",
    "ret_24h_bps",
    "ret_168h_bps",
    "vol_24h_bps",
    "volume_ratio_24h",
    "trades_last_bar",
    "funding_apr_pct",
    "half_spread_bps",
    "hour_of_week",
];
pub(crate) const HOURS_PER_YEAR: f64 = 8_760.0;
const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 86_400_000;
/// 1970-01-05 00:00 UTC was a Monday.
const MONDAY_MS: i64 = 4 * DAY_MS;

/// Features and the latest observation time they read.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AsOf {
    pub features: BTreeMap<String, f64>,
    /// `None` when no bar, funding or ctx row was read.
    pub asof_ms: Option<i64>,
}

/// The module table's features of `bars` (+ `funding`, `ctx`) at `t_ms`;
/// `half_spread_bps` = the cost model's estimate at t, when known.
#[cfg_attr(not(test), allow(dead_code))] // the engine uses `features_traced`; readers: tools (xlab)
pub fn features_asof(
    bars: &BarSeries,
    funding: Option<&FundingSeries>,
    ctx: Option<&CtxSeries>,
    t_ms: i64,
    half_spread_bps: Option<f64>,
) -> BTreeMap<String, f64> {
    features_traced(bars, funding, ctx, t_ms, half_spread_bps).features
}

/// [`features_asof`] plus the latest observation time read (the engine's
/// `data_asof_ms`).
pub(crate) fn features_traced(
    bars: &BarSeries,
    funding: Option<&FundingSeries>,
    ctx: Option<&CtxSeries>,
    t_ms: i64,
    half_spread_bps: Option<f64>,
) -> AsOf {
    let mut f = BTreeMap::new();
    let mut asof: Option<i64> = None;
    let mut read = |t: i64| asof = Some(asof.map_or(t, |a: i64| a.max(t)));
    let iv = bars.interval.ms();
    let obs = bars.observable_at(t_ms);
    if let Some(last) = obs.last() {
        let mut put = |key: &str, v: Option<f64>| {
            if let Some(x) = v.filter(|x| x.is_finite()) {
                f.insert(key.to_string(), x);
            }
        };
        put("ret_1h_bps", ret_bps(obs, iv, HOUR_MS));
        put("ret_24h_bps", ret_bps(obs, iv, 24 * HOUR_MS));
        put("ret_168h_bps", ret_bps(obs, iv, 168 * HOUR_MS));
        put("vol_24h_bps", vol_bps(obs, iv, DAY_MS));
        put("volume_ratio_24h", volume_ratio(obs, iv));
        put("trades_last_bar", last.n.map(|n| n as f64));
        if !f.is_empty() {
            read(last.t_close_ms(bars.interval));
        }
    }
    let ctx_row = ctx.and_then(|c| c.last_at(t_ms));
    let funding_row = funding
        .and_then(|s| s.observable_at(t_ms).last())
        .filter(|p| p.rate_1h.is_finite());
    if let Some(p) = funding_row {
        f.insert("funding_apr_pct".into(), p.rate_1h * HOURS_PER_YEAR * 100.0);
        read(p.t_ms);
    } else if let Some((c, rate)) =
        ctx_row.and_then(|c| c.funding_1h.filter(|r| r.is_finite()).map(|r| (c, r)))
    {
        f.insert("funding_apr_pct".into(), rate * HOURS_PER_YEAR * 100.0);
        read(c.t_ms);
    }
    if let Some(h) = half_spread_bps.filter(|h| h.is_finite()) {
        f.insert("half_spread_bps".into(), h);
    } else if let Some((c, h)) = ctx_row.and_then(|c| c.impact_half_spread_bps().map(|h| (c, h))) {
        f.insert("half_spread_bps".into(), h);
        read(c.t_ms);
    }
    f.insert("hour_of_week".into(), hour_of_week(t_ms) as f64);
    AsOf {
        features: f,
        asof_ms: asof,
    }
}

/// UTC hour of the week at `t_ms`, Monday 00:00 = 0 … Sunday 23:00 = 167.
pub fn hour_of_week(t_ms: i64) -> i64 {
    (t_ms - MONDAY_MS).div_euclid(HOUR_MS).rem_euclid(168)
}

/// Close-to-close return over `span_ms` back from the last bar of `obs`.
fn ret_bps(obs: &[Bar], iv: i64, span_ms: i64) -> Option<f64> {
    if span_ms < iv || span_ms % iv != 0 {
        return None;
    }
    let last = obs.last()?;
    let t_ref = last.t_open_ms.checked_sub(span_ms)?;
    let i = obs.binary_search_by_key(&t_ref, |b| b.t_open_ms).ok()?;
    ln_bps(last.c, obs[i].c)
}

/// Sample stdev of adjacent-bar log returns of the bars closing within
/// `window_ms` of the last one, × 10⁴.
fn vol_bps(obs: &[Bar], iv: i64, window_ms: i64) -> Option<f64> {
    let last = obs.last()?;
    let lo = last.t_open_ms - window_ms;
    let start = obs.partition_point(|b| b.t_open_ms <= lo).max(1);
    let rets: Vec<f64> = (start..obs.len())
        .filter(|&j| obs[j].t_open_ms - obs[j - 1].t_open_ms == iv)
        .filter_map(|j| ln_bps(obs[j].c, obs[j - 1].c).map(|r| r / 10_000.0))
        .collect();
    if rets.len() < 2 {
        return None;
    }
    let n = rets.len() as f64;
    let mean = rets.iter().sum::<f64>() / n;
    let var = rets.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (n - 1.0);
    Some(var.sqrt() * 10_000.0)
}

/// Volume of the last 24 h over the mean daily volume of the 7 days before.
fn volume_ratio(obs: &[Bar], iv: i64) -> Option<f64> {
    let last = obs.last()?;
    let t = last.t_open_ms;
    if obs.first()?.t_open_ms > t - 8 * DAY_MS + iv {
        return None;
    }
    // Volume of the bars opening in (lo, hi].
    let sum = |lo: i64, hi: i64| -> f64 {
        let a = obs.partition_point(|b| b.t_open_ms <= lo);
        let b = obs.partition_point(|b| b.t_open_ms <= hi);
        obs[a..b.max(a)].iter().map(|x| x.v).sum()
    };
    let day = sum(t - DAY_MS, t);
    let baseline = sum(t - 8 * DAY_MS, t - DAY_MS) / 7.0;
    (baseline > 0.0 && day.is_finite()).then(|| day / baseline)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::backtest::testkit::{bar, utc, H};
    use crate::domain::marketdata::{CtxPoint, FundingPoint, Interval};

    const ID: &str = "hyperliquid:xyz:TSLA";

    /// 9 days of hourly bars ending at 2026-09-28 00:00 UTC (a Monday):
    /// close 100 × 1.001^i, volume 1 (10 in the last day), n = i.
    fn hourly() -> BarSeries {
        let t0 = utc("2026-09-19 00:00");
        let bars = (0..216)
            .map(|i| {
                let mut b = bar(t0 + i * H, 100.0 * 1.001f64.powi(i as i32));
                b.v = if i >= 192 { 10.0 } else { 1.0 };
                b.n = Some(i as u64);
                b
            })
            .collect();
        BarSeries::new(ID, Interval::H1, bars)
    }

    #[test]
    fn returns_vol_volume_and_trades_from_closed_bars() {
        let s = hourly();
        let t = utc("2026-09-28 00:00"); // the close of bar 215
        let a = features_traced(&s, None, None, t, None);
        let f = &a.features;
        let step = 1.001f64.ln() * 10_000.0;
        assert!((f["ret_1h_bps"] - step).abs() < 1e-9);
        assert!((f["ret_24h_bps"] - 24.0 * step).abs() < 1e-8);
        assert!((f["ret_168h_bps"] - 168.0 * step).abs() < 1e-7);
        assert!(f["vol_24h_bps"].abs() < 1e-6, "constant returns: no vol");
        // Last 24 h: 24 bars × 10; the 7 days before: 168 bars × 1 = 24 a day.
        assert!((f["volume_ratio_24h"] - 240.0 / 24.0).abs() < 1e-12);
        assert_eq!(f["trades_last_bar"], 215.0);
        assert_eq!(f["hour_of_week"], 0.0, "Monday 00:00 UTC");
        assert_eq!(a.asof_ms, Some(t));
        assert!(!f.contains_key("funding_apr_pct") && !f.contains_key("half_spread_bps"));
        assert!(f.keys().all(|k| FEATURE_KEYS.contains(&k.as_str())));
        // Half an hour later the same bar is the last one observable.
        let later = features_traced(&s, None, None, t + H / 2, None);
        assert_eq!(later.asof_ms, Some(t));
        assert_eq!(later.features["ret_1h_bps"], f["ret_1h_bps"]);
        assert_eq!(later.features["hour_of_week"], 0.0);
    }

    #[test]
    fn vol_is_the_sample_stdev_of_adjacent_returns() {
        let t0 = utc("2026-09-27 00:00");
        // Returns +1 %, −1 %, +1 %, −1 % … over 24 bars (25 closes).
        let mut px = 100.0;
        let mut bars = vec![bar(t0, px)];
        for i in 1..25 {
            px *= if i % 2 == 1 { 1.01 } else { 1.0 / 1.01 };
            bars.push(bar(t0 + i * H, px));
        }
        let s = BarSeries::new(ID, Interval::H1, bars);
        let f = features_asof(&s, None, None, t0 + 25 * H, None);
        let r = 1.01f64.ln();
        // 24 returns ±r, mean 0: sample variance = 24 r² / 23.
        let want = (24.0 * r * r / 23.0).sqrt() * 10_000.0;
        assert!(
            (f["vol_24h_bps"] - want).abs() < 1e-9,
            "{}",
            f["vol_24h_bps"]
        );
        assert!(!f.contains_key("ret_168h_bps"), "not enough history");
        assert!(!f.contains_key("volume_ratio_24h"), "history under 8 days");
        // A gap: the return across it is not taken.
        let mut gappy = s.bars.clone();
        gappy.remove(12);
        let g = features_asof(
            &BarSeries::new(ID, Interval::H1, gappy),
            None,
            None,
            t0 + 25 * H,
            None,
        );
        assert!(g["vol_24h_bps"] > 0.0);
    }

    #[test]
    fn funding_spread_and_hour_of_week_with_fallbacks() {
        let s = hourly();
        let t = utc("2026-09-27 22:00"); // Sunday
        let funding = FundingSeries::new(
            ID,
            vec![
                FundingPoint {
                    t_ms: t - H,
                    rate_1h: 0.0000125,
                    premium: None,
                },
                FundingPoint {
                    t_ms: t + 1,
                    rate_1h: 0.01,
                    premium: None,
                },
            ],
        );
        let ctx = CtxSeries::new(
            ID,
            vec![CtxPoint {
                t_ms: t - 60_000,
                mid: Some(100.0),
                impact_bid: Some(99.95),
                impact_ask: Some(100.05),
                funding_1h: Some(-0.0001),
                ..Default::default()
            }],
        );
        let a = features_traced(&s, Some(&funding), Some(&ctx), t, Some(1.5));
        assert!((a.features["funding_apr_pct"] - 0.0000125 * 8760.0 * 100.0).abs() < 1e-12);
        assert_eq!(
            a.features["half_spread_bps"], 1.5,
            "the cost model's value wins"
        );
        assert_eq!(a.features["hour_of_week"], (6 * 24 + 22) as f64);
        assert_eq!(
            a.asof_ms,
            Some(t),
            "the bar closing at t is the latest read"
        );
        // Without a funding row or a given spread: the ctx row.
        let b = features_traced(&s, None, Some(&ctx), t, None);
        assert!((b.features["funding_apr_pct"] - (-0.0001 * 8760.0 * 100.0)).abs() < 1e-12);
        assert!((b.features["half_spread_bps"] - 5.0).abs() < 1e-9);
        // Nothing but the clock: only hour_of_week, no time read.
        let empty = BarSeries::new(ID, Interval::H1, vec![]);
        let c = features_traced(&empty, None, None, t, None);
        assert_eq!(c.features.len(), 1);
        assert_eq!(c.asof_ms, None);
        assert_eq!(hour_of_week(utc("2026-09-28 01:00")), 1);
        assert_eq!(hour_of_week(utc("2026-09-27 23:00")), 167);
    }

    /// A bar, a funding row or a ctx row after t changes nothing at t.
    #[test]
    fn rows_after_t_are_invisible() {
        let s = hourly();
        let t = utc("2026-09-27 12:00");
        let base = features_traced(&s, None, None, t, None);
        let mut future = s.clone();
        for b in future.bars.iter_mut().filter(|b| b.t_open_ms >= t) {
            b.c *= 1.5;
            b.h *= 1.5;
            b.v *= 50.0;
        }
        assert_eq!(features_traced(&future, None, None, t, None), base);
        assert!(base.asof_ms.unwrap() <= t);
    }
}
