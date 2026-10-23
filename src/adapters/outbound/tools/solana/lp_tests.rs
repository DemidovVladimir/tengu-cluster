//! Tests of `src/adapters/outbound/tools/solana/lp.rs`.

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::json;

use crate::adapters::outbound::observations::SqliteObservationStore;
use crate::adapters::outbound::solana::plan::tests::{
    dlmm_now_ms, dlmm_transport, fixture_reads, gpa_reply, k, methods, owner_positions, BOT_WALLET,
    DLMM_GMA, DLMM_SLOT, JUP_PRICE, OWNER, PERPS_GMA, PERPS_META, POOL,
};
use crate::adapters::outbound::solana::rpc::tests::{fake_rpc, FakeTransport};
use crate::adapters::outbound::solana::rpc::RpcError;
use crate::adapters::outbound::tools::workspace::test_support::TestHarness;
use crate::application::observe::tests::MemStore;
use crate::domain::lp::snapshot::decide_hedge;

use crate::domain::lp::snapshot::HedgeActionRecord;
use crate::domain::observation::{assert_features_ok, ErrorClass, MAX_FEATURES, MAX_LINE1_CHARS};
use crate::domain::solana::AccountRead;

/// Fixture position (bins -5440..-5371, active -5373), re-owned by
/// `BOT_WALLET` so the operator wallet has one in-range position.
const POSITION: &str = "H9fmcxgheDvVSn9iUeRSvZPAgTY5WXqvroNpkZ2HCVRW";
const ATA_USDC: &str = "D9ScKYy15cw1tpkkuwEnDKv62nCyuETwrvRSdP4usGg1";
const ATA_WSOL: &str = "E4PCnfEconGJW6vf7GDycEnkFe1VWC7teiNWJzQv3NTA";
const FLAT_LONG: &str = "FqymRcB92t63jpwh7om4RLbxMNUGoHnZPQMkkAA8ksVY";
const FLAT_SHORT: &str = "6HFhuYzQGcqdj4NGwC6vfVETRvMA3pXaVeZnHgWSKsJK";
/// A live PositionRequest (not executed) from `perps/requests_gma.json`.
const REQUEST: &str = "11q9teW5JiHhWeY8ak79i72C4qpDtppzVgH1ZeEUWp3";
const SOL_USD: f64 = 116.6589164559586;

#[tokio::test]
async fn lp_swap_plan_is_a_typed_pure_tool() {
    let tmp = tempfile::TempDir::new().unwrap();
    let harness = TestHarness::new(tmp.path());
    let tool = LpSwapPlanTool {
        def: defs::def(names::LP_SWAP_PLAN),
    };
    let out = tool
        .execute(
            &json!({
                "wallet_sol": 0.5,
                "wallet_usdc": 1000,
                "target_sol": 1,
                "target_usdc": 100,
                "permanent_minimum_sol": 0.2,
                "rent_reserve_sol": 0.1,
                "current_price": 100,
                "slippage_buffer_pct": 0.02,
                "context": "rebalance"
            }),
            &harness.ctx(),
        )
        .await
        .unwrap();
    let obs = out.observation.unwrap();
    assert_eq!(obs.schema, "lp_swap_plan/1");
    assert_eq!(obs.features["direction"], "usdc_to_sol");
    assert_eq!(obs.data["swaps"][0]["input_mint"], USDC);
    assert_eq!(obs.data["swaps"][0]["output_mint"], WSOL);
    assert!((obs.data["swaps"][0]["amount"].as_f64().unwrap() - 81.6).abs() < 1e-9);
}

const WALLET_GMA: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/solana/wallet/gma_base64.json"
));
const WALLET_META: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/solana/wallet/meta.json"
));
const REQUESTS_GMA: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/solana/perps/requests_gma.json"
));
const LPING_TOML: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/sandboxes/lping/config.toml"
));

fn keys_of(v: &Value) -> Vec<Pubkey> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|e| k(e.as_str().or_else(|| e["pubkey"].as_str()).unwrap()))
        .collect()
}

/// DLMM + perps + wallet + request fixtures behind one fake RPC at the
/// DLMM slot; `POSITION` re-owned by `BOT_WALLET` when `reown`.
fn transport(reown: bool) -> Arc<FakeTransport> {
    let t = dlmm_transport();
    let perps: Value = serde_json::from_str(PERPS_META).unwrap();
    for r in fixture_reads(PERPS_GMA, &keys_of(&perps["gma"]["keys"])) {
        t.put_account(r);
    }
    for r in fixture_reads(REQUESTS_GMA, &keys_of(&perps["requests_gma"]["keys"])) {
        t.put_account(r);
    }
    let wallet: Value = serde_json::from_str(WALLET_META).unwrap();
    for r in fixture_reads(
        WALLET_GMA,
        &keys_of(&wallet["files"]["gma_base64.json"]["keys"]),
    ) {
        t.put_account(r);
    }
    if reown {
        let read = t
            .accounts
            .lock()
            .unwrap()
            .get(&k(POSITION))
            .cloned()
            .unwrap();
        let mut data = read.data().unwrap();
        data[40..72].copy_from_slice(&k(BOT_WALLET).0);
        t.put_account(AccountRead::from_bytes(
            k(POSITION),
            read.slot,
            *read.owner().unwrap(),
            read.lamports().unwrap(),
            &data,
        ));
    }
    let _ = DLMM_GMA;
    t
}

/// Canned Jupiter price v3 answers (fixture: SOL = `SOL_USD`), counted.
struct Jup {
    calls: AtomicUsize,
    fail: bool,
}

impl Jup {
    fn ok() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            fail: false,
        }
    }
    fn down() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            fail: true,
        }
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
    async fn get(&self, url: String) -> Result<(u16, Value)> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert!(url.contains("lite-api.jup.ag/price/v3"), "{url}");
        assert!(url.contains(ids::WSOL), "{url}");
        if self.fail {
            return Err(RpcError::new(ErrorClass::Timeout, "lite-api.jup.ag timed out").into());
        }
        Ok((200, serde_json::from_str(JUP_PRICE).unwrap()))
    }
}

fn req(wallet: &str) -> SnapshotReq {
    SnapshotReq::parse(&json!({"wallet": wallet, "pool": POOL})).unwrap()
}

async fn snapshot(
    t: &Arc<FakeTransport>,
    store: &dyn ObservationStore,
    req: &SnapshotReq,
    args: Value,
    jup: &Jup,
    now: i64,
) -> Observation {
    let rpc = fake_rpc(t);
    lp_snapshot_obs(&rpc, Some(store), req, &args, |u| jup.get(u), now).await
}

fn assert_line1(o: &Observation, ids: &[&str]) {
    let text = o.render_text(o.observed_at_ms);
    let line1 = text.lines().next().unwrap();
    assert!(line1.chars().count() <= MAX_LINE1_CHARS, "{line1}");
    for id in ids {
        assert!(line1.contains(id), "{id} missing in {line1}");
    }
    assert!(o.features.len() <= MAX_FEATURES);
    assert_features_ok(&o.features);
}

fn gma_keys(t: &FakeTransport, i: usize) -> Vec<String> {
    t.gma_params()[i][0]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect()
}

/// `args` of a `hedge_watch` action in `sandboxes/lping/config.toml`.
fn toml_args(action: &str) -> Value {
    let v: toml::Value = toml::from_str(LPING_TOML).unwrap();
    let args = &v["decision_loops"]["hedge_watch"]["actions"][action]["args"];
    serde_json::to_value(args).unwrap()
}

/// Production hedge knobs from the sandbox TOML.
fn hedge_knobs() -> HedgeKnobs {
    HedgeKnobs::parse(&toml_args("decide_hedge")["knobs"]).unwrap()
}

fn lp_knobs() -> LpKnobs {
    LpKnobs::parse(&toml_args("decide_lp")["knobs"]).unwrap()
}

// ── lp_snapshot ─────────────────────────────────────────────────

#[tokio::test]
async fn snapshot_is_one_planned_read_composed_then_cached() {
    let t = transport(true);
    t.push(Ok(gpa_reply(DLMM_SLOT, &[k(POSITION)])));
    let store = MemStore::default();
    let jup = Jup::ok();
    let now = dlmm_now_ms();
    let r = req(BOT_WALLET);
    let o = snapshot(&t, &store, &r, json!({}), &jup, now).await;
    assert_eq!(o.status, ObsStatus::Ok, "{:?}", o.errors);
    assert_eq!(o.key, format!("lp_snapshot/1:{BOT_WALLET}:{POOL}"));
    assert_eq!((o.source, o.ttl_ms), (ObsSource::Live, 10_000));
    assert_eq!(o.tool, names::LP_SNAPSHOT);
    assert_line1(&o, &[BOT_WALLET, POOL]);

    // Discovery, then read_pool's two GMAs (the second pinned).
    assert_eq!(
        methods(&t),
        [
            "getProgramAccounts",
            "getMultipleAccounts",
            "getMultipleAccounts"
        ]
    );
    let first = gma_keys(&t, 0);
    assert_eq!(first[0], POOL);
    for key in [
        POSITION,
        FLAT_LONG,
        FLAT_SHORT,
        ids::JUP_CUSTODY_SOL,
        ids::JUP_CUSTODY_USDC,
        ids::JLP_POOL,
        BOT_WALLET,
        ATA_WSOL,
        ATA_USDC,
        ids::WSOL,
        ids::USDC,
    ] {
        assert!(first.contains(&key.to_string()), "read 1 lacks {key}");
    }
    assert_eq!(
        t.gma_params()[0][1]["minContextSlot"],
        DLMM_SLOT,
        "read 1 pinned to the gPA slot"
    );
    assert_eq!(t.gma_params()[1][1]["minContextSlot"], DLMM_SLOT);

    // Oracle fetched inline once and stored where `world` reads it.
    assert_eq!(jup.calls(), 1);
    let price = store
        .get(&format!("price_oracle/1:{}", ids::WSOL))
        .await
        .unwrap()
        .expect("price row stored");
    assert_eq!(
        (price.tool.as_str(), price.observed_at_ms),
        (names::SOL_PRICE, now)
    );

    let s: LpSnapshot = o.typed().unwrap();
    assert_eq!((s.wallet.as_str(), s.pool.as_str()), (BOT_WALLET, POOL));
    assert!(matches!(
        s.discovery,
        Discovery::Found {
            count: 1,
            source: DiscoverySource::Gpa,
            ..
        }
    ));
    assert_eq!(s.positions.len(), 1);
    assert_eq!(s.positions[0].position, POSITION);
    assert!(s.hedge.applicable);
    assert_eq!(
        (&s.hedge.long, &s.hedge.short),
        (&Field::Absent, &Field::Absent)
    );
    assert_eq!(s.hedge.max_request_execution_sec.value(), Some(&45));
    assert_eq!(s.wallet_balances.native_sol.value(), Some(&2.748_145_289));
    assert_eq!(s.wallet_balances.quote.value().unwrap().raw, "107808931");
    assert_eq!(s.wallet_balances.wsol.value().unwrap().raw, "0");
    assert_eq!(s.oracle.usd, Some(SOL_USD));
    assert_eq!(s.oracle.age_ms, Some(0));
    assert!(s.pool_vs_oracle_bps.unwrap().abs() < 10.0);
    assert_eq!((s.slot, s.slot_max), (DLMM_SLOT, DLMM_SLOT));
    for key in [POOL, POSITION, BOT_WALLET, ATA_USDC, FLAT_SHORT] {
        assert!(s.watch.contains(&key.to_string()), "watch lacks {key}");
    }
    assert_eq!(s.hedge.pending_request, None);

    // Within 10 s: the stored row, no RPC, no price fetch.
    let n = t.requests().len();
    let again = snapshot(&t, &store, &r, json!({}), &jup, now + 9_000).await;
    assert_eq!(again.source, ObsSource::Cache);
    assert_eq!(again.key, o.key);
    assert_eq!((t.requests().len(), jup.calls()), (n, 1));
}

/// Regression (review #10): a price row up to 30 s old was reused, so a
/// decision on that snapshot seconds later blocked `stale_input` (price
/// age + snapshot age > max_snapshot_age_secs 30) with a fresh price one
/// call away.
#[tokio::test]
async fn a_price_row_is_reused_only_within_the_snapshot_max_age() {
    let store = MemStore::default();
    let now = dlmm_now_ms();
    let jup = Jup::ok();
    let (w, p) = (k(BOT_WALLET), k(POOL));
    let price_key = format!("price_oracle/1:{}", ids::WSOL);
    // Seed a price row 26 s old.
    let t = transport(true);
    t.push(Ok(gpa_reply(DLMM_SLOT, &[k(POSITION)])));
    snapshot(&t, &store, &req(BOT_WALLET), json!({}), &jup, now - 26_000).await;
    assert_eq!(jup.calls(), 1);
    // 26 s later (discovery row still cached): the price is fetched
    // again (was reused), samples carried.
    let o = snapshot(
        &transport(true),
        &store,
        &req(BOT_WALLET),
        json!({}),
        &jup,
        now,
    )
    .await;
    assert_eq!(o.status, ObsStatus::Ok, "{:?}", o.errors);
    assert_eq!(jup.calls(), 2);
    let s: LpSnapshot = o.typed().unwrap();
    assert_eq!(s.oracle.age_ms, Some(0));
    let row = store.get(&price_key).await.unwrap().unwrap();
    assert_eq!(row.observed_at_ms, now);
    let price: OraclePrice = row.typed().unwrap();
    assert_eq!(price.samples.len(), 2, "{:?}", price.samples);
    // A decision on the snapshot row 9 s later is not stale.
    let (d, _) = hedge_decide_obs(Some(&store), &w, &p, &hedge_knobs(), false, now + 9_000).await;
    let d: HedgeDecision = d.typed().unwrap();
    assert_ne!(d.action.guard(), Some(Guard::StaleInput), "{:?}", d.action);
    // A new build within 10 s reuses the row as is ...
    let args = json!({"wallet": BOT_WALLET, "pool": POOL, "positions": [POSITION]});
    let r = SnapshotReq::parse(&args).unwrap();
    let o = snapshot(&transport(true), &store, &r, args, &jup, now + 9_000).await;
    let s: LpSnapshot = o.typed().unwrap();
    assert_eq!((jup.calls(), s.oracle.age_ms), (2, Some(9_000)));
    // ... unless the caller's max age is tighter (0 = live).
    let t = transport(true);
    t.push(Ok(gpa_reply(DLMM_SLOT, &[k(POSITION)])));
    let args = json!({"max_age_secs": 0});
    let o = snapshot(&t, &store, &req(BOT_WALLET), args, &jup, now + 9_000).await;
    let s: LpSnapshot = o.typed().unwrap();
    assert_eq!((jup.calls(), s.oracle.age_ms), (3, Some(0)));
}

/// The oracle bound keeps a decision on a cached snapshot inside the
/// production staleness budget with room for the loop's model calls.
#[test]
fn snapshot_age_plus_oracle_reuse_fits_the_decision_budget() {
    let worst = LP_SNAPSHOT_TTL_MS + SNAPSHOT_ORACLE_MAX_AGE_MS;
    for secs in [
        hedge_knobs().max_snapshot_age_secs,
        lp_knobs().max_snapshot_age_secs,
    ] {
        assert!(worst + 5_000 <= secs * 1000, "{worst} ms vs {secs} s");
    }
}

#[tokio::test]
async fn a_failed_inline_price_is_a_partial_snapshot_never_a_zero() {
    let t = transport(false);
    t.push(Ok(gpa_reply(DLMM_SLOT, &[])));
    let store = MemStore::default();
    let jup = Jup::down();
    let now = dlmm_now_ms();
    let o = snapshot(&t, &store, &req(BOT_WALLET), json!({}), &jup, now).await;
    assert_eq!(o.status, ObsStatus::Partial);
    let s: LpSnapshot = o.typed().unwrap();
    assert_eq!(s.oracle.usd, None);
    let e = s.oracle.error.as_ref().unwrap();
    assert_eq!((e.field.as_str(), e.class), ("oracle", ErrorClass::Timeout));
    assert!(!o.features.contains_key("oracle_usd"));
    assert!(
        store
            .get(&format!("price_oracle/1:{}", ids::WSOL))
            .await
            .unwrap()
            .is_none(),
        "an Error price row is never stored"
    );
    // The hedge refuses it.
    let (w, p) = (k(BOT_WALLET), k(POOL));
    let (d, _) = hedge_decide_obs(Some(&store), &w, &p, &hedge_knobs(), false, now).await;
    let d: HedgeDecision = d.typed().unwrap();
    assert_eq!(d.action.guard(), Some(Guard::InvalidRead), "{:?}", d.action);
    assert!(d.trace.invalid_fields.contains(&"oracle.usd".to_string()));
}

#[tokio::test]
async fn the_pending_keeper_request_is_read_in_the_same_gma() {
    let t = transport(false);
    t.push(Ok(gpa_reply(DLMM_SLOT, &[])));
    let store = MemStore::default();
    let now = dlmm_now_ms();
    let mut st = LpControllerState::new(BOT_WALLET, POOL);
    st.last_hedge_action = Some(HedgeActionRecord {
        at_ms: now - 10_000,
        action: "increase_short".into(),
        live: true,
        signatures: Vec::new(),
        position_request: Some(REQUEST.into()),
        counter: Some(473_577_047),
    });
    let row = Observation::of("hedge_decide", &st, now, LP_STATE_TTL_MS, ObsSource::Live);
    store.put(&row).await.unwrap();
    let o = snapshot(&t, &store, &req(BOT_WALLET), json!({}), &Jup::ok(), now).await;
    assert!(
        gma_keys(&t, 0).contains(&REQUEST.to_string()),
        "request in read 1"
    );
    assert_eq!(t.gma_params().len(), 2, "no extra read for the request");
    let s: LpSnapshot = o.typed().unwrap();
    let pending = s.hedge.pending_request.as_ref().unwrap().value().unwrap();
    assert_eq!(pending.position_request, REQUEST);
    assert!(pending.exists && !pending.executed, "{pending:?}");
    assert!(s.watch.contains(&REQUEST.to_string()));
    assert_eq!(o.features["pending_request"], json!(true));
}

#[tokio::test]
async fn read_after_write_and_explicit_positions_bypass_the_cached_row() {
    let store = MemStore::default();
    let now = dlmm_now_ms();
    let jup = Jup::ok();
    let t = transport(true);
    t.push(Ok(gpa_reply(DLMM_SLOT, &[k(POSITION)])));
    snapshot(&t, &store, &req(BOT_WALLET), json!({}), &jup, now).await;

    // min_context_slot above the cached rows: live, every read pinned
    // (rows at or above it would be reused), the row stored.
    let t = transport(true);
    t.slot.store(DLMM_SLOT + 5, Ordering::SeqCst);
    t.push(Ok(gpa_reply(DLMM_SLOT + 5, &[k(POSITION)])));
    let min = DLMM_SLOT + 1;
    let args = json!({"wallet": BOT_WALLET, "pool": POOL, "min_context_slot": min});
    let r = SnapshotReq::parse(&args).unwrap();
    let o = snapshot(&t, &store, &r, args, &jup, now + 1_000).await;
    assert_eq!(o.source, ObsSource::Live);
    assert_eq!(
        methods(&t),
        [
            "getProgramAccounts",
            "getMultipleAccounts",
            "getMultipleAccounts"
        ],
        "cached discovery and account rows bypassed"
    );
    // Read 1 pinned to max(min_context_slot, the gPA's slot).
    assert_eq!(t.gma_params()[0][1]["minContextSlot"], DLMM_SLOT + 5);
    assert_eq!(t.gma_params()[1][1]["minContextSlot"], DLMM_SLOT + 5);
    assert_eq!(o.slot, Some(DLMM_SLOT + 5));
    assert_eq!(
        store.get(&o.key).await.unwrap().unwrap().observed_at_ms,
        now + 1_000,
        "stored for the decide tools"
    );

    // Explicit positions: no gPA, found (args); a foreign key is flagged;
    // never stored (the canonical row above is kept).
    let t = transport(true);
    t.slot.store(DLMM_SLOT + 5, Ordering::SeqCst);
    let foreign = owner_positions()[0].to_string();
    let args = json!({"wallet": BOT_WALLET, "pool": POOL, "positions": [POSITION, foreign]});
    let r = SnapshotReq::parse(&args).unwrap();
    let o = snapshot(&t, &store, &r, args, &jup, now + 2_000).await;
    assert_eq!((o.source, o.ttl_ms), (ObsSource::Live, 0));
    assert_eq!(
        store.get(&o.key).await.unwrap().unwrap().observed_at_ms,
        now + 1_000,
        "the explicit subset is not stored"
    );
    assert!(!methods(&t).contains(&"getProgramAccounts".to_string()));
    let s: LpSnapshot = o.typed().unwrap();
    assert!(matches!(
        s.discovery,
        Discovery::Found {
            source: DiscoverySource::Args,
            ..
        }
    ));
    assert_eq!(s.positions.len(), 1);
    assert_eq!(o.status, ObsStatus::Partial);
    assert!(
        s.errors.iter().any(|e| e.message.contains(&foreign)),
        "{:?}",
        s.errors
    );
}

/// Regression (review #4/#9/#13): an explicit-positions snapshot (a
/// subset, or a foreign key ⇒ exposure 0) was stored under the
/// canonical key and answered the loop's snapshot step and the decide
/// tools for 10 s.
#[tokio::test]
async fn an_explicit_positions_snapshot_never_answers_discovery_or_decisions() {
    let store = MemStore::default();
    let now = dlmm_now_ms();
    let jup = Jup::ok();
    let (w, p) = (k(BOT_WALLET), k(POOL));
    let foreign = owner_positions()[0].to_string();
    let args = json!({"wallet": BOT_WALLET, "pool": POOL, "positions": [foreign]});
    let r = SnapshotReq::parse(&args).unwrap();
    let o = snapshot(&transport(true), &store, &r, args, &jup, now).await;
    let s: LpSnapshot = o.typed().unwrap();
    assert!(s.positions.is_empty());
    assert_eq!(o.ttl_ms, 0);
    assert!(store.get(&o.key).await.unwrap().is_none(), "never stored");
    // The decide tools find no row to decide on.
    let (d, _) = hedge_decide_obs(Some(&store), &w, &p, &hedge_knobs(), false, now).await;
    let d: HedgeDecision = d.typed().unwrap();
    assert!(d.view.is_none(), "{:?}", d.action);
    // The loop's snapshot step (no positions) within 10 s: live gPA.
    let t = transport(true);
    t.push(Ok(gpa_reply(DLMM_SLOT, &[k(POSITION)])));
    let o = snapshot(&t, &store, &req(BOT_WALLET), json!({}), &jup, now + 1_000).await;
    assert_eq!(o.source, ObsSource::Live);
    assert_eq!(methods(&t)[0], "getProgramAccounts");
    // A subset row an older binary stored is refused by both decide tools.
    let mut legacy: LpSnapshot = o.typed().unwrap();
    legacy.discovery = Discovery::Found {
        count: 1,
        source: DiscoverySource::Args,
        at_ms: now + 1_000,
    };
    let row = Observation::of(
        names::LP_SNAPSHOT,
        &legacy,
        now + 1_000,
        LP_SNAPSHOT_TTL_MS,
        ObsSource::Live,
    );
    store.put(&row).await.unwrap();
    let (d, _) = hedge_decide_obs(Some(&store), &w, &p, &hedge_knobs(), true, now + 2_000).await;
    let d: HedgeDecision = d.typed().unwrap();
    assert!(
        matches!(&d.action, HedgeAction::Blocked { reason, .. } if reason.contains("caller-chosen")),
        "{:?}",
        d.action
    );
    let (d, note) = lp_decide_obs(Some(&store), &w, &p, &lp_knobs(), true, now + 2_000, None).await;
    let d: LpDecision = d.typed().unwrap();
    assert!(
        matches!(&d.verdict, LpVerdict::Blocked { reason } if reason.contains("caller-chosen")),
        "{:?}",
        d.verdict
    );
    assert_eq!(note, None);
    assert!(store.get(&state_key(&w, &p)).await.unwrap().is_none());
}

/// Regression (review #1/#2/#5): a cached "no positions" discovery was
/// reused for 300 s, so a position another process opened read as LP 0.
#[tokio::test]
async fn a_position_opened_after_an_empty_discovery_shows_up_next_snapshot() {
    let store = MemStore::default();
    let now = dlmm_now_ms();
    let jup = Jup::ok();
    let t = transport(true);
    t.push(Ok(gpa_reply(DLMM_SLOT, &[])));
    let s: LpSnapshot = snapshot(&t, &store, &req(BOT_WALLET), json!({}), &jup, now)
        .await
        .typed()
        .unwrap();
    assert!(matches!(s.discovery, Discovery::Empty { .. }));
    // The snapshot row expired (10 s): the empty answer is as old, so
    // discovery runs again and sees the new position.
    let t = transport(true);
    t.push(Ok(gpa_reply(DLMM_SLOT, &[k(POSITION)])));
    let o = snapshot(&t, &store, &req(BOT_WALLET), json!({}), &jup, now + 11_000).await;
    assert_eq!(methods(&t)[0], "getProgramAccounts");
    let s: LpSnapshot = o.typed().unwrap();
    assert_eq!(s.positions.len(), 1, "{:?}", s.discovery);
    assert!(s.exposure.value().unwrap().base > 0.0);
}

#[tokio::test]
async fn the_fixture_owner_has_three_positions() {
    let t = transport(false);
    t.push(Ok(gpa_reply(DLMM_SLOT, &owner_positions())));
    let store = MemStore::default();
    let now = dlmm_now_ms();
    let o = snapshot(&t, &store, &req(OWNER), json!({}), &Jup::ok(), now).await;
    let s: LpSnapshot = o.typed().unwrap();
    assert_eq!(s.positions.len(), 3);
    assert!(s.hedge.applicable);
    assert_line1(&o, &[OWNER, POOL]);
    let (w, p) = (k(OWNER), k(POOL));
    let (d, _) = lp_decide_obs(Some(&store), &w, &p, &lp_knobs(), false, now, None).await;
    let d: LpDecision = d.typed().unwrap();
    assert!(
        matches!(d.verdict, LpVerdict::Paused { .. }),
        "{:?}",
        d.verdict
    );
}

#[tokio::test]
async fn a_whole_read_failure_is_an_error_row_never_stored() {
    // Pool absent on chain.
    let t = Arc::new(FakeTransport::at_slot(DLMM_SLOT));
    t.push(Ok(gpa_reply(DLMM_SLOT, &[])));
    let store = MemStore::default();
    let o = snapshot(&t, &store, &req(BOT_WALLET), json!({}), &Jup::ok(), 5).await;
    assert_eq!((o.status, o.ttl_ms), (ObsStatus::Error, 0));
    assert_line1(&o, &[BOT_WALLET, POOL]);
    assert_eq!(o.errors[0].class, ErrorClass::NotApplicable);
    assert!(store.get(&o.key).await.unwrap().is_none());
    // RPC quota exhausted (fresh store: nothing cached).
    let t = Arc::new(FakeTransport::at_slot(DLMM_SLOT));
    t.push(Ok(gpa_reply(DLMM_SLOT, &[])));
    t.push(Err(RpcError::new(
        ErrorClass::QuotaExhausted,
        "max usage reached",
    )));
    let store = MemStore::default();
    let o = snapshot(&t, &store, &req(BOT_WALLET), json!({}), &Jup::ok(), 5).await;
    assert_eq!(methods(&t), ["getProgramAccounts", "getMultipleAccounts"]);
    assert_eq!(o.status, ObsStatus::Error);
    assert_eq!(o.errors[0].class, ErrorClass::QuotaExhausted);
}

// ── hedge_decide / lp_decide ────────────────────────────────────

async fn seeded(store: &MemStore, now: i64) -> LpSnapshot {
    let t = transport(true);
    t.push(Ok(gpa_reply(DLMM_SLOT, &[k(POSITION)])));
    snapshot(&t, store, &req(BOT_WALLET), json!({}), &Jup::ok(), now)
        .await
        .typed()
        .unwrap()
}

#[tokio::test]
async fn decisions_without_a_snapshot_row_block_as_stale() {
    let store = MemStore::default();
    let (w, p) = (k(BOT_WALLET), k(POOL));
    let (o, note) = hedge_decide_obs(Some(&store), &w, &p, &hedge_knobs(), true, 1).await;
    let d: HedgeDecision = o.typed().unwrap();
    assert_eq!(d.action.guard(), Some(Guard::StaleInput));
    assert!(
        matches!(&d.action, HedgeAction::Blocked { reason, .. } if reason.contains("call lp_snapshot first")),
        "{:?}",
        d.action
    );
    assert_eq!(note, None, "nothing to commit");
    assert!(store.get(&state_key(&w, &p)).await.unwrap().is_none());
    let (o, _) = lp_decide_obs(None, &w, &p, &lp_knobs(), true, 1, None).await;
    let d: LpDecision = o.typed().unwrap();
    assert!(
        matches!(&d.verdict, LpVerdict::Blocked { reason } if reason.contains("observation store unavailable")),
        "{:?}",
        d.verdict
    );
    assert_line1(&o, &[BOT_WALLET, POOL]);
}

#[tokio::test]
async fn hedge_decide_with_lp_knobs_is_the_in_cycle_decision() {
    // Finding #14: with lp_knobs the hedge computes storm / imbalance from
    // this snapshot + the price row's samples instead of lp_state flags.
    let store = MemStore::default();
    let now = dlmm_now_ms();
    let snap = seeded(&store, now).await;
    let (w, p) = (k(BOT_WALLET), k(POOL));
    let (knobs, lpk) = (hedge_knobs(), lp_knobs());
    let at = now + 2_000;
    let (o, _) =
        hedge_decide_obs_in_cycle(Some(&store), &w, &p, &knobs, Some(&lpk), false, at, None).await;
    let d: HedgeDecision = o.typed().unwrap();
    let row = store.get(&snapshot_key(&w, &p)).await.unwrap().unwrap();
    let samples = price_samples(Some(&store), &snap).await;
    let cycle = LpCycle::new(&lpk, &samples);
    let fresh = LpControllerState::new(BOT_WALLET, POOL);
    let (want, _) = decide_hedge_in_cycle(
        &snap,
        &row.clone().served_from_cache().meta(at),
        2_000,
        &knobs,
        &fresh,
        Some(&cycle),
        at,
    );
    assert_eq!(d, want);
}

#[test]
fn hedge_decide_lp_knobs_are_validated_by_name() {
    let hk = serde_json::to_value(hedge_knobs()).unwrap();
    let args =
        json!({"wallet": BOT_WALLET, "pool": POOL, "knobs": hk, "lp_knobs": {"storm_pct_5m": 2}});
    let e = DecideArgs::parse(&args, names::HEDGE_DECIDE)
        .unwrap_err()
        .to_string();
    assert!(e.contains("lp_knobs") && e.contains("missing"), "{e}");
    let lk = serde_json::to_value(lp_knobs()).unwrap();
    let ok = json!({"wallet": BOT_WALLET, "pool": POOL, "knobs": hk, "lp_knobs": lk});
    assert!(DecideArgs::parse(&ok, names::HEDGE_DECIDE)
        .unwrap()
        .lp_knobs
        .is_some());
}

#[tokio::test]
async fn hedge_decide_reads_the_rows_and_commits_only_on_request() {
    let store = MemStore::default();
    let now = dlmm_now_ms();
    let snap = seeded(&store, now).await;
    let (w, p) = (k(BOT_WALLET), k(POOL));
    let knobs = hedge_knobs();
    let at = now + 2_000;

    let (o, note) = hedge_decide_obs(Some(&store), &w, &p, &knobs, false, at).await;
    assert_eq!(note, None);
    assert!(
        store.get(&state_key(&w, &p)).await.unwrap().is_none(),
        "commit=false writes nothing"
    );
    assert_eq!(o.key, format!("hedge_decide/1:{BOT_WALLET}:{POOL}"));
    assert_eq!((o.ttl_ms, o.status), (0, ObsStatus::Ok));
    assert!(
        store.get(&o.key).await.unwrap().is_none(),
        "decisions are never cached"
    );
    assert_line1(&o, &[BOT_WALLET, POOL]);
    let d: HedgeDecision = o.typed().unwrap();
    // Exactly the domain decision on the stored row.
    let row = store.get(&snapshot_key(&w, &p)).await.unwrap().unwrap();
    let fresh = LpControllerState::new(BOT_WALLET, POOL);
    let (want, next) = decide_hedge(
        &snap,
        &row.clone().served_from_cache().meta(at),
        2_000,
        &knobs,
        &fresh,
        at,
    );
    assert_eq!(d, want);
    assert_eq!(d.snapshot.as_ref().unwrap().source, ObsSource::Cache);
    assert_eq!(d.snapshot_age_ms, Some(2_000));
    assert_ne!(d.action.guard(), Some(Guard::InvalidRead), "{:?}", d.action);

    let (_, note) = hedge_decide_obs(Some(&store), &w, &p, &knobs, true, at).await;
    assert_eq!(
        note,
        Some(format!("lp_state committed: {}", state_key(&w, &p)))
    );
    let row = store.get(&state_key(&w, &p)).await.unwrap().unwrap();
    assert_eq!(
        (row.ttl_ms, row.tool.as_str()),
        (LP_STATE_TTL_MS, names::HEDGE_DECIDE)
    );
    let st: LpControllerState = row.typed().unwrap();
    assert_eq!(st, next);
    assert_eq!(st.last_position_seen_ms, Some(now), "the snapshot's time");

    // An old snapshot row → the domain staleness gate.
    let (o, _) = hedge_decide_obs(Some(&store), &w, &p, &knobs, false, now + 31_000).await;
    let d: HedgeDecision = o.typed().unwrap();
    assert_eq!(d.action.guard(), Some(Guard::StaleInput), "{:?}", d.action);
}

/// Review M1: a snapshot row read before the wallet's last landed write
/// (write fence) is stale for both decide tools.
#[tokio::test]
async fn a_snapshot_before_the_write_fence_is_stale() {
    let store = MemStore::default();
    let now = dlmm_now_ms();
    seeded(&store, now).await;
    let (w, p) = (k(BOT_WALLET), k(POOL));
    let slot = store
        .get(&snapshot_key(&w, &p))
        .await
        .unwrap()
        .unwrap()
        .slot
        .unwrap();
    let hedge = |fence| {
        let store = &store;
        async move {
            let (o, _) = hedge_decide_obs_in_cycle(
                Some(store),
                &w,
                &p,
                &hedge_knobs(),
                None,
                false,
                now,
                fence,
            )
            .await;
            o.typed::<HedgeDecision>().unwrap()
        }
    };
    let at = hedge(Some(slot)).await;
    assert_ne!(
        at.action.guard(),
        Some(Guard::StaleInput),
        "{:?}",
        at.action
    );
    let after = hedge(Some(slot + 1)).await;
    assert_eq!(
        after.action.guard(),
        Some(Guard::StaleInput),
        "{:?}",
        after.action
    );
    assert!(format!("{:?}", after.action).contains("predates the wallet's last write"));
    let (o, _) = lp_decide_obs(
        Some(&store),
        &w,
        &p,
        &lp_knobs(),
        false,
        now,
        Some(slot + 1),
    )
    .await;
    let d: LpDecision = o.typed().unwrap();
    assert!(
        format!("{:?}", d.verdict).contains("predates the wallet's last write"),
        "{:?}",
        d.verdict
    );
}

#[tokio::test]
async fn an_unreadable_state_row_is_never_overwritten() {
    let store = MemStore::default();
    let now = dlmm_now_ms();
    seeded(&store, now).await;
    let (w, p) = (k(BOT_WALLET), k(POOL));
    let mut bad = Observation::of(
        "hedge_decide",
        &LpControllerState::new(BOT_WALLET, POOL),
        now,
        LP_STATE_TTL_MS,
        ObsSource::Live,
    );
    bad.data = json!({"committed_regime": 7});
    store.put(&bad).await.unwrap();
    // Regression (review #8): the decision ran on a fresh state (regime
    // `in`, no grace, no cooldown) as if nothing were wrong; it blocks.
    let (o, note) = hedge_decide_obs(Some(&store), &w, &p, &hedge_knobs(), true, now).await;
    assert!(note.unwrap().starts_with("lp_state NOT committed"));
    assert_eq!(o.features["commit"], json!("unreadable"));
    let d: HedgeDecision = o.typed().unwrap();
    assert_eq!(d.action.guard(), Some(Guard::InvalidRead), "{:?}", d.action);
    assert_eq!(d.trace.invalid_fields, vec!["lp_state".to_string()]);
    assert!(d.snapshot.is_some() && d.view.is_none());
    let (o, note) = lp_decide_obs(Some(&store), &w, &p, &lp_knobs(), true, now, None).await;
    assert!(note.unwrap().starts_with("lp_state NOT committed"));
    let d: LpDecision = o.typed().unwrap();
    assert_eq!(
        d.verdict,
        LpVerdict::Paused {
            reason: PauseReason::InvalidRead
        }
    );
    assert_eq!(d.invalid_fields, vec!["lp_state".to_string()]);
    assert_eq!(store.get(&state_key(&w, &p)).await.unwrap().unwrap(), bad);
    assert_line1(&o, &[BOT_WALLET, POOL]);
}

/// Regression (review #8, path 2): a body that no longer parses as an
/// observation read as "no row" and `commit = true` overwrote it.
#[tokio::test]
async fn an_unparseable_state_body_blocks_and_is_never_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteObservationStore::open(dir.path()).unwrap();
    let now = now_ms();
    let t = transport(true);
    t.push(Ok(gpa_reply(DLMM_SLOT, &[k(POSITION)])));
    snapshot(&t, &store, &req(BOT_WALLET), json!({}), &Jup::ok(), now).await;
    let (w, p) = (k(BOT_WALLET), k(POOL));
    let key = state_key(&w, &p);
    let db = dir.path().join(".tengu/observations.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute(
        "INSERT INTO observations(key, schema, observed_at_ms, slot, ttl_ms, status, body)
             VALUES (?1, 'lp_state/1', ?2, NULL, 1000, 'ok', '{\"key\": 1')",
        rusqlite::params![key, now],
    )
    .unwrap();
    let (o, note) = hedge_decide_obs(Some(&store), &w, &p, &hedge_knobs(), true, now).await;
    let d: HedgeDecision = o.typed().unwrap();
    assert_eq!(d.action.guard(), Some(Guard::InvalidRead), "{:?}", d.action);
    assert!(note.unwrap().starts_with("lp_state NOT committed"));
    let body: String = conn
        .query_row(
            "SELECT body FROM observations WHERE key = ?1",
            rusqlite::params![key],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(body, "{\"key\": 1", "never overwritten");
}

/// Regression (review #11 #15): commits were blind whole-row replaces;
/// a writer that read the state before another's commit wiped it.
#[tokio::test]
async fn a_commit_on_a_state_changed_since_the_read_is_a_conflict() {
    let store = MemStore::default();
    let now = dlmm_now_ms();
    seeded(&store, now).await;
    let (w, p) = (k(BOT_WALLET), k(POOL));
    // First commit: no row yet (expected none).
    let (o, note) = hedge_decide_obs(Some(&store), &w, &p, &hedge_knobs(), true, now).await;
    assert_eq!(o.features["commit"], json!("committed"), "{note:?}");
    // Writer A reads the state ...
    let read = read_state(Some(&store), &w, &p).await;
    let StateRead::Found { observed_at_ms, .. } = &read else {
        panic!("{read:?}")
    };
    let v1 = *observed_at_ms;
    // ... writer B commits in between (same ms: the version still moves).
    let (o, _) = lp_decide_obs(Some(&store), &w, &p, &lp_knobs(), true, now, None).await;
    assert_eq!(o.features["commit"], json!("committed"));
    let b = store.get(&state_key(&w, &p)).await.unwrap().unwrap();
    assert!(b.observed_at_ms > v1);
    // ... A's commit on its stale read: conflict, B's row kept.
    let mut next = read.state(&w, &p).unwrap();
    next.storm_active = !next.storm_active;
    let (outcome, text) = commit_state(
        Some(&store),
        names::HEDGE_DECIDE,
        true,
        &read,
        &next,
        now + 1,
    )
    .await
    .unwrap();
    assert_eq!(outcome, "conflict");
    assert!(text.contains("NOT committed (conflict)") && text.contains(BOT_WALLET));
    assert_eq!(store.get(&state_key(&w, &p)).await.unwrap().unwrap(), b);
    // A fresh read commits.
    let (o, _) = hedge_decide_obs(Some(&store), &w, &p, &hedge_knobs(), true, now + 2).await;
    assert_eq!(o.features["commit"], json!("committed"));
}

#[tokio::test]
async fn lp_decide_runs_the_storm_gate_on_the_price_row_samples() {
    let store = MemStore::default();
    let now = dlmm_now_ms();
    // A price ring 5 min long: 110 → 116.66 (+6 %) — a storm at 2 %.
    let jup = |usd: f64| {
        Field::ok(market::JupiterPrice {
            usd,
            block_id: None,
            change_24h_pct: None,
            liquidity_usd: None,
        })
    };
    let old = market::combine_price(
        ids::WSOL,
        jup(110.0),
        Field::Absent,
        None,
        None,
        now - 300_000,
    );
    let fresh = market::combine_price(
        ids::WSOL,
        jup(SOL_USD),
        Field::Absent,
        None,
        Some(&old),
        now - 1_000,
    );
    assert_eq!(fresh.samples.len(), 2);
    let row = Observation::of(
        "sol_price",
        &fresh,
        now - 1_000,
        PRICE_TTL_MS,
        ObsSource::Live,
    );
    store.put(&row).await.unwrap();
    let snap = seeded(&store, now).await;
    assert_eq!(snap.oracle.age_ms, Some(1_000), "the stored row was used");
    let (w, p) = (k(BOT_WALLET), k(POOL));

    let mut knobs = lp_knobs();
    knobs.trend_confirm_ms = 0;
    let (o, _) = lp_decide_obs(Some(&store), &w, &p, &knobs, false, now, None).await;
    let d: LpDecision = o.typed().unwrap();
    assert!(d.storm.active, "{:?}", d.storm);
    assert!(d.storm.move_5m_pct.unwrap() > 5.0);
    assert!(
        matches!(d.verdict, LpVerdict::Paused { .. }),
        "{:?}",
        d.verdict
    );
    // Storm off → the imbalanced fixture position recenters.
    knobs.storm_pct_5m = 0.0;
    let (o, note) = lp_decide_obs(Some(&store), &w, &p, &knobs, true, now, None).await;
    let d: LpDecision = o.typed().unwrap();
    assert_eq!(d.verdict.name(), "recenter", "{:?}", d.verdict);
    assert!(note.unwrap().starts_with("lp_state committed"));
    let st: LpControllerState = store
        .get(&state_key(&w, &p))
        .await
        .unwrap()
        .unwrap()
        .typed()
        .unwrap();
    assert_eq!(st.known_positions, vec![POSITION.to_string()]);
    assert_line1(&o, &[BOT_WALLET, POOL]);
}

// ── arguments ───────────────────────────────────────────────────

#[test]
fn knob_errors_name_every_missing_and_unknown_knob() {
    let full = toml_args("decide_hedge");
    assert!(DecideArgs::parse(&full, names::HEDGE_DECIDE).is_ok());
    let mut v = full.clone();
    let knobs = v["knobs"].as_object_mut().unwrap();
    knobs.remove("no_lp_grace_ms");
    knobs.remove("cap_mult");
    knobs.insert("band_binz".into(), json!(4));
    let e = DecideArgs::parse(&v, names::HEDGE_DECIDE)
        .unwrap_err()
        .to_string();
    assert!(
        e.starts_with("hedge_decide: knobs missing required field(s): "),
        "{e}"
    );
    for name in [
        "no_lp_grace_ms",
        "cap_mult",
        "unknown knob(s): band_binz",
        "no defaults",
    ] {
        assert!(e.contains(name), "{name} not in {e}");
    }
    let mut v = full.clone();
    v.as_object_mut().unwrap().remove("knobs");
    let e = DecideArgs::parse(&v, names::HEDGE_DECIDE)
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("'knobs' is required") && e.contains("trend_confirm_ms"),
        "{e}"
    );
    let mut v = full.clone();
    v["commit"] = json!("yes");
    assert!(DecideArgs::parse(&v, names::HEDGE_DECIDE)
        .unwrap_err()
        .to_string()
        .contains("'commit' must be true or false"));
    let mut v = full.clone();
    v["knobs"]["lp_input"] = json!("median");
    let a = DecideArgs::parse(&v, names::HEDGE_DECIDE).unwrap();
    assert!(HedgeKnobs::parse(&a.knobs).is_err());
    // LP knobs: a hedge-only knob is unknown there.
    let mut v = toml_args("decide_lp");
    v["knobs"]["no_lp_grace_ms"] = json!(1);
    let e = DecideArgs::parse(&v, names::LP_DECIDE)
        .unwrap_err()
        .to_string();
    assert!(e.contains("unknown knob(s): no_lp_grace_ms"), "{e}");
}

#[test]
fn lping_toml_carries_complete_production_knobs() {
    let h = hedge_knobs();
    assert_eq!(h.lp_input, crate::domain::lp::snapshot::LpInput::Midpoint);
    assert!(h.include_wallet_sol);
    assert_eq!((h.bin_count, h.band_bins), (28, 8));
    assert_eq!(
        (h.delta_threshold_sol, h.target_collateral_ratio),
        (0.25, 0.33)
    );
    assert_eq!((h.trend_confirm_ms, h.no_lp_grace_ms), (600_000, 300_000));
    let l = lp_knobs();
    assert_eq!(
        (l.imbalance_threshold, l.bin_count, l.storm_pct_5m),
        (0.92, 28, 2.0)
    );
    assert_eq!(
        (l.reentry_confirm_ms, l.reentry_tol_frac),
        (7_200_000, 0.20)
    );
    assert_eq!(l.trend_confirm_ms, h.trend_confirm_ms);
    for action in ["decide_hedge", "decide_lp"] {
        let a = toml_args(action);
        assert_eq!(a["commit"], json!(true), "{action}");
        assert_eq!(a["wallet"], json!(BOT_WALLET));
        assert_eq!(a["pool"], json!(POOL));
    }
}

#[tokio::test]
async fn decide_tools_run_through_execute_with_a_scope_check() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Arc::new(MemStore::default());
    seeded(&store, now_ms()).await;
    let shared = SolanaShared {
        store: Some(store.clone() as Arc<dyn ObservationStore>),
        ..Default::default()
    };
    let lp_tools = tools(&shared);
    let by_name = |n: &str| {
        lp_tools
            .iter()
            .find(|t| t.definition().name == n)
            .unwrap()
            .clone()
    };
    let h = TestHarness::new(tmp.path());
    let hedge = by_name(names::HEDGE_DECIDE);
    let out = hedge
        .execute(&toml_args("decide_hedge"), &h.ctx())
        .await
        .unwrap();
    let o = out.observation.clone().unwrap();
    assert_eq!(o.key, format!("hedge_decide/1:{BOT_WALLET}:{POOL}"));
    assert!(
        out.text.ends_with(&format!(
            "lp_state committed: lp_state/1:{BOT_WALLET}:{POOL}"
        )),
        "{}",
        out.text
    );
    let out = by_name(names::LP_DECIDE)
        .execute(&toml_args("decide_lp"), &h.ctx())
        .await
        .unwrap();
    assert!(out.observation.unwrap().key.starts_with("lp_decide/1:"));
    // No fs root → denied before any read.
    let denied = TestHarness::with_scope(tmp.path(), Default::default());
    assert!(hedge
        .execute(&toml_args("decide_hedge"), &denied.ctx())
        .await
        .is_err());
    let snap = by_name(names::LP_SNAPSHOT);
    assert!(snap
        .execute(&json!({"wallet": BOT_WALLET, "pool": POOL}), &denied.ctx())
        .await
        .is_err());
    // Bad arguments are errors, not observations.
    let e = snap
        .execute(&json!({"wallet": "nope", "pool": POOL}), &h.ctx())
        .await
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("'wallet' is not a valid Solana address") && e.ends_with("nope"),
        "{e}"
    );
}

// ── live (public mainnet; `cargo test --bin tengu live_lp_ -- --ignored --test-threads 1`) ──

use super::super::price::tests::Live;

fn live_tool(live: &Live, name: &str) -> Arc<dyn Tool> {
    tools(&live.shared)
        .into_iter()
        .find(|t| t.definition().name == name)
        .unwrap()
}

/// `lp_snapshot` twice (the second from the store); the first result.
async fn live_snapshot(live: &Live, wallet: &str) -> (Observation, LpSnapshot) {
    let tool = live_tool(live, names::LP_SNAPSHOT);
    let args = json!({"wallet": wallet, "pool": POOL});
    let out = tool.execute(&args, &live.harness.ctx()).await.unwrap();
    eprintln!(
        "{}",
        out.text.lines().take(3).collect::<Vec<_>>().join("\n")
    );
    let o = out.observation.unwrap();
    assert!(o.status.usable(), "{}", out.text);
    assert_line1(&o, &[wallet, POOL]);
    let again = tool.execute(&args, &live.harness.ctx()).await.unwrap();
    assert_eq!(again.observation.unwrap().source, ObsSource::Cache);
    let s = o.typed().unwrap();
    (o, s)
}

/// A decide tool with the production knobs of the TOML (commit false).
async fn live_decide(live: &Live, action: &str, tool: &str, wallet: &str) -> Observation {
    let mut args = toml_args(action);
    args["wallet"] = json!(wallet);
    args["commit"] = json!(false);
    let out = live_tool(live, tool)
        .execute(&args, &live.harness.ctx())
        .await
        .unwrap();
    eprintln!(
        "{}",
        out.text.lines().take(2).collect::<Vec<_>>().join("\n")
    );
    out.observation.unwrap()
}

#[tokio::test]
#[ignore]
async fn live_lp_snapshot_operator_wallet() {
    let live = Live::new();
    let (o, s) = live_snapshot(&live, BOT_WALLET).await;
    assert_eq!(o.status, ObsStatus::Ok, "{:?}", o.errors);
    // No LP and no perps on chain today (bot HANDOVER Session 42).
    assert!(
        matches!(s.discovery, Discovery::Empty { .. }),
        "{:?}",
        s.discovery
    );
    assert!(s.positions.is_empty(), "{:?}", s.positions);
    assert!(s.hedge.applicable);
    assert_eq!(
        (&s.hedge.long, &s.hedge.short),
        (&Field::Absent, &Field::Absent),
        "perps flat"
    );
    assert!(s.oracle.usd.unwrap() > 1.0, "{:?}", s.oracle);
    assert!(s.wallet_balances.native_sol.value().is_some());
    let store = live.shared.store.as_deref().unwrap();
    let price = store
        .get(&format!("price_oracle/1:{}", ids::WSOL))
        .await
        .unwrap();
    assert!(price.is_some(), "the inline oracle row is stored");

    let h = live_decide(&live, "decide_hedge", names::HEDGE_DECIDE, BOT_WALLET).await;
    let d: HedgeDecision = h.typed().unwrap();
    assert!(d.snapshot.is_some());
    assert_ne!(d.action.guard(), Some(Guard::InvalidRead), "{:?}", d.action);
    let l = live_decide(&live, "decide_lp", names::LP_DECIDE, BOT_WALLET).await;
    let d: LpDecision = l.typed().unwrap();
    assert!(d.snapshot.is_some());
    assert!(d.invalid_fields.is_empty(), "{:?}", d.invalid_fields);
}

#[tokio::test]
#[ignore]
async fn live_lp_snapshot_fixture_owner() {
    let live = Live::new();
    let (_, s) = live_snapshot(&live, OWNER).await;
    eprintln!(
        "positions={} anomalies={:?}",
        s.positions.len(),
        s.anomalies
    );
    // 3 positions at the fixture capture (slot 450102095).
    assert!(!s.positions.is_empty(), "{:?}", s.discovery);
    assert!(s.hedge.applicable);
    let h = live_decide(&live, "decide_hedge", names::HEDGE_DECIDE, OWNER).await;
    let d: HedgeDecision = h.typed().unwrap();
    assert!(
        [
            "none",
            "blocked",
            "increase_long",
            "increase_short",
            "decrease_long",
            "decrease_short"
        ]
        .contains(&d.action.name()),
        "{:?}",
        d.action
    );
    assert_ne!(d.action.guard(), Some(Guard::InvalidRead), "{:?}", d.action);
    assert!(d.view.is_some(), "the controller saw the snapshot");
    let l = live_decide(&live, "decide_lp", names::LP_DECIDE, OWNER).await;
    let d: LpDecision = l.typed().unwrap();
    assert_eq!(d.health.len(), s.positions.len());
    eprintln!("lp verdict {}", d.verdict.name());
}
