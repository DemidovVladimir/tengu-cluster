//! Tests of `src/adapters/outbound/tools/xm/weekend_fade.rs`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use serde_json::json;

use super::*;
use crate::adapters::outbound::tools::xm::exec_common::tests::Rig;
use crate::config::xmarket::XmarketConfig;
use crate::domain::book::{L2Book, L2Level};
use crate::domain::market::{InstrumentKind, Listing, MarketInstrument, QuoteCcy};
use crate::domain::observation::{ObsStatus, ReadError};
use crate::domain::xm::weekend_fade::{PriceSource, ENTRY_WAIT_MS};
use crate::ports::book::{BookRead, BookSource};
use crate::ports::history::HistoryRow;

const TSLA: &str = "hyperliquid:xyz:TSLA";
const NVDA: &str = "hyperliquid:xyz:NVDA";
const AAPL: &str = "hyperliquid:xyz:AAPL";
const KIOXIA: &str = "hyperliquid:xyz:KIOXIA";
const AMD: &str = "hyperliquid:xyz:AMD";
const ACCOUNT: &str = "fade";
const SHADOW: &str = "fade-shadow";
/// Fri 2026-10-02 20:00 EDT / Sun 10-04 18:00 EDT / Mon 10-05 09:00 EDT.
const ANCHOR: i64 = 1_790_985_600_000;
const ENTRY: i64 = 1_791_151_200_000;
const EXIT: i64 = 1_791_205_200_000;
const DATE: &str = "2026-10-02";

const CALENDARS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/xmarket/calendars.toml"
));

/// NYSE from the calendar fixture (= `config.example.toml`).
fn nyse() -> ExchangeCalendar {
    #[derive(serde::Deserialize)]
    struct Fixture {
        xmarket: XmarketConfig,
    }
    let x = toml::from_str::<Fixture>(CALENDARS).unwrap().xmarket;
    x.calendars()["us_equity"].exchange().unwrap().clone()
}

/// `[risk]` with room for the capped fades: every universe name
/// allowed, ctx rows up to 2 min old, leverage 2.
fn risk(kill: &std::path::Path) -> RiskConfig {
    let toml = format!(
        r#"
account = "{ACCOUNT}"
mode = "paper"
venues = ["hyperliquid"]
min_lifecycle = "paper_tradable"
instruments_allow = ["{TSLA}", "{NVDA}", "{AAPL}", "{KIOXIA}", "{AMD}"]
instruments_deny = []
max_order_notional_usd = 25
max_position_notional_usd = 50
max_asset_exposure_usd = 50
max_venue_exposure_usd = 100
max_gross_exposure_usd = 100
max_net_exposure_usd = 100
max_leverage = 2
daily_loss_limit_usd = 10
total_loss_limit_usd = 25
min_edge_bps = 10
max_slippage_bps = 30
min_depth_usd = 250
require_hedge_for = []
max_skew_ms = 5000
max_orders_per_min = 8
max_open_orders = 4
kill_switch_file = "{}"
allow_reduce_degraded = true
exits = {{ take_profit_bps = 2000, stop_loss_bps = 1000, max_hold_secs = 172800 }}
max_data_age_ms = {{ book = 5000, ctx = 120000, reference = 60000, quote = 20000 }}
"#,
        kill.display()
    );
    toml::from_str::<RiskConfig>(&toml).unwrap().resolved()
}

fn fade_config() -> WeekendFadeConfig {
    toml::from_str(&format!(
        r#"
calendar = "us_equity"
universe = ["{TSLA}", "{NVDA}", "{AAPL}", "{KIOXIA}", "{AMD}"]
exclude = ["{KIOXIA}"]
capped_top_n = 2
min_abs_signal_bps = 50
capped_notional_usd = 20
shadow_account = "{SHADOW}"
shadow_initial_cash_usd = 10000
shadow_notional_usd = 100
expected_edge_bps = 23
anchor_max_age_secs = 600
entry_max_age_secs = 120
entry_lateness_max_secs = 600
max_slippage_bps = 50
"#
    ))
    .unwrap()
}

/// A book 0.05 either side of the mid, 1 000 deep, stamped now; a
/// failing id answers a timeout, a one-sided id has bids only.
struct FadeBooks {
    clock: Arc<crate::ports::clock::ManualClock>,
    mids: Mutex<BTreeMap<String, f64>>,
    failing: Mutex<BTreeSet<String>>,
    one_sided: Mutex<BTreeSet<String>>,
    reads: Mutex<Vec<String>>,
}

#[async_trait]
impl BookSource for FadeBooks {
    async fn fresh_book(&self, id: &InstrumentId) -> std::result::Result<BookRead, ReadError> {
        let key = id.to_string();
        self.reads.lock().unwrap().push(key.clone());
        if self.failing.lock().unwrap().contains(&key) {
            return Err(ReadError::new("hl_book", ErrorClass::Timeout, "slow"));
        }
        let mid = *self.mids.lock().unwrap().get(&key).ok_or_else(|| {
            ReadError::new(
                "hl_book",
                ErrorClass::NotApplicable,
                format!("no book {key}"),
            )
        })?;
        let now = self.clock.now_ms();
        let level = |px: f64| L2Level {
            px,
            sz: 1_000.0,
            n: 1,
        };
        let asks = if self.one_sided.lock().unwrap().contains(&key) {
            vec![]
        } else {
            vec![level(mid + 0.05)]
        };
        Ok(BookRead {
            book: L2Book::new(vec![level(mid - 0.05)], asks, now).unwrap(),
            observed_at_ms: now,
        })
    }
}

/// `mkt_ctx/1` (mid = mark = oracle = `px`) + `mkt_instrument/1` of `id`.
fn ctx_rows(id: &str, px: f64, at_ms: i64) -> Vec<Observation> {
    rows_with(id, px, at_ms, Some(false), 3)
}

/// [`ctx_rows`] with the OI-cap state as `hl_ctx` wrote it (`None`: the
/// `perpsAtOpenInterestCap` read failed) and the lot size.
fn rows_with(
    id: &str,
    px: f64,
    at_ms: i64,
    at_oi_cap: Option<bool>,
    sz_decimals: u32,
) -> Vec<Observation> {
    let iid = InstrumentId::parse(id).unwrap();
    let mut c = MarketCtx::new(iid.clone(), at_ms);
    c.mark = Field::ok(px);
    c.oracle = Field::ok(px);
    c.mid = Field::ok(px);
    c.funding_1h = Field::ok(0.0001);
    c.at_oi_cap = at_oi_cap;
    let mut i = MarketInstrument::new(iid, InstrumentKind::Perp, Listing::Listed);
    i.sz_decimals = Some(sz_decimals);
    i.quote_ccy = Some(QuoteCcy::Usdc);
    i.deployer_fee_scale = Some(1.0);
    i.growth_mode = Some(true);
    i.at_oi_cap = at_oi_cap;
    vec![
        Observation::of(names::HL_CTX, &c, at_ms, 5_000, ObsSource::Live),
        Observation::of(names::HL_CTX, &i, at_ms, 60_000, ObsSource::Live),
    ]
}

struct Fade {
    rig: Rig,
    books: FadeBooks,
    history: Arc<crate::adapters::outbound::history_sqlite::SqliteHistoryStore>,
    tool: XmWeekendFadeTool,
}

impl Fade {
    /// The paper rig with `[risk]` + `[xmarket.weekend_fade]` above, a
    /// recorder history, books for every name; clock at `now`.
    async fn new(now: i64) -> Fade {
        let mut rig = Rig::new(25).await;
        let history = Arc::new(
            crate::adapters::outbound::history_sqlite::SqliteHistoryStore::open(
                &rig.dir.path().join("history"),
                0,
            )
            .unwrap(),
        );
        rig.shared.risk = Some(risk(&rig.dir.path().join("KILL")));
        rig.shared.history = Some(history.clone() as Arc<dyn HistoryStore>);
        rig.shared.fade = Some(Arc::new(FadeSetup {
            config: fade_config(),
            calendar: Ok(nyse()),
        }));
        rig.clock.set(now);
        let books = FadeBooks {
            clock: rig.clock.clone(),
            mids: Mutex::new(BTreeMap::new()),
            failing: Mutex::new(BTreeSet::new()),
            one_sided: Mutex::new(BTreeSet::new()),
            reads: Mutex::new(Vec::new()),
        };
        let tool = XmWeekendFadeTool {
            def: defs::def(names::XM_WEEKEND_FADE),
            shared: rig.shared.clone(),
        };
        Fade {
            rig,
            books,
            history,
            tool,
        }
    }

    /// Anchor rows (60 s before the anchor) in the history, fresh entry
    /// rows in the store at `at_ms`, books at the entry prices.
    async fn market(&self, prices: &[(&str, Option<f64>, f64)], at_ms: i64) {
        for (id, anchor, entry) in prices {
            if let Some(a) = anchor {
                let rows: Vec<HistoryRow> = ctx_rows(id, *a, ANCHOR - 60_000)
                    .iter()
                    .map(|o| HistoryRow::of(o, false))
                    .collect();
                self.history.append(&rows).await.unwrap();
            }
            self.prices(id, *entry, at_ms).await;
        }
    }

    /// Fresh rows and the book of `id` at `px`.
    async fn prices(&self, id: &str, px: f64, at_ms: i64) {
        self.put(ctx_rows(id, px, at_ms)).await;
        self.books.mids.lock().unwrap().insert(id.to_string(), px);
    }

    async fn put(&self, rows: Vec<Observation>) {
        for row in rows {
            self.rig.store.put(&row).await.unwrap();
        }
    }

    /// The stored window row becomes `row` without its order outcomes:
    /// the snapshot as claimed — a crash after the fills but before the
    /// final save, or a concurrent call that saved last.
    async fn lose_outcomes(&self, row: &WeekendFade) {
        let mut claimed = row.clone();
        for n in &mut claimed.names {
            n.capped_order = None;
            n.shadow = None;
            n.capped_qty = None;
            n.shadow_qty = None;
        }
        let obs = Observation::of(
            names::XM_WEEKEND_FADE,
            &claimed,
            claimed.ts_ms,
            WINDOW_TTL_MS,
            ObsSource::Live,
        );
        self.rig.store.put(&obs).await.unwrap();
    }

    /// `xm_exits`' deadline close of every open capped position.
    async fn close_capped(&self, call: &str) {
        let now = self.rig.clock.now_ms();
        let snap = self.rig.ledger.snapshot(ACCOUNT, now).await.unwrap();
        for p in snap.account.open_positions() {
            let opened = p.opened_ms.unwrap();
            let order = ExecOrder {
                tool: names::XM_EXITS,
                gate: ExecGate::Risk,
                limits: self.rig.shared.risk.as_ref().unwrap().limits(),
                instrument: InstrumentId::parse(&p.instrument).unwrap(),
                side: Side::Sell,
                size: ExecSize::Close,
                kind: OrderKind::Market,
                limit_px: None,
                reduce_only: true,
                max_slippage_bps: 50.0,
                strategy: None,
                hedge_instrument: None,
                opportunity_key: None,
                client_order_id: Some(exit_client_order_id(
                    ACCOUNT,
                    &p.instrument,
                    ExitReason::Deadline,
                    opened,
                    1,
                )),
                exit_at_ms: None,
                tp_sl_on_book: None,
            };
            let io = ExecIo {
                clock: self.rig.clock.as_ref(),
                books: &self.books,
                rand01: 0.5,
            };
            run_exec(&self.rig.shared, &self.rig.ctx(Some(call)), &io, order)
                .await
                .unwrap();
        }
    }

    async fn step(&self, call: &str) -> Result<Observation> {
        let io = ExecIo {
            clock: self.rig.clock.as_ref(),
            books: &self.books,
            rand01: 0.5,
        };
        self.tool
            .step(&json!({}), &self.rig.ctx(Some(call)), &io)
            .await
    }

    /// `(account, client_order_id, side, status, exit_at_ms)` of every
    /// stored order, in order.
    fn orders(&self) -> Vec<(String, String, String, String, Option<i64>)> {
        let c = rusqlite::Connection::open(self.rig.dir.path().join("state/ledger.db")).unwrap();
        let mut s = c
            .prepare(
                "SELECT account, client_order_id, side, status, exit_at_ms FROM orders \
                     ORDER BY id",
            )
            .unwrap();
        s.query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect()
    }
}

fn fade_row(o: &Observation) -> WeekendFade {
    o.typed().unwrap()
}

fn name<'a>(row: &'a WeekendFade, id: &str) -> &'a FadeName {
    row.names.iter().find(|n| n.instrument == id).unwrap()
}

/// TSLA +198 bps and NVDA −100.5 bps (the capped two), AAPL +20 bps
/// (shadow only), KIOXIA excluded, AMD without an anchor.
const PRICES: [(&str, Option<f64>, f64); 5] = [
    (TSLA, Some(100.0), 102.0),
    (NVDA, Some(200.0), 198.0),
    (AAPL, Some(50.0), 50.1),
    (KIOXIA, Some(300.0), 100.0),
    (AMD, None, 150.0),
];

/// The window's whole life: waiting → the entry (capped + shadow, one
/// snapshot, a failed book placed again next call, never twice) → the
/// exit (shadow closed here, capped left to `xm_exits`) → closed with
/// the P&L → the next window waits.
#[tokio::test]
async fn a_window_from_waiting_to_closed() {
    let f = Fade::new(ENTRY - 3 * 86_400_000).await;
    let o = f.step("feed:xm_weekend_fade:1:0").await.unwrap();
    assert_eq!(o.key, format!("xm_weekend/1:{DATE}"));
    assert_eq!(o.status, ObsStatus::Ok, "{}", o.render_text(0));
    assert_eq!(
        o.headline,
        "xm_weekend 2026-10-02 waiting next_entry_s=259200 universe=5 excluded=1"
    );
    assert_eq!(o.ttl_ms, WINDOW_TTL_MS);
    assert!(f.orders().is_empty());
    assert!(f.rig.store.get(&o.key).await.unwrap().is_some(), "stored");

    // The entry, 30 s late; AAPL's book read times out once.
    f.rig.clock.set(ENTRY + 30_000);
    f.market(&PRICES, ENTRY + 20_000).await;
    f.books.failing.lock().unwrap().insert(AAPL.into());
    let o = f.step("feed:xm_weekend_fade:2:0").await.unwrap();
    let row = fade_row(&o);
    assert_eq!(row.phase, FadePhase::Entered);
    assert_eq!(o.status, ObsStatus::Partial, "AMD, AAPL: {:?}", o.errors);
    assert_eq!(
        o.headline,
        format!(
            "xm_weekend 2026-10-02 entered eligible=3/5 shadow=2 capped=2/2 \
                 missing_anchor=1 missing_entry=0 capped: {TSLA} {NVDA}"
        )
    );
    for (k, v) in [
        ("n_eligible", 3),
        ("n_capped", 2),
        ("n_capped_filled", 2),
        ("n_shadow_filled", 2),
        ("n_shadow_failed", 1),
        ("n_missing_anchor", 1),
        ("n_excluded", 1),
    ] {
        assert_eq!(o.features[k], v, "{k}");
    }
    let tsla = name(&row, TSLA);
    assert_eq!((tsla.side, tsla.capped), (Some(Side::Sell), true));
    assert_eq!(tsla.s_bps, Some((102.0f64 / 100.0).ln() * 10_000.0));
    assert_eq!(name(&row, NVDA).side, Some(Side::Buy));
    assert!(!name(&row, AAPL).capped);
    assert_eq!(name(&row, KIOXIA).skip, Some(Skip::Excluded));
    assert_eq!(name(&row, AMD).skip, Some(Skip::MissingAnchor));
    let aapl = name(&row, AAPL).shadow.clone().unwrap();
    assert_eq!(
        (aapl.status.as_str(), aapl.rule.as_deref()),
        ("denied", Some("missing:book"))
    );
    // The ledgers: capped (the [risk] gate) + shadow (its own gate),
    // deterministic ids, every position due at the exit.
    let capped_tsla = format!("fade:{ACCOUNT}:{TSLA}:{DATE}");
    let capped_nvda = format!("fade:{ACCOUNT}:{NVDA}:{DATE}");
    let shadow_tsla = format!("fade-shadow:{SHADOW}:{TSLA}:{DATE}");
    let shadow_nvda = format!("fade-shadow:{SHADOW}:{NVDA}:{DATE}");
    let shadow_aapl = format!("fade-shadow:{SHADOW}:{AAPL}:{DATE}");
    let s = |a: &str, id: &str, side: &str| {
        (
            a.to_string(),
            id.to_string(),
            side.to_string(),
            "filled".to_string(),
            Some(EXIT),
        )
    };
    // Capped first, in |s| order; the shadow fades run 4 at a time.
    let orders = f.orders();
    assert_eq!(
        orders[..2],
        [
            s(ACCOUNT, &capped_tsla, "sell"),
            s(ACCOUNT, &capped_nvda, "buy")
        ]
    );
    let mut shadow_orders = orders[2..].to_vec();
    shadow_orders.sort();
    assert_eq!(
        shadow_orders,
        [
            s(SHADOW, &shadow_nvda, "buy"),
            s(SHADOW, &shadow_tsla, "sell")
        ]
    );
    let lines = f.rig.risk_lines();
    assert_eq!(lines.len(), 5, "4 allowed + the AAPL denial");
    let shadow_line = lines
        .iter()
        .find(|l| l["client_order_id"] == shadow_tsla)
        .unwrap();
    assert_eq!(shadow_line["tool"], "xm_weekend_fade");
    assert_eq!(shadow_line["call_id"], "feed:xm_weekend_fade:2:0");
    let skipped = shadow_line["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["detail"] == "shadow: measurement only")
        .count();
    assert!(skipped >= 15, "{shadow_line}");
    // The capped fade named its signal row (edge 23 ≥ min_edge 10).
    let capped_line = lines
        .iter()
        .find(|l| l["client_order_id"] == capped_tsla)
        .unwrap();
    assert_eq!(capped_line["intent"]["strategy"], "overreaction");
    let signal_key = format!("xm_weekend_signal/1:{DATE}:{TSLA}");
    assert_eq!(capped_line["intent"]["opportunity_key"], signal_key);
    let signal = f.rig.store.get(&signal_key).await.unwrap().unwrap();
    assert_eq!(signal.features["edge_after_costs_bps"], 23.0);
    // Review #8: the row backs the capped fade only — its side, its
    // strategy, its size.
    assert_eq!(signal.features["side"], "sell");
    assert_eq!(signal.features["strategy"], "overreaction");
    assert_eq!(signal.features["max_notional_usd"], 20.0);
    assert_eq!(
        signal.observed_at_ms,
        ENTRY + 30_000,
        "stamped at the snapshot"
    );

    // Next call: the snapshot holds (a new TSLA price changes nothing),
    // AAPL is placed again (its book is back), nothing else twice.
    f.books.failing.lock().unwrap().clear();
    f.rig.clock.set(ENTRY + 90_000);
    f.prices(TSLA, 90.0, ENTRY + 85_000).await;
    let o = f.step("feed:xm_weekend_fade:3:0").await.unwrap();
    let again = fade_row(&o);
    assert_eq!(name(&again, TSLA).s_bps, tsla.s_bps, "decided once");
    assert_eq!(again.entered_at_ms, Some(ENTRY + 30_000));
    assert_eq!(name(&again, AAPL).shadow.as_ref().unwrap().status, "filled");
    assert_eq!(o.status, ObsStatus::Partial, "AMD still has no anchor");
    let orders = f.orders();
    assert_eq!(orders.len(), 5);
    assert_eq!(orders[4].1, shadow_aapl);
    assert_eq!(f.rig.risk_lines().len(), 6, "one new verdict: AAPL");
    // And again: nothing to place.
    f.step("feed:xm_weekend_fade:4:0").await.unwrap();
    assert_eq!((f.orders().len(), f.rig.risk_lines().len()), (5, 6));

    // The exit: the shadow positions close here; the capped ones are
    // left to xm_exits.
    f.rig.clock.set(EXIT + 10_000);
    for (id, px) in [(TSLA, 101.0), (NVDA, 199.0), (AAPL, 50.0)] {
        f.prices(id, px, EXIT + 5_000).await;
    }
    let o = f.step("feed:xm_weekend_fade:5:0").await.unwrap();
    let row = fade_row(&o);
    assert_eq!(o.key, format!("xm_weekend/1:{DATE}"), "the closing window");
    assert_eq!(row.phase, FadePhase::Closing);
    assert_eq!(row.shadow_exits.len(), 3);
    assert!(
        row.shadow_exits.iter().all(|e| e.status == "filled"),
        "{:?}",
        row.shadow_exits
    );
    assert!(row.shadow_exits[0]
        .client_order_id
        .starts_with(&format!("exit:{SHADOW}:")));
    assert_eq!(
        (
            o.features["n_shadow_open"].clone(),
            o.features["n_capped_open"].clone()
        ),
        (json!(0), json!(2))
    );
    assert!(name(&row, TSLA).shadow_pnl_usd.is_some());
    let capped_open = f
        .orders()
        .iter()
        .filter(|o| o.0 == ACCOUNT && o.1.starts_with("exit:"))
        .count();
    assert_eq!(capped_open, 0, "never closes the capped ledger");

    // xm_exits closes the capped two (deadline): closed, with the P&L.
    f.close_capped("feed:xm_exits:1:0").await;
    let o = f.step("feed:xm_weekend_fade:6:0").await.unwrap();
    let row = fade_row(&o);
    assert_eq!(row.phase, FadePhase::Closed, "{}", o.render_text(0));
    let shadow = f.rig.ledger.snapshot(SHADOW, EXIT + 20_000).await.unwrap();
    let mut want = 0.0;
    for id in [TSLA, NVDA, AAPL] {
        let n = name(&row, id);
        let net = position_net_usd(shadow.account.positions.get(id)) - n.shadow_base_usd;
        assert_eq!(n.shadow_pnl_usd, Some(net), "{id}");
        want += net;
    }
    let got = o.features["shadow_pnl_usd"].as_f64().unwrap();
    assert!((got - want).abs() < 1e-9, "{got} vs {want}");
    assert!(
        name(&row, TSLA).shadow_pnl_usd.unwrap() > 0.0,
        "a short from 102 to 101"
    );
    assert!(o.features.contains_key("capped_pnl_usd"));
    assert!(o
        .headline
        .starts_with("xm_weekend 2026-10-02 closed shadow_pnl_usd="));

    // Then the next window waits; the closed one stays closed.
    let o = f.step("feed:xm_weekend_fade:7:0").await.unwrap();
    assert_eq!(o.key, "xm_weekend/1:2026-10-09");
    assert_eq!(fade_row(&o).phase, FadePhase::Waiting);
    let closed = f
        .rig
        .store
        .get(&format!("xm_weekend/1:{DATE}"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fade_row(&closed).phase, FadePhase::Closed);
}

/// Review #8 (the finding): for the lateness after the entry, the capped
/// TSLA short's `xm_weekend_signal/1` row backs that fade only — a
/// `paper_order` buy of TSLA naming it is denied `min_edge`, so is a
/// sell larger than the fade; the fades themselves filled through it.
#[tokio::test]
async fn a_fade_signal_backs_its_own_fade_only() {
    let f = Fade::new(ENTRY + 30_000).await;
    f.market(&PRICES, ENTRY + 20_000).await;
    let row = fade_row(&f.step("feed:xm_weekend_fade:1:0").await.unwrap());
    for id in [TSLA, NVDA] {
        let o = name(&row, id).capped_order.clone().unwrap();
        assert_eq!(
            (o.status.as_str(), o.rule.as_deref()),
            ("filled", Some("ok")),
            "{id}"
        );
    }
    let key = format!("xm_weekend_signal/1:{DATE}:{TSLA}");
    let limits = f.rig.shared.risk.as_ref().unwrap().limits();
    let io = ExecIo {
        clock: f.rig.clock.as_ref(),
        books: &f.books,
        rand01: 0.5,
    };
    let order = |side: &str, usd: f64| {
        let args = json!({"instrument": TSLA, "side": side, "notional_usd": usd,
                              "kind": "market", "max_slippage_bps": 30,
                              "strategy": "overreaction", "opportunity": key});
        super::super::paper::parse_order(&args, limits.clone()).unwrap()
    };
    let denied = |o: &Observation| {
        let r: PaperFillRow = o.typed().unwrap();
        assert!(r.fill.is_none(), "{r:?}");
        (r.gate.rule, o.errors[0].message.clone())
    };
    let o = run_exec(
        &f.rig.shared,
        &f.rig.ctx(Some("op:1")),
        &io,
        order("buy", 20.0),
    )
    .await
    .unwrap();
    let (rule, why) = denied(&o);
    assert_eq!(rule, "min_edge");
    assert!(
        why.contains(&format!("{key} backs a sell; the order is a buy of {TSLA}")),
        "{why}"
    );
    let o = run_exec(
        &f.rig.shared,
        &f.rig.ctx(Some("op:2")),
        &io,
        order("sell", 25.0),
    )
    .await
    .unwrap();
    let (rule, why) = denied(&o);
    assert_eq!(rule, "min_edge");
    assert!(why.ends_with("max_notional_usd 20 USD"), "{why}");
    // Nothing traded: the capped ledger holds the two fades only.
    let capped: Vec<_> = f.orders().into_iter().filter(|o| o.0 == ACCOUNT).collect();
    assert_eq!(capped.len(), 2, "{capped:?}");
}

/// No call within the lateness: no late entry. A missing entry price on
/// every name keeps no snapshot (an error row), so the next call tries
/// again.
#[tokio::test]
async fn no_late_entry_and_no_snapshot_without_prices() {
    let f = Fade::new(ENTRY + 601_000).await;
    f.market(&PRICES, ENTRY + 600_000).await;
    let o = f.step("feed:xm_weekend_fade:1:0").await.unwrap();
    let row = fade_row(&o);
    assert_eq!((row.phase, row.late_s), (FadePhase::MissedEntry, Some(601)));
    assert_eq!(o.status, ObsStatus::Partial);
    assert!(
        o.errors[0].message.starts_with("missed_entry"),
        "{:?}",
        o.errors
    );
    assert!(f.orders().is_empty());

    // Inside the lateness, but every entry row is 10 min old.
    let f = Fade::new(ENTRY + 60_000).await;
    f.market(&PRICES, ENTRY - 600_000).await;
    let o = f.step("feed:xm_weekend_fade:1:0").await.unwrap();
    assert_eq!(o.status, ObsStatus::Error);
    assert_eq!(fade_row(&o).n_eligible(), 0);
    assert!(
        o.errors.iter().any(|e| e.class == ErrorClass::Transient),
        "a stale entry is retried soon: {:?}",
        o.errors
    );
    assert!(
        f.rig.store.get(&o.key).await.unwrap().is_none(),
        "no snapshot kept"
    );
    // Fresh rows: the next call enters.
    f.rig.clock.set(ENTRY + 120_000);
    f.market(&PRICES[..3], ENTRY + 110_000).await;
    let o = f.step("feed:xm_weekend_fade:2:0").await.unwrap();
    assert_eq!(fade_row(&o).phase, FadePhase::Entered);
    assert_eq!(f.orders().len(), 5, "2 capped + 3 shadow");
}

/// Two callers race for the entry: the snapshot kept first wins
/// (compare-and-swap) and the loser places that one, so the capped set
/// is never decided twice.
#[tokio::test]
async fn a_lost_snapshot_race_takes_the_winner() {
    let f = Fade::new(ENTRY + 30_000).await;
    f.market(&PRICES, ENTRY + 20_000).await;
    let w = fade_window(&nyse(), ENTRY).unwrap();
    let px = |px: f64, at_ms: i64| {
        Field::ok(PricePoint {
            px,
            at_ms,
            source: PriceSource::Mid,
        })
    };
    // Theirs: TSLA fell (a buy), the only name.
    let mut theirs = WeekendFade::new(
        FadePhase::Entered,
        w,
        ACCOUNT,
        SHADOW,
        5,
        1,
        600,
        ENTRY + 1_000,
    );
    let mut n = FadeName::at_entry(TSLA, false, px(100.0, ANCHOR), px(99.0, ENTRY));
    n.capped = true;
    theirs.names = vec![n];
    theirs.entered_at_ms = Some(ENTRY + 1_000);
    let obs = Observation::of(
        names::XM_WEEKEND_FADE,
        &theirs,
        ENTRY + 1_000,
        WINDOW_TTL_MS,
        ObsSource::Live,
    );
    // Ours read "no row", then theirs landed: the claim loses to it.
    f.rig.store.put(&obs).await.unwrap();
    let setup = f.rig.shared.fade.clone().unwrap();
    let env = Env {
        risk: f.rig.shared.risk.as_ref().unwrap(),
        ledger: f.rig.ledger.as_ref(),
        store: f.rig.shared.store.as_deref(),
        history: f.rig.shared.history.as_deref(),
        cfg: &setup.config,
        cal: setup.calendar.as_ref().unwrap(),
    };
    for (account, cash) in [(ACCOUNT, 100.0), (SHADOW, 10_000.0)] {
        f.rig
            .ledger
            .open_account(account, cash, ENTRY)
            .await
            .unwrap();
    }
    let mut ours = snapshot(&env, &w, ENTRY + 30_000).await.unwrap();
    assert_eq!(ours.n_eligible(), 3);
    claim(&env, &mut ours, None).await.unwrap();
    assert_eq!(ours, theirs, "the loser takes the winner's snapshot");
    // A whole step places theirs.
    let o = f.step("feed:xm_weekend_fade:1:0").await.unwrap();
    let row = fade_row(&o);
    assert_eq!(row.entered_at_ms, Some(ENTRY + 1_000), "theirs");
    assert_eq!(row.names.len(), 1);
    let orders = f.orders();
    assert_eq!(
        orders.iter().map(|o| o.1.clone()).collect::<Vec<_>>(),
        [
            format!("fade:{ACCOUNT}:{TSLA}:{DATE}"),
            format!("fade-shadow:{SHADOW}:{TSLA}:{DATE}")
        ]
    );
    assert_eq!(orders[0].2, "buy", "their signal: TSLA fell");
}

/// Review #1: the `perpsAtOpenInterestCap` read fails at the entry, so
/// every row has `at_oi_cap` unknown. Both gates deny every fade
/// `missing:at_oi_cap` (verdicts only, nothing stored); the next call,
/// on known rows, places every eligible fade under its first id.
#[tokio::test]
async fn an_unknown_oi_cap_at_entry_is_denied_then_placed_next_call() {
    let f = Fade::new(ENTRY + 30_000).await;
    f.market(&PRICES, ENTRY + 20_000).await;
    for (id, _, px) in PRICES {
        f.put(rows_with(id, px, ENTRY + 25_000, None, 3)).await;
    }
    let o = f.step("feed:xm_weekend_fade:1:0").await.unwrap();
    let row = fade_row(&o);
    assert_eq!(row.phase, FadePhase::Entered, "the snapshot is kept");
    assert!(f.orders().is_empty(), "nothing stored: {:?}", f.orders());
    let denied = |o: &Option<FadeOrder>| {
        let o = o.clone().unwrap();
        (o.status, o.rule)
    };
    let want = ("denied".to_string(), Some("missing:at_oi_cap".to_string()));
    for id in [TSLA, NVDA] {
        assert_eq!(denied(&name(&row, id).capped_order), want, "{id}");
    }
    for id in [TSLA, NVDA, AAPL] {
        assert_eq!(denied(&name(&row, id).shadow), want, "{id}");
    }
    assert_eq!(f.rig.risk_lines().len(), 5, "2 capped + 3 shadow verdicts");

    // The next hl_ctx run reads the cap list again.
    f.rig.clock.set(ENTRY + 90_000);
    for (id, _, px) in PRICES {
        f.prices(id, px, ENTRY + 85_000).await;
    }
    let o = f.step("feed:xm_weekend_fade:2:0").await.unwrap();
    let orders = f.orders();
    assert_eq!(orders.len(), 5, "{orders:?}");
    assert!(orders.iter().all(|o| o.3 == "filled"), "{orders:?}");
    assert_eq!(
        (
            o.features["n_capped_filled"].clone(),
            o.features["n_shadow_filled"].clone()
        ),
        (json!(2), json!(3))
    );
    let ids: BTreeSet<&str> = orders.iter().map(|o| o.1.as_str()).collect();
    for id in [
        format!("fade:{ACCOUNT}:{TSLA}:{DATE}"),
        format!("fade:{ACCOUNT}:{NVDA}:{DATE}"),
        format!("fade-shadow:{SHADOW}:{AAPL}:{DATE}"),
    ] {
        assert!(ids.contains(id.as_str()), "{id} in {ids:?}");
    }
}

/// Review #5: a stored rejection the next attempt may not meet (AAPL's
/// one-sided book: `missing:mid`) is placed again as `<id>:2` within the
/// lateness; a final one (NVDA in whole lots: $20 and $100 at 198 round
/// to 0, `MinTradeNtl`) and a fill never are; after the lateness nothing
/// is.
#[tokio::test]
async fn transient_entry_rejections_are_retried_within_the_lateness() {
    let f = Fade::new(ENTRY + 30_000).await;
    f.market(&PRICES, ENTRY + 20_000).await;
    f.put(rows_with(NVDA, 198.0, ENTRY + 20_000, Some(false), 0))
        .await;
    f.books.one_sided.lock().unwrap().insert(AAPL.into());
    let o = f.step("feed:xm_weekend_fade:1:0").await.unwrap();
    let row = fade_row(&o);
    let aapl = name(&row, AAPL).shadow.clone().unwrap();
    assert_eq!(aapl.status, "rejected");
    assert!(
        aapl.error
            .as_deref()
            .unwrap()
            .starts_with("rejected missing:mid"),
        "{aapl:?}"
    );
    let nvda = name(&row, NVDA);
    for o in [&nvda.capped_order, &nvda.shadow] {
        let o = o.clone().unwrap();
        assert_eq!(o.status, "rejected");
        assert!(
            o.error
                .as_deref()
                .unwrap()
                .starts_with("rejected MinTradeNtl"),
            "{o:?}"
        );
    }
    assert_eq!(f.orders().len(), 5, "every fade stored once");

    // Next call, AAPL's book whole again: its attempt 2 fills; NVDA's
    // final rejections and TSLA's fills stay.
    f.books.one_sided.lock().unwrap().clear();
    f.rig.clock.set(ENTRY + 90_000);
    let o = f.step("feed:xm_weekend_fade:2:0").await.unwrap();
    let row = fade_row(&o);
    let shadow_aapl = format!("fade-shadow:{SHADOW}:{AAPL}:{DATE}");
    let aapl = name(&row, AAPL).shadow.clone().unwrap();
    assert_eq!(
        (aapl.status.as_str(), aapl.client_order_id.as_str()),
        ("filled", format!("{shadow_aapl}:2").as_str())
    );
    let orders = f.orders();
    assert_eq!(orders.len(), 6, "{orders:?}");
    assert_eq!(
        (orders[5].1.as_str(), orders[5].3.as_str()),
        (format!("{shadow_aapl}:2").as_str(), "filled")
    );
    let nvda = name(&row, NVDA).capped_order.clone().unwrap();
    assert_eq!(
        (nvda.status.as_str(), nvda.client_order_id.as_str()),
        ("rejected", format!("fade:{ACCOUNT}:{NVDA}:{DATE}").as_str())
    );
    assert_eq!(o.features["n_shadow_filled"], 2);
    // And again: nothing to place.
    f.rig.clock.set(ENTRY + 150_000);
    f.step("feed:xm_weekend_fade:3:0").await.unwrap();
    assert_eq!(f.orders().len(), 6);

    // The same rejection with the lateness over: it stays the outcome.
    let g = Fade::new(ENTRY + 30_000).await;
    g.market(&PRICES, ENTRY + 20_000).await;
    g.books.one_sided.lock().unwrap().insert(AAPL.into());
    g.step("feed:xm_weekend_fade:1:0").await.unwrap();
    assert_eq!(g.orders().len(), 5);
    g.books.one_sided.lock().unwrap().clear();
    g.rig.clock.set(ENTRY + 600_000);
    for (id, _, px) in PRICES {
        g.prices(id, px, ENTRY + 595_000).await;
    }
    let o = g.step("feed:xm_weekend_fade:2:0").await.unwrap();
    assert_eq!(g.orders().len(), 5, "no attempt after the lateness");
    let aapl = name(&fade_row(&o), AAPL).shadow.clone().unwrap();
    assert_eq!(
        (aapl.status.as_str(), aapl.client_order_id.as_str()),
        ("rejected", shadow_aapl.as_str())
    );
}

/// Review #3: the window row is saved without the order outcomes (a
/// crash after the fills, or a concurrent `tool call` that saved the
/// claimed snapshot last). The next call reads each fade's outcome from
/// the ledger — inside the lateness or after the exit — with no double
/// entry, and the fills, closes and P&L of both ledgers count. A
/// position the fade did not open stays `position_open`.
#[tokio::test]
async fn a_lost_row_save_recovers_the_outcomes_from_the_ledger() {
    let f = Fade::new(ENTRY + 30_000).await;
    f.market(&PRICES, ENTRY + 20_000).await;
    let o = f.step("feed:xm_weekend_fade:1:0").await.unwrap();
    assert_eq!(o.features["n_capped_filled"], 2);
    let placed = fade_row(&o);
    let n_orders = f.orders().len();
    f.lose_outcomes(&placed).await;
    f.rig.clock.set(ENTRY + 90_000);
    let o = f.step("feed:xm_weekend_fade:2:0").await.unwrap();
    let row = fade_row(&o);
    assert_eq!(f.orders().len(), n_orders, "no double entry");
    for n in &placed.names {
        let got = name(&row, &n.instrument);
        assert_eq!(
            (&got.capped_order, &got.shadow),
            (&n.capped_order, &n.shadow),
            "{} read back from the ledger",
            n.instrument
        );
    }
    assert_eq!(
        (
            o.features["n_capped_filled"].clone(),
            o.features["n_shadow_filled"].clone()
        ),
        (json!(2), json!(3))
    );

    // The exit: the shadow closes here, the capped two by xm_exits; the
    // row closes with both ledgers' P&L.
    f.rig.clock.set(EXIT + 10_000);
    for (id, px) in [(TSLA, 101.0), (NVDA, 199.0), (AAPL, 50.0)] {
        f.prices(id, px, EXIT + 5_000).await;
    }
    f.close_capped("feed:xm_exits:1:0").await;
    let o = f.step("feed:xm_weekend_fade:3:0").await.unwrap();
    let row = fade_row(&o);
    assert_eq!(row.phase, FadePhase::Closed, "{}", o.render_text(0));
    assert_eq!(row.shadow_exits.len(), 3, "the shadow closed here");
    let now = f.rig.clock.now_ms();
    let shadow = f.rig.ledger.snapshot(SHADOW, now).await.unwrap().account;
    let capped = f.rig.ledger.snapshot(ACCOUNT, now).await.unwrap().account;
    for id in [TSLA, NVDA, AAPL] {
        let n = name(&row, id);
        let net = position_net_usd(shadow.positions.get(id)) - n.shadow_base_usd;
        assert_eq!(n.shadow_pnl_usd, Some(net), "{id}");
    }
    for id in [TSLA, NVDA] {
        let n = name(&row, id);
        let net = position_net_usd(capped.positions.get(id)) - n.capped_base_usd;
        assert_eq!(n.capped_pnl_usd, Some(net), "{id}");
    }
    assert!(
        o.features.contains_key("capped_pnl_usd"),
        "{:?}",
        o.features
    );
    assert!(o.features.contains_key("shadow_pnl_usd"));

    // Lost again, found only after the exit (the previous window's row).
    let g = Fade::new(ENTRY + 30_000).await;
    g.market(&PRICES, ENTRY + 20_000).await;
    let o = g.step("feed:xm_weekend_fade:1:0").await.unwrap();
    g.lose_outcomes(&fade_row(&o)).await;
    g.rig.clock.set(EXIT + 10_000);
    for (id, px) in [(TSLA, 101.0), (NVDA, 199.0), (AAPL, 50.0)] {
        g.prices(id, px, EXIT + 5_000).await;
    }
    let o = g.step("feed:xm_weekend_fade:2:0").await.unwrap();
    let row = fade_row(&o);
    assert_eq!(row.phase, FadePhase::Closing, "the capped two still open");
    assert_eq!(o.features["n_capped_open"], 2);
    g.close_capped("feed:xm_exits:1:0").await;
    let o = g.step("feed:xm_weekend_fade:3:0").await.unwrap();
    assert_eq!(fade_row(&o).phase, FadePhase::Closed);
    assert!(
        o.features.contains_key("capped_pnl_usd"),
        "{:?}",
        o.features
    );
    assert_eq!(g.orders().len(), 5 + 5, "5 entries, 5 closes");

    // A shadow position opened by hand before the entry: not faded.
    let h = Fade::new(ENTRY - 60_000).await;
    h.market(&PRICES, ENTRY - 70_000).await;
    let risk = h.rig.shared.risk.clone().unwrap();
    let by_hand = ExecOrder {
        tool: names::PAPER_ORDER,
        gate: ExecGate::Shadow {
            initial_cash_usd: 10_000.0,
            paper: PaperFills::of(&risk).unwrap(),
        },
        limits: RiskLimits {
            account: SHADOW.into(),
            ..risk.limits()
        },
        instrument: InstrumentId::parse(NVDA).unwrap(),
        side: Side::Buy,
        size: ExecSize::NotionalUsd(50.0),
        kind: OrderKind::Market,
        limit_px: None,
        reduce_only: false,
        max_slippage_bps: 50.0,
        strategy: None,
        hedge_instrument: None,
        opportunity_key: None,
        client_order_id: Some("manual:nvda:1".into()),
        exit_at_ms: None,
        tp_sl_on_book: None,
    };
    let io = ExecIo {
        clock: h.rig.clock.as_ref(),
        books: &h.books,
        rand01: 0.5,
    };
    let o = run_exec(&h.rig.shared, &h.rig.ctx(Some("manual:1")), &io, by_hand)
        .await
        .unwrap();
    assert_eq!(o.status, ObsStatus::Ok, "{}", o.render_text(0));
    h.rig.clock.set(ENTRY + 30_000);
    h.market(&PRICES, ENTRY + 20_000).await;
    let o = h.step("feed:xm_weekend_fade:1:0").await.unwrap();
    let row = fade_row(&o);
    let nvda = name(&row, NVDA).shadow.clone().unwrap();
    assert_eq!(nvda.status, "error");
    assert!(
        nvda.error.as_deref().unwrap().starts_with("position_open"),
        "{nvda:?}"
    );
    assert_eq!(
        name(&row, NVDA).capped_order.as_ref().unwrap().status,
        "filled",
        "the capped ledger holds no NVDA"
    );
}

/// Review #4: a restart near the entry finds some entry rows stale. The
/// snapshot is kept only complete: until entry + 120 s an incomplete
/// read keeps nothing (an `error` row); from then on the stale names are
/// left out `stale` for the window. A fresh row without a usable price
/// holds nothing back.
#[tokio::test]
async fn the_snapshot_waits_for_every_fresh_entry_row() {
    let f = Fade::new(ENTRY + 30_000).await;
    f.market(&PRICES, ENTRY - 300_000).await;
    f.market(&PRICES[..2], ENTRY + 20_000).await;
    let o = f.step("feed:xm_weekend_fade:1:0").await.unwrap();
    assert_eq!(o.status, ObsStatus::Error, "{}", o.render_text(0));
    let row = fade_row(&o);
    assert_eq!(name(&row, AAPL).skip, Some(Skip::Stale));
    assert_eq!(
        name(&row, AMD).skip,
        Some(Skip::MissingAnchor),
        "no anchor: never waited for"
    );
    assert!(
        f.rig.store.get(&o.key).await.unwrap().is_none(),
        "nothing kept"
    );
    assert!(f.orders().is_empty());
    // AAPL's row arrives: the whole snapshot.
    f.rig.clock.set(ENTRY + 60_000);
    f.prices(AAPL, 50.1, ENTRY + 55_000).await;
    let o = f.step("feed:xm_weekend_fade:2:0").await.unwrap();
    let row = fade_row(&o);
    assert_eq!((row.phase, row.n_eligible()), (FadePhase::Entered, 3));
    assert_eq!(f.orders().len(), 5);

    // AAPL never refreshes: kept without it from entry + 120 s.
    let g = Fade::new(ENTRY + 30_000).await;
    g.market(&PRICES, ENTRY - 300_000).await;
    g.market(&PRICES[..2], ENTRY + 20_000).await;
    g.rig.clock.set(ENTRY + ENTRY_WAIT_MS - 1);
    g.market(&PRICES[..2], ENTRY + 110_000).await;
    let o = g.step("feed:xm_weekend_fade:1:0").await.unwrap();
    assert_eq!(o.status, ObsStatus::Error, "still waiting");
    g.rig.clock.set(ENTRY + ENTRY_WAIT_MS);
    let o = g.step("feed:xm_weekend_fade:2:0").await.unwrap();
    let row = fade_row(&o);
    assert_eq!(row.phase, FadePhase::Entered);
    assert_eq!(name(&row, AAPL).skip, Some(Skip::Stale));
    assert_eq!(o.features["n_stale"], 1);
    assert!(o.headline.contains(" stale=1 "), "{}", o.headline);
    assert!(g.rig.store.get(&o.key).await.unwrap().is_some(), "kept");
    assert_eq!(g.orders().len(), 4, "2 capped + 2 shadow");
    // A fresh AAPL row later changes nothing.
    g.rig.clock.set(ENTRY + 180_000);
    g.prices(AAPL, 50.1, ENTRY + 175_000).await;
    g.step("feed:xm_weekend_fade:3:0").await.unwrap();
    assert_eq!(g.orders().len(), 4);

    // AAPL's row is fresh but has no price: `missing_entry`, kept at once.
    let h = Fade::new(ENTRY + 30_000).await;
    h.market(&PRICES, ENTRY + 20_000).await;
    let mut no_px = MarketCtx::new(InstrumentId::parse(AAPL).unwrap(), ENTRY + 25_000);
    no_px.at_oi_cap = Some(false);
    let row = Observation::of(
        names::HL_CTX,
        &no_px,
        ENTRY + 25_000,
        5_000,
        ObsSource::Live,
    );
    h.put(vec![row]).await;
    let o = h.step("feed:xm_weekend_fade:1:0").await.unwrap();
    let row = fade_row(&o);
    assert_eq!(row.phase, FadePhase::Entered, "{}", o.render_text(0));
    assert_eq!(name(&row, AAPL).skip, Some(Skip::MissingEntry));
    assert!(h.rig.store.get(&o.key).await.unwrap().is_some(), "kept");
    assert_eq!(h.orders().len(), 4, "2 capped + 2 shadow");
}

#[tokio::test]
async fn refusals_come_before_anything() {
    let f = Fade::new(ENTRY - 1_000).await;
    let io = ExecIo {
        clock: f.rig.clock.as_ref(),
        books: &f.books,
        rand01: 0.5,
    };
    let e = f
        .tool
        .step(&json!({"at": 1}), &f.rig.ctx(Some("x:1")), &io)
        .await
        .unwrap_err();
    assert!(
        e.to_string().contains("unknown argument(s) [\"at\"]"),
        "{e}"
    );
    let mut no_cfg = XmWeekendFadeTool {
        def: defs::def(names::XM_WEEKEND_FADE),
        shared: f.rig.shared.clone(),
    };
    no_cfg.shared.fade = None;
    let e = no_cfg
        .step(&json!({}), &f.rig.ctx(Some("x:2")), &io)
        .await
        .unwrap_err();
    assert!(
        e.to_string().starts_with(WEEKEND_FADE_CONFIG_MISSING),
        "{e}"
    );
    no_cfg.shared.fade = Some(Arc::new(FadeSetup {
        config: fade_config(),
        calendar: Err("calendar `us_equity` is not an exchange row".into()),
    }));
    let e = no_cfg
        .step(&json!({}), &f.rig.ctx(Some("x:3")), &io)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("not an exchange row"), "{e}");
    let mut routable = Fade::new(ENTRY - 1_000).await;
    routable.rig.agent.description = Some("routable".into());
    let e = routable.step("x:4").await.unwrap_err();
    assert!(e.to_string().starts_with("exec_agent_not_private"), "{e}");
    no_cfg.shared.risk = None;
    let e = no_cfg
        .step(&json!({}), &f.rig.ctx(Some("x:5")), &io)
        .await
        .unwrap_err();
    assert!(e.to_string().starts_with("risk_config_missing"), "{e}");
}

#[tokio::test]
async fn execute_gates_the_scope_first() {
    let f = Fade::new(ENTRY - 1_000).await;
    let mut denied = f.rig.ctx(Some("s:1"));
    let no_fs = crate::domain::scope::ToolScope::default();
    denied.scope = &no_fs;
    let e = f.tool.execute(&json!({}), &denied).await.unwrap_err();
    assert!(e.to_string().contains("fs"), "{e}");
    assert_eq!(f.tool.definition().name, "xm_weekend_fade");
}
