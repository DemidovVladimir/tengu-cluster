//! Cross-kind checks of the backtest engine (compiled for tests only).
//!
//! | Check | Holds |
//! |---|---|
//! | Worlds | deterministic random markets (`testkit`): 1h over 40 days (every kind, `thin_entry` on two); the 1h market re-cut to 4h and a 200-day market re-cut to 1d (the bar kinds — windows are ≤ 1h); 15m across both 2026 spring-forward switches (New York 03-08, Paris 03-29: daily windows with an anchor in the gap, the weekend window); 1h with a configured 2-for-1 split on the hour; 1d with a 3-for-1 split at 08:00 UTC — inside a day bar |
//! | Moves after t | the bar opening at t × 1.5; every later bar × 1.5 (volume × 4), funding × −3, ctx asks × 1.5; the bar opening at t deleted; everything after t cut (bars closing after t, ctx rows after t, funding rows of hours after t — hour t's row settles an exit at t) — applied to the venue's raw series, splits after |
//! | Time integrity — decisions (`docs/xlab-2026-10-01.md` § 39) | under every move, the candidates and skips decided at or before t are unchanged |
//! | Time integrity — arms | under every move, per arm: what became of each candidate decided at or before t (admitted — traded or held as `missing_exit` —, refused by rule, dropped at the decision) is unchanged, and so are the trades closed at or before t (the research arm under the cut: those whose exit plan ends inside the cut data — a later horizon is censored) |
//! | As-of | `data_asof_ms ≤ decided_at_ms` for every candidate and trade, every world and kind, both arms; entries fill at the decision's close |
//! | Splits | the split-adjusted series keeps no bar that opens before a split and closes after it; no candidate shows the split as a move |
//! | Rule W golden (`assert_rule_w_golden`, also the W1 generation's replay check) | `weekend_window` on the 2026-09-26 golden candles (`weekend_fade::golden`, 5 m) with half the round-trip cost per side, no spread, slippage or funding = `weekend_fade::replay`: the same 74 names, prices, signals, sides and instants exactly; gross = side × (exit / entry − 1) exactly and = the replay's log gross through dir × (e^{dir × g / 10⁴} − 1) × 10⁴; net = gross − the round trip; the mean of the converted rows (+93.90 bps; the replay's log mean +95.46); the same 53 positive; `top_n` 4 / 50 bps = the replay's capped four |

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value};

use crate::domain::backtest::costs::{CostSpec, HalfSpread};
use crate::domain::backtest::engine::{
    candidates, simulate, Arm, ArmResult, Candidate, CandidateSet, ExitPlan, MarketData, RiskCaps,
    RunParams, Skip, SkipReason, Trade,
};
use crate::domain::backtest::spec::StrategySpec;
use crate::domain::backtest::testkit::{
    aggregate, et, nyse, random_intraday, random_market, run_params, utc, H,
};
use crate::domain::marketdata::{fmt_time, Bar, BarSeries, Interval, StockSplit};
use crate::domain::xm::weekend_fade::{fade_window, golden, replay, FadeRule, Replay};

const IDS: [&str; 3] = [
    "hyperliquid:xyz:AAA",
    "hyperliquid:xyz:BBB",
    "hyperliquid:xyz:CCC",
];
const DAY: i64 = 24 * H;

/// One world of the checks (module table).
struct World {
    name: &'static str,
    /// The venue's series (pre-split prices where a split is configured).
    raw: MarketData,
    splits: BTreeMap<String, Vec<StockSplit>>,
    p: RunParams,
    specs: Vec<StrategySpec>,
    /// Decision instants checked per spec (evenly spread).
    samples: usize,
}

impl World {
    /// What a run reads: `raw` split-adjusted.
    fn md(&self, raw: &MarketData) -> MarketData {
        let mut md = raw.clone();
        md.adjust_for_splits(&self.splits);
        md
    }
}

fn costs() -> BTreeMap<String, CostSpec> {
    let cost = |half_spread| CostSpec {
        taker_fee_bps: 1.5,
        half_spread,
        slippage_bps: 0.5,
        funding: true,
    };
    BTreeMap::from([
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
    ])
}

fn params(from: i64, to: i64) -> RunParams {
    let mut p = run_params(from, to);
    p.universe = IDS.iter().map(|s| s.to_string()).collect();
    p.costs = costs();
    p
}

fn specs(values: Vec<Value>) -> Vec<StrategySpec> {
    values
        .into_iter()
        .map(|v| StrategySpec::from_value("k", &v).unwrap_or_else(|e| panic!("{v}: {e:?}")))
        .collect()
}

/// Twelve events from `from`, `step` apart, off the bar grid.
fn events(from: i64, step: i64) -> Vec<Value> {
    (0..12)
        .map(|i| {
            let t = from + i * step + 17 * 60_000 + 13_000;
            json!({"instrument": IDS[i as usize % 3], "t": fmt_time(t), "label": format!("e{i}")})
        })
        .collect()
}

/// Five-plus weeks of hourly bars from Mon 2026-08-31; decisions from day 9
/// (features have their 8 days) to day 36; one spec of every kind.
fn hourly() -> World {
    let t0 = utc("2026-08-31 00:00");
    let p = params(t0 + 9 * DAY, t0 + 36 * DAY);
    World {
        name: "1h",
        raw: random_market(&IDS, t0, 24 * 40, 11),
        splits: BTreeMap::new(),
        specs: every_kind(&p),
        p,
        samples: 10,
    }
}

/// One spec of every kind, tuned to decide on the 1h world.
fn every_kind(p: &RunParams) -> Vec<StrategySpec> {
    specs(vec![
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
        json!({"kind": "event_window", "interval": "1h", "events": events(p.from_ms, 49 * H),
               "entry_delay_mins": 30, "direction": "follow", "exit_after_mins": 180}),
    ])
}

/// The 1h market re-cut to 4h: the bar kinds (windows are ≤ 1h).
fn four_hour() -> World {
    let t0 = utc("2026-08-31 00:00");
    let p = params(t0 + 9 * DAY, t0 + 36 * DAY);
    World {
        name: "4h",
        raw: aggregate(&random_market(&IDS, t0, 24 * 40, 11), Interval::H4),
        splits: BTreeMap::new(),
        specs: specs(vec![
            json!({"kind": "move_trigger", "universe": IDS, "interval": "4h", "lookback_bars": 2,
                   "threshold_bps": 150, "min_volume_ratio": 0.5, "volume_baseline_bars": 12,
                   "direction": "fade", "hold_bars": 3, "take_profit_bps": 150,
                   "stop_loss_bps": 150, "min_entry_trades": 3}),
            json!({"kind": "funding_carry", "universe": IDS, "interval": "4h", "min_apr_pct": 60,
                   "exit_apr_pct": 20, "hold_hours": 48}),
            json!({"kind": "pair_spread", "interval": "4h", "legs": [IDS[0], IDS[1]],
                   "lookback_bars": 12, "entry_z": 1.5, "exit_z": 0.3, "max_hold_bars": 12}),
            json!({"kind": "event_window", "interval": "4h", "events": events(p.from_ms, 49 * H),
                   "entry_delay_mins": 30, "direction": "follow", "exit_after_mins": 600}),
        ]),
        p,
        samples: 10,
    }
}

/// The bar kinds on a 200-day market re-cut to 1d.
fn daily_bars(
    raw_hourly: &MarketData,
    name: &'static str,
    splits: BTreeMap<String, Vec<StockSplit>>,
) -> World {
    let t0 = utc("2026-03-02 00:00");
    let p = params(t0 + 10 * DAY, t0 + 190 * DAY);
    World {
        name,
        raw: aggregate(raw_hourly, Interval::D1),
        splits,
        specs: specs(vec![
            json!({"kind": "move_trigger", "universe": IDS, "interval": "1d", "lookback_bars": 1,
                   "threshold_bps": 150, "direction": "fade", "hold_bars": 2,
                   "take_profit_bps": 200, "stop_loss_bps": 200}),
            json!({"kind": "funding_carry", "universe": IDS, "interval": "1d", "min_apr_pct": 60,
                   "exit_apr_pct": 20, "hold_hours": 72}),
            json!({"kind": "pair_spread", "interval": "1d", "legs": [IDS[0], IDS[1]],
                   "lookback_bars": 10, "entry_z": 1.2, "exit_z": 0.3, "max_hold_bars": 5}),
            json!({"kind": "event_window", "interval": "1d", "events": events(p.from_ms, 15 * DAY),
                   "entry_delay_mins": 30, "direction": "fade", "exit_after_mins": 2880}),
        ]),
        p,
        samples: 10,
    }
}

fn daily() -> World {
    daily_bars(
        &random_market(&IDS, utc("2026-03-02 00:00"), 24 * 200, 5),
        "1d",
        BTreeMap::new(),
    )
}

/// 15m from 2026-02-26 to 04-05: both spring-forward switches inside the
/// decision range. New York 02:30 does not exist on 03-08, Paris 02:30 not
/// on 03-29 — each a daily window's anchor.
fn dst() -> World {
    let t0 = utc("2026-02-26 00:00");
    let p = params(utc("2026-03-04 00:00"), utc("2026-04-02 00:00"));
    World {
        name: "15m-dst",
        raw: random_intraday(&IDS, t0, 38 * 96, Interval::M15, 3),
        splits: BTreeMap::new(),
        specs: specs(vec![
            json!({"kind": "daily_window", "universe": IDS, "interval": "15m", "days": "all",
                   "tz": "America/New_York", "anchor": "02:30", "entry": "03:00", "exit": "04:00",
                   "direction": "fade"}),
            json!({"kind": "daily_window", "universe": IDS, "interval": "15m", "days": "all",
                   "tz": "Europe/Paris", "anchor": "02:30", "entry": "03:00", "exit": "05:00",
                   "direction": "follow"}),
            json!({"kind": "daily_window", "universe": IDS, "interval": "15m", "days": "trading",
                   "calendar": "us_equity", "tz": "America/New_York", "anchor": "16:00",
                   "entry": "20:00", "exit": "09:30", "direction": "fade", "top_n": 2}),
            json!({"kind": "weekend_window", "universe": IDS, "interval": "15m",
                   "calendar": "us_equity", "direction": "fade"}),
        ]),
        p,
        // Every instant: the gap days must be among them.
        samples: usize::MAX,
    }
}

/// The venue's view of a `ratio`-for-1 split of `id` at `at`: bars opening
/// before it priced × ratio, volume ÷ ratio (hourly, so a day bar re-cut
/// from them straddles a split inside it).
fn presplit(md: &MarketData, id: &str, at: i64, ratio: f64) -> MarketData {
    let mut raw = md.clone();
    for b in raw.bars.get_mut(id).unwrap().bars.iter_mut() {
        if b.t_open_ms < at {
            (b.o, b.h, b.l, b.c, b.v) = (
                b.o * ratio,
                b.h * ratio,
                b.l * ratio,
                b.c * ratio,
                b.v / ratio,
            );
        }
    }
    raw
}

fn split_of(id: &str, at: i64, ratio: f64) -> BTreeMap<String, Vec<StockSplit>> {
    BTreeMap::from([(id.to_string(), vec![StockSplit { at_ms: at, ratio }])])
}

/// The 1h world with BBB split 2-for-1 at 13:00 UTC on day 20.
fn split_hourly() -> World {
    let mut w = hourly();
    let at = utc("2026-08-31 13:00") + 20 * DAY;
    w.name = "1h-split";
    w.raw = presplit(&w.raw, IDS[1], at, 2.0);
    w.splits = split_of(IDS[1], at, 2.0);
    w
}

/// The 1d world with AAA split 3-for-1 at 08:00 UTC on day 100 — inside
/// that day's bar (KIOXIA, 2026-09-28).
fn split_daily() -> World {
    let at = utc("2026-03-02 08:00") + 100 * DAY;
    let hourly = random_market(&IDS, utc("2026-03-02 00:00"), 24 * 200, 5);
    daily_bars(
        &presplit(&hourly, IDS[0], at, 3.0),
        "1d-split",
        split_of(IDS[0], at, 3.0),
    )
}

fn worlds() -> Vec<World> {
    vec![
        hourly(),
        four_hour(),
        daily(),
        dst(),
        split_hourly(),
        split_daily(),
    ]
}

/// What comes after t, moved (module table).
#[derive(Debug, Clone, Copy)]
enum Move {
    NextBar,
    AllAfter,
    DropNext,
    Cut,
}

const MOVES: [Move; 4] = [Move::NextBar, Move::AllAfter, Move::DropNext, Move::Cut];

/// The hour a funding row settles (HL stamps a few ms late).
fn settles(t_ms: i64) -> i64 {
    (t_ms + H / 2).div_euclid(H) * H
}

/// Which funding rows come "after t": the ones stamped after t (what a
/// decision at t may not read), or the ones settling hours after t (an
/// exit at t books hour t's settlement, stamped a few ms after t).
#[derive(Debug, Clone, Copy)]
enum Funding {
    Stamped,
    Settled,
}

impl Funding {
    fn after(self, t_ms: i64, t: i64) -> bool {
        match self {
            Funding::Stamped => t_ms > t,
            Funding::Settled => settles(t_ms) > t,
        }
    }
}

/// `raw` with what comes after `t` moved (module table).
fn moved(raw: &MarketData, t: i64, how: Move, funding: Funding) -> MarketData {
    let mut out = raw.clone();
    for s in out.bars.values_mut() {
        let iv = s.interval.ms();
        match how {
            Move::NextBar | Move::AllAfter => {
                let hit = |b: &Bar| match how {
                    Move::NextBar => b.t_open_ms == t,
                    _ => b.t_open_ms >= t,
                };
                for b in s.bars.iter_mut().filter(|b| hit(b)) {
                    (b.o, b.h, b.l, b.c) = (b.o * 1.5, b.h * 1.5, b.l * 1.5, b.c * 1.5);
                    b.v *= 4.0;
                }
            }
            Move::DropNext => s.bars.retain(|b| b.t_open_ms != t),
            Move::Cut => s.bars.retain(|b| b.t_open_ms + iv <= t),
        }
    }
    match how {
        Move::AllAfter => {
            for f in out.funding.values_mut() {
                for p in f.points.iter_mut().filter(|p| funding.after(p.t_ms, t)) {
                    p.rate_1h *= -3.0;
                }
            }
            for c in out.ctx.values_mut() {
                for p in c.points.iter_mut().filter(|p| p.t_ms > t) {
                    p.impact_ask = p.impact_ask.map(|x| x * 1.5);
                }
            }
        }
        Move::Cut => {
            for f in out.funding.values_mut() {
                f.points.retain(|p| !funding.after(p.t_ms, t));
            }
            for c in out.ctx.values_mut() {
                c.points.retain(|p| p.t_ms <= t);
            }
        }
        Move::NextBar | Move::DropNext => {}
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

/// The decision instants checked for `set` (evenly spread, `n` at most).
fn instants(set: &CandidateSet, n: usize) -> Vec<i64> {
    let all: BTreeSet<i64> = set.candidates.iter().map(|c| c.decided_at_ms).collect();
    let step = (all.len() / n.max(1)).max(1);
    all.into_iter().step_by(step).collect()
}

/// § 39 (a): what a decision saw never moves when the future does.
#[test]
fn decisions_at_or_before_t_ignore_what_comes_after() {
    let mut thin = 0;
    for w in worlds() {
        let md = w.md(&w.raw);
        for s in &w.specs {
            let kind = s.kind_name();
            let base = candidates(s, &md, &w.p).unwrap();
            assert!(
                !base.candidates.is_empty(),
                "{} {kind} decided nothing",
                w.name
            );
            thin += base
                .skipped
                .iter()
                .filter(|k| k.reason == SkipReason::ThinEntry)
                .count();
            let mut future_read = false;
            for t in instants(&base, w.samples) {
                for how in MOVES {
                    let raw = moved(&w.raw, t, how, Funding::Stamped);
                    let again = candidates(s, &w.md(&raw), &w.p).unwrap();
                    assert_eq!(
                        upto(&again, t),
                        upto(&base, t),
                        "{} {kind} at {} ({how:?})",
                        w.name,
                        fmt_time(t)
                    );
                    future_read |= again != base;
                }
            }
            assert!(
                future_read,
                "{} {kind}: moving the future changed no later decision — the check proves nothing",
                w.name
            );
        }
    }
    assert!(thin > 0, "no thin_entry skip: the filter went unchecked");
}

/// Caps that bind: two $25 positions, net one way, a $3 daily stop.
fn caps() -> RiskCaps {
    RiskCaps {
        initial_cash_usd: 100.0,
        max_order_notional_usd: 25.0,
        max_gross_exposure_usd: 50.0,
        max_net_exposure_usd: 25.0,
        daily_loss_limit_usd: 3.0,
        total_loss_limit_usd: 1_000.0,
    }
}

/// What an arm did with each candidate decided at or before `t`, by
/// (instrument key, instant): admitted (traded, or held as `missing_exit`),
/// refused (rule) or dropped at the decision (reason).
fn admissions(r: &ArmResult, t: i64) -> BTreeMap<(String, i64), String> {
    let mut m = BTreeMap::new();
    for x in r.trades.iter().filter(|x| x.decided_at_ms <= t) {
        m.insert(
            (x.instrument.clone(), x.decided_at_ms),
            "admitted".to_string(),
        );
    }
    for k in r.skipped.iter().filter(|k| k.decided_at_ms <= t) {
        let what = match k.reason {
            SkipReason::MissingExit => "admitted".to_string(),
            other => format!("dropped:{}", other.as_str()),
        };
        m.insert((k.instrument.clone(), k.decided_at_ms), what);
    }
    for x in r.refusals.iter().filter(|x| x.decided_at_ms <= t) {
        m.insert(
            (x.instrument.clone(), x.decided_at_ms),
            format!("refused:{}", x.rule),
        );
    }
    m
}

/// The latest instant a candidate's exit plan can reach.
fn horizon(c: &Candidate) -> i64 {
    match c.exit {
        ExitPlan::At { exit_ms } => exit_ms,
        ExitPlan::Bars { max_exit_ms, .. }
        | ExitPlan::Funding { max_exit_ms, .. }
        | ExitPlan::Spread { max_exit_ms, .. } => max_exit_ms,
    }
}

/// The close of `id`'s last bar in `md`.
fn data_end(md: &MarketData, id: &str) -> i64 {
    md.bars[id]
        .bars
        .last()
        .map_or(i64::MIN, |b| b.t_close_ms(md.bars[id].interval))
}

/// § 39 (b): each arm's admissions at or before t, and its trades closed
/// by t, never depend on what comes after t.
#[test]
fn arms_at_or_before_t_ignore_what_comes_after() {
    for w in worlds() {
        let md = w.md(&w.raw);
        let (mut refused, mut held) = (0, 0);
        for s in &w.specs {
            let kind = s.kind_name();
            let base = candidates(s, &md, &w.p).unwrap();
            let arms = |md: &MarketData, set: &CandidateSet| {
                [Arm::Research, Arm::Capped(caps())]
                    .map(|arm| simulate(s, md, &w.p, &set.candidates, arm))
            };
            let full = arms(&md, &base);
            refused += full[1].refusals.len();
            for t in instants(&base, w.samples.min(8)) {
                for how in MOVES {
                    let md2 = w.md(&moved(&w.raw, t, how, Funding::Settled));
                    let set = candidates(s, &md2, &w.p).unwrap();
                    let again = arms(&md2, &set);
                    for (a, b) in full.iter().zip(&again) {
                        let at =
                            format!("{} {kind} {} at {} ({how:?})", w.name, a.arm, fmt_time(t));
                        assert_eq!(admissions(b, t), admissions(a, t), "{at}");
                        let closed = |r: &ArmResult| -> Vec<Trade> {
                            r.trades
                                .iter()
                                .filter(|x| x.exit_ms <= t)
                                .cloned()
                                .collect()
                        };
                        let want: Vec<Trade> = match (how, a.arm.as_str()) {
                            // Cut: the research arm keeps only what the cut
                            // data can finish.
                            (Move::Cut, "research") => a
                                .trades
                                .iter()
                                .filter(|x| {
                                    horizon(&base.candidates[x.seq])
                                        <= x.legs
                                            .iter()
                                            .map(|l| data_end(&md2, &l.instrument))
                                            .min()
                                            .unwrap_or(i64::MIN)
                                })
                                .cloned()
                                .collect(),
                            _ => closed(a),
                        };
                        let got = match (how, a.arm.as_str()) {
                            (Move::Cut, "research") => b.trades.clone(),
                            _ => closed(b),
                        };
                        assert_eq!(got, want, "{at}");
                        held += b
                            .skipped
                            .iter()
                            .filter(|k| k.reason == SkipReason::MissingExit)
                            .count();
                    }
                }
            }
        }
        // The check exercised what it guards: caps that refused, and
        // admitted candidates without an exit price held in the book.
        assert!(refused > 0, "{}: the caps never bound", w.name);
        assert!(held > 0, "{}: no unfilled exit was ever held", w.name);
    }
}

/// § 39 (c): `data_asof_ms ≤ decided_at_ms` for every candidate and trade;
/// entries fill at the decision's close.
#[test]
fn every_kind_reads_nothing_after_its_decision() {
    for w in worlds() {
        let md = w.md(&w.raw);
        for s in &w.specs {
            let kind = s.kind_name();
            let set = candidates(s, &md, &w.p).unwrap();
            assert!(
                !set.candidates.is_empty(),
                "{} {kind} decided nothing",
                w.name
            );
            for c in &set.candidates {
                assert!(
                    c.data_asof_ms <= c.decided_at_ms,
                    "{} {kind}: {c:?}",
                    w.name
                );
                assert!(
                    (w.p.from_ms..w.p.to_ms).contains(&c.decided_at_ms),
                    "{} {kind}: {c:?}",
                    w.name
                );
                for leg in &c.legs {
                    assert_eq!(
                        md.bars[&leg.instrument].close_at(c.decided_at_ms),
                        Some(leg.entry_px),
                        "{} {kind}",
                        w.name
                    );
                }
            }
            for arm in [Arm::Research, Arm::Capped(caps())] {
                let name = arm.name();
                let r = simulate(s, &md, &w.p, &set.candidates, arm);
                assert_eq!(r.arm, name);
                assert!(
                    !r.trades.is_empty(),
                    "{} {kind} {name} traded nothing",
                    w.name
                );
                for t in &r.trades {
                    assert!(
                        t.data_asof_ms <= t.decided_at_ms,
                        "{} {kind}: {t:?}",
                        w.name
                    );
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
}

/// A split-adjusted series never keeps a bar that opens before its split
/// and closes after it, and no decision shows the split as a move (ln 2 /
/// ln 3 = 6 931 / 10 986 bps; the market's own day moves stay far below).
#[test]
fn a_split_never_leaves_a_mixed_bar_or_a_fake_move() {
    for w in [split_hourly(), split_daily()] {
        let md = w.md(&w.raw);
        for (id, splits) in &w.splits {
            let s = &md.bars[id];
            for split in splits {
                assert!(
                    s.bars
                        .iter()
                        .all(|b| !(b.t_open_ms < split.at_ms
                            && split.at_ms < b.t_close_ms(s.interval))),
                    "{}: a bar straddles {}",
                    w.name,
                    fmt_time(split.at_ms)
                );
            }
        }
        let mut seen = 0;
        for s in &w.specs {
            let set = candidates(s, &md, &w.p).unwrap();
            for c in set
                .candidates
                .iter()
                .filter(|c| w.splits.keys().any(|id| c.instrument.contains(id.as_str())))
            {
                seen += 1;
                if !matches!(c.exit, ExitPlan::Spread { .. } | ExitPlan::Funding { .. }) {
                    assert!(
                        c.signal_bps.abs() < 5_000.0,
                        "{} {}: {} at {}",
                        w.name,
                        s.kind_name(),
                        c.signal_bps,
                        fmt_time(c.decided_at_ms)
                    );
                }
            }
            let r = simulate(s, &md, &w.p, &set.candidates, Arm::Research);
            for t in &r.trades {
                assert!(
                    t.gross_bps.abs() < 5_000.0,
                    "{} {}: {t:?}",
                    w.name,
                    s.kind_name()
                );
            }
        }
        assert!(seen > 0, "{}: the split instrument never decided", w.name);
    }
}

/// The golden weekend's candles as 5 m bars (every price the close).
pub(crate) fn golden_market() -> MarketData {
    let bars: BTreeMap<String, BarSeries> = golden::candles()
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
    MarketData {
        bars,
        ..Default::default()
    }
}

/// The rule's golden replay: its window (from the same calendar row) and the
/// capped rule of its test.
fn golden_replay() -> Replay {
    let w = fade_window(&nyse(), et("2026-09-25 12:00")).unwrap();
    let rule = FadeRule {
        capped_top_n: 4,
        min_abs_signal_bps: 50.0,
    };
    replay(
        &golden::candles(),
        &w,
        &golden::excluded(),
        golden::cost_rt_bps(),
        &rule,
    )
}

/// Rule W through the engine on the golden weekend = the replay, bit for
/// bit where it can be (module table: Rule W golden). `s` = a
/// `weekend_window` fade over the golden universe, KIOXIA excluded, half the
/// round trip a side, no spread / slippage / funding; `p` = its run params
/// over 2026-09-25 → 09-29 with a `us_equity` calendar. Shared with the W1
/// generation's replay check (`config/lineage_tests.rs`).
pub(crate) fn assert_rule_w_golden(s: &StrategySpec, p: &RunParams) {
    let md = golden_market();
    let w = fade_window(&nyse(), et("2026-09-25 12:00")).unwrap();
    let cost_rt = golden::cost_rt_bps();
    let set = candidates(s, &md, p).unwrap();
    let r = simulate(s, &md, p, &set.candidates, Arm::Research);
    let want = golden_replay();
    golden::assert_replay(&want);
    assert_eq!(r.trades.len(), want.rows.len());
    assert_eq!(r.trades.len(), 74);
    // The replay books log returns, the engine a linear perp's simple return
    // on the same prices: gross = dir × (exit / entry − 1) exactly, and the
    // replay's log gross g converts as dir × (e^{dir × g / 10⁴} − 1) × 10⁴.
    let simple = |dir: i8, g: f64| {
        let d = f64::from(dir);
        d * ((d * g / 10_000.0).exp() - 1.0) * 10_000.0
    };
    let mut nets = Vec::new();
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
        assert_eq!(
            t.gross_bps,
            f64::from(row.dir) * (row.exit_px / row.entry_px - 1.0) * 10_000.0,
            "{id} gross, bit for bit"
        );
        let converted = simple(row.dir, row.gross_bps);
        assert!(
            (t.gross_bps - converted).abs() < 1e-9 * converted.abs().max(1.0),
            "{id}: the replay's gross, converted"
        );
        assert!((t.net_bps - (converted - cost_rt)).abs() < 1e-9, "{id} net");
        assert_eq!(t.fee_bps, cost_rt, "{id}: half the round trip a side");
        assert_eq!((t.decided_at_ms, t.exit_ms), (w.entry_ms, w.exit_ms));
        assert_eq!(t.period, "2026-09-25");
        nets.push(converted - cost_rt);
    }
    let mean = nets.iter().sum::<f64>() / nets.len() as f64;
    assert!(
        (r.summary.mean_net_bps.unwrap() - mean).abs() < 1e-9,
        "the mean of the converted rows, summed in id order"
    );
    // On this weekend's moves: +93.90 simple (what the perps paid) vs the
    // replay's +95.46 log — the fade's shorts gain ≈ r² / 2 under ln.
    let (simple_mean, log_mean) = (r.summary.mean_net_bps.unwrap(), want.mean_net_bps.unwrap());
    assert!(
        (simple_mean - 93.9026).abs() < 1e-3 && (log_mean - 95.4585).abs() < 1e-3,
        "simple {simple_mean} vs log {log_mean}"
    );
    assert_eq!(r.summary.n, 74);
    let positive = nets.iter().filter(|x| **x > 0.0).count();
    assert_eq!(positive, want.positive, "the same 53 positive");
    assert_eq!(r.summary.hit_rate, Some(positive as f64 / 74.0));
    assert_eq!(
        set.skip_counts(),
        BTreeMap::from([("excluded".to_string(), 1)]),
        "only the split-halted KIOXIA"
    );
}

/// Rule W, bit for bit where it can be (module table).
#[test]
fn golden_weekend_window_equals_the_rule_w_replay() {
    let candles = golden::candles();
    let md = golden_market();
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
    assert_rule_w_golden(&spec_with(json!({})), &p);
    let want = golden_replay();
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
        assert_eq!(
            t.gross_bps,
            f64::from(row.dir) * (row.exit_px / row.entry_px - 1.0) * 10_000.0
        );
    }
}
