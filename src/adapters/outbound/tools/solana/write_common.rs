//! Shared machinery of the Solana write tools (phase 6b): the `mode`
//! argument, the per-agent signer gate and the simulate / send runner
//! around a tool's [`WriteBuilder`]. Pipeline: `outbound/solana/send.rs`.
//!
//! | Mode | Needs | Does |
//! |---|---|---|
//! | `simulate` (default) | nothing | live reads → checks → keyless simulation of every transaction |
//! | `send` | the tool's scope lists the wallet (`wallets = ["<pubkey>"]`), `[solana] signer_key_file` holds that wallet's key, the write store is up | lease + earlier send resolved → live reads (at or after the wallet's fence) → checks → sign + send + confirm each transaction → drop stale cache rows |
//!
//! A failed check refuses the write (nothing simulated or sent). Reads in
//! a write tool are always live — never the observation cache.
//!
//! Open Jupiter perps keeper requests ([`live_keeper_requests`], review
//! H5): while one is unexecuted its keeper may still pay into the wallet's
//! wSOL / USDC account, so DLMM writes leave the wSOL account open and
//! perps orders and SOL-leg swaps refuse.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::Value;

use super::SolanaShared;
use crate::adapters::outbound::solana::rpc::SolanaRpc;
use crate::adapters::outbound::solana::send::{Pipeline, TxPlan};
use crate::adapters::outbound::solana::signer::load_key_file;
use crate::domain::lp::perps::{
    decode_position_request, JupPositionRequest, POSITION_REQUEST_DISC,
};
use crate::domain::lp::snapshot::{LpControllerState, LP_STATE_TTL_MS};
use crate::domain::observation::{now_ms, ObsSource, Observation, Observed};
use crate::domain::scope::ToolScope;
use crate::domain::solana::{bs58_encode, ids, Pubkey};
use crate::domain::solana_write::{Check, WriteMode, WriteResult, WriteStatus};
use crate::ports::observation::ObservationStore;
use crate::ports::solana_signer::SolanaSigner;
use crate::ports::tool::{ToolCtx, ToolOutput};

/// An unexecuted keeper request older than this is treated as dead: the
/// keeper fills within the pool's `maxRequestExecutionSec` (45 s live) or
/// closes it. Younger ones block (conservative).
pub(crate) const KEEPER_REQUEST_LIVE_SECS: i64 = 300;

/// The wallet's unexecuted Jupiter perps keeper requests opened in the
/// last [`KEEPER_REQUEST_LIVE_SECS`], read live (gPA disc@0 + owner@8, then
/// the accounts). `Err` ⇒ the caller refuses (unknown is not "none").
pub(crate) async fn live_keeper_requests(
    rpc: &SolanaRpc,
    wallet: &Pubkey,
    now_s: i64,
) -> Result<Vec<(Pubkey, JupPositionRequest)>> {
    let filters = [
        serde_json::json!({"memcmp": {"offset": 0, "bytes": bs58_encode(&POSITION_REQUEST_DISC)}}),
        serde_json::json!({"memcmp": {"offset": 8, "bytes": wallet.to_string()}}),
    ];
    let (slot, keys) = rpc
        .get_program_account_keys(&ids::key(ids::JUP_PERPS), &filters)
        .await?;
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    let (_, reads) = rpc.get_multiple_accounts(&keys, Some(slot)).await?;
    let mut open = Vec::new();
    for read in reads {
        let Some(data) = read.data() else {
            continue; // closed since the gPA — nothing pending
        };
        let req = decode_position_request(&data)
            .map_err(|e| anyhow!("keeper request {}: {e}", read.pubkey))?;
        if !req.executed && now_s.saturating_sub(req.open_time) < KEEPER_REQUEST_LIVE_SECS {
            open.push((read.pubkey, req));
        }
    }
    Ok(open)
}

/// Apply a write tool's own change to `lp_state/1:<wallet>:<pool>` after a
/// landed write (review M1): re-read, `apply` to that version (or a fresh
/// state), compare-and-swap; retried up to 3 times on a conflict, so a
/// decide tool's concurrent commit is merged, never lost. An unreadable row
/// is never overwritten. Returns a note for the result.
pub(crate) async fn merge_lp_state(
    store: Option<&Arc<dyn ObservationStore>>,
    tool: &str,
    wallet: &Pubkey,
    pool: &Pubkey,
    apply: impl Fn(&mut LpControllerState),
) -> String {
    let Some(store) = store else {
        return "lp_state NOT updated: no observation store".into();
    };
    let key = Observation::key_for(LpControllerState::SCHEMA, &format!("{wallet}:{pool}"));
    for _ in 0..3 {
        let (mut state, expected) = match store.get(&key).await {
            Ok(None) => (
                LpControllerState::new(&wallet.to_string(), &pool.to_string()),
                None,
            ),
            Ok(Some(row)) => match row.typed::<LpControllerState>() {
                Ok(s) => (s, Some(row.observed_at_ms)),
                Err(e) => return format!("lp_state NOT updated: row unreadable ({e:#})"),
            },
            Err(e) => return format!("lp_state NOT updated: row unreadable ({e:#})"),
        };
        apply(&mut state);
        let now = now_ms();
        let at = expected.map_or(now, |v| now.max(v.saturating_add(1)));
        state.updated_at_ms = at;
        let obs = Observation::of(tool, &state, at, LP_STATE_TTL_MS, ObsSource::Live);
        match store.put_if_unchanged(&obs, expected).await {
            Ok(true) => return format!("lp_state updated: {key}"),
            Ok(false) => continue,
            Err(e) => return format!("lp_state NOT updated: store write failed ({e:#})"),
        }
    }
    format!("lp_state NOT updated: {key} kept changing (3 conflicts)")
}

/// What a write tool plans from fresh reads.
#[derive(Default)]
pub(crate) struct Built {
    pub checks: Vec<Check>,
    pub plans: Vec<TxPlan>,
    /// Tool payload for the result (`WriteResult::details`).
    pub details: Value,
    /// Cache rows a send makes stale (dropped after any send attempt).
    pub stale_keys: Vec<String>,
    /// Keep going after a transaction fails (independent batches, e.g.
    /// closing token accounts). Default: stop at the first failure.
    pub independent: bool,
}

#[async_trait]
pub(crate) trait WriteBuilder: Send + Sync {
    /// Live reads → checks + transactions. `fence` = the wallet's last
    /// landed write slot: reads that support it must be at or after it.
    async fn build(&self, rpc: &SolanaRpc, fence: Option<u64>) -> Result<Built>;

    /// After a send where at least one transaction landed (`confirmed` /
    /// `partial`): controller-state bookkeeping. Returns a note for
    /// `details.lp_state`.
    async fn after_send(&self, _result: &WriteResult, _shared: &SolanaShared) -> Option<String> {
        None
    }
}

/// The `mode` argument (`simulate` when absent).
pub(crate) fn parse_mode(args: &Value, tool: &str) -> Result<WriteMode> {
    let raw = match args.get("mode") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.as_str()),
        Some(v) => return Err(anyhow!("{tool}: 'mode' must be a string, got {v}")),
    };
    WriteMode::parse(raw).map_err(|e| anyhow!("{tool}: {e}"))
}

/// The signer for `wallet`, or the refusal reason. Only an agent whose
/// scope for `tool` lists the full wallet address may sign
/// (`check_wallet` has no wildcard; the permissive fallback never grants
/// an address).
pub(crate) fn signer_for(
    scope: &ToolScope,
    key_file: Option<&PathBuf>,
    wallet: &Pubkey,
    tool: &str,
) -> std::result::Result<Arc<dyn SolanaSigner>, String> {
    scope.check_wallet(&wallet.to_string()).map_err(|_| {
        format!(
            "signer_not_allowed: this agent's scope for {tool} does not list wallet {wallet} \
             (wallets = [\"{wallet}\"] on [agents.<name>.scopes.{tool}])"
        )
    })?;
    let path = key_file
        .ok_or_else(|| "no_signer: [solana] signer_key_file is not configured".to_string())?;
    let key =
        load_key_file(path).map_err(|e| format!("signer_unavailable: {}: {e}", path.display()))?;
    if key.pubkey() != *wallet {
        return Err(format!(
            "signer_mismatch: the key file holds {}, not {wallet}",
            key.pubkey()
        ));
    }
    Ok(Arc::new(key))
}

fn refuse_on_failed_check(r: WriteResult) -> WriteResult {
    match r
        .failed_check()
        .map(|c| format!("{}: {}", c.name, c.detail))
    {
        Some(reason) => r.refuse(reason),
        None => r,
    }
}

/// Run one write tool call in `mode`; always returns a `write/1`
/// observation (refusals included). `Err` only for a bad RPC URL.
pub(crate) async fn run_write(
    ctx: &ToolCtx<'_>,
    shared: &SolanaShared,
    tool: &str,
    wallet: Pubkey,
    mode: WriteMode,
    builder: &dyn WriteBuilder,
) -> Result<ToolOutput> {
    let rpc = Arc::new(SolanaRpc::from_ctx(ctx)?);
    let r = run_write_with(rpc, ctx.scope, shared, tool, wallet, mode, builder).await;
    let now = now_ms();
    Ok(ToolOutput::observed(
        Observation::of(tool, &r, now, 0, ObsSource::Live),
        now,
    ))
}

/// [`run_write`] over an explicit RPC client and scope (tests inject both).
pub(crate) async fn run_write_with(
    rpc: Arc<SolanaRpc>,
    scope: &ToolScope,
    shared: &SolanaShared,
    tool: &str,
    wallet: Pubkey,
    mode: WriteMode,
    builder: &dyn WriteBuilder,
) -> WriteResult {
    let r = WriteResult::new(tool, &wallet.to_string(), mode, now_ms(), rpc.host());
    let pipeline = Pipeline::new(rpc.clone(), tool, wallet);
    match mode {
        WriteMode::Simulate => simulate(&pipeline, &rpc, shared, builder, r).await,
        WriteMode::Send => send(scope, &pipeline, &rpc, shared, builder, r).await,
    }
}

async fn fence(shared: &SolanaShared, wallet: &str) -> Option<u64> {
    shared.writes.as_ref()?.fence(wallet).await.ok().flatten()
}

async fn simulate(
    pipeline: &Pipeline,
    rpc: &SolanaRpc,
    shared: &SolanaShared,
    builder: &dyn WriteBuilder,
    mut r: WriteResult,
) -> WriteResult {
    let built = match builder.build(rpc, fence(shared, &r.wallet).await).await {
        Ok(b) => b,
        Err(e) => return r.refuse(format!("read_failed: {e:#}")),
    };
    r.checks = built.checks;
    r.details = built.details;
    r = refuse_on_failed_check(r);
    if r.refused.is_some() {
        return r;
    }
    for plan in &built.plans {
        let t = pipeline.simulate(plan).await;
        let ok = t.status() == WriteStatus::Simulated;
        r.txs.push(t);
        if !ok && !built.independent {
            break;
        }
    }
    r.settle();
    r
}

async fn send(
    scope: &ToolScope,
    pipeline: &Pipeline,
    rpc: &SolanaRpc,
    shared: &SolanaShared,
    builder: &dyn WriteBuilder,
    mut r: WriteResult,
) -> WriteResult {
    let signer = match signer_for(
        scope,
        shared.signer_key_file.as_ref(),
        &pipeline.wallet,
        &r.tool,
    ) {
        Ok(s) => s,
        Err(reason) => return r.refuse(reason),
    };
    let Some(store) = shared.writes.clone() else {
        return r.refuse(
            "write_store_unavailable: <TENGU_HOME>/state/solana-writes.db could not be opened",
        );
    };
    let mut session = match pipeline.open_session(store.clone(), signer).await {
        Ok(s) => s,
        Err(reason) => return r.refuse(reason),
    };
    r.pending_resolved = session.pending_resolved.clone();
    let fence = store.fence(&r.wallet).await.ok().flatten();
    let mut stale = Vec::new();
    match builder.build(rpc, fence).await {
        Err(e) => r = r.refuse(format!("read_failed: {e:#}")),
        Ok(built) => {
            r.checks = built.checks;
            r.details = built.details;
            r = refuse_on_failed_check(r);
            if r.refused.is_none() {
                stale = built.stale_keys;
                for plan in &built.plans {
                    let t = session.send(plan).await;
                    let ok = t.status() == WriteStatus::Confirmed;
                    r.txs.push(t);
                    if !ok && !built.independent {
                        break;
                    }
                }
                r.settle();
            }
        }
    }
    let sent = r.txs.iter().any(|t| t.signature.is_some());
    session
        .close(shared.store.as_ref(), if sent { &stale } else { &[] })
        .await;
    if matches!(r.status, WriteStatus::Confirmed | WriteStatus::Partial) {
        if let Some(note) = builder.after_send(&r, shared).await {
            if let Some(d) = r.details.as_object_mut() {
                d.insert("lp_state".into(), Value::String(note));
            }
        }
    }
    r
}
