//! `lp_snapshot` + `lp_swap_plan` + `hedge_decide` + `lp_decide` — the
//! composed wallet × DLMM pool snapshot and pure swap / hedge / LP plans over
//! it. Composition and policy live in `domain::lp`; this file exposes tools,
//! reads, caches and persists.
//!
//! | Tool | Reads | Writes |
//! |---|---|---|
//! | `lp_snapshot` | `lp_state` row (pending keeper request) → `plan::discover_positions` → ONE `plan::read_pool` (read 1 = LbPair + positions + perps keys + wallet keys + the request; read 2 pinned = mints, reserves, bin arrays) → the `price_oracle/1:<base mint>` row | `lp_snapshot/1:<wallet>:<pool>` (10 s); the price row when fetched inline |
//! | `lp_swap_plan` | arguments only | nothing; returns zero or one deterministic route to merge into `jupiter_swap` args |
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
//! | Unreadable `lp_state` (store error, row that does not parse / decode) | `Blocked{invalid_read}` (hedge) / `Paused{invalid_read}` (lp), `invalid_fields = ["lp_state"]`; never overwritten |
//! | `commit` | default false: nothing written. True: compare-and-swap on the row's version as read (`put_if_unchanged`); a row changed since the read ⇒ the decision stands, NOT committed (conflict). `features.commit` = committed \| conflict \| unreadable \| failed |

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde::Deserialize;
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
use crate::domain::lp::gates::{
    plan_swap_for_deposit, DepositSwapContext, DepositSwapInput, PriceSample,
};
use crate::domain::lp::market::{self, OraclePrice, PRICE_TTL_MS};
use crate::domain::lp::perps::{build_perps, request_status};
use crate::domain::lp::snapshot::{
    compose_snapshot, decide_hedge_in_cycle, decide_lp, Guard, HedgeAction, HedgeDecision,
    HedgeKnobs, LpControllerState, LpCycle, LpDecision, LpKnobs, LpSnapshot, LpVerdict,
    PauseReason, LP_SNAPSHOT_TTL_MS, LP_STATE_TTL_MS,
};
use crate::domain::lp::wallet::{
    build_wallet_balances, build_wallet_inventory, lamports_from_set, wallet_keys,
};
use crate::domain::message::ToolDef;
use crate::domain::observation::{
    now_ms, set_str, CachePolicy, Field, ObsSource, ObsStatus, Observation, Observed, ReadError,
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
        Arc::new(LpSwapPlanTool {
            def: defs::def(names::LP_SWAP_PLAN),
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
    /// The row and its version (`observed_at_ms`) for the conditional commit.
    Found {
        state: Box<LpControllerState>,
        observed_at_ms: i64,
    },
    /// The store failed or the row does not decode: the decide tools block
    /// (`invalid_read`) and never overwrite it.
    Unreadable(String),
}

impl StateRead {
    /// The state to decide on; `Err(why)` when the row is unreadable.
    fn state(
        &self,
        wallet: &Pubkey,
        pool: &Pubkey,
    ) -> std::result::Result<LpControllerState, &str> {
        match self {
            StateRead::Found { state, .. } => Ok((**state).clone()),
            StateRead::Missing => Ok(LpControllerState::new(
                &wallet.to_string(),
                &pool.to_string(),
            )),
            StateRead::Unreadable(e) => Err(e),
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
            Ok(s) => StateRead::Found {
                state: Box::new(s),
                observed_at_ms: row.observed_at_ms,
            },
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
        let mut req = SnapshotReq::parse(args)?;
        let now = now_ms();
        let store = self.shared.store.as_deref();
        // Read-after-write: a cached row older than the wallet's last
        // landed write is bypassed and every read pinned to that slot.
        if let Some(f) = self.shared.fence(&req.wallet).await {
            let cached = match store {
                Some(s) => s
                    .get(&snapshot_key(&req.wallet, &req.pool))
                    .await
                    .ok()
                    .flatten()
                    .and_then(|r| r.slot),
                None => None,
            };
            if cached.is_none_or(|slot| slot < f) {
                req.min_context_slot = Some(req.min_context_slot.unwrap_or(0).max(f));
            }
        }
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
        StateRead::Found { state: s, .. } => s
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
    /// `hedge_decide` only: optional `lp_decide` knobs (storm / freeze).
    lp_knobs: Option<Value>,
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
        let lp_knobs = match args.get("lp_knobs") {
            None | Some(Value::Null) => None,
            Some(v) => {
                check_knob_names(names::LP_DECIDE, v)
                    .map_err(|e| anyhow!("{tool}: lp_knobs: {e}"))?;
                Some(v.clone())
            }
        };
        Ok(Self {
            wallet,
            pool,
            knobs,
            lp_knobs,
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
    fence: Option<u64>,
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
    // A write of this wallet landed after the row was read: it describes
    // the chain before that write (write fence, `outbound/solana/send.rs`).
    if let Some(f) = fence {
        if row.slot.is_none_or(|s| s < f) {
            return Err(format!(
                "{key} row (slot {}) predates the wallet's last write (slot {f}); call lp_snapshot again",
                row.slot.map_or("none".to_string(), |s| s.to_string())
            ));
        }
    }
    Ok((row.served_from_cache(), snap))
}

/// Outcome of a `commit = true` decision: `(features.commit, note)`.
type CommitNote = (&'static str, String);

fn unreadable_note(commit: bool, why: &str) -> Option<CommitNote> {
    commit.then(|| {
        (
            "unreadable",
            format!("lp_state NOT committed: the stored row is unreadable ({why})"),
        )
    })
}

/// Persist `next` when asked: a compare-and-swap on the row's version as
/// read (`None` = no row), so a concurrent writer's update is never lost —
/// the loser keeps its decision but reports NOT committed (conflict). An
/// unreadable row is never overwritten.
async fn commit_state(
    store: Option<&dyn ObservationStore>,
    tool: &str,
    commit: bool,
    read: &StateRead,
    next: &LpControllerState,
    now_ms: i64,
) -> Option<CommitNote> {
    if !commit {
        return None;
    }
    let expected = match read {
        StateRead::Unreadable(e) => return unreadable_note(commit, e),
        StateRead::Missing => None,
        StateRead::Found { observed_at_ms, .. } => Some(*observed_at_ms),
    };
    // Strictly newer than the version read, so every commit moves it.
    let at = expected.map_or(now_ms, |v| now_ms.max(v.saturating_add(1)));
    let obs = Observation::of(tool, next, at, LP_STATE_TTL_MS, ObsSource::Live);
    let failed = || {
        (
            "failed",
            format!("lp_state NOT committed: store write failed for {}", obs.key),
        )
    };
    let Some(s) = store else {
        return Some(failed());
    };
    match s.put_if_unchanged(&obs, expected).await {
        Ok(true) => Some(("committed", format!("lp_state committed: {}", obs.key))),
        Ok(false) => Some((
            "conflict",
            format!(
                "lp_state NOT committed (conflict): {} changed since it was read; the decision stands, re-run to decide on the new state",
                obs.key
            ),
        )),
        Err(e) => {
            let error = format!("{e:#}");
            warn!(key = %obs.key, %error, "observation store write failed");
            Some(failed())
        }
    }
}

/// The decision observation (`features.commit` = committed | conflict |
/// unreadable | failed when a commit was asked) and the commit note.
fn with_commit(mut obs: Observation, commit: Option<CommitNote>) -> (Observation, Option<String>) {
    match commit {
        Some((outcome, note)) => {
            set_str(&mut obs.features, "commit", Some(outcome));
            (obs, Some(note))
        }
        None => (obs, None),
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

fn unreadable_reason(why: &str) -> String {
    format!(
        "lp_state row unreadable ({why}): regime, timers, cooldown and the pending request are unknown"
    )
}

/// The price row's 5-minute samples for the snapshot's oracle key (empty when
/// there is no store or row).
async fn price_samples(
    store: Option<&dyn ObservationStore>,
    snap: &LpSnapshot,
) -> Vec<PriceSample> {
    match store {
        Some(s) => row_at(s, &snap.oracle.key)
            .await
            .and_then(|r| r.typed::<OraclePrice>().ok())
            .map(|p| p.samples)
            .unwrap_or_default(),
        None => Vec::new(),
    }
}

/// `hedge_decide` over the store rows: `(decision observation, commit note)`.
/// An unreadable `lp_state` row blocks (`invalid_read`).
#[cfg(test)]
pub(crate) async fn hedge_decide_obs(
    store: Option<&dyn ObservationStore>,
    wallet: &Pubkey,
    pool: &Pubkey,
    knobs: &HedgeKnobs,
    commit: bool,
    now_ms: i64,
) -> (Observation, Option<String>) {
    hedge_decide_obs_in_cycle(store, wallet, pool, knobs, None, commit, now_ms, None).await
}

/// `hedge_decide` with optional `lp_decide` knobs: when given, the storm latch
/// and imbalance freeze come from this snapshot + price samples
/// (`decide_hedge_in_cycle`), not from an earlier `lp_decide` commit.
pub(crate) async fn hedge_decide_obs_in_cycle(
    store: Option<&dyn ObservationStore>,
    wallet: &Pubkey,
    pool: &Pubkey,
    knobs: &HedgeKnobs,
    lp_knobs: Option<&LpKnobs>,
    commit: bool,
    now_ms: i64,
    fence: Option<u64>,
) -> (Observation, Option<String>) {
    let tool = names::HEDGE_DECIDE;
    let (w, p) = (wallet.to_string(), pool.to_string());
    let (row, snap) = match snapshot_row(store, wallet, pool, fence).await {
        Ok(v) => v,
        Err(why) => {
            let d = HedgeDecision::without_snapshot(&w, &p, knobs, &why);
            return (Observation::of(tool, &d, now_ms, 0, ObsSource::Live), None);
        }
    };
    let (meta, age) = (row.meta(now_ms), row.age_ms(now_ms));
    let read = read_state(store, wallet, pool).await;
    let state = match read.state(wallet, pool) {
        Ok(state) => state,
        Err(why) => {
            let reason = unreadable_reason(why);
            let mut d = HedgeDecision::without_snapshot(&w, &p, knobs, &reason);
            d.snapshot = Some(meta);
            d.snapshot_age_ms = Some(age);
            d.action = HedgeAction::Blocked {
                reason,
                guard: Guard::InvalidRead,
            };
            d.trace.invalid_fields = vec!["lp_state".into()];
            let obs = Observation::of(tool, &d, now_ms, 0, ObsSource::Live);
            return with_commit(obs, unreadable_note(commit, why));
        }
    };
    let samples = match lp_knobs {
        Some(_) => price_samples(store, &snap).await,
        None => Vec::new(),
    };
    let cycle = lp_knobs.map(|k| LpCycle::new(k, &samples));
    let (d, next) = decide_hedge_in_cycle(&snap, &meta, age, knobs, &state, cycle.as_ref(), now_ms);
    let note = commit_state(store, tool, commit, &read, &next, now_ms).await;
    with_commit(Observation::of(tool, &d, now_ms, 0, ObsSource::Live), note)
}

/// `lp_decide` over the store rows (+ the price row's 5-minute samples).
/// An unreadable `lp_state` row pauses (`invalid_read`).
pub(crate) async fn lp_decide_obs(
    store: Option<&dyn ObservationStore>,
    wallet: &Pubkey,
    pool: &Pubkey,
    knobs: &LpKnobs,
    commit: bool,
    now_ms: i64,
    fence: Option<u64>,
) -> (Observation, Option<String>) {
    let tool = names::LP_DECIDE;
    let (w, p) = (wallet.to_string(), pool.to_string());
    let (row, snap) = match snapshot_row(store, wallet, pool, fence).await {
        Ok(v) => v,
        Err(why) => {
            let d = LpDecision::without_snapshot(&w, &p, knobs, &why);
            return (Observation::of(tool, &d, now_ms, 0, ObsSource::Live), None);
        }
    };
    let (meta, age) = (row.meta(now_ms), row.age_ms(now_ms));
    let read = read_state(store, wallet, pool).await;
    let state = match read.state(wallet, pool) {
        Ok(state) => state,
        Err(why) => {
            let mut d = LpDecision::without_snapshot(&w, &p, knobs, &unreadable_reason(why));
            d.snapshot = Some(meta);
            d.snapshot_age_ms = Some(age);
            d.invalid_fields = vec!["lp_state".into()];
            d.verdict = LpVerdict::Paused {
                reason: PauseReason::InvalidRead,
            };
            let obs = Observation::of(tool, &d, now_ms, 0, ObsSource::Live);
            return with_commit(obs, unreadable_note(commit, why));
        }
    };
    let samples = price_samples(store, &snap).await;
    let (d, next) = decide_lp(&snap, &meta, age, knobs, &state, &samples, now_ms);
    let note = commit_state(store, tool, commit, &read, &next, now_ms).await;
    with_commit(Observation::of(tool, &d, now_ms, 0, ObsSource::Live), note)
}

const WSOL: &str = "So11111111111111111111111111111111111111112";
const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

fn default_wsol() -> String {
    WSOL.into()
}

fn default_usdc() -> String {
    USDC.into()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LpSwapPlanArgs {
    wallet_sol: f64,
    wallet_usdc: f64,
    target_sol: f64,
    target_usdc: f64,
    permanent_minimum_sol: f64,
    rent_reserve_sol: f64,
    #[serde(default)]
    position_rent_sol: f64,
    #[serde(default)]
    reserve_usdc: f64,
    current_price: f64,
    slippage_buffer_pct: f64,
    context: DepositSwapContext,
    #[serde(default = "default_wsol")]
    base_mint: String,
    #[serde(default = "default_usdc")]
    quote_mint: String,
}

impl From<LpSwapPlanArgs> for DepositSwapInput {
    fn from(a: LpSwapPlanArgs) -> Self {
        Self {
            wallet_sol: a.wallet_sol,
            wallet_usdc: a.wallet_usdc,
            target_sol: a.target_sol,
            target_usdc: a.target_usdc,
            permanent_minimum_sol: a.permanent_minimum_sol,
            rent_reserve_sol: a.rent_reserve_sol,
            position_rent_sol: a.position_rent_sol,
            reserve_usdc: a.reserve_usdc,
            current_price: a.current_price,
            slippage_buffer_pct: a.slippage_buffer_pct,
            context: a.context,
            base_mint: a.base_mint,
            quote_mint: a.quote_mint,
        }
    }
}

struct LpSwapPlanTool {
    def: ToolDef,
}

#[async_trait]
impl Tool for LpSwapPlanTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, _ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        // scope: pure-compute — deterministic reserve and shortfall arithmetic.
        let parsed: LpSwapPlanArgs = serde_json::from_value(args.clone())
            .map_err(|e| anyhow!("{}: {e}", names::LP_SWAP_PLAN))?;
        let plan = plan_swap_for_deposit(&parsed.into());
        let now = now_ms();
        Ok(ToolOutput::observed(
            Observation::of(names::LP_SWAP_PLAN, &plan, now, 0, ObsSource::Live),
            now,
        ))
    }
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
        let lp_knobs = match &a.lp_knobs {
            Some(v) => Some(LpKnobs::parse(v).map_err(|e| anyhow!("{tool}: lp_knobs: {e}"))?),
            None => None,
        };
        let now = now_ms();
        let store = self.shared.store.as_deref();
        let fence = self.shared.fence(&a.wallet).await;
        let (obs, note) = hedge_decide_obs_in_cycle(
            store,
            &a.wallet,
            &a.pool,
            &knobs,
            lp_knobs.as_ref(),
            a.commit,
            now,
            fence,
        )
        .await;
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
        let fence = self.shared.fence(&a.wallet).await;
        let (obs, note) =
            lp_decide_obs(store, &a.wallet, &a.pool, &knobs, a.commit, now, fence).await;
        Ok(decided(obs, note, now))
    }
}

#[cfg(test)]
#[path = "lp_tests.rs"]
mod tests;
