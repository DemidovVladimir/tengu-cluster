//! GeckoTerminal pool OHLCV → `market.db` `bars` (`source =
//! gecko:<network>:<pool>`). `GET <base>/networks/<network>/pools/<pool>/ohlcv/<timeframe>
//! ?aggregate=<n>&before_timestamp=<s>&limit=1000&currency=usd&token=<token>`,
//! `accept: application/json`; paged backwards by the oldest returned bar
//! until it is at or before `from`, or a reply is empty.
//!
//! | Concern | Rule |
//! |---|---|
//! | Base | `https://api.geckoterminal.com/api/v2`; `$GECKO_API_URL` overrides (tests, a proxy) |
//! | Network | the instrument's venue: `solana` → `solana`, `robinhood` → `robinhood` (others refused) |
//! | Timeframe | `1m` minute/1 · `5m` minute/5 · `15m` minute/15 · `1h` hour/1 · `4h` hour/4 · `1d` day/1 |
//! | `token` | the instrument's own address (its native id): the pool's price of that token in USD, whichever side of the pool it is on |
//! | Gate | `egress::policy().check_url` (`[egress] allow_hosts`) + the client's scope `net_hosts`, on every request; a denial is `Fatal`, nothing is sent |
//! | Budget | `[rate_limits.geckoterminal]` (`outbound/rate_limit.rs`; weight 1 per request; xlab: 25 / min, the free API allows ~30); a 429 drains it for `Retry-After`; no section = unlimited |
//! | Errors | `http_class` mapping (429 `rate_limited`, 5xx `transient`, other 4xx `fatal`, a body that is no OHLCV `decode`) |
//! | Audit | one egress line per request: `tool = "gecko_ohlcv"`, `host`, `path` (full pool address), `status`, `ms` |
//! | Partial bars | the newest bar is the open one: dropped (never stored) |

use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use reqwest::Url;
use serde_json::{json, Value};

use super::{
    grid_ceil, grid_floor, missing_ranges, no_bars_note, resume_note, stored_span, ReportRow, Retry,
};
use crate::adapters::outbound::egress;
use crate::adapters::outbound::http_class::{
    http_status_error, reqwest_error, retry_after_ms, HttpError, Scrubber,
};
use crate::adapters::outbound::rate_limit::{limiters, Limiters, Priority};
use crate::config::rate_limits::RateLimitConfig;
use crate::config::sections::SandboxSections;
use crate::domain::market::InstrumentId;
use crate::domain::marketdata::{Bar, Interval};
use crate::domain::marketdata_decode::{closed_bars, gecko_ohlcv};
use crate::domain::observation::ErrorClass;
use crate::domain::scope::ToolScope;
use crate::ports::market_data::MarketDataStore;

/// Public API base.
pub(crate) const GECKO_API_URL: &str = "https://api.geckoterminal.com/api/v2";
/// Env var overriding the API base (operator commands).
pub(crate) const GECKO_API_ENV: &str = "GECKO_API_URL";
/// The `[rate_limits.<name>]` Gecko requests budget against.
pub(crate) const RATE_LIMIT: &str = "geckoterminal";
/// Bars per reply (the API maximum).
pub(crate) const PAGE_LIMIT: usize = 1_000;
const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_BUDGET_WAIT: Duration = Duration::from_secs(120);

/// GeckoTerminal network of an instrument venue.
pub(crate) fn network_of(venue: &str) -> Result<&'static str, String> {
    match venue {
        "solana" => Ok("solana"),
        "robinhood" => Ok("robinhood"),
        other => Err(format!(
            "no GeckoTerminal network for venue `{other}` (solana, robinhood)"
        )),
    }
}

/// `(timeframe, aggregate)` of an interval.
pub(crate) fn timeframe(interval: Interval) -> (&'static str, u32) {
    match interval {
        Interval::M1 => ("minute", 1),
        Interval::M5 => ("minute", 5),
        Interval::M15 => ("minute", 15),
        Interval::H1 => ("hour", 1),
        Interval::H4 => ("hour", 4),
        Interval::D1 => ("day", 1),
    }
}

/// The OHLCV endpoint with its scope and budget.
pub(crate) struct GeckoClient {
    http: reqwest::Client,
    base: Url,
    host: String,
    scope: ToolScope,
    budget: Option<RateLimitConfig>,
    timeout: Duration,
    max_budget_wait: Duration,
    limiters: &'static Limiters,
}

/// An operator command's client (`tengu history backfill --source gecko`):
/// `$GECKO_API_URL` or the public base, a scope that pins that host (the
/// `[egress]` policy stays the ceiling), the sandbox's
/// `[rate_limits.geckoterminal]`.
pub(crate) fn operator_gecko(sections: &SandboxSections) -> Result<GeckoClient> {
    let base = std::env::var(GECKO_API_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| GECKO_API_URL.to_string());
    let host = Url::parse(&base)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .ok_or_else(|| anyhow!("{GECKO_API_ENV} is not a URL with a host"))?;
    let scope = ToolScope {
        net_hosts: vec![host],
        ..Default::default()
    };
    let http = egress::policy().tool_client(TIMEOUT)?;
    GeckoClient::new(
        http,
        &base,
        scope,
        sections.rate_limits.get(RATE_LIMIT).cloned(),
    )
}

impl GeckoClient {
    /// `base` = API base (`…/api/v2`).
    pub(crate) fn new(
        http: reqwest::Client,
        base: &str,
        scope: ToolScope,
        budget: Option<RateLimitConfig>,
    ) -> Result<Self> {
        let base = Url::parse(base.trim().trim_end_matches('/'))
            .map_err(|e| anyhow!("GeckoTerminal base is not a URL ({e})"))?;
        let host = base
            .host_str()
            .filter(|h| !h.is_empty())
            .ok_or_else(|| anyhow!("GeckoTerminal base has no host"))?
            .to_string();
        Ok(Self {
            http,
            base,
            host,
            scope,
            budget,
            timeout: TIMEOUT,
            max_budget_wait: MAX_BUDGET_WAIT,
            limiters: limiters(),
        })
    }

    #[cfg(test)]
    fn with_limiters(mut self, limiters: &'static Limiters) -> Self {
        self.limiters = limiters;
        self
    }

    /// The request URL of one page.
    fn url(
        &self,
        network: &str,
        pool: &str,
        interval: Interval,
        before_s: i64,
        token: &str,
    ) -> Result<Url> {
        let (tf, aggregate) = timeframe(interval);
        let mut url = self.base.clone();
        url.path_segments_mut()
            .map_err(|_| anyhow!("GeckoTerminal base cannot take a path"))?
            .pop_if_empty()
            .extend(["networks", network, "pools", pool, "ohlcv", tf]);
        url.query_pairs_mut()
            .append_pair("aggregate", &aggregate.to_string())
            .append_pair("before_timestamp", &before_s.to_string())
            .append_pair("limit", &PAGE_LIMIT.to_string())
            .append_pair("currency", "usd")
            .append_pair("token", token);
        Ok(url)
    }

    /// One page: the bars before `before_s` (seconds), ascending (module
    /// table). Errors carry an [`HttpError`].
    pub(crate) async fn ohlcv(
        &self,
        network: &str,
        pool: &str,
        interval: Interval,
        before_s: i64,
        token: &str,
    ) -> Result<Vec<Bar>> {
        let url = self.url(network, pool, interval, before_s, token)?;
        // The public URL carries no secret: errors show it in full, ids
        // intact (`for_api` would redact the `token` query value).
        let mut bare = url.clone();
        bare.set_query(None);
        let scrub = Scrubber::for_api(&bare);
        let gate = egress::policy()
            .check_url(&url)
            .and_then(|_| self.scope.check_net_host(&self.host));
        if let Err(e) = gate {
            let err = HttpError::new(ErrorClass::Fatal, format!("{e:#}"));
            self.audit(&url, Err(&err), None);
            return Err(err.into());
        }
        let budget = self.budget.as_ref();
        if let Err(err) = self
            .limiters
            .acquire(RATE_LIMIT, budget, 1, Priority::Read, self.max_budget_wait)
            .await
        {
            self.audit(&url, Err(&err), None);
            return Err(err.into());
        }
        let started = Instant::now();
        let sent = self
            .http
            .get(url.clone())
            .timeout(self.timeout)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await;
        let result = match sent {
            Err(e) => Err(reqwest_error(e, &self.host, &scrub)),
            Ok(resp) => {
                let status = resp.status().as_u16();
                let retry_after = retry_after_ms(resp.headers());
                match resp.text().await {
                    Err(e) => Err(reqwest_error(e, &self.host, &scrub)),
                    Ok(body) if (200..300).contains(&status) => {
                        decode(&body, interval, &self.host).map(|bars| (status, bars))
                    }
                    Ok(body) => Err(http_status_error(
                        status,
                        retry_after,
                        &body,
                        &self.host,
                        &scrub,
                    )),
                }
            }
        };
        let ms = Some(started.elapsed().as_millis() as u64);
        match result {
            Ok((status, bars)) => {
                self.audit(&url, Ok(status), ms);
                Ok(bars)
            }
            Err(err) => {
                if err.class == ErrorClass::RateLimited {
                    self.limiters
                        .penalize(RATE_LIMIT, budget, err.retry_after_ms);
                }
                self.audit(&url, Err(&err), ms);
                Err(err.into())
            }
        }
    }

    fn audit(&self, url: &Url, outcome: std::result::Result<u16, &HttpError>, ms: Option<u64>) {
        let mut event = json!({
            "tool": "gecko_ohlcv",
            "host": self.host,
            "path": url.path(),
            "ms": ms,
        });
        match outcome {
            Ok(status) => {
                event["verdict"] = "allowed".into();
                event["status"] = status.into();
            }
            Err(e) => {
                event["verdict"] = if ms.is_some() { "error" } else { "denied" }.into();
                event["class"] = e.class.as_str().into();
                event["reason"] = e.message.clone().into();
                if let Some(s) = e.http_status {
                    event["status"] = s.into();
                }
            }
        }
        egress::policy().audit(event);
    }
}

/// A 2xx body → bars; anything else `Decode`.
fn decode(body: &str, interval: Interval, host: &str) -> std::result::Result<Vec<Bar>, HttpError> {
    let decode_err = |m: String| HttpError::new(ErrorClass::Decode, format!("{host}: {m}"));
    let v: Value = serde_json::from_str(body.trim())
        .map_err(|e| decode_err(format!("the reply is not JSON ({e})")))?;
    gecko_ohlcv(&v, interval).map_err(decode_err)
}

/// What to fetch for one instrument.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct GeckoPlan {
    /// Full id: `solana:<mint>`, `robinhood:<token address>`.
    pub instrument: String,
    /// Pool address, verbatim.
    pub pool: String,
    pub interval: Interval,
    pub from_ms: i64,
    /// Exclusive; capped at the fetch time.
    pub to_ms: i64,
}

/// `gecko:<network>:<pool>`.
pub(crate) fn gecko_source(network: &str, pool: &str) -> String {
    format!("gecko:{network}:{pool}")
}

/// Bars of `plan` → `bars` (module table). Never fails: errors land in the
/// row.
pub(crate) async fn gecko_bars(
    client: &GeckoClient,
    store: &dyn MarketDataStore,
    plan: &GeckoPlan,
    retry: &Retry,
    now_ms: i64,
) -> ReportRow {
    let id = InstrumentId::parse(&plan.instrument);
    let network = id
        .as_ref()
        .map_err(Clone::clone)
        .and_then(|id| network_of(id.venue()));
    let source = gecko_source(network.as_deref().unwrap_or("?"), &plan.pool);
    let mut row = ReportRow::new(&plan.instrument, "bars", Some(plan.interval), &source);
    let fetched = match (id, network) {
        (Ok(id), Ok(network)) => {
            fetch(
                &mut row,
                client,
                store,
                plan,
                network,
                id.native(),
                retry,
                now_ms,
            )
            .await
        }
        (Err(e), _) | (_, Err(e)) => Err(anyhow!(e)),
    };
    if let Err(e) = fetched {
        row.errors.push(format!("{e:#}"));
    }
    row
}

#[allow(clippy::too_many_arguments)]
async fn fetch(
    row: &mut ReportRow,
    client: &GeckoClient,
    store: &dyn MarketDataStore,
    plan: &GeckoPlan,
    network: &str,
    token: &str,
    retry: &Retry,
    now_ms: i64,
) -> Result<()> {
    if plan.pool.is_empty() || plan.pool.contains(&['/', '?', '#'][..]) {
        return Err(anyhow!("pool `{}` is not a pool address", plan.pool));
    }
    let (interval, iv) = (plan.interval, plan.interval.ms());
    let from = grid_ceil(plan.from_ms, iv);
    // Only closed bars: the open one (from grid_floor(now)) is never asked for.
    let to = plan.to_ms.min(grid_floor(now_ms, iv));
    if let Some(note) = no_bars_note(from, to, interval) {
        row.notes.push(note);
        return Ok(());
    }
    let stored = stored_span(store, &plan.instrument, "bars", Some(interval)).await?;
    let ranges = missing_ranges(from, to, stored, iv, iv);
    row.notes.extend(resume_note(stored, &ranges));
    for (a, b) in ranges {
        // Bars with t_open < b: before the first second at or after b.
        let mut before_s = b.saturating_add(999).div_euclid(1000);
        loop {
            row.reads += 1;
            let page = retry
                .run("ohlcv", || {
                    client.ohlcv(network, &plan.pool, interval, before_s, token)
                })
                .await?;
            let Some(oldest) = page.first().map(|b| b.t_open_ms) else {
                break;
            };
            let keep: Vec<Bar> = closed_bars(page, interval, now_ms)
                .into_iter()
                .filter(|bar| (a..b).contains(&bar.t_open_ms))
                .collect();
            let n = store
                .put_bars(&plan.instrument, interval, &row.source, &keep)
                .await?;
            row.wrote(n, keep.iter().map(|bar| bar.t_open_ms));
            let next = oldest.div_euclid(1000);
            if oldest <= a || next >= before_s {
                break;
            }
            before_s = next;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::outbound::backfill::test_support::FAST;
    use crate::adapters::outbound::http_class::read_error;
    use crate::adapters::outbound::http_class::test_support::{
        canned, local_scope, serve, test_client, Canned,
    };
    use crate::adapters::outbound::market_data::SqliteMarketData;

    const H: i64 = 3_600_000;
    const MINT: &str = "So11111111111111111111111111111111111111112";
    const POOL: &str = "8sLbNZoA1cfnvMJLPfp98ZLAnFSYCFApfJKMbiXNLwxj";

    fn own_limiters() -> &'static Limiters {
        Box::leak(Box::new(Limiters::default()))
    }

    fn client(base: &str, budget: Option<RateLimitConfig>) -> GeckoClient {
        GeckoClient::new(test_client(), base, local_scope(), budget)
            .unwrap()
            .with_limiters(own_limiters())
    }

    /// A reply with bars opening at `hours` (newest first, as Gecko sends).
    fn page(hours: &[i64]) -> String {
        let list: Vec<Value> = hours
            .iter()
            .rev()
            .map(|h| json!([h * 3600, 150.0, 151.0, 149.0, 150.5, 1000.0]))
            .collect();
        json!({"data": {"id": "x", "type": "ohlcv_request_response",
            "attributes": {"ohlcv_list": list}}, "meta": {}})
        .to_string()
    }

    fn query(req: &str) -> Vec<(String, String)> {
        let line = req.lines().next().unwrap();
        let target = line.split_whitespace().nth(1).unwrap();
        let url = Url::parse(&format!("http://x{target}")).unwrap();
        url.query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect()
    }

    fn path(req: &str) -> String {
        let target = req
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap();
        target.split('?').next().unwrap().to_string()
    }

    #[tokio::test]
    async fn pages_backwards_until_from_and_drops_the_open_bar() {
        // now = 10:30: the 10:00 bar is open. from = 03:00.
        let now = 10 * H + H / 2;
        let (base, seen) = serve(vec![
            canned(200, page(&[7, 8, 9, 10])),
            Canned {
                status: 429,
                headers: "Retry-After: 0\r\n",
                body: "Throttled".into(),
                delay_ms: 0,
            },
            canned(200, page(&[2, 3, 4, 5, 6])),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteMarketData::open(dir.path()).unwrap();
        let plan = GeckoPlan {
            instrument: format!("solana:{MINT}"),
            pool: POOL.into(),
            interval: Interval::H1,
            from_ms: 3 * H,
            to_ms: i64::MAX,
        };
        let c = client(&format!("{base}/api/v2"), None);
        let row = gecko_bars(&c, &store, &plan, &FAST, now).await;
        assert!(row.errors.is_empty(), "{row:?}");
        assert_eq!(row.source, format!("gecko:solana:{POOL}"));
        assert_eq!((row.rows, row.reads), (7, 2), "03:00 … 09:00");
        assert_eq!((row.first_ms, row.last_ms), (Some(3 * H), Some(9 * H)));

        let reqs = seen.lock().unwrap().clone();
        assert_eq!(reqs.len(), 3, "the 429 was retried");
        assert_eq!(
            path(&reqs[0]),
            format!("/api/v2/networks/solana/pools/{POOL}/ohlcv/hour")
        );
        assert!(reqs[0]
            .to_ascii_lowercase()
            .contains("accept: application/json"));
        let q = query(&reqs[0]);
        let want = |k: &str, v: String| {
            assert!(q.contains(&(k.to_string(), v.clone())), "{k}={v} in {q:?}")
        };
        want("aggregate", "1".into());
        // Up to the open 10:00 bar, exclusive.
        want("before_timestamp", (10 * H / 1000).to_string());
        want("limit", "1000".into());
        want("currency", "usd".into());
        want("token", MINT.into());
        // Page 2 starts before the oldest bar of page 1 (07:00).
        assert!(query(&reqs[2]).contains(&("before_timestamp".into(), (7 * 3600).to_string())));

        let s = store
            .bars(&format!("solana:{MINT}"), Interval::H1, 0, i64::MAX)
            .await
            .unwrap();
        let hours: Vec<i64> = s.bars.iter().map(|b| b.t_open_ms / H).collect();
        assert_eq!(hours, vec![3, 4, 5, 6, 7, 8, 9]);

        // A re-run an hour later: only the new tail, one page, no head.
        let (base, seen) = serve(vec![canned(200, page(&[8, 9, 10, 11]))]).await;
        let c = client(&format!("{base}/api/v2"), None);
        let row = gecko_bars(&c, &store, &plan, &FAST, now + H).await;
        assert_eq!((row.rows, row.last_ms), (1, Some(10 * H)), "{row:?}");
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert!(
            row.notes[0].contains("fetching only 1970-01-01T10:00:00Z"),
            "{:?}",
            row.notes
        );
    }

    #[tokio::test]
    async fn timeframes_errors_and_refusals() {
        assert_eq!(timeframe(Interval::M15), ("minute", 15));
        assert_eq!(timeframe(Interval::H4), ("hour", 4));
        assert_eq!(timeframe(Interval::D1), ("day", 1));
        assert_eq!(network_of("robinhood"), Ok("robinhood"));
        assert!(network_of("hyperliquid")
            .unwrap_err()
            .contains("hyperliquid"));

        let (base, seen) = serve(vec![
            canned(404, r#"{"errors":[{"status":"404","title":"Not Found"}]}"#),
            canned(200, "<html>maintenance</html>"),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteMarketData::open(dir.path()).unwrap();
        let c = client(&base, None);
        let plan = |instrument: String| GeckoPlan {
            instrument,
            pool: POOL.into(),
            interval: Interval::H1,
            from_ms: 0,
            to_ms: 10 * H,
        };
        let row = gecko_bars(&c, &store, &plan(format!("solana:{MINT}")), &FAST, 20 * H).await;
        assert!(
            row.errors[0].contains("HTTP 404 from 127.0.0.1"),
            "{:?}",
            row.errors
        );
        assert_eq!(row.reads, 1, "fatal: no retry");
        let row = gecko_bars(&c, &store, &plan(format!("solana:{MINT}")), &FAST, 20 * H).await;
        assert!(row.errors[0].contains("not JSON"), "{:?}", row.errors);
        assert_eq!(seen.lock().unwrap().len(), 2);

        // No network for the venue, a bad pool: refused before any request.
        let row = gecko_bars(&c, &store, &plan("hyperliquid:SOL".into()), &FAST, 20 * H).await;
        assert!(
            row.errors[0].contains("no GeckoTerminal network"),
            "{:?}",
            row.errors
        );
        let bad_pool = GeckoPlan {
            pool: "a/b".into(),
            ..plan(format!("solana:{MINT}"))
        };
        let row = gecko_bars(&c, &store, &bad_pool, &FAST, 20 * H).await;
        assert!(
            row.errors[0].contains("not a pool address"),
            "{:?}",
            row.errors
        );

        // Scope denial: nothing sent.
        let denied = GeckoClient::new(test_client(), &base, ToolScope::default(), None)
            .unwrap()
            .with_limiters(own_limiters());
        let e = denied
            .ohlcv("solana", POOL, Interval::H1, 3600, MINT)
            .await
            .unwrap_err();
        assert_eq!(read_error("x", &e).class, ErrorClass::Fatal);
        assert!(format!("{e:#}").contains("net_hosts"), "{e:#}");
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn the_budget_is_taken_before_sending() {
        let (base, seen) = serve(vec![canned(200, page(&[1]))]).await;
        // 1 request / minute, bucket of 1: the second read is refused unsent.
        let budget = RateLimitConfig {
            per_minute: 1,
            burst: Some(1),
            exec_reserve: 0,
        };
        let mut c = client(&base, Some(budget));
        c.max_budget_wait = Duration::ZERO;
        c.ohlcv("solana", POOL, Interval::H1, 7200, MINT)
            .await
            .unwrap();
        let e = c
            .ohlcv("solana", POOL, Interval::H1, 7200, MINT)
            .await
            .unwrap_err();
        let r = read_error("x", &e);
        assert_eq!(r.class, ErrorClass::RateLimited);
        assert!(
            r.message.contains("[rate_limits.geckoterminal]"),
            "{}",
            r.message
        );
        assert_eq!(seen.lock().unwrap().len(), 1);
    }
}
