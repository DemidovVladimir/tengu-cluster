//! Tests of `src/domain/xm/paper.rs`.

use super::*;
use crate::domain::book::fixture::tsla_book;
use crate::domain::book::L2Level;
use crate::domain::xm::ledger::PaperAccount;

const TSLA: &str = "hyperliquid:xyz:TSLA";
const TESLA: &str = "company:tesla";
/// Fixture mid: (347.16 + 347.23) / 2.
const MID: f64 = 347.195;

fn close(a: f64, b: f64, what: &str) {
    let tol = 1e-9 * b.abs().max(1.0);
    assert!((a - b).abs() <= tol, "{what}: got {a}, want {b}");
}

fn id() -> InstrumentId {
    InstrumentId::parse(TSLA).unwrap()
}

/// xyz:TSLA at tier 0: 4.5 × 2 (deployer scale 1) × 0.1 (growth mode)
/// = 0.9 bps taker (`xm/cost.rs` worked example).
fn fees() -> FeeSchedule {
    FeeSchedule::new(0.9, 0.3).unwrap()
}

/// xyz:TSLA: perp, szDecimals 3, not at its OI cap.
fn rules() -> VenueRules {
    VenueRules::hyperliquid(HlKind::Perp, 3, Some(false))
}

fn flat() -> Position {
    Position::flat(TSLA, TESLA, "hyperliquid")
}

fn held(qty: f64, avg_px: f64) -> Position {
    Position {
        qty,
        avg_px: Some(avg_px),
        opened_ms: Some(0),
        ..flat()
    }
}

fn order(kind: OrderKind, side: Side, size: OrderSize, limit_px: Option<f64>) -> PaperOrder {
    PaperOrder {
        client_order_id: "loop:session-1:7".to_string(),
        instrument: id(),
        side,
        size,
        kind,
        tif: Tif::Ioc,
        limit_px,
        reduce_only: false,
        max_slippage_bps: 30.0,
        ref_mid: None,
    }
}

fn market(side: Side, size: OrderSize) -> PaperOrder {
    order(OrderKind::Market, side, size, None)
}

fn limit(side: Side, qty: f64, px: f64) -> PaperOrder {
    order(OrderKind::Limit, side, OrderSize::Qty(qty), Some(px))
}

fn run_with(o: &PaperOrder, rules: &VenueRules, position: &Position) -> FillResult {
    let fees = fees();
    let env = FillEnv {
        rules,
        status: MarketStatus::Open,
        position,
        fees: &fees,
        max_book_age_ms: 5_000,
        stale_book_ok: false,
    };
    simulate_fill(o, &tsla_book(), 1_000, &env)
}

fn run(o: &PaperOrder) -> FillResult {
    run_with(o, &rules(), &flat())
}

fn rejected(r: &FillResult, reason: FillReason) {
    assert_eq!(r.status, FillStatus::Rejected, "{r:?}");
    assert_eq!(r.reason, Some(reason), "{r:?}");
    assert!(r.fills.is_empty() && r.filled_qty == 0.0 && r.fee_usd == 0.0);
    assert_eq!((r.avg_px, r.slippage_bps), (None, None), "never 0");
}

/// Hand-computed with bc outside the repo (`tests/fixtures/xm/meta.json`
/// `fills`); fee 0.9 bps.
#[test]
fn fixture_fills_match_hand_computed_goldens() {
    use FillStatus::{Filled, Partial};
    use OrderSize::{NotionalUsd, Qty};
    use Side::{Buy, Sell};
    struct V {
        order: PaperOrder,
        status: FillStatus,
        reason: Option<FillReason>,
        order_qty: f64,
        bound: f64,
        filled_qty: f64,
        notional: f64,
        avg: f64,
        slip: f64,
        fee: f64,
        levels: usize,
    }
    let tight = PaperOrder {
        max_slippage_bps: 2.0,
        ..market(Buy, Qty(100.0))
    };
    let wide = PaperOrder {
        max_slippage_bps: 100.0,
        ..market(Buy, Qty(1_000.0))
    };
    let vectors = [
        // $1 000 → 2.880225… → 2.880; bound 347.195 × 1.003 = 348.236585 → 348.23.
        V {
            order: market(Buy, NotionalUsd(1_000.0)),
            status: Filled,
            reason: None,
            order_qty: 2.88,
            bound: 348.23,
            filled_qty: 2.88,
            notional: 1_000.022_4,
            avg: 347.23,
            slip: 1.008_079_033_396_218,
            fee: 0.090_002_016,
            levels: 1,
        },
        V {
            order: market(Buy, NotionalUsd(10_000.0)),
            status: Filled,
            reason: None,
            order_qty: 28.802,
            bound: 348.23,
            filled_qty: 28.802,
            notional: 10_000.999_87,
            avg: 347.232_826_539_824,
            slip: 1.089_489_762_917_766,
            fee: 0.900_089_988_3,
            levels: 2,
        },
        // Bound 347.195 × 0.997 = 346.153415 → rounded up to 346.16.
        V {
            order: market(Sell, Qty(100.0)),
            status: Filled,
            reason: None,
            order_qty: 100.0,
            bound: 346.16,
            filled_qty: 100.0,
            notional: 34_715.638_28,
            avg: 347.156_382_8,
            slip: 1.112_262_561_384_813,
            fee: 3.124_407_445_2,
            levels: 4,
        },
        V {
            order: market(Sell, NotionalUsd(50_000.0)),
            status: Filled,
            reason: None,
            order_qty: 144.011,
            bound: 346.16,
            filled_qty: 144.011,
            notional: 49_992.296_49,
            avg: 347.142_207_817_458,
            slip: 1.520_534_067_069_267,
            fee: 4.499_306_684_1,
            levels: 4,
        },
        // $25 → 0.0720056… → 0.072 (value 0.072 × 348.23 = 25.07256).
        V {
            order: market(Buy, NotionalUsd(25.0)),
            status: Filled,
            reason: None,
            order_qty: 0.072,
            bound: 348.23,
            filled_qty: 0.072,
            notional: 25.000_56,
            avg: 347.23,
            slip: 1.008_079_033_396_218,
            fee: 0.002_250_050_4,
            levels: 1,
        },
        // 2 bps: 347.264439 → 347.26 stops before 347.27 (partial, bound).
        V {
            order: tight,
            status: Partial,
            reason: Some(FillReason::Bound),
            order_qty: 100.0,
            bound: 347.26,
            filled_qty: 32.572,
            notional: 11_310.103_31,
            avg: 347.233_922_080_314,
            slip: 1.121_043_802_888_303,
            fee: 1.017_909_297_9,
            levels: 3,
        },
        // More than the 530.752 visible: partial, depth — never hidden liquidity.
        V {
            order: wide,
            status: Partial,
            reason: Some(FillReason::Depth),
            order_qty: 1_000.0,
            bound: 350.66,
            filled_qty: 530.752,
            notional: 184_357.911_6,
            avg: 347.352_269_233_088,
            slip: 4.529_709_042_127_526,
            fee: 16.592_212_044,
            levels: 20,
        },
        // Limit tighter than the 30 bps bound: it binds.
        V {
            order: limit(Buy, 100.0, 347.27),
            status: Partial,
            reason: Some(FillReason::Bound),
            order_qty: 100.0,
            bound: 347.27,
            filled_qty: 61.594,
            notional: 21_388.573_25,
            avg: 347.250_921_355_976,
            slip: 1.610_661_327_963_578,
            fee: 1.924_971_592_5,
            levels: 4,
        },
        V {
            order: limit(Sell, 100.0, 347.12),
            status: Partial,
            reason: Some(FillReason::Bound),
            order_qty: 100.0,
            bound: 347.12,
            filled_qty: 98.15,
            notional: 34_073.484_78,
            avg: 347.157_257_055_527,
            slip: 1.087_082_028_046_078,
            fee: 3.066_613_630_2,
            levels: 3,
        },
    ];
    for v in vectors {
        let r = run(&v.order);
        let what = format!("{:?} {:?} {:?}", v.order.kind, v.order.side, v.order.size);
        assert_eq!((r.status, r.reason), (v.status, v.reason), "{what}: {r:?}");
        assert_eq!(r.order_qty, Some(v.order_qty), "{what}");
        assert_eq!(r.bound_px, Some(v.bound), "{what}");
        close(r.ref_px.unwrap(), MID, &format!("{what} ref_px = book mid"));
        assert_eq!(r.fills.len(), v.levels, "{what}");
        close(r.filled_qty, v.filled_qty, &format!("{what} filled_qty"));
        close(
            r.filled_notional_usd,
            v.notional,
            &format!("{what} notional"),
        );
        close(r.avg_px.unwrap(), v.avg, &format!("{what} avg_px"));
        let slip = r.slippage_bps.unwrap();
        assert!(
            (slip - v.slip).abs() < 1e-9,
            "{what} slip: {slip} vs {}",
            v.slip
        );
        close(r.fee_usd, v.fee, &format!("{what} fee"));
        close(r.mid.unwrap(), MID, "mid");
        assert_eq!(r.book_age_ms, Some(1_000));
        assert_eq!(r.message.is_some(), v.reason.is_some(), "{what}");
    }
}

#[test]
fn fills_are_the_walk_levels() {
    let r = run(&market(Side::Buy, OrderSize::NotionalUsd(10_000.0)));
    let got: Vec<(usize, f64)> = r.fills.iter().map(|f| (f.level, f.px)).collect();
    assert_eq!(got, [(0, 347.23), (1, 347.24)]);
    close(r.fills[0].qty, 20.661, "level 0 whole");
    close(r.fills[1].qty, 8.141, "level 1 rest");
    // A partial fill says what was canceled and why.
    let tight = PaperOrder {
        max_slippage_bps: 2.0,
        ..market(Side::Buy, OrderSize::Qty(100.0))
    };
    assert_eq!(
        run(&tight).message.as_deref(),
        Some("32.572 of 100 filled, rest canceled: the next level is beyond the bound 347.26")
    );
    let wide = PaperOrder {
        max_slippage_bps: 100.0,
        ..market(Side::Buy, OrderSize::Qty(1_000.0))
    };
    assert_eq!(
        run(&wide).message.as_deref(),
        Some("530.752 of 1000 filled, rest canceled: visible depth ran out after 20 levels")
    );
}

#[test]
fn min_trade_ntl_counts_the_rounded_size_at_the_bound() {
    // $10 → 0.0288… → 0.028; 0.028 × 348.23 = $9.75044 < $10.
    let r = run(&market(Side::Buy, OrderSize::NotionalUsd(10.0)));
    rejected(&r, FillReason::MinTradeNtl);
    assert_eq!(r.order_qty, Some(0.028));
    let m = r.message.unwrap();
    assert!(
        m.starts_with("Order must have minimum value of $10.") && m.contains("9.75044"),
        "{m}"
    );
    // $10.50 → 0.030; 0.03 × 348.23 = $10.4469 passes.
    let r = run(&market(Side::Buy, OrderSize::NotionalUsd(10.5)));
    assert_eq!(r.status, FillStatus::Filled);
    close(r.filled_notional_usd, 10.4169, "notional");
    close(r.fee_usd, 0.000_937_521, "fee");
    // A size that rounds to 0.
    let r = run(&market(Side::Buy, OrderSize::Qty(0.0004)));
    rejected(&r, FillReason::MinTradeNtl);
    assert_eq!(r.order_qty, Some(0.0));
}

#[test]
fn tick_rule_on_limit_prices() {
    // 347.165 has 6 significant figures.
    rejected(&run(&limit(Side::Buy, 1.0, 347.165)), FillReason::Tick);
    // 34.7165 would need 4 decimals > 6 − 3.
    let r = run(&limit(Side::Buy, 1.0, 34.7165));
    rejected(&r, FillReason::Tick);
    assert!(r
        .message
        .unwrap()
        .starts_with("Price must be divisible by tick size."));
    // f64 noise on a valid price is not a tick error.
    let r = run(&limit(Side::Buy, 1.0, 347.13 + 0.1));
    assert_eq!(r.status, FillStatus::Filled, "{r:?}");
    assert_eq!(r.avg_px, Some(347.23));
}

#[test]
fn ioc_and_market_orders_that_take_nothing() {
    // 347.2 is on tick but under the best ask 347.23.
    let r = run(&limit(Side::Buy, 1.0, 347.2));
    rejected(&r, FillReason::IocCancel);
    assert_eq!(r.bound_px, Some(347.2));
    assert!(r
        .message
        .unwrap()
        .contains("best 347.23 is beyond the bound 347.2"));
    // Priced at 345 (the market moved up during the latency): the bound
    // 345 × 1.003 = 346.035 → 346.03 is under every ask.
    let anchored = PaperOrder {
        ref_mid: Some(345.0),
        ..market(Side::Buy, OrderSize::NotionalUsd(1_000.0))
    };
    let r = run(&anchored);
    rejected(&r, FillReason::MarketOrderNoLiquidity);
    assert_eq!(
        (r.ref_px, r.bound_px, r.order_qty),
        (Some(345.0), Some(346.03), Some(2.898))
    );
    // An empty book side: no liquidity.
    let bids_only = L2Book::new(
        vec![L2Level {
            px: 347.16,
            sz: 1.0,
            n: 1,
        }],
        vec![],
        0,
    )
    .unwrap();
    let fees = fees();
    let env = FillEnv {
        rules: &rules(),
        status: MarketStatus::Open,
        position: &flat(),
        fees: &fees,
        max_book_age_ms: 5_000,
        stale_book_ok: false,
    };
    let buy = market(Side::Buy, OrderSize::Qty(1.0));
    rejected(
        &simulate_fill(&buy, &bids_only, 0, &env),
        FillReason::MarketOrderNoLiquidity,
    );
    let buy_limit = limit(Side::Buy, 1.0, 347.3);
    rejected(
        &simulate_fill(&buy_limit, &bids_only, 0, &env),
        FillReason::IocCancel,
    );
    // Its liquid side, but no mid to bound slippage: refused.
    let sell = market(Side::Sell, OrderSize::Qty(1.0));
    rejected(
        &simulate_fill(&sell, &bids_only, 0, &env),
        FillReason::MissingMid,
    );
    // …unless the order carries the mid it was priced at.
    let sell = PaperOrder {
        ref_mid: Some(347.2),
        ..sell
    };
    let r = simulate_fill(&sell, &bids_only, 0, &env);
    assert_eq!(
        (r.status, r.mid, r.slippage_bps),
        (FillStatus::Filled, None, None)
    );
}

#[test]
fn reduce_only_clips_to_the_position_and_never_adds() {
    let ro = |side, size| PaperOrder {
        reduce_only: true,
        ..market(side, size)
    };
    // Flat, or the same side as the position.
    let r = run(&ro(Side::Sell, OrderSize::Qty(1.0)));
    rejected(&r, FillReason::ReduceOnly);
    assert!(r
        .message
        .unwrap()
        .starts_with("Reduce only order would increase position."));
    let long = held(2.0, 340.0);
    rejected(
        &run_with(&ro(Side::Buy, OrderSize::Qty(1.0)), &rules(), &long),
        FillReason::ReduceOnly,
    );
    // Larger than the position: clipped to it.
    let r = run_with(&ro(Side::Sell, OrderSize::Qty(5.0)), &rules(), &long);
    assert_eq!((r.status, r.order_qty), (FillStatus::Filled, Some(2.0)));
    close(r.filled_notional_usd, 694.32, "2 × 347.16");
    // A $3 dust position closes whole, under the $10 minimum.
    let dust = held(0.009, 350.0);
    let r = run_with(
        &ro(Side::Sell, OrderSize::NotionalUsd(25.0)),
        &rules(),
        &dust,
    );
    assert_eq!((r.status, r.order_qty), (FillStatus::Filled, Some(0.009)));
    close(r.filled_notional_usd, 3.124_44, "0.009 × 347.16");
    // A partial reduce under $10 is still rejected: 0.02 × 346.16 = 6.9232.
    let r = run_with(&ro(Side::Sell, OrderSize::Qty(0.02)), &rules(), &long);
    rejected(&r, FillReason::MinTradeNtl);
    // A short reduces with buys.
    let short = held(-1.0, 350.0);
    let r = run_with(&ro(Side::Buy, OrderSize::Qty(3.0)), &rules(), &short);
    assert_eq!((r.status, r.order_qty), (FillStatus::Filled, Some(1.0)));
}

#[test]
fn oracle_band_rejects_far_prices_and_unknown_oracles() {
    let band = |oracle_px| VenueRules {
        oracle_band: Some(OracleBand {
            oracle_px,
            max_bps: 100.0,
        }),
        ..rules()
    };
    let buy = market(Side::Buy, OrderSize::NotionalUsd(1_000.0));
    // Bound 348.23 vs oracle 340: 242.06 bps > 100.
    let r = run_with(&buy, &band(Some(340.0)), &flat());
    rejected(&r, FillReason::Oracle);
    assert!(r
        .message
        .unwrap()
        .starts_with("Order price too far from oracle"));
    // 35.4 bps from 347: inside the band.
    assert_eq!(
        run_with(&buy, &band(Some(347.0)), &flat()).status,
        FillStatus::Filled
    );
    for unknown in [None, Some(0.0), Some(f64::NAN)] {
        rejected(
            &run_with(&buy, &band(unknown), &flat()),
            FillReason::MissingOracle,
        );
    }
}

#[test]
fn oi_cap_blocks_opening_adding_and_flipping_only() {
    let capped = VenueRules {
        at_oi_cap: Some(true),
        ..rules()
    };
    let unknown = VenueRules {
        at_oi_cap: None,
        ..rules()
    };
    let long = held(1.0, 340.0);
    let buy = market(Side::Buy, OrderSize::Qty(0.5));
    let r = run_with(&buy, &capped, &flat());
    rejected(&r, FillReason::PositionIncreaseAtOpenInterestCap);
    assert!(r
        .message
        .unwrap()
        .starts_with("Order would increase open interest while open interest is capped"));
    rejected(
        &run_with(&buy, &capped, &long),
        FillReason::PositionIncreaseAtOpenInterestCap,
    );
    let flip = market(Side::Sell, OrderSize::Qty(2.0));
    rejected(
        &run_with(&flip, &capped, &long),
        FillReason::PositionFlipAtOpenInterestCap,
    );
    // Reducing passes, capped or unknown.
    let reduce = market(Side::Sell, OrderSize::Qty(0.5));
    assert_eq!(run_with(&reduce, &capped, &long).status, FillStatus::Filled);
    assert_eq!(
        run_with(&reduce, &unknown, &long).status,
        FillStatus::Filled
    );
    let ro = PaperOrder {
        reduce_only: true,
        ..flip
    };
    assert_eq!(run_with(&ro, &unknown, &long).order_qty, Some(1.0));
    // Unknown cap state refuses what would add.
    rejected(&run_with(&buy, &unknown, &flat()), FillReason::MissingOiCap);
}

/// `stale_book_ok` (the gate waived `book_age` for a reduce-only exit):
/// a stale book fills, marked `stale_book`. Without the flag, or for an
/// order that is not reduce-only, it is still refused.
#[test]
fn a_waived_exit_fills_at_a_stale_book() {
    let (rules, fees, book, long) = (rules(), fees(), tsla_book(), held(2.0, 340.0));
    let env = |stale_book_ok| FillEnv {
        rules: &rules,
        status: MarketStatus::Open,
        position: &long,
        fees: &fees,
        max_book_age_ms: 5_000,
        stale_book_ok,
    };
    let exit = PaperOrder {
        reduce_only: true,
        ..market(Side::Sell, OrderSize::Qty(2.0))
    };
    let r = simulate_fill(&exit, &book, 9_000, &env(true));
    assert_eq!(
        (r.status, r.stale_book, r.book_age_ms, r.filled_qty),
        (FillStatus::Filled, true, Some(9_000), 2.0)
    );
    assert_eq!(serde_json::to_value(&r).unwrap()["stale_book"], true);
    rejected(
        &simulate_fill(&exit, &book, 9_000, &env(false)),
        FillReason::StaleBook,
    );
    let entry = market(Side::Buy, OrderSize::NotionalUsd(1_000.0));
    rejected(
        &simulate_fill(&entry, &book, 9_000, &env(true)),
        FillReason::StaleBook,
    );
    // A fresh book is never marked, and the field stays off the wire.
    let fresh = simulate_fill(&exit, &book, 1_000, &env(true));
    assert!(!fresh.stale_book);
    assert!(serde_json::to_value(&fresh)
        .unwrap()
        .get("stale_book")
        .is_none());
}

#[test]
fn market_state_and_book_age_refuse_before_pricing() {
    let o = market(Side::Buy, OrderSize::NotionalUsd(1_000.0));
    let fees = fees();
    let book = tsla_book();
    for (status, reason) in [
        (MarketStatus::Halted, FillReason::MarketHalted),
        (MarketStatus::Closed, FillReason::MarketClosed),
        (MarketStatus::Delisted, FillReason::Delisted),
    ] {
        let env = FillEnv {
            rules: &rules(),
            status,
            position: &flat(),
            fees: &fees,
            max_book_age_ms: 5_000,
            stale_book_ok: false,
        };
        let r = simulate_fill(&o, &book, 0, &env);
        rejected(&r, reason);
        assert!(r.message.unwrap().contains(TSLA), "full id in the message");
    }
    let env = FillEnv {
        rules: &rules(),
        status: MarketStatus::Open,
        position: &flat(),
        fees: &fees,
        max_book_age_ms: 5_000,
        stale_book_ok: false,
    };
    let r = simulate_fill(&o, &book, 5_001, &env);
    rejected(&r, FillReason::StaleBook);
    assert_eq!((r.bound_px, r.book_age_ms), (None, Some(5_001)));
    assert_eq!(
        simulate_fill(&o, &book, 5_000, &env).status,
        FillStatus::Filled
    );
    // A deserialized book skips `L2Book::new`: a crossed one is refused.
    let lvl = |px| L2Level { px, sz: 1.0, n: 1 };
    let crossed = L2Book {
        bids: vec![lvl(347.3)],
        asks: vec![lvl(347.2)],
        venue_ts_ms: 0,
    };
    let r = simulate_fill(&o, &crossed, 0, &env);
    rejected(&r, FillReason::BadBook);
    assert!(r.message.unwrap().contains("crossed"));
}

#[test]
fn malformed_orders_are_refused() {
    let base = market(Side::Buy, OrderSize::NotionalUsd(1_000.0));
    let bad = [
        PaperOrder {
            size: OrderSize::Qty(f64::NAN),
            ..base.clone()
        },
        PaperOrder {
            size: OrderSize::NotionalUsd(0.0),
            ..base.clone()
        },
        PaperOrder {
            max_slippage_bps: 0.0,
            ..base.clone()
        },
        PaperOrder {
            max_slippage_bps: 10_000.0,
            ..base.clone()
        },
        PaperOrder {
            ref_mid: Some(-1.0),
            ..base.clone()
        },
        PaperOrder {
            limit_px: Some(347.3),
            ..base.clone()
        },
        PaperOrder {
            kind: OrderKind::Limit,
            ..base.clone()
        },
        PaperOrder {
            client_order_id: " ".to_string(),
            ..base.clone()
        },
    ];
    for o in &bad {
        rejected(&run(o), FillReason::InvalidOrder);
    }
    let other = Position::flat("hyperliquid:xyz:NVDA", "company:nvidia", "hyperliquid");
    let r = run_with(&base, &rules(), &other);
    rejected(&r, FillReason::InvalidOrder);
    assert!(r.message.unwrap().contains("hyperliquid:xyz:NVDA"));
    let nan = Position {
        qty: f64::NAN,
        ..flat()
    };
    rejected(&run_with(&base, &rules(), &nan), FillReason::InvalidOrder);
    let wide = VenueRules {
        sz_decimals: 7,
        ..rules()
    };
    rejected(&run_with(&base, &wide, &flat()), FillReason::InvalidOrder);
}

#[test]
fn a_fill_books_into_the_ledger() {
    let r = run(&market(Side::Buy, OrderSize::NotionalUsd(10_000.0)));
    let fill = r.ledger_fill(TESLA, 1_790_775_353_605).unwrap();
    assert_eq!(
        (fill.instrument.as_str(), fill.venue.as_str(), fill.side),
        (TSLA, "hyperliquid", Side::Buy)
    );
    let mut a = PaperAccount::new("paper-test", 100_000.0).unwrap();
    a.apply_fill(&fill).unwrap();
    let p = &a.positions[TSLA];
    close(p.qty, 28.802, "qty");
    close(p.avg_px.unwrap(), 347.232_826_539_824, "avg");
    close(a.cash_usd, 100_000.0 - 0.900_089_988_3, "cash pays the fee");
    let none = run(&limit(Side::Buy, 1.0, 347.2));
    assert_eq!(none.ledger_fill(TESLA, 0), None);
}

#[test]
fn reasons_serialize_as_hl_codes_and_engine_names() {
    for r in FillReason::ALL {
        assert_eq!(serde_json::to_value(r).unwrap(), r.as_str(), "{r:?}");
        let back: FillReason = serde_json::from_value(r.as_str().into()).unwrap();
        assert_eq!(back, r);
        let venue = r.as_str().starts_with(char::is_uppercase);
        assert_eq!(r.hl_error().is_some(), venue, "{r:?}");
    }
    let r = run(&market(Side::Buy, OrderSize::NotionalUsd(10.0)));
    let v = serde_json::to_value(&r).unwrap();
    assert_eq!(v["reason"], "MinTradeNtl");
    assert_eq!(v["status"], "rejected");
    assert_eq!(v["instrument"], TSLA);
    assert_eq!(serde_json::from_value::<FillResult>(v).unwrap(), r);
}

/// Missing data, book age, liquidity and price bounds are transient;
/// size / lot rules, a bad order and delisting are final.
#[test]
fn reasons_split_into_transient_and_final() {
    let transient: Vec<&str> = FillReason::ALL
        .into_iter()
        .filter(|r| r.is_transient())
        .map(FillReason::as_str)
        .collect();
    assert_eq!(
        transient,
        [
            "IocCancel",
            "MarketOrderNoLiquidity",
            "Oracle",
            "PositionIncreaseAtOpenInterestCap",
            "PositionFlipAtOpenInterestCap",
            "market_halted",
            "market_closed",
            "stale_book",
            "bad_book",
            "missing:mid",
            "missing:oracle",
            "missing:at_oi_cap",
            "bound",
            "depth"
        ]
    );
    for r in [
        FillReason::MinTradeNtl,
        FillReason::Tick,
        FillReason::ReduceOnly,
        FillReason::InvalidOrder,
        FillReason::OrderType,
        FillReason::Delisted,
    ] {
        assert!(!r.is_transient(), "{r:?}");
    }
}

#[test]
fn orders_round_trip_and_reject_unknown_fields() {
    let o: PaperOrder = serde_json::from_str(
        r#"{"client_order_id":"c1","instrument":"hyperliquid:xyz:TSLA","side":"buy",
                "size":{"notional_usd":25},"kind":"market","tif":"ioc","reduce_only":false,
                "max_slippage_bps":30}"#,
    )
    .unwrap();
    assert_eq!(o.size, OrderSize::NotionalUsd(25.0));
    assert_eq!(o.instrument.to_string(), TSLA);
    let back: PaperOrder = serde_json::from_value(serde_json::to_value(&o).unwrap()).unwrap();
    assert_eq!(back, o);
    assert!(
        serde_json::from_str::<PaperOrder>(
            r#"{"client_order_id":"c1","instrument":"hyperliquid:xyz:TSLA","side":"buy",
                "size":{"qty":1},"kind":"market","tif":"gtc","reduce_only":false,
                "max_slippage_bps":30}"#
        )
        .is_err(),
        "gtc is P1"
    );
    assert!(serde_json::from_str::<PaperOrder>(
        r#"{"client_order_id":"c1","instrument":"hyperliquid:xyz:TSLA","side":"buy",
                "size":{"qty":1},"kind":"market","tif":"ioc","reduce_only":false,
                "max_slippage_bps":30,"post_only":true}"#
    )
    .is_err());
}

#[test]
fn latency_jitter_is_uniform_around_the_base() {
    for (base, jitter, r, want) in [
        (250, 100, 0.5, 250),
        (250, 100, 0.0, 150),
        (250, 100, 1.0, 350),
        (250, 100, 0.25, 200),
        (250, 100, f64::NAN, 250),
        (250, 100, 1.5, 250),
        (250, 100, -0.1, 250),
        (250, 0, 0.9, 250),
        (100, 100, 0.0, 0),
        (0, 0, 0.3, 0),
    ] {
        assert_eq!(
            jittered_latency_ms(base, jitter, r),
            want,
            "{base} ± {jitter} @ {r}"
        );
    }
}
