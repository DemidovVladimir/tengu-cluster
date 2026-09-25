//! `lp_snapshot` + `hedge_decide` + `lp_decide` — the composed wallet × DLMM
//! pool snapshot and the pure hedge / LP decisions over it. Composition and
//! policy live in `domain::lp::snapshot`; this file reads, caches, persists.
//!
//! | Tool | Reads | Writes |
//! |---|---|---|
//! | `lp_snapshot` | `lp_state` row (pending keeper request) → `plan::discover_positions` → ONE `plan::read_pool` (read 1 = LbPair + positions + perps keys + wallet keys + the request; read 2 pinned = mints, reserves, bin arrays) → the `price_oracle/1:<base mint>` row | `lp_snapshot/1:<wallet>:<pool>` (10 s); the price row when fetched inline |
//! | `hedge_decide` | `lp_snapshot` + `lp_state` rows — no network | `lp_state/1:<wallet>:<pool>` (7 days) only with `commit = true` |
//! | `lp_decide` | `lp_snapshot` + `lp_state` + `price_oracle` rows (5-minute samples) — no network | same |
//!
//! | Case | Rule |
//! |---|---|
//! | Oracle | usable `price_oracle/1:<base mint>` row ≤ 10 s old (`SNAPSHOT_ORACLE_MAX_AGE_MS` = `PRICE_TTL_MS`, and ≤ the caller's max age; `max_age_secs = 0` ⇒ live) so snapshot age + price age stays inside the decide tools' `max_snapshot_age_secs`; else Jupiter lite price v3 inline exactly like `sol_price` without `pool` (no Pyth), put back so `world` sees it; only when the pool's quote is USDC or its base is wSOL |
//! | Wallet keys | wallet + wSOL / USDC Tokenkeg ATAs (+ mints) in read 1; a pair mint outside wSOL / USDC gets its ATA in one extra pinned read |
//! | Pending keeper request | `lp_state.last_hedge_action.position_request` is read in read 1 → `LpSnapshot::set_pending_request` |
//! | `min_context_slot` | pins discovery, both pool reads and the extra ATA read; the cached snapshot is not served; the result is stored (the canonical read-after-write view the decide tools read) |
//! | Explicit `positions` | discovery `found (args)`, no gPA; the cached snapshot is not served and the result is NOT stored (ttl 0), so a caller-chosen subset never answers a discovery-based call; the decide tools refuse an `args` row; keys that are not the wallet's PositionV2 in the pool are `NotApplicable` errors |
//! | Discovery | read 1 pinned to ≥ the discovery's slot; a cached "no positions" answer is reused only within the caller's max age (≤ 10 s); a discovered key that does not value ⇒ exposure `Error` (`dlmm::flag_unvalued`), never 0 |
//! | Cached discovery that went stale | one re-discovery (gPA) + re-read |
//! | Whole-read failure (RPC, pool / mint unreadable) | `Ok` with an `Error` observation (never cached); bad arguments are `Err` |
//! | Decide without a usable snapshot row | `Blocked{stale_input}` (hedge) / `Blocked` (lp); an old row → the domain's staleness gate |
//! | `knobs` | every knob required, unknown knobs rejected; errors name all missing / unknown knobs |
//! | `commit` | default false: nothing written. An unreadable `lp_state` row is never overwritten |

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::Value;
use tracing::warn;

use super::dlmm::{opt_u64_arg, pubkey_arg, pubkeys_arg};
use super::{defs, SolanaShared};
use crate::adapters::outbound::solana::accounts::fetch_accounts;
use crate::adapters::outbound::solana::http_json::fetch_json;
use crate::adapters::outbound::solana::plan::{
    self, failed_observation, read_failure, DiscoverOpts, Discovered, PerpsKeys, PoolRead,
    PoolReadOpts, ReadFailure,
};
use crate::adapters::outbound::solana::rpc::{read_error, SolanaRpc};
use crate::application::observe::observe;
use crate::domain::lp::dlmm::{
    build_dlmm_pool, build_positions, flag_unvalued, Discovery, DiscoverySource,
};
use crate::domain::lp::gates::PriceSample;
use crate::domain::lp::market::{self, OraclePrice, PRICE_TTL_MS};
use crate::domain::lp::perps::{build_perps, request_status};
use crate::domain::lp::snapshot::{
    compose_snapshot, decide_hedge, decide_lp, HedgeDecision, HedgeKnobs, LpControllerState,
    LpDecision, LpKnobs, LpSnapshot, LP_SNAPSHOT_TTL_MS, LP_STATE_TTL_MS,
};
use crate::domain::lp::wallet::{
    build_wallet_balances, build_wallet_inventory, lamports_from_set, wallet_keys,
};
use crate::domain::message::ToolDef;
use crate::domain::observation::{
    now_ms, CachePolicy, Field, ObsSource, ObsStatus, Observation, Observed, ReadError,
};
use crate::domain::solana::{ata, ids, AccountSet, Pubkey};
use crate::domain::tools as names;
use crate::ports::observation::ObservationStore;
use crate::ports::tool::{Tool, ToolCtx, ToolOutput};

pub(crate) fn tools(shared: &SolanaShared) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(LpSnapshotTool {
            def: defs::def(names::LP_SNAPSHOT),
            shared: shared.clone(),
        }),
        Arc::new(HedgeDecideTool {
            def: defs::def(names::HEDGE_DECIDE),
            shared: shared.clone(),
        }),
        Arc::new(LpDecideTool {
            def: defs::def(names::LP_DECIDE),
            shared: shared.clone(),
        }),
    ]
}

fn subject(wallet: &Pubkey, pool: &Pubkey) -> String {
    format!("{wallet}:{pool}")
}

fn state_key(wallet: &Pubkey, pool: &Pubkey) -> String {
    Observation::key_for(LpControllerState::SCHEMA, &subject(wallet, pool))
}

fn snapshot_key(wallet: &Pubkey, pool: &Pubkey) -> String {
    Observation::key_for(LpSnapshot::SCHEMA, &subject(wallet, pool))
}

/// Max age of a `price_oracle/1` row a snapshot reuses (else Jupiter
/// inline); the caller's max age caps it further. A decision reads a
/// snapshot row ≤ `LP_SNAPSHOT_TTL_MS` old, so its price is ≤ 20 s old plus
/// the loop's model latency — inside `max_snapshot_age_secs` (30 in lping),
/// where the old 30 s reuse sat right on it.
const SNAPSHOT_ORACLE_MAX_AGE_MS: u64 = PRICE_TTL_MS;

// ---------------------------------------------------------------------------
// Store rows
// ---------------------------------------------------------------------------

/// `lp_state/1:<wallet>:<pool>` as read from the store.
#[derive(Debug, Clone, PartialEq)]
enum StateRead {
    /// No row (or no store): a fresh `LpControllerState`.
    Missing,
    Found(Box<LpControllerState>),
    /// A row exists but could not be read / decoded: decide on a fresh
    /// state but never overwrite the row.
    Unreadable(String),
}

impl StateRead {
    fn state(&self, wallet: &Pubkey, pool: &Pubkey) -> LpControllerState {
        match self {
            StateRead::Found(s) => (**s).clone(),
            _ => LpControllerState::new(&wallet.to_string(), &pool.to_string()),
        }
    }
}

async fn read_state(
    store: Option<&dyn ObservationStore>,
    wallet: &Pubkey,
    pool: &Pubkey,
) -> StateRead {
    let Some(store) = store else {
        return StateRead::Missing;
    };
    let key = state_key(wallet, pool);
    match store.get(&key).await {
        Ok(None) => StateRead::Missing,
        Ok(Some(row)) => match row.typed::<LpControllerState>() {
            Ok(s) => StateRead::Found(Box::new(s)),
            Err(e) => {
                let error = format!("{e:#}");
                warn!(%key, %error, "lp_state row does not decode");
                StateRead::Unreadable(error)
            }
        },
        Err(e) => {
            let error = format!("{e:#}");
            warn!(%key, %error, "observation store read failed");
            StateRead::Unreadable(error)
        }
    }
}

/// The stored row at `key` at any age; store failures are `None` (warned).
async fn row_at(store: &dyn ObservationStore, key: &str) -> Option<Observation> {
    match store.get(key).await {
        Ok(row) => row,
        Err(e) => {
            let error = format!("{e:#}");
            warn!(key, %error, "observation store read failed");
            None
        }
    }
}

async fn put_row(store: Option<&dyn ObservationStore>, obs: &Observation) -> bool {
    let Some(store) = store else {
        return false;
    };
    match store.put(obs).await {
        Ok(stored) => stored,
        Err(e) => {
            let error = format!("{e:#}");
            warn!(key = %obs.key, %error, "observation store write failed");
            false
        }
    }
}

// ---------------------------------------------------------------------------
// lp_snapshot
// ---------------------------------------------------------------------------

/// Parsed `lp_snapshot` arguments.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SnapshotReq {
    pub wallet: Pubkey,
    pub pool: Pubkey,
    /// Explicit PositionV2 keys (empty ⇒ discovery).
    pub positions: Vec<Pubkey>,
    pub min_context_slot: Option<u64>,
}

impl SnapshotReq {
    pub(crate) fn parse(args: &Value) -> Result<Self> {
        let t = names::LP_SNAPSHOT;
        opt_u64_arg(args, t, "max_age_secs")?;
        Ok(Self {
            wallet: pubkey_arg(args, t, "wallet")?,
            pool: pubkey_arg(args, t, "pool")?,
            positions: pubkeys_arg(args, t, "positions")?,
            min_context_slot: opt_u64_arg(args, t, "min_context_slot")?,
        })
    }
}

struct LpSnapshotTool {
    def: ToolDef,
    shared: SolanaShared,
}

#[async_trait]
impl Tool for LpSnapshotTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        ctx.scope.check_fs_write(ctx.workspace)?;
        let req = SnapshotReq::parse(args)?;
        let now = now_ms();
        let store = self.shared.store.as_deref();
        let obs = match SolanaRpc::from_ctx(ctx) {
            Ok(rpc) => {
                let get_json = |url: String| async move { fetch_json(ctx, &url).await };
                lp_snapshot_obs(&rpc, store, &req, args, get_json, now).await
            }
            Err(e) => snapshot_failed(&req, read_failure("rpc", &e), now),
        };
        Ok(ToolOutput::observed(obs, now))
    }
}

fn snapshot_failed(req: &SnapshotReq, error: ReadError, now_ms: i64) -> Observation {
    failed_observation(
        names::LP_SNAPSHOT,
        LpSnapshot::SCHEMA,
        &subject(&req.wallet, &req.pool),
        format!("lp_snapshot {} {} error", req.wallet, req.pool),
        vec![error],
        now_ms,
    )
}

/// `lp_snapshot` over `rpc` + the cache (see the module table). `get_json`
/// = `fetch_json` in production (the inline oracle read).
pub(crate) async fn lp_snapshot_obs<J, JF>(
    rpc: &SolanaRpc,
    store: Option<&dyn ObservationStore>,
    req: &SnapshotReq,
    args: &Value,
    get_json: J,
    now_ms: i64,
) -> Observation
where
    J: Fn(String) -> JF,
    JF: Future<Output = Result<(u16, Value)>>,
{
    let base = CachePolicy::new(
        LpSnapshot::SCHEMA,
        &subject(&req.wallet, &req.pool),
        LP_SNAPSHOT_TTL_MS,
        args,
    );
    let acct_max_age = base.max_age_ms;
    let explicit = !req.positions.is_empty();
    // A caller-chosen key list or a read-after-write request is never
    // answered from the cache. A read-after-write result is the canonical
    // view (stored); a caller-chosen subset is never stored, so it answers
    // neither a later discovery-based call nor the decide tools.
    let policy = if explicit || req.min_context_slot.is_some() {
        CachePolicy {
            max_age_ms: 0,
            ..base
        }
    } else {
        base
    };
    let ttl = if explicit { 0 } else { LP_SNAPSHOT_TTL_MS };
    let get_json = &get_json;
    let fetched = observe(store, names::LP_SNAPSHOT, &policy, now_ms, || async {
        let snap = build_snapshot(rpc, store, req, acct_max_age, get_json, now_ms).await?;
        Ok((snap, ttl))
    })
    .await;
    fetched.unwrap_or_else(|e| snapshot_failed(req, read_failure("accounts", &e), now_ms))
}

/// `(mint, ATA, token program)` of the wSOL and USDC Tokenkeg accounts every
/// snapshot reads (the hedge's SOL / USDC legs).
fn base_atas(wallet: &Pubkey) -> Vec<(Pubkey, Pubkey, Pubkey)> {
    let token = ids::key(ids::TOKEN);
    [ids::WSOL, ids::USDC]
        .iter()
        .map(|m| {
            let mint = ids::key(m);
            (mint, ata(wallet, &mint, &token), token)
        })
        .collect()
}

/// Read 1 extras: perps keys, wallet keys, the pending request (if any).
fn fixed_extra(wallet: &Pubkey, perps: &PerpsKeys, request: Option<&Pubkey>) -> Vec<Pubkey> {
    let mut extra = perps.keys.clone();
    extra.extend(wallet_keys(wallet, &base_atas(wallet)));
    extra.extend(request.copied());
    extra
}

async fn read_snapshot_accounts(
    rpc: &SolanaRpc,
    store: Option<&dyn ObservationStore>,
    req: &SnapshotReq,
    found: &Discovered,
    fixed: &[Pubkey],
    max_age_ms: u64,
    now_ms: i64,
) -> Result<PoolRead> {
    let mut extra = found.positions.clone();
    extra.extend(fixed.iter().copied());
    let opts = PoolReadOpts {
        extra,
        positions_of: Some(req.wallet),
        min_slot: found.pin(req.min_context_slot),
    };
    plan::read_pool(rpc, store, &req.pool, &opts, max_age_ms, now_ms).await
}

/// The snapshot (see the module table). `Err` = whole-read failure.
async fn build_snapshot<J, JF>(
    rpc: &SolanaRpc,
    store: Option<&dyn ObservationStore>,
    req: &SnapshotReq,
    max_age_ms: u64,
    get_json: &J,
    now_ms: i64,
) -> Result<LpSnapshot>
where
    J: Fn(String) -> JF,
    JF: Future<Output = Result<(u16, Value)>>,
{
    let (wallet, pool) = (&req.wallet, &req.pool);
    let request: Option<Pubkey> = match read_state(store, wallet, pool).await {
        StateRead::Found(s) => s
            .last_hedge_action
            .and_then(|a| a.position_request)
            .and_then(|r| match r.parse::<Pubkey>() {
                Ok(k) => Some(k),
                Err(e) => {
                    warn!(request = %r, error = %e, "lp_state position_request is not a pubkey");
                    None
                }
            }),
        _ => None,
    };
    let perps = plan::perps_keys(wallet);
    let fixed = fixed_extra(wallet, &perps, request.as_ref());

    let mut opts = DiscoverOpts {
        explicit: req.positions.clone(),
        min_slot: req.min_context_slot,
        force: max_age_ms == 0,
        max_age_ms,
    };
    let mut found = plan::discover_positions(rpc, store, wallet, pool, &opts, now_ms).await;
    let mut read =
        read_snapshot_accounts(rpc, store, req, &found, &fixed, max_age_ms, now_ms).await?;
    if found.is_cached() && plan::discovery_stale(&read.set, &found.positions, wallet, pool) {
        opts.force = true;
        found = plan::discover_positions(rpc, store, wallet, pool, &opts, now_ms).await;
        read = read_snapshot_accounts(rpc, store, req, &found, &fixed, max_age_ms, now_ms).await?;
    }
    let mut set = read.set;

    let pool_state = build_dlmm_pool(&set, pool, &read.bin_arrays, now_ms).map_err(ReadFailure)?;
    let mut positions =
        build_positions(&set, wallet, pool, found.discovery).map_err(ReadFailure)?;
    if let Some(source) = found.source {
        flag_unvalued(&mut positions, &found.positions, source);
    }

    // Wallet balances: base, quote, wSOL ATAs under the pair's programs.
    let pair = &pool_state.pair;
    let mut triples: Vec<(Pubkey, Pubkey, Pubkey)> = Vec::new();
    for (mint, program) in [
        (pair.base_mint.as_str(), pair.base_token_program.as_str()),
        (pair.quote_mint.as_str(), pair.quote_token_program.as_str()),
        (ids::WSOL, ids::TOKEN),
    ] {
        let (Ok(m), Ok(p)) = (mint.parse::<Pubkey>(), program.parse::<Pubkey>()) else {
            continue;
        };
        if !triples.iter().any(|(tm, _, _)| *tm == m) {
            triples.push((m, ata(wallet, &m, &p), p));
        }
    }
    let missing: Vec<Pubkey> = triples
        .iter()
        .flat_map(|(m, a, _)| [*a, *m])
        .filter(|k| set.get(k).is_none())
        .collect();
    if !missing.is_empty() {
        let pin = req.min_context_slot.unwrap_or(0).max(set.slot_max);
        let more = fetch_accounts(rpc, store, &missing, max_age_ms, Some(pin), now_ms).await?;
        for r in more.accounts.into_values() {
            set.insert(r);
        }
    }
    let balances = build_wallet_balances(&set, wallet, &triples, &BTreeMap::new());
    let wallet_slot = set.get(wallet).map_or(set.slot_max, |r| r.slot);
    let inventory = build_wallet_inventory(
        *wallet,
        wallet_slot,
        lamports_from_set(&set, wallet),
        Field::Absent,
        Field::Absent,
        balances,
    );

    // Oracle row (base mint), then perps priced with SOL.
    let needed = pair.quote_is_usd || pair.base_mint == ids::WSOL;
    let oracle_max_age = max_age_ms.min(SNAPSHOT_ORACLE_MAX_AGE_MS);
    let oracle = oracle_row(
        store,
        &pair.base_mint,
        needed,
        oracle_max_age,
        get_json,
        now_ms,
    )
    .await;
    let sol_usd = if pair.base_mint == ids::WSOL {
        oracle
            .as_ref()
            .and_then(|o| o.typed::<OraclePrice>().ok())
            .and_then(|p| p.usd)
    } else {
        plan::cached_usd(store, ids::WSOL, oracle_max_age, now_ms).await
    };
    let now_s = now_ms.div_euclid(1000);
    let perps_state = build_perps(&set, wallet, &perps.long, &perps.short, sol_usd, now_s);

    let mut snap = compose_snapshot(
        wallet,
        &pool_state,
        &positions,
        Some(&perps_state),
        &inventory,
        oracle.as_ref(),
        now_ms,
    );
    if let Some(r) = request {
        let max_exec = perps_state.max_request_execution_sec.value().copied();
        snap.set_pending_request(&r, request_status(&set, &r, now_s, max_exec));
    }
    add_to_watch(&mut snap, &set, &triples);
    Ok(snap)
}

/// Pair ATAs read outside `wallet_balances` (a mint outside wSOL / USDC).
fn add_to_watch(snap: &mut LpSnapshot, set: &AccountSet, triples: &[(Pubkey, Pubkey, Pubkey)]) {
    let mut watch: BTreeSet<String> = snap.watch.iter().cloned().collect();
    for (_, a, _) in triples {
        if set.get(a).is_some() {
            watch.insert(a.to_string());
        }
    }
    snap.watch = watch.into_iter().collect();
}

/// `price_oracle/1:<mint>`: a usable row ≤ `max_age_ms` old as is; else
/// (when `fetch`) Jupiter inline, exactly like `sol_price` without `pool`
/// (no Pyth, samples carried from the previous row), put back when usable.
async fn oracle_row<J, JF>(
    store: Option<&dyn ObservationStore>,
    mint: &str,
    fetch: bool,
    max_age_ms: u64,
    get_json: &J,
    now_ms: i64,
) -> Option<Observation>
where
    J: Fn(String) -> JF,
    JF: Future<Output = Result<(u16, Value)>>,
{
    let key = Observation::key_for(OraclePrice::SCHEMA, mint);
    let prev = match store {
        Some(s) => row_at(s, &key).await,
        None => None,
    };
    let prev_price = prev.as_ref().and_then(|r| r.typed::<OraclePrice>().ok());
    if let (Some(row), Some(_)) = (&prev, &prev_price) {
        if row.status.usable() && row.age_ms(now_ms) <= max_age_ms {
            return prev;
        }
    }
    if !fetch {
        return None;
    }
    let jupiter = match get_json(market::jupiter_price_url(mint)).await {
        Ok((_, v)) => market::parse_jupiter_price(&v, mint),
        Err(e) => Field::err(read_error("jupiter", &e)),
    };
    let price = market::combine_price(
        mint,
        jupiter,
        Field::Absent,
        None,
        prev_price.as_ref(),
        now_ms,
    );
    let obs = Observation::of(
        names::SOL_PRICE,
        &price,
        now_ms,
        PRICE_TTL_MS,
        ObsSource::Live,
    );
    if obs.key != key {
        warn!(key = %obs.key, want = %key, "inline oracle row key differs");
    }
    if obs.status != ObsStatus::Error {
        put_row(store, &obs).await;
    }
    Some(obs)
}

// ---------------------------------------------------------------------------
// hedge_decide / lp_decide
// ---------------------------------------------------------------------------

/// Parsed arguments of a decide tool; `knobs` is still raw JSON.
#[derive(Debug, Clone, PartialEq)]
struct DecideArgs {
    wallet: Pubkey,
    pool: Pubkey,
    knobs: Value,
    commit: bool,
}

impl DecideArgs {
    fn parse(args: &Value, tool: &str) -> Result<Self> {
        opt_u64_arg(args, tool, "max_age_secs")?;
        let wallet = pubkey_arg(args, tool, "wallet")?;
        let pool = pubkey_arg(args, tool, "pool")?;
        let commit = match args.get("commit") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(b)) => *b,
            Some(v) => return Err(anyhow!("{tool}: 'commit' must be true or false, got {v}")),
        };
        let knobs = args.get("knobs").cloned().unwrap_or(Value::Null);
        check_knob_names(tool, &knobs)?;
        Ok(Self {
            wallet,
            pool,
            knobs,
            commit,
        })
    }
}

/// Every knob of `tool`'s schema present, none unknown; the error names all
/// missing and unknown knobs (the domain parser reports only the first).
fn check_knob_names(tool: &str, knobs: &Value) -> Result<()> {
    let def = defs::def(tool);
    let props: Vec<String> = def.parameters["properties"]["knobs"]["properties"]
        .as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    let Some(obj) = knobs.as_object() else {
        return Err(anyhow!(
            "{tool}: 'knobs' is required: an object with every knob ({}); there are no defaults",
            props.join(", ")
        ));
    };
    let missing: Vec<&str> = props
        .iter()
        .filter(|k| !obj.contains_key(k.as_str()))
        .map(String::as_str)
        .collect();
    let unknown: Vec<&str> = obj
        .keys()
        .filter(|k| !props.contains(k))
        .map(String::as_str)
        .collect();
    let mut problems = Vec::new();
    if !missing.is_empty() {
        problems.push(format!(
            "knobs missing required field(s): {}",
            missing.join(", ")
        ));
    }
    if !unknown.is_empty() {
        problems.push(format!("unknown knob(s): {}", unknown.join(", ")));
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(anyhow!(
            "{tool}: {} (every knob is required, there are no defaults)",
            problems.join("; ")
        ))
    }
}

/// The `lp_snapshot` row a decision runs on, or why there is none.
async fn snapshot_row(
    store: Option<&dyn ObservationStore>,
    wallet: &Pubkey,
    pool: &Pubkey,
) -> std::result::Result<(Observation, LpSnapshot), String> {
    let key = snapshot_key(wallet, pool);
    let Some(store) = store else {
        return Err(format!(
            "observation store unavailable: no {key} row to decide on"
        ));
    };
    let row = match store.get(&key).await {
        Ok(Some(row)) => row,
        Ok(None) => return Err(format!("no {key} row; call lp_snapshot first")),
        Err(e) => return Err(format!("{key} row unreadable: {e:#}")),
    };
    if !row.status.usable() {
        return Err(format!("{key} row has status {}", row.status.as_str()));
    }
    let snap = match row.typed::<LpSnapshot>() {
        Ok(snap) => snap,
        Err(e) => return Err(format!("{key} row does not decode: {e:#}")),
    };
    // Never stored since explicit snapshots got ttl 0; a row an older
    // binary wrote is still a caller-chosen subset, not the wallet's set.
    if matches!(
        snap.discovery,
        Discovery::Found {
            source: DiscoverySource::Args,
            ..
        }
    ) {
        return Err(format!(
            "{key} row covers caller-chosen positions only (discovery args); call lp_snapshot without positions"
        ));
    }
    Ok((row.served_from_cache(), snap))
}

/// Persist `next` when asked and the stored state was not unreadable.
async fn commit_state(
    store: Option<&dyn ObservationStore>,
    tool: &str,
    commit: bool,
    read: &StateRead,
    next: &LpControllerState,
    now_ms: i64,
) -> Option<String> {
    if !commit {
        return None;
    }
    if let StateRead::Unreadable(e) = read {
        return Some(format!(
            "lp_state NOT committed: the stored row is unreadable ({e})"
        ));
    }
    let obs = Observation::of(tool, next, now_ms, LP_STATE_TTL_MS, ObsSource::Live);
    if put_row(store, &obs).await {
        Some(format!("lp_state committed: {}", obs.key))
    } else {
        Some(format!(
            "lp_state NOT committed: store write failed for {}",
            obs.key
        ))
    }
}

fn decided(obs: Observation, note: Option<String>, now_ms: i64) -> ToolOutput {
    let mut out = ToolOutput::observed(obs, now_ms);
    if let Some(n) = note {
        out.text.push('\n');
        out.text.push_str(&n);
    }
    out
}

/// `hedge_decide` over the store rows: `(decision observation, commit note)`.
pub(crate) async fn hedge_decide_obs(
    store: Option<&dyn ObservationStore>,
    wallet: &Pubkey,
    pool: &Pubkey,
    knobs: &HedgeKnobs,
    commit: bool,
    now_ms: i64,
) -> (Observation, Option<String>) {
    let tool = names::HEDGE_DECIDE;
    let (row, snap) = match snapshot_row(store, wallet, pool).await {
        Ok(v) => v,
        Err(why) => {
            let d = HedgeDecision::without_snapshot(
                &wallet.to_string(),
                &pool.to_string(),
                knobs,
                &why,
            );
            return (Observation::of(tool, &d, now_ms, 0, ObsSource::Live), None);
        }
    };
    let read = read_state(store, wallet, pool).await;
    let state = read.state(wallet, pool);
    let (d, next) = decide_hedge(
        &snap,
        &row.meta(now_ms),
        row.age_ms(now_ms),
        knobs,
        &state,
        now_ms,
    );
    let note = commit_state(store, tool, commit, &read, &next, now_ms).await;
    (Observation::of(tool, &d, now_ms, 0, ObsSource::Live), note)
}

/// `lp_decide` over the store rows (+ the price row's 5-minute samples).
pub(crate) async fn lp_decide_obs(
    store: Option<&dyn ObservationStore>,
    wallet: &Pubkey,
    pool: &Pubkey,
    knobs: &LpKnobs,
    commit: bool,
    now_ms: i64,
) -> (Observation, Option<String>) {
    let tool = names::LP_DECIDE;
    let (row, snap) = match snapshot_row(store, wallet, pool).await {
        Ok(v) => v,
        Err(why) => {
            let d =
                LpDecision::without_snapshot(&wallet.to_string(), &pool.to_string(), knobs, &why);
            return (Observation::of(tool, &d, now_ms, 0, ObsSource::Live), None);
        }
    };
    let samples: Vec<PriceSample> = match store {
        Some(s) => row_at(s, &snap.oracle.key)
            .await
            .and_then(|r| r.typed::<OraclePrice>().ok())
            .map(|p| p.samples)
            .unwrap_or_default(),
        None => Vec::new(),
    };
    let read = read_state(store, wallet, pool).await;
    let state = read.state(wallet, pool);
    let (d, next) = decide_lp(
        &snap,
        &row.meta(now_ms),
        row.age_ms(now_ms),
        knobs,
        &state,
        &samples,
        now_ms,
    );
    let note = commit_state(store, tool, commit, &read, &next, now_ms).await;
    (Observation::of(tool, &d, now_ms, 0, ObsSource::Live), note)
}

struct HedgeDecideTool {
    def: ToolDef,
    shared: SolanaShared,
}

#[async_trait]
impl Tool for HedgeDecideTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        ctx.scope.check_fs_write(ctx.workspace)?;
        let tool = names::HEDGE_DECIDE;
        let a = DecideArgs::parse(args, tool)?;
        let knobs = HedgeKnobs::parse(&a.knobs).map_err(|e| anyhow!("{tool}: {e}"))?;
        let now = now_ms();
        let store = self.shared.store.as_deref();
        let (obs, note) = hedge_decide_obs(store, &a.wallet, &a.pool, &knobs, a.commit, now).await;
        Ok(decided(obs, note, now))
    }
}

struct LpDecideTool {
    def: ToolDef,
    shared: SolanaShared,
}

#[async_trait]
impl Tool for LpDecideTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        ctx.scope.check_fs_write(ctx.workspace)?;
        let tool = names::LP_DECIDE;
        let a = DecideArgs::parse(args, tool)?;
        let knobs = LpKnobs::parse(&a.knobs).map_err(|e| anyhow!("{tool}: {e}"))?;
        let now = now_ms();
        let store = self.shared.store.as_deref();
        let (obs, note) = lp_decide_obs(store, &a.wallet, &a.pool, &knobs, a.commit, now).await;
        Ok(decided(obs, note, now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;

    use crate::adapters::outbound::solana::plan::tests::{
        dlmm_now_ms, dlmm_transport, fixture_reads, gpa_reply, k, methods, owner_positions,
        BOT_WALLET, DLMM_GMA, DLMM_SLOT, JUP_PRICE, OWNER, PERPS_GMA, PERPS_META, POOL,
    };
    use crate::adapters::outbound::solana::rpc::tests::{fake_rpc, FakeTransport};
    use crate::adapters::outbound::solana::rpc::RpcError;
    use crate::adapters::outbound::tools::workspace::test_support::TestHarness;
    use crate::application::observe::tests::MemStore;

    use crate::domain::lp::snapshot::{Guard, HedgeAction, HedgeActionRecord, LpVerdict};
    use crate::domain::observation::{
        assert_features_ok, ErrorClass, MAX_FEATURES, MAX_LINE1_CHARS,
    };
    use crate::domain::solana::AccountRead;

    /// Fixture position (bins -5440..-5371, active -5373), re-owned by
    /// `BOT_WALLET` so the operator wallet has one in-range position.
    const POSITION: &str = "H9fmcxgheDvVSn9iUeRSvZPAgTY5WXqvroNpkZ2HCVRW";
    const ATA_USDC: &str = "D9ScKYy15cw1tpkkuwEnDKv62nCyuETwrvRSdP4usGg1";
    const ATA_WSOL: &str = "E4PCnfEconGJW6vf7GDycEnkFe1VWC7teiNWJzQv3NTA";
    const FLAT_LONG: &str = "FqymRcB92t63jpwh7om4RLbxMNUGoHnZPQMkkAA8ksVY";
    const FLAT_SHORT: &str = "6HFhuYzQGcqdj4NGwC6vfVETRvMA3pXaVeZnHgWSKsJK";
    /// A live PositionRequest (not executed) from `perps/requests_gma.json`.
    const REQUEST: &str = "11q9teW5JiHhWeY8ak79i72C4qpDtppzVgH1ZeEUWp3";
    const SOL_USD: f64 = 116.6589164559586;

    const WALLET_GMA: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/solana/wallet/gma_base64.json"
    ));
    const WALLET_META: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/solana/wallet/meta.json"
    ));
    const REQUESTS_GMA: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/solana/perps/requests_gma.json"
    ));
    const LPING_TOML: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/sandboxes/lping/config.toml"
    ));

    fn keys_of(v: &Value) -> Vec<Pubkey> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|e| k(e.as_str().or_else(|| e["pubkey"].as_str()).unwrap()))
            .collect()
    }

    /// DLMM + perps + wallet + request fixtures behind one fake RPC at the
    /// DLMM slot; `POSITION` re-owned by `BOT_WALLET` when `reown`.
    fn transport(reown: bool) -> Arc<FakeTransport> {
        let t = dlmm_transport();
        let perps: Value = serde_json::from_str(PERPS_META).unwrap();
        for r in fixture_reads(PERPS_GMA, &keys_of(&perps["gma"]["keys"])) {
            t.put_account(r);
        }
        for r in fixture_reads(REQUESTS_GMA, &keys_of(&perps["requests_gma"]["keys"])) {
            t.put_account(r);
        }
        let wallet: Value = serde_json::from_str(WALLET_META).unwrap();
        for r in fixture_reads(
            WALLET_GMA,
            &keys_of(&wallet["files"]["gma_base64.json"]["keys"]),
        ) {
            t.put_account(r);
        }
        if reown {
            let read = t
                .accounts
                .lock()
                .unwrap()
                .get(&k(POSITION))
                .cloned()
                .unwrap();
            let mut data = read.data().unwrap();
            data[40..72].copy_from_slice(&k(BOT_WALLET).0);
            t.put_account(AccountRead::from_bytes(
                k(POSITION),
                read.slot,
                *read.owner().unwrap(),
                read.lamports().unwrap(),
                &data,
            ));
        }
        let _ = DLMM_GMA;
        t
    }

    /// Canned Jupiter price v3 answers (fixture: SOL = `SOL_USD`), counted.
    struct Jup {
        calls: AtomicUsize,
        fail: bool,
    }

    impl Jup {
        fn ok() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                fail: false,
            }
        }
        fn down() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                fail: true,
            }
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
        async fn get(&self, url: String) -> Result<(u16, Value)> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert!(url.contains("lite-api.jup.ag/price/v3"), "{url}");
            assert!(url.contains(ids::WSOL), "{url}");
            if self.fail {
                return Err(RpcError::new(ErrorClass::Timeout, "lite-api.jup.ag timed out").into());
            }
            Ok((200, serde_json::from_str(JUP_PRICE).unwrap()))
        }
    }

    fn req(wallet: &str) -> SnapshotReq {
        SnapshotReq::parse(&json!({"wallet": wallet, "pool": POOL})).unwrap()
    }

    async fn snapshot(
        t: &Arc<FakeTransport>,
        store: &MemStore,
        req: &SnapshotReq,
        args: Value,
        jup: &Jup,
        now: i64,
    ) -> Observation {
        let rpc = fake_rpc(t);
        lp_snapshot_obs(&rpc, Some(store), req, &args, |u| jup.get(u), now).await
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

    fn gma_keys(t: &FakeTransport, i: usize) -> Vec<String> {
        t.gma_params()[i][0]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
    }

    /// `args` of a `hedge_watch` action in `sandboxes/lping/config.toml`.
    fn toml_args(action: &str) -> Value {
        let v: toml::Value = toml::from_str(LPING_TOML).unwrap();
        let args = &v["decision_loops"]["hedge_watch"]["actions"][action]["args"];
        serde_json::to_value(args).unwrap()
    }

    /// Production hedge knobs from the sandbox TOML.
    fn hedge_knobs() -> HedgeKnobs {
        HedgeKnobs::parse(&toml_args("decide_hedge")["knobs"]).unwrap()
    }

    fn lp_knobs() -> LpKnobs {
        LpKnobs::parse(&toml_args("decide_lp")["knobs"]).unwrap()
    }

    // ── lp_snapshot ─────────────────────────────────────────────────

    #[tokio::test]
    async fn snapshot_is_one_planned_read_composed_then_cached() {
        let t = transport(true);
        t.push(Ok(gpa_reply(DLMM_SLOT, &[k(POSITION)])));
        let store = MemStore::default();
        let jup = Jup::ok();
        let now = dlmm_now_ms();
        let r = req(BOT_WALLET);
        let o = snapshot(&t, &store, &r, json!({}), &jup, now).await;
        assert_eq!(o.status, ObsStatus::Ok, "{:?}", o.errors);
        assert_eq!(o.key, format!("lp_snapshot/1:{BOT_WALLET}:{POOL}"));
        assert_eq!((o.source, o.ttl_ms), (ObsSource::Live, 10_000));
        assert_eq!(o.tool, names::LP_SNAPSHOT);
        assert_line1(&o, &[BOT_WALLET, POOL]);

        // Discovery, then read_pool's two GMAs (the second pinned).
        assert_eq!(
            methods(&t),
            [
                "getProgramAccounts",
                "getMultipleAccounts",
                "getMultipleAccounts"
            ]
        );
        let first = gma_keys(&t, 0);
        assert_eq!(first[0], POOL);
        for key in [
            POSITION,
            FLAT_LONG,
            FLAT_SHORT,
            ids::JUP_CUSTODY_SOL,
            ids::JUP_CUSTODY_USDC,
            ids::JLP_POOL,
            BOT_WALLET,
            ATA_WSOL,
            ATA_USDC,
            ids::WSOL,
            ids::USDC,
        ] {
            assert!(first.contains(&key.to_string()), "read 1 lacks {key}");
        }
        assert_eq!(
            t.gma_params()[0][1]["minContextSlot"],
            DLMM_SLOT,
            "read 1 pinned to the gPA slot"
        );
        assert_eq!(t.gma_params()[1][1]["minContextSlot"], DLMM_SLOT);

        // Oracle fetched inline once and stored where `world` reads it.
        assert_eq!(jup.calls(), 1);
        let price = store
            .get(&format!("price_oracle/1:{}", ids::WSOL))
            .await
            .unwrap()
            .expect("price row stored");
        assert_eq!(
            (price.tool.as_str(), price.observed_at_ms),
            (names::SOL_PRICE, now)
        );

        let s: LpSnapshot = o.typed().unwrap();
        assert_eq!((s.wallet.as_str(), s.pool.as_str()), (BOT_WALLET, POOL));
        assert!(matches!(
            s.discovery,
            Discovery::Found {
                count: 1,
                source: DiscoverySource::Gpa,
                ..
            }
        ));
        assert_eq!(s.positions.len(), 1);
        assert_eq!(s.positions[0].position, POSITION);
        assert!(s.hedge.applicable);
        assert_eq!(
            (&s.hedge.long, &s.hedge.short),
            (&Field::Absent, &Field::Absent)
        );
        assert_eq!(s.hedge.max_request_execution_sec.value(), Some(&45));
        assert_eq!(s.wallet_balances.native_sol.value(), Some(&2.748_145_289));
        assert_eq!(s.wallet_balances.quote.value().unwrap().raw, "107808931");
        assert_eq!(s.wallet_balances.wsol.value().unwrap().raw, "0");
        assert_eq!(s.oracle.usd, Some(SOL_USD));
        assert_eq!(s.oracle.age_ms, Some(0));
        assert!(s.pool_vs_oracle_bps.unwrap().abs() < 10.0);
        assert_eq!((s.slot, s.slot_max), (DLMM_SLOT, DLMM_SLOT));
        for key in [POOL, POSITION, BOT_WALLET, ATA_USDC, FLAT_SHORT] {
            assert!(s.watch.contains(&key.to_string()), "watch lacks {key}");
        }
        assert_eq!(s.hedge.pending_request, None);

        // Within 10 s: the stored row, no RPC, no price fetch.
        let n = t.requests().len();
        let again = snapshot(&t, &store, &r, json!({}), &jup, now + 9_000).await;
        assert_eq!(again.source, ObsSource::Cache);
        assert_eq!(again.key, o.key);
        assert_eq!((t.requests().len(), jup.calls()), (n, 1));
    }

    /// Regression (review #10): a price row up to 30 s old was reused, so a
    /// decision on that snapshot seconds later blocked `stale_input` (price
    /// age + snapshot age > max_snapshot_age_secs 30) with a fresh price one
    /// call away.
    #[tokio::test]
    async fn a_price_row_is_reused_only_within_the_snapshot_max_age() {
        let store = MemStore::default();
        let now = dlmm_now_ms();
        let jup = Jup::ok();
        let (w, p) = (k(BOT_WALLET), k(POOL));
        let price_key = format!("price_oracle/1:{}", ids::WSOL);
        // Seed a price row 26 s old.
        let t = transport(true);
        t.push(Ok(gpa_reply(DLMM_SLOT, &[k(POSITION)])));
        snapshot(&t, &store, &req(BOT_WALLET), json!({}), &jup, now - 26_000).await;
        assert_eq!(jup.calls(), 1);
        // 26 s later (discovery row still cached): the price is fetched
        // again (was reused), samples carried.
        let o = snapshot(
            &transport(true),
            &store,
            &req(BOT_WALLET),
            json!({}),
            &jup,
            now,
        )
        .await;
        assert_eq!(o.status, ObsStatus::Ok, "{:?}", o.errors);
        assert_eq!(jup.calls(), 2);
        let s: LpSnapshot = o.typed().unwrap();
        assert_eq!(s.oracle.age_ms, Some(0));
        let row = store.get(&price_key).await.unwrap().unwrap();
        assert_eq!(row.observed_at_ms, now);
        let price: OraclePrice = row.typed().unwrap();
        assert_eq!(price.samples.len(), 2, "{:?}", price.samples);
        // A decision on the snapshot row 9 s later is not stale.
        let (d, _) =
            hedge_decide_obs(Some(&store), &w, &p, &hedge_knobs(), false, now + 9_000).await;
        let d: HedgeDecision = d.typed().unwrap();
        assert_ne!(d.action.guard(), Some(Guard::StaleInput), "{:?}", d.action);
        // A new build within 10 s reuses the row as is ...
        let args = json!({"wallet": BOT_WALLET, "pool": POOL, "positions": [POSITION]});
        let r = SnapshotReq::parse(&args).unwrap();
        let o = snapshot(&transport(true), &store, &r, args, &jup, now + 9_000).await;
        let s: LpSnapshot = o.typed().unwrap();
        assert_eq!((jup.calls(), s.oracle.age_ms), (2, Some(9_000)));
        // ... unless the caller's max age is tighter (0 = live).
        let t = transport(true);
        t.push(Ok(gpa_reply(DLMM_SLOT, &[k(POSITION)])));
        let args = json!({"max_age_secs": 0});
        let o = snapshot(&t, &store, &req(BOT_WALLET), args, &jup, now + 9_000).await;
        let s: LpSnapshot = o.typed().unwrap();
        assert_eq!((jup.calls(), s.oracle.age_ms), (3, Some(0)));
    }

    /// The oracle bound keeps a decision on a cached snapshot inside the
    /// production staleness budget with room for the loop's model calls.
    #[test]
    fn snapshot_age_plus_oracle_reuse_fits_the_decision_budget() {
        let worst = LP_SNAPSHOT_TTL_MS + SNAPSHOT_ORACLE_MAX_AGE_MS;
        for secs in [
            hedge_knobs().max_snapshot_age_secs,
            lp_knobs().max_snapshot_age_secs,
        ] {
            assert!(worst + 5_000 <= secs * 1000, "{worst} ms vs {secs} s");
        }
    }

    #[tokio::test]
    async fn a_failed_inline_price_is_a_partial_snapshot_never_a_zero() {
        let t = transport(false);
        t.push(Ok(gpa_reply(DLMM_SLOT, &[])));
        let store = MemStore::default();
        let jup = Jup::down();
        let now = dlmm_now_ms();
        let o = snapshot(&t, &store, &req(BOT_WALLET), json!({}), &jup, now).await;
        assert_eq!(o.status, ObsStatus::Partial);
        let s: LpSnapshot = o.typed().unwrap();
        assert_eq!(s.oracle.usd, None);
        let e = s.oracle.error.as_ref().unwrap();
        assert_eq!((e.field.as_str(), e.class), ("oracle", ErrorClass::Timeout));
        assert!(!o.features.contains_key("oracle_usd"));
        assert!(
            store
                .get(&format!("price_oracle/1:{}", ids::WSOL))
                .await
                .unwrap()
                .is_none(),
            "an Error price row is never stored"
        );
        // The hedge refuses it.
        let (w, p) = (k(BOT_WALLET), k(POOL));
        let (d, _) = hedge_decide_obs(Some(&store), &w, &p, &hedge_knobs(), false, now).await;
        let d: HedgeDecision = d.typed().unwrap();
        assert_eq!(d.action.guard(), Some(Guard::InvalidRead), "{:?}", d.action);
        assert!(d.trace.invalid_fields.contains(&"oracle.usd".to_string()));
    }

    #[tokio::test]
    async fn the_pending_keeper_request_is_read_in_the_same_gma() {
        let t = transport(false);
        t.push(Ok(gpa_reply(DLMM_SLOT, &[])));
        let store = MemStore::default();
        let now = dlmm_now_ms();
        let mut st = LpControllerState::new(BOT_WALLET, POOL);
        st.last_hedge_action = Some(HedgeActionRecord {
            at_ms: now - 10_000,
            action: "increase_short".into(),
            live: true,
            signatures: Vec::new(),
            position_request: Some(REQUEST.into()),
            counter: Some(473_577_047),
        });
        let row = Observation::of("hedge_decide", &st, now, LP_STATE_TTL_MS, ObsSource::Live);
        store.put(&row).await.unwrap();
        let o = snapshot(&t, &store, &req(BOT_WALLET), json!({}), &Jup::ok(), now).await;
        assert!(
            gma_keys(&t, 0).contains(&REQUEST.to_string()),
            "request in read 1"
        );
        assert_eq!(t.gma_params().len(), 2, "no extra read for the request");
        let s: LpSnapshot = o.typed().unwrap();
        let pending = s.hedge.pending_request.as_ref().unwrap().value().unwrap();
        assert_eq!(pending.position_request, REQUEST);
        assert!(pending.exists && !pending.executed, "{pending:?}");
        assert!(s.watch.contains(&REQUEST.to_string()));
        assert_eq!(o.features["pending_request"], json!(true));
    }

    #[tokio::test]
    async fn read_after_write_and_explicit_positions_bypass_the_cached_row() {
        let store = MemStore::default();
        let now = dlmm_now_ms();
        let jup = Jup::ok();
        let t = transport(true);
        t.push(Ok(gpa_reply(DLMM_SLOT, &[k(POSITION)])));
        snapshot(&t, &store, &req(BOT_WALLET), json!({}), &jup, now).await;

        // min_context_slot above the cached rows: live, every read pinned
        // (rows at or above it would be reused), the row stored.
        let t = transport(true);
        t.slot.store(DLMM_SLOT + 5, Ordering::SeqCst);
        t.push(Ok(gpa_reply(DLMM_SLOT + 5, &[k(POSITION)])));
        let min = DLMM_SLOT + 1;
        let args = json!({"wallet": BOT_WALLET, "pool": POOL, "min_context_slot": min});
        let r = SnapshotReq::parse(&args).unwrap();
        let o = snapshot(&t, &store, &r, args, &jup, now + 1_000).await;
        assert_eq!(o.source, ObsSource::Live);
        assert_eq!(
            methods(&t),
            [
                "getProgramAccounts",
                "getMultipleAccounts",
                "getMultipleAccounts"
            ],
            "cached discovery and account rows bypassed"
        );
        // Read 1 pinned to max(min_context_slot, the gPA's slot).
        assert_eq!(t.gma_params()[0][1]["minContextSlot"], DLMM_SLOT + 5);
        assert_eq!(t.gma_params()[1][1]["minContextSlot"], DLMM_SLOT + 5);
        assert_eq!(o.slot, Some(DLMM_SLOT + 5));
        assert_eq!(
            store.get(&o.key).await.unwrap().unwrap().observed_at_ms,
            now + 1_000,
            "stored for the decide tools"
        );

        // Explicit positions: no gPA, found (args); a foreign key is flagged;
        // never stored (the canonical row above is kept).
        let t = transport(true);
        t.slot.store(DLMM_SLOT + 5, Ordering::SeqCst);
        let foreign = owner_positions()[0].to_string();
        let args = json!({"wallet": BOT_WALLET, "pool": POOL, "positions": [POSITION, foreign]});
        let r = SnapshotReq::parse(&args).unwrap();
        let o = snapshot(&t, &store, &r, args, &jup, now + 2_000).await;
        assert_eq!((o.source, o.ttl_ms), (ObsSource::Live, 0));
        assert_eq!(
            store.get(&o.key).await.unwrap().unwrap().observed_at_ms,
            now + 1_000,
            "the explicit subset is not stored"
        );
        assert!(!methods(&t).contains(&"getProgramAccounts".to_string()));
        let s: LpSnapshot = o.typed().unwrap();
        assert!(matches!(
            s.discovery,
            Discovery::Found {
                source: DiscoverySource::Args,
                ..
            }
        ));
        assert_eq!(s.positions.len(), 1);
        assert_eq!(o.status, ObsStatus::Partial);
        assert!(
            s.errors.iter().any(|e| e.message.contains(&foreign)),
            "{:?}",
            s.errors
        );
    }

    /// Regression (review #4/#9/#13): an explicit-positions snapshot (a
    /// subset, or a foreign key ⇒ exposure 0) was stored under the
    /// canonical key and answered the loop's snapshot step and the decide
    /// tools for 10 s.
    #[tokio::test]
    async fn an_explicit_positions_snapshot_never_answers_discovery_or_decisions() {
        let store = MemStore::default();
        let now = dlmm_now_ms();
        let jup = Jup::ok();
        let (w, p) = (k(BOT_WALLET), k(POOL));
        let foreign = owner_positions()[0].to_string();
        let args = json!({"wallet": BOT_WALLET, "pool": POOL, "positions": [foreign]});
        let r = SnapshotReq::parse(&args).unwrap();
        let o = snapshot(&transport(true), &store, &r, args, &jup, now).await;
        let s: LpSnapshot = o.typed().unwrap();
        assert!(s.positions.is_empty());
        assert_eq!(o.ttl_ms, 0);
        assert!(store.get(&o.key).await.unwrap().is_none(), "never stored");
        // The decide tools find no row to decide on.
        let (d, _) = hedge_decide_obs(Some(&store), &w, &p, &hedge_knobs(), false, now).await;
        let d: HedgeDecision = d.typed().unwrap();
        assert!(d.view.is_none(), "{:?}", d.action);
        // The loop's snapshot step (no positions) within 10 s: live gPA.
        let t = transport(true);
        t.push(Ok(gpa_reply(DLMM_SLOT, &[k(POSITION)])));
        let o = snapshot(&t, &store, &req(BOT_WALLET), json!({}), &jup, now + 1_000).await;
        assert_eq!(o.source, ObsSource::Live);
        assert_eq!(methods(&t)[0], "getProgramAccounts");
        // A subset row an older binary stored is refused by both decide tools.
        let mut legacy: LpSnapshot = o.typed().unwrap();
        legacy.discovery = Discovery::Found {
            count: 1,
            source: DiscoverySource::Args,
            at_ms: now + 1_000,
        };
        let row = Observation::of(
            names::LP_SNAPSHOT,
            &legacy,
            now + 1_000,
            LP_SNAPSHOT_TTL_MS,
            ObsSource::Live,
        );
        store.put(&row).await.unwrap();
        let (d, _) =
            hedge_decide_obs(Some(&store), &w, &p, &hedge_knobs(), true, now + 2_000).await;
        let d: HedgeDecision = d.typed().unwrap();
        assert!(
            matches!(&d.action, HedgeAction::Blocked { reason, .. } if reason.contains("caller-chosen")),
            "{:?}",
            d.action
        );
        let (d, note) = lp_decide_obs(Some(&store), &w, &p, &lp_knobs(), true, now + 2_000).await;
        let d: LpDecision = d.typed().unwrap();
        assert!(
            matches!(&d.verdict, LpVerdict::Blocked { reason } if reason.contains("caller-chosen")),
            "{:?}",
            d.verdict
        );
        assert_eq!(note, None);
        assert!(store.get(&state_key(&w, &p)).await.unwrap().is_none());
    }

    /// Regression (review #1/#2/#5): a cached "no positions" discovery was
    /// reused for 300 s, so a position another process opened read as LP 0.
    #[tokio::test]
    async fn a_position_opened_after_an_empty_discovery_shows_up_next_snapshot() {
        let store = MemStore::default();
        let now = dlmm_now_ms();
        let jup = Jup::ok();
        let t = transport(true);
        t.push(Ok(gpa_reply(DLMM_SLOT, &[])));
        let s: LpSnapshot = snapshot(&t, &store, &req(BOT_WALLET), json!({}), &jup, now)
            .await
            .typed()
            .unwrap();
        assert!(matches!(s.discovery, Discovery::Empty { .. }));
        // The snapshot row expired (10 s): the empty answer is as old, so
        // discovery runs again and sees the new position.
        let t = transport(true);
        t.push(Ok(gpa_reply(DLMM_SLOT, &[k(POSITION)])));
        let o = snapshot(&t, &store, &req(BOT_WALLET), json!({}), &jup, now + 11_000).await;
        assert_eq!(methods(&t)[0], "getProgramAccounts");
        let s: LpSnapshot = o.typed().unwrap();
        assert_eq!(s.positions.len(), 1, "{:?}", s.discovery);
        assert!(s.exposure.value().unwrap().base > 0.0);
    }

    #[tokio::test]
    async fn the_fixture_owner_has_three_positions() {
        let t = transport(false);
        t.push(Ok(gpa_reply(DLMM_SLOT, &owner_positions())));
        let store = MemStore::default();
        let now = dlmm_now_ms();
        let o = snapshot(&t, &store, &req(OWNER), json!({}), &Jup::ok(), now).await;
        let s: LpSnapshot = o.typed().unwrap();
        assert_eq!(s.positions.len(), 3);
        assert!(s.hedge.applicable);
        assert_line1(&o, &[OWNER, POOL]);
        let (w, p) = (k(OWNER), k(POOL));
        let (d, _) = lp_decide_obs(Some(&store), &w, &p, &lp_knobs(), false, now).await;
        let d: LpDecision = d.typed().unwrap();
        assert!(
            matches!(d.verdict, LpVerdict::Paused { .. }),
            "{:?}",
            d.verdict
        );
    }

    #[tokio::test]
    async fn a_whole_read_failure_is_an_error_row_never_stored() {
        // Pool absent on chain.
        let t = Arc::new(FakeTransport::at_slot(DLMM_SLOT));
        t.push(Ok(gpa_reply(DLMM_SLOT, &[])));
        let store = MemStore::default();
        let o = snapshot(&t, &store, &req(BOT_WALLET), json!({}), &Jup::ok(), 5).await;
        assert_eq!((o.status, o.ttl_ms), (ObsStatus::Error, 0));
        assert_line1(&o, &[BOT_WALLET, POOL]);
        assert_eq!(o.errors[0].class, ErrorClass::NotApplicable);
        assert!(store.get(&o.key).await.unwrap().is_none());
        // RPC quota exhausted (fresh store: nothing cached).
        let t = Arc::new(FakeTransport::at_slot(DLMM_SLOT));
        t.push(Ok(gpa_reply(DLMM_SLOT, &[])));
        t.push(Err(RpcError::new(
            ErrorClass::QuotaExhausted,
            "max usage reached",
        )));
        let store = MemStore::default();
        let o = snapshot(&t, &store, &req(BOT_WALLET), json!({}), &Jup::ok(), 5).await;
        assert_eq!(methods(&t), ["getProgramAccounts", "getMultipleAccounts"]);
        assert_eq!(o.status, ObsStatus::Error);
        assert_eq!(o.errors[0].class, ErrorClass::QuotaExhausted);
    }

    // ── hedge_decide / lp_decide ────────────────────────────────────

    async fn seeded(store: &MemStore, now: i64) -> LpSnapshot {
        let t = transport(true);
        t.push(Ok(gpa_reply(DLMM_SLOT, &[k(POSITION)])));
        snapshot(&t, store, &req(BOT_WALLET), json!({}), &Jup::ok(), now)
            .await
            .typed()
            .unwrap()
    }

    #[tokio::test]
    async fn decisions_without_a_snapshot_row_block_as_stale() {
        let store = MemStore::default();
        let (w, p) = (k(BOT_WALLET), k(POOL));
        let (o, note) = hedge_decide_obs(Some(&store), &w, &p, &hedge_knobs(), true, 1).await;
        let d: HedgeDecision = o.typed().unwrap();
        assert_eq!(d.action.guard(), Some(Guard::StaleInput));
        assert!(
            matches!(&d.action, HedgeAction::Blocked { reason, .. } if reason.contains("call lp_snapshot first")),
            "{:?}",
            d.action
        );
        assert_eq!(note, None, "nothing to commit");
        assert!(store.get(&state_key(&w, &p)).await.unwrap().is_none());
        let (o, _) = lp_decide_obs(None, &w, &p, &lp_knobs(), true, 1).await;
        let d: LpDecision = o.typed().unwrap();
        assert!(
            matches!(&d.verdict, LpVerdict::Blocked { reason } if reason.contains("observation store unavailable")),
            "{:?}",
            d.verdict
        );
        assert_line1(&o, &[BOT_WALLET, POOL]);
    }

    #[tokio::test]
    async fn hedge_decide_reads_the_rows_and_commits_only_on_request() {
        let store = MemStore::default();
        let now = dlmm_now_ms();
        let snap = seeded(&store, now).await;
        let (w, p) = (k(BOT_WALLET), k(POOL));
        let knobs = hedge_knobs();
        let at = now + 2_000;

        let (o, note) = hedge_decide_obs(Some(&store), &w, &p, &knobs, false, at).await;
        assert_eq!(note, None);
        assert!(
            store.get(&state_key(&w, &p)).await.unwrap().is_none(),
            "commit=false writes nothing"
        );
        assert_eq!(o.key, format!("hedge_decide/1:{BOT_WALLET}:{POOL}"));
        assert_eq!((o.ttl_ms, o.status), (0, ObsStatus::Ok));
        assert!(
            store.get(&o.key).await.unwrap().is_none(),
            "decisions are never cached"
        );
        assert_line1(&o, &[BOT_WALLET, POOL]);
        let d: HedgeDecision = o.typed().unwrap();
        // Exactly the domain decision on the stored row.
        let row = store.get(&snapshot_key(&w, &p)).await.unwrap().unwrap();
        let fresh = LpControllerState::new(BOT_WALLET, POOL);
        let (want, next) = decide_hedge(
            &snap,
            &row.clone().served_from_cache().meta(at),
            2_000,
            &knobs,
            &fresh,
            at,
        );
        assert_eq!(d, want);
        assert_eq!(d.snapshot.as_ref().unwrap().source, ObsSource::Cache);
        assert_eq!(d.snapshot_age_ms, Some(2_000));
        assert_ne!(d.action.guard(), Some(Guard::InvalidRead), "{:?}", d.action);

        let (_, note) = hedge_decide_obs(Some(&store), &w, &p, &knobs, true, at).await;
        assert_eq!(
            note,
            Some(format!("lp_state committed: {}", state_key(&w, &p)))
        );
        let row = store.get(&state_key(&w, &p)).await.unwrap().unwrap();
        assert_eq!(
            (row.ttl_ms, row.tool.as_str()),
            (LP_STATE_TTL_MS, names::HEDGE_DECIDE)
        );
        let st: LpControllerState = row.typed().unwrap();
        assert_eq!(st, next);
        assert_eq!(st.last_position_seen_ms, Some(now), "the snapshot's time");

        // An old snapshot row → the domain staleness gate.
        let (o, _) = hedge_decide_obs(Some(&store), &w, &p, &knobs, false, now + 31_000).await;
        let d: HedgeDecision = o.typed().unwrap();
        assert_eq!(d.action.guard(), Some(Guard::StaleInput), "{:?}", d.action);
    }

    #[tokio::test]
    async fn an_unreadable_state_row_is_never_overwritten() {
        let store = MemStore::default();
        let now = dlmm_now_ms();
        seeded(&store, now).await;
        let (w, p) = (k(BOT_WALLET), k(POOL));
        let mut bad = Observation::of(
            "hedge_decide",
            &LpControllerState::new(BOT_WALLET, POOL),
            now,
            LP_STATE_TTL_MS,
            ObsSource::Live,
        );
        bad.data = json!({"committed_regime": 7});
        store.put(&bad).await.unwrap();
        let (_, note) = hedge_decide_obs(Some(&store), &w, &p, &hedge_knobs(), true, now).await;
        assert!(note.unwrap().starts_with("lp_state NOT committed"));
        assert_eq!(store.get(&state_key(&w, &p)).await.unwrap().unwrap(), bad);
    }

    #[tokio::test]
    async fn lp_decide_runs_the_storm_gate_on_the_price_row_samples() {
        let store = MemStore::default();
        let now = dlmm_now_ms();
        // A price ring 5 min long: 110 → 116.66 (+6 %) — a storm at 2 %.
        let jup = |usd: f64| {
            Field::ok(market::JupiterPrice {
                usd,
                block_id: None,
                change_24h_pct: None,
                liquidity_usd: None,
            })
        };
        let old = market::combine_price(
            ids::WSOL,
            jup(110.0),
            Field::Absent,
            None,
            None,
            now - 300_000,
        );
        let fresh = market::combine_price(
            ids::WSOL,
            jup(SOL_USD),
            Field::Absent,
            None,
            Some(&old),
            now - 1_000,
        );
        assert_eq!(fresh.samples.len(), 2);
        let row = Observation::of(
            "sol_price",
            &fresh,
            now - 1_000,
            PRICE_TTL_MS,
            ObsSource::Live,
        );
        store.put(&row).await.unwrap();
        let snap = seeded(&store, now).await;
        assert_eq!(snap.oracle.age_ms, Some(1_000), "the stored row was used");
        let (w, p) = (k(BOT_WALLET), k(POOL));

        let mut knobs = lp_knobs();
        knobs.trend_confirm_ms = 0;
        let (o, _) = lp_decide_obs(Some(&store), &w, &p, &knobs, false, now).await;
        let d: LpDecision = o.typed().unwrap();
        assert!(d.storm.active, "{:?}", d.storm);
        assert!(d.storm.move_5m_pct.unwrap() > 5.0);
        assert!(
            matches!(d.verdict, LpVerdict::Paused { .. }),
            "{:?}",
            d.verdict
        );
        // Storm off → the imbalanced fixture position recenters.
        knobs.storm_pct_5m = 0.0;
        let (o, note) = lp_decide_obs(Some(&store), &w, &p, &knobs, true, now).await;
        let d: LpDecision = o.typed().unwrap();
        assert_eq!(d.verdict.name(), "recenter", "{:?}", d.verdict);
        assert!(note.unwrap().starts_with("lp_state committed"));
        let st: LpControllerState = store
            .get(&state_key(&w, &p))
            .await
            .unwrap()
            .unwrap()
            .typed()
            .unwrap();
        assert_eq!(st.known_positions, vec![POSITION.to_string()]);
        assert_line1(&o, &[BOT_WALLET, POOL]);
    }

    // ── arguments ───────────────────────────────────────────────────

    #[test]
    fn knob_errors_name_every_missing_and_unknown_knob() {
        let full = toml_args("decide_hedge");
        assert!(DecideArgs::parse(&full, names::HEDGE_DECIDE).is_ok());
        let mut v = full.clone();
        let knobs = v["knobs"].as_object_mut().unwrap();
        knobs.remove("no_lp_grace_ms");
        knobs.remove("cap_mult");
        knobs.insert("band_binz".into(), json!(4));
        let e = DecideArgs::parse(&v, names::HEDGE_DECIDE)
            .unwrap_err()
            .to_string();
        assert!(
            e.starts_with("hedge_decide: knobs missing required field(s): "),
            "{e}"
        );
        for name in [
            "no_lp_grace_ms",
            "cap_mult",
            "unknown knob(s): band_binz",
            "no defaults",
        ] {
            assert!(e.contains(name), "{name} not in {e}");
        }
        let mut v = full.clone();
        v.as_object_mut().unwrap().remove("knobs");
        let e = DecideArgs::parse(&v, names::HEDGE_DECIDE)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("'knobs' is required") && e.contains("trend_confirm_ms"),
            "{e}"
        );
        let mut v = full.clone();
        v["commit"] = json!("yes");
        assert!(DecideArgs::parse(&v, names::HEDGE_DECIDE)
            .unwrap_err()
            .to_string()
            .contains("'commit' must be true or false"));
        let mut v = full.clone();
        v["knobs"]["lp_input"] = json!("median");
        let a = DecideArgs::parse(&v, names::HEDGE_DECIDE).unwrap();
        assert!(HedgeKnobs::parse(&a.knobs).is_err());
        // LP knobs: a hedge-only knob is unknown there.
        let mut v = toml_args("decide_lp");
        v["knobs"]["no_lp_grace_ms"] = json!(1);
        let e = DecideArgs::parse(&v, names::LP_DECIDE)
            .unwrap_err()
            .to_string();
        assert!(e.contains("unknown knob(s): no_lp_grace_ms"), "{e}");
    }

    #[test]
    fn lping_toml_carries_complete_production_knobs() {
        let h = hedge_knobs();
        assert_eq!(h.lp_input, crate::domain::lp::snapshot::LpInput::Midpoint);
        assert!(h.include_wallet_sol);
        assert_eq!((h.bin_count, h.band_bins), (28, 8));
        assert_eq!(
            (h.delta_threshold_sol, h.target_collateral_ratio),
            (0.25, 0.33)
        );
        assert_eq!((h.trend_confirm_ms, h.no_lp_grace_ms), (600_000, 300_000));
        let l = lp_knobs();
        assert_eq!(
            (l.imbalance_threshold, l.bin_count, l.storm_pct_5m),
            (0.92, 28, 2.0)
        );
        assert_eq!(
            (l.reentry_confirm_ms, l.reentry_tol_frac),
            (7_200_000, 0.20)
        );
        assert_eq!(l.trend_confirm_ms, h.trend_confirm_ms);
        for action in ["decide_hedge", "decide_lp"] {
            let a = toml_args(action);
            assert_eq!(a["commit"], json!(true), "{action}");
            assert_eq!(a["wallet"], json!(BOT_WALLET));
            assert_eq!(a["pool"], json!(POOL));
        }
    }

    #[tokio::test]
    async fn decide_tools_run_through_execute_with_a_scope_check() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = Arc::new(MemStore::default());
        seeded(&store, now_ms()).await;
        let shared = SolanaShared {
            store: Some(store.clone() as Arc<dyn ObservationStore>),
        };
        let lp_tools = tools(&shared);
        let by_name = |n: &str| {
            lp_tools
                .iter()
                .find(|t| t.definition().name == n)
                .unwrap()
                .clone()
        };
        let h = TestHarness::new(tmp.path());
        let hedge = by_name(names::HEDGE_DECIDE);
        let out = hedge
            .execute(&toml_args("decide_hedge"), &h.ctx())
            .await
            .unwrap();
        let o = out.observation.clone().unwrap();
        assert_eq!(o.key, format!("hedge_decide/1:{BOT_WALLET}:{POOL}"));
        assert!(
            out.text.ends_with(&format!(
                "lp_state committed: lp_state/1:{BOT_WALLET}:{POOL}"
            )),
            "{}",
            out.text
        );
        let out = by_name(names::LP_DECIDE)
            .execute(&toml_args("decide_lp"), &h.ctx())
            .await
            .unwrap();
        assert!(out.observation.unwrap().key.starts_with("lp_decide/1:"));
        // No fs root → denied before any read.
        let denied = TestHarness::with_scope(tmp.path(), Default::default());
        assert!(hedge
            .execute(&toml_args("decide_hedge"), &denied.ctx())
            .await
            .is_err());
        let snap = by_name(names::LP_SNAPSHOT);
        assert!(snap
            .execute(&json!({"wallet": BOT_WALLET, "pool": POOL}), &denied.ctx())
            .await
            .is_err());
        // Bad arguments are errors, not observations.
        let e = snap
            .execute(&json!({"wallet": "nope", "pool": POOL}), &h.ctx())
            .await
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("'wallet' is not a valid Solana address") && e.ends_with("nope"),
            "{e}"
        );
    }

    // ── live (public mainnet; `cargo test --bin tengu live_lp_ -- --ignored --test-threads 1`) ──

    use super::super::price::tests::Live;

    fn live_tool(live: &Live, name: &str) -> Arc<dyn Tool> {
        tools(&live.shared)
            .into_iter()
            .find(|t| t.definition().name == name)
            .unwrap()
    }

    /// `lp_snapshot` twice (the second from the store); the first result.
    async fn live_snapshot(live: &Live, wallet: &str) -> (Observation, LpSnapshot) {
        let tool = live_tool(live, names::LP_SNAPSHOT);
        let args = json!({"wallet": wallet, "pool": POOL});
        let out = tool.execute(&args, &live.harness.ctx()).await.unwrap();
        eprintln!(
            "{}",
            out.text.lines().take(3).collect::<Vec<_>>().join("\n")
        );
        let o = out.observation.unwrap();
        assert!(o.status.usable(), "{}", out.text);
        assert_line1(&o, &[wallet, POOL]);
        let again = tool.execute(&args, &live.harness.ctx()).await.unwrap();
        assert_eq!(again.observation.unwrap().source, ObsSource::Cache);
        let s = o.typed().unwrap();
        (o, s)
    }

    /// A decide tool with the production knobs of the TOML (commit false).
    async fn live_decide(live: &Live, action: &str, tool: &str, wallet: &str) -> Observation {
        let mut args = toml_args(action);
        args["wallet"] = json!(wallet);
        args["commit"] = json!(false);
        let out = live_tool(live, tool)
            .execute(&args, &live.harness.ctx())
            .await
            .unwrap();
        eprintln!(
            "{}",
            out.text.lines().take(2).collect::<Vec<_>>().join("\n")
        );
        out.observation.unwrap()
    }

    #[tokio::test]
    #[ignore]
    async fn live_lp_snapshot_operator_wallet() {
        let live = Live::new();
        let (o, s) = live_snapshot(&live, BOT_WALLET).await;
        assert_eq!(o.status, ObsStatus::Ok, "{:?}", o.errors);
        // No LP and no perps on chain today (bot HANDOVER Session 42).
        assert!(
            matches!(s.discovery, Discovery::Empty { .. }),
            "{:?}",
            s.discovery
        );
        assert!(s.positions.is_empty(), "{:?}", s.positions);
        assert!(s.hedge.applicable);
        assert_eq!(
            (&s.hedge.long, &s.hedge.short),
            (&Field::Absent, &Field::Absent),
            "perps flat"
        );
        assert!(s.oracle.usd.unwrap() > 1.0, "{:?}", s.oracle);
        assert!(s.wallet_balances.native_sol.value().is_some());
        let store = live.shared.store.as_deref().unwrap();
        let price = store
            .get(&format!("price_oracle/1:{}", ids::WSOL))
            .await
            .unwrap();
        assert!(price.is_some(), "the inline oracle row is stored");

        let h = live_decide(&live, "decide_hedge", names::HEDGE_DECIDE, BOT_WALLET).await;
        let d: HedgeDecision = h.typed().unwrap();
        assert!(d.snapshot.is_some());
        assert_ne!(d.action.guard(), Some(Guard::InvalidRead), "{:?}", d.action);
        let l = live_decide(&live, "decide_lp", names::LP_DECIDE, BOT_WALLET).await;
        let d: LpDecision = l.typed().unwrap();
        assert!(d.snapshot.is_some());
        assert!(d.invalid_fields.is_empty(), "{:?}", d.invalid_fields);
    }

    #[tokio::test]
    #[ignore]
    async fn live_lp_snapshot_fixture_owner() {
        let live = Live::new();
        let (_, s) = live_snapshot(&live, OWNER).await;
        eprintln!(
            "positions={} anomalies={:?}",
            s.positions.len(),
            s.anomalies
        );
        // 3 positions at the fixture capture (slot 450102095).
        assert!(!s.positions.is_empty(), "{:?}", s.discovery);
        assert!(s.hedge.applicable);
        let h = live_decide(&live, "decide_hedge", names::HEDGE_DECIDE, OWNER).await;
        let d: HedgeDecision = h.typed().unwrap();
        assert!(
            [
                "none",
                "blocked",
                "increase_long",
                "increase_short",
                "decrease_long",
                "decrease_short"
            ]
            .contains(&d.action.name()),
            "{:?}",
            d.action
        );
        assert_ne!(d.action.guard(), Some(Guard::InvalidRead), "{:?}", d.action);
        assert!(d.view.is_some(), "the controller saw the snapshot");
        let l = live_decide(&live, "decide_lp", names::LP_DECIDE, OWNER).await;
        let d: LpDecision = l.typed().unwrap();
        assert_eq!(d.health.len(), s.positions.len());
        eprintln!("lp verdict {}", d.verdict.name());
    }
}
