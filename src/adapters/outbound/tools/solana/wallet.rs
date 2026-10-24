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
#[path = "wallet_tests.rs"]
mod tests;
