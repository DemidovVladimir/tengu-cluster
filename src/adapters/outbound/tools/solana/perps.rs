//! `jup_perps` — a wallet's Jupiter perps SOL long / short positions and the
//! SOL / USDC custody rates + JLP pool `maxRequestExecutionSec`, from one
//! cache-through account read.
//!
//! | Step | Rule |
//! |---|---|
//! | Keys | `plan::perps_keys(wallet)`: long + short PDAs (`solana::jup_position_pda`), SOL + USDC custody, JLP pool |
//! | Oracle (PnL, liquidation distance) | `plan::oracle_usd`: usable `price_oracle/1:So11111111111111111111111111111111111111112` row ≤ 30 s old, else Jupiter lite price v3 inline (needs `lite-api.jup.ag` in `net_hosts`), else `None`; fetched concurrently with the account read |
//! | Build | `perps::build_perps` (flat side = `Absent`; a failed side = `Error`, never 0) |
//! | Cache row | `jup_perps/1:<wallet>`, 5 s |
//! | Whole-read failure (RPC error) | `Ok` with an `Error` observation (never cached); bad arguments are `Err` |

use std::future::Future;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use super::dlmm::{opt_u64_arg, pubkey_arg};
use super::{defs, SolanaShared};
use crate::adapters::outbound::solana::accounts::fetch_accounts;
use crate::adapters::outbound::solana::plan::{self, failed_observation, read_failure};
use crate::adapters::outbound::solana::rpc::SolanaRpc;
use crate::application::observe::observe;
use crate::domain::lp::perps::{build_perps, PerpsState};
use crate::domain::message::ToolDef;
use crate::domain::observation::{now_ms, CachePolicy, Observation, Observed, ReadError};
use crate::domain::solana::{ids, Pubkey};
use crate::domain::tools as names;
use crate::ports::observation::ObservationStore;
use crate::ports::tool::{Tool, ToolCtx, ToolOutput};

/// `jup_perps/1` TTL.
pub(crate) const JUP_PERPS_TTL_MS: u64 = 5_000;

pub(crate) fn tools(shared: &SolanaShared) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(JupPerpsTool::new(shared.clone()))]
}

pub(crate) struct JupPerpsTool {
    def: ToolDef,
    shared: SolanaShared,
}

impl JupPerpsTool {
    pub(crate) fn new(shared: SolanaShared) -> Self {
        Self {
            def: defs::def(names::JUP_PERPS),
            shared,
        }
    }
}

#[async_trait]
impl Tool for JupPerpsTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        ctx.scope.check_fs_write(ctx.workspace)?;
        let wallet = pubkey_arg(args, names::JUP_PERPS, "wallet")?;
        opt_u64_arg(args, names::JUP_PERPS, "max_age_secs")?;
        let now = now_ms();
        let store = self.shared.store.as_deref();
        let obs = match SolanaRpc::from_ctx(ctx) {
            Ok(rpc) => {
                let oracle = || plan::oracle_usd(ctx, store, ids::WSOL, now);
                jup_perps_obs(&rpc, store, &wallet, args, now, oracle).await
            }
            Err(e) => perps_failed(&wallet, read_failure("rpc", &e), now),
        };
        Ok(ToolOutput::observed(obs, now))
    }
}

fn perps_failed(wallet: &Pubkey, error: ReadError, now_ms: i64) -> Observation {
    failed_observation(
        names::JUP_PERPS,
        PerpsState::SCHEMA,
        &wallet.to_string(),
        format!("jup_perps wallet={wallet} error"),
        vec![error],
        now_ms,
    )
}

/// `jup_perps` over `rpc` + the cache. `oracle` resolves the SOL USD price
/// (only on a cache miss, concurrently with the account read).
pub(crate) async fn jup_perps_obs<F, Fut>(
    rpc: &SolanaRpc,
    store: Option<&dyn ObservationStore>,
    wallet: &Pubkey,
    args: &Value,
    now_ms: i64,
    oracle: F,
) -> Observation
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Option<f64>>,
{
    let policy = CachePolicy::new(
        PerpsState::SCHEMA,
        &wallet.to_string(),
        JUP_PERPS_TTL_MS,
        args,
    );
    let keys = plan::perps_keys(wallet);
    let fetched = observe(store, names::JUP_PERPS, &policy, now_ms, || async {
        let (set, oracle_usd) = tokio::join!(
            fetch_accounts(rpc, store, &keys.keys, policy.max_age_ms, None, now_ms),
            oracle()
        );
        let state = build_perps(
            &set?,
            wallet,
            &keys.long,
            &keys.short,
            oracle_usd,
            now_ms.div_euclid(1000),
        );
        Ok((state, JUP_PERPS_TTL_MS))
    })
    .await;
    fetched.unwrap_or_else(|e| perps_failed(wallet, read_failure("accounts", &e), now_ms))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;

    use crate::adapters::outbound::solana::plan::tests::{
        fixture_reads, k, BOT_WALLET, PERPS_GMA, PERPS_META,
    };
    use crate::adapters::outbound::solana::rpc::tests::{err_envelope, fake_rpc, FakeTransport};
    use crate::adapters::outbound::tools::workspace::test_support::TestHarness;
    use crate::application::observe::tests::MemStore;
    use crate::domain::lp::perps::Side;
    use crate::domain::observation::{
        assert_features_ok, ErrorClass, Field, ObsSource, ObsStatus, MAX_FEATURES, MAX_LINE1_CHARS,
    };
    use crate::domain::scope::ToolScope;

    /// Both sides open in the fixture.
    const HEDGED: &str = "2xxyBSRyi1KVxwuZcFkU74c8HvhjBdV8YJ6F4gkdKk3i";
    const SOL_AT_CAPTURE: f64 = 116.6589164559586;

    fn meta() -> Value {
        serde_json::from_str(PERPS_META).unwrap()
    }

    fn perps_transport() -> Arc<FakeTransport> {
        let m = meta();
        let slot = m["gma"]["slot"].as_u64().unwrap();
        let keys: Vec<Pubkey> = m["gma"]["keys"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| k(e["pubkey"].as_str().unwrap()))
            .collect();
        let t = Arc::new(FakeTransport::at_slot(slot));
        for r in fixture_reads(PERPS_GMA, &keys) {
            t.put_account(r);
        }
        t
    }

    fn now() -> i64 {
        meta()["gma"]["captured_unix_s"].as_i64().unwrap() * 1000
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

    #[tokio::test]
    async fn flat_wallet_from_the_fixture_then_cache() {
        let t = perps_transport();
        let rpc = fake_rpc(&t);
        let store = MemStore::default();
        let calls = AtomicUsize::new(0);
        let oracle = || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Some(SOL_AT_CAPTURE)
        };
        let wallet = k(BOT_WALLET);
        let o = jup_perps_obs(&rpc, Some(&store), &wallet, &json!({}), now(), oracle).await;
        assert_eq!(o.status, ObsStatus::Ok, "{:?}", o.errors);
        assert_eq!(o.key, format!("jup_perps/1:{BOT_WALLET}"));
        assert_eq!((o.source, o.ttl_ms), (ObsSource::Live, JUP_PERPS_TTL_MS));
        assert_line1(&o, &[BOT_WALLET]);
        let p: PerpsState = o.typed().unwrap();
        // Long PDA absent, short PDA exists with sizeUsd 0: both flat.
        assert_eq!(
            (p.long.clone(), p.short.clone()),
            (Field::Absent, Field::Absent)
        );
        assert_eq!(p.base_sol(Side::Long), Some(0.0));
        assert_eq!(p.collateral_ratio, None);
        let g = &meta()["gma"]["golden"];
        let sol = p.sol.value().unwrap();
        let usdc = p.usdc.value().unwrap();
        assert!(
            (sol.borrow_apr_pct - g["sol_custody"]["borrow_apr_pct"].as_f64().unwrap()).abs()
                < 1e-9
        );
        assert!(
            (usdc.borrow_apr_pct - g["usdc_custody"]["borrow_apr_pct"].as_f64().unwrap()).abs()
                < 1e-9
        );
        assert_eq!(p.max_request_execution_sec, Field::ok(45));
        assert_eq!(p.oracle_usd, Some(SOL_AT_CAPTURE));
        assert_eq!(
            p.watch,
            [
                "FqymRcB92t63jpwh7om4RLbxMNUGoHnZPQMkkAA8ksVY",
                "6HFhuYzQGcqdj4NGwC6vfVETRvMA3pXaVeZnHgWSKsJK",
                ids::JUP_CUSTODY_SOL,
                ids::JUP_CUSTODY_USDC,
                ids::JLP_POOL
            ]
        );
        // One GMA over exactly the planned keys.
        let gma = t.gma_params();
        assert_eq!(gma.len(), 1);
        assert_eq!(gma[0][0].as_array().unwrap().len(), 5);
        // Within 5 s: cached, no RPC, no oracle call.
        let c = jup_perps_obs(
            &rpc,
            Some(&store),
            &wallet,
            &json!({}),
            now() + 4_000,
            || async {
                calls.fetch_add(1, Ordering::SeqCst);
                Some(1.0)
            },
        )
        .await;
        assert_eq!(c.source, ObsSource::Cache);
        assert_eq!(t.requests().len(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn both_sides_open_matches_the_golden() {
        let t = perps_transport();
        let rpc = fake_rpc(&t);
        let o = jup_perps_obs(&rpc, None, &k(HEDGED), &json!({}), now(), || async {
            Some(SOL_AT_CAPTURE)
        })
        .await;
        assert_eq!(o.status, ObsStatus::Ok, "{:?}", o.errors);
        let p: PerpsState = o.typed().unwrap();
        assert!(p.both_sides_open);
        let g = &meta()["gma"]["golden"]["positions"];
        let long = p.long.value().unwrap();
        let short = p.short.value().unwrap();
        assert_eq!(
            long.position_pda,
            "2DNqvKcgnx5Huk6VkjeoZbjhna6hZd7GRuG8RCH2dPEV"
        );
        assert_eq!(
            short.position_pda,
            "HCZsYUEGtiGVvFJJYcuqhrq1L2MQ7GwWXmQRQSxFNWWX"
        );
        let liq = |pda: &str| g[pda]["liq_usd"].as_f64().unwrap();
        let near = |a: Option<f64>, b: f64| (a.unwrap() - b).abs() < 1e-6;
        assert!(near(long.liquidation_price_usd, liq(&long.position_pda)));
        assert!(near(short.liquidation_price_usd, liq(&short.position_pda)));
        assert_eq!(o.features["both_sides_open"], json!(true));
        assert!(p.collateral_ratio.is_some());
    }

    #[tokio::test]
    async fn no_oracle_keeps_sides_without_price_fields() {
        let t = perps_transport();
        let rpc = fake_rpc(&t);
        let o = jup_perps_obs(&rpc, None, &k(HEDGED), &json!({}), now(), || async { None }).await;
        let p: PerpsState = o.typed().unwrap();
        assert_eq!(p.oracle_usd, None);
        // Entry-price delta needs no oracle; PnL does.
        let long = p.long.value().unwrap();
        assert!(long.base_sol > 0.0);
        assert_eq!(long.unrealized_pnl_usd, None);
    }

    #[tokio::test]
    async fn rpc_failure_is_an_error_observation() {
        let t = Arc::new(FakeTransport::at_slot(1));
        t.push(Ok(err_envelope(-32429, "max usage reached")));
        let rpc = fake_rpc(&t);
        let store = MemStore::default();
        let wallet = k(BOT_WALLET);
        let o = jup_perps_obs(&rpc, Some(&store), &wallet, &json!({}), 0, || async {
            None
        })
        .await;
        assert_eq!(o.status, ObsStatus::Error);
        assert_eq!(
            (o.errors[0].field.as_str(), o.errors[0].class),
            ("accounts", ErrorClass::QuotaExhausted)
        );
        assert_line1(&o, &[BOT_WALLET]);
        assert!(store.get(&o.key).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn execute_gates_scope_and_args() {
        let dir = tempfile::tempdir().unwrap();
        let tool = JupPerpsTool::new(SolanaShared::default());
        assert_eq!(tool.definition().name, names::JUP_PERPS);
        let h = TestHarness::with_scope(dir.path(), ToolScope::default());
        assert!(tool
            .execute(&json!({"wallet": BOT_WALLET}), &h.ctx())
            .await
            .is_err());
        let h = TestHarness::new(dir.path());
        let e = tool
            .execute(&json!({"wallet": "0OIl"}), &h.ctx())
            .await
            .unwrap_err();
        assert!(
            e.to_string()
                .contains("'wallet' is not a valid Solana address"),
            "{e}"
        );
    }

    // ── live (public mainnet; `cargo test --bin tengu live_ -- --ignored --test-threads 1`) ──

    #[tokio::test]
    #[ignore]
    async fn live_jup_perps_bot_wallet_flat() {
        use crate::adapters::outbound::egress;
        use crate::adapters::outbound::observations::SqliteObservationStore;
        use crate::adapters::outbound::solana::rpc::REQUEST_TIMEOUT;

        let dir = tempfile::tempdir().unwrap();
        let scope = ToolScope {
            fs_roots: vec![dir.path().to_path_buf()],
            net_hosts: vec![
                "api.mainnet-beta.solana.com".into(),
                "lite-api.jup.ag".into(),
            ],
            ..Default::default()
        };
        let mut h = TestHarness::with_scope(dir.path(), scope);
        h.http = egress::policy().tool_client(REQUEST_TIMEOUT).unwrap();
        let tool = JupPerpsTool::new(SolanaShared {
            store: Some(Arc::new(SqliteObservationStore::open(dir.path()).unwrap())),
        });
        let args = json!({"wallet": BOT_WALLET});
        let o = tool
            .execute(&args, &h.ctx())
            .await
            .unwrap()
            .observation
            .unwrap();
        println!(
            "{}",
            o.render_text(o.observed_at_ms).lines().next().unwrap()
        );
        assert_eq!(o.status, ObsStatus::Ok, "{:?}", o.errors);
        assert_line1(&o, &[BOT_WALLET]);
        let p: PerpsState = o.typed().unwrap();
        assert_eq!(p.long, Field::Absent, "bot is flat");
        assert_eq!(p.short, Field::Absent, "bot is flat");
        assert!(p.sol.value().is_some() && p.usdc.value().is_some());
        assert!(p.max_request_execution_sec.value().is_some_and(|s| *s > 0));
        assert!(
            p.oracle_usd.is_some_and(|x| x > 1.0),
            "inline Jupiter price"
        );
        let c = tool
            .execute(&args, &h.ctx())
            .await
            .unwrap()
            .observation
            .unwrap();
        assert_eq!(c.source, ObsSource::Cache);
    }
}
