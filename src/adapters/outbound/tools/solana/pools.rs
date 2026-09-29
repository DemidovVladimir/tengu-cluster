//! `dlmm_pools` — search Meteora DLMM pools on the datapi. Parsing, the
//! fee/TVL cross-check, sort / filter / limit live in
//! `domain::lp::market::parse_datapi_pools`; this file fetches.
//!
//! | Concern | Rule |
//! |---|---|
//! | Request | `GET dlmm.datapi.meteora.ag/pools?page=1&page_size=100&query=<query>` (`market::DATAPI_PAGE_SIZE`) |
//! | Sort / filter / limit | in Rust over the fetched page: `sort` = `fee_tvl_24h` (default) \| `tvl` \| `volume_24h`, `min_tvl_usd`, `limit` 1-50 (default 10) |
//! | Cache key | `dlmm_pools/1:<query>\|<sort>\|<limit>\|<min_tvl>` (`DlmmPoolList::subject_for`) |
//! | TTL | 60 s (`market::POOLS_TTL_MS`) |
//! | Errors | bad args → `Err`; HTTP / timeout / non-JSON → an `Error` observation (never cached) |

use std::future::Future;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::Value;

use super::{defs, SolanaShared};
use crate::adapters::outbound::solana::http_json::fetch_json;
use crate::adapters::outbound::solana::rpc::read_error;
use crate::application::observe::observe;
use crate::domain::lp::market::{
    self, DlmmPoolList, PoolSort, DATAPI_PAGE_SIZE, POOLS_LIMIT_DEFAULT, POOLS_LIMIT_MAX,
    POOLS_TTL_MS,
};
use crate::domain::message::ToolDef;
use crate::domain::observation::{now_ms, CachePolicy, Observation, Observed};
use crate::domain::tools as names;
use crate::ports::observation::ObservationStore;
use crate::ports::tool::{Tool, ToolCtx, ToolOutput};

pub(crate) fn tools(shared: &SolanaShared) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(DlmmPoolsTool {
        def: defs::def(names::DLMM_POOLS),
        store: shared.store.clone(),
    })]
}

#[derive(Debug, Clone, PartialEq)]
struct PoolsRequest {
    query: String,
    sort: PoolSort,
    limit: u32,
    min_tvl_usd: Option<f64>,
}

impl PoolsRequest {
    fn parse(args: &Value) -> Result<Self> {
        let tool = names::DLMM_POOLS;
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .ok_or_else(|| {
                anyhow!("{tool}: 'query' is required (non-empty string, e.g. SOL-USDC)")
            })?
            .to_string();
        let limit = match args.get("limit") {
            None | Some(Value::Null) => POOLS_LIMIT_DEFAULT,
            Some(v) => v
                .as_u64()
                .filter(|l| (1..=u64::from(POOLS_LIMIT_MAX)).contains(l))
                .map(|l| l as u32)
                .ok_or_else(|| {
                    anyhow!("{tool}: 'limit' must be an integer 1-{POOLS_LIMIT_MAX}, got {v}")
                })?,
        };
        let sort = match args.get("sort") {
            None | Some(Value::Null) => PoolSort::FeeTvl24h,
            Some(v) => v.as_str().and_then(PoolSort::parse).ok_or_else(|| {
                anyhow!("{tool}: 'sort' must be one of fee_tvl_24h, tvl, volume_24h, got {v}")
            })?,
        };
        let min_tvl_usd = match args.get("min_tvl_usd") {
            None | Some(Value::Null) => None,
            Some(v) => Some(
                v.as_f64()
                    .filter(|m| m.is_finite() && *m >= 0.0)
                    .ok_or_else(|| {
                        anyhow!("{tool}: 'min_tvl_usd' must be a number >= 0, got {v}")
                    })?,
            ),
        };
        Ok(Self {
            query,
            sort,
            limit,
            min_tvl_usd,
        })
    }

    fn subject(&self) -> String {
        DlmmPoolList::subject_for(&self.query, self.sort, self.limit, self.min_tvl_usd)
    }
}

struct DlmmPoolsTool {
    def: ToolDef,
    store: Option<Arc<dyn ObservationStore>>,
}

#[async_trait]
impl Tool for DlmmPoolsTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        ctx.scope.check_fs_write(ctx.workspace)?;
        let req = PoolsRequest::parse(args)?;
        let now = now_ms();
        let get_json = |url: String| async move { fetch_json(ctx, &url).await };
        let obs = pools_observation(self.store.as_deref(), &req, args, get_json, now).await?;
        Ok(ToolOutput::observed(obs, now))
    }
}

/// The `dlmm_pools` observation (cache-or-fetch); `get_json` = `fetch_json`
/// in production.
async fn pools_observation<J, JF>(
    store: Option<&dyn ObservationStore>,
    req: &PoolsRequest,
    args: &Value,
    get_json: J,
    now: i64,
) -> Result<Observation>
where
    J: Fn(String) -> JF,
    JF: Future<Output = Result<(u16, Value)>>,
{
    let policy = CachePolicy::new(DlmmPoolList::SCHEMA, &req.subject(), POOLS_TTL_MS, args);
    observe(store, names::DLMM_POOLS, &policy, now, || async {
        let url = market::datapi_pools_url(&req.query, DATAPI_PAGE_SIZE);
        let list = match get_json(url).await {
            Ok((_, v)) => {
                market::parse_datapi_pools(&v, &req.query, req.sort, req.limit, req.min_tvl_usd)
            }
            Err(e) => DlmmPoolList::failed(
                &req.query,
                req.sort,
                req.limit,
                req.min_tvl_usd,
                read_error("pools", &e),
            ),
        };
        Ok((list, POOLS_TTL_MS))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use crate::adapters::outbound::solana::rpc::RpcError;
    use crate::adapters::outbound::tools::solana::price::tests::{FakeJson, Live, POOL};
    use crate::application::observe::tests::MemStore;
    use crate::domain::observation::{
        assert_features_ok, ErrorClass, ObsSource, ObsStatus, MAX_LINE1_CHARS,
    };

    const POOLS: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/solana/market/datapi_pools_sol_usdc.json"
    ));
    const POOLS_EMPTY: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/solana/market/datapi_pools_empty.json"
    ));

    fn datapi(body: &str) -> FakeJson {
        FakeJson::default().with(
            "dlmm.datapi.meteora.ag",
            Ok(serde_json::from_str(body).unwrap()),
        )
    }

    async fn run(
        store: Option<&dyn ObservationStore>,
        args: Value,
        json: &FakeJson,
        now: i64,
    ) -> Observation {
        let req = PoolsRequest::parse(&args).unwrap();
        pools_observation(store, &req, &args, |u| json.get(u), now)
            .await
            .unwrap()
    }

    #[test]
    fn args_defaults_and_errors() {
        let r = PoolsRequest::parse(&json!({"query": " SOL-USDC "})).unwrap();
        assert_eq!(
            (r.query.as_str(), r.sort, r.limit, r.min_tvl_usd),
            ("SOL-USDC", PoolSort::FeeTvl24h, 10, None)
        );
        assert_eq!(r.subject(), "SOL-USDC|fee_tvl_24h|10|0");
        let r = PoolsRequest::parse(
            &json!({"query": "SOL-USDC", "sort": "tvl", "limit": 50, "min_tvl_usd": 100000}),
        )
        .unwrap();
        assert_eq!(r.subject(), "SOL-USDC|tvl|50|100000");
        for (args, needle) in [
            (json!({}), "'query'"),
            (json!({"query": "  "}), "'query'"),
            (json!({"query": "SOL-USDC", "limit": 0}), "'limit'"),
            (json!({"query": "SOL-USDC", "limit": 51}), "'limit'"),
            (json!({"query": "SOL-USDC", "limit": "5"}), "'limit'"),
            (json!({"query": "SOL-USDC", "sort": "apr"}), "'sort'"),
            (
                json!({"query": "SOL-USDC", "min_tvl_usd": -1}),
                "'min_tvl_usd'",
            ),
        ] {
            let e = PoolsRequest::parse(&args).unwrap_err().to_string();
            assert!(e.starts_with("dlmm_pools:") && e.contains(needle), "{e}");
        }
    }

    #[tokio::test]
    async fn fixture_sorted_filtered_and_cached() {
        let store = MemStore::default();
        let json = datapi(POOLS);
        let args = json!({"query": "SOL-USDC", "limit": 5});
        let o = run(Some(&store), args.clone(), &json, 1_000).await;
        assert_eq!(o.key, "dlmm_pools/1:SOL-USDC|fee_tvl_24h|5|0");
        assert_eq!(
            (o.status, o.source),
            (ObsStatus::Ok, ObsSource::Live),
            "{:?}",
            o.errors
        );
        assert_features_ok(&o.features);
        let list: DlmmPoolList = o.typed().unwrap();
        let top: Vec<&str> = list.pools.iter().map(|p| p.address.as_str()).collect();
        // Pinned in market.rs (computed independently in python).
        assert_eq!(
            top,
            vec![
                "3M9nHQhxRMrK66hxRVTGLEmrvEK6Pimds6C3f3WaaLyt",
                "CLM92hJx6CGNBqTifR6Lvcvs3BuFbGWw1U4zLHELzQFL",
                "HTvjzsfX3yU6BUodCjZ5vZkUrAxMDTrBs3CJaq43ashR",
                "EYRZ7TiMxfaergZb5j9UQga3dXAtGbiaeWrWDMKrNUVm",
                "BGm1tav58oGcsQJehL9WXBFXF7D27vZsKefj4xJKD5Y",
            ]
        );
        assert_eq!(
            json.seen(),
            vec![
                "https://dlmm.datapi.meteora.ag/pools?page=1&page_size=100&query=SOL-USDC"
                    .to_string()
            ]
        );
        let line1 = o.render_text(1_000).lines().next().unwrap().to_string();
        assert!(
            line1.contains("3M9nHQhxRMrK66hxRVTGLEmrvEK6Pimds6C3f3WaaLyt"),
            "{line1}"
        );
        assert!(line1.chars().count() <= MAX_LINE1_CHARS, "{line1}");

        // Same args within 60 s: cached. Other args: another key, live.
        let b = run(Some(&store), args, &json, 50_000).await;
        assert_eq!(b.source, ObsSource::Cache);
        let c = run(
            Some(&store),
            json!({"query": "SOL-USDC", "sort": "tvl", "min_tvl_usd": 10000}),
            &json,
            50_000,
        )
        .await;
        assert_eq!(c.source, ObsSource::Live);
        assert_eq!(c.key, "dlmm_pools/1:SOL-USDC|tvl|10|10000");
        let list: DlmmPoolList = c.typed().unwrap();
        assert_eq!(list.pools[0].address, POOL);
        assert!(list.pools.iter().all(|p| p.tvl_usd >= 10_000.0));
        assert_eq!(json.seen().len(), 2);
    }

    #[tokio::test]
    async fn empty_result_is_absent() {
        let json = datapi(POOLS_EMPTY);
        let o = run(None, json!({"query": "ZZNOTAPAIRZZ"}), &json, 0).await;
        assert_eq!(o.status, ObsStatus::Absent);
        assert_eq!(o.typed::<DlmmPoolList>().unwrap().pools.len(), 0);
    }

    #[tokio::test]
    async fn transport_error_is_an_uncached_error_row() {
        let store = MemStore::default();
        let json = FakeJson::default().with(
            "dlmm.datapi.meteora.ag",
            Err(RpcError::new(
                ErrorClass::RateLimited,
                "HTTP 429 from dlmm.datapi.meteora.ag",
            )),
        );
        let o = run(Some(&store), json!({"query": "SOL-USDC"}), &json, 0).await;
        assert_eq!(o.status, ObsStatus::Error);
        assert_eq!(
            (o.errors[0].field.as_str(), o.errors[0].class),
            ("pools", ErrorClass::RateLimited)
        );
        assert!(store.get(&o.key).await.unwrap().is_none());
    }

    // ── live ──

    #[tokio::test]
    #[ignore]
    async fn live_dlmm_pools_sol_usdc() {
        let live = Live::new();
        let o = live
            .call_twice(names::DLMM_POOLS, json!({"query": "SOL-USDC", "limit": 10}))
            .await;
        assert_eq!(o.key, "dlmm_pools/1:SOL-USDC|fee_tvl_24h|10|0");
        let list: DlmmPoolList = o.typed().unwrap();
        assert!(list.pools.len() >= 3, "{list:?}");
        // The apr == fees_24h / tvl x 100 cross-check held on every row.
        assert!(
            !o.errors.iter().any(|e| e.field == "fee_tvl_24h_pct"),
            "{:?}",
            o.errors
        );
        for p in list.pools.iter().filter(|p| p.tvl_usd > 0.0) {
            let (ft, fees) = (p.fee_tvl_24h_pct.unwrap(), p.fees_24h_usd.unwrap());
            assert!(
                (ft - fees / p.tvl_usd * 100.0).abs() <= 1e-6 * ft.abs().max(1.0),
                "{p:?}"
            );
        }
        let line1 = o
            .render_text(o.observed_at_ms)
            .lines()
            .next()
            .unwrap()
            .to_string();
        assert!(line1.contains(&list.pools[0].address), "{line1}");
    }
}
