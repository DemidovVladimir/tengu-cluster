//! Meteora DLMM write tools. Instruction encoders: `domain/lp/dlmm_ix.rs`;
//! runner: `write_common.rs`.
//!
//! `dlmm_close_position` (bot `meteoraAdapter.ts` `removeLiquidity({bps:
//! 10000, shouldClaimAndClose: true})`):
//!
//! | Rule | Detail |
//! |---|---|
//! | Reads | live, pinned ≥ the wallet's write fence: position + pool + bitmap extension, then mints + reward mints |
//! | Checks | position exists and decodes; owner = wallet; in `pool`; fee owner default or the wallet; both pool mints Tokenkeg (Token-2022 pools not supported yet) |
//! | Transaction | ATA idempotent X / Y / reward mints → `remove_liquidity_by_range2` (10000 bps, only when a bin holds liquidity) → `claim_fee2` → `claim_reward2` per initialized reward → `close_position_if_empty` → wSOL unwrap. Over 1232 bytes ⇒ split after the claims (rewards stop accruing once liquidity is out, so the close still finds the position empty) |
//! | > 70 bins | one [remove, claim fee, claim rewards] transaction per ≤ 70-bin chunk, then the close; every chunk is sent (the bot sent only the first) |
//! | wSOL unwrap | skipped while the wallet has an open Jupiter perps keeper request (or it cannot be read) — the keeper may pay into that account |
//! | `arm_reentry` | on a landed close: `lp_state.reentry = ReentryWait::arm(range low, range high, active price)` via `merge_lp_state` |

use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

use super::price::require_pubkey;
use super::write_common::{
    live_keeper_requests, merge_lp_state, parse_mode, run_write, Built, WriteBuilder,
};
use super::{defs, SolanaShared};
use crate::adapters::outbound::solana::rpc::SolanaRpc;
use crate::adapters::outbound::solana::send::TxPlan;
use crate::domain::lp::dlmm::{decode_lb_pair, decode_position_v2, LbPair, PositionV2};
use crate::domain::lp::dlmm_ix::{
    bin_arrays_for_range, bitmap_extension_pda, claim_fee2, claim_reward2, close_position_if_empty,
    remove_liquidity_by_range2, LiquidityAccounts, RewardAccounts, BPS_ALL,
};
use crate::domain::lp::gates::price_from_bin;
use crate::domain::lp::snapshot::ReentryWait;
use crate::domain::lp::wallet::decode_mint;
use crate::domain::message::ToolDef;
use crate::domain::observation::now_ms;
use crate::domain::solana::{ata, ids, AccountRead, Pubkey};
use crate::domain::solana_tx::{
    ata_create_idempotent, cu_limit, cu_price, spl_close_account, Instruction, LegacyMessage,
    MAX_COMPUTE_UNITS, PACKET_DATA_SIZE,
};
use crate::domain::solana_write::{Check, WriteResult, WriteStatus, CU_PRICE_FLOOR};
use crate::domain::tools as names;
use crate::ports::tool::{Tool, ToolCtx, ToolOutput};

/// Bins per remove / claim chunk (`DEFAULT_BIN_PER_POSITION`).
const CHUNK_BINS: i32 = 70;

pub(crate) fn tools(shared: &SolanaShared) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(CloseTool {
        def: defs::def(names::DLMM_CLOSE_POSITION),
        shared: shared.clone(),
    })]
}

/// Whether `ixs` (plus the pipeline's CU limit + price) fit one transaction.
pub(crate) fn fits(wallet: &Pubkey, ixs: &[Instruction]) -> bool {
    let mut all = vec![cu_limit(MAX_COMPUTE_UNITS), cu_price(CU_PRICE_FLOOR)];
    all.extend_from_slice(ixs);
    LegacyMessage::compile(wallet, &all, [0; 32])
        .map(|m| m.tx_size() <= PACKET_DATA_SIZE)
        .unwrap_or(false)
}

fn data_of(read: &AccountRead, what: &str) -> Result<Vec<u8>> {
    read.data()
        .ok_or_else(|| anyhow!("{what} {} does not exist", read.pubkey))
}

// ---------------------------------------------------------------------------
// dlmm_close_position
// ---------------------------------------------------------------------------

struct CloseTool {
    def: ToolDef,
    shared: SolanaShared,
}

#[async_trait]
impl Tool for CloseTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        ctx.scope.check_fs_write(ctx.workspace)?;
        let tool = names::DLMM_CLOSE_POSITION;
        let builder = CloseBuilder::parse(args)?;
        let mode = parse_mode(args, tool)?;
        let wallet = builder.wallet;
        run_write(ctx, &self.shared, tool, wallet, mode, &builder).await
    }
}

/// Prices of the closed range, kept for `arm_reentry`.
#[derive(Debug, Clone, Copy)]
struct ClosedRange {
    lower_price: f64,
    upper_price: f64,
    active_price: f64,
}

pub(crate) struct CloseBuilder {
    pub wallet: Pubkey,
    pub pool: Pubkey,
    pub position: Pubkey,
    pub arm_reentry: bool,
    range: Mutex<Option<ClosedRange>>,
}

impl CloseBuilder {
    pub(crate) fn parse(args: &Value) -> Result<Self> {
        let tool = names::DLMM_CLOSE_POSITION;
        let arm_reentry = args
            .get("arm_reentry")
            .and_then(Value::as_bool)
            .ok_or_else(|| anyhow!("{tool}: 'arm_reentry' is required (bool)"))?;
        Ok(CloseBuilder {
            wallet: require_pubkey(args, tool, "wallet")?,
            pool: require_pubkey(args, tool, "pool")?,
            position: require_pubkey(args, tool, "position")?,
            arm_reentry,
            range: Mutex::new(None),
        })
    }
}

/// Everything the close reads.
struct CloseReads {
    position: PositionV2,
    pair: LbPair,
    bitmap_extension: Option<Pubkey>,
    decimals: (u8, u8),
    /// `(index, mint, vault, token program)` per initialized reward.
    rewards: Vec<(u64, Pubkey, Pubkey, Pubkey)>,
}

async fn read_close(rpc: &SolanaRpc, b: &CloseBuilder, fence: Option<u64>) -> Result<CloseReads> {
    let ext = bitmap_extension_pda(&b.pool);
    let (slot, reads) = rpc
        .get_multiple_accounts(&[b.position, b.pool, ext], fence)
        .await?;
    let position = decode_position_v2(&data_of(&reads[0], "position")?)
        .map_err(|e| anyhow!("position {}: {e}", b.position))?;
    let pair = decode_lb_pair(&data_of(&reads[1], "pool")?)
        .map_err(|e| anyhow!("pool {}: {e}", b.pool))?;
    let bitmap_extension = reads[2].exists().then_some(ext);
    let rewards = pair.rewards();
    let mut keys = vec![pair.token_x_mint, pair.token_y_mint];
    keys.extend(rewards.iter().map(|r| r.1));
    let (_, mints) = rpc.get_multiple_accounts(&keys, Some(slot)).await?;
    let decimals_of = |r: &AccountRead| -> Result<(u8, Pubkey)> {
        let owner = *r
            .owner()
            .ok_or_else(|| anyhow!("mint {} does not exist", r.pubkey))?;
        let m = decode_mint(&owner, &data_of(r, "mint")?)
            .map_err(|e| anyhow!("mint {}: {}", r.pubkey, e.message))?;
        Ok((m.decimals, owner))
    };
    let (dx, _) = decimals_of(&mints[0])?;
    let (dy, _) = decimals_of(&mints[1])?;
    let mut reward_rows = Vec::new();
    for (i, (idx, mint, vault)) in rewards.into_iter().enumerate() {
        let (_, program) = decimals_of(&mints[2 + i])?;
        reward_rows.push((idx, mint, vault, program));
    }
    Ok(CloseReads {
        position,
        pair,
        bitmap_extension,
        decimals: (dx, dy),
        rewards: reward_rows,
    })
}

/// ≤ 70-bin chunks of `[lower, upper]`.
pub(crate) fn chunks(lower: i32, upper: i32) -> Vec<(i32, i32)> {
    let mut out = Vec::new();
    let mut lo = lower;
    while lo <= upper {
        let hi = (lo + CHUNK_BINS - 1).min(upper);
        out.push((lo, hi));
        lo = hi + 1;
    }
    out
}

impl CloseBuilder {
    fn plans(&self, rd: &CloseReads, unwrap_wsol: bool) -> Vec<TxPlan> {
        let w = self.wallet;
        let token = ids::key(ids::TOKEN);
        let pair = &rd.pair;
        let accounts = LiquidityAccounts {
            position: self.position,
            lb_pair: self.pool,
            bitmap_extension: rd.bitmap_extension,
            user_token_x: ata(&w, &pair.token_x_mint, &token),
            user_token_y: ata(&w, &pair.token_y_mint, &token),
            reserve_x: pair.reserve_x,
            reserve_y: pair.reserve_y,
            token_x_mint: pair.token_x_mint,
            token_y_mint: pair.token_y_mint,
            sender: w,
            token_x_program: token,
            token_y_program: token,
        };
        let mut atas = vec![
            ata_create_idempotent(&w, &accounts.user_token_x, &w, &pair.token_x_mint, &token),
            ata_create_idempotent(&w, &accounts.user_token_y, &w, &pair.token_y_mint, &token),
        ];
        let rewards: Vec<RewardAccounts> = rd
            .rewards
            .iter()
            .map(|(i, mint, vault, program)| RewardAccounts {
                reward_index: *i,
                reward_vault: *vault,
                reward_mint: *mint,
                user_token_account: ata(&w, mint, program),
                token_program: *program,
            })
            .collect();
        for r in &rewards {
            atas.push(ata_create_idempotent(
                &w,
                &r.user_token_account,
                &w,
                &r.reward_mint,
                &r.token_program,
            ));
        }
        let pos = &rd.position;
        let chunk_ixs: Vec<Vec<Instruction>> = chunks(pos.lower_bin_id, pos.upper_bin_id)
            .into_iter()
            .map(|(lo, hi)| {
                let arrays = bin_arrays_for_range(&self.pool, lo, hi);
                let has_liquidity = (lo..=hi).any(|b| {
                    pos.bins
                        .get((b - pos.lower_bin_id) as usize)
                        .is_some_and(|x| x.liquidity_share > 0)
                });
                let mut ixs = Vec::new();
                if has_liquidity {
                    ixs.push(remove_liquidity_by_range2(
                        &accounts, lo, hi, BPS_ALL, &arrays,
                    ));
                }
                ixs.push(claim_fee2(&accounts, lo, hi, &arrays));
                for r in &rewards {
                    ixs.push(claim_reward2(
                        &self.pool,
                        &self.position,
                        &w,
                        r,
                        lo,
                        hi,
                        &arrays,
                    ));
                }
                ixs
            })
            .collect();
        let mut close = vec![close_position_if_empty(&self.position, &w, &w)];
        let wsol = ids::key(ids::WSOL);
        if unwrap_wsol && (pair.token_x_mint == wsol || pair.token_y_mint == wsol) {
            close.push(spl_close_account(&ata(&w, &wsol, &token), &w, &w, &token));
        }
        if chunk_ixs.len() == 1 {
            let mut one = atas.clone();
            one.extend(chunk_ixs[0].clone());
            one.extend(close.clone());
            if fits(&w, &one) {
                return vec![TxPlan::new("remove + claim + close", one)];
            }
        }
        let n = chunk_ixs.len();
        let mut plans: Vec<TxPlan> = chunk_ixs
            .into_iter()
            .enumerate()
            .map(|(i, ixs)| {
                let mut all = atas.clone();
                all.extend(ixs);
                TxPlan::new(&format!("remove + claim {}/{n}", i + 1), all)
            })
            .collect();
        plans.push(TxPlan::new("close", close));
        plans
    }
}

#[async_trait]
impl WriteBuilder for CloseBuilder {
    async fn build(&self, rpc: &SolanaRpc, fence: Option<u64>) -> Result<Built> {
        let rd = read_close(rpc, self, fence).await?;
        let pos = &rd.position;
        let pair = &rd.pair;
        let fee_owner_ok = pos.fee_owner == Pubkey::default() || pos.fee_owner == self.wallet;
        let tokenkeg = pair.token_mint_x_program_flag == 0 && pair.token_mint_y_program_flag == 0;
        let (keeper_ok_to_unwrap, keeper_detail) =
            match live_keeper_requests(rpc, &self.wallet, now_ms() / 1000).await {
                Ok(open) if open.is_empty() => {
                    (true, "no open keeper request — wSOL unwrapped".to_string())
                }
                Ok(open) => (
                    false,
                    format!(
                        "open keeper request(s) {} — wSOL account left open",
                        open.iter()
                            .map(|(k, _)| k.to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                ),
                Err(e) => (
                    false,
                    format!("keeper requests unreadable ({e:#}) — wSOL account left open"),
                ),
            };
        let checks = vec![
            Check::new(
                "owner",
                pos.owner == self.wallet,
                format!("position owner {} (wallet {})", pos.owner, self.wallet),
            ),
            Check::new(
                "pool",
                pos.lb_pair == self.pool,
                format!("position pool {} (requested {})", pos.lb_pair, self.pool),
            ),
            Check::new(
                "fee_owner",
                fee_owner_ok,
                format!("fee owner {} (default or the wallet)", pos.fee_owner),
            ),
            Check::new(
                "token_programs",
                tokenkeg,
                if tokenkeg {
                    "both mints Tokenkeg".to_string()
                } else {
                    "a Token-2022 mint — not supported by the write tools yet".to_string()
                },
            ),
            Check::new("keeper_requests", true, keeper_detail),
        ];
        let (dx, dy) = rd.decimals;
        *self.range.lock().unwrap() = Some(ClosedRange {
            lower_price: price_from_bin(pos.lower_bin_id, pair.bin_step, dx, dy),
            upper_price: price_from_bin(pos.upper_bin_id, pair.bin_step, dx, dy),
            active_price: price_from_bin(pair.active_id, pair.bin_step, dx, dy),
        });
        let plans = self.plans(&rd, keeper_ok_to_unwrap);
        let w = self.wallet;
        let token = ids::key(ids::TOKEN);
        let mut stale = vec![
            format!("dlmm_positions/1:{w}:{}", self.pool),
            format!("dlmm_discovery/1:{w}:{}", self.pool),
            format!("lp_snapshot/1:{w}:{}", self.pool),
            format!("dlmm_pool/1:{}", self.pool),
            format!("solana_wallet/1:{w}"),
            format!("acct/1:{w}"),
            format!("acct/1:{}", self.position),
            format!("acct/1:{}", self.pool),
            format!("acct/1:{}", pair.reserve_x),
            format!("acct/1:{}", pair.reserve_y),
            format!("acct/1:{}", ata(&w, &pair.token_x_mint, &token)),
            format!("acct/1:{}", ata(&w, &pair.token_y_mint, &token)),
        ];
        stale.extend(
            bin_arrays_for_range(&self.pool, pos.lower_bin_id, pos.upper_bin_id)
                .iter()
                .map(|k| format!("acct/1:{k}")),
        );
        Ok(Built {
            checks,
            details: json!({
                "position": self.position,
                "pool": self.pool,
                "range": [pos.lower_bin_id, pos.upper_bin_id],
                "bins_with_liquidity": pos.bins.iter().filter(|b| b.liquidity_share > 0).count(),
                "rewards": rd.rewards.iter().map(|r| r.1).collect::<Vec<_>>(),
                "bitmap_extension": rd.bitmap_extension,
                "unwrap_wsol": keeper_ok_to_unwrap,
                "arm_reentry": self.arm_reentry,
                "transactions": plans.iter().map(|p| p.label.clone()).collect::<Vec<_>>(),
            }),
            plans,
            stale_keys: stale,
            independent: false,
        })
    }

    async fn after_send(&self, result: &WriteResult, shared: &SolanaShared) -> Option<String> {
        let closed = result.status == WriteStatus::Confirmed;
        if !self.arm_reentry {
            return None;
        }
        if !closed {
            return Some("reentry NOT armed: the close did not fully land".into());
        }
        let range = (*self.range.lock().unwrap())?;
        let now = now_ms();
        Some(
            merge_lp_state(
                shared.store.as_ref(),
                names::DLMM_CLOSE_POSITION,
                &self.wallet,
                &self.pool,
                |s| {
                    s.reentry = Some(ReentryWait::arm(
                        range.lower_price,
                        range.upper_price,
                        range.active_price,
                        now,
                    ))
                },
            )
            .await,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live, keyless: simulate closing the funded operator wallet's real
    /// SOL/USDC position on mainnet (full close path: ATAs, remove, claim
    /// fee, rewards, close, unwrap). `cargo test --bin tengu -- --ignored
    /// live_dlmm_close --nocapture`.
    #[tokio::test]
    #[ignore]
    async fn live_dlmm_close_simulate() {
        use crate::adapters::outbound::tools::solana::write_common::run_write_with;
        use crate::domain::lp::dlmm::position_gpa_filters;
        use crate::domain::observation::{ObsSource, Observation};
        use crate::domain::scope::ToolScope;
        use crate::domain::solana_write::WriteMode;
        let operator: Pubkey = "F3YvPiLdniRPGpeKrbeGWR2zg2wPpzVuvqBA5BBJBQ5S"
            .parse()
            .unwrap();
        let pool: Pubkey = "5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS6"
            .parse()
            .unwrap();
        let rpc = Arc::new(crate::adapters::outbound::solana::rpc::tests::live_rpc());
        let (_, mine) = rpc
            .get_program_account_keys(
                &ids::key(ids::DLMM),
                &position_gpa_filters(&operator, &pool),
            )
            .await
            .unwrap();
        // No operator position: any open position of the pool, simulated as
        // its owner (keyless, sigVerify off — public state, nothing sent).
        let (wallet, position) = match mine.first() {
            Some(p) => (operator, *p),
            None => {
                let filters = vec![
                    json!({"memcmp": {"offset": 0, "bytes": crate::domain::solana::bs58_encode(&crate::domain::lp::dlmm::POSITION_V2_DISC)}}),
                    json!({"memcmp": {"offset": 8, "bytes": pool.to_string()}}),
                ];
                let (_, any) = rpc
                    .get_program_account_keys(&ids::key(ids::DLMM), &filters)
                    .await
                    .unwrap();
                let (_, reads) = rpc
                    .get_multiple_accounts(&any[..any.len().min(40)], None)
                    .await
                    .unwrap();
                let open = reads
                    .iter()
                    .filter_map(|r| r.data().map(|d| (r.pubkey, d)))
                    .filter_map(|(k, d)| decode_position_v2(&d).ok().map(|p| (k, p)))
                    .find(|(_, p)| {
                        p.bins.iter().any(|b| b.liquidity_share > 0) && p.bins.len() <= 70
                    })
                    .expect("an open position in the pool");
                println!(
                    "operator has no position; simulating {} as owner {}",
                    open.0, open.1.owner
                );
                (open.1.owner, open.0)
            }
        };
        let b = CloseBuilder::parse(
            &json!({"wallet": wallet.to_string(), "pool": pool.to_string(),
            "position": position.to_string(), "arm_reentry": false}),
        )
        .unwrap();
        let out = run_write_with(
            rpc,
            &ToolScope::default(),
            &SolanaShared::default(),
            names::DLMM_CLOSE_POSITION,
            wallet,
            WriteMode::Simulate,
            &b,
        )
        .await;
        let obs = Observation::of("t", &out, now_ms(), 0, ObsSource::Live);
        println!("{}", obs.headline);
        println!("{}", serde_json::to_string_pretty(&out.details).unwrap());
        for c in &out.checks {
            println!("check {} {} — {}", c.name, c.ok, c.detail);
        }
        for t in &out.txs {
            println!(
                "{} {:?} units={:?} size={:?} err={:?} note={:?}\n{:#?}",
                t.label, t.status, t.units, t.tx_size, t.err, t.note, t.logs_tail
            );
        }
        assert_eq!(out.status, WriteStatus::Simulated, "{:?}", out.refused);
    }

    #[test]
    fn chunks_cover_the_range_in_70_bin_steps() {
        assert_eq!(chunks(-39, 30), vec![(-39, 30)]);
        assert_eq!(chunks(0, 69), vec![(0, 69)]);
        assert_eq!(chunks(0, 70), vec![(0, 69), (70, 70)]);
        assert_eq!(chunks(-100, 63), vec![(-100, -31), (-30, 39), (40, 63)]);
    }

    #[test]
    fn close_args_are_required() {
        let w = "AKnL4NNf3DGWZJS6cPknBuEGnVsV4A4m5tgebLHaRSZ9";
        let ok = json!({"wallet": w, "pool": w, "position": w, "arm_reentry": false});
        assert!(CloseBuilder::parse(&ok).is_ok());
        let mut bad = ok.clone();
        bad.as_object_mut().unwrap().remove("arm_reentry");
        assert!(CloseBuilder::parse(&bad).is_err());
    }
}
