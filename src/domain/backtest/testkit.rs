//! Test fixtures of the backtest modules (compiled for tests only).
//!
//! | Helper | Gives |
//! |---|---|
//! | `utc` / `et` | `YYYY-MM-DD HH:MM` in UTC / New York → epoch ms |
//! | `bar` / `ohlc` / `series` / `sparse` / `market` / `funding` | bars, series and a `MarketData` |
//! | `nyse` | the `us_equity` row of `tests/fixtures/xmarket/calendars.toml` (holidays 2026–2028) |
//! | `run_params` | `[from, to)`, $100 a trade, free costs for `hyperliquid:`, the NYSE calendar as `us_equity`, B = 200, seed 7 |
//! | `spec` | a spec named `t` from JSON (panics on errors) |
//! | `trade` | a hand-made trade for the statistics |
//! | `random_market` | a deterministic random market: correlated hourly bars with jumps, gaps and volume spikes, hourly funding (AR(1), stamped 37 ms late), ctx rows for the last id |

use std::collections::BTreeMap;

use chrono::NaiveDateTime;
use serde_json::Value;

use crate::domain::backtest::costs::{CostSpec, HalfSpread};
use crate::domain::backtest::engine::{MarketData, RunParams, Trade};
use crate::domain::backtest::fills::ExitReason;
use crate::domain::backtest::spec::StrategySpec;
use crate::domain::backtest::stats::SplitMix64;
use crate::domain::book::Side;
use crate::domain::calendar::{parse_date, parse_hm, Calendar, ExchangeCalendar};
use crate::domain::marketdata::{
    Bar, BarSeries, CtxPoint, CtxSeries, FundingPoint, FundingSeries, Interval,
};
use crate::domain::tz::Zone;

pub(crate) const H: i64 = 3_600_000;

fn naive(s: &str) -> NaiveDateTime {
    NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap()
}

pub(crate) fn utc(s: &str) -> i64 {
    naive(s).and_utc().timestamp_millis()
}

/// Wall-clock time in New York → epoch ms.
pub(crate) fn et(s: &str) -> i64 {
    Zone::NewYork.to_utc_ms(naive(s))
}

/// A flat bar: o = h = l = c, volume 1, one trade.
pub(crate) fn bar(t_open_ms: i64, c: f64) -> Bar {
    ohlc(t_open_ms, c, c, c, c)
}

pub(crate) fn ohlc(t_open_ms: i64, o: f64, h: f64, l: f64, c: f64) -> Bar {
    Bar {
        t_open_ms,
        o,
        h,
        l,
        c,
        v: 1.0,
        n: Some(1),
    }
}

/// Consecutive bars from `t0` with these closes.
pub(crate) fn series(id: &str, iv: Interval, t0: i64, closes: &[f64]) -> BarSeries {
    BarSeries::new(
        id,
        iv,
        closes
            .iter()
            .enumerate()
            .map(|(i, c)| bar(t0 + i as i64 * iv.ms(), *c))
            .collect(),
    )
}

/// Bars at these (open time, close) points.
pub(crate) fn sparse(id: &str, iv: Interval, points: &[(i64, f64)]) -> BarSeries {
    BarSeries::new(id, iv, points.iter().map(|&(t, c)| bar(t, c)).collect())
}

pub(crate) fn funding(id: &str, points: &[(i64, f64)]) -> FundingSeries {
    FundingSeries::new(
        id,
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

pub(crate) fn market(bars: Vec<BarSeries>) -> MarketData {
    MarketData {
        bars: bars
            .into_iter()
            .map(|s| (s.instrument.clone(), s))
            .collect(),
        ..Default::default()
    }
}

/// NYSE — the `us_equity` row of `tests/fixtures/xmarket/calendars.toml`.
pub(crate) fn nyse() -> ExchangeCalendar {
    let hm = |s: &str| parse_hm(s).unwrap();
    let dates = |list: &[&str]| list.iter().map(|d| parse_date(d).unwrap()).collect();
    ExchangeCalendar {
        zone: Zone::NewYork,
        open: hm("09:30"),
        close: hm("16:00"),
        pre: Some(hm("04:00")),
        post: Some(hm("20:00")),
        overnight: true,
        early_close: Some(hm("13:00")),
        early_post: Some(hm("17:00")),
        holidays: dates(&[
            "2026-01-01",
            "2026-01-19",
            "2026-02-16",
            "2026-04-03",
            "2026-05-25",
            "2026-06-19",
            "2026-07-03",
            "2026-09-07",
            "2026-11-26",
            "2026-12-25",
            "2027-01-01",
            "2027-01-18",
            "2027-02-15",
            "2027-03-26",
            "2027-05-31",
            "2027-06-18",
            "2027-07-05",
            "2027-09-06",
            "2027-11-25",
            "2027-12-24",
            "2028-01-17",
            "2028-02-21",
            "2028-04-14",
            "2028-05-29",
            "2028-06-19",
            "2028-07-04",
            "2028-09-04",
            "2028-11-23",
            "2028-12-25",
        ]),
        early_closes: dates(&[
            "2026-11-27",
            "2026-12-24",
            "2027-11-26",
            "2028-07-03",
            "2028-11-24",
        ]),
    }
}

pub(crate) fn free_costs() -> CostSpec {
    CostSpec {
        taker_fee_bps: 0.0,
        half_spread: HalfSpread::Fixed { bps: 0.0 },
        slippage_bps: 0.0,
        funding: false,
    }
}

pub(crate) fn run_params(from_ms: i64, to_ms: i64) -> RunParams {
    RunParams {
        from_ms,
        to_ms,
        universe: Vec::new(),
        notional_usd: 100.0,
        costs: BTreeMap::from([("hyperliquid:".to_string(), free_costs())]),
        calendars: BTreeMap::from([("us_equity".to_string(), Calendar::Exchange(nyse()))]),
        bootstrap: 200,
        seed: 7,
    }
}

/// A spec named `t`; panics with the errors.
pub(crate) fn spec(v: Value) -> StrategySpec {
    StrategySpec::from_value("t", &v).unwrap_or_else(|e| panic!("{e:?}"))
}

/// A trade for the statistics: `net_bps` on `notional_usd`, 1 bps fee,
/// decided 1 ms before `exit_ms`.
pub(crate) fn trade(
    period: &str,
    instrument: &str,
    net_bps: f64,
    notional_usd: f64,
    exit_ms: i64,
) -> Trade {
    Trade {
        seq: 0,
        instrument: instrument.into(),
        side: Side::Buy,
        legs: Vec::new(),
        signal_bps: 0.0,
        decided_at_ms: exit_ms - 1,
        data_asof_ms: exit_ms - 1,
        entry_ms: exit_ms - 1,
        exit_ms,
        notional_usd,
        gross_bps: net_bps + 1.0,
        fee_bps: 1.0,
        spread_bps: 0.0,
        slippage_bps: 0.0,
        funding_bps: 0.0,
        funding_complete: true,
        net_bps,
        net_usd: net_bps * notional_usd / 10_000.0,
        period: period.into(),
        exit_reason: ExitReason::Window,
        label: None,
    }
}

/// `hours` hourly bars from `t0` per id (module table), seeded.
pub(crate) fn random_market(ids: &[&str], t0: i64, hours: usize, seed: u64) -> MarketData {
    let mut rng = SplitMix64::new(seed);
    let k = ids.len();
    let mut logs = vec![100f64.ln(); k];
    let mut rate = vec![0.0f64; k];
    let mut bars: Vec<Vec<Bar>> = vec![Vec::new(); k];
    let mut rows: Vec<Vec<FundingPoint>> = vec![Vec::new(); k];
    let mut ctx: Vec<CtxPoint> = Vec::new();
    for h in 0..hours {
        let t = t0 + h as i64 * H;
        let common = (rng.unit() - 0.5) * 0.01;
        for i in 0..k {
            let o = logs[i].exp();
            let jump = if rng.unit() < 0.04 {
                (rng.unit() - 0.5) * 0.08
            } else {
                0.0
            };
            logs[i] += common + (rng.unit() - 0.5) * 0.012 + jump;
            let c = logs[i].exp();
            let wiggle = 1.0 + rng.unit() * 0.003;
            let v = 1.0 + rng.unit() * 4.0 + if jump != 0.0 { 20.0 } else { 0.0 };
            rate[i] = (rate[i] * 0.9 + (rng.unit() - 0.5) * 0.0002).clamp(-0.0003, 0.0003);
            if rng.unit() > 0.01 {
                rows[i].push(FundingPoint {
                    t_ms: t + 37,
                    rate_1h: rate[i],
                    premium: None,
                });
            }
            if i + 1 == k {
                for m in [10, 40] {
                    ctx.push(CtxPoint {
                        t_ms: t + m * 60_000,
                        mid: Some(c),
                        impact_bid: Some(c * (1.0 - 0.0004)),
                        impact_ask: Some(c * (1.0 + 0.0004)),
                        funding_1h: Some(rate[i]),
                        ..Default::default()
                    });
                }
            }
            // An hour without a trade now and then: no bar.
            if rng.unit() < 0.02 {
                continue;
            }
            bars[i].push(Bar {
                t_open_ms: t,
                o,
                h: o.max(c) * wiggle,
                l: o.min(c) / wiggle,
                c,
                v,
                n: Some(1 + v as u64),
            });
        }
    }
    let mut md = MarketData::default();
    for (i, id) in ids.iter().enumerate() {
        md.bars.insert(
            id.to_string(),
            BarSeries::new(*id, Interval::H1, std::mem::take(&mut bars[i])),
        );
        md.funding.insert(
            id.to_string(),
            FundingSeries::new(*id, std::mem::take(&mut rows[i])),
        );
    }
    if let Some(last) = ids.last() {
        md.ctx.insert(last.to_string(), CtxSeries::new(*last, ctx));
    }
    md
}
