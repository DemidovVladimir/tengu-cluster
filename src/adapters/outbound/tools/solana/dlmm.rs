//! `dlmm_pool` + `dlmm_positions` — Meteora DLMM pool and position state
//! from Solana RPC account reads (key planning: `outbound::solana::plan`).
//!
//! | Tool | Reads | Cache row |
//! |---|---|---|
//! | `dlmm_pool` | `plan::read_pool` (LbPair; then mints + reserves + bin arrays active ± 50) → `dlmm::build_dlmm_pool` | `dlmm_pool/1:<pool>`, 5 s |
//! | `dlmm_positions` | `plan::discover_positions` → `plan::read_pool` (+ the positions, + their bin arrays) → `dlmm::build_positions` | `dlmm_positions/1:<wallet>:<pool>`, 10 s |
//!
//! | Case | Rule |
//! |---|---|
//! | Explicit `positions` (non-empty; `[]` = discover) | discovery `found (args)`, no gPA; the cached row is not served and the result is not stored (ttl 0), so a caller-chosen subset never answers a later discovery-based call; requested keys that are not the wallet's PositionV2 in the pool are `NotApplicable` errors (Partial) |
//! | `min_context_slot` | pins every account read; the cached row is not served; a discovery row below it is bypassed |
//! | `max_age_secs = 0` | live typed row, live `acct/1` rows, live discovery |
//! | Discovery | the account reads are pinned to ≥ the discovery's slot (`Discovered::pin`); a cached "no positions" row is reused only within the caller's max age; a discovered key that does not value ⇒ exposure `Error` (`dlmm::flag_unvalued`), status `Error`, never 0 |
//! | Cached discovery that went stale | a listed key no longer reads as the wallet's position ⇒ one re-discovery (gPA) + re-read |
//! | Whole-read failure (RPC error, pool or mint unreadable) | `Ok` with an `Error` observation (never cached), not `Err`; bad arguments are `Err` |

use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::Value;

use super::{defs, SolanaShared};
use crate::adapters::outbound::solana::plan::{
    self, failed_observation, read_failure, DiscoverOpts, Discovered, PoolRead, PoolReadOpts,
    ReadFailure,
};
use crate::adapters::outbound::solana::rpc::SolanaRpc;
use crate::adapters::outbound::tools::args::{opt_u64_arg, require_str};
use crate::application::observe::observe;
use crate::domain::lp::dlmm::{
    build_dlmm_pool, build_positions, flag_unvalued, DlmmPoolState, DlmmPositions,
};
use crate::domain::message::ToolDef;
use crate::domain::observation::{now_ms, CachePolicy, Observation, Observed, ReadError};
use crate::domain::solana::Pubkey;
use crate::domain::tools as names;
use crate::ports::observation::ObservationStore;
use crate::ports::tool::{Tool, ToolCtx, ToolOutput};

/// `dlmm_pool/1` TTL.
pub(crate) const DLMM_POOL_TTL_MS: u64 = 5_000;
/// `dlmm_positions/1` TTL.
pub(crate) const DLMM_POSITIONS_TTL_MS: u64 = 10_000;

pub(crate) fn tools(shared: &SolanaShared) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(DlmmPoolTool::new(shared.clone())),
        Arc::new(DlmmPositionsTool::new(shared.clone())),
    ]
}

// ── argument helpers (shared with perps.rs) ─────────────────────

/// A required base58 address argument; errors name the tool and the arg.
pub(crate) fn pubkey_arg(args: &Value, tool: &str, key: &str) -> Result<Pubkey> {
    let s = require_str(args, tool, key)?;
    s.trim()
        .parse()
        .map_err(|e| anyhow!("{tool}: '{key}' is not a valid Solana address ({e}): {s}"))
}

/// An optional array of base58 addresses (absent / null ⇒ empty).
pub(crate) fn pubkeys_arg(args: &Value, tool: &str, key: &str) -> Result<Vec<Pubkey>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                let s = v
                    .as_str()
                    .ok_or_else(|| anyhow!("{tool}: '{key}' must be an array of strings"))?;
                s.trim().parse().map_err(|e| {
                    anyhow!("{tool}: '{key}' entry is not a valid Solana address ({e}): {s}")
                })
            })
            .collect(),
        Some(_) => Err(anyhow!("{tool}: '{key}' must be an array of strings")),
    }
}

// ── dlmm_pool ───────────────────────────────────────────────────

pub(crate) struct DlmmPoolTool {
    def: ToolDef,
    shared: SolanaShared,
}

impl DlmmPoolTool {
    pub(crate) fn new(shared: SolanaShared) -> Self {
        Self {
            def: defs::def(names::DLMM_POOL),
            shared,
        }
    }
}

#[async_trait]
impl Tool for DlmmPoolTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        ctx.scope.check_fs_write(ctx.workspace)?;
        let pool = pubkey_arg(args, names::DLMM_POOL, "pool")?;
        opt_u64_arg(args, names::DLMM_POOL, "max_age_secs")?;
        let now = now_ms();
        let store = self.shared.store.as_deref();
        let obs = match SolanaRpc::from_ctx(ctx) {
            Ok(rpc) => dlmm_pool_obs(&rpc, store, &pool, args, now).await,
            Err(e) => pool_failed(&pool, read_failure("rpc", &e), now),
        };
        Ok(ToolOutput::observed(obs, now))
    }
}

fn pool_failed(pool: &Pubkey, error: ReadError, now_ms: i64) -> Observation {
    failed_observation(
        names::DLMM_POOL,
        DlmmPoolState::SCHEMA,
        &pool.to_string(),
        format!("dlmm_pool {pool} error"),
        vec![error],
        now_ms,
    )
}

/// `dlmm_pool` over `rpc` + the cache (see the module table).
pub(crate) async fn dlmm_pool_obs(
    rpc: &SolanaRpc,
    store: Option<&dyn ObservationStore>,
    pool: &Pubkey,
    args: &Value,
    now_ms: i64,
) -> Observation {
    let policy = CachePolicy::new(
        DlmmPoolState::SCHEMA,
        &pool.to_string(),
        DLMM_POOL_TTL_MS,
        args,
    );
    let fetched = observe(store, names::DLMM_POOL, &policy, now_ms, || async {
        let read = plan::read_pool(
            rpc,
            store,
            pool,
            &PoolReadOpts::default(),
            policy.max_age_ms,
            now_ms,
        )
        .await?;
        let state =
            build_dlmm_pool(&read.set, pool, &read.bin_arrays, now_ms).map_err(ReadFailure)?;
        Ok((state, DLMM_POOL_TTL_MS))
    })
    .await;
    fetched.unwrap_or_else(|e| pool_failed(pool, read_failure("accounts", &e), now_ms))
}

// ── dlmm_positions ──────────────────────────────────────────────

pub(crate) struct DlmmPositionsTool {
    def: ToolDef,
    shared: SolanaShared,
}

impl DlmmPositionsTool {
    pub(crate) fn new(shared: SolanaShared) -> Self {
        Self {
            def: defs::def(names::DLMM_POSITIONS),
            shared,
        }
    }
}

/// Parsed `dlmm_positions` arguments.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PositionsReq {
    pub wallet: Pubkey,
    pub pool: Pubkey,
    /// Explicit PositionV2 keys (empty ⇒ discovery).
    pub positions: Vec<Pubkey>,
    pub min_context_slot: Option<u64>,
}

impl PositionsReq {
    pub(crate) fn parse(args: &Value) -> Result<Self> {
        let t = names::DLMM_POSITIONS;
        opt_u64_arg(args, t, "max_age_secs")?;
        Ok(Self {
            wallet: pubkey_arg(args, t, "wallet")?,
            pool: pubkey_arg(args, t, "pool")?,
            positions: pubkeys_arg(args, t, "positions")?,
            min_context_slot: opt_u64_arg(args, t, "min_context_slot")?,
        })
    }

    fn subject(&self) -> String {
        format!("{}:{}", self.wallet, self.pool)
    }
}

#[async_trait]
impl Tool for DlmmPositionsTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        ctx.scope.check_fs_write(ctx.workspace)?;
        let req = PositionsReq::parse(args)?;
        let now = now_ms();
        let store = self.shared.store.as_deref();
        let obs = match SolanaRpc::from_ctx(ctx) {
            Ok(rpc) => dlmm_positions_obs(&rpc, store, &req, args, now).await,
            Err(e) => positions_failed(&req, read_failure("rpc", &e), now),
        };
        Ok(ToolOutput::observed(obs, now))
    }
}

fn positions_failed(req: &PositionsReq, error: ReadError, now_ms: i64) -> Observation {
    failed_observation(
        names::DLMM_POSITIONS,
        DlmmPositions::SCHEMA,
        &req.subject(),
        format!("dlmm_positions {} {} error", req.wallet, req.pool),
        vec![error],
        now_ms,
    )
}

async fn read_positions(
    rpc: &SolanaRpc,
    store: Option<&dyn ObservationStore>,
    req: &PositionsReq,
    found: &Discovered,
    max_age_ms: u64,
    now_ms: i64,
) -> Result<PoolRead> {
    let opts = PoolReadOpts {
        extra: found.positions.clone(),
        positions_of: Some(req.wallet),
        min_slot: found.pin(req.min_context_slot),
    };
    plan::read_pool(rpc, store, &req.pool, &opts, max_age_ms, now_ms).await
}

/// `dlmm_positions` over `rpc` + the cache (see the module table).
pub(crate) async fn dlmm_positions_obs(
    rpc: &SolanaRpc,
    store: Option<&dyn ObservationStore>,
    req: &PositionsReq,
    args: &Value,
    now_ms: i64,
) -> Observation {
    let base = CachePolicy::new(
        DlmmPositions::SCHEMA,
        &req.subject(),
        DLMM_POSITIONS_TTL_MS,
        args,
    );
    let acct_max_age = base.max_age_ms;
    let explicit = !req.positions.is_empty();
    // A cached row answers neither a caller-chosen key list nor a
    // read-after-write request; an explicit list is never stored.
    let policy = if explicit || req.min_context_slot.is_some() {
        CachePolicy {
            max_age_ms: 0,
            ..base
        }
    } else {
        base
    };
    let ttl = if explicit { 0 } else { DLMM_POSITIONS_TTL_MS };
    let fetched = observe(store, names::DLMM_POSITIONS, &policy, now_ms, || async {
        let mut opts = DiscoverOpts {
            explicit: req.positions.clone(),
            min_slot: req.min_context_slot,
            force: acct_max_age == 0,
            max_age_ms: acct_max_age,
        };
        let mut found =
            plan::discover_positions(rpc, store, &req.wallet, &req.pool, &opts, now_ms).await;
        let mut read = read_positions(rpc, store, req, &found, acct_max_age, now_ms).await?;
        if found.is_cached()
            && plan::discovery_stale(&read.set, &found.positions, &req.wallet, &req.pool)
        {
            opts.force = true;
            found =
                plan::discover_positions(rpc, store, &req.wallet, &req.pool, &opts, now_ms).await;
            read = read_positions(rpc, store, req, &found, acct_max_age, now_ms).await?;
        }
        let mut out = build_positions(&read.set, &req.wallet, &req.pool, found.discovery)
            .map_err(ReadFailure)?;
        if let Some(source) = found.source {
            flag_unvalued(&mut out, &found.positions, source);
        }
        Ok((out, ttl))
    })
    .await;
    fetched.unwrap_or_else(|e| positions_failed(req, read_failure("accounts", &e), now_ms))
}

#[cfg(test)]
#[path = "dlmm_tests.rs"]
mod tests;
