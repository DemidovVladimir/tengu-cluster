//! Paper fills with latency (`risk-paper-fill-engine`, tracker convention
//! 16): sleep the simulated latency on the `Clock`, THEN read the book from
//! the `BookSource`, THEN `domain::xm::paper::simulate_fill` against that
//! book — the market moves during the latency. The same code runs live
//! (`SystemClock` + a fresh `hl_book` read) and in replay (recorded time +
//! `HistoryStore` as-of books, M7).
//!
//! | Step | Rule |
//! |---|---|
//! | 1 | kind in `[paper] order_types` (`market` = market order, `ioc` = limit IOC), else `order_type`; malformed ⇒ `invalid_order` — both refused at once: no latency, no book read |
//! | 2 | latency = `latency_ms ± latency_jitter_ms`, uniform in the injected `rand01` (`rate_limit::jitter01()` live, fixed in tests and replay) |
//! | 3 | `sleep_until_ms(sent + latency)`, then `fresh_book(instrument)`; a failed read is `Err` — nothing filled, the caller records or retries |
//! | 4 | `simulate_fill` on that book, age = now − the older of its venue / read stamps (`BookRead::age_ms`) |
//!
//! A caller that re-reads its position after the sleep (the gate's ledger
//! transaction) re-runs `simulate_fill` on `PaperFill.book` — pure, cheap.
//!
//! [`decide`] is that step for the exec tools (`risk-gate-enforcement`,
//! `tools/xm/exec_common.rs`): the `Decide` closure `PaperLedger::place` runs
//! inside its transaction.
//!
//! | Step (on the snapshot read inside the transaction) | Rule |
//! |---|---|
//! | Value | `RiskState::value` at the plan's marks with the UTC day rolled |
//! | Intent | the order's size at the post-latency book's mid (else the mark, else the position's entry); a close = the whole snapshot position, opposite side; underlying = the position's, else the instrument id |
//! | Gate | `evaluate` against the plan's legs (the book read after the latency, the `mkt_ctx/1` row), opportunity row and kill-switch probe |
//! | Fill | allowed ⇒ `simulate_fill` on that book against the snapshot position; no book ⇒ refused `stale_book` (a degraded exit may be allowed without one) |
//! | Trips | the verdict's halts recorded (`RiskState::trip`) |

// Callers land with `risk-paper-tools` (the exec tools run `run_exec`).
#![allow(dead_code)]

use std::collections::BTreeMap;

use crate::config::risk::{OrderType, PaperConfig};
use crate::domain::book::Side;
use crate::domain::observation::{Field, ReadError};
use crate::domain::xm::cost::FeeSchedule;
use crate::domain::xm::ledger::{Mark, Position};
use crate::domain::xm::paper::{
    check_order, jittered_latency_ms, simulate_fill, FillEnv, FillReason, FillResult, MarketStatus,
    OrderKind, OrderSize, PaperOrder, Tif, VenueRules,
};
use crate::domain::xm::risk::{
    evaluate, venue_of, EdgeInput, LegMarket, OrderIntent, RiskContext, RiskLimits,
};
use crate::ports::book::{BookRead, BookSource};
use crate::ports::clock::Clock;
use crate::ports::paper::{Decide, Decision, LedgerSnapshot, Outcome};

/// One paper order after its latency.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PaperFill {
    /// Clock time the order was sent (before the latency).
    pub sent_ms: i64,
    /// Latency slept; 0 when refused before sending.
    pub latency_ms: u64,
    /// The book read after the latency; `None` when refused before sending.
    pub book: Option<BookRead>,
    pub result: FillResult,
}

/// The `[paper] order_types` entry `order` needs.
pub(crate) fn order_type(order: &PaperOrder) -> OrderType {
    match (order.kind, order.tif) {
        (OrderKind::Market, _) => OrderType::Market,
        (OrderKind::Limit, Tif::Ioc) => OrderType::Ioc,
    }
}

fn type_name(t: OrderType) -> &'static str {
    match t {
        OrderType::Market => "market",
        OrderType::Ioc => "ioc",
    }
}

/// The module table: refuse at once, else sleep, read, simulate.
pub(crate) async fn fill_with_latency(
    clock: &dyn Clock,
    books: &dyn BookSource,
    paper: &PaperConfig,
    order: &PaperOrder,
    env: &FillEnv<'_>,
    rand01: f64,
) -> Result<PaperFill, ReadError> {
    let sent_ms = clock.now_ms();
    let refused = |reason: FillReason, detail: &str| PaperFill {
        sent_ms,
        latency_ms: 0,
        book: None,
        result: FillResult::refused(order, reason, detail),
    };
    let needs = order_type(order);
    if !paper.order_types.contains(&needs) {
        let on: Vec<&str> = paper.order_types.iter().map(|t| type_name(*t)).collect();
        return Ok(refused(
            FillReason::OrderType,
            &format!(
                "{} orders are off ([paper] order_types = {on:?})",
                type_name(needs)
            ),
        ));
    }
    if let Err(what) = check_order(order, env) {
        return Ok(refused(FillReason::InvalidOrder, &what));
    }
    let latency_ms = jittered_latency_ms(paper.latency_ms, paper.latency_jitter_ms, rand01);
    let arrive_ms = sent_ms.saturating_add(i64::try_from(latency_ms).unwrap_or(i64::MAX));
    clock.sleep_until_ms(arrive_ms).await;
    let read = books.fresh_book(&order.instrument).await?;
    let book_age_ms = read.age_ms(clock.now_ms());
    let result = simulate_fill(order, &read.book, book_age_ms, env);
    Ok(PaperFill {
        sent_ms,
        latency_ms,
        book: Some(read),
        result,
    })
}

/// Everything an exec order read before `PaperLedger::place`: the closure
/// only computes (`ports/paper.rs`).
pub(crate) struct ExecPlan {
    /// The gate's limits: `[risk]`, or a shadow account's own.
    pub limits: RiskLimits,
    /// The order as sent — sized from the position read before the latency.
    pub order: PaperOrder,
    /// Close the whole position: size and side from the snapshot.
    pub close: bool,
    pub strategy: Option<String>,
    pub hedge_instrument: Option<String>,
    pub opportunity_key: Option<String>,
    pub exit_at_ms: Option<i64>,
    /// Marks by full id: the open positions and the order's instrument.
    pub marks: BTreeMap<String, Field<Mark>>,
    /// Market inputs by full id: the order leg (the book below + its
    /// `mkt_ctx/1` row), the hedge leg.
    pub legs: BTreeMap<String, LegMarket>,
    pub opportunity: Field<EdgeInput>,
    pub kill_switch: Field<bool>,
    pub rules: VenueRules,
    pub fees: FeeSchedule,
    pub status: MarketStatus,
    /// The book read after the latency, or why there is none.
    pub book: Result<BookRead, ReadError>,
}

/// The ledger closure for `plan` (module table).
pub(crate) fn decide(plan: ExecPlan) -> Decide {
    Box::new(move |snap: &LedgerSnapshot| {
        let now = snap.now_ms;
        let limits = &plan.limits;
        let id = plan.order.instrument.to_string();
        let held = snap.account.positions.get(&id);
        let underlying = held.map_or_else(|| id.clone(), |p| p.underlying.clone());
        let position = held
            .cloned()
            .unwrap_or_else(|| Position::flat(&id, &underlying, venue_of(&id).unwrap_or_default()));
        let mut order = plan.order.clone();
        if plan.close {
            if position.qty != 0.0 {
                order.side = if position.qty > 0.0 {
                    Side::Sell
                } else {
                    Side::Buy
                };
            }
            // Flat inside the transaction ⇒ qty 0 ⇒ denied `intent`.
            order.size = OrderSize::Qty(position.qty.abs());
        }
        let book_mid = plan.book.as_ref().ok().and_then(|b| b.book.mid());
        let mark_px = plan.marks.get(&id).and_then(|m| m.value()).map(|m| m.px);
        let ref_px = book_mid
            .or(mark_px)
            .or(position.avg_px)
            .filter(|p| p.is_finite() && *p > 0.0);
        let (qty, notional_usd) = match (order.size, ref_px) {
            (OrderSize::Qty(q), Some(px)) => (q, q * px),
            (OrderSize::Qty(q), None) => (q, f64::NAN),
            (OrderSize::NotionalUsd(n), Some(px)) => (n / px, n),
            (OrderSize::NotionalUsd(n), None) => (f64::NAN, n),
        };
        let intent = OrderIntent {
            account: limits.account.clone(),
            instrument: id.clone(),
            underlying,
            side: order.side,
            qty,
            notional_usd,
            reduce_only: order.reduce_only,
            strategy: plan.strategy.clone(),
            hedge_instrument: plan.hedge_instrument.clone(),
            opportunity_key: plan.opportunity_key.clone(),
        };
        let (valued, rolled) =
            snap.risk
                .value(&snap.account, &plan.marks, now, limits.max_data_age_ms.ctx);
        let ctx = RiskContext {
            account: valued,
            halt: rolled.effective_halt(now).cloned(),
            kill_switch: plan.kill_switch.clone(),
            lifecycle: Field::Absent,
            orders_last_min: snap.orders_last_min,
            open_orders: snap.open_orders,
            legs: plan.legs.clone(),
            opportunity: plan.opportunity.clone(),
        };
        let verdict = evaluate(&intent, &ctx, limits, now);
        let outcome = if verdict.allow {
            let env = FillEnv {
                rules: &plan.rules,
                status: plan.status,
                position: &position,
                fees: &plan.fees,
                max_book_age_ms: limits.max_data_age_ms.book,
            };
            let result = match &plan.book {
                Ok(read) => simulate_fill(&order, &read.book, read.age_ms(now), &env),
                Err(e) => FillResult::refused(
                    &order,
                    FillReason::StaleBook,
                    &format!(
                        "no book after the latency: {} {} {}",
                        e.field,
                        e.class.as_str(),
                        e.message
                    ),
                ),
            };
            Outcome::Sent {
                result,
                exit_at_ms: plan.exit_at_ms,
            }
        } else {
            Outcome::Denied
        };
        let next = rolled.trip(&verdict.trips, now);
        Decision {
            intent,
            verdict,
            context: ctx.digest(now),
            outcome,
            risk: (next != snap.risk).then_some(next),
        }
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::domain::book::fixture::tsla_book;
    use crate::domain::book::{L2Book, L2Level, Side};
    use crate::domain::market::InstrumentId;
    use crate::domain::observation::ErrorClass;
    use crate::domain::xm::cost::{FeeSchedule, HlKind};
    use crate::domain::xm::ledger::Position;
    use crate::domain::xm::paper::{FillStatus, MarketStatus, OrderSize, VenueRules};
    use crate::ports::book::ScriptedBooks;
    use crate::ports::clock::ManualClock;

    const TSLA: &str = "hyperliquid:xyz:TSLA";
    /// Venue time of the fixture book (2026-09-30T13:35:52.605Z).
    const FIXTURE_TS: i64 = 1_790_775_352_605;
    /// Orders are sent 400 ms after that snapshot.
    const T0: i64 = FIXTURE_TS + 400;

    fn paper(order_types: Vec<OrderType>) -> PaperConfig {
        PaperConfig {
            initial_cash_usd: 100.0,
            latency_ms: 250,
            latency_jitter_ms: 100,
            fee_tier: 0,
            staking_discount_pct: 0.0,
            order_types,
        }
    }

    fn both() -> PaperConfig {
        paper(vec![OrderType::Market, OrderType::Ioc])
    }

    /// The market 40 bps up (mid 348.585), stamped 20 ms before the order
    /// arrives at T0 + 250.
    fn moved_book() -> L2Book {
        let l = |px, sz| L2Level { px, sz, n: 1 };
        L2Book::new(
            vec![l(348.55, 10.0), l(348.5, 50.0)],
            vec![l(348.62, 10.0), l(348.7, 50.0)],
            T0 + 230,
        )
        .unwrap()
    }

    struct Rig {
        clock: Arc<ManualClock>,
        books: ScriptedBooks,
    }

    fn rig_with(start: i64, script: Vec<(i64, L2Book)>) -> Rig {
        let clock = Arc::new(ManualClock::at(start));
        let books = ScriptedBooks::new(clock.clone(), InstrumentId::parse(TSLA).unwrap(), script);
        Rig { clock, books }
    }

    /// Clock at T0; the fixture book until T0 + 250, the moved book from then.
    fn rig() -> Rig {
        rig_with(T0, vec![(0, tsla_book()), (T0 + 250, moved_book())])
    }

    fn close(a: Option<f64>, b: f64, what: &str) {
        let a = a.unwrap_or_else(|| panic!("{what}: missing"));
        assert!((a - b).abs() <= 1e-9 * b.abs(), "{what}: got {a}, want {b}");
    }

    fn buy(size: OrderSize) -> PaperOrder {
        PaperOrder {
            client_order_id: "xm_entry:session-1:3".to_string(),
            instrument: InstrumentId::parse(TSLA).unwrap(),
            side: Side::Buy,
            size,
            kind: OrderKind::Market,
            tif: Tif::Ioc,
            limit_px: None,
            reduce_only: false,
            max_slippage_bps: 30.0,
            ref_mid: None,
        }
    }

    async fn fill(
        rig: &Rig,
        cfg: &PaperConfig,
        order: &PaperOrder,
        rand01: f64,
    ) -> Result<PaperFill, ReadError> {
        let rules = VenueRules::hyperliquid(HlKind::Perp, 3, Some(false));
        let position = Position::flat(TSLA, "company:tesla", "hyperliquid");
        let fees = FeeSchedule::new(0.9, 0.3).unwrap();
        let env = FillEnv {
            rules: &rules,
            status: MarketStatus::Open,
            position: &position,
            fees: &fees,
            max_book_age_ms: 5_000,
        };
        fill_with_latency(rig.clock.as_ref(), &rig.books, cfg, order, &env, rand01).await
    }

    #[tokio::test]
    async fn sleeps_then_reads_then_fills_against_the_book_it_finds() {
        let rig = rig();
        let pf = fill(&rig, &both(), &buy(OrderSize::NotionalUsd(1_000.0)), 0.5)
            .await
            .unwrap();
        assert_eq!((pf.sent_ms, pf.latency_ms), (T0, 250));
        assert_eq!(rig.clock.now_ms(), T0 + 250, "slept the latency");
        assert_eq!(rig.books.reads(), [T0 + 250], "one read, after the sleep");
        let read = pf.book.unwrap();
        assert_eq!(
            (read.observed_at_ms, read.venue_ts_ms()),
            (T0 + 250, T0 + 230)
        );
        // Filled against the moved book: $1 000 / 348.585 → 2.868 at 348.62,
        // bound 348.585 × 1.003 = 349.630755 → 349.63.
        let r = pf.result;
        assert_eq!(r.status, FillStatus::Filled, "{r:?}");
        assert_eq!(r.order_qty, Some(2.868));
        close(r.avg_px, 348.62, "avg_px");
        close(r.mid, 348.585, "mid of the moved book");
        assert_eq!((r.bound_px, r.book_age_ms), (Some(349.63), Some(20)));
    }

    #[tokio::test]
    async fn jitter_decides_which_book_the_order_meets() {
        // rand01 = 0 ⇒ 250 − 100 = 150 ms: the market has not moved yet.
        let rig = rig();
        let pf = fill(&rig, &both(), &buy(OrderSize::NotionalUsd(1_000.0)), 0.0)
            .await
            .unwrap();
        assert_eq!(pf.latency_ms, 150);
        assert_eq!(rig.books.reads(), [T0 + 150]);
        close(pf.result.avg_px, 347.23, "avg_px on the fixture book");
        assert_eq!(pf.result.book_age_ms, Some(550));
    }

    #[tokio::test]
    async fn an_order_priced_before_the_move_misses_the_moved_market() {
        // Priced at the fixture mid; the market moved 40 bps > 30 bps.
        let rig = rig();
        let order = PaperOrder {
            ref_mid: Some(347.195),
            ..buy(OrderSize::NotionalUsd(1_000.0))
        };
        let pf = fill(&rig, &both(), &order, 0.5).await.unwrap();
        let r = &pf.result;
        assert_eq!(r.status, FillStatus::Rejected);
        assert_eq!(r.reason, Some(FillReason::MarketOrderNoLiquidity));
        assert_eq!((r.order_qty, r.bound_px), (Some(2.88), Some(348.23)));
        assert!(
            r.message.as_deref().unwrap().contains("best 348.62"),
            "{r:?}"
        );
        assert!(pf.book.is_some(), "the book it missed is kept");
    }

    #[tokio::test]
    async fn a_book_that_is_old_after_the_latency_is_refused() {
        // The only book is the fixture; the order lands 9 s after it.
        let rig = rig_with(FIXTURE_TS + 8_750, vec![(0, tsla_book())]);
        let pf = fill(&rig, &both(), &buy(OrderSize::NotionalUsd(1_000.0)), 0.5)
            .await
            .unwrap();
        assert_eq!(pf.result.reason, Some(FillReason::StaleBook));
        assert_eq!(pf.result.book_age_ms, Some(9_000));
    }

    #[tokio::test]
    async fn a_failed_read_is_an_error_after_the_latency() {
        let Rig { clock, books } = rig_with(T0, vec![]);
        let rig = Rig {
            clock,
            books: books.failing(ReadError::new("hl_book", ErrorClass::Timeout, "slow")),
        };
        let e = fill(&rig, &both(), &buy(OrderSize::NotionalUsd(1_000.0)), 0.5)
            .await
            .unwrap_err();
        assert_eq!(e.class, ErrorClass::Timeout);
        assert_eq!(rig.books.reads(), [T0 + 250]);
    }

    #[tokio::test]
    async fn disabled_types_and_malformed_orders_refuse_without_latency() {
        let rig = rig();
        let ioc = PaperOrder {
            kind: OrderKind::Limit,
            limit_px: Some(347.3),
            ..buy(OrderSize::Qty(1.0))
        };
        let pf = fill(&rig, &paper(vec![OrderType::Market]), &ioc, 0.5)
            .await
            .unwrap();
        assert_eq!(pf.result.reason, Some(FillReason::OrderType));
        assert_eq!(
            pf.result.message.as_deref(),
            Some("ioc orders are off ([paper] order_types = [\"market\"])")
        );
        assert_eq!((pf.latency_ms, pf.book.is_none()), (0, true));
        assert_eq!(pf.result.book_age_ms, None);
        assert_eq!((rig.clock.now_ms(), rig.books.reads().len()), (T0, 0));
        // Market orders pass the same config.
        let ok = fill(
            &rig,
            &paper(vec![OrderType::Market]),
            &buy(OrderSize::Qty(1.0)),
            0.5,
        )
        .await
        .unwrap();
        assert_eq!(ok.result.status, FillStatus::Filled);
        let reads = rig.books.reads().len();
        let bad = fill(&rig, &both(), &buy(OrderSize::Qty(-1.0)), 0.5)
            .await
            .unwrap();
        assert_eq!(bad.result.reason, Some(FillReason::InvalidOrder));
        assert_eq!(rig.books.reads().len(), reads, "no book read");
        assert_eq!(order_type(&ioc), OrderType::Ioc);
    }
}
