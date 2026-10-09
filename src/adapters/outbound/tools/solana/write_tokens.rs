//! `solana_close_token_accounts` — close the wallet's empty SPL token
//! accounts and reclaim their rent (the bot's wallet janitor,
//! `walletJanitor.ts`). Runner: `write_common.rs`.
//!
//! | Rule | Detail |
//! |---|---|
//! | Closable | amount 0, not frozen, authority = the wallet, mint not protected |
//! | Protected mints | wSOL, USDC, and every `keep_mints` entry (pool mints of an open LP belong here) |
//! | Instruction | SPL `CloseAccount` under the account's own program (Tokenkeg or Token-2022); rent → the wallet |
//! | Batching | 8 accounts per transaction; batches are independent (one failing never stops the rest) |
//! | Reads | `getTokenAccountsByOwner` × both programs, live |

use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

use super::price::{parse_pubkey_value, require_pubkey};
use super::write_common::{parse_mode, run_write, Built, WriteBuilder};
use super::{defs, SolanaShared};
use crate::adapters::outbound::solana::rpc::SolanaRpc;
use crate::adapters::outbound::solana::send::TxPlan;
use crate::domain::lp::wallet::{
    parse_token_accounts_by_owner, TokenAccountRow, TokenAccountState,
};
use crate::domain::message::ToolDef;
use crate::domain::solana::{ids, Pubkey};
use crate::domain::solana_tx::spl_close_account;
use crate::domain::solana_write::Check;
use crate::domain::tools as names;
use crate::ports::tool::{Tool, ToolCtx, ToolOutput};

/// Accounts closed per transaction (bot: 4; a close adds one key + 3 bytes).
pub(crate) const BATCH: usize = 8;
/// Max `keep_mints` entries.
const MAX_KEEP: usize = 32;

pub(crate) fn tools(shared: &SolanaShared) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(CloseTokenAccountsTool {
        def: defs::def(names::SOLANA_CLOSE_TOKEN_ACCOUNTS),
        shared: shared.clone(),
    })]
}

struct CloseTokenAccountsTool {
    def: ToolDef,
    shared: SolanaShared,
}

#[async_trait]
impl Tool for CloseTokenAccountsTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        ctx.scope.check_fs_write(ctx.workspace)?;
        let tool = names::SOLANA_CLOSE_TOKEN_ACCOUNTS;
        let wallet = require_pubkey(args, tool, "wallet")?;
        let mode = parse_mode(args, tool)?;
        let keep = parse_keep_mints(args)?;
        let builder = CloseBuilder { wallet, keep };
        run_write(ctx, &self.shared, tool, wallet, mode, &builder).await
    }
}

fn parse_keep_mints(args: &Value) -> Result<Vec<Pubkey>> {
    let tool = names::SOLANA_CLOSE_TOKEN_ACCOUNTS;
    let mut keep = vec![ids::key(ids::WSOL), ids::key(ids::USDC)];
    match args.get("keep_mints") {
        None | Some(Value::Null) => {}
        Some(Value::Array(items)) => {
            if items.len() > MAX_KEEP {
                return Err(anyhow!(
                    "{tool}: at most {MAX_KEEP} 'keep_mints', got {}",
                    items.len()
                ));
            }
            for (i, v) in items.iter().enumerate() {
                let m = parse_pubkey_value(v, tool, &format!("keep_mints[{i}]"))?;
                if !keep.contains(&m) {
                    keep.push(m);
                }
            }
        }
        Some(v) => return Err(anyhow!("{tool}: 'keep_mints' must be an array, got {v}")),
    }
    Ok(keep)
}

struct CloseBuilder {
    wallet: Pubkey,
    keep: Vec<Pubkey>,
}

/// Why a token account is left open (`None` = closable).
fn skip_reason(row: &TokenAccountRow, wallet: &Pubkey, keep: &[Pubkey]) -> Option<&'static str> {
    if keep.contains(&row.mint) {
        Some("protected_mint")
    } else if row.owner != *wallet {
        Some("foreign_authority")
    } else if row.state != TokenAccountState::Initialized {
        Some("frozen_or_uninitialized")
    } else if row.amount.raw != "0" {
        Some("non_zero")
    } else {
        None
    }
}

/// Closable rows (sorted by program, mint, address) + skip counts.
pub(crate) fn plan_closes(
    rows: &[TokenAccountRow],
    wallet: &Pubkey,
    keep: &[Pubkey],
) -> (Vec<TokenAccountRow>, Value) {
    let mut closable = Vec::new();
    let mut skipped = serde_json::Map::new();
    for row in rows {
        match skip_reason(row, wallet, keep) {
            None => closable.push(row.clone()),
            Some(why) => {
                let n = skipped.get(why).and_then(Value::as_u64).unwrap_or(0);
                skipped.insert(why.into(), json!(n + 1));
            }
        }
    }
    closable.sort_by(|a, b| (a.program, a.mint, a.address).cmp(&(b.program, b.mint, b.address)));
    (closable, Value::Object(skipped))
}

#[async_trait]
impl WriteBuilder for CloseBuilder {
    async fn build(&self, rpc: &SolanaRpc, _fence: Option<u64>) -> Result<Built> {
        let mut rows = Vec::new();
        for program in [ids::key(ids::TOKEN), ids::key(ids::TOKEN_2022)] {
            let (_, v) = rpc
                .get_token_accounts_by_owner(&self.wallet, &program)
                .await?;
            rows.extend(
                parse_token_accounts_by_owner(&v, &program)
                    .map_err(|e| anyhow!("{}: {}", e.field, e.message))?,
            );
        }
        let (closable, skipped) = plan_closes(&rows, &self.wallet, &self.keep);
        let reclaim: u64 = closable.iter().map(|r| r.lamports).sum();
        let plans = closable
            .chunks(BATCH)
            .enumerate()
            .map(|(i, chunk)| {
                let ixs = chunk
                    .iter()
                    .map(|r| spl_close_account(&r.address, &self.wallet, &self.wallet, &r.program))
                    .collect();
                TxPlan::new(&format!("close batch {}", i + 1), ixs)
            })
            .collect();
        let mut stale = vec![format!("solana_wallet/1:{}", self.wallet)];
        stale.extend(closable.iter().map(|r| format!("acct/1:{}", r.address)));
        Ok(Built {
            checks: vec![Check::new(
                "token_accounts_read",
                true,
                format!("{} token accounts read", rows.len()),
            )],
            details: json!({
                "closable": closable.iter().map(|r| json!({
                    "account": r.address, "mint": r.mint, "program": r.program,
                    "rent_lamports": r.lamports,
                })).collect::<Vec<_>>(),
                "rent_reclaim_lamports": reclaim,
                "skipped": skipped,
                "protected_mints": self.keep,
            }),
            plans,
            stale_keys: stale,
            independent: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::lp::wallet::TokenAmount;

    fn row(
        i: u8,
        mint: Pubkey,
        raw: u64,
        state: TokenAccountState,
        owner: Pubkey,
    ) -> TokenAccountRow {
        TokenAccountRow {
            address: Pubkey([i; 32]),
            program: ids::key(if i % 2 == 0 {
                ids::TOKEN
            } else {
                ids::TOKEN_2022
            }),
            mint,
            owner,
            amount: TokenAmount::new(mint, raw, 6),
            state,
            is_native: false,
            lamports: 2_039_280,
        }
    }

    #[test]
    fn only_empty_unprotected_own_accounts_close() {
        let w = Pubkey([200; 32]);
        let junk = Pubkey([100; 32]);
        let keep =
            parse_keep_mints(&json!({"keep_mints": [Pubkey([101; 32]).to_string()]})).unwrap();
        assert_eq!(keep.len(), 3, "wSOL + USDC + one");
        use TokenAccountState::*;
        let rows = vec![
            row(1, junk, 0, Initialized, w),
            row(2, junk, 0, Initialized, w),
            row(3, junk, 5, Initialized, w),
            row(4, junk, 0, Frozen, w),
            row(5, junk, 0, Initialized, Pubkey([7; 32])),
            row(6, ids::key(ids::WSOL), 0, Initialized, w),
            row(7, ids::key(ids::USDC), 0, Initialized, w),
            row(8, Pubkey([101; 32]), 0, Initialized, w),
        ];
        let (closable, skipped) = plan_closes(&rows, &w, &keep);
        let mut got: Vec<u8> = closable.iter().map(|r| r.address.0[0]).collect();
        got.sort();
        assert_eq!(got, vec![1, 2]);
        assert_eq!(
            skipped,
            json!({"non_zero": 1, "frozen_or_uninitialized": 1, "foreign_authority": 1, "protected_mint": 3})
        );
    }

    // ── the tool end-to-end on a fake cluster ───────────────────────

    use crate::adapters::outbound::observations::SqliteObservationStore;
    use crate::adapters::outbound::solana::signer::LocalKeypair;
    use crate::adapters::outbound::solana::test_chain::Chain;
    use crate::adapters::outbound::solana::writes_store::SqliteWriteStore;
    use crate::adapters::outbound::tools::solana::write_common::run_write_with;
    use crate::domain::observation::{ObsSource, ObsStatus, Observation};
    use crate::domain::scope::ToolScope;
    use crate::domain::solana_write::{WriteMode, WriteResult, WriteStatus};
    use crate::ports::solana_signer::SolanaSigner;

    fn keypair() -> LocalKeypair {
        LocalKeypair::from_seed(&[1; 32])
    }

    /// jsonParsed token account rows: `n` empty junk accounts (Tokenkeg) +
    /// one empty USDC + one funded junk account.
    fn token_accounts(wallet: &Pubkey, program: &str, n: u8) -> Value {
        let row = |addr: Pubkey, mint: Pubkey, amount: &str| {
            json!({"pubkey": addr.to_string(), "account": {
                "owner": program, "lamports": 2_039_280, "executable": false, "rentEpoch": 0,
                "data": {"program": "spl-token", "space": 165, "parsed": {"type": "account", "info": {
                    "mint": mint.to_string(), "owner": wallet.to_string(), "state": "initialized",
                    "isNative": false,
                    "tokenAmount": {"amount": amount, "decimals": 6, "uiAmount": 0.0, "uiAmountString": "0"}}}}}})
        };
        let mut v = vec![];
        if program == ids::TOKEN {
            for i in 0..n {
                v.push(row(Pubkey([i + 10; 32]), Pubkey([100; 32]), "0"));
            }
            v.push(row(Pubkey([90; 32]), ids::key(ids::USDC), "0"));
            v.push(row(Pubkey([91; 32]), Pubkey([100; 32]), "5"));
        }
        json!({"context": {"slot": 500}, "value": v})
    }

    struct Rig {
        chain: Arc<Chain>,
        dir: tempfile::TempDir,
        shared: SolanaShared,
        wallet: Pubkey,
    }

    fn rig(n: u8, with_key: bool) -> Rig {
        let chain = Arc::new(Chain::new());
        let wallet = keypair().pubkey();
        chain.route(move |b| {
            (b["method"] == "getTokenAccountsByOwner")
                .then(|| token_accounts(&wallet, b["params"][1]["programId"].as_str().unwrap(), n))
        });
        let dir = tempfile::tempdir().unwrap();
        let key_file = with_key.then(|| {
            use std::os::unix::fs::PermissionsExt;
            let p = dir.path().join("signer.json");
            let mut bytes = vec![1u8; 32];
            bytes.extend_from_slice(&keypair().pubkey().0);
            std::fs::write(&p, serde_json::to_vec(&bytes).unwrap()).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
            p
        });
        let shared = SolanaShared {
            store: Some(Arc::new(SqliteObservationStore::open(dir.path()).unwrap())),
            writes: Some(Arc::new(
                SqliteWriteStore::open(&dir.path().join("state")).unwrap(),
            )),
            signer_key_file: key_file,
        };
        Rig {
            chain,
            dir,
            shared,
            wallet,
        }
    }

    impl Rig {
        async fn run(&self, mode: WriteMode, scope: &ToolScope) -> WriteResult {
            let rpc = Arc::new(SolanaRpc::with_transport(self.chain.clone(), "fake.rpc"));
            let builder = CloseBuilder {
                wallet: self.wallet,
                keep: parse_keep_mints(&json!({})).unwrap(),
            };
            run_write_with(
                rpc,
                scope,
                &self.shared,
                names::SOLANA_CLOSE_TOKEN_ACCOUNTS,
                self.wallet,
                mode,
                &builder,
            )
            .await
        }
        fn granted(&self) -> ToolScope {
            ToolScope {
                wallets: vec![self.wallet.to_string()],
                ..Default::default()
            }
        }
    }

    #[tokio::test]
    async fn simulate_batches_and_sends_nothing() {
        let r = rig(10, false);
        let out = r.run(WriteMode::Simulate, &ToolScope::default()).await;
        assert_eq!(out.status, WriteStatus::Simulated, "{:?}", out.refused);
        assert_eq!(out.txs.len(), 2, "8 + 2");
        assert_eq!(out.details["closable"].as_array().unwrap().len(), 10);
        assert_eq!(out.details["rent_reclaim_lamports"], 10 * 2_039_280u64);
        assert_eq!(
            out.details["skipped"],
            json!({"protected_mint": 1, "non_zero": 1})
        );
        assert!(!r.chain.methods().contains(&"sendTransaction".to_string()));
        let obs = Observation::of("t", &out, 1, 0, ObsSource::Live);
        assert_eq!(obs.status, ObsStatus::Ok);
    }

    #[tokio::test]
    async fn nothing_to_close_is_noop() {
        let r = rig(0, true);
        let out = r.run(WriteMode::Send, &r.granted()).await;
        assert_eq!(out.status, WriteStatus::Noop, "{:?}", out.refused);
        assert!(out.txs.is_empty());
        assert!(!r.chain.methods().contains(&"sendTransaction".to_string()));
    }

    #[tokio::test]
    async fn send_is_refused_without_a_wallet_grant_or_key() {
        let r = rig(3, true);
        for scope in [
            ToolScope::default(),
            ToolScope {
                wallets: vec!["*".into()],
                ..Default::default()
            },
            ToolScope {
                wallets: vec!["default".into()],
                ..Default::default()
            },
        ] {
            let out = r.run(WriteMode::Send, &scope).await;
            assert_eq!(out.status, WriteStatus::Refused);
            assert!(
                out.refused
                    .as_deref()
                    .unwrap()
                    .starts_with("signer_not_allowed"),
                "{:?}",
                out.refused
            );
        }
        let no_key = rig(3, false);
        let out = no_key.run(WriteMode::Send, &no_key.granted()).await;
        assert!(out.refused.as_deref().unwrap().starts_with("no_signer"));
        let mut other = rig(3, true);
        other.wallet = Pubkey([42; 32]);
        let out = other.run(WriteMode::Send, &other.granted()).await;
        assert!(out
            .refused
            .as_deref()
            .unwrap()
            .starts_with("signer_mismatch"));
        assert!(!r
            .chain
            .methods()
            .iter()
            .any(|m| m == "sendTransaction" || m == "getTokenAccountsByOwner"));
    }

    #[tokio::test]
    async fn send_confirms_every_batch_and_drops_stale_rows() {
        let r = rig(10, true);
        let store = r.shared.store.clone().unwrap();
        let mut row = Observation::of(
            "t",
            &WriteResult::new("x", "y", WriteMode::Simulate, 1, "h"),
            1,
            60_000,
            ObsSource::Live,
        );
        row.key = format!("solana_wallet/1:{}", r.wallet);
        row.status = ObsStatus::Ok;
        row.errors.clear();
        assert!(store.put(&row).await.unwrap());
        let out = r.run(WriteMode::Send, &r.granted()).await;
        assert_eq!(
            out.status,
            WriteStatus::Confirmed,
            "{:?} {:?}",
            out.refused,
            out.txs
        );
        assert_eq!(out.txs.len(), 2);
        assert!(out.txs.iter().all(|t| t.signature.is_some()));
        assert!(
            store.get(&row.key).await.unwrap().is_none(),
            "stale wallet row dropped"
        );
        let writes = r.shared.writes.clone().unwrap();
        assert_eq!(
            writes.fence(&r.wallet.to_string()).await.unwrap(),
            Some(777)
        );
        assert_eq!(r.chain.sent.lock().unwrap().len(), 2);
        drop(r.dir);
    }

    /// Live, keyless: simulate closing the funded operator wallet's empty
    /// token accounts on mainnet. `cargo test --bin tengu -- --ignored
    /// live_close_token_accounts --nocapture`.
    #[tokio::test]
    #[ignore]
    async fn live_close_token_accounts_simulate() {
        let wallet: Pubkey = "F3YvPiLdniRPGpeKrbeGWR2zg2wPpzVuvqBA5BBJBQ5S"
            .parse()
            .unwrap();
        let rpc = Arc::new(crate::adapters::outbound::solana::rpc::tests::live_rpc());
        let builder = CloseBuilder {
            wallet,
            keep: parse_keep_mints(&json!({})).unwrap(),
        };
        let out = run_write_with(
            rpc,
            &ToolScope::default(),
            &SolanaShared::default(),
            names::SOLANA_CLOSE_TOKEN_ACCOUNTS,
            wallet,
            WriteMode::Simulate,
            &builder,
        )
        .await;
        let obs = Observation::of(
            "t",
            &out,
            crate::domain::observation::now_ms(),
            0,
            ObsSource::Live,
        );
        println!("{}", obs.render_text(obs.observed_at_ms));
        for t in &out.txs {
            println!(
                "{} {:?} units={:?} size={:?} logs={:#?}",
                t.label, t.status, t.units, t.tx_size, t.logs_tail
            );
        }
        assert!(
            matches!(out.status, WriteStatus::Simulated | WriteStatus::Noop),
            "{:?} {:?}",
            out.refused,
            out.txs
        );

        // The pipeline itself on mainnet: simulate CloseAccount of any empty
        // account of the wallet (protection bypassed — a check, not the tool).
        let rpc = Arc::new(crate::adapters::outbound::solana::rpc::tests::live_rpc());
        let mut empty = None;
        for program in [ids::key(ids::TOKEN), ids::key(ids::TOKEN_2022)] {
            let (_, v) = rpc
                .get_token_accounts_by_owner(&wallet, &program)
                .await
                .unwrap();
            let rows = parse_token_accounts_by_owner(&v, &program).unwrap();
            empty = empty.or(rows.into_iter().find(|r| r.amount.raw == "0"));
        }
        let (label, ixs) = match empty {
            Some(row) => (
                format!("close {} ({})", row.address, row.mint),
                vec![spl_close_account(
                    &row.address,
                    &wallet,
                    &wallet,
                    &row.program,
                )],
            ),
            // No empty account: prove the wire format with a 1-lamport
            // self-transfer instead.
            None => (
                "self-transfer 1 lamport".to_string(),
                vec![crate::domain::solana_tx::system_transfer(
                    &wallet, &wallet, 1,
                )],
            ),
        };
        let p = crate::adapters::outbound::solana::send::Pipeline::new(rpc, "t", wallet);
        let t = p.simulate(&TxPlan::new(&label, ixs)).await;
        println!(
            "{label}: {:?} units={:?} size={:?} logs={:#?}",
            t.status, t.units, t.tx_size, t.logs_tail
        );
        assert_eq!(
            t.status(),
            WriteStatus::Simulated,
            "{:?} {:?}",
            t.err,
            t.note
        );
        assert!(t.units.unwrap() > 0);
    }

    #[test]
    fn keep_mints_is_validated() {
        assert!(parse_keep_mints(&json!({"keep_mints": "x"})).is_err());
        assert!(parse_keep_mints(&json!({"keep_mints": ["not-a-key"]})).is_err());
        let many: Vec<String> = (0..33u8).map(|i| Pubkey([i; 32]).to_string()).collect();
        assert!(parse_keep_mints(&json!({ "keep_mints": many })).is_err());
    }
}
