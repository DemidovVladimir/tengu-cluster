//! Tests of `src/domain/lp/snapshot/`.

//! Fixtures: `tests/fixtures/solana/{dlmm,perps,wallet,market}` (stage 2,
//! live mainnet captures) decoded through the real builders.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde_json::json;

use super::*;
use crate::domain::lp::dlmm::{self, DiscoverySource};
use crate::domain::lp::{market, perps, wallet};
use crate::domain::observation::{assert_features_ok, ObsSource, MAX_FEATURES, MAX_LINE1_CHARS};
use crate::domain::solana::{AccountRead, AccountSet, AccountState};

macro_rules! fixture {
    ($rel:literal) => {
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/solana/",
            $rel
        ))
    };
}

const WALLET: &str = "F3YvPiLdniRPGpeKrbeGWR2zg2wPpzVuvqBA5BBJBQ5S";
const POOL: &str = "5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS6";
/// Fixture position (bins -5440..-5371, active -5373: in range, near
/// the upper edge), re-owned by `WALLET` for the composed snapshot.
const POSITION: &str = "H9fmcxgheDvVSn9iUeRSvZPAgTY5WXqvroNpkZ2HCVRW";
/// Owner of the three standard positions in the dlmm fixture.
const FIXTURE_OWNER: &str = "JBggt27MzM4eohjumT9Tuec7MBoWAgDM4BJjkoisDUcs";
const FLAT_LONG: &str = "FqymRcB92t63jpwh7om4RLbxMNUGoHnZPQMkkAA8ksVY";
const FLAT_SHORT: &str = "6HFhuYzQGcqdj4NGwC6vfVETRvMA3pXaVeZnHgWSKsJK";
const BOTH_WALLET: &str = "2xxyBSRyi1KVxwuZcFkU74c8HvhjBdV8YJ6F4gkdKk3i";
const BOTH_LONG: &str = "2DNqvKcgnx5Huk6VkjeoZbjhna6hZd7GRuG8RCH2dPEV";
const BOTH_SHORT: &str = "HCZsYUEGtiGVvFJJYcuqhrq1L2MQ7GwWXmQRQSxFNWWX";
const ATA_USDC: &str = "D9ScKYy15cw1tpkkuwEnDKv62nCyuETwrvRSdP4usGg1";
const ATA_WSOL: &str = "E4PCnfEconGJW6vf7GDycEnkFe1VWC7teiNWJzQv3NTA";
const REQUEST: &str = "11q9teW5JiHhWeY8ak79i72C4qpDtppzVgH1ZeEUWp3";
/// A 44-char pool id for the max-length rendering checks.
const ID_44: &str = "G18jKKXQwBbrHeiK3C9MRXhkHsLHf7XgCSisykV46EZa";
/// dlmm fixture clock (`golden.json` `clock_unix_timestamp`), ms.
const NOW: i64 = 1_790_272_406_000;
/// perps fixture wall clock at capture.
const PERPS_NOW_S: i64 = 1_790_272_211;
const SOL_USD_AT_PERPS: f64 = 116.658_916_455_958_6;

fn pk(s: &str) -> Pubkey {
    s.parse().unwrap()
}

fn json_of(s: &str) -> Value {
    serde_json::from_str(s).unwrap()
}

/// One raw getMultipleAccounts response + its request key order.
fn gma_set(gma: &str, keys: &[String]) -> AccountSet {
    let v = json_of(gma);
    let slot = v["result"]["context"]["slot"].as_u64().unwrap();
    let values = v["result"]["value"].as_array().unwrap();
    assert_eq!(keys.len(), values.len());
    let mut set = AccountSet::default();
    for (k, a) in keys.iter().zip(values) {
        let state = if a.is_null() {
            AccountState::Absent
        } else {
            AccountState::Ok {
                owner: pk(a["owner"].as_str().unwrap()),
                lamports: a["lamports"].as_u64().unwrap(),
                data_b64: a["data"][0].as_str().unwrap().to_string(),
                executable: a["executable"].as_bool().unwrap(),
            }
        };
        set.insert(AccountRead {
            pubkey: pk(k),
            slot,
            state,
        });
    }
    set
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|k| k.as_str().unwrap().to_string())
        .collect()
}

fn dlmm_set() -> AccountSet {
    let meta = json_of(fixture!("dlmm/meta.json"));
    gma_set(fixture!("dlmm/gma.json"), &strings(&meta["keys"]))
}

fn dlmm_array_keys() -> Vec<(i64, Pubkey)> {
    let meta = json_of(fixture!("dlmm/meta.json"));
    meta["roles"]["bin_array_indexes"]
        .as_array()
        .unwrap()
        .iter()
        .zip(strings(&meta["roles"]["bin_arrays"]))
        .map(|(i, k)| (i.as_i64().unwrap(), pk(&k)))
        .collect()
}

/// The dlmm fixture with `POSITION` re-owned by `WALLET` (owner @40).
fn owned_dlmm_set() -> AccountSet {
    let mut set = dlmm_set();
    let read = set.get(&pk(POSITION)).unwrap().clone();
    let mut data = read.data().unwrap();
    data[40..72].copy_from_slice(&pk(WALLET).0);
    set.insert(AccountRead::from_bytes(
        pk(POSITION),
        read.slot,
        *read.owner().unwrap(),
        read.lamports().unwrap(),
        &data,
    ));
    set
}

fn perps_set() -> AccountSet {
    let meta = json_of(fixture!("perps/meta.json"));
    let keys: Vec<String> = meta["gma"]["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k["pubkey"].as_str().unwrap().to_string())
        .collect();
    gma_set(fixture!("perps/gma.json"), &keys)
}

fn wallet_set() -> AccountSet {
    let meta = json_of(fixture!("wallet/meta.json"));
    gma_set(
        fixture!("wallet/gma_base64.json"),
        &strings(&meta["files"]["gma_base64.json"]["keys"]),
    )
}

fn pool_state(set: &AccountSet) -> DlmmPoolState {
    dlmm::build_dlmm_pool(set, &pk(POOL), &dlmm_array_keys(), NOW).unwrap()
}

fn positions_of(set: &AccountSet, wallet: &str) -> DlmmPositions {
    dlmm::build_positions(
        set,
        &pk(wallet),
        &pk(POOL),
        Discovery::Found {
            count: 1,
            source: DiscoverySource::Gpa,
            at_ms: NOW,
        },
    )
    .unwrap()
}

fn perps_flat() -> PerpsState {
    perps::build_perps(
        &perps_set(),
        &pk(WALLET),
        &pk(FLAT_LONG),
        &pk(FLAT_SHORT),
        Some(SOL_USD_AT_PERPS),
        PERPS_NOW_S,
    )
}

fn wallet_inv() -> WalletInventory {
    let set = wallet_set();
    let w = pk(WALLET);
    let token = ids::key(ids::TOKEN);
    let atas = [
        (ids::key(ids::WSOL), pk(ATA_WSOL), token),
        (ids::key(ids::USDC), pk(ATA_USDC), token),
    ];
    let balances = wallet::build_wallet_balances(&set, &w, &atas, &BTreeMap::new());
    wallet::build_wallet_inventory(
        w,
        set.slot_max,
        wallet::lamports_from_set(&set, &w),
        Field::Absent,
        Field::Absent,
        balances,
    )
}

/// `price_oracle/1:<wSOL>` built from the Jupiter fixture at `at_ms`.
fn oracle_obs(at_ms: i64) -> Observation {
    let v = json_of(fixture!("market/jupiter_price_v3.json"));
    let jup = market::parse_jupiter_price(&v, ids::WSOL);
    let p = market::combine_price(ids::WSOL, jup, Field::Absent, None, None, at_ms);
    Observation::of(
        "sol_price",
        &p,
        at_ms,
        market::PRICE_TTL_MS,
        ObsSource::Live,
    )
}

/// The composed fixture snapshot: `WALLET` with one in-range position,
/// a flat hedge, 2.748145289 SOL + 107.808931 USDC, a 2 s old price.
fn composed() -> LpSnapshot {
    static SNAP: OnceLock<LpSnapshot> = OnceLock::new();
    SNAP.get_or_init(|| {
        let set = owned_dlmm_set();
        compose_snapshot(
            &pk(WALLET),
            &pool_state(&set),
            &positions_of(&set, WALLET),
            Some(&perps_flat()),
            &wallet_inv(),
            Some(&oracle_obs(NOW - 2_000)),
            NOW,
        )
    })
    .clone()
}

fn meta_of(s: &LpSnapshot, age_ms: u64) -> ObsMeta {
    Observation::of(
        "lp_snapshot",
        s,
        NOW - age_ms as i64,
        LP_SNAPSHOT_TTL_MS,
        ObsSource::Live,
    )
    .meta(NOW)
}

/// Bot code defaults (`env.ts:336-454`) except a 0.5 SOL fixed band so
/// the fixture LP (1.86 SOL) is out of band.
fn hedge_knobs() -> HedgeKnobs {
    HedgeKnobs {
        target_delta_sol: 0.0,
        delta_threshold_sol: 0.5,
        band_bins: 0,
        bin_count: 20,
        cap_mult: 1.25,
        max_notional_usd: 0.0,
        min_collateral_ratio: 0.15,
        target_collateral_ratio: 1.0,
        carry_cap_bps: 5000.0,
        cooldown_ms: 600_000,
        lp_input: LpInput::Live,
        include_wallet_sol: false,
        min_wallet_sol: 0.2,
        rent_reserve_sol: 0.1,
        max_divergence_bps: 50.0,
        max_snapshot_age_secs: 30,
        trend_confirm_ms: 0,
        no_lp_grace_ms: 0,
    }
}

fn lp_knobs() -> LpKnobs {
    LpKnobs {
        imbalance_threshold: 0.9,
        bin_count: 20,
        storm_pct_5m: 0.0,
        trend_confirm_ms: 0,
        reentry_confirm_ms: 0,
        reentry_tol_frac: 0.15,
        max_divergence_bps: 50.0,
        max_snapshot_age_secs: 30,
        min_wallet_sol: 0.2,
        rent_reserve_sol: 0.1,
    }
}

fn fresh_state() -> LpControllerState {
    LpControllerState::new(WALLET, POOL)
}

fn hedge(
    s: &LpSnapshot,
    k: &HedgeKnobs,
    st: &LpControllerState,
) -> (HedgeDecision, LpControllerState) {
    decide_hedge(s, &meta_of(s, 1_000), 1_000, k, st, NOW)
}

fn lp(
    s: &LpSnapshot,
    k: &LpKnobs,
    st: &LpControllerState,
    samples: &[PriceSample],
) -> (LpDecision, LpControllerState) {
    decide_lp(s, &meta_of(s, 1_000), 1_000, k, st, samples, NOW)
}

fn err(field: &str) -> ReadError {
    ReadError::new(field, ErrorClass::Timeout, "rpc timed out")
}

fn guard_of(d: &HedgeDecision) -> Option<Guard> {
    d.action.guard()
}

/// `next` is `st` but for the position observation
/// (`last_position_seen_ms`, `no_lp_since_ms`, `updated_at_ms`).
fn only_observed(next: &LpControllerState, st: &LpControllerState) -> bool {
    let mut n = next.clone();
    n.last_position_seen_ms = st.last_position_seen_ms;
    n.no_lp_since_ms = st.no_lp_since_ms;
    n.updated_at_ms = st.updated_at_ms;
    n == *st
}

fn check_observed<T: Observed>(v: &T) {
    let f = v.features();
    assert_features_ok(&f);
    assert!(f.len() <= MAX_FEATURES, "{} features", f.len());
    let o = Observation::of("test", v, NOW, 5_000, ObsSource::Live);
    let text = o.render_text(NOW + 12_345);
    let line1 = text.lines().next().unwrap();
    assert!(
        line1.chars().count() <= MAX_LINE1_CHARS,
        "{} chars: {line1}",
        line1.chars().count()
    );
    assert!(
        v.headline().chars().count() <= 165,
        "headline {} chars: {}",
        v.headline().chars().count(),
        v.headline()
    );
    assert!(o.typed::<T>().is_ok(), "round trip {}", T::SCHEMA);
}

// ── composition ────────────────────────────────────────────────────

#[test]
fn composes_the_fixture_snapshot_through_the_real_builders() {
    let s = composed();
    assert_eq!(s.wallet, WALLET);
    assert_eq!(s.pool, POOL);
    assert!(s.pair.base_is_native_sol && s.pair.quote_is_usd);
    assert_eq!(s.pair.base_mint, ids::WSOL);
    assert_eq!(s.pair.quote_mint, ids::USDC);
    assert!(s.pool_enabled);
    assert_eq!(s.active_id, -5373);
    assert_eq!(s.bin_step, 4);
    assert!((s.pool_price - 116.627_489_413_434_7).abs() < 1e-9);
    let jup_usd = 116.608_416_065_151_2;
    // Oracle: the Jupiter fixture row, 2 s old at compose time.
    assert_eq!(s.oracle.key, format!("price_oracle/1:{}", ids::WSOL));
    assert!((s.oracle.usd.unwrap() - jup_usd).abs() < 1e-12);
    assert_eq!(s.oracle.age_ms, Some(2_000));
    assert_eq!(s.oracle.source, Some(PriceSource::Jupiter));
    assert!(s.oracle.degraded, "single source");
    let bps = s.pool_vs_oracle_bps.unwrap();
    let want = (116.627_489_413_434_7 / jup_usd - 1.0) * 1e4;
    assert!((bps - want).abs() < 1e-6, "{bps} vs {want}");
    // Wallet.
    let w = &s.wallet_balances;
    assert_eq!(w.native_sol.value(), Some(&2.748_145_289));
    assert_eq!(w.quote.value().unwrap().raw, "107808931");
    assert_eq!(w.wsol.value().unwrap().raw, "0", "wSOL ATA absent = 0");
    assert_eq!(w.base, w.wsol, "SOL pair: base mint is wSOL");
    // Position + exposure.
    assert!(matches!(s.discovery, Discovery::Found { count: 1, .. }));
    assert_eq!(s.positions.len(), 1);
    let p = &s.positions[0];
    assert_eq!(p.position, POSITION);
    assert!(p.in_range && p.complete);
    assert_eq!((p.lower_bin_id, p.upper_bin_id), (-5440, -5371));
    let e = s.exposure.value().unwrap();
    assert!((e.base - 1.863_240_114).abs() < 1e-12);
    assert!((e.quote - 14_782.778_388).abs() < 1e-9);
    // Hedge: flat, both custodies read.
    assert!(s.hedge.applicable);
    assert_eq!(s.hedge.long, Field::Absent);
    assert_eq!(s.hedge.short, Field::Absent);
    assert!(s.hedge.sol_custody.value().is_some());
    assert!(s.hedge.usdc_custody.value().is_some());
    assert_eq!(s.hedge.max_request_execution_sec.value(), Some(&45));
    assert!(s.anomalies.is_empty(), "{:?}", s.anomalies);
    // Slot range spans the three captures.
    assert_eq!(s.slot, 450_101_361, "perps capture is the oldest");
    assert_eq!(s.slot_max, 450_102_095, "dlmm capture is the newest");
    assert_eq!(s.status(), ObsStatus::Ok, "{:?}", s.errors());
}

#[test]
fn watch_is_the_union_of_every_key_read() {
    let s = composed();
    for key in [
        POOL,
        ids::WSOL,
        ids::USDC,
        "EYj9xKw6ZszwpyNibHY7JD5o3QgTVrSdcBp1fMJhrR9o",
        "CoaxzEh8p5YyGLcj36Eo3cUThVJxeKCs7qvLAGDYwBcz",
        POSITION,
        // Bin arrays -78 / -77 hold the position and the depth bands.
        "HP15ZCgcgunsV9ypHKpDJSURFn4dn7i63uCNB4k7K5MV",
        "Vc6P6kjaRUgnQCwycL4QzTG3tiNR4CgC3gqWHvvMMJh",
        FLAT_LONG,
        FLAT_SHORT,
        ids::JUP_CUSTODY_SOL,
        ids::JUP_CUSTODY_USDC,
        ids::JLP_POOL,
        WALLET,
        ATA_USDC,
        ATA_WSOL,
    ] {
        assert!(s.watch.contains(&key.to_string()), "watch lacks {key}");
    }
    let mut sorted = s.watch.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted, s.watch, "sorted + unique");
    assert!(s.watch.iter().all(|k| k.parse::<Pubkey>().is_ok()));
}

#[test]
fn snapshot_observation_fits_the_render_and_feature_contract() {
    let s = composed();
    check_observed(&s);
    let h = s.headline();
    assert!(h.contains(WALLET) && h.contains(POOL), "{h}");
    assert!(
        h.contains("perps=flat") && h.contains("pos=1 in_range=1"),
        "{h}"
    );
    let o = Observation::of("lp_snapshot", &s, NOW, LP_SNAPSHOT_TTL_MS, ObsSource::Live);
    assert_eq!(o.key, format!("lp_snapshot/1:{WALLET}:{POOL}"));
    assert_eq!(o.slot, Some(450_101_361));
    let back: LpSnapshot = o.typed().unwrap();
    assert_eq!(back.positions.len(), 1);
    assert_eq!(back.watch, s.watch);
    let f = &o.features;
    assert_eq!(f["hedge_applicable"], json!(true));
    assert_eq!(f["perp_short_sol"], json!(0.0));
    assert_eq!(f["n_invalid_fields"], json!(0));
    assert!(
        f.get("collateral_ratio").is_none(),
        "flat: omitted, never Infinity"
    );
    // Max-length ids still fit line 1.
    let mut long = s.clone();
    long.pool = ID_44.into();
    long.oracle.usd = Some(0.000_012_345_678_9);
    long.pool_price = 123_456_789.123;
    check_observed(&long);
}

#[test]
fn missing_or_foreign_oracle_is_a_partial_read() {
    let set = owned_dlmm_set();
    let (pool, pos, perps, inv) = (
        pool_state(&set),
        positions_of(&set, WALLET),
        perps_flat(),
        wallet_inv(),
    );
    let none = compose_snapshot(&pk(WALLET), &pool, &pos, Some(&perps), &inv, None, NOW);
    assert_eq!(none.oracle.usd, None);
    assert_eq!(none.pool_vs_oracle_bps, None);
    assert_eq!(none.status(), ObsStatus::Partial);
    assert!(none.errors().iter().any(|e| e.field == "oracle"));
    // A USDC price row is not the base's oracle.
    let v = json_of(fixture!("market/jupiter_price_v3.json"));
    let usdc = market::combine_price(
        ids::USDC,
        market::parse_jupiter_price(&v, ids::USDC),
        Field::Absent,
        None,
        None,
        NOW,
    );
    let usdc_obs = Observation::of("sol_price", &usdc, NOW, 10_000, ObsSource::Live);
    let foreign = compose_snapshot(
        &pk(WALLET),
        &pool,
        &pos,
        Some(&perps),
        &inv,
        Some(&usdc_obs),
        NOW,
    );
    assert_eq!(foreign.oracle.usd, None);
    let e = foreign.oracle.error.as_ref().unwrap();
    assert_eq!(e.class, ErrorClass::Fatal);
    assert!(
        e.message.contains(ids::USDC) && e.message.contains(ids::WSOL),
        "{}",
        e.message
    );
}

#[test]
fn components_for_another_wallet_are_rejected_not_mixed() {
    let set = owned_dlmm_set();
    let pool = pool_state(&set);
    let foreign_pos = positions_of(&set, FIXTURE_OWNER);
    let s = compose_snapshot(
        &pk(WALLET),
        &pool,
        &foreign_pos,
        None,
        &wallet_inv(),
        Some(&oracle_obs(NOW)),
        NOW,
    );
    assert!(matches!(s.discovery, Discovery::Error { .. }));
    assert!(s.positions.is_empty() && s.exposure.is_error());
    // Perps not read on an applicable pool: sides are errors, not flat.
    assert_eq!(s.hedge.long.error().unwrap().class, ErrorClass::Fatal);
    assert_eq!(s.status(), ObsStatus::Partial);
    // Wallet inventory of another wallet.
    let mut inv = wallet_inv();
    inv.wallet = pk(FIXTURE_OWNER);
    let s = compose_snapshot(
        &pk(WALLET),
        &pool,
        &positions_of(&set, WALLET),
        Some(&perps_flat()),
        &inv,
        Some(&oracle_obs(NOW)),
        NOW,
    );
    assert!(s.wallet_balances.native_sol.is_error());
    assert!(s.wallet_balances.quote.is_error());
    assert!(!s.watch.contains(&WALLET.to_string()));
}

#[test]
fn anomalies_map_from_every_component() {
    let set = dlmm_set();
    let pool = pool_state(&set);
    // Three standard positions for the fixture owner → MultiplePositions.
    let pos = positions_of(&set, FIXTURE_OWNER);
    let mut both = perps::build_perps(
        &perps_set(),
        &pk(BOTH_WALLET),
        &pk(BOTH_LONG),
        &pk(BOTH_SHORT),
        Some(SOL_USD_AT_PERPS),
        PERPS_NOW_S,
    );
    both.wallet = FIXTURE_OWNER.into();
    let mut inv = wallet_inv();
    inv.wallet = pk(FIXTURE_OWNER);
    inv.lamports = Field::ok(0);
    let s = compose_snapshot(
        &pk(FIXTURE_OWNER),
        &pool,
        &pos,
        Some(&both),
        &inv,
        Some(&oracle_obs(NOW)),
        NOW,
    );
    assert!(s
        .anomalies
        .contains(&Anomaly::MultiplePositions { count: 3 }));
    assert!(s.anomalies.contains(&Anomaly::BothSidesOpen));
    assert!(s.anomalies.contains(&Anomaly::WalletSolZero));
    assert!(s.hedge.collateral_ratio.is_some());
    check_observed(&s);
    // Extended position + disabled pool + not-applicable hedge.
    let mut pool2 = pool.clone();
    pool2.enabled = false;
    pool2.pair.quote_is_usd = false;
    let ext = positions_of(&set, "D5HPLvKdJHwKGaAkn99aFDj4BJSAbXyexKececgRQWss");
    let mut inv2 = wallet_inv();
    inv2.wallet = pk("D5HPLvKdJHwKGaAkn99aFDj4BJSAbXyexKececgRQWss");
    let s = compose_snapshot(
        &pk("D5HPLvKdJHwKGaAkn99aFDj4BJSAbXyexKececgRQWss"),
        &pool2,
        &ext,
        None,
        &inv2,
        None,
        NOW,
    );
    assert!(s.anomalies.contains(&Anomaly::ExtendedPosition {
        position: "14JU64KbNMLmFiS8qHuZ9ZF1swmzmiBYCRZidM24rH1m".into()
    }));
    assert!(s.anomalies.contains(&Anomaly::PoolDisabled));
    assert!(s.anomalies.contains(&Anomaly::HedgeNotApplicable));
    // Not applicable: missing perps / oracle do not make it Partial.
    assert!(!s.hedge.applicable && !s.oracle.needed);
    assert_eq!(s.status(), ObsStatus::Ok, "{:?}", s.errors());
    assert_eq!(s.cycle_price(), Some(s.pool_price));
    assert_eq!(s.perps_label(), "n/a");
}

#[test]
fn pending_request_joins_the_watch_list_in_order() {
    let mut s = composed();
    s.set_pending_request(
        &pk(REQUEST),
        Field::ok(RequestStatus {
            position_request: REQUEST.into(),
            exists: true,
            executed: false,
            age_secs: Some(3),
            expired: false,
        }),
    );
    assert!(s.watch.contains(&REQUEST.to_string()));
    let mut sorted = s.watch.clone();
    sorted.sort();
    assert_eq!(sorted, s.watch);
    assert_eq!(s.features()["pending_request"], json!(true));
    s.set_pending_request(&pk(REQUEST), Field::err(err("x")));
    assert_eq!(
        s.hedge
            .pending_request
            .as_ref()
            .unwrap()
            .error()
            .unwrap()
            .field,
        "hedge.pending_request"
    );
    assert_eq!(s.status(), ObsStatus::Partial);
}

// ── hedge_decide: parity with the core ─────────────────────────────

#[test]
fn flat_hedge_in_range_position_matches_hedge_decide_exactly() {
    let s = composed();
    let k = hedge_knobs();
    let (d, next) = hedge(&s, &k, &fresh_state());
    // The equivalent input, assembled by hand (jupiterPerpsEngine.ts:840-913).
    let e = s.exposure.value().unwrap();
    let price = s.oracle.usd.unwrap();
    let full = e.base + e.quote / price;
    let sol_apr = s.hedge.sol_custody.value().unwrap().borrow_apr_pct;
    let usdc_apr = s.hedge.usdc_custody.value().unwrap().borrow_apr_pct;
    let usdc = s.wallet_balances.quote.value().unwrap().ui;
    assert!((usdc - 107.808_931).abs() < 1e-12);
    let expected = HedgeInput {
        lp_sol: e.base,
        long_sol: 0.0,
        short_sol: 0.0,
        long_notional_usd: 0.0,
        short_notional_usd: 0.0,
        long_collateral_usd: 0.0,
        short_collateral_usd: 0.0,
        carry_cost_bps_long: sol_apr * 100.0,
        carry_cost_bps_short: usdc_apr * 100.0,
        oracle_price_usd: Some(price),
        wallet_sol: 2_748_145_289.0 / 1e9,
        wallet_reserve_sol: 0.2 + 0.1,
        wallet_usdc: usdc,
        target_delta_sol: 0.0,
        band_sol: 0.5,
        carry_cap_bps: 5000.0,
        max_hedge_notional_usd: full * price * 1.25,
        min_collateral_ratio: 0.15,
        target_collateral_ratio: 1.0,
        now_ms: NOW,
        last_action_at_ms: None,
        cooldown_ms: 600_000,
    };
    assert_eq!(d.input.as_ref(), Some(&expected));
    let core = hedge::decide(&expected);
    assert_eq!(d.action, HedgeAction::from_core(&core));
    // Short 1.86 SOL wanted; the USDC wallet (107.81) caps it (BUG-013).
    match &d.action {
        HedgeAction::IncreaseShort {
            size_usd,
            collateral_tokens,
            adjust_sol,
        } => {
            assert!((size_usd - usdc).abs() < 1e-9);
            assert!((collateral_tokens - usdc).abs() < 1e-9);
            assert!((adjust_sol + usdc / price).abs() < 1e-12);
        }
        other => panic!("expected increase_short, got {other:?}"),
    }
    assert_eq!(d.trace.clamped_by, vec!["collateral".to_string()]);
    assert_eq!(d.trace.projected_ratio, Some(1.0));
    let v = d.view.as_ref().unwrap();
    assert_eq!(v.band_source, BandSource::Floor);
    assert_eq!(v.cap_source, CapSource::Auto);
    assert!(v.out_of_band && v.net_delta_sol == e.base);
    assert_eq!(next.updated_at_ms, NOW);
    assert_eq!(
        next.committed_regime,
        LpRegime::In,
        "live mode keeps the regime"
    );
    check_observed(&d);
    let o = Observation::of("hedge_decide", &d, NOW, 0, ObsSource::Live);
    assert_eq!(o.key, format!("hedge_decide/1:{WALLET}:{POOL}"));
    assert_eq!(o.features["action"], json!("increase_short"));

    // `data.order` = the action as `jup_perps_order` arguments.
    let order = PerpsOrder {
        side: Side::Short,
        action: PerpsOrderKind::Increase,
        size_usd: usdc,
        collateral: usdc,
        max_notional_usd: v.max_notional_usd,
    };
    assert_eq!(d.order.as_ref(), Some(&order));
    assert_eq!(o.data["order"]["side"], json!("short"));
    assert_eq!(o.data["order"]["action"], json!("increase"));
    assert_eq!(o.data["order"]["size_usd"], json!(usdc));

    // Decrease / close: the side's current notional keeps the cap > 0
    // when the auto cap is gone (LP closed) — unwinding never blocks.
    let mut unwind = d.clone();
    unwind.view.as_mut().unwrap().max_notional_usd = 0.0;
    unwind.input.as_mut().unwrap().short_notional_usd = 140.0;
    unwind.input.as_mut().unwrap().long_notional_usd = 30.0;
    unwind.action = HedgeAction::DecreaseShort {
        size_usd: 140.0,
        entire_position: true,
        withdraw_collateral_usd: 45.0,
        adjust_sol: 1.2,
    };
    let close = unwind.perps_order().unwrap();
    assert_eq!(
        (
            close.side,
            close.action,
            close.collateral,
            close.max_notional_usd
        ),
        (Side::Short, PerpsOrderKind::Close, 45.0, 140.0)
    );
    unwind.action = HedgeAction::DecreaseLong {
        size_usd: 20.0,
        entire_position: false,
        withdraw_collateral_usd: 5.0,
        adjust_sol: -0.2,
    };
    let dec = unwind.perps_order().unwrap();
    assert_eq!(
        (dec.side, dec.action),
        (Side::Long, PerpsOrderKind::Decrease)
    );
    // A decrease with nothing open on that side is no order.
    unwind.input.as_mut().unwrap().long_notional_usd = 0.0;
    assert_eq!(unwind.perps_order(), None);
    // An increase needs a positive cap of its own.
    unwind.action = d.action.clone();
    assert_eq!(unwind.perps_order(), None);
    // No action, no order.
    unwind.action = HedgeAction::None {
        reason: "in band".into(),
    };
    assert_eq!(unwind.perps_order(), None);
    let blocked = HedgeDecision::without_snapshot(WALLET, POOL, &d.knobs, "no row");
    assert_eq!(blocked.order, None);
    let o = Observation::of("hedge_decide", &blocked, NOW, 0, ObsSource::Live);
    assert!(
        o.data.get("order").is_none(),
        "no order key without an order"
    );
}

#[test]
fn core_block_reasons_map_to_guards() {
    let base = HedgeInput {
        lp_sol: 10.0,
        long_sol: 0.0,
        short_sol: 0.0,
        long_notional_usd: 0.0,
        short_notional_usd: 0.0,
        long_collateral_usd: 0.0,
        short_collateral_usd: 0.0,
        carry_cost_bps_long: 1200.0,
        carry_cost_bps_short: 1200.0,
        oracle_price_usd: Some(100.0),
        wallet_sol: 10.0,
        wallet_reserve_sol: 0.3,
        wallet_usdc: 1_000_000.0,
        target_delta_sol: 0.0,
        band_sol: 0.5,
        carry_cap_bps: 5000.0,
        max_hedge_notional_usd: 12_000.0,
        min_collateral_ratio: 0.15,
        target_collateral_ratio: 1.0,
        now_ms: 0,
        last_action_at_ms: None,
        cooldown_ms: 0,
    };
    let cases: [(&str, HedgeInput, Guard); 5] = [
        (
            "oracle",
            HedgeInput {
                oracle_price_usd: None,
                ..base.clone()
            },
            Guard::Oracle,
        ),
        (
            "carry",
            HedgeInput {
                carry_cost_bps_short: 6000.0,
                ..base.clone()
            },
            Guard::Carry,
        ),
        (
            "headroom",
            HedgeInput {
                max_hedge_notional_usd: 5.0,
                ..base.clone()
            },
            Guard::Headroom,
        ),
        (
            "collateral",
            HedgeInput {
                wallet_usdc: 5.0,
                ..base.clone()
            },
            Guard::Collateral,
        ),
        (
            "ratio",
            HedgeInput {
                min_collateral_ratio: 0.5,
                target_collateral_ratio: 0.2,
                ..base.clone()
            },
            Guard::CollateralRatio,
        ),
    ];
    for (name, input, want) in cases {
        let a = HedgeAction::from_core(&hedge::decide(&input));
        assert_eq!(a.guard(), Some(want), "{name}: {a:?}");
    }
    assert_eq!(Guard::of_core_reason("something new"), Guard::Core);
}

// ── hedge_decide: gate table ───────────────────────────────────────

#[test]
fn invalid_reads_block_before_anything_else() {
    type Mutate = fn(&mut LpSnapshot);
    let cases: [(&str, Mutate); 8] = [
        ("discovery", |s| {
            s.discovery = Discovery::Error {
                error: err("discovery"),
                fallback_count: 0,
            };
        }),
        ("exposure", |s| s.exposure = Field::err(err("exposure"))),
        ("positions.complete", |s| s.positions[0].complete = false),
        ("wallet.native_sol", |s| {
            s.wallet_balances.native_sol = Field::err(err("wallet.native_sol"))
        }),
        ("wallet.quote", |s| {
            s.wallet_balances.quote = Field::err(err("wallet.quote"))
        }),
        ("oracle.usd", |s| s.oracle.usd = None),
        ("hedge.short", |s| {
            s.hedge.short = Field::err(err("hedge.short"))
        }),
        ("hedge.usdc_custody", |s| {
            s.hedge.usdc_custody = Field::err(err("hedge.usdc_custody"))
        }),
    ];
    let state = fresh_state();
    for (field, mutate) in cases {
        let mut s = composed();
        mutate(&mut s);
        // Every later gate would also fire: InvalidRead wins.
        s.pool_vs_oracle_bps = Some(500.0);
        let (d, next) = decide_hedge(
            &s,
            &meta_of(&s, 99_000),
            99_000,
            &hedge_knobs(),
            &state,
            NOW,
        );
        assert_eq!(guard_of(&d), Some(Guard::InvalidRead), "{field}");
        assert!(
            d.trace.invalid_fields.contains(&field.to_string()),
            "{field}: {:?}",
            d.trace.invalid_fields
        );
        assert!(d.view.is_none() && d.input.is_none(), "{field}");
        if field.starts_with("discovery") || field.starts_with("positions") || field == "exposure" {
            assert_eq!(
                next, state,
                "{field}: no whole position read, nothing recorded"
            );
        } else {
            assert!(
                only_observed(&next, &state),
                "{field}: only the observation"
            );
            assert_eq!(next.last_position_seen_ms, Some(NOW - 99_000), "{field}");
        }
        check_observed(&d);
    }
    // Pending request unreadable, or its expiry unknown.
    let mut s = composed();
    s.set_pending_request(&pk(REQUEST), Field::err(err("req")));
    let (d, _) = hedge(&s, &hedge_knobs(), &state);
    assert_eq!(
        d.trace.invalid_fields,
        vec!["hedge.pending_request".to_string()]
    );
    let mut s = composed();
    s.hedge.max_request_execution_sec = Field::err(err("pool"));
    let (d, _) = hedge(&s, &hedge_knobs(), &state);
    assert!(
        guard_of(&d) != Some(Guard::InvalidRead),
        "no request: max exec unused"
    );
    s.set_pending_request(&pk(REQUEST), Field::ok(request(true, false, 1)));
    let (d, _) = hedge(&s, &hedge_knobs(), &state);
    assert_eq!(
        d.trace.invalid_fields,
        vec!["hedge.max_request_execution_sec".to_string()]
    );
}

fn request(exists: bool, executed: bool, age_secs: i64) -> RequestStatus {
    RequestStatus {
        position_request: REQUEST.into(),
        exists,
        executed,
        age_secs: exists.then_some(age_secs),
        expired: false,
    }
}

#[test]
fn wallet_sol_zero_blocks_only_when_wallet_sol_is_hedged() {
    let mut s = composed();
    s.wallet_balances.native_sol = Field::ok(0.0);
    let mut k = hedge_knobs();
    k.include_wallet_sol = true;
    let (d, _) = hedge(&s, &k, &fresh_state());
    assert_eq!(guard_of(&d), Some(Guard::WalletSolZero));
    k.include_wallet_sol = false;
    let (d, _) = hedge(&s, &k, &fresh_state());
    assert_ne!(guard_of(&d), Some(Guard::WalletSolZero));
}

#[test]
fn hedge_not_applicable_outside_sol_usdc() {
    let mut s = composed();
    s.hedge.applicable = false;
    s.hedge.long = Field::err(err("hedge.long"));
    s.oracle.usd = None;
    let (d, next) = hedge(&s, &hedge_knobs(), &fresh_state());
    assert_eq!(guard_of(&d), Some(Guard::NotApplicable));
    assert!(
        d.trace.invalid_fields.is_empty(),
        "perps/oracle not critical"
    );
    assert!(only_observed(&next, &fresh_state()));
}

#[test]
fn pending_request_blocks_until_max_execution_plus_grace() {
    // JLP pool max request execution = 45 s (fixture) + 15 s grace.
    let cases = [
        (request(true, false, 10), Some(Guard::PendingRequest)),
        (request(true, false, 59), Some(Guard::PendingRequest)),
        (request(true, false, 60), None),
        (request(true, true, 10), None),
        (request(false, false, 0), None),
    ];
    for (r, want) in cases {
        let mut s = composed();
        s.set_pending_request(&pk(REQUEST), Field::ok(r.clone()));
        let (d, next) = hedge(&s, &hedge_knobs(), &fresh_state());
        match want {
            Some(g) => {
                assert_eq!(guard_of(&d), Some(g), "{r:?}");
                assert!(d.view.is_some() && d.input.is_some(), "view computed");
                assert!(only_observed(&next, &fresh_state()));
            }
            None => assert_eq!(d.action.name(), "increase_short", "{r:?}"),
        }
    }
}

#[test]
fn divergence_and_staleness_block_after_the_view() {
    let k = hedge_knobs();
    for (bps, blocked) in [(80.0, true), (-80.0, true), (50.0, false), (-40.0, false)] {
        let mut s = composed();
        s.pool_vs_oracle_bps = Some(bps);
        let (d, _) = hedge(&s, &k, &fresh_state());
        assert_eq!(guard_of(&d) == Some(Guard::Divergence), blocked, "{bps}");
    }
    let mut s = composed();
    s.oracle.age_ms = Some(0);
    let (d, _) = decide_hedge(&s, &meta_of(&s, 31_000), 31_000, &k, &fresh_state(), NOW);
    assert_eq!(guard_of(&d), Some(Guard::StaleInput));
    let (d, _) = decide_hedge(&s, &meta_of(&s, 30_000), 30_000, &k, &fresh_state(), NOW);
    assert_ne!(guard_of(&d), Some(Guard::StaleInput), "30 s is not > 30 s");
    // The composed price row was 2 s old: 2 s + 30 s > 30 s.
    let s = composed();
    let (d, _) = decide_hedge(&s, &meta_of(&s, 30_000), 30_000, &k, &fresh_state(), NOW);
    assert_eq!(guard_of(&d), Some(Guard::StaleInput));
    // The price row ages with the snapshot: 29 s + 2 s > 30 s.
    let mut s = composed();
    s.oracle.age_ms = Some(29_000);
    let (d, _) = decide_hedge(&s, &meta_of(&s, 2_000), 2_000, &k, &fresh_state(), NOW);
    match &d.action {
        HedgeAction::Blocked { guard, reason } => {
            assert_eq!(*guard, Guard::StaleInput);
            assert!(reason.contains(ids::WSOL), "{reason}");
        }
        other => panic!("{other:?}"),
    }
    assert!(d.trace.headroom_usd.is_some(), "trace on a computed view");
}

#[test]
fn in_band_and_cooldown_come_from_the_core() {
    let s = composed();
    let mut k = hedge_knobs();
    k.band_bins = 4;
    k.delta_threshold_sol = 2.0;
    let (d, _) = hedge(&s, &k, &fresh_state());
    assert_eq!(
        d.action,
        HedgeAction::None {
            reason: "in band".into()
        }
    );
    let v = d.view.unwrap();
    assert_eq!(
        v.band_source,
        BandSource::Auto,
        "LP full value / 20 × 4 > 2"
    );
    assert!(!v.out_of_band);
    // A live mutation 1 s ago starts the 600 s cooldown; a dry one does not.
    let mut st = fresh_state();
    st.last_hedge_action = Some(HedgeActionRecord {
        at_ms: NOW - 1_000,
        action: "increase_short".into(),
        live: true,
        signatures: Vec::new(),
        position_request: None,
        counter: None,
    });
    let (d, _) = hedge(&s, &hedge_knobs(), &st);
    match &d.action {
        HedgeAction::None { reason } => assert!(reason.starts_with("cooldown"), "{reason}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(d.view.unwrap().cooldown_remaining_ms, Some(599_000));
    st.last_hedge_action.as_mut().unwrap().live = false;
    let (d, _) = hedge(&s, &hedge_knobs(), &st);
    assert_eq!(d.action.name(), "increase_short");
}

#[test]
fn venue_permission_blocks_a_disallowed_increase() {
    let mut s = composed();
    if let Field::Ok { value } = &mut s.hedge.sol_custody {
        value.allow_increase = false;
    }
    let (d, next) = hedge(&s, &hedge_knobs(), &fresh_state());
    assert_eq!(guard_of(&d), Some(Guard::VenuePermission));
    assert_eq!(next.updated_at_ms, NOW, "post-core check: state advances");
}

#[test]
fn midpoint_clamps_and_the_recenter_freeze_holds_the_regime() {
    // The fixture position sits 2 bins under its upper edge: ~1.4 % of
    // its value is SOL, below the 2 % enter-above line → regime Above.
    let s = composed();
    let mut k = hedge_knobs();
    k.lp_input = LpInput::Midpoint;
    let (d, next) = hedge(&s, &k, &fresh_state());
    let v = d.view.as_ref().unwrap();
    assert_eq!(v.computed_regime, Some(LpRegime::Above));
    assert_eq!(v.regime_outcome, Some(RegimeOutcome::Committed));
    assert_eq!(v.lp_delta_used, 0.0, "above the range: no LP delta");
    assert_eq!(d.action.name(), "none");
    assert_eq!(next.committed_regime, LpRegime::Above);
    assert_eq!(next.pending_regime, None);
    // Imbalance timer running, no storm → the recenter owns the signal.
    let mut st = fresh_state();
    st.imbalance_since_ms = Some(NOW - 5_000);
    let (d, next) = hedge(&s, &k, &st);
    let v = d.view.as_ref().unwrap();
    assert_eq!(v.regime_outcome, Some(RegimeOutcome::Frozen));
    assert_eq!(v.regime, LpRegime::In);
    let e = s.exposure.value().unwrap();
    let mid = hedge::lp_midpoint_sol(e.base, e.quote, v.price_usd);
    assert_eq!(v.lp_delta_used, mid);
    assert_eq!(next.committed_regime, LpRegime::In);
    assert_eq!(next.pending_regime.unwrap().regime, LpRegime::Above);
    assert_eq!(
        d.action,
        HedgeAction::from_core(&hedge::decide(d.input.as_ref().unwrap()))
    );
    // A storm lifts the freeze.
    st.storm_active = true;
    let (_, next) = hedge(&s, &k, &st);
    assert_eq!(next.committed_regime, LpRegime::Above);
}

#[test]
fn trend_confirm_ms_delays_the_clamp_regime_commit() {
    // Same Above candidate as above, now with a 5-minute confirmation.
    let s = composed();
    let mut k = hedge_knobs();
    k.lp_input = LpInput::Midpoint;
    k.trend_confirm_ms = 300_000;
    let (d, next) = hedge(&s, &k, &fresh_state());
    let v = d.view.as_ref().unwrap();
    assert_eq!(v.computed_regime, Some(LpRegime::Above));
    assert_eq!(v.regime_outcome, Some(RegimeOutcome::Pending));
    assert_eq!(v.regime, LpRegime::In, "the committed regime is priced");
    let e = s.exposure.value().unwrap();
    assert_eq!(
        v.lp_delta_used,
        hedge::lp_midpoint_sol(e.base, e.quote, v.price_usd)
    );
    assert_eq!(next.committed_regime, LpRegime::In);
    let pending = next.pending_regime.unwrap();
    assert_eq!((pending.regime, pending.since_ms), (LpRegime::Above, NOW));
    // Still inside the window: pending keeps its start.
    let mut st = next.clone();
    let (d, next) = decide_hedge(&s, &meta_of(&s, 1_000), 1_000, &k, &st, NOW + 299_999);
    assert_eq!(
        d.view.as_ref().unwrap().regime_outcome,
        Some(RegimeOutcome::Pending)
    );
    assert_eq!(next.pending_regime.unwrap().since_ms, NOW);
    // Window elapsed: commits.
    let (d, next) = decide_hedge(&s, &meta_of(&s, 1_000), 1_000, &k, &st, NOW + 300_000);
    assert_eq!(
        d.view.as_ref().unwrap().regime_outcome,
        Some(RegimeOutcome::Committed)
    );
    assert_eq!(next.committed_regime, LpRegime::Above);
    assert_eq!(next.pending_regime, None);
    // A storm bypasses the window.
    st = fresh_state();
    st.storm_active = true;
    let (_, next) = hedge(&s, &k, &st);
    assert_eq!(next.committed_regime, LpRegime::Above);
    // lp_input live never runs the clamp.
    k.lp_input = LpInput::Live;
    let (d, _) = hedge(&s, &k, &fresh_state());
    assert_eq!(d.view.as_ref().unwrap().regime_outcome, None);
}

/// Flat hedge, no position, 2.748145289 SOL in the wallet counted as
/// idle: the core wants to short ~2.45 SOL.
fn grace_knobs(grace_ms: u64) -> HedgeKnobs {
    let mut k = hedge_knobs();
    k.include_wallet_sol = true;
    k.no_lp_grace_ms = grace_ms;
    k
}

#[test]
fn no_lp_grace_holds_the_hedge_from_the_first_no_lp_read() {
    let s = no_position();
    let k = grace_knobs(300_000);
    // Last sighting 10 min ago (e.g. the evaluations in between were
    // gated): the clock starts at the first no-LP read, like the bot's
    // no-LP cycle counter — not at the last sighting.
    let mut st = fresh_state();
    st.last_position_seen_ms = Some(NOW - 600_000);
    let (d, next) = hedge(&s, &k, &st);
    match &d.action {
        HedgeAction::None { reason } => {
            assert!(reason.starts_with("no-LP grace"), "{reason}");
            assert!(reason.contains("1000 ms"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(next.no_lp_since_ms, Some(NOW - 1_000), "the snapshot time");
    assert!(
        only_observed(&next, &st),
        "a held evaluation records only that"
    );
    assert!(d.view.is_some() && d.input.is_some(), "view for the log");
    check_observed(&d);
    // The core would short the idle wallet SOL.
    let core = HedgeAction::from_core(&hedge::decide(d.input.as_ref().unwrap()));
    assert_eq!(core.name(), "increase_short", "{core:?}");
    // No state row at all (first run, purged or unreadable row): the
    // bot's counter starts at 0 on start-up — hold too.
    let (d, fresh_next) = hedge(&s, &k, &fresh_state());
    assert_eq!(d.action.name(), "none", "{:?}", d.action);
    assert_eq!(fresh_next.no_lp_since_ms, Some(NOW - 1_000));

    // Still no LP 4 min later: the clock keeps its start.
    let at = |now: i64| decide_hedge(&s, &meta_of(&s, 1_000), 1_000, &k, &next, now);
    let (d, later) = at(NOW + 240_000);
    assert_eq!(d.action.name(), "none", "{:?}", d.action);
    assert_eq!(later.no_lp_since_ms, Some(NOW - 1_000));
    // Grace elapsed since the first no-LP read → the core decides.
    let (d, _) = at(NOW + 299_000);
    assert_eq!(d.action.name(), "increase_short", "{:?}", d.action);
    // A position seen again stops the clock.
    let (_, seen) = hedge(&composed(), &k, &next);
    assert_eq!(
        (seen.last_position_seen_ms, seen.no_lp_since_ms),
        (Some(NOW - 1_000), None)
    );
    // Off (0), or a deliberate re-entry wait → no grace.
    let (d, _) = hedge(&s, &grace_knobs(0), &st);
    assert_eq!(d.action.name(), "increase_short");
    st.reentry = Some(ReentryWait::arm(110.0, 120.0, 116.0, NOW - 5_000));
    let (d, _) = hedge(&s, &grace_knobs(300_000), &st);
    assert_eq!(
        d.action.name(),
        "increase_short",
        "A15: the wallet bag stays hedged"
    );
    // Stale input still blocks first.
    st.reentry = None;
    let (d, _) = decide_hedge(
        &s,
        &meta_of(&s, 31_000),
        31_000,
        &grace_knobs(300_000),
        &st,
        NOW,
    );
    assert_eq!(guard_of(&d), Some(Guard::StaleInput));
}

#[test]
fn position_observations_are_recorded_gated_or_not() {
    let s = composed();
    let (_, next) = hedge(&s, &hedge_knobs(), &fresh_state());
    assert_eq!(
        next.last_position_seen_ms,
        Some(NOW - 1_000),
        "snapshot time"
    );
    let (_, next) = lp(&s, &lp_knobs(), &fresh_state(), &[]);
    assert_eq!(next.last_position_seen_ms, Some(NOW - 1_000));
    // Gated evaluations observe too (a pool lagging the oracle gates
    // both decisions for minutes in a fast move).
    let mut div = composed();
    div.pool_vs_oracle_bps = Some(80.0);
    let (d, next) = hedge(&div, &hedge_knobs(), &fresh_state());
    assert_eq!(guard_of(&d), Some(Guard::Divergence));
    assert_eq!(
        (next.last_position_seen_ms, next.updated_at_ms),
        (Some(NOW - 1_000), NOW)
    );
    assert!(only_observed(&next, &fresh_state()));
    let (d, next) = lp(&div, &lp_knobs(), &fresh_state(), &[]);
    assert_eq!(d.verdict.name(), "paused");
    assert_eq!(next.last_position_seen_ms, Some(NOW - 1_000));
    assert!(only_observed(&next, &fresh_state()));
    let mut bad = composed();
    bad.wallet_balances.native_sol = Field::err(err("wallet.native_sol"));
    let (d, next) = hedge(&bad, &hedge_knobs(), &fresh_state());
    assert_eq!(guard_of(&d), Some(Guard::InvalidRead));
    assert_eq!(next.last_position_seen_ms, Some(NOW - 1_000));
    // A position read that is not whole records nothing.
    let mut bad = no_position();
    bad.discovery = Discovery::Error {
        error: err("discovery"),
        fallback_count: 0,
    };
    let (_, next) = hedge(&bad, &hedge_knobs(), &fresh_state());
    assert_eq!(next, fresh_state());
    let (_, next) = lp(&bad, &lp_knobs(), &fresh_state(), &[]);
    assert_eq!(next, fresh_state());
    // Never moves backwards; a no-LP read older than the last sighting
    // starts no clock.
    let mut st = fresh_state();
    st.last_position_seen_ms = Some(NOW + 5);
    let (_, next) = hedge(&s, &hedge_knobs(), &st);
    assert_eq!(next.last_position_seen_ms, Some(NOW + 5));
    let (_, next) = hedge(&no_position(), &hedge_knobs(), &st);
    assert_eq!(
        (next.last_position_seen_ms, next.no_lp_since_ms),
        (Some(NOW + 5), None)
    );
    // A no-LP read starts the clock once (either decision).
    st.last_position_seen_ms = Some(NOW - 90_000);
    let (_, next) = lp(&no_position(), &lp_knobs(), &st, &[]);
    assert_eq!(
        (next.last_position_seen_ms, next.no_lp_since_ms),
        (Some(NOW - 90_000), Some(NOW - 1_000))
    );
    let (_, again) = hedge(&no_position(), &hedge_knobs(), &next);
    assert_eq!(again.no_lp_since_ms, Some(NOW - 1_000));
    // Round trip + a row written before the fields existed.
    let o = Observation::of("t", &next, NOW, LP_STATE_TTL_MS, ObsSource::Live);
    assert_eq!(o.typed::<LpControllerState>().unwrap(), next);
    assert_eq!(o.features["last_position_seen_ms"], json!(NOW - 90_000));
    assert_eq!(o.features["no_lp_since_ms"], json!(NOW - 1_000));
    let mut v = serde_json::to_value(fresh_state()).unwrap();
    v.as_object_mut().unwrap().remove("last_position_seen_ms");
    v.as_object_mut().unwrap().remove("no_lp_since_ms");
    let old: LpControllerState = serde_json::from_value(v).unwrap();
    assert_eq!(
        (old.last_position_seen_ms, old.no_lp_since_ms),
        (None, None)
    );
}

/// Finding #3 (BUG-023): a position that failed to read is never "no
/// position" / exposure 0 — built through the real builders.
#[test]
fn position_read_errors_block_both_decisions() {
    let compose = |set: &AccountSet, positions: &DlmmPositions| {
        compose_snapshot(
            &pk(WALLET),
            &pool_state(set),
            positions,
            Some(&perps_flat()),
            &wallet_inv(),
            Some(&oracle_obs(NOW - 2_000)),
            NOW,
        )
    };
    // (a) The wallet's position carries the PositionV2 discriminator
    // (pool + owner intact) but does not decode.
    let mut set = owned_dlmm_set();
    let read = set.get(&pk(POSITION)).unwrap().clone();
    let data = read.data().unwrap();
    set.insert(AccountRead::from_bytes(
        pk(POSITION),
        read.slot,
        *read.owner().unwrap(),
        read.lamports().unwrap(),
        &data[..100],
    ));
    let undecodable = compose(&set, &positions_of(&set, WALLET));
    assert!(undecodable.positions.is_empty());
    assert_eq!(
        undecodable.exposure.value().map(|e| e.base),
        Some(0.0),
        "the builder sums the positions it valued"
    );
    // (b) An explicitly requested key that is another wallet's position.
    let set = dlmm_set();
    let args = dlmm::build_positions(
        &set,
        &pk(WALLET),
        &pk(POOL),
        Discovery::Found {
            count: 1,
            source: DiscoverySource::Args,
            at_ms: NOW,
        },
    )
    .unwrap();
    let foreign = compose(&set, &args);
    assert!(foreign.positions.is_empty());
    // (c) Discovery found 2 keys, 1 valued, no error at all.
    let mut short = composed();
    short.discovery = Discovery::Found {
        count: 2,
        source: DiscoverySource::Gpa,
        at_ms: NOW,
    };
    let cases: [(&str, &LpSnapshot, &[&str]); 3] = [
        (
            "undecodable",
            &undecodable,
            &["positions", "discovery.count"],
        ),
        ("foreign", &foreign, &["positions", "discovery.count"]),
        ("short", &short, &["discovery.count"]),
    ];
    for (name, s, want) in cases {
        let (d, next) = hedge(s, &hedge_knobs(), &fresh_state());
        assert_eq!(
            guard_of(&d),
            Some(Guard::InvalidRead),
            "{name}: {:?}",
            d.action
        );
        for f in want {
            assert!(
                d.trace.invalid_fields.contains(&f.to_string()),
                "{name}: {f} in {:?}",
                d.trace.invalid_fields
            );
        }
        assert_eq!(next, fresh_state(), "{name}: nothing observed");
        let (l, next) = lp(s, &lp_knobs(), &fresh_state(), &[]);
        assert_eq!(
            l.verdict,
            LpVerdict::Paused {
                reason: PauseReason::InvalidRead
            },
            "{name}"
        );
        for f in want {
            assert!(l.invalid_fields.contains(&f.to_string()), "{name}: {f}");
        }
        assert_eq!(next, fresh_state(), "{name}");
    }
}

/// Finding #14: `hedge_decide`'s clamp confirm sees THIS read's storm
/// latch and imbalance timer (the bot updates both before the hedge in
/// the same cycle), not the ones an earlier `lp_decide` left.
#[test]
fn hedge_in_cycle_prices_the_clamp_with_this_reads_storm_and_imbalance() {
    // Composed: computed clamp regime Above (committed In); the position
    // is ~97 % quote (imbalanced at 0.9).
    let s = composed();
    let price = s.oracle.usd.unwrap();
    let mut k = hedge_knobs();
    k.lp_input = LpInput::Midpoint;
    k.trend_confirm_ms = 300_000;
    let mut lk = lp_knobs();
    lk.storm_pct_5m = 1.0;
    let in_cycle = |k: &HedgeKnobs, lk: &LpKnobs, st: &LpControllerState, ps: &[PriceSample]| {
        let c = LpCycle::new(lk, ps);
        decide_hedge_in_cycle(&s, &meta_of(&s, 1_000), 1_000, k, st, Some(&c), NOW)
    };
    let outcome = |d: &HedgeDecision| d.view.as_ref().unwrap().regime_outcome;

    // Storm onset this cycle (5 % in 5 min): ADR-023 bypass commits now.
    let storm = [PriceSample {
        t_ms: NOW - 300_000,
        usd: price / 1.05,
    }];
    let (d, next) = in_cycle(&k, &lk, &fresh_state(), &storm);
    assert_eq!(
        d.view.as_ref().unwrap().regime_signals,
        Some(RegimeSignals {
            storm_active: true,
            imbalance_pending: true,
            this_cycle: true
        })
    );
    assert_eq!(outcome(&d), Some(RegimeOutcome::Committed));
    assert_eq!(next.committed_regime, LpRegime::Above);
    assert!(!next.storm_active, "the latch stays lp_decide's to write");
    let (l, _) = lp(&s, &lk, &fresh_state(), &storm);
    assert!(l.storm.active, "lp_decide sees the same storm");
    // On lp_state's flags (no storm recorded yet): still pending.
    let (d, _) = hedge(&s, &k, &fresh_state());
    assert_eq!(outcome(&d), Some(RegimeOutcome::Pending));
    assert!(!d.view.as_ref().unwrap().regime_signals.unwrap().this_cycle);

    // ADR-025 freeze on this read's imbalance.
    k.trend_confirm_ms = 0;
    lk.storm_pct_5m = 0.0;
    let (d, next) = in_cycle(&k, &lk, &fresh_state(), &[]);
    assert_eq!(outcome(&d), Some(RegimeOutcome::Frozen), "imbalanced now");
    assert_eq!(next.committed_regime, LpRegime::In);
    let (d, _) = hedge(&s, &k, &fresh_state());
    assert_eq!(outcome(&d), Some(RegimeOutcome::Committed), "flags: none");
    // Balanced at 0.99 while lp_state keeps an hour-old timer.
    lk.imbalance_threshold = 0.99;
    let mut st = fresh_state();
    st.imbalance_since_ms = Some(NOW - 3_600_000);
    let (d, next) = in_cycle(&k, &lk, &st, &[]);
    assert_eq!(outcome(&d), Some(RegimeOutcome::Committed), "balanced now");
    assert_eq!(next.committed_regime, LpRegime::Above);
    let (d, _) = hedge(&s, &k, &st);
    assert_eq!(outcome(&d), Some(RegimeOutcome::Frozen), "stale flag");
    // Several positions: lp_decide pauses, so no recenter owns the
    // imbalance and nothing freezes the clamp.
    lk.imbalance_threshold = 0.9;
    let mut two = composed();
    let mut second = two.positions[0].clone();
    second.position = "Ui6V49xptXnsgay1h75MVch6zh2B3ahP2yb7iRTwX6x".into();
    two.positions.push(second);
    two.discovery = Discovery::Found {
        count: 2,
        source: DiscoverySource::Gpa,
        at_ms: NOW,
    };
    let c = LpCycle::new(&lk, &[]);
    let (d, _) = decide_hedge_in_cycle(&two, &meta_of(&two, 1_000), 1_000, &k, &st, Some(&c), NOW);
    let signals = d.view.as_ref().unwrap().regime_signals.unwrap();
    assert!(!signals.imbalance_pending && signals.this_cycle);
}

#[test]
fn missing_snapshot_blocks_as_stale() {
    let d =
        HedgeDecision::without_snapshot(WALLET, POOL, &hedge_knobs(), "no fresh lp_snapshot row");
    assert_eq!(guard_of(&d), Some(Guard::StaleInput));
    check_observed(&d);
    let d = LpDecision::without_snapshot(WALLET, POOL, &lp_knobs(), "no fresh lp_snapshot row");
    assert_eq!(d.verdict.name(), "blocked");
    check_observed(&d);
}

// ── lp_decide ──────────────────────────────────────────────────────

/// The composed snapshot with its position re-centred 10 bins either
/// side of the active bin (50 / 50 composition).
fn balanced() -> LpSnapshot {
    let mut s = composed();
    let (a, step) = (s.active_id, s.bin_step);
    let p = &mut s.positions[0];
    p.lower_bin_id = a - 10;
    p.upper_bin_id = a + 10;
    p.lower_price = gates::price_from_bin(a - 10, step, 9, 6);
    p.upper_price = gates::price_from_bin(a + 10, step, 9, 6);
    p.bins_below_active = 10;
    p.bins_above_active = 10;
    s
}

fn no_position() -> LpSnapshot {
    let mut s = composed();
    s.positions.clear();
    s.discovery = Discovery::Empty { at_ms: NOW };
    s.exposure = Field::ok(LpExposure {
        base: 0.0,
        quote: 0.0,
        claimable_base: 0.0,
        claimable_quote: 0.0,
        value_quote: 0.0,
        full_value_base: 0.0,
        position_count: 0,
    });
    s
}

#[test]
fn fixture_position_is_imbalanced_and_recenters() {
    let s = composed();
    let (d, next) = lp(&s, &lp_knobs(), &fresh_state(), &[]);
    let h = &d.health[0];
    assert!(h.in_range && h.is_imbalanced);
    assert!(h.quote_pct_linear >= 90.0, "{h:?}");
    assert_eq!((h.bins_to_lower_edge, h.bins_to_upper_edge), (67, 2));
    match &d.verdict {
        LpVerdict::Recenter {
            position,
            close_only,
            plan,
            reason,
        } => {
            assert_eq!(position, POSITION);
            assert!(!close_only);
            assert!(reason.contains("token Y"), "{reason}");
            let p = plan.as_ref().unwrap();
            assert_eq!((p.min_bin_id, p.max_bin_id, p.width), (-5383, -5364, 20));
            assert_eq!(p.bin_array_indexes, vec![-77]);
            assert!(p.lower_price < s.pool_price && s.pool_price < p.upper_price);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(next.imbalance_since_ms, Some(NOW));
    assert_eq!(next.known_positions, vec![POSITION.to_string()]);
    // Wallet 2.748 SOL (2.448 above reserves) + 107.81 USDC: SOL-heavy.
    let w = d.wallet_split.as_ref().unwrap();
    let sol_usd = (2.748_145_289 - 0.3) * s.oracle.usd.unwrap();
    let total = sol_usd + 107.808_931;
    assert!((w.base_value_ratio - sol_usd / total).abs() < 1e-9);
    assert!(!w.balanced);
    assert!((w.swap_needed_quote - (sol_usd / total - 0.5).abs() * total).abs() < 1e-9);
    check_observed(&d);
    // close-only when the re-entry wait is enabled.
    let mut k = lp_knobs();
    k.reentry_confirm_ms = 60_000;
    let (d, _) = lp(&s, &k, &fresh_state(), &[]);
    assert!(matches!(
        d.verdict,
        LpVerdict::Recenter {
            close_only: true,
            plan: None,
            ..
        }
    ));
}

#[test]
fn trend_confirm_waits_then_recenters() {
    let s = composed();
    let mut k = lp_knobs();
    k.trend_confirm_ms = 60_000;
    let (d, st) = lp(&s, &k, &fresh_state(), &[]);
    assert_eq!(
        d.verdict,
        LpVerdict::WaitTrend {
            since_ms: NOW,
            remaining_ms: 60_000
        }
    );
    let later = NOW + 61_000;
    let (d, st2) = decide_lp(&s, &meta_of(&s, 0), 0, &k, &st, &[], later);
    assert_eq!(d.verdict.name(), "recenter");
    assert_eq!(st2.imbalance_since_ms, Some(NOW));
    // Balanced again → the timer resets.
    let (d, st3) = lp(&balanced(), &k, &st, &[]);
    assert_eq!(d.verdict.name(), "hold");
    assert_eq!(st3.imbalance_since_ms, None);
}

#[test]
fn storm_pauses_an_imbalanced_position_and_latches() {
    let s = composed();
    let price = s.oracle.usd.unwrap();
    let mut k = lp_knobs();
    k.storm_pct_5m = 1.0;
    // Reference 5 min old, 5 % lower: move ≈ 5.3 % > 1 %.
    let samples = [PriceSample {
        t_ms: NOW - 300_000,
        usd: price / 1.05,
    }];
    let (d, next) = lp(&s, &k, &fresh_state(), &samples);
    assert_eq!(
        d.verdict,
        LpVerdict::Paused {
            reason: PauseReason::Storm
        }
    );
    assert!(d.storm.active && next.storm_active);
    assert!((d.storm.move_5m_pct.unwrap() - 5.0).abs() < 1e-6);
    // Balanced position in a storm just holds.
    let (d, _) = lp(&balanced(), &k, &fresh_state(), &samples);
    assert_eq!(d.verdict.name(), "hold");
    // No reference sample: the latch keeps its state.
    let mut st = fresh_state();
    st.storm_active = true;
    let (d, next) = lp(&balanced(), &k, &st, &[]);
    assert!(d.storm.move_5m_pct.is_none() && next.storm_active);
}

#[test]
fn lp_validity_gates_pause_or_block_and_record_only_the_observation() {
    let k = lp_knobs();
    let state = fresh_state();
    let mut s = composed();
    s.oracle.usd = None;
    let (d, next) = lp(&s, &k, &state, &[]);
    assert_eq!(
        d.verdict,
        LpVerdict::Paused {
            reason: PauseReason::InvalidRead
        }
    );
    assert!(d.invalid_fields.contains(&"oracle.usd".to_string()));
    assert!(only_observed(&next, &state));

    let mut s = composed();
    s.discovery = Discovery::Error {
        error: err("discovery"),
        fallback_count: 1,
    };
    let (d, _) = lp(&s, &k, &state, &[]);
    assert_eq!(
        d.verdict,
        LpVerdict::Paused {
            reason: PauseReason::InvalidRead
        }
    );

    let mut s = composed();
    s.wallet_balances.quote = Field::err(err("wallet.quote"));
    let (d, _) = lp(&s, &k, &state, &[]);
    assert_eq!(d.invalid_fields, vec!["wallet.quote".to_string()]);

    let mut s = balanced();
    let mut second = s.positions[0].clone();
    second.position = "Ui6V49xptXnsgay1h75MVch6zh2B3ahP2yb7iRTwX6x".into();
    s.positions.push(second);
    s.discovery = Discovery::Found {
        count: 2,
        source: DiscoverySource::Gpa,
        at_ms: NOW,
    };
    let (d, next) = lp(&s, &k, &state, &[]);
    assert_eq!(
        d.verdict,
        LpVerdict::Paused {
            reason: PauseReason::MultiplePositions
        }
    );
    assert!(only_observed(&next, &state));
    check_observed(&d);

    let mut s = balanced();
    s.pool_vs_oracle_bps = Some(-51.0);
    let (d, next) = lp(&s, &k, &state, &[]);
    assert_eq!(
        d.verdict,
        LpVerdict::Paused {
            reason: PauseReason::Divergence
        }
    );
    assert!(only_observed(&next, &state));

    let s = balanced();
    let (d, next) = decide_lp(&s, &meta_of(&s, 31_000), 31_000, &k, &state, &[], NOW);
    assert_eq!(d.verdict.name(), "blocked");
    assert!(only_observed(&next, &state));
}

#[test]
fn balanced_position_holds_and_clears_an_armed_wait() {
    let mut st = fresh_state();
    st.reentry = Some(ReentryWait::arm(100.0, 102.0, 101.0, NOW - 1));
    let (d, next) = lp(&balanced(), &lp_knobs(), &st, &[]);
    match &d.verdict {
        LpVerdict::Hold { reason } => assert!(reason.contains("within 90.00%"), "{reason}"),
        other => panic!("{other:?}"),
    }
    assert!(!d.health[0].is_imbalanced);
    assert!(
        next.reentry.is_none(),
        "a position on chain wins (self-heal)"
    );
}

#[test]
fn no_position_opens_a_centered_range_capped_at_70_bins() {
    let s = no_position();
    let (d, next) = lp(&s, &lp_knobs(), &fresh_state(), &[]);
    match &d.verdict {
        LpVerdict::Open { plan, reentry } => {
            assert!(!reentry);
            assert_eq!(plan.width, 20);
            assert_eq!(plan.min_bin_id, s.active_id - 10);
        }
        other => panic!("{other:?}"),
    }
    assert!(next.known_positions.is_empty() && next.imbalance_since_ms.is_none());
    let mut k = lp_knobs();
    k.bin_count = 80;
    let (d, _) = lp(&s, &k, &fresh_state(), &[]);
    match &d.verdict {
        LpVerdict::Open { plan, .. } => {
            assert_eq!(plan.width, 70);
            assert!(plan.clamped_to_max_width);
            assert_eq!(plan.max_bin_id - plan.min_bin_id + 1, 70);
        }
        other => panic!("{other:?}"),
    }
    check_observed(&d);
}

#[test]
fn open_is_blocked_on_a_disabled_pool_or_an_unfunded_wallet() {
    let mut s = no_position();
    s.pool_enabled = false;
    let (d, _) = lp(&s, &lp_knobs(), &fresh_state(), &[]);
    assert_eq!(d.verdict.name(), "blocked");
    let mut s = no_position();
    // 0.35 SOL < 0.2 + 0.1 reserves + 0.0575 position rent.
    s.wallet_balances.native_sol = Field::ok(0.35);
    let (d, _) = lp(&s, &lp_knobs(), &fresh_state(), &[]);
    match &d.verdict {
        LpVerdict::Blocked { reason } => assert!(reason.contains("position rent"), "{reason}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn reentry_wait_holds_rearms_then_opens() {
    let s = no_position();
    let price = s.oracle.usd.unwrap();
    let mut k = lp_knobs();
    k.reentry_confirm_ms = 60_000;
    let wait = |anchor: f64, since: i64| {
        let mut st = fresh_state();
        st.reentry = Some(ReentryWait {
            anchor_price: anchor,
            stable_since_ms: since,
            width_frac: 0.02,
            closed_at_ms: Some(since),
        });
        st
    };
    // Inside the 0.15 × 2 % = 0.3 % corridor, 10 s of 60 s held.
    let (d, next) = lp(&s, &k, &wait(price, NOW - 10_000), &[]);
    assert_eq!(
        d.verdict,
        LpVerdict::Reentry {
            decision: ReentryDecision::Hold
        }
    );
    let r = d.reentry.as_ref().unwrap();
    assert_eq!((r.held_ms, r.remaining_ms), (10_000, 50_000));
    assert!((r.tol_price_frac - 0.003).abs() < 1e-15);
    assert_eq!(next.reentry.as_ref().unwrap().anchor_price, price);
    // Breakout: 1 % away → re-anchor at the current price.
    let (d, next) = lp(&s, &k, &wait(price * 1.01, NOW - 50_000), &[]);
    assert_eq!(
        d.verdict,
        LpVerdict::Reentry {
            decision: ReentryDecision::Rearm {
                anchor_price: price,
                stable_since_ms: NOW
            }
        }
    );
    let w = next.reentry.as_ref().unwrap();
    assert_eq!((w.anchor_price, w.stable_since_ms), (price, NOW));
    // Held 70 s ≥ 60 s → open; the wait stays armed until the open lands.
    let (d, next) = lp(&s, &k, &wait(price, NOW - 70_000), &[]);
    assert!(matches!(d.verdict, LpVerdict::Open { reentry: true, .. }));
    assert!(next.reentry.is_some());
    check_observed(&d);
}

#[test]
fn non_usd_quote_runs_on_the_pool_price_and_ignores_oracle_samples() {
    let mut s = composed();
    s.pair.quote_is_usd = false;
    s.oracle.needed = false;
    s.oracle.usd = None;
    s.pool_vs_oracle_bps = None;
    let mut k = lp_knobs();
    k.storm_pct_5m = 1.0;
    let samples = [PriceSample {
        t_ms: NOW - 300_000,
        usd: 1.0,
    }];
    let (d, _) = lp(&s, &k, &fresh_state(), &samples);
    assert_eq!(d.cycle_price, Some(s.pool_price));
    assert!(
        d.storm.move_5m_pct.is_none(),
        "SOL/USD samples are not this pool's series"
    );
    assert_eq!(d.verdict.name(), "recenter");
}

// ── controller state ───────────────────────────────────────────────

#[test]
fn controller_state_is_an_observation_that_round_trips() {
    let mut st = LpControllerState::new(WALLET, ID_44);
    st.pending_regime = Some(PendingRegime {
        regime: LpRegime::Below,
        since_ms: NOW,
    });
    // Simple decimals: serde_json's default float parser is not
    // round-trip exact for every double.
    st.reentry = Some(ReentryWait {
        anchor_price: 111.5,
        stable_since_ms: NOW,
        width_frac: 0.02,
        closed_at_ms: Some(NOW),
    });
    st.last_hedge_action = Some(HedgeActionRecord {
            at_ms: NOW,
            action: "increase_short".into(),
            live: true,
            signatures: vec![
                "L8TEY2sSvscX2R2EBChD1p1o3HApdBUHqVfTJKLxJ4zK4M6pebL3diuYKcKjPF5deW7GF6DVnMdPU2tBwgz1Mfi".into(),
            ],
            position_request: Some(REQUEST.into()),
            counter: Some(473_577_047),
        });
    check_observed(&st);
    let o = Observation::of("lp_decide", &st, NOW, LP_STATE_TTL_MS, ObsSource::Live);
    assert_eq!(o.key, format!("lp_state/1:{WALLET}:{ID_44}"));
    assert_eq!(o.typed::<LpControllerState>().unwrap(), st);
    let w = ReentryWait::arm(110.0, 112.0, 111.0, NOW);
    assert!((w.width_frac - 2.0 / 111.0).abs() < 1e-15);
    assert_eq!(
        (w.anchor_price, w.stable_since_ms, w.closed_at_ms),
        (111.0, NOW, Some(NOW))
    );
    assert_eq!(
        ReentryWait::arm(0.0, 0.0, 111.0, NOW).width_frac,
        0.02,
        "fallback"
    );
    // A bare row (only the required fields) still decodes.
    let bare: LpControllerState = serde_json::from_value(json!({
        "wallet": WALLET, "pool": POOL, "committed_regime": "in"
    }))
    .unwrap();
    assert_eq!(bare, LpControllerState::new(WALLET, POOL));
}

// ── knobs ──────────────────────────────────────────────────────────

/// A valid sample value for every property of a defs.rs knob schema.
fn schema_sample(schema: &Value) -> Value {
    let mut o = serde_json::Map::new();
    for (name, p) in schema["properties"].as_object().unwrap() {
        let v = match p["type"].as_str() {
            Some("number") => {
                let lo = p["minimum"].as_f64().unwrap_or(0.0);
                match p["maximum"].as_f64() {
                    Some(hi) => json!((lo + hi) / 2.0),
                    None => json!(lo + 1.0),
                }
            }
            Some("integer") => json!(p["minimum"].as_i64().unwrap_or(0) + 1),
            Some("boolean") => json!(true),
            Some("string") => p["enum"][0].clone(),
            other => panic!("{name}: unexpected schema type {other:?}"),
        };
        o.insert(name.clone(), v);
    }
    Value::Object(o)
}

fn keys_of(v: &Value) -> Vec<String> {
    let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
    k.sort();
    k
}

#[test]
fn knobs_deserialize_exactly_the_defs_schema_properties() {
    use crate::adapters::outbound::tools::solana::defs::def;
    use crate::domain::tools as names;
    let hedge_schema = def(names::HEDGE_DECIDE).parameters["properties"]["knobs"].clone();
    let lp_schema = def(names::LP_DECIDE).parameters["properties"]["knobs"].clone();

    let sample = schema_sample(&hedge_schema);
    let k = HedgeKnobs::parse(&sample).unwrap();
    assert_eq!(
        keys_of(&serde_json::to_value(&k).unwrap()),
        keys_of(&sample)
    );
    for mode in hedge_schema["properties"]["lp_input"]["enum"]
        .as_array()
        .unwrap()
    {
        let mut v = sample.clone();
        v["lp_input"] = mode.clone();
        HedgeKnobs::parse(&v).unwrap();
    }
    for name in keys_of(&sample) {
        let mut v = sample.clone();
        v.as_object_mut().unwrap().remove(&name);
        let e = HedgeKnobs::parse(&v).unwrap_err();
        assert!(e.contains(&name), "missing {name}: {e}");
    }

    let sample = schema_sample(&lp_schema);
    let k = LpKnobs::parse(&sample).unwrap();
    assert_eq!(
        keys_of(&serde_json::to_value(&k).unwrap()),
        keys_of(&sample)
    );
    for name in keys_of(&sample) {
        let mut v = sample.clone();
        v.as_object_mut().unwrap().remove(&name);
        assert!(LpKnobs::parse(&v).is_err(), "missing {name}");
    }

    // Unknown fields are rejected.
    let mut v = schema_sample(&lp_schema);
    v["imbalance_threshhold"] = json!(0.9);
    assert!(LpKnobs::parse(&v)
        .unwrap_err()
        .contains("imbalance_threshhold"));
}

#[test]
fn knob_validation_names_the_knob() {
    let mut k = hedge_knobs();
    k.target_collateral_ratio = 0.1;
    let e = k.validate().unwrap_err();
    assert!(
        e.contains("knobs.target_collateral_ratio") && e.contains("knobs.min_collateral_ratio"),
        "{e}"
    );
    k = hedge_knobs();
    k.delta_threshold_sol = -1.0;
    assert!(k
        .validate()
        .unwrap_err()
        .contains("knobs.delta_threshold_sol"));
    k = hedge_knobs();
    k.cap_mult = f64::NAN;
    assert!(k.validate().unwrap_err().contains("finite"));
    let mut l = lp_knobs();
    l.imbalance_threshold = 1.5;
    assert!(l
        .validate()
        .unwrap_err()
        .contains("knobs.imbalance_threshold"));
    l = lp_knobs();
    l.bin_count = 0;
    assert!(l.validate().unwrap_err().contains("knobs.bin_count"));
    assert!(hedge_knobs().validate().is_ok() && lp_knobs().validate().is_ok());
    assert!(HedgeKnobs::parse(&json!({"target_delta_sol": 0}))
        .unwrap_err()
        .starts_with("knobs:"));
}
