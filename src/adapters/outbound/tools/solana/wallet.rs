//! `solana_wallet` + `solana_tx` — wallet inventory and transaction status.
//! Decoders, balance building and status parsing live in
//! `domain::lp::wallet`; this file reads the chain.
//!
//! | Tool | Reads | Key / TTL |
//! |---|---|---|
//! | `solana_wallet` | `getBalance`, `getTokenAccountsByOwner` × {Tokenkeg, Token-2022} and one `fetch_accounts` over each requested mint + its two candidate ATAs, all concurrent; each is its own `Field` (one failing never hides another) | `solana_wallet/1:<wallet>`, 5 s |
//! | `solana_tx` | `getSignatureStatuses([sig], searchTransactionHistory)`, then `getTransaction` only when found | `solana_tx/1:<signature>`, 1 day once finalized with the tx body read, else 2 s |
//!
//! | Rule | Detail |
//! |---|---|
//! | Balance ATA | derived under the mint account's owner program (Tokenkeg or Token-2022, read in the same GMA); absent ATA = 0, unread / foreign ATA = error |
//! | `mints` | default wSOL + USDC; at most 32 (3 keys each, one GMA) |
//! | Cache reuse (`solana_wallet`) | a fresh row is reused only when it covers every requested mint |
//! | Errors | bad args → `Err`; RPC failures → `Field::Error` / an `Error` observation (never cached) |

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::Value;
use tracing::warn;

use super::price::{parse_pubkey_value, require_pubkey};
use super::{defs, SolanaShared};
use crate::adapters::outbound::solana::accounts::fetch_accounts;
use crate::adapters::outbound::solana::rpc::{read_error, SolanaRpc};
use crate::application::observe::observe;
use crate::domain::lp::wallet::{
    build_wallet_balances, build_wallet_inventory, is_token_program, parse_token_accounts_by_owner,
    parse_tx_status, TokenAccountRow, TxStatus, WalletInventory, DEFAULT_MINTS, TX_FINAL_TTL_MS,
    WALLET_TTL_MS,
};
use crate::domain::message::ToolDef;
use crate::domain::observation::{now_ms, CachePolicy, Field, Observation, Observed};
use crate::domain::solana::{ata, ids, AccountSet, Pubkey, Signature};
use crate::domain::tools as names;
use crate::ports::observation::ObservationStore;
use crate::ports::tool::{Tool, ToolCtx, ToolOutput};

/// Max `mints` per `solana_wallet` call (3 keys each ⇒ one 100-key GMA).
pub(crate) const MAX_MINTS: usize = 32;

pub(crate) fn tools(shared: &SolanaShared) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(WalletTool {
            def: defs::def(names::SOLANA_WALLET),
            store: shared.store.clone(),
        }),
        Arc::new(TxTool {
            def: defs::def(names::SOLANA_TX),
            store: shared.store.clone(),
        }),
    ]
}

// ---------------------------------------------------------------------------
// solana_wallet
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
struct WalletRequest {
    wallet: Pubkey,
    /// Deduplicated, request order kept.
    mints: Vec<Pubkey>,
}

impl WalletRequest {
    fn parse(args: &Value) -> Result<Self> {
        let tool = names::SOLANA_WALLET;
        let wallet = require_pubkey(args, tool, "wallet")?;
        let mints = match args.get("mints") {
            None | Some(Value::Null) => DEFAULT_MINTS.iter().map(|m| ids::key(m)).collect(),
            Some(Value::Array(items)) => {
                let mut out: Vec<Pubkey> = Vec::with_capacity(items.len());
                for (i, v) in items.iter().enumerate() {
                    let m = parse_pubkey_value(v, tool, &format!("mints[{i}]"))?;
                    if !out.contains(&m) {
                        out.push(m);
                    }
                }
                out
            }
            Some(v) => {
                return Err(anyhow!(
                    "{tool}: 'mints' must be an array of base58 mints, got {v}"
                ))
            }
        };
        if mints.len() > MAX_MINTS {
            return Err(anyhow!(
                "{tool}: at most {MAX_MINTS} 'mints' per call, got {}",
                mints.len()
            ));
        }
        Ok(Self { wallet, mints })
    }
}

struct WalletTool {
    def: ToolDef,
    store: Option<Arc<dyn ObservationStore>>,
}

#[async_trait]
impl Tool for WalletTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        ctx.scope.check_fs_write(ctx.workspace)?;
        let req = WalletRequest::parse(args)?;
        let now = now_ms();
        let rpc = SolanaRpc::from_ctx(ctx)?;
        let obs = wallet_observation(&rpc, self.store.as_deref(), &req, args, now).await?;
        Ok(ToolOutput::observed(obs, now))
    }
}

async fn wallet_observation(
    rpc: &SolanaRpc,
    store: Option<&dyn ObservationStore>,
    req: &WalletRequest,
    args: &Value,
    now: i64,
) -> Result<Observation> {
    let mut policy = CachePolicy::new(
        WalletInventory::SCHEMA,
        &req.wallet.to_string(),
        WALLET_TTL_MS,
        args,
    );
    let account_max_age = policy.max_age_ms;
    if row_lacks_mints(store, &policy.key, &req.mints).await {
        // Same key, other mints: read live, then replace the row.
        policy.max_age_ms = 0;
    }
    observe(store, names::SOLANA_WALLET, &policy, now, || async {
        let inv = read_wallet(rpc, store, req, account_max_age, now).await;
        Ok((inv, WALLET_TTL_MS))
    })
    .await
}

/// A stored row exists but lacks a balance for one of `mints` (or does not
/// decode). No row / no store / store failure ⇒ `false`.
async fn row_lacks_mints(
    store: Option<&dyn ObservationStore>,
    key: &str,
    mints: &[Pubkey],
) -> bool {
    let Some(store) = store else {
        return false;
    };
    match store.get(key).await {
        Ok(Some(row)) => match row.typed::<WalletInventory>() {
            Ok(inv) => mints.iter().any(|m| !inv.balances.contains_key(m)),
            Err(_) => true,
        },
        Ok(None) => false,
        Err(e) => {
            let error = format!("{e:#}");
            warn!(key, %error, "observation store read failed");
            false
        }
    }
}

/// Every mint plus its ATA under both token programs — the owner program
/// is only known once the mint is read, and both fit one GMA.
fn balance_keys(wallet: &Pubkey, mints: &[Pubkey]) -> Vec<Pubkey> {
    let programs = [ids::key(ids::TOKEN), ids::key(ids::TOKEN_2022)];
    mints
        .iter()
        .flat_map(|m| {
            [
                *m,
                ata(wallet, m, &programs[0]),
                ata(wallet, m, &programs[1]),
            ]
        })
        .collect()
}

/// `(mint, ata, token_program)` per mint, the program being the mint
/// account's owner in `set` (Tokenkeg when the mint is absent / not an SPL
/// mint — `build_wallet_balances` then reports the mint error).
fn ata_triples(
    set: &AccountSet,
    wallet: &Pubkey,
    mints: &[Pubkey],
) -> Vec<(Pubkey, Pubkey, Pubkey)> {
    let tokenkeg = ids::key(ids::TOKEN);
    mints
        .iter()
        .map(|m| {
            let program = set
                .get(m)
                .and_then(|r| r.owner().copied())
                .filter(is_token_program)
                .unwrap_or(tokenkeg);
            (*m, ata(wallet, m, &program), program)
        })
        .collect()
}

fn token_rows(
    read: Result<(u64, Value)>,
    program: &Pubkey,
    field: &str,
    slot: &mut u64,
) -> Field<Vec<TokenAccountRow>> {
    match read {
        Ok((s, v)) => {
            *slot = (*slot).max(s);
            parse_token_accounts_by_owner(&v, program)
                .map(Field::ok)
                .unwrap_or_else(Field::err)
        }
        Err(e) => Field::err(read_error(field, &e)),
    }
}

async fn read_wallet(
    rpc: &SolanaRpc,
    store: Option<&dyn ObservationStore>,
    req: &WalletRequest,
    max_age_ms: u64,
    now: i64,
) -> WalletInventory {
    let (tokenkeg, token_2022) = (ids::key(ids::TOKEN), ids::key(ids::TOKEN_2022));
    let keys = balance_keys(&req.wallet, &req.mints);
    let (lamports, keg, t22, set) = tokio::join!(
        rpc.get_balance(&req.wallet),
        rpc.get_token_accounts_by_owner(&req.wallet, &tokenkeg),
        rpc.get_token_accounts_by_owner(&req.wallet, &token_2022),
        fetch_accounts(rpc, store, &keys, max_age_ms, None, now),
    );
    let mut slot = 0u64;
    let lamports = match lamports {
        Ok((s, l)) => {
            slot = slot.max(s);
            Field::ok(l)
        }
        Err(e) => Field::err(read_error("lamports", &e)),
    };
    let token_accounts = token_rows(keg, &tokenkeg, "token_accounts", &mut slot);
    let token_2022_accounts = token_rows(t22, &token_2022, "token_2022_accounts", &mut slot);
    let balances = match set {
        Ok(set) => {
            if !set.accounts.is_empty() {
                slot = slot.max(set.slot_max);
            }
            let triples = ata_triples(&set, &req.wallet, &req.mints);
            build_wallet_balances(&set, &req.wallet, &triples, &BTreeMap::new())
        }
        Err(e) => req
            .mints
            .iter()
            .map(|m| (*m, Field::err(read_error(&format!("balances.{m}"), &e))))
            .collect(),
    };
    build_wallet_inventory(
        req.wallet,
        slot,
        lamports,
        token_accounts,
        token_2022_accounts,
        balances,
    )
}

// ---------------------------------------------------------------------------
// solana_tx
// ---------------------------------------------------------------------------

fn parse_signature(args: &Value) -> Result<Signature> {
    let tool = names::SOLANA_TX;
    let s = args
        .get("signature")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("{tool}: 'signature' is required (base58, 64 bytes)"))?;
    s.trim()
        .parse::<Signature>()
        .map_err(|e| anyhow!("{tool}: 'signature' is not a valid signature ({e}): {s}"))
}

struct TxTool {
    def: ToolDef,
    store: Option<Arc<dyn ObservationStore>>,
}

#[async_trait]
impl Tool for TxTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        ctx.scope.check_fs_write(ctx.workspace)?;
        let sig = parse_signature(args)?;
        let now = now_ms();
        let rpc = SolanaRpc::from_ctx(ctx)?;
        let obs = tx_observation(&rpc, self.store.as_deref(), sig, args, now).await?;
        Ok(ToolOutput::observed(obs, now))
    }
}

async fn tx_observation(
    rpc: &SolanaRpc,
    store: Option<&dyn ObservationStore>,
    sig: Signature,
    args: &Value,
    now: i64,
) -> Result<Observation> {
    // The policy TTL is the longest a row may live (finalized); each row's
    // own ttl_ms (2 s while pending) bounds its reuse.
    let policy = CachePolicy::new(TxStatus::SCHEMA, &sig.to_string(), TX_FINAL_TTL_MS, args);
    observe(store, names::SOLANA_TX, &policy, now, || async {
        let st = read_tx(rpc, sig).await;
        let ttl = st.ttl_ms();
        Ok((st, ttl))
    })
    .await
}

async fn read_tx(rpc: &SolanaRpc, sig: Signature) -> TxStatus {
    let (slot, statuses) = match rpc.get_signature_statuses(&[sig], true).await {
        Ok(r) => r,
        Err(e) => return TxStatus::failed(sig, None, read_error("status", &e)),
    };
    let found = statuses
        .as_array()
        .and_then(|a| a.first())
        .is_some_and(|e| !e.is_null());
    if !found {
        return parse_tx_status(sig, Some(slot), &statuses, None);
    }
    let tx = match rpc.get_transaction(&sig).await {
        Ok(v) => Ok(v.unwrap_or(Value::Null)),
        Err(e) => Err(read_error("tx", &e)),
    };
    parse_tx_status(
        sig,
        Some(slot),
        &statuses,
        Some(tx.as_ref().map_err(Clone::clone)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::lp::wallet::TX_TTL_MS;
    use crate::domain::observation::ObsStatus;
    use std::sync::Mutex;

    use serde_json::json;

    use crate::adapters::outbound::solana::rpc::tests::FakeTransport;
    use crate::adapters::outbound::solana::rpc::{parse_gma, RpcError, RpcTransport};
    use crate::adapters::outbound::tools::solana::price::tests::{Live, USDC, WSOL};
    use crate::application::observe::tests::MemStore;
    use crate::domain::lp::wallet::TokenAmount;
    use crate::domain::observation::{assert_features_ok, ErrorClass, ObsSource, MAX_LINE1_CHARS};

    const WALLET: &str = "F3YvPiLdniRPGpeKrbeGWR2zg2wPpzVuvqBA5BBJBQ5S";
    const MINT_98S: &str = "98sMhvDwXj1RQi5c5Mndm3vPe9cBqPrbLaufMXFNMh5g";
    const PYUSD: &str = "2b1kV6DkPAnxd5ixfnxCpjxmKwqjjaYmCZfHsFu24GXo";
    const SIG_OK: &str =
        "L8TEY2sSvscX2R2EBChD1p1o3HApdBUHqVfTJKLxJ4zK4M6pebL3diuYKcKjPF5deW7GF6DVnMdPU2tBwgz1Mfi";
    const SIG_FAILED: &str =
        "24vJBCdpUCW5nAb9JQZL4E5ptwSCyQ7QYvrrTTYp13esMq5q26LdZ9mzC1qZX6MDF31hsAcr3HWLHa3kaa9P1yPE";
    const SIG_UNKNOWN: &str =
        "99eUso3aSbE9tqGSTXzo3TLfKb9RkMTURrHKQ1K7Zh3BbeqPevr5E1iCbpTjqHuTFLtfxTTD5ekfVuZFzQyEQf8";

    macro_rules! fixture {
        ($name:literal) => {
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/solana/wallet/",
                $name
            ))
        };
    }

    fn value(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    type Reply = std::result::Result<Value, RpcError>;

    /// Replies by JSON-RPC method (+ optional params substring; replies
    /// persist, so the single retry sees the same answer); anything else
    /// (`getMultipleAccounts`) goes to a [`FakeTransport`] serving the wallet
    /// GMA fixture.
    struct Routed {
        gma: Arc<FakeTransport>,
        /// (method, params substring, reply)
        routes: Mutex<Vec<(String, Option<String>, Reply)>>,
        calls: Mutex<Vec<Value>>,
    }

    impl Routed {
        fn fixture() -> Arc<Self> {
            let gma = Arc::new(FakeTransport::at_slot(450_101_767));
            let meta = value(fixture!("meta.json"));
            let keys: Vec<Pubkey> = meta["files"]["gma_base64.json"]["keys"]
                .as_array()
                .unwrap()
                .iter()
                .map(|k| k.as_str().unwrap().parse().unwrap())
                .collect();
            let env = value(fixture!("gma_base64.json"));
            for r in parse_gma(&env["result"], &keys).unwrap().1 {
                gma.put_account(r);
            }
            let t = Arc::new(Self {
                gma,
                routes: Mutex::new(Vec::new()),
                calls: Mutex::new(Vec::new()),
            });
            t.route("getBalance", None, Ok(value(fixture!("get_balance.json"))));
            t.route(
                "getTokenAccountsByOwner",
                Some(ids::TOKEN),
                Ok(value(fixture!("token_accounts_tokenkeg.json"))),
            );
            t.route(
                "getTokenAccountsByOwner",
                Some(ids::TOKEN_2022),
                Ok(value(fixture!("token_accounts_token2022.json"))),
            );
            t
        }
        /// Prepend a route (it wins over earlier ones).
        fn route(&self, method: &str, param: Option<&str>, reply: Reply) {
            self.routes
                .lock()
                .unwrap()
                .insert(0, (method.into(), param.map(Into::into), reply));
        }
        fn calls(&self, method: &str) -> Vec<Value> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|b| b["method"] == method)
                .map(|b| b["params"].clone())
                .collect()
        }
        fn rpc(self: &Arc<Self>) -> SolanaRpc {
            SolanaRpc::with_transport(self.clone(), "fake.rpc").with_backoff_ms(0)
        }
    }

    #[async_trait]
    impl RpcTransport for Routed {
        async fn call(&self, body: Value) -> std::result::Result<Value, RpcError> {
            self.calls.lock().unwrap().push(body.clone());
            let method = body["method"].as_str().unwrap_or("").to_string();
            let params = body["params"].to_string();
            let hit = self
                .routes
                .lock()
                .unwrap()
                .iter()
                .find_map(|(m, p, reply)| {
                    (*m == method && p.as_ref().map_or(true, |p| params.contains(p.as_str())))
                        .then(|| reply.clone())
                });
            match hit {
                Some(reply) => reply,
                None => self.gma.call(body).await,
            }
        }
    }

    async fn wallet(
        t: &Arc<Routed>,
        store: Option<&dyn ObservationStore>,
        args: Value,
        now: i64,
    ) -> Observation {
        let req = WalletRequest::parse(&args).unwrap();
        wallet_observation(&t.rpc(), store, &req, &args, now)
            .await
            .unwrap()
    }

    fn balance(inv: &WalletInventory, mint: &str) -> Field<TokenAmount> {
        inv.balance(&mint.parse().unwrap()).unwrap().clone()
    }

    #[test]
    fn wallet_args() {
        let r = WalletRequest::parse(&json!({"wallet": WALLET})).unwrap();
        assert_eq!(r.wallet.to_string(), WALLET);
        let mints: Vec<String> = r.mints.iter().map(Pubkey::to_string).collect();
        assert_eq!(mints, vec![WSOL, USDC]);
        let r =
            WalletRequest::parse(&json!({"wallet": WALLET, "mints": [USDC, USDC, PYUSD]})).unwrap();
        assert_eq!(r.mints.len(), 2, "deduplicated");
        let too_many: Vec<String> = (0..=MAX_MINTS as u8)
            .map(|i| Pubkey([i; 32]).to_string())
            .collect();
        for (args, needle) in [
            (json!({}), "'wallet'"),
            (json!({"wallet": "abc"}), "'wallet'"),
            (json!({"wallet": WALLET, "mints": USDC}), "'mints'"),
            (
                json!({"wallet": WALLET, "mints": [USDC, "nope"]}),
                "'mints[1]'",
            ),
            (json!({"wallet": WALLET, "mints": too_many}), "at most 32"),
        ] {
            let e = WalletRequest::parse(&args).unwrap_err().to_string();
            assert!(e.starts_with("solana_wallet:") && e.contains(needle), "{e}");
        }
    }

    #[tokio::test]
    async fn wallet_from_fixtures_is_ok_and_cached() {
        let t = Routed::fixture();
        let store = MemStore::default();
        let o = wallet(&t, Some(&store), json!({"wallet": WALLET}), 1_000).await;
        assert_eq!(o.key, format!("solana_wallet/1:{WALLET}"));
        assert_eq!(
            (o.status, o.source),
            (ObsStatus::Ok, ObsSource::Live),
            "{:?}",
            o.errors
        );
        assert_eq!(o.slot, Some(450_101_767), "highest read slot");
        assert_features_ok(&o.features);
        let inv: WalletInventory = o.typed().unwrap();
        assert_eq!(inv.lamports, Field::ok(2_748_145_289));
        assert_eq!(inv.token_accounts.value().unwrap().len(), 2);
        assert_eq!(inv.token_2022_accounts.value().unwrap().len(), 0);
        let usdc = balance(&inv, USDC);
        assert_eq!(usdc.value().unwrap().raw, "107808931");
        assert_eq!(usdc.value().unwrap().decimals, 6);
        // The wSOL ATA E4PCnfEconGJW6vf7GDycEnkFe1VWC7teiNWJzQv3NTA does not exist: 0, not an error.
        assert_eq!(balance(&inv, WSOL).value().unwrap().raw, "0");
        let line1 = o.render_text(1_000).lines().next().unwrap().to_string();
        assert!(
            line1.contains(WALLET) && line1.chars().count() <= MAX_LINE1_CHARS,
            "{line1}"
        );
        // One GMA: each mint + its Tokenkeg and Token-2022 ATA.
        let gma = t.calls("getMultipleAccounts");
        assert_eq!(gma.len(), 1);
        let keys = gma[0][0].as_array().unwrap();
        assert_eq!(keys.len(), 6);
        assert!(keys.contains(&json!("E4PCnfEconGJW6vf7GDycEnkFe1VWC7teiNWJzQv3NTA")));
        assert!(keys.contains(&json!("D9ScKYy15cw1tpkkuwEnDKv62nCyuETwrvRSdP4usGg1")));

        let b = wallet(&t, Some(&store), json!({"wallet": WALLET}), 4_000).await;
        assert_eq!(b.source, ObsSource::Cache);
        assert_eq!(t.calls("getBalance").len(), 1);
        // A mint the cached row lacks: live despite the fresh row.
        let c = wallet(
            &t,
            Some(&store),
            json!({"wallet": WALLET, "mints": [MINT_98S]}),
            4_500,
        )
        .await;
        assert_eq!(c.source, ObsSource::Live);
        let inv: WalletInventory = c.typed().unwrap();
        assert_eq!(balance(&inv, MINT_98S).value().unwrap().raw, "8488252");
    }

    #[tokio::test]
    async fn token_2022_mint_uses_its_own_ata() {
        let t = Routed::fixture();
        let o = wallet(&t, None, json!({"wallet": WALLET, "mints": [PYUSD]}), 0).await;
        let inv: WalletInventory = o.typed().unwrap();
        // PYUSD is a Token-2022 mint; its ATA BUACmTyTknjx6zdRwazawArchbQehNUYkZjjweMvyNLb is absent.
        let b = balance(&inv, PYUSD);
        assert_eq!(
            (b.value().unwrap().raw.as_str(), b.value().unwrap().decimals),
            ("0", 6)
        );
        assert!(t.calls("getMultipleAccounts")[0][0]
            .as_array()
            .unwrap()
            .contains(&json!("BUACmTyTknjx6zdRwazawArchbQehNUYkZjjweMvyNLb")));
    }

    #[tokio::test]
    async fn one_failed_read_does_not_hide_the_others() {
        let t = Routed::fixture();
        t.route(
            "getTokenAccountsByOwner",
            Some(ids::TOKEN_2022),
            Err(RpcError::new(
                ErrorClass::QuotaExhausted,
                "max usage reached",
            )),
        );
        t.route(
            "getBalance",
            None,
            Err(RpcError::new(ErrorClass::Timeout, "timed out")),
        );
        let store = MemStore::default();
        let o = wallet(&t, Some(&store), json!({"wallet": WALLET}), 0).await;
        assert_eq!(o.status, ObsStatus::Partial);
        let inv: WalletInventory = o.typed().unwrap();
        assert_eq!(inv.lamports.error().unwrap().class, ErrorClass::Timeout);
        assert!(inv.sol().is_none(), "a failed read never becomes 0");
        let e = inv.token_2022_accounts.error().unwrap();
        assert_eq!(
            (e.field.as_str(), e.class),
            ("token_2022_accounts", ErrorClass::QuotaExhausted)
        );
        assert_eq!(inv.token_accounts.value().unwrap().len(), 2);
        assert!(balance(&inv, USDC).value().is_some());
        assert!(
            store.get(&o.key).await.unwrap().is_some(),
            "partial rows are cached"
        );
    }

    #[tokio::test]
    async fn every_read_failing_is_an_uncached_error() {
        let t = Routed::fixture();
        let quota = || {
            Err(RpcError::new(
                ErrorClass::QuotaExhausted,
                "max usage reached",
            ))
        };
        for m in [
            "getBalance",
            "getTokenAccountsByOwner",
            "getMultipleAccounts",
        ] {
            t.route(m, None, quota());
        }
        let store = MemStore::default();
        let o = wallet(&t, Some(&store), json!({"wallet": WALLET}), 0).await;
        assert_eq!(o.status, ObsStatus::Error);
        let inv: WalletInventory = o.typed().unwrap();
        let e = balance(&inv, USDC).error().unwrap().clone();
        assert_eq!(e.field, format!("balances.{USDC}"));
        assert!(store.get(&o.key).await.unwrap().is_none());
    }

    // ── solana_tx ──

    async fn tx(
        t: &Arc<Routed>,
        store: Option<&dyn ObservationStore>,
        sig: &str,
        now: i64,
    ) -> Observation {
        let args = json!({"signature": sig});
        tx_observation(&t.rpc(), store, parse_signature(&args).unwrap(), &args, now)
            .await
            .unwrap()
    }

    /// `sig_statuses_history.json` with only entry `i` (one signature).
    fn statuses_entry(i: usize) -> Value {
        let mut v = value(fixture!("sig_statuses_history.json"));
        let entry = v["result"]["value"][i].clone();
        v["result"]["value"] = json!([entry]);
        v
    }

    #[test]
    fn tx_args() {
        assert_eq!(
            parse_signature(&json!({"signature": SIG_OK}))
                .unwrap()
                .to_string(),
            SIG_OK
        );
        for args in [
            json!({}),
            json!({"signature": WALLET}),
            json!({"signature": 5}),
        ] {
            let e = parse_signature(&args).unwrap_err().to_string();
            assert!(
                e.starts_with("solana_tx:") && e.contains("'signature'"),
                "{e}"
            );
        }
    }

    #[tokio::test]
    async fn finalized_tx_is_cached_for_a_day() {
        let t = Routed::fixture();
        t.route("getSignatureStatuses", None, Ok(statuses_entry(0)));
        t.route("getTransaction", None, Ok(value(fixture!("tx_ok.json"))));
        let store = MemStore::default();
        let o = tx(&t, Some(&store), SIG_OK, 1_000).await;
        assert_eq!(o.key, format!("solana_tx/1:{SIG_OK}"));
        assert_eq!((o.status, o.ttl_ms), (ObsStatus::Ok, TX_FINAL_TTL_MS));
        assert_features_ok(&o.features);
        let st: TxStatus = o.typed().unwrap();
        assert!(st.is_final() && st.succeeded() == Some(true));
        assert_eq!(
            (st.fee_lamports.clone(), st.compute_units.clone()),
            (Field::ok(7_600), Field::ok(190_822))
        );
        let line1 = o.render_text(1_000).lines().next().unwrap().to_string();
        assert!(
            line1.contains(SIG_OK) && line1.chars().count() <= MAX_LINE1_CHARS,
            "{line1}"
        );
        let params = &t.calls("getSignatureStatuses")[0];
        assert_eq!(params[1]["searchTransactionHistory"], json!(true));
        // An hour later: still the cached row, no RPC.
        let b = tx(&t, Some(&store), SIG_OK, 3_601_000).await;
        assert_eq!(b.source, ObsSource::Cache);
        assert_eq!(t.calls("getSignatureStatuses").len(), 1);
    }

    #[tokio::test]
    async fn failed_tx_reports_the_instruction_error() {
        let t = Routed::fixture();
        t.route("getSignatureStatuses", None, Ok(statuses_entry(1)));
        t.route(
            "getTransaction",
            None,
            Ok(value(fixture!("tx_failed.json"))),
        );
        let o = tx(&t, None, SIG_FAILED, 0).await;
        assert_eq!(o.status, ObsStatus::Ok);
        let st: TxStatus = o.typed().unwrap();
        assert_eq!(st.succeeded(), Some(false));
        assert_eq!(o.features["err_custom"], json!(3012));
        assert_eq!(st.fee_lamports, Field::ok(12_178));
    }

    #[tokio::test]
    async fn unknown_tx_is_absent_without_get_transaction() {
        let t = Routed::fixture();
        t.route(
            "getSignatureStatuses",
            None,
            Ok(value(fixture!("sig_statuses_unknown.json"))),
        );
        let o = tx(&t, None, SIG_UNKNOWN, 0).await;
        assert_eq!((o.status, o.ttl_ms), (ObsStatus::Absent, TX_TTL_MS));
        assert!(t.calls("getTransaction").is_empty());
        assert!(o
            .render_text(0)
            .starts_with(&format!("tx {SIG_UNKNOWN} not found")));
    }

    #[tokio::test]
    async fn tx_body_failure_is_partial_and_short_lived() {
        let t = Routed::fixture();
        t.route("getSignatureStatuses", None, Ok(statuses_entry(0)));
        t.route(
            "getTransaction",
            None,
            Err(RpcError::new(
                ErrorClass::RateLimited,
                "Too many requests for a specific RPC call",
            )),
        );
        let store = MemStore::default();
        let o = tx(&t, Some(&store), SIG_OK, 0).await;
        assert_eq!((o.status, o.ttl_ms), (ObsStatus::Partial, TX_TTL_MS));
        assert_eq!(o.errors[0].class, ErrorClass::RateLimited);
        // Finalized but incomplete: re-read after 2 s, not pinned for a day.
        let b = tx(&t, Some(&store), SIG_OK, 3_000).await;
        assert_eq!(b.source, ObsSource::Live);
    }

    #[tokio::test]
    async fn status_rpc_failure_is_an_uncached_error() {
        let t = Routed::fixture();
        t.route(
            "getSignatureStatuses",
            None,
            Err(RpcError::new(ErrorClass::AuthRequired, "HTTP 401")),
        );
        let store = MemStore::default();
        let o = tx(&t, Some(&store), SIG_OK, 0).await;
        assert_eq!(o.status, ObsStatus::Error);
        assert_eq!(
            (o.errors[0].field.as_str(), o.errors[0].class),
            ("status", ErrorClass::AuthRequired)
        );
        assert!(store.get(&o.key).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn tools_are_named_and_gate_on_the_workspace() {
        let shared = SolanaShared::default();
        let tools = tools(&shared);
        let names: Vec<&str> = tools.iter().map(|t| t.definition().name.as_str()).collect();
        assert_eq!(names, vec![names::SOLANA_WALLET, names::SOLANA_TX]);
        let tmp = tempfile::TempDir::new().unwrap();
        let denied =
            crate::adapters::outbound::tools::workspace::test_support::TestHarness::with_scope(
                tmp.path(),
                Default::default(),
            );
        for tool in &tools {
            let e = tool.execute(&json!({}), &denied.ctx()).await.unwrap_err();
            assert!(
                !e.to_string().contains("is required"),
                "scope checked first: {e}"
            );
        }
    }

    // ── live ──

    #[tokio::test]
    #[ignore]
    async fn live_solana_wallet() {
        let live = Live::new();
        let o = live
            .call_twice(names::SOLANA_WALLET, json!({"wallet": WALLET}))
            .await;
        assert_eq!(o.key, format!("solana_wallet/1:{WALLET}"));
        let line1 = o
            .render_text(o.observed_at_ms)
            .lines()
            .next()
            .unwrap()
            .to_string();
        assert!(line1.contains(WALLET), "{line1}");
        let inv: WalletInventory = o.typed().unwrap();
        assert!(inv.lamports.value().is_some(), "{inv:?}");
        for m in [WSOL, USDC] {
            assert!(balance(&inv, m).value().is_some(), "{m}: {inv:?}");
        }
        assert!(inv.token_accounts.value().is_some() && inv.token_2022_accounts.value().is_some());
    }

    #[tokio::test]
    #[ignore]
    async fn live_solana_tx() {
        let rpc = crate::adapters::outbound::solana::rpc::tests::live_rpc();
        let sigs = rpc
            .call(
                "getSignaturesForAddress",
                json!([WALLET, {"limit": 1, "commitment": "confirmed"}]),
            )
            .await
            .unwrap();
        let sig = sigs[0]["signature"].as_str().unwrap().to_string();
        let live = Live::new();
        let o = live
            .call_twice(names::SOLANA_TX, json!({"signature": sig}))
            .await;
        assert_eq!(o.key, format!("solana_tx/1:{sig}"));
        let line1 = o
            .render_text(o.observed_at_ms)
            .lines()
            .next()
            .unwrap()
            .to_string();
        assert!(line1.contains(&sig), "{line1}");
        let st: TxStatus = o.typed().unwrap();
        assert!(st.found, "{st:?}");
    }
}
