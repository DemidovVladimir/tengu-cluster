//! Cross-kind checks of the backtest engine (compiled for tests only).
//!
//! | Check | Holds |
//! |---|---|
//! | Time integrity (`docs/xlab-2026-10-01.md` § 39) | on a deterministic random market, every kind's candidates and skips (incl. `thin_entry`: two kinds set `min_entry_trades`) at or before an instant t are unchanged when the bar right after t jumps 50 %, and when every bar, funding row and ctx row after t moves; `data_asof_ms ≤ decided_at_ms` for every candidate and trade of every kind, both arms |
//! | Rule W golden | `weekend_window` on the 2026-09-26 golden candles (`weekend_fade::golden`, 5 m) with half the round-trip cost per side, no spread, slippage or funding = `weekend_fade::replay`: the same 74 names, prices, signals, sides and gross bps, net within 1e-9, the same mean; `top_n` 4 / 50 bps = the replay's capped four |

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value};

use crate::domain::backtest::costs::{CostSpec, HalfSpread};
use crate::domain::backtest::engine::{
    candidates, simulate, Arm, Candidate, CandidateSet, MarketData, RiskCaps, RunParams, Skip,
    SkipReason,
};
use crate::domain::backtest::spec::StrategySpec;
use crate::domain::backtest::testkit::{et, nyse, random_market, run_params, utc, H};
use crate::domain::marketdata::{fmt_time, Bar, BarSeries, Interval};
use crate::domain::xm::weekend_fade::{fade_window, golden, replay, FadeRule};

const IDS: [&str; 3] = [
    "hyperliquid:xyz:AAA",
    "hyperliquid:xyz:BBB",
    "hyperliquid:xyz:CCC",
];

/// Five-plus weeks of hourly bars from Mon 2026-08-31; decisions from day 9
/// (features have their 8 days) to day 36.
fn world() -> (MarketData, RunParams) {
    let t0 = utc("2026-08-31 00:00");
    let md = random_market(&IDS, t0, 24 * 40, 11);
    let mut p = run_params(t0 + 9 * 24 * H, t0 + 36 * 24 * H);
    p.universe = IDS.iter().map(|s| s.to_string()).collect();
    let cost = |half_spread| CostSpec {
        taker_fee_bps: 1.5,
        half_spread,
        slippage_bps: 0.5,
        funding: true,
    };
    p.costs = BTreeMap::from([
        (
            "hyperliquid:".to_string(),
            cost(HalfSpread::AbdiRanaldo {
                window_bars: 24,
                floor_bps: 0.5,
            }),
        ),
        (
            IDS[2].to_string(),
            cost(HalfSpread::Ctx { fallback_bps: 3.0 }),
        ),
    ]);
    (md, p)
}

/// One spec of every kind, tuned to decide on [`world`].
fn every_kind(p: &RunParams) -> Vec<StrategySpec> {
    let events: Vec<Value> = (0..12)
        .map(|i| {
            let t = p.from_ms + i * 49 * H + 17 * 60_000 + 13_000;
            json!({"instrument": IDS[i as usize % 3], "t": fmt_time(t), "label": format!("e{i}")})
        })
        .collect();
    [
        json!({"kind": "weekend_window", "universe": IDS, "interval": "1h", "calendar": "us_equity",
               "direction": "fade"}),
        json!({"kind": "daily_window", "universe": IDS, "interval": "1h", "days": "weekdays",
               "tz": "America/New_York", "anchor": "16:00", "entry": "20:00", "exit": "10:00",
               "direction": "follow", "top_n": 2, "min_entry_trades": 3}),
        json!({"kind": "move_trigger", "universe": IDS, "interval": "1h", "lookback_bars": 3,
               "threshold_bps": 150, "min_volume_ratio": 0.5, "volume_baseline_bars": 24,
               "direction": "fade", "hold_bars": 6, "take_profit_bps": 100, "stop_loss_bps": 100,
               "min_entry_trades": 3}),
        json!({"kind": "funding_carry", "universe": IDS, "interval": "1h", "min_apr_pct": 60,
               "exit_apr_pct": 20, "hold_hours": 24}),
        json!({"kind": "pair_spread", "interval": "1h", "legs": [IDS[0], IDS[1]], "lookback_bars": 24,
               "entry_z": 1.5, "exit_z": 0.3, "max_hold_bars": 24}),
        json!({"kind": "event_window", "interval": "1h", "events": events, "entry_delay_mins": 30,
               "direction": "follow", "exit_after_mins": 180}),
    ]
    .into_iter()
    .map(|v| StrategySpec::from_value("k", &v).unwrap_or_else(|e| panic!("{v}: {e:?}")))
    .collect()
}

/// `md` with the future after `t` moved: the bar opening at `t` jumps 50 %
/// (`next_only`), or every bar from `t` on jumps, 4× volume, funding rows
/// after `t` × −3 and ctx impact asks × 1.5.
fn moved_after(md: &MarketData, t: i64, next_only: bool) -> MarketData {
    let mut out = md.clone();
    let hit = |b: &Bar| {
        if next_only {
            b.t_open_ms == t
        } else {
            b.t_open_ms >= t
        }
    };
    for s in out.bars.values_mut() {
        for b in s.bars.iter_mut().filter(|b| hit(b)) {
            b.o *= 1.5;
            b.h *= 1.5;
            b.l *= 1.5;
            b.c *= 1.5;
            b.v *= 4.0;
        }
    }
    if !next_only {
        for f in out.funding.values_mut() {
            for p in f.points.iter_mut().filter(|p| p.t_ms > t) {
                p.rate_1h *= -3.0;
            }
        }
        for c in out.ctx.values_mut() {
            for p in c.points.iter_mut().filter(|p| p.t_ms > t) {
                p.impact_ask = p.impact_ask.map(|x| x * 1.5);
            }
        }
    }
    out
}

/// Candidates and skips decided at or before `t`.
fn upto(set: &CandidateSet, t: i64) -> (Vec<&Candidate>, Vec<&Skip>) {
    (
        set.candidates
            .iter()
            .filter(|c| c.decided_at_ms <= t)
            .collect(),
        set.skipped
            .iter()
            .filter(|k| k.decided_at_ms <= t)
            .collect(),
    )
}

/// § 39 (a): what a decision saw never moves when the future does.
#[test]
fn a_jump_after_a_decision_changes_nothing_at_or_before_it() {
    let (md, p) = world();
    let mut thin = 0;
    for s in every_kind(&p) {
        let kind = s.kind_name();
        let base = candidates(&s, &md, &p).unwrap();
        assert!(!base.candidates.is_empty(), "{kind} decided nothing");
        thin += base
            .skipped
            .iter()
            .filter(|k| k.reason == SkipReason::ThinEntry)
            .count();
        let instants: BTreeSet<i64> = base.candidates.iter().map(|c| c.decided_at_ms).collect();
        let step = (instants.len() / 10).max(1);
        let mut future_read = false;
        for &t in instants.iter().step_by(step) {
            for next_only in [true, false] {
                let again = candidates(&s, &moved_after(&md, t, next_only), &p).unwrap();
                assert_eq!(
                    upto(&again, t),
                    upto(&base, t),
                    "{kind} at {} (next bar only: {next_only})",
                    fmt_time(t)
                );
                future_read |= again != base;
            }
        }
        assert!(
            future_read,
            "{kind}: moving the future changed no later decision — the check proves nothing"
        );
    }
    assert!(thin > 0, "no thin_entry skip: the filter went unchecked");
}

/// § 39 (b): `data_asof_ms ≤ decided_at_ms` for every candidate and trade;
/// entries fill at the decision's close.
#[test]
fn every_kind_reads_nothing_after_its_decision() {
    let (md, p) = world();
    let caps = RiskCaps {
        initial_cash_usd: 100.0,
        max_order_notional_usd: 25.0,
        max_gross_exposure_usd: 100.0,
        max_net_exposure_usd: 75.0,
        daily_loss_limit_usd: 10.0,
        total_loss_limit_usd: 25.0,
    };
    for s in every_kind(&p) {
        let kind = s.kind_name();
        let set = candidates(&s, &md, &p).unwrap();
        assert!(!set.candidates.is_empty(), "{kind} decided nothing");
        for c in &set.candidates {
            assert!(c.data_asof_ms <= c.decided_at_ms, "{kind}: {c:?}");
            assert!(
                (p.from_ms..p.to_ms).contains(&c.decided_at_ms),
                "{kind}: {c:?}"
            );
            for leg in &c.legs {
                assert_eq!(
                    md.bars[&leg.instrument].close_at(c.decided_at_ms),
                    Some(leg.entry_px),
                    "{kind}"
                );
            }
        }
        for arm in [Arm::Research, Arm::Capped(caps.clone())] {
            let name = arm.name();
            let r = simulate(&s, &md, &p, &set.candidates, arm);
            assert_eq!(r.arm, name);
            assert!(!r.trades.is_empty(), "{kind} {name} traded nothing");
            for t in &r.trades {
                assert!(t.data_asof_ms <= t.decided_at_ms, "{kind}: {t:?}");
                assert_eq!(t.entry_ms, t.decided_at_ms);
                assert!(t.exit_ms > t.entry_ms);
                assert!(t.net_bps.is_finite() && t.net_usd.is_finite());
            }
            if name == "capped" {
                assert!(r.trades.iter().all(|t| t.notional_usd <= 25.0));
            }
        }
    }
}

/// Rule W, bit for bit where it can be (module table).
#[test]
fn golden_weekend_window_equals_the_rule_w_replay() {
    let candles = golden::candles();
    let bars: BTreeMap<String, BarSeries> = candles
        .iter()
        .map(|(id, cs)| {
            let bars = cs
                .iter()
                .map(|c| Bar {
                    t_open_ms: c.t_ms,
                    o: c.close,
                    h: c.close,
                    l: c.close,
                    c: c.close,
                    v: 0.0,
                    n: Some(c.trades),
                })
                .collect();
            (id.clone(), BarSeries::new(id.clone(), Interval::M5, bars))
        })
        .collect();
    let md = MarketData {
        bars,
        ..Default::default()
    };
    // The window of the rule's golden test, from the same calendar row.
    let w = fade_window(&nyse(), et("2026-09-25 12:00")).unwrap();
    assert_eq!(
        (w.anchor_ms, w.entry_ms, w.exit_ms),
        (
            utc("2026-09-26 00:00"),
            utc("2026-09-27 22:00"),
            utc("2026-09-28 13:00")
        )
    );
    let excluded = golden::excluded();
    let cost_rt = golden::cost_rt_bps();
    let ids: Vec<&String> = candles.keys().collect();
    let spec_with = |extra: Value| {
        let mut v = json!({"kind": "weekend_window", "universe": ids, "interval": "5m",
            "calendar": "us_equity", "direction": "fade", "min_abs_signal_bps": 0,
            "exclude": excluded,
            "costs": {"taker_fee_bps": cost_rt / 2.0, "half_spread": {"model": "fixed", "bps": 0},
                      "slippage_bps": 0, "funding": false}});
        for (k, x) in extra.as_object().unwrap() {
            v[k] = x.clone();
        }
        StrategySpec::from_value("weekend_fade", &v).unwrap()
    };
    let p = run_params(utc("2026-09-25 00:00"), utc("2026-09-29 00:00"));
    let s = spec_with(json!({}));
    let set = candidates(&s, &md, &p).unwrap();
    let r = simulate(&s, &md, &p, &set.candidates, Arm::Research);
    let rule = FadeRule {
        capped_top_n: 4,
        min_abs_signal_bps: 50.0,
    };
    let want = replay(&candles, &w, &excluded, cost_rt, &rule);
    golden::assert_replay(&want);
    assert_eq!(r.trades.len(), want.rows.len());
    assert_eq!(r.trades.len(), 74);
    for ((t, c), row) in r.trades.iter().zip(&set.candidates).zip(&want.rows) {
        let id = &row.instrument;
        assert_eq!((&t.instrument, &c.instrument), (id, id));
        let leg = &t.legs[0];
        assert_eq!(
            (c.anchor_px, leg.entry_px, leg.exit_px),
            (Some(row.anchor_px), row.entry_px, row.exit_px),
            "{id}"
        );
        assert_eq!(t.signal_bps, row.s_bps, "{id}");
        assert_eq!(t.side.sign(), f64::from(row.dir), "{id}");
        assert_eq!(t.gross_bps, row.gross_bps, "{id} gross, bit for bit");
        assert!((t.net_bps - row.net_bps).abs() < 1e-9, "{id} net");
        assert_eq!(t.fee_bps, cost_rt, "{id}: half the round trip a side");
        assert_eq!((t.decided_at_ms, t.exit_ms), (w.entry_ms, w.exit_ms));
        assert_eq!(t.period, "2026-09-25");
    }
    assert_eq!(
        r.summary.mean_net_bps, want.mean_net_bps,
        "the mean, summed in id order"
    );
    assert_eq!(r.summary.n, 74);
    assert_eq!(
        r.summary.hit_rate,
        Some(want.positive as f64 / 74.0),
        "53 positive"
    );
    assert_eq!(
        set.skip_counts(),
        BTreeMap::from([("excluded".to_string(), 1)]),
        "only the split-halted KIOXIA"
    );
    // The capped selection: the four largest |s| ≥ 50 bps.
    let capped = spec_with(json!({"top_n": 4, "min_abs_signal_bps": 50}));
    let set4 = candidates(&capped, &md, &p).unwrap();
    let got: BTreeSet<&str> = set4
        .candidates
        .iter()
        .map(|c| c.instrument.as_str())
        .collect();
    let want4: BTreeSet<&str> = want.capped.iter().map(String::as_str).collect();
    assert_eq!(got, want4);
    assert_eq!(set4.candidates.len(), 4);
    let r4 = simulate(&capped, &md, &p, &set4.candidates, Arm::Research);
    for t in &r4.trades {
        let row = want
            .rows
            .iter()
            .find(|row| row.instrument == t.instrument)
            .unwrap();
        assert_eq!(t.gross_bps, row.gross_bps);
    }
}
