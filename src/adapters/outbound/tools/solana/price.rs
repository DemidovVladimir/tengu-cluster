//! `sol_price` — USD oracle price of a mint: Jupiter price v3 always, Pyth
//! Hermes only with `pyth_feed_id`, and a Meteora DLMM pool's active price
//! as the cross-check only with `pool`. Pure selection / sample ring live in
//! `domain::lp::market::combine_price`; this file fetches.
//!
//! | Concern | Rule |
//! |---|---|
//! | Cache key | `price_oracle/1:<mint>`; with `pool`: `price_oracle/1:<mint>:<pool>` — a pool-less row never answers a pool call (and vice versa) |
//! | TTL | 10 s (`market::PRICE_TTL_MS`) |
//! | Cache reuse | a fresh row is reused only when it was built with the same Pyth request (`pyth_feed_id` given ⇔ the row read that feed) |
//! | Jupiter | `GET lite-api.jup.ag/price/v3?ids=<mint>` → `market::parse_jupiter_price`; transport failure → `Field::Error` (field `jupiter`) |
//! | Pyth | only with `pyth_feed_id` (Hermes answers 401 today → `AuthRequired`, field `pyth`); else `Field::Absent` |
//! | Pool | LbPair then both mints via `fetch_accounts` (mints reuse rows up to 60 s — decimals / program never change) → `dlmm::build_dlmm_pool` without bin arrays; `active_price` used only when base = `mint` and quote = USDC, else `Field::Error(NotApplicable)` (field `pool_price`) |
//! | Sample ring | the previous row at the same key, at any age (`combine_price` uses only its samples) |
//! | Errors | bad args → `Err`; no usable source → an `Error` observation (never cached) |
//!
//! Also hosts the argument helpers of the WP-GLUE-A tools (`pools.rs`,
//! `wallet.rs`): pubkeys are parsed in full and errors name the argument.

use std::future::Future;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::warn;

use super::{defs, SolanaShared};
use crate::adapters::outbound::solana::accounts::{fetch_accounts, ACCOUNT_ROW_TTL_MS};
use crate::adapters::outbound::solana::http_json::fetch_json;
use crate::adapters::outbound::solana::rpc::{read_error, SolanaRpc};
use crate::application::observe::observe;
use crate::domain::lp::dlmm;
use crate::domain::lp::market::{
    self, JupiterPrice, OraclePrice, PoolQuote, PythPrice, PRICE_TTL_MS,
};
use crate::domain::message::ToolDef;
use crate::domain::observation::{
    now_ms, CachePolicy, ErrorClass, Features, Field, ObsStatus, Observation, Observed, ReadError,
};
use crate::domain::solana::{ids, Pubkey};
use crate::domain::tools as names;
use crate::ports::observation::ObservationStore;
use crate::ports::tool::{Tool, ToolCtx, ToolOutput};

pub(crate) fn tools(shared: &SolanaShared) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(SolPriceTool {
        def: defs::def(names::SOL_PRICE),
        store: shared.store.clone(),
    })]
}

// ---------------------------------------------------------------------------
// Argument helpers (shared by pools.rs / wallet.rs)
// ---------------------------------------------------------------------------

/// Optional base58 pubkey argument; `null` / missing = `None`.
pub(super) fn opt_pubkey(args: &Value, tool: &str, key: &str) -> Result<Option<Pubkey>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => parse_pubkey_value(v, tool, key).map(Some),
    }
}

/// Required base58 pubkey argument.
pub(super) fn require_pubkey(args: &Value, tool: &str, key: &str) -> Result<Pubkey> {
    opt_pubkey(args, tool, key)?
        .ok_or_else(|| anyhow!("{tool}: '{key}' is required (base58 pubkey)"))
}

/// A pubkey from one JSON value; `what` names it in the error
/// (`mints[2]`). The value is echoed in full.
pub(super) fn parse_pubkey_value(v: &Value, tool: &str, what: &str) -> Result<Pubkey> {
    let s = v
        .as_str()
        .ok_or_else(|| anyhow!("{tool}: '{what}' must be a base58 pubkey string, got {v}"))?;
    s.trim()
        .parse::<Pubkey>()
        .map_err(|e| anyhow!("{tool}: '{what}' is not a valid pubkey ({e}): {s}"))
}

/// Cache subject of a `sol_price` row: `<mint>` or `<mint>:<pool>`; the
/// store key is `Observation::key_for(OraclePrice::SCHEMA, subject)`.
pub(crate) fn price_subject(mint: &str, pool: Option<&str>) -> String {
    match pool {
        Some(p) => format!("{mint}:{p}"),
        None => mint.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
struct PriceRequest {
    mint: Pubkey,
    pool: Option<Pubkey>,
    /// 64 lowercase hex chars, no `0x`.
    pyth_feed_id: Option<String>,
}

impl PriceRequest {
    fn parse(args: &Value) -> Result<Self> {
        let tool = names::SOL_PRICE;
        let mint = opt_pubkey(args, tool, "mint")?.unwrap_or_else(|| ids::key(ids::WSOL));
        let pool = opt_pubkey(args, tool, "pool")?;
        let pyth_feed_id = match args.get("pyth_feed_id") {
            None | Some(Value::Null) => None,
            Some(v) => {
                let raw = v.as_str().ok_or_else(|| {
                    anyhow!("{tool}: 'pyth_feed_id' must be a hex string, got {v}")
                })?;
                let hex = raw.trim().trim_start_matches("0x").to_ascii_lowercase();
                if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(anyhow!(
                        "{tool}: 'pyth_feed_id' must be 64 hex chars (optional 0x), got {raw}"
                    ));
                }
                Some(hex)
            }
        };
        Ok(Self {
            mint,
            pool,
            pyth_feed_id,
        })
    }

    fn subject(&self) -> String {
        price_subject(
            &self.mint.to_string(),
            self.pool.map(|p| p.to_string()).as_deref(),
        )
    }

    /// May a row built earlier answer this request? Only when it asked
    /// Pyth for the same feed (or neither asked).
    fn pyth_matches(&self, row: &OraclePrice) -> bool {
        match (&self.pyth_feed_id, &row.pyth) {
            (None, Field::Absent) => true,
            (None, _) | (Some(_), Field::Absent) => false,
            (Some(feed), Field::Ok { value }) => value.feed_id.eq_ignore_ascii_case(feed),
            (Some(feed), Field::Error { error }) => error.message.contains(feed.as_str()),
        }
    }
}

// ---------------------------------------------------------------------------
// Observed wrapper: the pool-aware cache subject
// ---------------------------------------------------------------------------

/// `OraclePrice` keyed by `<mint>` or `<mint>:<pool>` (market.rs keys by
/// mint only). Transparent: the stored `data` is the plain `OraclePrice`,
/// so readers use `typed::<OraclePrice>()`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
struct KeyedPrice(OraclePrice);

impl Observed for KeyedPrice {
    const SCHEMA: &'static str = <OraclePrice as Observed>::SCHEMA;
    fn subject(&self) -> String {
        price_subject(&self.0.mint, self.0.pool.as_ref().map(|q| q.pool.as_str()))
    }
    fn headline(&self) -> String {
        self.0.headline()
    }
    fn features(&self) -> Features {
        self.0.features()
    }
    fn slot(&self) -> Option<u64> {
        self.0.slot()
    }
    fn status(&self) -> ObsStatus {
        self.0.status()
    }
    fn errors(&self) -> Vec<ReadError> {
        self.0.errors()
    }
}

// ---------------------------------------------------------------------------
// Tool
// ---------------------------------------------------------------------------

struct SolPriceTool {
    def: ToolDef,
    store: Option<Arc<dyn ObservationStore>>,
}

#[async_trait]
impl Tool for SolPriceTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        ctx.scope.check_fs_write(ctx.workspace)?;
        let req = PriceRequest::parse(args)?;
        let now = now_ms();
        let rpc = match req.pool {
            Some(_) => Some(SolanaRpc::from_ctx(ctx)?),
            None => None,
        };
        let get_json = |url: String| async move { fetch_json(ctx, &url).await };
        let obs = price_observation(
            self.store.as_deref(),
            &req,
            args,
            rpc.as_ref(),
            get_json,
            now,
        )
        .await?;
        Ok(ToolOutput::observed(obs, now))
    }
}

/// The `sol_price` observation (cache-or-fetch). `get_json` = `fetch_json`
/// in production; `rpc` is `Some` iff a pool is requested.
async fn price_observation<J, JF>(
    store: Option<&dyn ObservationStore>,
    req: &PriceRequest,
    args: &Value,
    rpc: Option<&SolanaRpc>,
    get_json: J,
    now: i64,
) -> Result<Observation>
where
    J: Fn(String) -> JF,
    JF: Future<Output = Result<(u16, Value)>>,
{
    let mut policy = CachePolicy::new(OraclePrice::SCHEMA, &req.subject(), PRICE_TTL_MS, args);
    let account_max_age = policy.max_age_ms;
    let prev = previous_row(store, &policy.key).await;
    if prev.as_ref().is_some_and(|p| !req.pyth_matches(p)) {
        // Same key, different Pyth request: read live, then replace the row.
        policy.max_age_ms = 0;
    }
    let mint = req.mint.to_string();
    let get_json = &get_json;
    observe(store, names::SOL_PRICE, &policy, now, || async {
        let (jupiter, pyth, pool) = tokio::join!(
            jupiter_field(get_json, &mint),
            pyth_field(get_json, req.pyth_feed_id.as_deref()),
            pool_quote(rpc, store, req, account_max_age, now),
        );
        let price = market::combine_price(&mint, jupiter, pyth, pool, prev.as_ref(), now);
        Ok((KeyedPrice(price), PRICE_TTL_MS))
    })
    .await
}

/// The stored row at `key` at any age (sample ring + reuse check); store
/// failures and undecodable rows are `None`.
async fn previous_row(store: Option<&dyn ObservationStore>, key: &str) -> Option<OraclePrice> {
    let row = match store?.get(key).await {
        Ok(row) => row?,
        Err(e) => {
            let error = format!("{e:#}");
            warn!(key, %error, "observation store read failed; no previous price row");
            return None;
        }
    };
    row.typed::<OraclePrice>().ok()
}

async fn jupiter_field<J, JF>(get_json: &J, mint: &str) -> Field<JupiterPrice>
where
    J: Fn(String) -> JF,
    JF: Future<Output = Result<(u16, Value)>>,
{
    match get_json(market::jupiter_price_url(mint)).await {
        Ok((_, v)) => market::parse_jupiter_price(&v, mint),
        Err(e) => Field::err(read_error("jupiter", &e)),
    }
}

/// Pyth read for `feed` (`Absent` without one). Every error message names
/// the feed, so a cached row can be matched to its request.
async fn pyth_field<J, JF>(get_json: &J, feed: Option<&str>) -> Field<PythPrice>
where
    J: Fn(String) -> JF,
    JF: Future<Output = Result<(u16, Value)>>,
{
    let Some(feed) = feed else {
        return Field::Absent;
    };
    let field = match get_json(market::pyth_latest_url(feed)).await {
        Ok((_, v)) => market::parse_pyth(&v, feed),
        Err(e) => Field::err(read_error("pyth", &e)),
    };
    match field {
        Field::Error { mut error } => {
            if !error.message.contains(feed) {
                error.message = format!("feed {feed}: {}", error.message);
            }
            Field::err(error)
        }
        other => other,
    }
}

/// The pool cross-check; `None` without a `pool` arg.
async fn pool_quote(
    rpc: Option<&SolanaRpc>,
    store: Option<&dyn ObservationStore>,
    req: &PriceRequest,
    max_age_ms: u64,
    now: i64,
) -> Option<PoolQuote> {
    let pool = req.pool?;
    let price = match rpc {
        Some(rpc) => read_pool_price(rpc, store, &pool, &req.mint, max_age_ms, now).await,
        None => Err(ReadError::new(
            POOL_FIELD,
            ErrorClass::Fatal,
            "no Solana RPC client for the pool read",
        )),
    };
    Some(PoolQuote {
        pool: pool.to_string(),
        price: price.map(Field::ok).unwrap_or_else(Field::err),
    })
}

const POOL_FIELD: &str = "pool_price";

fn as_pool_field(mut e: ReadError) -> ReadError {
    e.field = POOL_FIELD.to_string();
    e
}

/// USD per `mint` from the DLMM pool's active bin — only for a pool whose
/// base (token X) is `mint` and whose quote (token Y) is USDC.
async fn read_pool_price(
    rpc: &SolanaRpc,
    store: Option<&dyn ObservationStore>,
    pool: &Pubkey,
    mint: &Pubkey,
    max_age_ms: u64,
    now: i64,
) -> std::result::Result<f64, ReadError> {
    let mut set = fetch_accounts(rpc, store, &[*pool], max_age_ms, None, now)
        .await
        .map_err(|e| read_error(POOL_FIELD, &e))?;
    let pair = dlmm::lb_pair_from_set(&set, pool).map_err(as_pool_field)?;
    let usdc = ids::key(ids::USDC);
    if pair.token_x_mint != *mint || pair.token_y_mint != usdc {
        return Err(ReadError::new(
            POOL_FIELD,
            ErrorClass::NotApplicable,
            format!(
                "pool {pool} is {} / {}: its active price is USD per {mint} only with base {mint} and quote USDC {usdc}",
                pair.token_x_mint, pair.token_y_mint
            ),
        ));
    }
    // Mint decimals / token program never change: reuse rows up to 60 s
    // unless the caller forced a live read.
    let mint_max_age = if max_age_ms == 0 {
        0
    } else {
        ACCOUNT_ROW_TTL_MS
    };
    let mints = fetch_accounts(
        rpc,
        store,
        &[pair.token_x_mint, pair.token_y_mint],
        mint_max_age,
        None,
        now,
    )
    .await
    .map_err(|e| read_error(POOL_FIELD, &e))?;
    for read in mints.accounts.into_values() {
        set.insert(read);
    }
    let state = dlmm::build_dlmm_pool(&set, pool, &[], now).map_err(as_pool_field)?;
    Ok(state.active_price)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;

    use serde_json::json;

    use crate::adapters::outbound::solana::rpc::tests::{
        core_keys, fake_rpc, FakeTransport, CORE_GMA,
    };
    use crate::adapters::outbound::solana::rpc::{parse_gma, RpcError};
    use crate::application::observe::tests::MemStore;
    use crate::domain::observation::{assert_features_ok, ObsSource, MAX_LINE1_CHARS};

    pub(crate) const WSOL: &str = "So11111111111111111111111111111111111111112";
    pub(crate) const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
    pub(crate) const POOL: &str = "5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS6";
    const FEED: &str = "ef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d";
    const JUP_V3: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/solana/market/jupiter_price_v3.json"
    ));

    /// Canned JSON by URL substring (first match wins, replies persist);
    /// records every URL requested.
    #[derive(Default)]
    pub(crate) struct FakeJson {
        pub replies: Vec<(String, std::result::Result<Value, RpcError>)>,
        pub seen: Mutex<Vec<String>>,
    }

    impl FakeJson {
        pub(crate) fn with(
            mut self,
            url_part: &str,
            reply: std::result::Result<Value, RpcError>,
        ) -> Self {
            self.replies.push((url_part.to_string(), reply));
            self
        }
        pub(crate) async fn get(&self, url: String) -> Result<(u16, Value)> {
            self.seen.lock().unwrap().push(url.clone());
            match self
                .replies
                .iter()
                .find(|(part, _)| url.contains(part.as_str()))
            {
                Some((_, Ok(v))) => Ok((200, v.clone())),
                Some((_, Err(e))) => Err(e.clone().into()),
                None => Err(RpcError::new(
                    ErrorClass::Fatal,
                    format!("fake json: no reply for {url}"),
                )
                .into()),
            }
        }
        pub(crate) fn seen(&self) -> Vec<String> {
            self.seen.lock().unwrap().clone()
        }
    }

    fn jupiter() -> FakeJson {
        FakeJson::default().with("lite-api.jup.ag", Ok(serde_json::from_str(JUP_V3).unwrap()))
    }

    /// Fake RPC serving the core mainnet fixture (slot 450104084).
    pub(crate) fn core_transport() -> Arc<FakeTransport> {
        let t = Arc::new(FakeTransport::at_slot(450_104_084));
        let env: Value = serde_json::from_str(CORE_GMA).unwrap();
        let (_, reads) = parse_gma(&env["result"], &core_keys()).unwrap();
        for r in reads {
            t.put_account(r);
        }
        t
    }

    fn req(args: &Value) -> PriceRequest {
        PriceRequest::parse(args).unwrap()
    }

    async fn run(
        store: Option<&dyn ObservationStore>,
        args: Value,
        rpc: Option<&SolanaRpc>,
        json: &FakeJson,
        now: i64,
    ) -> Observation {
        price_observation(store, &req(&args), &args, rpc, |u| json.get(u), now)
            .await
            .unwrap()
    }

    #[test]
    fn args_default_mint_and_name_bad_values() {
        let r = req(&json!({}));
        assert_eq!(r.mint.to_string(), WSOL);
        assert_eq!((r.pool, r.pyth_feed_id), (None, None));
        let r = req(
            &json!({"mint": USDC, "pool": POOL, "pyth_feed_id": format!("0x{}", FEED.to_uppercase())}),
        );
        assert_eq!(r.mint.to_string(), USDC);
        assert_eq!(r.pool.unwrap().to_string(), POOL);
        assert_eq!(r.pyth_feed_id.as_deref(), Some(FEED));
        for (args, needle) in [
            (json!({"mint": "notakey"}), "'mint'"),
            (json!({"pool": 7}), "'pool'"),
            (
                json!({"pool": "5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS"}),
                "'pool'",
            ),
            (json!({"pyth_feed_id": "abc"}), "'pyth_feed_id'"),
        ] {
            let e = PriceRequest::parse(&args).unwrap_err().to_string();
            assert!(e.contains(needle) && e.starts_with("sol_price:"), "{e}");
        }
        // The rejected value is echoed in full.
        let e = PriceRequest::parse(&json!({"pool": "5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS"}))
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS"),
            "{e}"
        );
    }

    #[test]
    fn subject_includes_the_pool() {
        let key = |pool| Observation::key_for(OraclePrice::SCHEMA, &price_subject(WSOL, pool));
        assert_eq!(key(None), format!("price_oracle/1:{WSOL}"));
        assert_eq!(key(Some(POOL)), format!("price_oracle/1:{WSOL}:{POOL}"));
        assert_eq!(req(&json!({})).subject(), WSOL);
        assert_eq!(
            req(&json!({"pool": POOL})).subject(),
            format!("{WSOL}:{POOL}")
        );
    }

    #[tokio::test]
    async fn jupiter_only_is_ok_degraded_and_cached() {
        let store = MemStore::default();
        let json = jupiter();
        let a = run(Some(&store), json!({}), None, &json, 1_000).await;
        assert_eq!(a.key, format!("price_oracle/1:{WSOL}"));
        assert_eq!((a.status, a.source), (ObsStatus::Ok, ObsSource::Live));
        assert_features_ok(&a.features);
        let p: OraclePrice = a.typed().unwrap();
        assert_eq!(p.usd, Some(116.6084160651512));
        assert!(p.degraded && matches!(p.pyth, Field::Absent) && p.pool.is_none());
        assert_eq!(
            json.seen(),
            vec![format!("https://lite-api.jup.ag/price/v3?ids={WSOL}")]
        );
        let line1 = a.render_text(1_000).lines().next().unwrap().to_string();
        assert!(
            line1.contains(WSOL) && line1.chars().count() <= MAX_LINE1_CHARS,
            "{line1}"
        );

        // Within the 10 s TTL: served from the store, no new request.
        let b = run(Some(&store), json!({}), None, &json, 6_000).await;
        assert_eq!(b.source, ObsSource::Cache);
        assert_eq!(json.seen().len(), 1);
        // Past the TTL: live again; the sample ring carries the first sample.
        let c = run(Some(&store), json!({}), None, &json, 12_000).await;
        assert_eq!(c.source, ObsSource::Live);
        assert_eq!(c.typed::<OraclePrice>().unwrap().samples.len(), 2);
    }

    #[tokio::test]
    async fn pool_price_from_the_core_fixture() {
        let t = core_transport();
        let rpc = fake_rpc(&t);
        let store = MemStore::default();
        let json = jupiter();
        let o = run(
            Some(&store),
            json!({"pool": POOL}),
            Some(&rpc),
            &json,
            1_000,
        )
        .await;
        assert_eq!(o.key, format!("price_oracle/1:{WSOL}:{POOL}"));
        assert_eq!(o.status, ObsStatus::Ok, "{:?}", o.errors);
        let p: OraclePrice = o.typed().unwrap();
        let pool_price = p.pool_price.unwrap();
        assert!((100.0..140.0).contains(&pool_price), "{pool_price}");
        let bps = p.pool_vs_oracle_bps.unwrap();
        assert!(bps.abs() < 100.0, "pool vs oracle {bps} bps");
        assert_eq!(p.sources_ok, 2);
        assert!(!p.degraded);
        assert_eq!(p.pool.as_ref().unwrap().pool, POOL);
        let line1 = o.render_text(1_000).lines().next().unwrap().to_string();
        assert!(line1.contains(WSOL) && line1.contains(POOL), "{line1}");
        assert!(line1.chars().count() <= MAX_LINE1_CHARS, "{line1}");
        assert_features_ok(&o.features);
        // Two GMAs: the LbPair, then its two mints.
        let gma = t.gma_params();
        assert_eq!(gma.len(), 2);
        assert_eq!(gma[0][0], json!([POOL]));
        assert_eq!(gma[1][0], json!([WSOL, USDC]));
    }

    /// The stage-2 open issue: a pool-less row must never answer a pool call.
    #[tokio::test]
    async fn pool_and_pool_less_rows_do_not_collide() {
        let t = core_transport();
        let rpc = fake_rpc(&t);
        let store = MemStore::default();
        let json = jupiter();
        let plain = run(Some(&store), json!({}), None, &json, 1_000).await;
        let pooled = run(
            Some(&store),
            json!({"pool": POOL}),
            Some(&rpc),
            &json,
            2_000,
        )
        .await;
        assert_eq!(
            pooled.source,
            ObsSource::Live,
            "not served from the pool-less row"
        );
        assert!(pooled.typed::<OraclePrice>().unwrap().pool_price.is_some());
        let again = run(Some(&store), json!({}), None, &json, 3_000).await;
        assert_eq!(again.source, ObsSource::Cache);
        assert!(again.typed::<OraclePrice>().unwrap().pool.is_none());
        for key in [&plain.key, &pooled.key] {
            assert!(store.get(key).await.unwrap().is_some(), "{key} stored");
        }
    }

    #[tokio::test]
    async fn pool_for_another_mint_is_not_applicable() {
        let t = core_transport();
        let rpc = fake_rpc(&t);
        let json = jupiter();
        let o = run(
            None,
            json!({"mint": USDC, "pool": POOL}),
            Some(&rpc),
            &json,
            1_000,
        )
        .await;
        assert_eq!(o.status, ObsStatus::Partial);
        let p: OraclePrice = o.typed().unwrap();
        let e = p.pool.unwrap().price.error().unwrap().clone();
        assert_eq!(
            (e.field.as_str(), e.class),
            ("pool_price", ErrorClass::NotApplicable)
        );
        assert!(
            e.message.contains(POOL) && e.message.contains(USDC),
            "{}",
            e.message
        );
        assert!(p.usd.is_some() && p.pool_price.is_none());
        assert_eq!(t.gma_params().len(), 1, "no mint read once NotApplicable");
    }

    #[tokio::test]
    async fn pool_rpc_failure_is_a_field_error() {
        let t = Arc::new(FakeTransport::at_slot(1));
        t.push(Err(RpcError::new(
            ErrorClass::QuotaExhausted,
            "max usage reached",
        )));
        let rpc = fake_rpc(&t);
        let json = jupiter();
        let o = run(None, json!({"pool": POOL}), Some(&rpc), &json, 1_000).await;
        assert_eq!(o.status, ObsStatus::Partial);
        let e = o.errors.iter().find(|e| e.field == "pool_price").unwrap();
        assert_eq!(e.class, ErrorClass::QuotaExhausted);
    }

    #[tokio::test]
    async fn pyth_401_is_auth_required_and_keyed_by_request() {
        let store = MemStore::default();
        let json = jupiter().with(
            "hermes.pyth.network",
            Err(RpcError::new(
                ErrorClass::AuthRequired,
                "HTTP 401 unauthorized",
            )),
        );
        let args = json!({"pyth_feed_id": FEED});
        let a = run(Some(&store), args.clone(), None, &json, 1_000).await;
        assert_eq!(a.status, ObsStatus::Partial);
        let e = a.errors.iter().find(|e| e.field == "pyth").unwrap();
        assert_eq!(e.class, ErrorClass::AuthRequired);
        assert!(e.message.contains(FEED), "{}", e.message);
        assert!(json.seen().iter().any(|u| u.contains(FEED)));
        // Same request within the TTL: cached.
        let b = run(Some(&store), args, None, &json, 2_000).await;
        assert_eq!(b.source, ObsSource::Cache);
        // A Pyth-less request does not reuse the Pyth row …
        let c = run(Some(&store), json!({}), None, &json, 3_000).await;
        assert_eq!((c.source, c.status), (ObsSource::Live, ObsStatus::Ok));
        // … nor a request for another feed.
        let other = "eaa020c61cc479712813461ce153894a96a6c00b21ed0cfc2798d1f9a9e9c94a";
        let d = run(
            Some(&store),
            json!({"pyth_feed_id": other}),
            None,
            &json,
            4_000,
        )
        .await;
        assert_eq!(d.source, ObsSource::Live);
    }

    #[tokio::test]
    async fn no_source_is_an_uncached_error() {
        let store = MemStore::default();
        let json = FakeJson::default().with(
            "lite-api.jup.ag",
            Err(RpcError::new(ErrorClass::Timeout, "timed out")),
        );
        let o = run(Some(&store), json!({}), None, &json, 1_000).await;
        assert_eq!(o.status, ObsStatus::Error);
        assert_eq!(o.errors[0].class, ErrorClass::Timeout);
        assert!(o.typed::<OraclePrice>().unwrap().usd.is_none(), "never 0");
        assert!(
            store.get(&o.key).await.unwrap().is_none(),
            "error rows are not stored"
        );
    }

    #[test]
    fn line1_fits_with_44_char_ids() {
        // 44-char mint + pool, large price: line 1 stays ≤ 200 without cutting ids.
        let mint = "F3YvPiLdniRPGpeKrbeGWR2zg2wPpzVuvqBA5BBJBQ5S";
        let pool = "6HFhuYzQGcqdj4NGwC6vfVETRvMA3pXaVeZnHgWSKsJK";
        let jup = Field::ok(JupiterPrice {
            usd: 123_456.123_456,
            block_id: None,
            change_24h_pct: None,
            liquidity_usd: None,
        });
        let quote = PoolQuote {
            pool: pool.into(),
            price: Field::ok(123_000.5),
        };
        let p = market::combine_price(mint, jup, Field::Absent, Some(quote), None, 0);
        let o = Observation::of(
            "sol_price",
            &KeyedPrice(p),
            0,
            PRICE_TTL_MS,
            ObsSource::Live,
        );
        assert_eq!(o.key, format!("price_oracle/1:{mint}:{pool}"));
        let line1 = o.render_text(0).lines().next().unwrap().to_string();
        assert!(line1.chars().count() <= MAX_LINE1_CHARS, "{line1}");
        assert!(line1.contains(mint) && line1.contains(pool), "{line1}");
    }

    #[tokio::test]
    async fn tool_rejects_bad_args_and_denied_scope() {
        let tmp = tempfile::TempDir::new().unwrap();
        let shared = SolanaShared::default();
        let tool = tools(&shared).pop().unwrap();
        assert_eq!(tool.definition().name, names::SOL_PRICE);
        let h =
            crate::adapters::outbound::tools::workspace::test_support::TestHarness::new(tmp.path());
        let e = tool
            .execute(&json!({"mint": "x"}), &h.ctx())
            .await
            .unwrap_err();
        assert!(e.to_string().contains("'mint'"), "{e}");
        let denied =
            crate::adapters::outbound::tools::workspace::test_support::TestHarness::with_scope(
                tmp.path(),
                Default::default(),
            );
        assert!(tool.execute(&json!({}), &denied.ctx()).await.is_err());
    }

    // ── live (public mainnet; `cargo test --bin tengu live_ -- --ignored --test-threads 1`) ──

    use crate::domain::scope::ToolScope;

    /// A real tool context (egress tool client, scope with the public hosts)
    /// and a SQLite store in a temp workspace.
    pub(crate) struct Live {
        /// Keeps the workspace (and its store) alive.
        pub _tmp: tempfile::TempDir,
        pub harness: crate::adapters::outbound::tools::workspace::test_support::TestHarness,
        pub shared: SolanaShared,
    }

    impl Live {
        pub(crate) fn new() -> Self {
            use crate::adapters::outbound::egress;
            use crate::adapters::outbound::observations::SqliteObservationStore;
            use crate::adapters::outbound::solana::rpc::REQUEST_TIMEOUT;
            let tmp = tempfile::TempDir::new().unwrap();
            let scope = ToolScope {
                fs_roots: vec![tmp.path().to_path_buf()],
                net_hosts: vec![
                    "api.mainnet-beta.solana.com".into(),
                    "lite-api.jup.ag".into(),
                    "dlmm.datapi.meteora.ag".into(),
                    "hermes.pyth.network".into(),
                ],
                ..Default::default()
            };
            let mut harness =
                crate::adapters::outbound::tools::workspace::test_support::TestHarness::with_scope(
                    tmp.path(),
                    scope,
                );
            harness.http = egress::policy().tool_client(REQUEST_TIMEOUT).unwrap();
            let store = SqliteObservationStore::open(tmp.path()).unwrap();
            let shared = SolanaShared {
                store: Some(Arc::new(store)),
                ..Default::default()
            };
            Self {
                _tmp: tmp,
                harness,
                shared,
            }
        }

        pub(crate) fn tool(&self, name: &str) -> Arc<dyn Tool> {
            super::super::price::tools(&self.shared)
                .into_iter()
                .chain(super::super::pools::tools(&self.shared))
                .chain(super::super::wallet::tools(&self.shared))
                .find(|t| t.definition().name == name)
                .unwrap()
        }

        /// Execute `name`, then again: the second call is served from the
        /// store. Returns the first observation.
        pub(crate) async fn call_twice(&self, name: &str, args: Value) -> Observation {
            let tool = self.tool(name);
            let first = tool.execute(&args, &self.harness.ctx()).await.unwrap();
            let obs = first.observation.clone().unwrap();
            eprintln!("{}", first.text.lines().next().unwrap_or(""));
            assert!(obs.status.usable(), "{}", first.text);
            assert_features_ok(&obs.features);
            let line1 = first.text.lines().next().unwrap();
            assert!(line1.chars().count() <= MAX_LINE1_CHARS, "{line1}");
            let second = tool.execute(&args, &self.harness.ctx()).await.unwrap();
            let again = second.observation.unwrap();
            assert_eq!(again.source, ObsSource::Cache, "{}", second.text);
            assert_eq!(again.key, obs.key);
            obs
        }
    }

    #[tokio::test]
    #[ignore]
    async fn live_sol_price_jupiter() {
        let live = Live::new();
        let o = live.call_twice(names::SOL_PRICE, json!({})).await;
        assert_eq!(o.key, format!("price_oracle/1:{WSOL}"));
        let line1 = o
            .render_text(o.observed_at_ms)
            .lines()
            .next()
            .unwrap()
            .to_string();
        assert!(line1.contains(WSOL), "{line1}");
        let p: OraclePrice = o.typed().unwrap();
        assert!(p.usd.unwrap() > 1.0, "{p:?}");
    }

    #[tokio::test]
    #[ignore]
    async fn live_sol_price_with_pool() {
        let live = Live::new();
        let o = live
            .call_twice(names::SOL_PRICE, json!({"pool": POOL}))
            .await;
        assert_eq!(o.key, format!("price_oracle/1:{WSOL}:{POOL}"));
        let line1 = o
            .render_text(o.observed_at_ms)
            .lines()
            .next()
            .unwrap()
            .to_string();
        assert!(line1.contains(WSOL) && line1.contains(POOL), "{line1}");
        let p: OraclePrice = o.typed().unwrap();
        let bps = p.pool_vs_oracle_bps.unwrap();
        assert!(bps.abs() < 100.0, "pool vs oracle {bps} bps: {p:?}");
        assert_eq!(o.status, ObsStatus::Ok, "{:?}", o.errors);
    }

    #[tokio::test]
    #[ignore]
    async fn live_sol_price_pyth_auth_required() {
        let live = Live::new();
        let tool = live.tool(names::SOL_PRICE);
        let out = tool
            .execute(&json!({"pyth_feed_id": FEED}), &live.harness.ctx())
            .await
            .unwrap();
        let o = out.observation.unwrap();
        eprintln!("{}", out.text);
        // Hermes answered 401 on 2026-09-24; a 200 would give a Pyth price.
        match o.errors.iter().find(|e| e.field == "pyth") {
            Some(e) => assert_eq!(e.class, ErrorClass::AuthRequired, "{e:?}"),
            None => assert!(o.typed::<OraclePrice>().unwrap().pyth.value().is_some()),
        }
    }
}
