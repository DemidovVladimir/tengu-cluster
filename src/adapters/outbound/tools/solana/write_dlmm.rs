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
//!
//! `dlmm_open_position` (bot `initializePositionAndAddLiquidityByStrategy`):
//!
//! | Rule | Detail |
//! |---|---|
//! | Range | `gates::centered_range(active, bin_count)` — ≤ 70 bins (the bot's re-center made 71) |
//! | Reads | live, pinned ≥ the wallet's fence: pool, bitmap extension, wallet SOL, mints, the wallet's X / Y ATAs, the range's bin arrays; position discovery (gPA) must answer at or after the fence (a lagging node could hide a just-opened position ⇒ double open) |
//! | Checks | Tokenkeg mints; amounts ≥ 1 base unit, not both 0; no position in the pool unless `allow_existing`; new bin arrays ≤ `max_new_bin_arrays`; SOL ≥ SOL legs + rent (position 0.0574, bin array 0.0714, ATA 0.00204 SOL) + 0.005 fees + `min_wallet_sol`; token legs ≤ ATA balances; SOL/USDC pool: active price within `max_divergence_bps` of the oracle |
//! | Transaction | `initialize_bin_array` (missing) → `initialize_position` (fresh key signs) → ATA idempotent X / Y → wSOL wrap (transfer + SyncNative) → `add_liquidity_by_strategy2` (spot / curve / bidask → the SDK's `*ImBalanced`, bitmap extension only when the range leaves the default bitmap) → wSOL unwrap. Over 1232 B ⇒ the bin arrays go first in their own transaction (position + liquidity stay atomic) |
//! | wSOL unwrap | skipped while a Jupiter keeper request is open or unreadable |

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
use crate::adapters::outbound::solana::signer::LocalKeypair;
use crate::domain::lp::dlmm::{
    decode_lb_pair, decode_position_v2, position_gpa_filters, LbPair, PositionV2,
};
use crate::domain::lp::dlmm_ix::{
    add_liquidity_by_strategy2, bin_arrays_for_range, bitmap_extension_pda, claim_fee2,
    claim_reward2, close_position_if_empty, initialize_bin_array, initialize_position,
    range_needs_bitmap_extension, remove_liquidity_by_range2, LiquidityAccounts, RewardAccounts,
    Strategy, StrategyLiquidity, BPS_ALL,
};
use crate::domain::lp::gates::{bin_array_indexes, centered_range, price_from_bin};
use crate::domain::lp::snapshot::ReentryWait;
use crate::domain::lp::wallet::decode_mint;
use crate::domain::message::ToolDef;
use crate::domain::observation::now_ms;
use crate::domain::solana::{ata, ids, AccountRead, Pubkey};
use crate::domain::solana_tx::{
    ata_create_idempotent, cu_limit, cu_price, spl_close_account, spl_sync_native, system_transfer,
    Instruction, LegacyMessage, MAX_COMPUTE_UNITS, PACKET_DATA_SIZE,
};
use crate::domain::solana_write::{Check, WriteResult, WriteStatus, CU_PRICE_FLOOR};
use crate::domain::tools as names;
use crate::ports::solana_signer::SolanaSigner;
use crate::ports::tool::{Tool, ToolCtx, ToolOutput};

/// Bins per remove / claim chunk (`DEFAULT_BIN_PER_POSITION`).
const CHUNK_BINS: i32 = 70;

pub(crate) fn tools(shared: &SolanaShared) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(CloseTool {
            def: defs::def(names::DLMM_CLOSE_POSITION),
            shared: shared.clone(),
        }),
        Arc::new(OpenTool {
            def: defs::def(names::DLMM_OPEN_POSITION),
            shared: shared.clone(),
        }),
    ]
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

// ---------------------------------------------------------------------------
// dlmm_open_position
// ---------------------------------------------------------------------------

/// Rent-exempt minimum of an account of `len` bytes: (128 + len) × 6960
/// lamports (3480 lamports / byte-year × 2 years).
pub(crate) fn rent_exempt(len: usize) -> u64 {
    (128 + len as u64) * 6_960
}
/// PositionV2 with 70 bins (`dlmm::POSITION_V2_MIN_LEN`).
const POSITION_LEN: usize = 8_120;
/// BinArray (`dlmm::BIN_ARRAY_MIN_LEN`).
const BIN_ARRAY_LEN: usize = 10_136;
const TOKEN_ACCOUNT_LEN: usize = 165;
/// SOL kept for signatures + priority fees on top of every need.
const FEE_BUFFER_LAMPORTS: u64 = 5_000_000;
const LAMPORTS_PER_SOL: f64 = 1e9;

struct OpenTool {
    def: ToolDef,
    shared: SolanaShared,
}

#[async_trait]
impl Tool for OpenTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        ctx.scope.check_fs_write(ctx.workspace)?;
        let tool = names::DLMM_OPEN_POSITION;
        let mut builder = OpenBuilder::parse(args)?;
        let mode = parse_mode(args, tool)?;
        builder.oracle = crate::adapters::outbound::solana::plan::oracle_usd(
            ctx,
            self.shared.store.as_deref(),
            ids::WSOL,
            now_ms(),
        )
        .await;
        let wallet = builder.wallet;
        run_write(ctx, &self.shared, tool, wallet, mode, &builder).await
    }
}

/// Strategy knobs of an open (all required — no defaults).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OpenKnobs {
    pub bin_count: u32,
    pub strategy: Strategy,
    pub max_active_bin_slippage: i32,
    pub min_wallet_sol: f64,
    pub max_new_bin_arrays: u32,
    pub max_divergence_bps: f64,
}

pub(crate) struct OpenBuilder {
    pub wallet: Pubkey,
    pub pool: Pubkey,
    /// UI units.
    pub amount_x: f64,
    pub amount_y: f64,
    pub knobs: OpenKnobs,
    pub allow_existing: bool,
    /// SOL / USD (set by the tool before running).
    pub oracle: Option<f64>,
}

fn num_arg(args: &Value, key: &str, min: f64) -> Result<f64> {
    let tool = names::DLMM_OPEN_POSITION;
    let v = args
        .get(key)
        .and_then(Value::as_f64)
        .ok_or_else(|| anyhow!("{tool}: '{key}' is required (number)"))?;
    if !v.is_finite() || v < min {
        return Err(anyhow!("{tool}: '{key}' must be >= {min}, got {v}"));
    }
    Ok(v)
}

impl OpenBuilder {
    pub(crate) fn parse(args: &Value) -> Result<Self> {
        let tool = names::DLMM_OPEN_POSITION;
        let strategy = args
            .get("strategy")
            .and_then(Value::as_str)
            .and_then(Strategy::parse)
            .ok_or_else(|| anyhow!("{tool}: 'strategy' must be spot | curve | bidask"))?;
        let int = |key: &str, min: f64, max: f64| -> Result<f64> {
            let v = num_arg(args, key, min)?;
            if v.fract() != 0.0 || v > max {
                return Err(anyhow!(
                    "{tool}: '{key}' must be an integer in [{min}, {max}], got {v}"
                ));
            }
            Ok(v)
        };
        let knobs = OpenKnobs {
            bin_count: int("bin_count", 1.0, 70.0)? as u32,
            strategy,
            max_active_bin_slippage: int("max_active_bin_slippage", 0.0, 1000.0)? as i32,
            min_wallet_sol: num_arg(args, "min_wallet_sol", 0.0)?,
            max_new_bin_arrays: int("max_new_bin_arrays", 0.0, 2.0)? as u32,
            max_divergence_bps: num_arg(args, "max_divergence_bps", 0.0)?,
        };
        if knobs.max_divergence_bps <= 0.0 {
            return Err(anyhow!("{tool}: 'max_divergence_bps' must be > 0"));
        }
        Ok(OpenBuilder {
            wallet: require_pubkey(args, tool, "wallet")?,
            pool: require_pubkey(args, tool, "pool")?,
            amount_x: num_arg(args, "amount_x", 0.0)?,
            amount_y: num_arg(args, "amount_y", 0.0)?,
            knobs,
            allow_existing: args
                .get("allow_existing")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            oracle: None,
        })
    }
}

/// Everything an open reads.
#[derive(Debug, Clone)]
pub(crate) struct OpenReads {
    pub pair: LbPair,
    pub bitmap_extension_exists: bool,
    pub decimals: (u8, u8),
    pub lamports: u64,
    /// Raw balance of the wallet's X / Y ATA; `None` = the ATA does not exist.
    pub ata_x: Option<u64>,
    pub ata_y: Option<u64>,
    /// Bin arrays of the range that do not exist yet (indexes).
    pub missing_bin_arrays: Vec<i64>,
    pub existing_positions: Vec<Pubkey>,
    /// Open keeper requests; `None` = unreadable.
    pub keeper_requests: Option<usize>,
}

fn ata_amount(read: &AccountRead) -> Result<Option<u64>> {
    match read.data() {
        None => Ok(None),
        Some(d) if d.len() >= 72 => Ok(Some(u64::from_le_bytes(
            d[64..72].try_into().expect("8 bytes"),
        ))),
        Some(_) => Err(anyhow!("token account {} is too short", read.pubkey)),
    }
}

async fn read_open(rpc: &SolanaRpc, b: &OpenBuilder, fence: Option<u64>) -> Result<OpenReads> {
    let ext = bitmap_extension_pda(&b.pool);
    let (slot, reads) = rpc.get_multiple_accounts(&[b.pool, ext], fence).await?;
    let pair = decode_lb_pair(&data_of(&reads[0], "pool")?)
        .map_err(|e| anyhow!("pool {}: {e}", b.pool))?;
    let range = centered_range(pair.active_id, b.knobs.bin_count)
        .ok_or_else(|| anyhow!("no range for active bin {}", pair.active_id))?;
    let token = ids::key(ids::TOKEN);
    let arrays = bin_array_indexes(range.min_bin_id, range.max_bin_id);
    let mut keys = vec![
        pair.token_x_mint,
        pair.token_y_mint,
        ata(&b.wallet, &pair.token_x_mint, &token),
        ata(&b.wallet, &pair.token_y_mint, &token),
    ];
    keys.extend(bin_arrays_for_range(
        &b.pool,
        range.min_bin_id,
        range.max_bin_id,
    ));
    let pin = Some(fence.unwrap_or(0).max(slot));
    let (_, more) = rpc.get_multiple_accounts(&keys, pin).await?;
    let decimals_of = |r: &AccountRead| -> Result<u8> {
        let owner = *r
            .owner()
            .ok_or_else(|| anyhow!("mint {} does not exist", r.pubkey))?;
        decode_mint(&owner, &data_of(r, "mint")?)
            .map(|m| m.decimals)
            .map_err(|e| anyhow!("mint {}: {}", r.pubkey, e.message))
    };
    let decimals = (decimals_of(&more[0])?, decimals_of(&more[1])?);
    let missing_bin_arrays = arrays
        .iter()
        .zip(&more[4..])
        .filter(|(_, r)| !r.exists())
        .map(|(i, _)| *i)
        .collect();
    let (_, lamports) = rpc.get_balance(&b.wallet).await?;
    let (gpa_slot, existing_positions) = rpc
        .get_program_account_keys(
            &ids::key(ids::DLMM),
            &position_gpa_filters(&b.wallet, &b.pool),
        )
        .await?;
    if let Some(f) = fence {
        if gpa_slot < f {
            return Err(anyhow!(
                "position discovery answered at slot {gpa_slot}, before the wallet's last write (slot {f}) — a node behind could hide a just-opened position; retry"
            ));
        }
    }
    let keeper_requests = live_keeper_requests(rpc, &b.wallet, now_ms() / 1000)
        .await
        .ok()
        .map(|v| v.len());
    Ok(OpenReads {
        bitmap_extension_exists: reads[1].exists(),
        decimals,
        lamports,
        ata_x: ata_amount(&more[2])?,
        ata_y: ata_amount(&more[3])?,
        missing_bin_arrays,
        existing_positions,
        keeper_requests,
        pair,
    })
}

/// A UI amount in base units (floored); `None` when not representable.
fn to_raw(ui: f64, decimals: u8) -> Option<u64> {
    let raw = (ui * 10f64.powi(i32::from(decimals))).floor();
    (raw.is_finite() && raw >= 0.0 && raw < u64::MAX as f64).then_some(raw as u64)
}

/// The pure part of an open: checks, the bin-array instructions, the main
/// instructions (in order), details. `position` = the fresh position key.
pub(crate) fn plan_open(
    b: &OpenBuilder,
    rd: &OpenReads,
    position: &Pubkey,
) -> (Vec<Check>, Vec<Instruction>, Vec<Instruction>, Value) {
    let w = b.wallet;
    let pair = &rd.pair;
    let token = ids::key(ids::TOKEN);
    let wsol = ids::key(ids::WSOL);
    let mut checks = Vec::new();
    let tokenkeg = pair.token_mint_x_program_flag == 0 && pair.token_mint_y_program_flag == 0;
    checks.push(Check::new(
        "token_programs",
        tokenkeg,
        if tokenkeg {
            "both mints Tokenkeg"
        } else {
            "a Token-2022 mint — not supported by the write tools yet"
        },
    ));
    let range = centered_range(pair.active_id, b.knobs.bin_count).expect("checked in read_open");
    let (rx, ry) = (
        to_raw(b.amount_x, rd.decimals.0),
        to_raw(b.amount_y, rd.decimals.1),
    );
    let amounts_ok = matches!((rx, ry), (Some(x), Some(y)) if x > 0 || y > 0)
        && (b.amount_x == 0.0 || rx > Some(0))
        && (b.amount_y == 0.0 || ry > Some(0));
    let (raw_x, raw_y) = (rx.unwrap_or(0), ry.unwrap_or(0));
    checks.push(Check::new(
        "amounts",
        amounts_ok,
        format!(
            "amount_x {} → {raw_x} raw, amount_y {} → {raw_y} raw (each ≥ 1 base unit, not both 0)",
            b.amount_x, b.amount_y
        ),
    ));
    checks.push(Check::new(
        "existing_position",
        rd.existing_positions.is_empty() || b.allow_existing,
        if rd.existing_positions.is_empty() {
            "no position of the wallet in this pool".to_string()
        } else {
            format!(
                "the wallet already has position(s) {} in this pool (allow_existing = {})",
                rd.existing_positions
                    .iter()
                    .map(Pubkey::to_string)
                    .collect::<Vec<_>>()
                    .join(", "),
                b.allow_existing
            )
        },
    ));
    let new_arrays = rd.missing_bin_arrays.len() as u32;
    checks.push(Check::new(
        "new_bin_arrays",
        new_arrays <= b.knobs.max_new_bin_arrays,
        format!(
            "{new_arrays} bin array(s) to create {:?} (max {}, {} SOL rent each, not refunded)",
            rd.missing_bin_arrays,
            b.knobs.max_new_bin_arrays,
            rent_exempt(BIN_ARRAY_LEN) as f64 / LAMPORTS_PER_SOL
        ),
    ));
    let sol_legs = if pair.token_x_mint == wsol { raw_x } else { 0 }
        + if pair.token_y_mint == wsol { raw_y } else { 0 };
    let missing_atas = u64::from(rd.ata_x.is_none()) + u64::from(rd.ata_y.is_none());
    let rent = rent_exempt(POSITION_LEN)
        + u64::from(new_arrays) * rent_exempt(BIN_ARRAY_LEN)
        + missing_atas * rent_exempt(TOKEN_ACCOUNT_LEN);
    let reserve = (b.knobs.min_wallet_sol * LAMPORTS_PER_SOL).ceil() as u64;
    let need = sol_legs + rent + FEE_BUFFER_LAMPORTS + reserve;
    checks.push(Check::new(
        "sol_budget",
        rd.lamports >= need,
        format!(
            "wallet {} SOL; needs {} SOL = legs {} + rent {} + fees {} + min_wallet_sol {}",
            rd.lamports as f64 / LAMPORTS_PER_SOL,
            need as f64 / LAMPORTS_PER_SOL,
            sol_legs as f64 / LAMPORTS_PER_SOL,
            rent as f64 / LAMPORTS_PER_SOL,
            FEE_BUFFER_LAMPORTS as f64 / LAMPORTS_PER_SOL,
            b.knobs.min_wallet_sol
        ),
    ));
    for (name, mint, raw, bal) in [
        ("balance_x", pair.token_x_mint, raw_x, rd.ata_x),
        ("balance_y", pair.token_y_mint, raw_y, rd.ata_y),
    ] {
        if mint == wsol || raw == 0 {
            continue;
        }
        let have = bal.unwrap_or(0);
        checks.push(Check::new(
            name,
            have >= raw,
            format!("{mint}: need {raw}, the wallet's ATA holds {have}"),
        ));
    }
    let (dx, dy) = rd.decimals;
    let pool_price = price_from_bin(pair.active_id, pair.bin_step, dx, dy);
    let sol_usdc = pair.token_x_mint == wsol && pair.token_y_mint == ids::key(ids::USDC);
    if sol_usdc {
        let (ok, detail) = match b.oracle {
            Some(o) if o > 0.0 => {
                let bps = (pool_price - o).abs() / o * 1e4;
                (
                    bps <= b.knobs.max_divergence_bps,
                    format!(
                        "pool {pool_price} vs oracle {o}: {bps:.1} bps (max {})",
                        b.knobs.max_divergence_bps
                    ),
                )
            }
            _ => (false, "no SOL/USD oracle price".to_string()),
        };
        checks.push(Check::new("divergence", ok, detail));
    } else {
        checks.push(Check::new(
            "divergence",
            true,
            "not applicable (not a wSOL/USDC pool)",
        ));
    }
    let unwrap = rd.keeper_requests == Some(0);
    checks.push(Check::new(
        "keeper_requests",
        true,
        match rd.keeper_requests {
            Some(0) => "no open keeper request — wSOL unwrapped at the end".to_string(),
            Some(n) => format!("{n} open keeper request(s) — wSOL account left open"),
            None => "keeper requests unreadable — wSOL account left open".to_string(),
        },
    ));

    let arrays_ixs: Vec<Instruction> = rd
        .missing_bin_arrays
        .iter()
        .map(|i| initialize_bin_array(&b.pool, *i, &w))
        .collect();
    let user_x = ata(&w, &pair.token_x_mint, &token);
    let user_y = ata(&w, &pair.token_y_mint, &token);
    let mut main = vec![
        initialize_position(&w, position, &b.pool, range.min_bin_id, range.width as i32),
        ata_create_idempotent(&w, &user_x, &w, &pair.token_x_mint, &token),
        ata_create_idempotent(&w, &user_y, &w, &pair.token_y_mint, &token),
    ];
    for (mint, user, raw) in [
        (pair.token_x_mint, user_x, raw_x),
        (pair.token_y_mint, user_y, raw_y),
    ] {
        if mint == wsol && raw > 0 {
            main.push(system_transfer(&w, &user, raw));
            main.push(spl_sync_native(&user));
        }
    }
    let accounts = LiquidityAccounts {
        position: *position,
        lb_pair: b.pool,
        bitmap_extension: range_needs_bitmap_extension(range.min_bin_id, range.max_bin_id)
            .then(|| bitmap_extension_pda(&b.pool)),
        user_token_x: user_x,
        user_token_y: user_y,
        reserve_x: pair.reserve_x,
        reserve_y: pair.reserve_y,
        token_x_mint: pair.token_x_mint,
        token_y_mint: pair.token_y_mint,
        sender: w,
        token_x_program: token,
        token_y_program: token,
    };
    main.push(add_liquidity_by_strategy2(
        &accounts,
        &StrategyLiquidity {
            amount_x: raw_x,
            amount_y: raw_y,
            active_id: pair.active_id,
            max_active_bin_slippage: b.knobs.max_active_bin_slippage,
            min_bin_id: range.min_bin_id,
            max_bin_id: range.max_bin_id,
            strategy: b.knobs.strategy,
        },
        &bin_arrays_for_range(&b.pool, range.min_bin_id, range.max_bin_id),
    ));
    if unwrap && (pair.token_x_mint == wsol || pair.token_y_mint == wsol) {
        main.push(spl_close_account(&ata(&w, &wsol, &token), &w, &w, &token));
    }
    let details = json!({
        "position": position,
        "pool": b.pool,
        "range": [range.min_bin_id, range.max_bin_id],
        "bins": range.width,
        "active_id": pair.active_id,
        "pool_price": pool_price,
        "oracle_sol_usd": b.oracle,
        "amount_x_raw": raw_x,
        "amount_y_raw": raw_y,
        "strategy": format!("{:?}", b.knobs.strategy).to_lowercase(),
        "new_bin_arrays": rd.missing_bin_arrays,
        "rent_lamports": rent,
        "sol_needed_lamports": need,
        "wallet_lamports": rd.lamports,
        "bitmap_extension_exists": rd.bitmap_extension_exists,
        "unwrap_wsol": unwrap,
    });
    (checks, arrays_ixs, main, details)
}

#[async_trait]
impl WriteBuilder for OpenBuilder {
    async fn build(&self, rpc: &SolanaRpc, fence: Option<u64>) -> Result<Built> {
        let rd = read_open(rpc, self, fence).await?;
        let key = Arc::new(LocalKeypair::generate()?);
        let position = key.pubkey();
        let (checks, arrays_ixs, main, details) = plan_open(self, &rd, &position);
        let mut one = arrays_ixs.clone();
        one.extend(main.clone());
        let signer: Arc<dyn SolanaSigner> = key;
        let plans = if arrays_ixs.is_empty() || fits(&self.wallet, &one) {
            let mut p = TxPlan::new("open + add liquidity", one);
            p.extra_signers.push(signer);
            vec![p]
        } else {
            let mut p = TxPlan::new("open + add liquidity", main);
            p.extra_signers.push(signer);
            vec![TxPlan::new("init bin arrays", arrays_ixs), p]
        };
        let w = self.wallet;
        let token = ids::key(ids::TOKEN);
        let pair = &rd.pair;
        let stale = vec![
            format!("dlmm_positions/1:{w}:{}", self.pool),
            format!("dlmm_discovery/1:{w}:{}", self.pool),
            format!("lp_snapshot/1:{w}:{}", self.pool),
            format!("dlmm_pool/1:{}", self.pool),
            format!("solana_wallet/1:{w}"),
            format!("acct/1:{w}"),
            format!("acct/1:{}", self.pool),
            format!("acct/1:{}", pair.reserve_x),
            format!("acct/1:{}", pair.reserve_y),
            format!("acct/1:{}", ata(&w, &pair.token_x_mint, &token)),
            format!("acct/1:{}", ata(&w, &pair.token_y_mint, &token)),
        ];
        Ok(Built {
            checks,
            plans,
            details,
            stale_keys: stale,
            independent: false,
        })
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

    /// Live, keyless: simulate opening a small 20-bin spot position as the
    /// operator wallet on mainnet. `cargo test --bin tengu -- --ignored
    /// live_dlmm_open --nocapture`.
    #[tokio::test]
    #[ignore]
    async fn live_dlmm_open_simulate() {
        use crate::adapters::outbound::solana::http_json::request_json;
        use crate::adapters::outbound::solana::rpc::REQUEST_TIMEOUT;
        use crate::adapters::outbound::tools::solana::write_common::run_write_with;
        use crate::domain::scope::ToolScope;
        use crate::domain::solana_write::WriteMode;
        let scope = ToolScope {
            net_hosts: vec!["lite-api.jup.ag".into()],
            ..Default::default()
        };
        let http = crate::adapters::outbound::egress::policy()
            .tool_client(REQUEST_TIMEOUT)
            .unwrap();
        let (_, price) = request_json(
            &http,
            &scope,
            &format!("https://lite-api.jup.ag/price/v3?ids={}", ids::WSOL),
            None,
            REQUEST_TIMEOUT,
        )
        .await
        .unwrap();
        let mut b = OpenBuilder::parse(&json!({
            "wallet": "F3YvPiLdniRPGpeKrbeGWR2zg2wPpzVuvqBA5BBJBQ5S",
            "pool": "5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS6",
            "amount_x": 0.02, "amount_y": 2.0, "bin_count": 20, "strategy": "spot",
            "max_active_bin_slippage": 3, "min_wallet_sol": 0.05, "max_new_bin_arrays": 2,
            "max_divergence_bps": 200,
        }))
        .unwrap();
        b.oracle = price[ids::WSOL]["usdPrice"].as_f64();
        let rpc = Arc::new(crate::adapters::outbound::solana::rpc::tests::live_rpc());
        let out = run_write_with(
            rpc,
            &ToolScope::default(),
            &SolanaShared::default(),
            names::DLMM_OPEN_POSITION,
            b.wallet,
            WriteMode::Simulate,
            &b,
        )
        .await;
        println!("{} {:?}", out.status.as_str(), out.refused);
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
        assert!(
            out.status == WriteStatus::Simulated
                || out
                    .refused
                    .as_deref()
                    .is_some_and(|r| r.starts_with("sol_budget") || r.starts_with("balance_")),
            "{:?} {:?}",
            out.refused,
            out.txs
        );
    }

    // ── dlmm_open_position planning (pure) ──────────────────────────

    use crate::domain::lp::dlmm_ix::{ADD_LIQUIDITY_BY_STRATEGY2, INITIALIZE_POSITION};

    fn fixture_pair() -> LbPair {
        use base64::Engine as _;
        let gma: Value =
            serde_json::from_str(crate::adapters::outbound::solana::plan::tests::DLMM_GMA).unwrap();
        let b64 = gma["result"]["value"][0]["data"][0].as_str().unwrap();
        decode_lb_pair(
            &base64::engine::general_purpose::STANDARD
                .decode(b64)
                .unwrap(),
        )
        .unwrap()
    }

    fn open_args() -> Value {
        json!({
            "wallet": "AKnL4NNf3DGWZJS6cPknBuEGnVsV4A4m5tgebLHaRSZ9",
            "pool": "5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS6",
            "amount_x": 0.5, "amount_y": 60.0, "bin_count": 20, "strategy": "spot",
            "max_active_bin_slippage": 1, "min_wallet_sol": 0.2, "max_new_bin_arrays": 1,
            "max_divergence_bps": 150,
        })
    }

    fn reads(pair: LbPair) -> OpenReads {
        OpenReads {
            pair,
            bitmap_extension_exists: true,
            decimals: (9, 6),
            lamports: 2_000_000_000,
            ata_x: Some(0),
            ata_y: Some(100_000_000),
            missing_bin_arrays: vec![],
            existing_positions: vec![],
            keeper_requests: Some(0),
        }
    }

    fn builder_with_oracle(pair: &LbPair) -> OpenBuilder {
        let mut b = OpenBuilder::parse(&open_args()).unwrap();
        b.oracle = Some(price_from_bin(pair.active_id, pair.bin_step, 9, 6));
        b
    }

    fn failed(checks: &[Check]) -> Vec<String> {
        checks
            .iter()
            .filter(|c| !c.ok)
            .map(|c| c.name.clone())
            .collect()
    }

    #[test]
    fn open_plans_the_sdk_instruction_order() {
        let pair = fixture_pair();
        let b = builder_with_oracle(&pair);
        let rd = reads(pair.clone());
        let position = Pubkey([2; 32]);
        let (checks, arrays, main, details) = plan_open(&b, &rd, &position);
        assert!(failed(&checks).is_empty(), "{checks:?}");
        assert!(arrays.is_empty());
        let shape: Vec<(String, Vec<u8>)> = main
            .iter()
            .map(|ix| {
                (
                    ix.program_id.to_string(),
                    ix.data[..ix.data.len().min(8)].to_vec(),
                )
            })
            .collect();
        let dlmm = ids::DLMM.to_string();
        assert_eq!(shape[0], (dlmm.clone(), INITIALIZE_POSITION.to_vec()));
        assert_eq!(shape[1].0, ids::ATA);
        assert_eq!(shape[2].0, ids::ATA);
        assert_eq!(shape[3].0, ids::SYSTEM, "wrap: transfer");
        assert_eq!(
            shape[4],
            (ids::TOKEN.to_string(), vec![17]),
            "wrap: SyncNative"
        );
        assert_eq!(shape[5], (dlmm, ADD_LIQUIDITY_BY_STRATEGY2.to_vec()));
        assert_eq!(shape[6], (ids::TOKEN.to_string(), vec![9]), "unwrap");
        assert_eq!(main.len(), 7);
        assert_eq!(details["amount_x_raw"], 500_000_000u64);
        assert_eq!(details["amount_y_raw"], 60_000_000u64);
        let (lo, hi) = (
            details["range"][0].as_i64().unwrap(),
            details["range"][1].as_i64().unwrap(),
        );
        assert_eq!(hi - lo + 1, 20);
        assert!(lo <= i64::from(pair.active_id) && i64::from(pair.active_id) <= hi);
        // The transfer wraps exactly amount_x into the wallet's wSOL ATA.
        assert_eq!(main[3].data[4..12], 500_000_000u64.to_le_bytes());
        assert!(fits(&b.wallet, &main), "one transaction");
    }

    #[test]
    fn open_refusals() {
        let pair = fixture_pair();
        let p = Pubkey([2; 32]);
        let check = |f: &dyn Fn(&mut OpenBuilder, &mut OpenReads)| {
            let mut b = builder_with_oracle(&pair);
            let mut rd = reads(pair.clone());
            f(&mut b, &mut rd);
            failed(&plan_open(&b, &rd, &p).0)
        };
        assert_eq!(
            check(&|_, r| r.existing_positions = vec![Pubkey([5; 32])]),
            vec!["existing_position"]
        );
        assert!(check(&|b, r| {
            r.existing_positions = vec![Pubkey([5; 32])];
            b.allow_existing = true;
        })
        .is_empty());
        assert_eq!(
            check(&|_, r| r.missing_bin_arrays = vec![-78, -77]),
            vec!["new_bin_arrays"]
        );
        // 0.5 SOL leg + 0.0574 position rent + 0.005 fees + 0.2 reserve = 0.7624.
        assert_eq!(check(&|_, r| r.lamports = 762_406_079), vec!["sol_budget"]);
        assert!(check(&|_, r| r.lamports = 762_406_080).is_empty());
        assert_eq!(check(&|_, r| r.ata_y = Some(59_999_999)), vec!["balance_y"]);
        assert_eq!(
            check(&|b, _| b.oracle = Some(b.oracle.unwrap() * 1.02)),
            vec!["divergence"]
        );
        assert_eq!(check(&|b, _| b.oracle = None), vec!["divergence"]);
        assert_eq!(
            check(&|b, _| {
                b.amount_x = 0.0;
                b.amount_y = 0.0
            }),
            vec!["amounts"]
        );
        assert_eq!(check(&|b, _| b.amount_x = 1e-12), vec!["amounts"]);
        assert_eq!(
            check(&|_, r| r.pair.token_mint_y_program_flag = 1),
            vec!["token_programs"]
        );
        // An open keeper request: not a refusal, but the wSOL account stays open.
        let mut rd = reads(pair.clone());
        rd.keeper_requests = Some(1);
        let (checks, _, main, details) = plan_open(&builder_with_oracle(&pair), &rd, &p);
        assert!(failed(&checks).is_empty());
        assert_eq!(details["unwrap_wsol"], false);
        assert_ne!(main.last().unwrap().data, vec![9]);
    }

    #[test]
    fn open_knobs_are_required_and_bounded() {
        assert!(OpenBuilder::parse(&open_args()).is_ok());
        for key in [
            "bin_count",
            "strategy",
            "max_active_bin_slippage",
            "min_wallet_sol",
            "max_new_bin_arrays",
            "max_divergence_bps",
            "amount_x",
        ] {
            let mut a = open_args();
            a.as_object_mut().unwrap().remove(key);
            assert!(OpenBuilder::parse(&a).is_err(), "{key}");
        }
        for (key, v) in [
            ("bin_count", json!(71)),
            ("bin_count", json!(0)),
            ("max_new_bin_arrays", json!(3)),
            ("strategy", json!("wide")),
            ("max_divergence_bps", json!(0)),
        ] {
            let mut a = open_args();
            a[key] = v;
            assert!(OpenBuilder::parse(&a).is_err(), "{key}");
        }
    }

    #[test]
    fn rent_matches_known_minimums() {
        assert_eq!(rent_exempt(165), 2_039_280, "token account");
        assert_eq!(rent_exempt(POSITION_LEN), 57_406_080);
        assert_eq!(rent_exempt(BIN_ARRAY_LEN), 71_437_440);
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
