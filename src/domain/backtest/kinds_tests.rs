//! Tests of `src/domain/backtest/kinds.rs`.

use serde_json::json;

use super::*;
use crate::domain::backtest::engine::{candidates, simulate, Arm};
use crate::domain::backtest::fills::ExitReason;
use crate::domain::backtest::testkit::{
    et, funding, market, run_params, series, sparse, spec, utc, H,
};
use crate::domain::marketdata::{EventCoverage, Interval, MarketEvent};

const A: &str = "hyperliquid:xyz:AAA";
const B: &str = "hyperliquid:xyz:BBB";
const C: &str = "hyperliquid:xyz:CCC";
const D: &str = "hyperliquid:xyz:DDD";

/// A long's simple return on exit / entry = `x`, bps (a short's: −ret).
fn ret(x: f64) -> f64 {
    (x - 1.0) * 10_000.0
}

/// The 2026-09-26 → 28 window on 1 h bars: A rose (faded short), B fell
/// (faded long), C excluded, D without bars.
fn weekend_case(extra: serde_json::Value) -> (StrategySpec, MarketData, RunParams) {
    let (anchor, entry, exit) = (
        utc("2026-09-26 00:00"),
        utc("2026-09-27 22:00"),
        utc("2026-09-28 13:00"),
    );
    let px = |a: f64, e: f64, x: f64| [(anchor - H, a), (entry - H, e), (exit - H, x)];
    let md = market(vec![
        sparse(A, Interval::H1, &px(100.0, 102.0, 101.0)),
        sparse(B, Interval::H1, &px(50.0, 49.5, 50.0)),
        sparse(C, Interval::H1, &px(10.0, 11.0, 10.0)),
    ]);
    let mut v = json!({"kind": "weekend_window", "universe": [A, B, C, D], "interval": "1h",
            "calendar": "us_equity", "direction": "fade", "exclude": [C], "notional_usd": 250,
            "costs": {"taker_fee_bps": 2, "half_spread": {"model": "fixed", "bps": 1}, "slippage_bps": 0.5, "funding": false}});
    for (k, x) in extra.as_object().unwrap() {
        v[k] = x.clone();
    }
    (
        spec(v),
        md,
        run_params(utc("2026-09-21 00:00"), utc("2026-09-29 00:00")),
    )
}

/// Phase 9: `stop_loss_bps` turns the window exit into the bar-close
/// stop walk — the fade short leaves at the first close 50 bps against
/// it; without the knob the same trade waits for the window exit.
#[test]
fn weekend_window_stop_leaves_at_the_first_adverse_close() {
    let (anchor, entry, exit) = (
        utc("2026-09-26 00:00"),
        utc("2026-09-27 22:00"),
        utc("2026-09-28 13:00"),
    );
    let md = market(vec![sparse(
        A,
        Interval::H1,
        &[
            (anchor - H, 100.0),
            (entry - H, 102.0),
            (entry, 102.2),
            (entry + H, 103.0),
            (exit - H, 101.0),
        ],
    )]);
    let p = run_params(utc("2026-09-21 00:00"), utc("2026-09-29 00:00"));
    let run = |extra: serde_json::Value| {
        let mut v = json!({"kind": "weekend_window", "universe": [A], "interval": "1h",
                "calendar": "us_equity", "direction": "fade"});
        for (k, x) in extra.as_object().unwrap() {
            v[k] = x.clone();
        }
        let s = spec(v);
        let set = candidates(&s, &md, &p).unwrap();
        let r = simulate(&s, &md, &p, &set.candidates, Arm::Research);
        (set.candidates[0].exit.clone(), r.trades[0].clone())
    };
    let (plan, t) = run(json!({"stop_loss_bps": 50}));
    assert_eq!(
        plan,
        ExitPlan::Bars {
            max_exit_ms: exit,
            take_profit_bps: None,
            stop_loss_bps: Some(50.0)
        }
    );
    // 102.2 at entry + 1 h is −19.6 bps for the short; 103.0 at + 2 h is −98.
    assert_eq!(
        (t.exit_ms, t.exit_reason, t.legs[0].exit_px),
        (entry + 2 * H, ExitReason::StopLoss, 103.0)
    );
    let (plan, t) = run(json!({}));
    assert_eq!(plan, ExitPlan::At { exit_ms: exit });
    assert_eq!((t.exit_ms, t.exit_reason), (exit, ExitReason::Window));
}

/// Phase 9: `rank_by = "net_of_cost"` ranks `top_n` by |s| − 2 × the side
/// cost at the decision: A's larger move (+198 bps) loses its one slot to
/// B (−100 bps) once A's half-spread is 150 bps.
#[test]
fn net_of_cost_ranking_gives_the_slot_to_the_cheaper_name() {
    let (anchor, entry, exit) = (
        utc("2026-09-26 00:00"),
        utc("2026-09-27 22:00"),
        utc("2026-09-28 13:00"),
    );
    let px = |a: f64, e: f64, x: f64| [(anchor - H, a), (entry - H, e), (exit - H, x)];
    let md = market(vec![
        sparse(A, Interval::H1, &px(100.0, 102.0, 101.0)),
        sparse(B, Interval::H1, &px(50.0, 49.5, 50.0)),
    ]);
    let mut p = run_params(utc("2026-09-21 00:00"), utc("2026-09-29 00:00"));
    p.costs.insert(
        A.to_string(),
        serde_json::from_value(
            json!({"taker_fee_bps": 0, "half_spread": {"model": "fixed", "bps": 150}}),
        )
        .unwrap(),
    );
    let pick = |rank: Option<&str>| {
        let mut v = json!({"kind": "weekend_window", "universe": [A, B], "interval": "1h",
                "calendar": "us_equity", "direction": "fade", "top_n": 1});
        if let Some(r) = rank {
            v["rank_by"] = json!(r);
        }
        let set = candidates(&spec(v), &md, &p).unwrap();
        assert_eq!(set.candidates.len(), 1);
        set.candidates[0].instrument.clone()
    };
    assert_eq!(pick(None), A);
    assert_eq!(pick(Some("signal")), A);
    assert_eq!(pick(Some("net_of_cost")), B);
    // A ranking with nothing to rank, and a stop of 0, are refused.
    let bad = |extra: serde_json::Value| {
        let mut v = json!({"kind": "weekend_window", "universe": [A], "interval": "1h",
                "calendar": "us_equity", "direction": "fade"});
        for (k, x) in extra.as_object().unwrap() {
            v[k] = x.clone();
        }
        format!("{:?}", StrategySpec::from_value("t", &v).unwrap_err())
    };
    assert!(bad(json!({"rank_by": "net_of_cost"})).contains("set top_n"));
    assert!(bad(json!({"stop_loss_bps": 0})).contains("stop_loss_bps"));
}

#[test]
fn weekend_window_trades_by_hand() {
    let (s, md, p) = weekend_case(json!({}));
    let set = candidates(&s, &md, &p).unwrap();
    let entry = utc("2026-09-27 22:00");
    assert_eq!(set.candidates.len(), 2);
    let (a, b) = (&set.candidates[0], &set.candidates[1]);
    assert_eq!((a.instrument.as_str(), a.side, a.seq), (A, Side::Sell, 0));
    assert_eq!((b.instrument.as_str(), b.side, b.seq), (B, Side::Buy, 1));
    assert_eq!(a.signal_bps, (102.0f64 / 100.0).ln() * 10_000.0);
    assert_eq!((a.decided_at_ms, a.data_asof_ms), (entry, entry));
    assert_eq!(a.period, "2026-09-25");
    assert_eq!(a.anchor_px, Some(100.0));
    assert_eq!(
        a.exit,
        ExitPlan::At {
            exit_ms: utc("2026-09-28 13:00")
        }
    );
    assert_eq!(a.features["half_spread_bps"], 1.0);
    let skips: Vec<(&str, SkipReason)> = set
        .skipped
        .iter()
        .map(|k| (k.instrument.as_str(), k.reason))
        .collect();
    assert_eq!(
        skips,
        vec![(C, SkipReason::Excluded), (D, SkipReason::MissingAnchor)]
    );
    assert_eq!(set.skip_counts()["excluded"], 1);

    let r = simulate(&s, &md, &p, &set.candidates, Arm::Research);
    assert_eq!(r.trades.len(), 2);
    let ta = &r.trades[0];
    assert_eq!(ta.gross_bps, -1.0 * (101.0f64 / 102.0 - 1.0) * 10_000.0);
    assert_eq!(
        (ta.fee_bps, ta.spread_bps, ta.slippage_bps),
        (4.0, 2.0, 1.0)
    );
    assert!((ta.net_bps - (ta.gross_bps - 7.0)).abs() < 1e-12);
    assert_eq!(ta.notional_usd, 250.0);
    assert!((ta.net_usd - ta.net_bps * 250.0 / 10_000.0).abs() < 1e-12);
    assert_eq!((ta.funding_bps, ta.funding_complete), (0.0, true));
    assert_eq!(
        (ta.entry_ms, ta.exit_ms, ta.exit_reason),
        (entry, utc("2026-09-28 13:00"), ExitReason::Window)
    );
    assert_eq!((ta.legs[0].entry_px, ta.legs[0].exit_px), (102.0, 101.0));
    let tb = &r.trades[1];
    assert!((tb.gross_bps - ret(50.0 / 49.5)).abs() < 1e-9);
    assert_eq!(r.summary.n, 2);
    assert_eq!(r.summary.n_periods, 1);
    assert_eq!(r.summary.periods_per_year, 52.0);

    // follow flips the sides; top_n / min keep the larger move.
    let (s, md, p) = weekend_case(json!({"direction": "follow"}));
    let set = candidates(&s, &md, &p).unwrap();
    assert_eq!(
        (set.candidates[0].side, set.candidates[1].side),
        (Side::Buy, Side::Sell)
    );
    for extra in [json!({"top_n": 1}), json!({"min_abs_signal_bps": 150})] {
        let (s, md, p) = weekend_case(extra.clone());
        let set = candidates(&s, &md, &p).unwrap();
        assert_eq!(set.candidates.len(), 1, "{extra}");
        assert_eq!(set.candidates[0].instrument, A);
        let why = set
            .skipped
            .iter()
            .find(|k| k.instrument == B)
            .unwrap()
            .reason;
        let want = if extra.get("top_n").is_some() {
            SkipReason::NotTopN
        } else {
            SkipReason::BelowMinSignal
        };
        assert_eq!(why, want);
    }
    // Offsets move the instants (whole bars); the bar there is missing.
    let (s, md, p) = weekend_case(json!({"entry_offset_mins": 60}));
    let set = candidates(&s, &md, &p).unwrap();
    assert!(set.candidates.is_empty());
    assert!(set
        .skipped
        .iter()
        .any(|k| k.reason == SkipReason::MissingEntry && k.decided_at_ms == entry + H));
}

/// `min_entry_trades` per kind (module table): `(kind, filter) → (decided
/// instants, thin_entry skips)`; windows filter before `top_n`, a thin
/// trigger starts no cooldown, a thin carry or pair opens no position; a
/// bar without a trade count passes.
#[test]
fn thin_entries_are_skipped_before_ranking_cooldown_and_positions() {
    fn set_n(md: &mut MarketData, id: &str, t_open: i64, n: Option<u64>) {
        let s = md.bars.get_mut(id).unwrap();
        let b = s.bars.iter_mut().find(|b| b.t_open_ms == t_open).unwrap();
        b.n = n;
    }
    let with = |mut v: serde_json::Value, min: Option<u64>| {
        if let Some(m) = min {
            v["min_entry_trades"] = json!(m);
        }
        spec(v)
    };
    let run = |s: &StrategySpec, md: &MarketData, p: &RunParams| {
        let set = candidates(s, md, p).unwrap();
        let decided: Vec<(String, i64)> = set
            .candidates
            .iter()
            .map(|c| (c.instrument.clone(), c.decided_at_ms))
            .collect();
        let thin: Vec<(String, i64)> = set
            .skipped
            .iter()
            .filter(|k| k.reason == SkipReason::ThinEntry)
            .map(|k| (k.instrument.clone(), k.decided_at_ms))
            .collect();
        (decided, thin)
    };
    let pair = |id: &str, t: i64| (id.to_string(), t);

    // Weekend window, top 1: A moved most but its entry hour traded 3
    // times; B (100 trades) takes the slot. Without n, A passes.
    let (w, mut md, p) = weekend_case(json!({"top_n": 1}));
    let entry = utc("2026-09-27 22:00");
    set_n(&mut md, A, entry - H, Some(3));
    set_n(&mut md, B, entry - H, Some(100));
    let wv = w.to_value();
    assert_eq!(
        run(&with(wv.clone(), None), &md, &p),
        (vec![pair(A, entry)], vec![])
    );
    assert_eq!(
        run(&with(wv.clone(), Some(50)), &md, &p),
        (vec![pair(B, entry)], vec![pair(A, entry)])
    );
    set_n(&mut md, A, entry - H, None);
    assert_eq!(
        run(&with(wv, Some(50)), &md, &p),
        (vec![pair(A, entry)], vec![])
    );

    // Move trigger, cooldown 3: bars 4 (+295 bps, 2 trades) and 5 (+334).
    let t0 = utc("2026-09-28 00:00");
    let closes = [
        100.0, 100.0, 100.0, 100.0, 103.0, 106.5, 106.5, 106.5, 106.5,
    ];
    let mut md = market(vec![series(A, Interval::H1, t0, &closes)]);
    for i in 0..closes.len() as i64 {
        set_n(&mut md, A, t0 + i * H, Some(if i == 4 { 2 } else { 50 }));
    }
    let mv = json!({"kind": "move_trigger", "universe": [A], "interval": "1h", "lookback_bars": 1,
            "threshold_bps": 200, "direction": "fade", "hold_bars": 2, "cooldown_bars": 3});
    let p = run_params(t0, t0 + 12 * H);
    assert_eq!(
        run(&with(mv.clone(), None), &md, &p),
        (vec![pair(A, t0 + 5 * H)], vec![])
    );
    assert_eq!(
        run(&with(mv, Some(10)), &md, &p),
        (vec![pair(A, t0 + 6 * H)], vec![pair(A, t0 + 5 * H)]),
        "the thin trigger starts no cooldown"
    );

    // Funding carry: the 02:00 entry bar is thin — the 03:00 settlement
    // enters instead (no position was open), then 06:00 as before.
    let mut closes = vec![100.0; 30];
    closes[4] = 99.0;
    let mut md = market(vec![series(A, Interval::H1, t0, &closes)]);
    let rates = [0.00001, 0.0001, 0.0001, 0.0001, 0.000001, 0.0001, 0.0001];
    let rows: Vec<(i64, f64)> = rates
        .iter()
        .enumerate()
        .map(|(k, r)| (t0 + k as i64 * H + 37, *r))
        .collect();
    md.funding.insert(A.into(), funding(A, &rows));
    set_n(&mut md, A, t0 + H, Some(0));
    let fc = json!({"kind": "funding_carry", "universe": [A], "interval": "1h", "min_apr_pct": 50,
            "exit_apr_pct": 10, "hold_hours": 24});
    let p = run_params(t0, t0 + 24 * H);
    assert_eq!(
        run(&with(fc.clone(), None), &md, &p),
        (vec![pair(A, t0 + 2 * H), pair(A, t0 + 6 * H)], vec![])
    );
    assert_eq!(
        run(&with(fc, Some(1)), &md, &p),
        (
            vec![pair(A, t0 + 3 * H), pair(A, t0 + 6 * H)],
            vec![pair(A, t0 + 2 * H)]
        )
    );

    // Pair spread: leg b's entry bar is thin — the pair is skipped.
    let d = 0.01f64;
    let a: Vec<f64> = [d, -d, d, -d, 5.0 * d, d, d]
        .iter()
        .map(|s| 100.0 * s.exp())
        .collect();
    let mut md = market(vec![
        series(A, Interval::H1, t0, &a),
        series(B, Interval::H1, t0, &[100.0; 7]),
    ]);
    set_n(&mut md, B, t0 + 4 * H, Some(4));
    let ps = json!({"kind": "pair_spread", "interval": "1h", "legs": [A, B], "lookback_bars": 4,
            "entry_z": 2, "exit_z": 0.5, "max_hold_bars": 10});
    let p = run_params(t0, t0 + 10 * H);
    let key = format!("{A}/{B}");
    assert_eq!(
        run(&with(ps.clone(), None), &md, &p),
        (vec![pair(&key, t0 + 5 * H)], vec![])
    );
    assert_eq!(
        run(&with(ps, Some(5)), &md, &p),
        (vec![], vec![pair(&key, t0 + 5 * H)])
    );

    // Event window: the 14:00 entry bar (opening 13:00) traded once.
    let day = |h: &str| utc(&format!("2026-09-29 {h}"));
    let mut md = market(vec![sparse(
        A,
        Interval::H1,
        &[
            (day("12:00"), 100.0),
            (day("13:00"), 101.0),
            (day("15:00"), 102.0),
        ],
    )]);
    let ev = json!({"kind": "event_window", "interval": "1h", "direction": "follow",
            "events": [{"instrument": A, "t": "2026-09-29T13:30:00Z"}],
            "entry_delay_mins": 30, "exit_after_mins": 120});
    let p = run_params(utc("2026-09-29 00:00"), utc("2026-09-30 00:00"));
    assert_eq!(
        run(&with(ev.clone(), Some(1)), &md, &p),
        (vec![pair(A, day("14:00"))], vec![])
    );
    set_n(&mut md, A, day("13:00"), Some(0));
    assert_eq!(
        run(&with(ev, Some(1)), &md, &p),
        (vec![], vec![pair(A, day("14:00"))])
    );
}

#[test]
fn daily_window_instants_and_trades() {
    // Weekdays: Mon 2026-09-28 20:00 EDT, anchor 16:00 the same day,
    // exit 10:00 the next day.
    let md = market(vec![sparse(
        A,
        Interval::H1,
        &[
            (et("2026-09-28 15:00"), 100.0),
            (et("2026-09-28 19:00"), 103.0),
            (et("2026-09-29 09:00"), 104.0),
        ],
    )]);
    let s = spec(
        json!({"kind": "daily_window", "universe": [A], "interval": "1h", "days": "weekdays",
            "tz": "America/New_York", "anchor": "16:00", "entry": "20:00", "exit": "10:00", "direction": "follow"}),
    );
    let p = run_params(utc("2026-09-28 12:00"), utc("2026-09-29 12:00"));
    let set = candidates(&s, &md, &p).unwrap();
    assert_eq!(set.candidates.len(), 1, "{:?}", set.skipped);
    let c = &set.candidates[0];
    assert_eq!(
        (c.side, c.decided_at_ms, c.period.as_str()),
        (Side::Buy, et("2026-09-28 20:00"), "2026-09-28")
    );
    assert_eq!(
        c.exit,
        ExitPlan::At {
            exit_ms: et("2026-09-29 10:00")
        }
    );
    let r = simulate(&s, &md, &p, &set.candidates, Arm::Research);
    assert!((r.trades[0].gross_bps - ret(104.0 / 103.0)).abs() < 1e-9);
    assert_eq!(r.summary.periods_per_year, 261.0);

    // Trading days around Labor Day (Mon 2026-09-07): anchor 21:00 ≥ the
    // entry time ⇒ the previous trading day; exit ⇒ the next one.
    let t0 = et("2026-09-02 00:00");
    let closes: Vec<f64> = (0..240).map(|i| 100.0 + i as f64 * 0.01).collect();
    let md = market(vec![series(A, Interval::H1, t0, &closes)]);
    let s = spec(
        json!({"kind": "daily_window", "universe": [A], "interval": "1h", "days": "trading",
            "calendar": "us_equity", "tz": "America/New_York", "anchor": "21:00", "entry": "20:00",
            "exit": "10:00", "direction": "fade"}),
    );
    let p = run_params(et("2026-09-04 00:00"), et("2026-09-09 00:00"));
    let set = candidates(&s, &md, &p).unwrap();
    let got: Vec<(i64, Option<f64>, ExitPlan)> = set
        .candidates
        .iter()
        .map(|c| (c.decided_at_ms, c.anchor_px, c.exit.clone()))
        .collect();
    let close_at = |t: i64| md.bars[A].close_at(t);
    assert_eq!(
        got,
        vec![
            (
                et("2026-09-04 20:00"),
                close_at(et("2026-09-03 21:00")),
                ExitPlan::At {
                    exit_ms: et("2026-09-08 10:00")
                }
            ),
            (
                et("2026-09-08 20:00"),
                close_at(et("2026-09-04 21:00")),
                ExitPlan::At {
                    exit_ms: et("2026-09-09 10:00")
                }
            ),
        ]
    );
    assert!(
        set.candidates.iter().all(|c| c.side == Side::Sell),
        "rising: faded"
    );
}

/// Regression (review: a DST-gap anchor after the entry): anchor 02:30,
/// entry 03:00, exit 04:00 New York on 15m bars. On 2026-03-08 (spring
/// forward) 02:30 does not exist and reads as 07:30Z — after the 07:00Z
/// entry. That day is skipped with a note; the days around it decide,
/// each on an anchor before its entry and recorded in `data_asof_ms`.
#[test]
fn a_daily_anchor_in_the_spring_forward_gap_never_follows_the_entry() {
    let t0 = utc("2026-03-06 00:00");
    let closes: Vec<f64> = (0..(4 * 24 * 4))
        .map(|i| 100.0 * (1.0 + 0.002 * ((i as f64) / 3.0).sin()))
        .collect();
    let md = market(vec![series(A, Interval::M15, t0, &closes)]);
    let s = spec(
        json!({"kind": "daily_window", "universe": [A], "interval": "15m", "days": "all",
            "tz": "America/New_York", "anchor": "02:30", "entry": "03:00", "exit": "04:00",
            "direction": "fade"}),
    );
    let p = run_params(utc("2026-03-07 00:00"), utc("2026-03-10 00:00"));
    let set = candidates(&s, &md, &p).unwrap();
    let days: Vec<(String, i64)> = set
        .candidates
        .iter()
        .map(|c| (c.period.clone(), c.decided_at_ms))
        .collect();
    assert_eq!(
        days,
        vec![
            ("2026-03-07".to_string(), utc("2026-03-07 08:00")),
            ("2026-03-09".to_string(), utc("2026-03-09 07:00")),
        ],
        "{:?}",
        set.notes
    );
    assert!(
        set.notes
            .iter()
            .any(|n| n.contains("2026-03-08") && n.contains("out of order")),
        "{:?}",
        set.notes
    );
    for c in &set.candidates {
        // The anchor's close is 30 min before the entry, on every day.
        let anchor_at = c.decided_at_ms - 30 * 60_000;
        assert_eq!(c.anchor_px, md.bars[A].close_at(anchor_at), "{}", c.period);
        assert!(c.data_asof_ms <= c.decided_at_ms);
    }
}

#[test]
fn move_trigger_take_profit_hold_stop_loss_and_cooldown() {
    let t0 = utc("2026-09-28 00:00");
    let closes = [
        100.0, 100.0, 100.0, 100.0, 103.0, 104.0, 106.5, 101.0, 100.0, 100.0, 100.0, 100.0, 96.0,
        98.0, 98.0, 98.0,
    ];
    let md = market(vec![series(A, Interval::H1, t0, &closes)]);
    let v = json!({"kind": "move_trigger", "universe": [A], "interval": "1h", "lookback_bars": 1,
            "threshold_bps": 200, "direction": "follow", "hold_bars": 3, "take_profit_bps": 250, "stop_loss_bps": 150});
    let s = spec(v.clone());
    let p = run_params(t0, t0 + 20 * H);
    let set = candidates(&s, &md, &p).unwrap();
    let got: Vec<(i64, Side)> = set
        .candidates
        .iter()
        .map(|c| (c.decided_at_ms, c.side))
        .collect();
    // Bars 4 (+295 bps), 7 (−530, after the 3-bar cooldown), 12 (−408).
    assert_eq!(
        got,
        vec![
            (t0 + 5 * H, Side::Buy),
            (t0 + 8 * H, Side::Sell),
            (t0 + 13 * H, Side::Sell)
        ]
    );
    assert_eq!(set.candidates[0].anchor_px, Some(100.0));
    let r = simulate(&s, &md, &p, &set.candidates, Arm::Research);
    let exits: Vec<(i64, ExitReason)> = r
        .trades
        .iter()
        .map(|t| (t.exit_ms, t.exit_reason))
        .collect();
    assert_eq!(
        exits,
        vec![
            (t0 + 7 * H, ExitReason::TakeProfit),
            (t0 + 11 * H, ExitReason::Hold),
            (t0 + 14 * H, ExitReason::StopLoss)
        ]
    );
    assert!((r.trades[0].gross_bps - ret(106.5 / 103.0)).abs() < 1e-9);
    assert!((r.trades[1].gross_bps + ret(100.0 / 101.0)).abs() < 1e-9);
    assert!((r.trades[2].gross_bps + ret(98.0 / 96.0)).abs() < 1e-9);
    assert_eq!(r.trades[0].period, "2026-09-28");

    // Volume: only bar 4 trades 3× its 3-bar baseline.
    let mut vol = md.clone();
    vol.bars.get_mut(A).unwrap().bars[4].v = 3.0;
    let mut v2 = v.clone();
    v2["min_volume_ratio"] = json!(2);
    v2["volume_baseline_bars"] = json!(3);
    let set = candidates(&spec(v2), &vol, &p).unwrap();
    assert_eq!(set.candidates.len(), 1);
    assert_eq!(set.candidates[0].decided_at_ms, t0 + 5 * H);
    // Cooldown 0: bar 6 triggers too (+237 bps).
    let mut v3 = v;
    v3["cooldown_bars"] = json!(0);
    let set = candidates(&spec(v3), &md, &p).unwrap();
    assert!(set.candidates.iter().any(|c| c.decided_at_ms == t0 + 7 * H));
}

#[test]
fn funding_carry_enters_the_receiving_side_and_exits_on_a_low_rate() {
    let t0 = utc("2026-09-28 00:00");
    let mut closes = vec![100.0; 30];
    closes[4] = 99.0; // close at t0 + 5 h
    let rates = [0.00001, 0.0001, 0.0001, 0.0001, 0.000001, 0.0001, 0.0001];
    let mut md = market(vec![series(A, Interval::H1, t0, &closes)]);
    let rows: Vec<(i64, f64)> = rates
        .iter()
        .enumerate()
        .map(|(k, r)| (t0 + k as i64 * H + 37, *r))
        .collect();
    md.funding.insert(A.into(), funding(A, &rows));
    let s = spec(
        json!({"kind": "funding_carry", "universe": [A], "interval": "1h", "min_apr_pct": 50,
            "exit_apr_pct": 10, "hold_hours": 24, "costs": {"taker_fee_bps": 1, "funding": true}}),
    );
    let p = run_params(t0, t0 + 24 * H);
    let set = candidates(&s, &md, &p).unwrap();
    // Row 1 (stamped 01:00:00.037) ⇒ the 02:00 close; rows 2–3 held;
    // row 4 is under the exit APR; row 5 re-enters at 06:00.
    let got: Vec<(i64, Side)> = set
        .candidates
        .iter()
        .map(|c| (c.decided_at_ms, c.side))
        .collect();
    assert_eq!(
        got,
        vec![(t0 + 2 * H, Side::Sell), (t0 + 6 * H, Side::Sell)]
    );
    let c = &set.candidates[0];
    assert!((c.signal_bps - 0.0001 * 8760.0 * 10_000.0).abs() < 1e-6);
    assert_eq!(c.data_asof_ms, t0 + 2 * H);
    assert!((c.features["funding_apr_pct"] - 87.6).abs() < 1e-9);
    let r = simulate(&s, &md, &p, &set.candidates, Arm::Research);
    let t = &r.trades[0];
    assert_eq!(
        (t.exit_ms, t.exit_reason),
        (t0 + 5 * H, ExitReason::FundingBelowExit)
    );
    assert!(
        (t.gross_bps + ret(99.0 / 100.0)).abs() < 1e-9,
        "short from 100 to 99"
    );
    // Settlements at 03, 04, 05 h: the short receives.
    assert!(
        (t.funding_bps - (1.0 + 0.01 + 1.0)).abs() < 1e-9,
        "{}",
        t.funding_bps
    );
    assert!(t.funding_complete);
    assert!((t.net_bps - (t.gross_bps - 2.0 + t.funding_bps)).abs() < 1e-9);
    // The second runs out of rows after 06:00: funding incomplete.
    assert!(!r.trades[1].funding_complete);
    assert_eq!(r.summary.funding_incomplete, 1);
}

#[test]
fn pair_spread_by_hand() {
    let t0 = utc("2026-09-28 00:00");
    let d = 0.01f64;
    // Flat at d after the jump, past the 10-bar max hold: the research
    // arm censors a plan the data cannot finish.
    let mut spreads = vec![d, -d, d, -d, 5.0 * d, d, d];
    spreads.resize(16, d);
    let a: Vec<f64> = spreads.iter().map(|s| 100.0 * s.exp()).collect();
    let md = market(vec![
        series(A, Interval::H1, t0, &a),
        series(B, Interval::H1, t0, &[100.0; 16]),
    ]);
    let s = spec(
        json!({"kind": "pair_spread", "interval": "1h", "legs": [A, B], "lookback_bars": 4,
            "entry_z": 2, "exit_z": 0.5, "max_hold_bars": 10,
            "costs": {"taker_fee_bps": 1, "funding": false}}),
    );
    let p = run_params(t0, t0 + 10 * H);
    let set = candidates(&s, &md, &p).unwrap();
    assert_eq!(set.candidates.len(), 1);
    let c = &set.candidates[0];
    assert_eq!(c.instrument, format!("{A}/{B}"));
    assert_eq!(
        (c.side, c.legs[0].side, c.legs[1].side),
        (Side::Sell, Side::Sell, Side::Buy)
    );
    assert!((c.signal_bps - 500.0).abs() < 1e-6, "5d over a mean of 0");
    let r = simulate(&s, &md, &p, &set.candidates, Arm::Research);
    let t = &r.trades[0];
    assert_eq!((t.exit_ms, t.exit_reason), (t0 + 6 * H, ExitReason::ExitZ));
    assert_eq!(t.legs[0].notional_usd, 50.0);
    // Short a from 100·e^{5d} to 100·e^{d}: 1 − e^{−4d} = +392.1 bps of
    // its $50 (the log spread moved 400); b flat.
    let short_a = (1.0 - (-4.0 * d).exp()) * 10_000.0;
    assert!(
        (t.legs[0].gross_bps - short_a).abs() < 1e-6,
        "{}",
        t.legs[0].gross_bps
    );
    assert!(t.legs[1].gross_bps.abs() < 1e-9);
    assert!((t.gross_bps - short_a / 2.0).abs() < 1e-6);
    assert!(
        (t.net_bps - (short_a / 2.0 - 2.0)).abs() < 1e-6,
        "each leg pays 2 bps"
    );
    assert!((t.net_usd - (short_a / 2.0 - 2.0) / 100.0).abs() < 1e-6);
    // The data cut after the 7th close: the z exit (06:00) is in it, the
    // 10-bar max hold is not. The research arm drops the trade —
    // keeping only the plans that happened to exit early would pick by
    // outcome; the capped ledger books what happened.
    let mut short = md.clone();
    for series in short.bars.values_mut() {
        series.bars.truncate(7);
    }
    let set = candidates(&s, &short, &p).unwrap();
    let research = simulate(&s, &short, &p, &set.candidates, Arm::Research);
    assert!(research.trades.is_empty());
    assert_eq!(research.skipped[0].reason, SkipReason::MissingExit);
    let caps = crate::domain::backtest::engine::RiskCaps {
        initial_cash_usd: 100.0,
        max_order_notional_usd: 100.0,
        max_gross_exposure_usd: 100.0,
        max_net_exposure_usd: 100.0,
        daily_loss_limit_usd: 10.0,
        total_loss_limit_usd: 25.0,
    };
    let capped = simulate(&s, &short, &p, &set.candidates, Arm::Capped(caps));
    assert_eq!(capped.trades.len(), 1);
    assert_eq!(capped.trades[0].exit_reason, ExitReason::ExitZ);
}

#[test]
fn event_window_by_hand() {
    let day = |h: &str| utc(&format!("2026-09-29 {h}"));
    let bars = sparse(
        A,
        Interval::H1,
        &[
            (day("12:00"), 100.0),
            (day("13:00"), 101.0),
            (day("15:00"), 102.0),
        ],
    );
    let md = market(vec![bars, sparse(C, Interval::H1, &[(day("12:00"), 5.0)])]);
    let base = json!({"kind": "event_window", "interval": "1h", "direction": "follow",
            "events": [
                {"instrument": A, "t": "2026-09-29T13:30:00Z", "label": "cpi"},
                {"instrument": B, "t": "2026-09-29T13:10:00Z"},
                {"instrument": C, "t": "2026-09-29T13:20:00Z"},
                {"instrument": D, "t": "2026-09-30T13:20:00Z"}
            ],
            "entry_delay_mins": 30, "exit_after_mins": 120, "exclude": [D]});
    let p = run_params(utc("2026-09-29 00:00"), utc("2026-09-30 00:00"));
    let set = candidates(&spec(base.clone()), &md, &p).unwrap();
    assert_eq!(set.candidates.len(), 1);
    let c = &set.candidates[0];
    // 13:30 + 30 min ⇒ the 14:00 close; the anchor = the 13:00 close.
    assert_eq!(
        (c.decided_at_ms, c.anchor_px, c.side),
        (day("14:00"), Some(100.0), Side::Buy)
    );
    assert_eq!(
        c.exit,
        ExitPlan::At {
            exit_ms: day("16:00")
        }
    );
    assert_eq!(c.label.as_deref(), Some("cpi"));
    let skips: Vec<(&str, SkipReason)> = set
        .skipped
        .iter()
        .map(|k| (k.instrument.as_str(), k.reason))
        .collect();
    assert_eq!(
        skips,
        vec![
            (B, SkipReason::MissingAnchor),
            (C, SkipReason::MissingEntry)
        ],
        "D is out of range"
    );
    let r = simulate(&spec(base.clone()), &md, &p, &set.candidates, Arm::Research);
    assert!((r.trades[0].gross_bps - ret(102.0 / 101.0)).abs() < 1e-9);
    assert_eq!(r.trades[0].label.as_deref(), Some("cpi"));
    // exit_at 12:00 New York (EDT) after 10:00 EDT = 16:00 UTC too.
    let mut at = base.clone();
    at.as_object_mut().unwrap().remove("exit_after_mins");
    at["exit_at"] = json!("12:00");
    at["tz"] = json!("America/New_York");
    let set = candidates(&spec(at), &md, &p).unwrap();
    assert_eq!(
        set.candidates[0].exit,
        ExitPlan::At {
            exit_ms: day("16:00")
        }
    );
    // A move under the minimum.
    let mut min = base;
    min["min_abs_move_bps"] = json!(150);
    let set = candidates(&spec(min), &md, &p).unwrap();
    assert!(set.candidates.is_empty());
    assert_eq!(set.skipped[0].reason, SkipReason::BelowMinSignal);
}

/// `weekend_case` + Phase 7 data: an 8-K of A at Fri 16:30 New York
/// (after the close, inside the default window), SEC coverage of A and
/// B over the year, both listed in January.
fn labelled_case(labels: serde_json::Value) -> (StrategySpec, MarketData, RunParams) {
    let (s, mut md, p) = weekend_case(json!({"labels": labels, "top_n": 1}));
    let cover = |id: &str| EventCoverage {
        instrument: id.into(),
        source: "sec".into(),
        from_ms: utc("2026-01-01 00:00"),
        to_ms: utc("2026-10-08 00:00"),
        covered: true,
        note: None,
        fetched_at_ms: utc("2026-10-08 00:00"),
    };
    md.info.events.insert(
        A.into(),
        vec![MarketEvent {
            instrument: A.into(),
            published_ms: et("2026-09-25 16:30"),
            kind: "filing".into(),
            id: "0001318605-26-000123".into(),
            form: "8-K".into(),
            title: Some("Item 2.02 Results of Operations".into()),
        }],
    );
    for id in [A, B] {
        md.info.coverage.insert(id.into(), vec![cover(id)]);
        md.info.listed_ms.insert(id.into(), utc("2026-01-02 00:00"));
    }
    (s, md, p)
}

/// Labels come before `top_n`: label only, A (NEWS, the larger move)
/// trades and B (NOISE) is `not_top_n`; skipping NEWS skips A
/// (`label_skipped`, NEWS in the skip) and B moves up; an unlabelled
/// spec carries no `info_label` and writes none.
#[test]
fn labels_skip_before_top_n_and_every_candidate_carries_one() {
    let classes = |set: &CandidateSet| -> Vec<(String, Option<InfoLabel>)> {
        set.candidates
            .iter()
            .map(|c| (c.instrument.clone(), c.info_label))
            .collect()
    };
    let reasons = |set: &CandidateSet| -> Vec<(String, SkipReason, Option<InfoLabel>)> {
        set.skipped
            .iter()
            .map(|k| (k.instrument.clone(), k.reason, k.info_label))
            .collect()
    };
    let (s, md, p) = labelled_case(json!({}));
    let set = candidates(&s, &md, &p).unwrap();
    assert_eq!(classes(&set), vec![(A.into(), Some(InfoLabel::News))]);
    assert_eq!(
        reasons(&set),
        vec![
            (B.into(), SkipReason::NotTopN, Some(InfoLabel::Noise)),
            (C.into(), SkipReason::Excluded, None),
            (D.into(), SkipReason::MissingAnchor, None),
        ]
    );
    assert!(
        set.notes.contains(&String::from(
            "labels: NEWS 1 · UNCERTAIN 0 · NOISE 1 (window anchor − 240 min → decision; \
                 new listing < 14 d; every filing) — nothing skipped"
        )),
        "{:?}",
        set.notes
    );
    let line = serde_json::to_value(&set.candidates[0]).unwrap();
    assert_eq!(line["info_label"], json!("NEWS"));

    let (s, md, p) = labelled_case(json!({"skip": ["NEWS"]}));
    let set = candidates(&s, &md, &p).unwrap();
    assert_eq!(classes(&set), vec![(B.into(), Some(InfoLabel::Noise))]);
    assert_eq!(
        reasons(&set)[0],
        (A.into(), SkipReason::LabelSkipped, Some(InfoLabel::News))
    );
    assert_eq!(set.skip_counts()["label_skipped:NEWS"], 1);
    assert!(set
        .notes
        .iter()
        .any(|n| n.ends_with("label_skipped: NEWS 1")));
    // The skipped name never reaches the arms.
    let r = simulate(&s, &md, &p, &set.candidates, Arm::Research);
    assert_eq!(r.trades.len(), 1);
    assert_eq!(r.trades[0].instrument, B);

    // B without coverage: UNCERTAIN; skipping it too leaves nothing.
    let (s, mut md, p) = labelled_case(json!({"skip": ["NEWS", "UNCERTAIN"]}));
    md.info.coverage.remove(B);
    let set = candidates(&s, &md, &p).unwrap();
    assert!(set.candidates.is_empty());
    assert_eq!(
        reasons(&set)[..2],
        [
            (A.into(), SkipReason::LabelSkipped, Some(InfoLabel::News)),
            (
                B.into(),
                SkipReason::LabelSkipped,
                Some(InfoLabel::Uncertain)
            ),
        ]
    );

    // The window opens at the anchor − lookback, the bound in: 210 min
    // before Fri 20:00 is the 8-K's 16:30 (NEWS), 209 a minute after it.
    let (s, md, p) = labelled_case(json!({"lookback_mins": 210}));
    let set = candidates(&s, &md, &p).unwrap();
    assert_eq!(classes(&set), vec![(A.into(), Some(InfoLabel::News))]);
    let (s, md, p) = labelled_case(json!({"lookback_mins": 209}));
    let set = candidates(&s, &md, &p).unwrap();
    assert_eq!(classes(&set), vec![(A.into(), Some(InfoLabel::Noise))]);

    // Unlabelled: no label anywhere, none written; an old line parses.
    let (s, md, p) = weekend_case(json!({"top_n": 1}));
    let set = candidates(&s, &md, &p).unwrap();
    assert!(set.candidates.iter().all(|c| c.info_label.is_none()));
    assert!(set.skipped.iter().all(|k| k.info_label.is_none()));
    assert!(!set.notes.iter().any(|n| n.starts_with("labels:")));
    let line = serde_json::to_value(&set.candidates[0]).unwrap();
    assert!(line.get("info_label").is_none());
    let back: Candidate = serde_json::from_value(line).unwrap();
    assert_eq!(back, set.candidates[0]);
}
