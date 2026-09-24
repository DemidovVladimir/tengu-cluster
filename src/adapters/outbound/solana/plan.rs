//! Read planning shared by the Solana LP glue (`tools/solana/{dlmm,perps}.rs`,
//! reused by `lp_snapshot`): which accounts to read, in what order, and how
//! DLMM position discovery is throttled. Every account read goes through
//! [`fetch_accounts`] (cache-through `acct/1:<pubkey>` rows).
//!
//! | Piece | Rule |
//! |---|---|
//! | [`read_pool`] | read 1 = LbPair + caller extras (positions, perps, wallet keys); read 2 (`minContextSlot` ≥ read 1's newest slot) = mints + reserves + bin arrays for the depth bands (active ± 50) ∪ the wallet's position ranges |
//! | [`discover_positions`] | explicit keys ⇒ `Found{args}` (no gPA); a fresh `dlmm_discovery/1:<wallet>:<pool>` row ⇒ `Found/Empty{cached}`; else gPA DLMM with memcmp disc@0 (`LgkNAEYaVX3`), lb_pair@8, owner@40 and NO `dataSize` (extended positions are longer) |
//! | Discovery row TTL | found 60 s, empty 300 s, error never stored (bot `meteoraAdapter.ts:88,253`) |
//! | Discovery bypass | `force` (caller `max_age_secs = 0`) or a row older than `min_context_slot`; a gPA answered below `min_context_slot` is a `Transient` error |
//! | Discovery failure | `Discovery::Error` + the last known (expired) row's keys as the fallback set |
//! | Stale cached discovery | [`discovery_stale`]: a cached key that no longer reads as the wallet's PositionV2 in the pool ⇒ the caller re-discovers |
//! | [`perps_keys`] | long + short SOL position PDAs (`solana::jup_position_pda`) + SOL / USDC custody + JLP pool |
//! | [`oracle_usd`] | usable `price_oracle/1:<mint>` row ≤ 30 s old, else Jupiter lite price v3 inline, else `None` |
//! | Whole-read failure | [`ReadFailure`] carries a domain `ReadError` through `anyhow`; [`read_failure`] recovers it (or classifies an RPC / HTTP error); [`failed_observation`] = the never-cached `Error` observation tools return instead of `Err` |

use std::fmt;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{debug, warn};

use super::accounts::fetch_accounts;
use super::http_json::fetch_json;
use super::rpc::{read_error, RpcError, SolanaRpc};
use crate::domain::lp::dlmm::{
    bin_array_keys_needed, lb_pair_from_set, plan_position_keys, pool_account_keys,
    position_gpa_filters, Discovery, DiscoverySource, LbPair, POSITION_V2_DISC,
};
use crate::domain::lp::market::{jupiter_price_url, parse_jupiter_price, OraclePrice};
use crate::domain::lp::perps;
use crate::domain::observation::{
    set_int, ErrorClass, Features, ObsSource, ObsStatus, Observation, Observed, ReadError,
};
use crate::domain::solana::{bin_array_pda, ids, jup_position_pda, AccountSet, Pubkey};
use crate::ports::observation::ObservationStore;
use crate::ports::tool::ToolCtx;

/// Discovery row TTL when positions were found (`meteoraAdapter.ts:88`).
pub(crate) const DISCOVERY_FOUND_TTL_MS: u64 = 60_000;
/// Discovery row TTL when the wallet has no position (`meteoraAdapter.ts:253`).
pub(crate) const DISCOVERY_EMPTY_TTL_MS: u64 = 300_000;
/// `Observation::tool` of discovery rows.
pub(crate) const DISCOVERY_TOOL: &str = "dlmm_discovery";
/// Max age of a cached `price_oracle/1` row used as the perps oracle.
pub(crate) const ORACLE_MAX_AGE_MS: u64 = 30_000;

// ---------------------------------------------------------------------------
// Whole-read failures
// ---------------------------------------------------------------------------

/// A whole-read failure carrying its domain [`ReadError`] through `anyhow`
/// (e.g. the LbPair or a mint is unreadable).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ReadFailure(pub ReadError);

impl fmt::Display for ReadFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: {} ({})",
            self.0.field,
            self.0.message,
            self.0.class.as_str()
        )
    }
}

impl std::error::Error for ReadFailure {}

/// `ReadError` of any glue error: a [`ReadFailure`] as is, anything else
/// classified by `rpc::read_error` under `field`.
pub(crate) fn read_failure(field: &str, e: &anyhow::Error) -> ReadError {
    match e.chain().find_map(|c| c.downcast_ref::<ReadFailure>()) {
        Some(f) => f.0.clone(),
        None => read_error(field, e),
    }
}

/// `Error` observation for a read whose primary answer is unavailable:
/// never cached (`ttl_ms = 0`, and the store ignores `Error` rows), no
/// features, no data. `headline` carries the full ids.
pub(crate) fn failed_observation(
    tool: &str,
    schema: &str,
    subject: &str,
    headline: String,
    errors: Vec<ReadError>,
    now_ms: i64,
) -> Observation {
    Observation {
        key: Observation::key_for(schema, subject),
        schema: schema.to_string(),
        tool: tool.to_string(),
        observed_at_ms: now_ms,
        slot: None,
        ttl_ms: 0,
        source: ObsSource::Live,
        status: ObsStatus::Error,
        errors,
        headline,
        features: Features::new(),
        data: Value::Null,
    }
}

// ---------------------------------------------------------------------------
// Pool reads
// ---------------------------------------------------------------------------

/// Extra keys / options of [`read_pool`].
#[derive(Debug, Clone, Default)]
pub(crate) struct PoolReadOpts {
    /// Read with the LbPair in read 1 (positions, perps, wallet accounts).
    pub extra: Vec<Pubkey>,
    /// Also cover this wallet's positions found in read 1 with bin arrays.
    pub positions_of: Option<Pubkey>,
    /// Read-after-write floor for every account (RPC `minContextSlot`).
    pub min_slot: Option<u64>,
}

/// One pool read: the account set, the decoded LbPair and the `(index,
/// PDA)` bin arrays requested (so an array absent on chain counts as empty).
#[derive(Debug, Clone)]
pub(crate) struct PoolRead {
    pub set: AccountSet,
    /// Decoded once here; `lp_snapshot` (pair roles, active bin) reads it.
    #[allow(dead_code)]
    pub pair: LbPair,
    pub bin_arrays: Vec<(i64, Pubkey)>,
}

/// `(index, PDA)` of `pool`'s bin arrays for `indexes`.
pub(crate) fn bin_array_pdas(pool: &Pubkey, indexes: &[i64]) -> Vec<(i64, Pubkey)> {
    indexes
        .iter()
        .map(|i| (*i, bin_array_pda(pool, *i)))
        .collect()
}

/// Bin arrays covering `wallet`'s PositionV2 accounts of `pool` in `set`.
pub(crate) fn position_bin_array_keys(
    set: &AccountSet,
    wallet: &Pubkey,
    pool: &Pubkey,
) -> Vec<(i64, Pubkey)> {
    bin_array_pdas(pool, &plan_position_keys(set, wallet, pool))
}

/// Read `pool` and everything its builders need (see the module table).
/// `Err`: an RPC failure (`RpcError` in the chain) or an unreadable LbPair
/// ([`ReadFailure`]: `NotApplicable` when the pool does not exist, `Decode`
/// on a bad account).
pub(crate) async fn read_pool(
    rpc: &SolanaRpc,
    store: Option<&dyn ObservationStore>,
    pool: &Pubkey,
    opts: &PoolReadOpts,
    max_age_ms: u64,
    now_ms: i64,
) -> Result<PoolRead> {
    let mut first = Vec::with_capacity(1 + opts.extra.len());
    first.push(*pool);
    first.extend(opts.extra.iter().copied());
    let mut set = fetch_accounts(rpc, store, &first, max_age_ms, opts.min_slot, now_ms).await?;
    let pair = lb_pair_from_set(&set, pool).map_err(ReadFailure)?;

    let mut bin_arrays = bin_array_pdas(pool, &bin_array_keys_needed(&pair, &[]));
    if let Some(wallet) = &opts.positions_of {
        bin_arrays.extend(position_bin_array_keys(&set, wallet, pool));
        bin_arrays.sort_unstable_by_key(|(i, _)| *i);
        bin_arrays.dedup_by_key(|(i, _)| *i);
    }
    let mut second = pool_account_keys(&pair);
    second.extend(bin_arrays.iter().map(|(_, k)| *k));
    let pin = opts.min_slot.unwrap_or(0).max(set.slot_max);
    let more = fetch_accounts(rpc, store, &second, max_age_ms, Some(pin), now_ms).await?;
    for read in more.accounts.into_values() {
        set.insert(read);
    }
    Ok(PoolRead {
        set,
        pair,
        bin_arrays,
    })
}

// ---------------------------------------------------------------------------
// Position discovery
// ---------------------------------------------------------------------------

/// `dlmm_discovery/1:<wallet>:<pool>` — the PositionV2 keys one gPA found.
/// Stored with TTL 60 s (found) / 300 s (empty); failures are never rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct DiscoveryRow {
    pub wallet: String,
    pub pool: String,
    /// gPA context slot.
    pub slot: u64,
    /// PositionV2 accounts, full base58, sorted.
    pub positions: Vec<String>,
}

impl DiscoveryRow {
    pub(crate) fn ttl_ms(&self) -> u64 {
        if self.positions.is_empty() {
            DISCOVERY_EMPTY_TTL_MS
        } else {
            DISCOVERY_FOUND_TTL_MS
        }
    }

    fn keys(&self) -> std::result::Result<Vec<Pubkey>, String> {
        self.positions.iter().map(|s| s.parse()).collect()
    }
}

impl Observed for DiscoveryRow {
    const SCHEMA: &'static str = "dlmm_discovery/1";

    fn subject(&self) -> String {
        format!("{}:{}", self.wallet, self.pool)
    }

    fn headline(&self) -> String {
        format!(
            "dlmm_discovery {} {} n={}",
            self.wallet,
            self.pool,
            self.positions.len()
        )
    }

    fn features(&self) -> Features {
        let mut f = Features::new();
        set_int(&mut f, "position_count", Some(self.positions.len() as i64));
        f
    }

    fn slot(&self) -> Option<u64> {
        Some(self.slot)
    }

    fn status(&self) -> ObsStatus {
        if self.positions.is_empty() {
            ObsStatus::Absent
        } else {
            ObsStatus::Ok
        }
    }
}

/// Store key of a discovery row.
pub(crate) fn discovery_key(wallet: &Pubkey, pool: &Pubkey) -> String {
    Observation::key_for(DiscoveryRow::SCHEMA, &format!("{wallet}:{pool}"))
}

/// Options of [`discover_positions`].
#[derive(Debug, Clone, Default)]
pub(crate) struct DiscoverOpts {
    /// Non-empty ⇒ `Found{source: args}` over exactly these keys, no gPA.
    pub explicit: Vec<Pubkey>,
    /// A cached row below this slot is not reused; a gPA answered below it
    /// is a `Transient` error.
    pub min_slot: Option<u64>,
    /// Skip the cached row (caller asked for a live read).
    pub force: bool,
}

/// Discovery outcome + the PositionV2 keys to read (the fallback set when
/// discovery failed).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Discovered {
    pub discovery: Discovery,
    pub positions: Vec<Pubkey>,
}

impl Discovered {
    pub(crate) fn is_cached(&self) -> bool {
        match &self.discovery {
            Discovery::Found { source, .. } => *source == DiscoverySource::Cached,
            Discovery::Empty { .. } => false,
            Discovery::Error { .. } => false,
        }
    }
}

fn found_or_empty(keys: Vec<Pubkey>, source: DiscoverySource, at_ms: i64) -> Discovered {
    let discovery = if keys.is_empty() {
        Discovery::Empty { at_ms }
    } else {
        Discovery::Found {
            count: keys.len() as u32,
            source,
            at_ms,
        }
    };
    Discovered {
        discovery,
        positions: keys,
    }
}

/// `wallet`'s PositionV2 accounts in `pool` (see the module table). Never
/// fails: an RPC error becomes `Discovery::Error` with a fallback set.
pub(crate) async fn discover_positions(
    rpc: &SolanaRpc,
    store: Option<&dyn ObservationStore>,
    wallet: &Pubkey,
    pool: &Pubkey,
    opts: &DiscoverOpts,
    now_ms: i64,
) -> Discovered {
    if !opts.explicit.is_empty() {
        let mut keys = opts.explicit.clone();
        keys.sort_unstable();
        keys.dedup();
        return found_or_empty(keys, DiscoverySource::Args, now_ms);
    }

    let key = discovery_key(wallet, pool);
    let last = match store {
        Some(s) => match s.get(&key).await {
            Ok(row) => row,
            Err(e) => {
                let error = format!("{e:#}");
                warn!(%key, %error, "observation store read failed; discovering live");
                None
            }
        },
        None => None,
    };
    let last = last.and_then(|obs| {
        let row = obs.typed::<DiscoveryRow>().ok()?;
        let keys = row.keys().ok()?;
        Some((obs, row, keys))
    });
    if let Some((obs, row, keys)) = &last {
        let fresh = obs.is_fresh(now_ms, u64::MAX);
        let recent_enough = opts.min_slot.map_or(true, |m| row.slot >= m);
        if !opts.force && fresh && recent_enough {
            return found_or_empty(keys.clone(), DiscoverySource::Cached, obs.observed_at_ms);
        }
    }

    match gpa_positions(rpc, wallet, pool, opts.min_slot).await {
        Ok((slot, keys)) => {
            if let Some(s) = store {
                let row = DiscoveryRow {
                    wallet: wallet.to_string(),
                    pool: pool.to_string(),
                    slot,
                    positions: keys.iter().map(Pubkey::to_string).collect(),
                };
                let obs =
                    Observation::of(DISCOVERY_TOOL, &row, now_ms, row.ttl_ms(), ObsSource::Live);
                if let Err(e) = s.put(&obs).await {
                    let error = format!("{e:#}");
                    warn!(key = %obs.key, %error, "observation store write failed");
                }
            }
            found_or_empty(keys, DiscoverySource::Gpa, now_ms)
        }
        Err(e) => {
            let fallback = last.map(|(_, _, keys)| keys).unwrap_or_default();
            Discovered {
                discovery: Discovery::Error {
                    error: read_failure("discovery", &e),
                    fallback_count: fallback.len() as u32,
                },
                positions: fallback,
            }
        }
    }
}

/// gPA over DLMM for `wallet`'s positions in `pool` → (context slot, sorted keys).
async fn gpa_positions(
    rpc: &SolanaRpc,
    wallet: &Pubkey,
    pool: &Pubkey,
    min_slot: Option<u64>,
) -> Result<(u64, Vec<Pubkey>)> {
    let (slot, mut keys) = rpc
        .get_program_account_keys(&ids::key(ids::DLMM), &position_gpa_filters(wallet, pool))
        .await?;
    if let Some(m) = min_slot.filter(|m| slot < *m) {
        return Err(RpcError::new(
            ErrorClass::Transient,
            format!(
                "getProgramAccounts @ {} answered at slot {slot} < min_context_slot {m}",
                rpc.host()
            ),
        )
        .into());
    }
    keys.sort_unstable();
    keys.dedup();
    Ok((slot, keys))
}

/// `true` when a key of a cached discovery no longer reads as `wallet`'s
/// PositionV2 in `pool` (closed or transferred since). Keys not in `set`
/// cannot be judged and do not count.
pub(crate) fn discovery_stale(
    set: &AccountSet,
    positions: &[Pubkey],
    wallet: &Pubkey,
    pool: &Pubkey,
) -> bool {
    let dlmm = ids::key(ids::DLMM);
    positions.iter().any(|k| {
        let Some(read) = set.get(k) else {
            return false;
        };
        let is_position = read.owner() == Some(&dlmm)
            && read.data().is_some_and(|d| {
                d.len() >= 72
                    && d[..8] == POSITION_V2_DISC[..]
                    && d[8..40] == pool.0[..]
                    && d[40..72] == wallet.0[..]
            });
        !is_position
    })
}

// ---------------------------------------------------------------------------
// Jupiter perps
// ---------------------------------------------------------------------------

/// A wallet's SOL position PDAs and every key `perps::build_perps` reads.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PerpsKeys {
    pub long: Pubkey,
    pub short: Pubkey,
    /// long, short, SOL custody, USDC custody, JLP pool.
    pub keys: Vec<Pubkey>,
}

pub(crate) fn perps_keys(wallet: &Pubkey) -> PerpsKeys {
    let long = jup_position_pda(wallet, true);
    let short = jup_position_pda(wallet, false);
    PerpsKeys {
        long,
        short,
        keys: perps::perps_keys(&long, &short),
    }
}

// ---------------------------------------------------------------------------
// Oracle price
// ---------------------------------------------------------------------------

/// USD price of `mint` from a usable `price_oracle/1:<mint>` row at most
/// `max_age_ms` old (regardless of the row's own TTL).
pub(crate) async fn cached_usd(
    store: Option<&dyn ObservationStore>,
    mint: &str,
    max_age_ms: u64,
    now_ms: i64,
) -> Option<f64> {
    let key = Observation::key_for(OraclePrice::SCHEMA, mint);
    let row = match store?.get(&key).await {
        Ok(row) => row?,
        Err(e) => {
            let error = format!("{e:#}");
            warn!(%key, %error, "observation store read failed");
            return None;
        }
    };
    if !row.status.usable() || row.age_ms(now_ms) > max_age_ms {
        return None;
    }
    row.typed::<OraclePrice>()
        .ok()?
        .usd
        .filter(|p| p.is_finite() && *p > 0.0)
}

/// Jupiter lite price v3 USD price of `mint`; `None` on any failure.
pub(crate) async fn jupiter_usd(ctx: &ToolCtx<'_>, mint: &str) -> Option<f64> {
    jupiter_usd_at(ctx, &jupiter_price_url(mint), mint).await
}

async fn jupiter_usd_at(ctx: &ToolCtx<'_>, url: &str, mint: &str) -> Option<f64> {
    match fetch_json(ctx, url).await {
        Ok((_, v)) => parse_jupiter_price(&v, mint).value().map(|p| p.usd),
        Err(e) => {
            let error = read_error("jupiter", &e);
            debug!(mint, class = error.class.as_str(), message = %error.message, "inline Jupiter price failed");
            None
        }
    }
}

/// Oracle USD for `mint`: a cached row ≤ [`ORACLE_MAX_AGE_MS`], else an
/// inline Jupiter read, else `None`.
pub(crate) async fn oracle_usd(
    ctx: &ToolCtx<'_>,
    store: Option<&dyn ObservationStore>,
    mint: &str,
    now_ms: i64,
) -> Option<f64> {
    match cached_usd(store, mint, ORACLE_MAX_AGE_MS, now_ms).await {
        Some(p) => Some(p),
        None => jupiter_usd(ctx, mint).await,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Arc;

    use serde_json::json;

    use crate::adapters::outbound::solana::rpc::parse_gma;
    use crate::adapters::outbound::solana::rpc::tests::{
        canned, err_envelope, fake_rpc, local_scope, ok_envelope, serve, test_client, FakeTransport,
    };
    use crate::adapters::outbound::tools::workspace::test_support::TestHarness;
    use crate::application::observe::tests::MemStore;
    use crate::domain::lp::market::JupiterPrice;
    use crate::domain::observation::{assert_features_ok, Field};
    use crate::domain::solana::AccountRead;

    // ── fixtures shared with tools/solana/{dlmm,perps}.rs tests ─────

    pub(crate) const DLMM_GMA: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/solana/dlmm/gma.json"
    ));
    pub(crate) const DLMM_META: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/solana/dlmm/meta.json"
    ));
    pub(crate) const DLMM_GOLDEN: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/solana/dlmm/golden.json"
    ));
    pub(crate) const PERPS_GMA: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/solana/perps/gma.json"
    ));
    pub(crate) const PERPS_META: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/solana/perps/meta.json"
    ));
    pub(crate) const JUP_PRICE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/solana/perps/jup_price.json"
    ));

    pub(crate) const POOL: &str = "5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS6";
    /// Owner of three positions in the DLMM fixture.
    pub(crate) const OWNER: &str = "JBggt27MzM4eohjumT9Tuec7MBoWAgDM4BJjkoisDUcs";
    pub(crate) const BOT_WALLET: &str = "F3YvPiLdniRPGpeKrbeGWR2zg2wPpzVuvqBA5BBJBQ5S";
    pub(crate) const DLMM_SLOT: u64 = 450_102_095;

    pub(crate) fn k(s: &str) -> Pubkey {
        s.parse().unwrap()
    }

    /// Reads of a fixture `getMultipleAccounts` envelope for `keys`.
    pub(crate) fn fixture_reads(gma: &str, keys: &[Pubkey]) -> Vec<AccountRead> {
        let v: Value = serde_json::from_str(gma).unwrap();
        parse_gma(&v["result"], keys).unwrap().1
    }

    pub(crate) fn dlmm_meta() -> Value {
        serde_json::from_str(DLMM_META).unwrap()
    }

    pub(crate) fn dlmm_keys() -> Vec<Pubkey> {
        dlmm_meta()["keys"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| k(s.as_str().unwrap()))
            .collect()
    }

    /// The fixture owner's positions (sorted), per `meta.json`.
    pub(crate) fn owner_positions() -> Vec<Pubkey> {
        let mut p: Vec<Pubkey> = dlmm_meta()["roles"]["positions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| k(s.as_str().unwrap()))
            .filter(|p| p.to_string() != "14JU64KbNMLmFiS8qHuZ9ZF1swmzmiBYCRZidM24rH1m")
            .collect();
        p.sort_unstable();
        p
    }

    /// Fake RPC serving the DLMM fixture accounts at its slot.
    pub(crate) fn dlmm_transport() -> Arc<FakeTransport> {
        let t = Arc::new(FakeTransport::at_slot(DLMM_SLOT));
        for r in fixture_reads(DLMM_GMA, &dlmm_keys()) {
            t.put_account(r);
        }
        t
    }

    /// Unix ms of the DLMM golden's clock (fee projection input).
    pub(crate) fn dlmm_now_ms() -> i64 {
        let g: Value = serde_json::from_str(DLMM_GOLDEN).unwrap();
        g["clock_unix_timestamp"].as_i64().unwrap() * 1000
    }

    /// A scripted `getProgramAccounts` (withContext, keys only) reply.
    pub(crate) fn gpa_reply(slot: u64, keys: &[Pubkey]) -> Value {
        let rows: Vec<Value> = keys
            .iter()
            .map(|k| json!({"pubkey": k.to_string(), "account": {"data": ["", "base64"]}}))
            .collect();
        ok_envelope(json!({"context": {"slot": slot}, "value": rows}))
    }

    pub(crate) fn methods(t: &FakeTransport) -> Vec<String> {
        t.requests()
            .iter()
            .map(|r| r["method"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn bin_array_pdas_match_the_dlmm_fixture() {
        let m = dlmm_meta();
        let pool = k(m["pool"].as_str().unwrap());
        let idx: Vec<i64> = m["roles"]["bin_array_indexes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i.as_i64().unwrap())
            .collect();
        let want: Vec<(i64, Pubkey)> = idx
            .iter()
            .zip(m["roles"]["bin_arrays"].as_array().unwrap())
            .map(|(i, s)| (*i, k(s.as_str().unwrap())))
            .collect();
        assert_eq!(want.len(), 4);
        assert_eq!(bin_array_pdas(&pool, &idx), want);
        // Each fixture array is a DLMM BinArray whose index@8 / lb_pair@24 match.
        let reads = fixture_reads(DLMM_GMA, &dlmm_keys());
        for (i, key) in &want {
            let r = reads.iter().find(|r| r.pubkey == *key).unwrap();
            assert_eq!(r.owner(), Some(&ids::key(ids::DLMM)));
            let d = r.data().unwrap();
            assert_eq!(i64::from_le_bytes(d[8..16].try_into().unwrap()), *i);
            assert_eq!(&d[24..56], &pool.0[..]);
        }
    }

    #[tokio::test]
    async fn read_pool_is_two_reads_with_the_second_pinned() {
        let t = dlmm_transport();
        let rpc = fake_rpc(&t);
        let pool = k(POOL);
        let owner = k(OWNER);
        let opts = PoolReadOpts {
            extra: owner_positions(),
            positions_of: Some(owner),
            min_slot: None,
        };
        let read = read_pool(&rpc, None, &pool, &opts, 5_000, 0).await.unwrap();
        let gma = t.gma_params();
        assert_eq!(gma.len(), 2);
        let first: Vec<&str> = gma[0][0]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(first[0], POOL);
        assert_eq!(first.len(), 4, "pool + 3 positions");
        assert!(gma[0][1].get("minContextSlot").is_none());
        assert_eq!(gma[1][1]["minContextSlot"], DLMM_SLOT);
        // Depth (active -5373 ± 50 → -78, -77) ∪ the owner's ranges.
        let idx: Vec<i64> = read.bin_arrays.iter().map(|(i, _)| *i).collect();
        assert!(idx.contains(&-78) && idx.contains(&-77), "{idx:?}");
        assert_eq!(read.pair.active_id, -5373);
        assert!(read
            .set
            .get(&k("EYj9xKw6ZszwpyNibHY7JD5o3QgTVrSdcBp1fMJhrR9o"))
            .is_some());
        let covering = position_bin_array_keys(&read.set, &owner, &pool);
        assert!(!covering.is_empty());
        assert!(covering.iter().all(|(i, _)| idx.contains(i)));
    }

    #[tokio::test]
    async fn read_pool_errors_are_classified() {
        // Pool absent on chain → ReadFailure NotApplicable.
        let t = Arc::new(FakeTransport::at_slot(7));
        let rpc = fake_rpc(&t);
        let e = read_pool(&rpc, None, &k(POOL), &PoolReadOpts::default(), 5_000, 0)
            .await
            .unwrap_err();
        let r = read_failure("accounts", &e);
        assert_eq!(
            (r.field.as_str(), r.class),
            ("pool", ErrorClass::NotApplicable)
        );
        assert!(r.message.contains(POOL), "{}", r.message);
        // RPC quota → classified under the caller's field.
        t.push(Ok(err_envelope(-32429, "max usage reached")));
        let e = read_pool(&rpc, None, &k(POOL), &PoolReadOpts::default(), 5_000, 0)
            .await
            .unwrap_err();
        let r = read_failure("accounts", &e);
        assert_eq!(
            (r.field.as_str(), r.class),
            ("accounts", ErrorClass::QuotaExhausted)
        );
    }

    #[tokio::test]
    async fn discovery_gpa_then_cached_then_ttl_expiry() {
        let t = dlmm_transport();
        let rpc = fake_rpc(&t);
        let store = MemStore::default();
        let (wallet, pool) = (k(OWNER), k(POOL));
        let keys = owner_positions();
        t.push(Ok(gpa_reply(DLMM_SLOT, &keys)));
        let d = discover_positions(
            &rpc,
            Some(&store),
            &wallet,
            &pool,
            &DiscoverOpts::default(),
            1_000,
        )
        .await;
        assert_eq!(d.positions, keys);
        assert!(matches!(
            d.discovery,
            Discovery::Found {
                count: 3,
                source: DiscoverySource::Gpa,
                at_ms: 1_000
            }
        ));
        let req = &t.requests()[0];
        assert_eq!(req["method"], "getProgramAccounts");
        assert_eq!(req["params"][0], ids::DLMM);
        let filters = req["params"][1]["filters"].as_array().unwrap();
        assert_eq!(filters.len(), 3, "no dataSize filter");
        assert_eq!(
            filters[0]["memcmp"],
            json!({"offset": 0, "bytes": "LgkNAEYaVX3"})
        );
        assert_eq!(filters[1]["memcmp"], json!({"offset": 8, "bytes": POOL}));
        assert_eq!(filters[2]["memcmp"], json!({"offset": 40, "bytes": OWNER}));
        let row = store
            .get(&discovery_key(&wallet, &pool))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (row.ttl_ms, row.slot, row.status, row.tool.as_str()),
            (
                DISCOVERY_FOUND_TTL_MS,
                Some(DLMM_SLOT),
                ObsStatus::Ok,
                DISCOVERY_TOOL
            )
        );
        assert_eq!(row.key, format!("dlmm_discovery/1:{OWNER}:{POOL}"));
        assert_features_ok(&row.features);

        // Within 60 s: cached, no request.
        let d = discover_positions(
            &rpc,
            Some(&store),
            &wallet,
            &pool,
            &DiscoverOpts::default(),
            60_000,
        )
        .await;
        assert!(d.is_cached());
        assert!(matches!(
            d.discovery,
            Discovery::Found {
                count: 3,
                at_ms: 1_000,
                ..
            }
        ));
        assert_eq!(t.requests().len(), 1);
        // force (max_age_secs = 0) and a newer min_slot both bypass the row.
        t.push(Ok(gpa_reply(DLMM_SLOT, &keys)));
        let force = DiscoverOpts {
            force: true,
            ..Default::default()
        };
        let d = discover_positions(&rpc, Some(&store), &wallet, &pool, &force, 60_000).await;
        assert!(matches!(
            d.discovery,
            Discovery::Found {
                source: DiscoverySource::Gpa,
                ..
            }
        ));
        t.push(Ok(gpa_reply(DLMM_SLOT + 10, &keys)));
        let newer = DiscoverOpts {
            min_slot: Some(DLMM_SLOT + 5),
            ..Default::default()
        };
        let d = discover_positions(&rpc, Some(&store), &wallet, &pool, &newer, 60_000).await;
        assert!(matches!(
            d.discovery,
            Discovery::Found {
                source: DiscoverySource::Gpa,
                ..
            }
        ));
        assert_eq!(t.requests().len(), 3);
        // Past 60 s: a new gPA.
        t.push(Ok(gpa_reply(DLMM_SLOT + 10, &keys)));
        discover_positions(
            &rpc,
            Some(&store),
            &wallet,
            &pool,
            &DiscoverOpts::default(),
            125_000,
        )
        .await;
        assert_eq!(t.requests().len(), 4);
    }

    #[tokio::test]
    async fn empty_discovery_is_cached_five_minutes() {
        let t = Arc::new(FakeTransport::at_slot(9));
        let rpc = fake_rpc(&t);
        let store = MemStore::default();
        let (wallet, pool) = (k(BOT_WALLET), k(POOL));
        t.push(Ok(gpa_reply(9, &[])));
        let d = discover_positions(
            &rpc,
            Some(&store),
            &wallet,
            &pool,
            &DiscoverOpts::default(),
            0,
        )
        .await;
        assert_eq!(d.discovery, Discovery::Empty { at_ms: 0 });
        let row = store
            .get(&discovery_key(&wallet, &pool))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (row.ttl_ms, row.status),
            (DISCOVERY_EMPTY_TTL_MS, ObsStatus::Absent)
        );
        let d = discover_positions(
            &rpc,
            Some(&store),
            &wallet,
            &pool,
            &DiscoverOpts::default(),
            299_000,
        )
        .await;
        assert_eq!(d.discovery, Discovery::Empty { at_ms: 0 });
        assert_eq!(t.requests().len(), 1);
    }

    #[tokio::test]
    async fn explicit_keys_skip_gpa() {
        let t = Arc::new(FakeTransport::at_slot(9));
        let rpc = fake_rpc(&t);
        let mut keys = owner_positions();
        keys.push(keys[0]);
        keys.reverse();
        let opts = DiscoverOpts {
            explicit: keys,
            ..Default::default()
        };
        let d = discover_positions(&rpc, None, &k(OWNER), &k(POOL), &opts, 5).await;
        assert_eq!(d.positions, owner_positions(), "sorted + deduplicated");
        assert!(matches!(
            d.discovery,
            Discovery::Found {
                count: 3,
                source: DiscoverySource::Args,
                at_ms: 5
            }
        ));
        assert!(t.requests().is_empty());
    }

    #[tokio::test]
    async fn gpa_failure_is_an_error_with_the_last_known_keys() {
        let t = Arc::new(FakeTransport::at_slot(9));
        let rpc = fake_rpc(&t);
        let store = MemStore::default();
        let (wallet, pool) = (k(OWNER), k(POOL));
        // No previous row: Error, no fallback.
        t.push(Ok(err_envelope(
            -32010,
            "excluded from account secondary indexes",
        )));
        let d = discover_positions(
            &rpc,
            Some(&store),
            &wallet,
            &pool,
            &DiscoverOpts::default(),
            0,
        )
        .await;
        let Discovery::Error {
            error,
            fallback_count,
        } = &d.discovery
        else {
            panic!("{d:?}")
        };
        assert_eq!(
            (error.field.as_str(), error.class, *fallback_count),
            ("discovery", ErrorClass::Fatal, 0)
        );
        assert!(d.positions.is_empty());
        assert!(
            store
                .get(&discovery_key(&wallet, &pool))
                .await
                .unwrap()
                .is_none(),
            "errors are never stored"
        );
        // An expired found row is the fallback set.
        t.push(Ok(gpa_reply(9, &owner_positions())));
        discover_positions(
            &rpc,
            Some(&store),
            &wallet,
            &pool,
            &DiscoverOpts::default(),
            0,
        )
        .await;
        t.push(Err(RpcError::new(
            ErrorClass::QuotaExhausted,
            "max usage reached",
        )));
        let d = discover_positions(
            &rpc,
            Some(&store),
            &wallet,
            &pool,
            &DiscoverOpts::default(),
            61_000,
        )
        .await;
        assert!(matches!(
            &d.discovery,
            Discovery::Error { error, fallback_count: 3 } if error.class == ErrorClass::QuotaExhausted
        ));
        assert_eq!(d.positions, owner_positions());
        // A gPA answered below min_context_slot is Transient, never "empty".
        t.push(Ok(gpa_reply(9, &[])));
        let opts = DiscoverOpts {
            min_slot: Some(10),
            force: true,
            ..Default::default()
        };
        let d = discover_positions(&rpc, Some(&store), &wallet, &pool, &opts, 0).await;
        assert!(matches!(
            &d.discovery,
            Discovery::Error { error, .. } if error.class == ErrorClass::Transient && error.message.contains("min_context_slot 10")
        ));
    }

    #[test]
    fn discovery_staleness() {
        let reads = fixture_reads(DLMM_GMA, &dlmm_keys());
        let mut set = AccountSet::default();
        for r in reads {
            set.insert(r);
        }
        let (owner, pool) = (k(OWNER), k(POOL));
        let keys = owner_positions();
        assert!(!discovery_stale(&set, &keys, &owner, &pool));
        // Another wallet's position, a non-position account, an absent account.
        let ext = k("14JU64KbNMLmFiS8qHuZ9ZF1swmzmiBYCRZidM24rH1m");
        assert!(discovery_stale(&set, &[ext], &owner, &pool));
        assert!(discovery_stale(&set, &[pool], &owner, &pool));
        let gone = k(BOT_WALLET);
        set.insert(AccountRead {
            pubkey: gone,
            slot: 1,
            state: crate::domain::solana::AccountState::Absent,
        });
        assert!(discovery_stale(&set, &[keys[0], gone], &owner, &pool));
        // Keys never read cannot be judged.
        assert!(!discovery_stale(&set, &[k(ids::JLP_POOL)], &owner, &pool));
    }

    #[test]
    fn perps_keys_derive_the_verified_pdas() {
        let p = perps_keys(&k(BOT_WALLET));
        assert_eq!(p.long, k("FqymRcB92t63jpwh7om4RLbxMNUGoHnZPQMkkAA8ksVY"));
        assert_eq!(p.short, k("6HFhuYzQGcqdj4NGwC6vfVETRvMA3pXaVeZnHgWSKsJK"));
        assert_eq!(
            p.keys,
            vec![
                p.long,
                p.short,
                k(ids::JUP_CUSTODY_SOL),
                k(ids::JUP_CUSTODY_USDC),
                k(ids::JLP_POOL)
            ]
        );
    }

    fn oracle_row(usd: f64, now_ms: i64) -> Observation {
        let price = crate::domain::lp::market::combine_price(
            ids::WSOL,
            Field::ok(JupiterPrice {
                usd,
                block_id: Some(1),
                change_24h_pct: None,
                liquidity_usd: None,
            }),
            Field::Absent,
            None,
            None,
            now_ms,
        );
        Observation::of("sol_price", &price, now_ms, 10_000, ObsSource::Live)
    }

    #[tokio::test]
    async fn cached_usd_takes_a_usable_row_up_to_30_s() {
        let store = MemStore::default();
        assert_eq!(
            cached_usd(Some(&store), ids::WSOL, ORACLE_MAX_AGE_MS, 0).await,
            None
        );
        store.put(&oracle_row(150.25, 1_000)).await.unwrap();
        // Past the row's 10 s TTL but within 30 s: still used.
        assert_eq!(
            cached_usd(Some(&store), ids::WSOL, ORACLE_MAX_AGE_MS, 25_000).await,
            Some(150.25)
        );
        assert_eq!(
            cached_usd(Some(&store), ids::WSOL, ORACLE_MAX_AGE_MS, 31_001).await,
            None
        );
        assert_eq!(
            cached_usd(None, ids::WSOL, ORACLE_MAX_AGE_MS, 1_000).await,
            None
        );
        assert_eq!(
            cached_usd(Some(&store), ids::USDC, ORACLE_MAX_AGE_MS, 1_000).await,
            None
        );
    }

    #[tokio::test]
    async fn jupiter_usd_parses_the_fixture_and_fails_soft() {
        let (base, _) = serve(vec![canned(200, JUP_PRICE), canned(503, "down")]).await;
        let mut h = TestHarness::with_scope(&std::env::temp_dir(), local_scope());
        h.http = test_client();
        let url = format!("{base}/price/v3?ids={}", ids::WSOL);
        assert_eq!(
            jupiter_usd_at(&h.ctx(), &url, ids::WSOL).await,
            Some(116.6589164559586)
        );
        assert_eq!(jupiter_usd_at(&h.ctx(), &url, ids::WSOL).await, None);
        // Host outside the scope: None, nothing sent.
        let denied = TestHarness::with_scope(&std::env::temp_dir(), Default::default());
        assert_eq!(jupiter_usd_at(&denied.ctx(), &url, ids::WSOL).await, None);
    }

    #[test]
    fn failed_observation_is_never_fresh_and_renders_ids() {
        let e = ReadError::new(
            "pool",
            ErrorClass::NotApplicable,
            format!("DLMM pool {POOL} does not exist"),
        );
        let o = failed_observation(
            "dlmm_pool",
            "dlmm_pool/1",
            POOL,
            format!("dlmm_pool {POOL} error"),
            vec![e],
            5,
        );
        assert_eq!(o.key, format!("dlmm_pool/1:{POOL}"));
        assert_eq!((o.status, o.ttl_ms), (ObsStatus::Error, 0));
        assert!(!o.is_fresh(5, u64::MAX));
        let text = o.render_text(5);
        assert!(text.lines().next().unwrap().contains(POOL), "{text}");
        assert!(text.contains("error pool: not_applicable"), "{text}");
        // ReadFailure round-trips through anyhow.
        let err: anyhow::Error = ReadFailure(o.errors[0].clone()).into();
        assert_eq!(read_failure("accounts", &err), o.errors[0]);
    }
}
