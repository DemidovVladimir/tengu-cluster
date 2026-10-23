//! Tests of `src/adapters/outbound/tools/solana/dlmm.rs`.

use super::*;
use std::sync::Arc;

use serde_json::json;

use crate::adapters::outbound::solana::plan::tests::{
    dlmm_now_ms, dlmm_transport, gpa_reply, k, methods, owner_positions, BOT_WALLET, DLMM_GOLDEN,
    DLMM_SLOT, OWNER, POOL,
};
use crate::adapters::outbound::solana::plan::{discovery_key, DISCOVERY_FOUND_TTL_MS};
use crate::adapters::outbound::solana::rpc::tests::{err_envelope, fake_rpc, FakeTransport};
use crate::adapters::outbound::tools::workspace::test_support::TestHarness;
use crate::application::observe::tests::MemStore;
use crate::domain::lp::dlmm::{Discovery, DiscoverySource};
use crate::domain::observation::{
    assert_features_ok, ErrorClass, ObsSource, ObsStatus, MAX_FEATURES, MAX_LINE1_CHARS,
};
use crate::domain::scope::ToolScope;

const EXT_POSITION: &str = "14JU64KbNMLmFiS8qHuZ9ZF1swmzmiBYCRZidM24rH1m";

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

fn golden() -> Value {
    serde_json::from_str(DLMM_GOLDEN).unwrap()
}

#[test]
fn args_are_validated_with_clear_errors() {
    let e = pubkey_arg(&json!({}), "dlmm_pool", "pool").unwrap_err();
    assert!(e.to_string().contains("'pool' is required"), "{e}");
    let e = pubkey_arg(&json!({"pool": "not-a-key"}), "dlmm_pool", "pool").unwrap_err();
    assert!(
        e.to_string()
            .contains("'pool' is not a valid Solana address"),
        "{e}"
    );
    assert!(e.to_string().ends_with("not-a-key"), "{e}");
    assert_eq!(
        pubkey_arg(&json!({"pool": format!(" {POOL} ")}), "t", "pool").unwrap(),
        k(POOL)
    );
    let req = PositionsReq::parse(&json!({
        "wallet": OWNER, "pool": POOL, "positions": [EXT_POSITION], "min_context_slot": 5
    }))
    .unwrap();
    assert_eq!(req.positions, vec![k(EXT_POSITION)]);
    assert_eq!(req.min_context_slot, Some(5));
    for bad in [
        json!({"wallet": OWNER, "pool": POOL, "positions": "x"}),
        json!({"wallet": OWNER, "pool": POOL, "positions": ["x"]}),
        json!({"wallet": OWNER, "pool": POOL, "min_context_slot": -1}),
        json!({"wallet": OWNER, "pool": POOL, "max_age_secs": "5"}),
        json!({"wallet": OWNER}),
    ] {
        assert!(PositionsReq::parse(&bad).is_err(), "{bad}");
    }
}

#[tokio::test]
async fn dlmm_pool_from_the_fixture_then_cache() {
    let t = dlmm_transport();
    let rpc = fake_rpc(&t);
    let store = MemStore::default();
    let now = dlmm_now_ms();
    let o = dlmm_pool_obs(&rpc, Some(&store), &k(POOL), &json!({}), now).await;
    assert_eq!(o.status, ObsStatus::Ok, "{:?}", o.errors);
    assert_eq!(o.key, format!("dlmm_pool/1:{POOL}"));
    assert_eq!(
        (o.source, o.slot, o.ttl_ms),
        (ObsSource::Live, Some(DLMM_SLOT), 5_000)
    );
    assert_line1(&o, &[POOL]);
    let st: DlmmPoolState = o.typed().unwrap();
    let g = golden();
    assert_eq!(st.active_id, -5373);
    let want = g["active_price"].as_f64().unwrap();
    assert!(
        (st.active_price / want - 1.0).abs() < 1e-9,
        "{} vs {want}",
        st.active_price
    );
    assert!(st.tvl_quote_onchain.is_some());
    assert!(st.depth.value().unwrap().missing_bin_arrays.is_empty());
    // Two reads, the second pinned to the first's slot.
    let gma = t.gma_params();
    assert_eq!(gma.len(), 2);
    assert_eq!(gma[1][1]["minContextSlot"], DLMM_SLOT);
    // Within 5 s: the typed row, no RPC.
    let c = dlmm_pool_obs(&rpc, Some(&store), &k(POOL), &json!({}), now + 4_000).await;
    assert_eq!(c.source, ObsSource::Cache);
    assert_eq!(t.requests().len(), 2);
    // max_age_secs = 0: live typed row AND live account rows.
    let l = dlmm_pool_obs(
        &rpc,
        Some(&store),
        &k(POOL),
        &json!({"max_age_secs": 0}),
        now + 4_000,
    )
    .await;
    assert_eq!(l.source, ObsSource::Live);
    assert_eq!(t.gma_params().len(), 4);
}

#[tokio::test]
async fn dlmm_pool_failures_are_error_observations_never_cached() {
    let store = MemStore::default();
    // Pool absent on chain.
    let t = Arc::new(FakeTransport::at_slot(3));
    let rpc = fake_rpc(&t);
    let o = dlmm_pool_obs(&rpc, Some(&store), &k(POOL), &json!({}), 0).await;
    assert_eq!(o.status, ObsStatus::Error);
    assert_eq!(
        (o.errors[0].field.as_str(), o.errors[0].class),
        ("pool", ErrorClass::NotApplicable)
    );
    assert_line1(&o, &[POOL]);
    // RPC quota: classified, not Err (live read: the absent-pool row is cached).
    t.push(Ok(err_envelope(-32429, "max usage reached")));
    let live = json!({"max_age_secs": 0});
    let o = dlmm_pool_obs(&rpc, Some(&store), &k(POOL), &live, 0).await;
    assert_eq!(
        (o.status, o.errors[0].field.as_str(), o.errors[0].class),
        (ObsStatus::Error, "accounts", ErrorClass::QuotaExhausted)
    );
    assert!(store.get(&o.key).await.unwrap().is_none());
}

#[tokio::test]
async fn dlmm_positions_gpa_discovery_values_the_owner_positions() {
    let t = dlmm_transport();
    let rpc = fake_rpc(&t);
    let store = MemStore::default();
    let now = dlmm_now_ms();
    let req = PositionsReq {
        wallet: k(OWNER),
        pool: k(POOL),
        positions: Vec::new(),
        min_context_slot: None,
    };
    t.push(Ok(gpa_reply(DLMM_SLOT, &owner_positions())));
    let o = dlmm_positions_obs(&rpc, Some(&store), &req, &json!({}), now).await;
    assert_eq!(o.status, ObsStatus::Ok, "{:?}", o.errors);
    assert_eq!(o.key, format!("dlmm_positions/1:{OWNER}:{POOL}"));
    assert_line1(&o, &[OWNER, POOL]);
    assert_eq!(
        methods(&t),
        [
            "getProgramAccounts",
            "getMultipleAccounts",
            "getMultipleAccounts"
        ]
    );
    let p: DlmmPositions = o.typed().unwrap();
    assert!(matches!(
        p.discovery,
        Discovery::Found {
            count: 3,
            source: DiscoverySource::Gpa,
            ..
        }
    ));
    assert_eq!(p.positions.len(), 3);
    // Exact SDK golden totals for the owner's positions.
    let g = golden();
    let gp = g["positions"].as_array().unwrap();
    for pos in &p.positions {
        let want = gp
            .iter()
            .find(|w| w["position"] == pos.position.as_str())
            .unwrap();
        let raw = |f: &str| want[f].as_str().unwrap().parse::<f64>().unwrap();
        let (x, y) = (raw("total_x_raw") / 1e9, raw("total_y_raw") / 1e6);
        assert!(
            (pos.amount_base - x).abs() <= 1e-9 * x.max(1.0),
            "{} base",
            pos.position
        );
        assert!(
            (pos.amount_quote - y).abs() <= 1e-9 * y.max(1.0),
            "{} quote",
            pos.position
        );
        assert!(pos.complete);
    }
    let exp = p.exposure.value().unwrap();
    assert_eq!(exp.position_count, 3);
    assert!(exp.base > 0.0 && exp.quote > 0.0);
    assert!(p.anomalies.iter().any(|a| matches!(
        a,
        crate::domain::lp::dlmm::DlmmAnomaly::MultiplePositions { count: 3 }
    )));
    // Discovery row (60 s) + typed row (10 s) stored.
    let row = store
        .get(&discovery_key(&req.wallet, &req.pool))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.ttl_ms, DISCOVERY_FOUND_TTL_MS);
    // Within 10 s: typed row.
    let c = dlmm_positions_obs(&rpc, Some(&store), &req, &json!({}), now + 9_000).await;
    assert_eq!(c.source, ObsSource::Cache);
    assert_eq!(t.requests().len(), 3);
    // After 10 s but within 60 s: cached discovery, fresh account reads.
    let d = dlmm_positions_obs(&rpc, Some(&store), &req, &json!({}), now + 20_000).await;
    assert_eq!(d.source, ObsSource::Live);
    let p: DlmmPositions = d.typed().unwrap();
    assert!(matches!(
        p.discovery,
        Discovery::Found {
            source: DiscoverySource::Cached,
            ..
        }
    ));
    assert_eq!(
        methods(&t)[3..],
        ["getMultipleAccounts", "getMultipleAccounts"]
    );
}

#[tokio::test]
async fn dlmm_positions_empty_wallet_is_absent() {
    let t = dlmm_transport();
    let rpc = fake_rpc(&t);
    let store = MemStore::default();
    let req = PositionsReq {
        wallet: k(BOT_WALLET),
        pool: k(POOL),
        positions: Vec::new(),
        min_context_slot: None,
    };
    t.push(Ok(gpa_reply(DLMM_SLOT, &[])));
    let o = dlmm_positions_obs(&rpc, Some(&store), &req, &json!({}), dlmm_now_ms()).await;
    assert_eq!(o.status, ObsStatus::Absent, "{:?}", o.errors);
    let p: DlmmPositions = o.typed().unwrap();
    assert!(matches!(p.discovery, Discovery::Empty { .. }));
    assert_eq!(p.exposure.value().unwrap().position_count, 0);
    assert_eq!(o.features["discovery"], json!("empty"));
    assert!(
        store.get(&o.key).await.unwrap().is_some(),
        "Absent rows are cached"
    );
}

#[tokio::test]
async fn dlmm_positions_explicit_keys_skip_gpa_and_are_never_stored() {
    let t = dlmm_transport();
    let rpc = fake_rpc(&t);
    let store = MemStore::default();
    let absent = k(BOT_WALLET);
    let req = PositionsReq {
        wallet: k(OWNER),
        pool: k(POOL),
        positions: vec![owner_positions()[0], k(EXT_POSITION), absent],
        min_context_slot: None,
    };
    let o = dlmm_positions_obs(&rpc, Some(&store), &req, &json!({}), dlmm_now_ms()).await;
    assert!(!methods(&t).contains(&"getProgramAccounts".to_string()));
    assert_eq!(o.status, ObsStatus::Partial);
    assert_eq!(o.ttl_ms, 0);
    let p: DlmmPositions = o.typed().unwrap();
    assert!(matches!(
        p.discovery,
        Discovery::Found {
            count: 3,
            source: DiscoverySource::Args,
            ..
        }
    ));
    assert_eq!(p.positions.len(), 1);
    // The other wallet's position and the non-position key are both reported, ids in full.
    assert!(
        p.errors.iter().any(|e| e.message.contains(EXT_POSITION)),
        "{:?}",
        p.errors
    );
    assert!(
        p.errors
            .iter()
            .any(|e| e.class == ErrorClass::NotApplicable && e.message.contains(BOT_WALLET)),
        "{:?}",
        p.errors
    );
    assert!(store.get(&o.key).await.unwrap().is_none());
    assert!(store
        .get(&discovery_key(&req.wallet, &req.pool))
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn dlmm_positions_discovery_failure_is_error_not_empty() {
    let t = dlmm_transport();
    let rpc = fake_rpc(&t);
    let store = MemStore::default();
    let req = PositionsReq {
        wallet: k(OWNER),
        pool: k(POOL),
        positions: Vec::new(),
        min_context_slot: None,
    };
    t.push(Ok(err_envelope(-32429, "max usage reached")));
    let o = dlmm_positions_obs(&rpc, Some(&store), &req, &json!({}), dlmm_now_ms()).await;
    assert_eq!(o.status, ObsStatus::Error);
    assert_eq!(o.errors[0].field, "discovery");
    assert_eq!(o.errors[0].class, ErrorClass::QuotaExhausted);
    assert!(
        !o.features.contains_key("lp_base"),
        "no exposure numbers on a failed discovery"
    );
    assert!(store.get(&o.key).await.unwrap().is_none());
}

/// Regression (review #7): gPA found a position on one node, the account
/// read went unpinned to a node behind it and returned `null`: the
/// position was dropped and the exposure read Ok(0).
#[tokio::test]
async fn a_discovered_position_the_read_misses_is_never_exposure_zero() {
    let t = dlmm_transport();
    let rpc = fake_rpc(&t);
    let store = MemStore::default();
    let now = dlmm_now_ms();
    let req = PositionsReq {
        wallet: k(BOT_WALLET),
        pool: k(POOL),
        positions: Vec::new(),
        min_context_slot: None,
    };
    // Not in the fake's accounts: the lagging node answers null.
    let opened = k("G18jKKXQwBbrHeiK3C9MRXhkHsLHf7XgCSisykV46EZa");
    t.push(Ok(gpa_reply(DLMM_SLOT + 2, &[opened])));
    let o = dlmm_positions_obs(&rpc, Some(&store), &req, &json!({}), now).await;
    // Read 1 is pinned to the gPA slot: the node behind it errors.
    assert_eq!(t.gma_params()[0][1]["minContextSlot"], DLMM_SLOT + 2);
    assert_eq!(o.status, ObsStatus::Error, "{:?}", o.errors);
    assert_eq!(o.errors[0].class, ErrorClass::Transient);
    assert!(!o.features.contains_key("lp_base"), "no exposure numbers");
    // The node caught up but the account is missing at the gPA's slot
    // (closed in between): exposure Error, never 0.
    t.slot
        .store(DLMM_SLOT + 2, std::sync::atomic::Ordering::SeqCst);
    t.push(Ok(gpa_reply(DLMM_SLOT + 2, &[opened])));
    let live = json!({"max_age_secs": 0});
    let o = dlmm_positions_obs(&rpc, Some(&store), &req, &live, now + 1_000).await;
    assert_eq!(methods(&t).last().unwrap(), "getMultipleAccounts");
    assert_eq!(o.status, ObsStatus::Error, "{:?}", o.errors);
    let p: DlmmPositions = o.typed().unwrap();
    let e = p.exposure.error().expect("exposure is an error");
    assert!(e.message.contains(&opened.to_string()), "{e:?}");
    assert!(!o.features.contains_key("lp_base"));
}

#[tokio::test]
async fn stale_cached_discovery_is_rediscovered() {
    let t = dlmm_transport();
    let rpc = fake_rpc(&t);
    let store = MemStore::default();
    let now = dlmm_now_ms();
    let mut req = PositionsReq {
        wallet: k(OWNER),
        pool: k(POOL),
        positions: Vec::new(),
        min_context_slot: None,
    };
    // The discovery row lists a key that is gone (never a position).
    let gone = k(BOT_WALLET);
    let mut listed = owner_positions();
    listed.push(gone);
    t.push(Ok(gpa_reply(DLMM_SLOT, &listed)));
    let first = dlmm_positions_obs(&rpc, Some(&store), &req, &json!({}), now).await;
    // A discovered key that does not read as a position: exposure unknown.
    assert_eq!(first.status, ObsStatus::Error, "{:?}", first.errors);
    assert!(first.errors.iter().any(|e| e.message.contains(BOT_WALLET)));
    let before = t.requests().len();
    // min_context_slot skips the typed row; discovery + acct rows at the
    // floor are reused, so the only RPC is the re-discovery gPA.
    req.min_context_slot = Some(DLMM_SLOT);
    t.push(Ok(gpa_reply(DLMM_SLOT, &owner_positions())));
    let args = json!({"min_context_slot": DLMM_SLOT});
    let o = dlmm_positions_obs(&rpc, Some(&store), &req, &args, now + 1_000).await;
    let p: DlmmPositions = o.typed().unwrap();
    assert!(matches!(
        p.discovery,
        Discovery::Found {
            count: 3,
            source: DiscoverySource::Gpa,
            ..
        }
    ));
    assert_eq!(o.status, ObsStatus::Ok, "{:?}", o.errors);
    assert_eq!(methods(&t)[before..], ["getProgramAccounts"]);
    let row = store
        .get(&discovery_key(&req.wallet, &req.pool))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.data["positions"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn min_context_slot_bypasses_rows_and_pins_reads() {
    let t = dlmm_transport();
    let rpc = fake_rpc(&t);
    let store = MemStore::default();
    let now = dlmm_now_ms();
    let mut req = PositionsReq {
        wallet: k(OWNER),
        pool: k(POOL),
        positions: Vec::new(),
        min_context_slot: None,
    };
    t.push(Ok(gpa_reply(DLMM_SLOT, &owner_positions())));
    dlmm_positions_obs(&rpc, Some(&store), &req, &json!({}), now).await;
    let before = t.requests().len();
    req.min_context_slot = Some(DLMM_SLOT);
    t.push(Ok(gpa_reply(DLMM_SLOT, &owner_positions())));
    let args = json!({"min_context_slot": DLMM_SLOT});
    let o = dlmm_positions_obs(&rpc, Some(&store), &req, &args, now + 1_000).await;
    assert_eq!(o.source, ObsSource::Live, "typed row not served");
    let after = &t.requests()[before..];
    // The discovery row is at DLMM_SLOT (not below the floor): reused.
    assert!(
        after.iter().all(|r| r["method"] == "getMultipleAccounts"),
        "{after:?}"
    );
    // Every account read ≥ DLMM_SLOT; the acct/1 rows at DLMM_SLOT are reused.
    assert!(after
        .iter()
        .all(|r| r["params"][1]["minContextSlot"] == DLMM_SLOT));
    // A floor above the node's slot: Transient, as an Error observation.
    req.min_context_slot = Some(DLMM_SLOT + 1);
    let o = dlmm_positions_obs(&rpc, Some(&store), &req, &json!({}), now + 2_000).await;
    assert_eq!(o.status, ObsStatus::Error);
    assert_eq!(o.errors[0].class, ErrorClass::Transient);
}

#[tokio::test]
async fn tools_run_through_execute_with_the_scope_gate() {
    let dir = tempfile::tempdir().unwrap();
    let shared = SolanaShared::default();
    let names: Vec<String> = tools(&shared)
        .iter()
        .map(|t| t.definition().name.clone())
        .collect();
    assert_eq!(names, [names::DLMM_POOL, names::DLMM_POSITIONS]);
    // Workspace outside fs_roots: refused before any read.
    let h = TestHarness::with_scope(dir.path(), ToolScope::default());
    let tool = DlmmPoolTool::new(shared.clone());
    let e = tool
        .execute(&json!({"pool": POOL}), &h.ctx())
        .await
        .unwrap_err();
    assert!(format!("{e:#}").contains("fs"), "{e:#}");
    // Bad argument: Err naming it.
    let h = TestHarness::new(dir.path());
    let e = DlmmPositionsTool::new(shared)
        .execute(&json!({"wallet": OWNER, "pool": "nope"}), &h.ctx())
        .await
        .unwrap_err();
    assert!(e.to_string().contains("'pool'"), "{e}");
}

// ── live (public mainnet; `cargo test --bin tengu live_ -- --ignored --test-threads 1`) ──

mod live {
    use super::*;
    use crate::adapters::outbound::egress;
    use crate::adapters::outbound::observations::SqliteObservationStore;
    use crate::adapters::outbound::solana::http_json::fetch_json;
    use crate::adapters::outbound::solana::rpc::REQUEST_TIMEOUT;
    use crate::domain::lp::market::{jupiter_price_url, parse_jupiter_price};
    use crate::domain::solana::ids;

    const POOL_2: &str = "BGm1tav58oGcsQJehL9WXBFXF7D27vZsKefj4xJKD5Y";

    pub(crate) fn harness(dir: &std::path::Path) -> TestHarness {
        let scope = ToolScope {
            fs_roots: vec![dir.to_path_buf()],
            net_hosts: vec![
                "api.mainnet-beta.solana.com".into(),
                "lite-api.jup.ag".into(),
            ],
            ..Default::default()
        };
        let mut h = TestHarness::with_scope(dir, scope);
        h.http = egress::policy().tool_client(REQUEST_TIMEOUT).unwrap();
        h
    }

    pub(crate) fn shared(dir: &std::path::Path) -> SolanaShared {
        SolanaShared {
            store: Some(Arc::new(SqliteObservationStore::open(dir).unwrap())),
            ..Default::default()
        }
    }

    async fn run(tool: &dyn Tool, args: Value, h: &TestHarness) -> Observation {
        tool.execute(&args, &h.ctx())
            .await
            .unwrap()
            .observation
            .unwrap()
    }

    async fn jupiter_sol(h: &TestHarness) -> f64 {
        let (_, v) = fetch_json(&h.ctx(), &jupiter_price_url(ids::WSOL))
            .await
            .unwrap();
        parse_jupiter_price(&v, ids::WSOL).value().unwrap().usd
    }

    #[tokio::test]
    #[ignore]
    async fn live_dlmm_pool_matches_jupiter_and_caches() {
        let dir = tempfile::tempdir().unwrap();
        let h = harness(dir.path());
        let tool = DlmmPoolTool::new(shared(dir.path()));
        let sol = jupiter_sol(&h).await;
        for pool in [POOL, POOL_2] {
            let o = run(&tool, json!({"pool": pool}), &h).await;
            println!(
                "{}",
                o.render_text(o.observed_at_ms).lines().next().unwrap()
            );
            assert!(o.status.usable(), "{pool}: {:?}", o.errors);
            assert_eq!(o.source, ObsSource::Live);
            assert_line1(&o, &[pool]);
            let st: DlmmPoolState = o.typed().unwrap();
            assert!(st.pair.base_is_native_sol && st.pair.quote_is_usd, "{pool}");
            let dev = (st.active_price / sol - 1.0).abs();
            assert!(
                dev < 0.01,
                "{pool}: active {} vs jupiter {sol}",
                st.active_price
            );
            let c = run(&tool, json!({"pool": pool}), &h).await;
            assert_eq!(c.source, ObsSource::Cache, "{pool}: second call within 5 s");
        }
    }

    #[tokio::test]
    #[ignore]
    async fn live_dlmm_positions_fixture_owner_found() {
        let dir = tempfile::tempdir().unwrap();
        let h = harness(dir.path());
        let tool = DlmmPositionsTool::new(shared(dir.path()));
        let args = json!({"wallet": OWNER, "pool": POOL});
        let o = run(&tool, args.clone(), &h).await;
        println!(
            "{}",
            o.render_text(o.observed_at_ms).lines().next().unwrap()
        );
        assert!(o.status.usable(), "{:?}", o.errors);
        assert_line1(&o, &[OWNER, POOL]);
        let p: DlmmPositions = o.typed().unwrap();
        assert!(
            matches!(p.discovery, Discovery::Found { source: DiscoverySource::Gpa, count, .. } if count >= 1),
            "{:?}",
            p.discovery
        );
        assert!(!p.positions.is_empty());
        for pos in &p.positions {
            assert!(
                pos.amount_base + pos.amount_quote > 0.0,
                "{} empty",
                pos.position
            );
        }
        let e = p.exposure.value().unwrap();
        assert!(e.value_quote > 0.0);
        let c = run(&tool, args, &h).await;
        assert_eq!(c.source, ObsSource::Cache);
    }

    #[tokio::test]
    #[ignore]
    async fn live_dlmm_positions_bot_wallet_empty() {
        let dir = tempfile::tempdir().unwrap();
        let h = harness(dir.path());
        let tool = DlmmPositionsTool::new(shared(dir.path()));
        let args = json!({"wallet": BOT_WALLET, "pool": POOL});
        let o = run(&tool, args.clone(), &h).await;
        println!(
            "{}",
            o.render_text(o.observed_at_ms).lines().next().unwrap()
        );
        assert_eq!(o.status, ObsStatus::Absent, "{:?}", o.errors);
        assert_line1(&o, &[BOT_WALLET, POOL]);
        let p: DlmmPositions = o.typed().unwrap();
        assert!(
            matches!(p.discovery, Discovery::Empty { .. }),
            "{:?}",
            p.discovery
        );
        let c = run(&tool, args, &h).await;
        assert_eq!(c.source, ObsSource::Cache);
    }
}
