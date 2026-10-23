//! Tests of `src/adapters/outbound/tools/xm/exits.rs`.

use std::collections::BTreeSet;
use std::sync::Mutex;

use serde_json::json;

use super::*;
use crate::adapters::outbound::tools::xm::exec_common::tests::{market_rows, Rig, NOW, TSLA};
use crate::application::observe::tests::MemStore;
use crate::domain::book::fixture::tsla_book;
use crate::domain::book::{L2Book, L2Level};
use crate::domain::market::{InstrumentId, MarketCtx};
use crate::domain::observation::{ErrorClass, ObsSource, ObsStatus, ReadError};
use crate::domain::xm::risk::CheckStatus;
use crate::ports::book::ScriptedBooks;
use crate::ports::observation::ObservationStore;

fn tool(shared: &XmShared) -> XmExitsTool {
    XmExitsTool {
        def: defs::def(names::XM_EXITS),
        shared: shared.clone(),
    }
}

fn io<'a>(rig: &'a Rig, books: &'a ScriptedBooks) -> ExecIo<'a> {
    ExecIo {
        clock: rig.clock.as_ref(),
        books,
        rand01: 0.5,
    }
}

/// Run `xm_exits` on the rig's clock and book as call `call_id`.
async fn run(rig: &Rig, call_id: &str) -> Observation {
    tool(&rig.shared)
        .check(&json!({}), &rig.ctx(Some(call_id)), &io(rig, &rig.books))
        .await
        .unwrap()
}

fn exits(o: &Observation) -> XmExits {
    o.typed().unwrap()
}

/// A $20 TSLA buy (0.057 at 347.23) whose position is due at `exit_at_ms`.
async fn open(rig: &Rig, exit_at_ms: Option<i64>) -> i64 {
    let mut buy = rig.buy(20.0);
    buy.exit_at_ms = exit_at_ms;
    let o = rig.run(buy, "entry:s:1").await.unwrap();
    assert_eq!(o.status, ObsStatus::Ok, "{}", o.render_text(NOW));
    let snap = rig.ledger.snapshot("xmarket", NOW + 1_000).await.unwrap();
    snap.account.positions[TSLA].opened_ms.unwrap()
}

/// `(client_order_id, status, reduce_only)` of every stored order.
fn orders(rig: &Rig) -> Vec<(String, String, bool)> {
    let c = rusqlite::Connection::open(rig.dir.path().join("state/ledger.db")).unwrap();
    let mut s = c
        .prepare("SELECT client_order_id, status, reduce_only FROM orders ORDER BY id")
        .unwrap();
    s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// Past its deadline (stale mark and all) the position is closed under
/// `exit:<account>:<instrument>:deadline:<opened_ms>`, once: the next
/// run finds nothing open.
#[tokio::test]
async fn a_position_past_its_deadline_is_closed_once() {
    let rig = Rig::new(25).await;
    let opened = open(&rig, Some(NOW + 1_000)).await;
    // Not due yet.
    let o = run(&rig, "feed:xm_exits:1:0").await;
    let e = exits(&o);
    assert_eq!((e.n_due(), e.positions[0].status), (0, ExitStatus::Held));
    assert_eq!(o.status, ObsStatus::Ok, "{}", o.render_text(NOW));
    rig.clock.set(NOW + 2_000);
    let o = run(&rig, "feed:xm_exits:2:0").await;
    let coid = format!("exit:xmarket:{TSLA}:deadline:{opened}");
    assert_eq!(o.key, "xm_exits/1:xmarket");
    assert_eq!(o.status, ObsStatus::Ok, "{}", o.render_text(NOW));
    assert_eq!(
        o.headline,
        format!("xm_exits account=xmarket open=1 due=1 closed=1 failed=0 deadline {TSLA} filled")
    );
    let c = &exits(&o).positions[0];
    assert_eq!(
        (c.reason, c.status, c.attempt, c.client_order_id.as_deref()),
        (
            Some(ExitReason::Deadline),
            ExitStatus::Filled,
            Some(1),
            Some(coid.as_str())
        )
    );
    assert_eq!(c.filled_qty, Some(0.057));
    for (k, v) in [
        ("n_open", 1),
        ("n_due", 1),
        ("n_closed", 1),
        ("n_failed", 0),
    ] {
        assert_eq!(o.features[k], v, "{k}");
    }
    assert_eq!(
        orders(&rig),
        [
            ("entry:s:1".to_string(), "filled".to_string(), false),
            (coid.clone(), "filled".to_string(), true)
        ]
    );
    // The verdict names the tool and the feed's call.
    let line = rig.risk_lines().pop().unwrap();
    assert_eq!(
        (&line["tool"], &line["call_id"], &line["client_order_id"]),
        (
            &json!("xm_exits"),
            &json!("feed:xm_exits:2:0"),
            &json!(coid)
        )
    );
    // A retry of the same feed call: nothing open, nothing placed.
    let again = run(&rig, "feed:xm_exits:2:0").await;
    assert_eq!(again.features["n_open"], 0);
    assert_eq!(orders(&rig).len(), 2);
    assert_eq!(rig.risk_lines().len(), 2);
}

/// Take-profit fires on a fresh mark. The same price on a stale mark
/// fires nothing by itself: TP / SL are judged on the live book instead
/// (here at the entry: held, nothing placed, no verdict; the stale mark
/// still named in `errors`).
#[tokio::test]
async fn take_profit_needs_a_fresh_mark_or_the_book() {
    let rig = Rig::new(25).await;
    open(&rig, None).await;
    // +201 bps over the 347.23 entry, 30 s old: stale (ctx limit 20 s).
    for r in market_rows(354.2, NOW - 30_000) {
        rig.store.put(&r).await.unwrap();
    }
    let o = run(&rig, "x:1").await;
    let e = exits(&o);
    assert_eq!(o.status, ObsStatus::Partial);
    assert_eq!((e.n_due(), e.n_stale_marks(), e.n_book_marks()), (0, 1, 1));
    let c = &e.positions[0];
    assert!(c.mark_px.is_error());
    assert_eq!(c.status, ExitStatus::Held);
    let mid = c.book_mid.unwrap();
    assert!(
        (mid - 347.195).abs() < 1e-9,
        "the fixture book's mid: {mid}"
    );
    assert!(c.pnl_bps.unwrap() < 0.0, "at the book mid: {c:?}");
    assert_eq!(o.errors[0].field, format!("mark:{TSLA}"));
    assert_eq!(orders(&rig).len(), 1, "nothing closed");
    assert_eq!(
        rig.rows()["risk_decisions"],
        1,
        "nothing judged by the gate"
    );
    // The same price, fresh: take-profit. The close fills at the book.
    for r in market_rows(354.2, rig.clock.now_ms()) {
        rig.store.put(&r).await.unwrap();
    }
    let o = run(&rig, "x:2").await;
    let c = &exits(&o).positions[0];
    assert_eq!(
        (c.reason, c.status),
        (Some(ExitReason::TakeProfit), ExitStatus::Filled)
    );
    assert!(c.pnl_bps.unwrap() > 200.0, "{:?}", c.pnl_bps);
    assert!(c.triggered_ms.is_some(), "recorded before the close");
    assert!(c
        .client_order_id
        .as_deref()
        .unwrap()
        .starts_with(&format!("exit:xmarket:{TSLA}:take_profit:")));
}

/// A two-level TSLA book around `mid`, 10 deep, stamped at the rig
/// clock's now.
fn book_at(rig: &Rig, mid: f64) -> ScriptedBooks {
    let level = |px: f64| L2Level { px, sz: 10.0, n: 1 };
    let book = L2Book::new(
        vec![level(mid - 0.05)],
        vec![level(mid + 0.05)],
        rig.clock.now_ms(),
    )
    .unwrap();
    ScriptedBooks::new(
        rig.clock.clone(),
        InstrumentId::parse(TSLA).unwrap(),
        vec![(0, book)],
    )
}

/// Review #6: with a stale mark the stop-loss is judged on the mid of
/// the live book the close reads after its latency — a failed read
/// judges nothing (held, nothing placed); 122 bps under the entry it
/// fires, is recorded, and closes under the stop-loss id.
#[tokio::test]
async fn a_stop_loss_on_a_stale_mark_fires_on_the_live_book() {
    let rig = Rig::new(25).await;
    let opened = open(&rig, None).await;
    // The rig's rows are from NOW − 1 s: 61 s old, stale (limit 20 s).
    rig.clock.set(NOW + 60_000);
    let o = tool(&rig.shared)
        .check(&json!({}), &rig.ctx(Some("b:1")), &io(&rig, &no_book(&rig)))
        .await
        .unwrap();
    let c = &exits(&o).positions[0];
    assert_eq!(
        (c.status, c.reason, c.book_mid),
        (ExitStatus::Held, None, None)
    );
    assert!(c.error.as_deref().unwrap().contains("not judged"), "{c:?}");
    assert_eq!(orders(&rig).len(), 1, "nothing placed");
    let books = book_at(&rig, 343.0);
    let o = tool(&rig.shared)
        .check(&json!({}), &rig.ctx(Some("b:2")), &io(&rig, &books))
        .await
        .unwrap();
    let c = &exits(&o).positions[0];
    assert_eq!(
        (c.reason, c.status, c.book_mid),
        (Some(ExitReason::StopLoss), ExitStatus::Filled, Some(343.0)),
        "{c:?}"
    );
    assert!(c.mark_px.is_error() && c.triggered_ms.is_some(), "{c:?}");
    assert!(c.pnl_bps.unwrap() < -100.0, "{c:?}");
    let id = format!("exit:xmarket:{TSLA}:stop_loss:{opened}");
    assert_eq!(c.client_order_id.as_deref(), Some(id.as_str()));
    for (k, v) in [("n_book_marks", 1), ("n_closed", 1), ("n_stale_marks", 1)] {
        assert_eq!(o.features[k], v, "{k}");
    }
    let line = rig.risk_lines().pop().unwrap();
    assert_eq!(
        (&line["client_order_id"], &line["tool"]),
        (&json!(id), &json!("xm_exits"))
    );
    let snap = rig
        .ledger
        .snapshot("xmarket", rig.clock.now_ms())
        .await
        .unwrap();
    assert!(snap.account.positions[TSLA].is_flat());
}

/// Review #6: a stop-loss that fired and was rejected stays due — on a
/// stale mark (the backoff holds it, never `held`) and with the price
/// back inside the band (attempt 2 closes it).
#[tokio::test]
async fn a_fired_stop_loss_stays_due_until_the_position_closes() {
    let rig = Rig::new(25).await;
    let opened = open(&rig, None).await;
    rig.clock.set(NOW + 1_000);
    for r in market_rows(343.0, NOW + 900) {
        rig.store.put(&r).await.unwrap();
    }
    let o = tool(&rig.shared)
        .check(&json!({}), &rig.ctx(Some("t:1")), &io(&rig, &no_book(&rig)))
        .await
        .unwrap();
    let c = &exits(&o).positions[0];
    assert_eq!(
        (c.reason, c.status, c.attempt),
        (Some(ExitReason::StopLoss), ExitStatus::Rejected, Some(1))
    );
    let fired = rig
        .ledger
        .snapshot("xmarket", NOW + 2_000)
        .await
        .unwrap()
        .exit_triggers[TSLA];
    assert_eq!(fired.reason, ExitReason::StopLoss);
    assert_eq!(c.triggered_ms, Some(fired.at_ms));
    // The mark goes stale: still due, held back by the backoff.
    rig.clock.set(NOW + 5_000);
    for r in market_rows(343.0, NOW - 60_000) {
        rig.store.put(&r).await.unwrap();
    }
    let o = run(&rig, "t:2").await;
    let c = &exits(&o).positions[0];
    assert_eq!(
        (c.reason, c.status, c.book_mid),
        (Some(ExitReason::StopLoss), ExitStatus::Backoff, None)
    );
    assert!(c.mark_px.is_error());
    // The price back inside the band, fresh: still due — attempt 2.
    rig.clock.set(NOW + 20_000);
    for r in market_rows(350.0, NOW + 19_900) {
        rig.store.put(&r).await.unwrap();
    }
    let books = fresh_books(&rig);
    let o = tool(&rig.shared)
        .check(&json!({}), &rig.ctx(Some("t:3")), &io(&rig, &books))
        .await
        .unwrap();
    let c = &exits(&o).positions[0];
    let base = format!("exit:xmarket:{TSLA}:stop_loss:{opened}");
    assert_eq!(
        (c.reason, c.status, c.client_order_id.clone()),
        (
            Some(ExitReason::StopLoss),
            ExitStatus::Filled,
            Some(format!("{base}:2"))
        )
    );
    assert!(c.pnl_bps.unwrap() > 0.0, "the mark says no stop: {c:?}");
    assert_eq!(c.triggered_ms, Some(fired.at_ms), "fired once");
}

/// Review #6: the instrument row purged (the store keeps 7 days) or no
/// store at all, a close still fills on the venue facts kept with the
/// position at its entry; an entry still needs the row, and so does a
/// position from before the kept facts.
#[tokio::test]
async fn a_close_without_the_instrument_row_uses_the_kept_facts() {
    let rig = Rig::new(25).await;
    open(&rig, Some(NOW + 500)).await;
    let kept = rig
        .ledger
        .snapshot("xmarket", NOW + 600)
        .await
        .unwrap()
        .facts[TSLA];
    assert_eq!((kept.sz_decimals, kept.at_ms), (3, NOW - 1_000));
    assert!((kept.fees.taker_bps - 0.9).abs() < 1e-12, "{kept:?}");
    // Purged: the store holds the ctx row only.
    let purged = Arc::new(MemStore::default());
    purged.put(&market_rows(347.2, NOW + 500)[0]).await.unwrap();
    let mut gone = rig.shared.clone();
    gone.store = Some(purged as Arc<dyn ObservationStore>);
    rig.clock.set(NOW + 1_000);
    let books = fresh_books(&rig);
    let o = tool(&gone)
        .check(&json!({}), &rig.ctx(Some("p:1")), &io(&rig, &books))
        .await
        .unwrap();
    let c = &exits(&o).positions[0];
    assert_eq!(
        (c.reason, c.status),
        (Some(ExitReason::Deadline), ExitStatus::Filled),
        "{c:?}"
    );
    let id = c.client_order_id.clone().unwrap();
    let r = rig
        .ledger
        .order("xmarket", &id)
        .await
        .unwrap()
        .unwrap()
        .result;
    let fee = r.filled_notional_usd * 0.9e-4;
    assert!((r.fee_usd - fee).abs() < 1e-12, "the kept fee: {r:?}");
    // An entry on that store is refused: it needs the row.
    let e = run_exec(
        &gone,
        &rig.ctx(Some("p:2")),
        &io(&rig, &books),
        rig.buy(20.0),
    )
    .await
    .unwrap_err();
    assert!(e.to_string().starts_with("missing:mkt_instrument"), "{e}");
    // No store at all: a close still fills.
    rig.clock.set(NOW + 2_000);
    rig.run(rig.buy(20.0), "entry:s:2").await.unwrap();
    let mut blind = rig.shared.clone();
    blind.store = None;
    let books = fresh_books(&rig);
    let o = run_exec(
        &blind,
        &rig.ctx(Some("n:1")),
        &io(&rig, &books),
        rig.close(),
    )
    .await
    .unwrap();
    let r: PaperFillRow = o.typed().unwrap();
    assert_eq!(
        (r.fill.unwrap().status, r.position_qty_after),
        (FillStatus::Filled, 0.0)
    );
    // A position without kept facts (an older ledger) needs the row.
    rig.clock.set(NOW + 3_000);
    rig.run(rig.buy(20.0), "entry:s:3").await.unwrap();
    rusqlite::Connection::open(rig.dir.path().join("state/ledger.db"))
        .unwrap()
        .execute_batch("UPDATE positions SET sz_decimals = NULL")
        .unwrap();
    let e = run_exec(&gone, &rig.ctx(Some("p:3")), &io(&rig, &books), rig.close())
        .await
        .unwrap_err();
    assert!(e.to_string().starts_with("missing:mkt_instrument"), "{e}");
}

/// The fixture book stamped at the rig clock's now (a fresh read).
fn fresh_books(rig: &Rig) -> ScriptedBooks {
    let mut book = tsla_book();
    book.venue_ts_ms = rig.clock.now_ms();
    ScriptedBooks::new(
        rig.clock.clone(),
        InstrumentId::parse(TSLA).unwrap(),
        vec![(0, book)],
    )
}

/// A book read that times out after the latency.
fn no_book(rig: &Rig) -> ScriptedBooks {
    ScriptedBooks::new(
        rig.clock.clone(),
        InstrumentId::parse(TSLA).unwrap(),
        vec![],
    )
    .failing(ReadError::new("book", ErrorClass::Timeout, "slow"))
}

/// A close rejected for lack of a book waits 15 s (`backoff`), then is
/// retried under the next attempt id; the first id is never reused.
#[tokio::test]
async fn a_rejected_exit_is_retried_under_the_next_attempt() {
    let rig = Rig::new(25).await;
    let opened = open(&rig, Some(NOW + 500)).await;
    rig.clock.set(NOW + 1_000);
    let no_book = no_book(&rig);
    let o = tool(&rig.shared)
        .check(&json!({}), &rig.ctx(Some("f:1")), &io(&rig, &no_book))
        .await
        .unwrap();
    let c = &exits(&o).positions[0];
    assert_eq!((c.status, c.attempt), (ExitStatus::Rejected, Some(1)));
    assert!(
        c.error
            .as_deref()
            .unwrap()
            .starts_with("rejected stale_book"),
        "{c:?}"
    );
    assert_eq!(o.status, ObsStatus::Error, "every due close failed");
    assert_eq!(o.features["n_failed"], 1);
    let base = format!("exit:xmarket:{TSLA}:deadline:{opened}");
    let rejected_at = rig
        .ledger
        .order("xmarket", &base)
        .await
        .unwrap()
        .unwrap()
        .ts_ms;
    // The next run, too soon: held back, nothing placed.
    let o = run(&rig, "f:2").await;
    let c = &exits(&o).positions[0];
    assert_eq!(
        (c.status, c.attempt, c.next_attempt_ms),
        (ExitStatus::Backoff, Some(1), Some(rejected_at + 15_000))
    );
    assert!(
        c.error.as_deref().unwrap().contains("1 rejected in a row"),
        "{c:?}"
    );
    assert_eq!(orders(&rig).len(), 2);
    // 15 s later, the book back: attempt 2 closes it.
    rig.clock.set(rejected_at + 15_000);
    let books = fresh_books(&rig);
    let o = tool(&rig.shared)
        .check(&json!({}), &rig.ctx(Some("f:3")), &io(&rig, &books))
        .await
        .unwrap();
    let c = &exits(&o).positions[0];
    assert_eq!(
        (c.status, c.client_order_id.clone()),
        (ExitStatus::Filled, Some(format!("{base}:2")))
    );
    let ids: Vec<(String, String)> = orders(&rig)
        .into_iter()
        .skip(1)
        .map(|(id, status, _)| (id, status))
        .collect();
    assert_eq!(
        ids,
        [
            (base.clone(), "rejected".to_string()),
            (format!("{base}:2"), "filled".to_string())
        ]
    );
}

/// A gate denial stores no order, so the next run judges the same id
/// again; a caller that is not private is refused before anything.
#[tokio::test]
async fn a_denied_exit_keeps_its_id_and_callers_are_checked() {
    let rig = Rig::new(25).await;
    open(&rig, Some(NOW + 500)).await;
    rig.clock.set(NOW + 1_000);
    std::fs::write(rig.dir.path().join("KILL"), "").unwrap();
    let mut strict = rig.shared.clone();
    strict.risk.as_mut().unwrap().allow_reduce_degraded = false;
    for call in ["k:1", "k:2"] {
        let o = tool(&strict)
            .check(&json!({}), &rig.ctx(Some(call)), &io(&rig, &rig.books))
            .await
            .unwrap();
        let c = &exits(&o).positions[0];
        assert_eq!(
            (c.status, c.attempt, c.rule.as_deref()),
            (ExitStatus::Denied, Some(1), Some("kill_switch"))
        );
    }
    assert_eq!(orders(&rig).len(), 1, "denials store no order");
    assert_eq!(rig.rows()["risk_decisions"], 3, "two denied verdicts");
    // Refusals: a routable caller, an unknown argument, no [risk].
    let mut routable = Rig::new(25).await;
    routable.agent.description = Some("routable".into());
    let e = tool(&routable.shared)
        .check(
            &json!({}),
            &routable.ctx(Some("r:1")),
            &io(&routable, &routable.books),
        )
        .await
        .unwrap_err();
    assert!(e.to_string().starts_with("exec_agent_not_private"), "{e}");
    let e = tool(&rig.shared)
        .check(
            &json!({"all": true}),
            &rig.ctx(Some("r:2")),
            &io(&rig, &rig.books),
        )
        .await
        .unwrap_err();
    assert!(
        e.to_string().contains("unknown argument(s) [\"all\"]"),
        "{e}"
    );
    let mut none = rig.shared.clone();
    none.risk = None;
    let e = tool(&none)
        .check(&json!({}), &rig.ctx(Some("r:3")), &io(&rig, &rig.books))
        .await
        .unwrap_err();
    assert!(e.to_string().starts_with("risk_config_missing"), "{e}");
    // Review #9: the IOC bound of an exit is at most 500 bps.
    let e = tool(&rig.shared)
        .check(
            &json!({"max_slippage_bps": 501}),
            &rig.ctx(Some("r:4")),
            &io(&rig, &rig.books),
        )
        .await
        .unwrap_err();
    assert!(e.to_string().contains("≤ 500"), "{e}");
}

/// Review #3, the reviewer's probe asserting the fix: a close that keeps
/// failing (no book after the latency), run every 5 s, is placed 15 s,
/// then 30 s … apart — `backoff` in between; no run is denied
/// `order_rate`, the exits never count toward it, and an unrelated entry
/// afterwards passes the rate rule. Each run is a new tool: the backoff
/// comes from the ledger (a restart keeps it).
#[tokio::test]
async fn a_failing_exit_backs_off_and_never_uses_the_order_budget() {
    let rig = Rig::new(25).await;
    open(&rig, Some(NOW + 500)).await;
    let no_book = no_book(&rig);
    let mut runs = Vec::new();
    for i in 0..7i64 {
        rig.clock.set(NOW + 1_000 + i * 5_000);
        let o = tool(&rig.shared)
            .check(
                &json!({}),
                &rig.ctx(Some(&format!("f:{i}"))),
                &io(&rig, &no_book),
            )
            .await
            .unwrap();
        let c = &exits(&o).positions[0];
        runs.push((c.status, c.rule.clone(), c.attempt));
    }
    use ExitStatus::{Backoff, Rejected};
    let statuses: Vec<ExitStatus> = runs.iter().map(|r| r.0).collect();
    // Attempt 1 at +1.25 s; attempt 2 at +21.25 s (≥ 15 s later); the
    // third waits 30 s.
    assert_eq!(
        statuses,
        [Rejected, Backoff, Backoff, Backoff, Rejected, Backoff, Backoff],
        "{runs:?}"
    );
    assert_eq!(runs[4].2, Some(2));
    assert!(
        runs.iter()
            .all(|(_, rule, _)| rule.as_deref() != Some("order_rate")),
        "{runs:?}"
    );
    assert_eq!(orders(&rig).len(), 3, "the entry + two exit attempts");
    // An unrelated entry: fresh rows and book; the rate rule passes.
    let now = rig.clock.now_ms();
    for r in market_rows(347.2, now - 500) {
        rig.store.put(&r).await.unwrap();
    }
    let books = fresh_books(&rig);
    let o = run_exec(
        &rig.shared,
        &rig.ctx(Some("entry:s:2")),
        &io(&rig, &books),
        rig.buy(20.0),
    )
    .await
    .unwrap();
    let r: PaperFillRow = o.typed().unwrap();
    assert_eq!(r.gate.rule, "ok", "{:?}", r.gate);
    let d = rig.ledger.decisions("xmarket", 1).await.unwrap();
    let rate = d[0].verdict.check("order_rate").unwrap();
    assert_eq!(rate.status, CheckStatus::Pass, "{rate:?}");
    assert!(rate.detail.starts_with("1 entries"), "{rate:?}");
}

/// Review #3: an exit rejected for a final reason (`delisted`) is never
/// placed again — `stuck` on every later run, also after a restart (a
/// new tool on the same ledger); one stored attempt and one verdict line
/// stay the operator's marker.
#[tokio::test]
async fn a_final_rejection_stops_the_retries() {
    let rig = Rig::new(25).await;
    let opened = open(&rig, Some(NOW + 500)).await;
    rig.clock.set(NOW + 1_000);
    let gone = MarketCtx::not_found(InstrumentId::parse(TSLA).unwrap(), NOW + 900);
    rig.store
        .put(&Observation::of(
            names::HL_CTX,
            &gone,
            NOW + 900,
            5_000,
            ObsSource::Live,
        ))
        .await
        .unwrap();
    let o = run(&rig, "d:1").await;
    let c = &exits(&o).positions[0];
    assert_eq!((c.status, c.attempt), (ExitStatus::Rejected, Some(1)));
    assert!(
        c.error.as_deref().unwrap().starts_with("rejected delisted"),
        "{c:?}"
    );
    let base = format!("exit:xmarket:{TSLA}:deadline:{opened}");
    rig.clock.set(NOW + 3_600_000);
    for call in ["d:2", "d:3"] {
        let o = run(&rig, call).await;
        let c = &exits(&o).positions[0];
        assert_eq!(
            (c.status, c.attempt, c.client_order_id.as_deref()),
            (ExitStatus::Stuck, Some(1), Some(base.as_str()))
        );
        assert!(
            c.error.as_deref().unwrap().contains("`delisted` is final"),
            "{c:?}"
        );
        assert_eq!(o.features["n_stuck"], 1);
        assert_eq!(o.status, ObsStatus::Error, "the position stays open");
    }
    assert_eq!(orders(&rig).len(), 2, "the entry + one exit attempt");
    assert_eq!(rig.risk_lines().len(), 2, "one verdict line for the exit");
}

/// The attempt search: first unstored attempt, O(log k) reads, never a
/// stored one.
#[tokio::test]
async fn the_attempt_search_finds_the_first_unstored_id() {
    for (stored, want, max_reads) in [
        (BTreeSet::new(), 1, 1),
        (BTreeSet::from([1]), 2, 3),
        ((1..=5).collect::<BTreeSet<u32>>(), 6, 6),
        ((1..=1_000).collect(), 1_001, 21),
        // Not a prefix (an id placed by hand): still an unstored one.
        (BTreeSet::from([1, 2, 5]), 3, 5),
    ] {
        let reads = Mutex::new(0);
        let got = first_unstored(|n| {
            *reads.lock().unwrap() += 1;
            let hit = stored.contains(&n);
            async move { Ok(hit) }
        })
        .await
        .unwrap();
        assert_eq!(got, want, "{stored:?}");
        assert!(!stored.contains(&got));
        let n = *reads.lock().unwrap();
        assert!(n <= max_reads, "{n} reads for {stored:?}");
    }
}

#[tokio::test]
async fn execute_gates_the_scope_first() {
    let rig = Rig::new(25).await;
    let mut denied = rig.ctx(Some("s:1"));
    let no_fs = crate::domain::scope::ToolScope::default();
    denied.scope = &no_fs;
    let t = tool(&rig.shared);
    let e = t.execute(&json!({}), &denied).await.unwrap_err();
    assert!(e.to_string().contains("fs"), "{e}");
    assert_eq!(t.definition().name, "xm_exits");
}
