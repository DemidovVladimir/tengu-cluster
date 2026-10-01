//! HL history → `market.db` through [`HlInfo`] (egress gate + audit
//! `hl_info`, `[rate_limits.hyperliquid]` budget, HL error mapping), each
//! request retried per `Retry`.
//!
//! | Request | Paging | Rule |
//! |---|---|---|
//! | `{"type":"candleSnapshot","req":{"coin","interval","startTime","endTime"}}` | forward, windows of ≤ 5 000 bars | HL serves only the newest 5 000 bars per interval: a start before `now − 4 999 × interval` (the open bar counts) is clamped there and the report says so; the still-open last bar is dropped; weight 20 + 1 per 60 bars |
//! | `{"type":"fundingHistory","coin","startTime","endTime"}` | `startTime = last.time + 1` until a reply is empty or past the end | ≤ 500 rows per reply; times stored verbatim; weight 20 + 1 per 20 rows |
//!
//! | Coin | Instrument |
//! |---|---|
//! | `xyz:TSLA`, `SOL` | `hyperliquid:xyz:TSLA`, `hyperliquid:SOL` (`domain::marketdata_decode::hl_coin`) |

use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use reqwest::Url;
use serde_json::json;

use super::{
    grid_ceil, grid_floor, missing_ranges, no_bars_note, resume_note, stored_span, ReportRow, Retry,
};
use crate::adapters::outbound::egress;
use crate::adapters::outbound::hyperliquid::info::{
    api_url, HlInfo, InfoReply, API_URL_ENV, MAINNET_URL, RATE_LIMIT,
};
use crate::config::sections::SandboxSections;
use crate::domain::marketdata::{fmt_time, Bar, Interval};
use crate::domain::marketdata_decode::{closed_bars, hl_candles, hl_coin, hl_funding};
use crate::domain::scope::ToolScope;
use crate::ports::market_data::MarketDataStore;

/// HL keeps this many bars per interval (the newest).
pub(crate) const HL_MAX_BARS: i64 = 5_000;
/// HL settles funding hourly, a few ms past the hour: a `from` less than an
/// hour before the first stored row has no missing head.
const FUNDING_HEAD_MIN_MS: i64 = 3_600_000;
const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_BUDGET_WAIT: Duration = Duration::from_secs(120);

/// An [`HlInfo`] for an operator command (`tengu history backfill`): the API
/// base of `api_url` (`$HL_API_URL` honoured), a scope that pins that host
/// — the `[egress]` policy stays the ceiling — the sandbox's
/// `[rate_limits.hyperliquid]`, a 30 s timeout and budget waits ≤ 120 s.
pub(crate) fn operator_hl(sections: &SandboxSections) -> Result<HlInfo> {
    let base = api_url(&ToolScope {
        env_reads: vec![API_URL_ENV.to_string()],
        ..Default::default()
    });
    // The value is never echoed: a provider URL may carry a key.
    let host = Url::parse(&base)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .ok_or_else(|| anyhow!("{API_URL_ENV} is not a URL with a host"))?;
    let scope = ToolScope {
        net_hosts: vec![host],
        ..Default::default()
    };
    let http = egress::policy().tool_client(TIMEOUT)?;
    Ok(HlInfo::new(
        http,
        &base,
        scope,
        sections.rate_limits.get(RATE_LIMIT).cloned(),
    )?
    .with_timeout(TIMEOUT)
    .with_max_budget_wait(MAX_BUDGET_WAIT))
}

/// The `source` of rows from `host`: `hl` on mainnet, else `hl:<host>` — a
/// testnet backfill is never mistaken for mainnet history.
pub(crate) fn hl_source(host: &str) -> String {
    let mainnet = Url::parse(MAINNET_URL)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string));
    if mainnet.as_deref() == Some(host) {
        "hl".to_string()
    } else {
        format!("hl:{host}")
    }
}

/// What to fetch for one instrument.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HlPlan {
    /// Full id, `hyperliquid:<coin>`.
    pub instrument: String,
    pub interval: Interval,
    pub from_ms: i64,
    /// Exclusive; capped at the fetch time.
    pub to_ms: i64,
}

/// The first bar HL still serves at `now_ms`: the newest 5 000 bars include
/// the open one.
pub(crate) fn hl_reach_ms(interval: Interval, now_ms: i64) -> i64 {
    let iv = interval.ms();
    grid_floor(now_ms, iv) - (HL_MAX_BARS - 1) * iv
}

/// Bars of `plan` → `bars` (module table). Never fails: errors land in the
/// row.
pub(crate) async fn hl_bars(
    hl: &HlInfo,
    store: &dyn MarketDataStore,
    plan: &HlPlan,
    retry: &Retry,
    now_ms: i64,
) -> ReportRow {
    let mut row = ReportRow::new(
        &plan.instrument,
        "bars",
        Some(plan.interval),
        &hl_source(hl.host()),
    );
    if let Err(e) = fetch_bars(&mut row, hl, store, plan, retry, now_ms).await {
        row.errors.push(format!("{e:#}"));
    }
    row
}

async fn fetch_bars(
    row: &mut ReportRow,
    hl: &HlInfo,
    store: &dyn MarketDataStore,
    plan: &HlPlan,
    retry: &Retry,
    now_ms: i64,
) -> Result<()> {
    let coin = hl_coin(&plan.instrument).map_err(|e| anyhow!(e))?;
    let (interval, iv) = (plan.interval, plan.interval.ms());
    let mut from = grid_ceil(plan.from_ms, iv);
    let reach = hl_reach_ms(interval, now_ms);
    if from < reach {
        row.notes.push(format!(
            "HL serves only the newest {HL_MAX_BARS} {interval} bars: start clamped from {} to {}",
            fmt_time(from),
            fmt_time(reach)
        ));
        from = reach;
    }
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
        let mut w = a;
        while w < b {
            let w_end = w.saturating_add(HL_MAX_BARS * iv).min(b);
            let body = json!({"type": "candleSnapshot", "req": {
                "coin": coin, "interval": interval.as_str(), "startTime": w, "endTime": w_end - 1}});
            row.reads += 1;
            let bars = match retry.run("candles", || hl.post(&body)).await? {
                InfoReply::Json(v) => hl_candles(&v, &coin, interval).map_err(|e| anyhow!(e))?,
                InfoReply::Null => bail!("HL does not know coin {coin} (200 null)"),
            };
            let bars: Vec<Bar> = closed_bars(bars, interval, now_ms)
                .into_iter()
                .filter(|b| (w..w_end).contains(&b.t_open_ms))
                .collect();
            let n = store
                .put_bars(&plan.instrument, interval, &row.source, &bars)
                .await?;
            row.wrote(n, bars.iter().map(|b| b.t_open_ms));
            w = w_end;
        }
    }
    Ok(())
}

/// Funding of `plan` (its interval unused) → `funding` (module table).
/// Never fails: errors land in the row.
pub(crate) async fn hl_funding_history(
    hl: &HlInfo,
    store: &dyn MarketDataStore,
    plan: &HlPlan,
    retry: &Retry,
    now_ms: i64,
) -> ReportRow {
    let mut row = ReportRow::new(&plan.instrument, "funding", None, &hl_source(hl.host()));
    if let Err(e) = fetch_funding(&mut row, hl, store, plan, retry, now_ms).await {
        row.errors.push(format!("{e:#}"));
    }
    row
}

async fn fetch_funding(
    row: &mut ReportRow,
    hl: &HlInfo,
    store: &dyn MarketDataStore,
    plan: &HlPlan,
    retry: &Retry,
    now_ms: i64,
) -> Result<()> {
    let coin = hl_coin(&plan.instrument).map_err(|e| anyhow!(e))?;
    let to = plan.to_ms.min(now_ms.saturating_add(1));
    let stored = stored_span(store, &plan.instrument, "funding", None).await?;
    let ranges = missing_ranges(plan.from_ms, to, stored, 1, FUNDING_HEAD_MIN_MS);
    row.notes.extend(resume_note(stored, &ranges));
    for (a, b) in ranges {
        let mut start = a;
        loop {
            let body = json!({"type": "fundingHistory", "coin": coin,
                "startTime": start, "endTime": b - 1});
            row.reads += 1;
            let points = match retry.run("funding", || hl.post(&body)).await? {
                InfoReply::Json(v) => hl_funding(&v, &coin).map_err(|e| anyhow!(e))?,
                InfoReply::Null => bail!("HL does not know coin {coin} (200 null)"),
            };
            let Some(last) = points.last().map(|p| p.t_ms) else {
                break;
            };
            let keep: Vec<_> = points
                .into_iter()
                .filter(|p| (a..b).contains(&p.t_ms))
                .collect();
            let n = store
                .put_funding(&plan.instrument, &row.source, &keep)
                .await?;
            row.wrote(n, keep.iter().map(|p| p.t_ms));
            if last.saturating_add(1) >= b || last < start {
                break;
            }
            start = last + 1;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::outbound::backfill::test_support::FAST;
    use crate::adapters::outbound::http_class::test_support::{
        canned, local_scope, serve, test_client,
    };
    use crate::adapters::outbound::market_data::SqliteMarketData;
    use serde_json::Value;

    const H: i64 = 3_600_000;
    const TSLA: &str = "hyperliquid:xyz:TSLA";

    fn fixture() -> String {
        let path = format!(
            "{}/tests/fixtures/hyperliquid/candleSnapshot_xyz_TSLA_1h.json",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    fn body_of(req: &str) -> Value {
        serde_json::from_str(req.split("\r\n\r\n").nth(1).unwrap()).unwrap()
    }

    fn hl(base: &str) -> HlInfo {
        HlInfo::new(test_client(), base, local_scope(), None).unwrap()
    }

    #[tokio::test]
    async fn candles_page_store_and_drop_the_open_bar() {
        let candles: Value = serde_json::from_str(&fixture()).unwrap();
        let rows = candles.as_array().unwrap();
        let (first, last) = (
            rows[0]["t"].as_i64().unwrap(),
            rows[66]["t"].as_i64().unwrap(),
        );
        // Fetched while the last bar is open; a 502 first (retried).
        let now = last + H / 2;
        let (base, seen) = serve(vec![canned(502, "bad gateway"), canned(200, fixture())]).await;
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteMarketData::open(dir.path()).unwrap();
        let plan = HlPlan {
            instrument: TSLA.into(),
            interval: Interval::H1,
            from_ms: first,
            to_ms: i64::MAX,
        };
        let row = hl_bars(&hl(&base), &store, &plan, &FAST, now).await;
        assert!(row.errors.is_empty(), "{row:?}");
        assert_eq!((row.rows, row.reads), (66, 1));
        assert_eq!((row.first_ms, row.last_ms), (Some(first), Some(last - H)));
        assert_eq!(row.source, "hl:127.0.0.1");
        assert!(row.notes.is_empty(), "{:?}", row.notes);
        let reqs = seen.lock().unwrap().clone();
        assert_eq!(reqs.len(), 2, "one retry");
        // Asked up to the open bar, exclusive; its row in the reply is dropped.
        assert_eq!(
            body_of(&reqs[1]),
            json!({"type": "candleSnapshot", "req": {"coin": "xyz:TSLA", "interval": "1h",
                "startTime": first, "endTime": last - 1}})
        );
        let stored = store.bars(TSLA, Interval::H1, 0, i64::MAX).await.unwrap();
        assert_eq!(stored.bars.len(), 66);
        assert!(
            stored.bars.iter().all(|b| b.t_open_ms < last),
            "open bar not stored"
        );

        // A re-run an hour later fetches only the tail: from the open bar on.
        let (base, seen) = serve(vec![canned(200, fixture())]).await;
        let row = hl_bars(&hl(&base), &store, &plan, &FAST, last + H).await;
        assert_eq!((row.rows, row.last_ms), (1, Some(last)), "{row:?}");
        assert_eq!(body_of(&seen.lock().unwrap()[0])["req"]["startTime"], last);
        assert!(
            row.notes[0].starts_with("stored 2026-09-25T20:00:00Z"),
            "{:?}",
            row.notes
        );
        // Up to date — also within the next bar's hour: no request at all.
        let (base, seen) = serve(vec![]).await;
        let row = hl_bars(&hl(&base), &store, &plan, &FAST, last + 2 * H - 1).await;
        assert_eq!((row.rows, row.reads), (0, 0));
        assert!(row.notes[0].contains("up to date"), "{:?}", row.notes);
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_old_start_is_clamped_to_the_newest_5000_bars() {
        let now = 20_000 * H + 1_000;
        let (base, seen) = serve(vec![canned(200, "[]")]).await;
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteMarketData::open(dir.path()).unwrap();
        let plan = HlPlan {
            instrument: TSLA.into(),
            interval: Interval::H1,
            from_ms: 0,
            to_ms: i64::MAX,
        };
        let row = hl_bars(&hl(&base), &store, &plan, &FAST, now).await;
        assert!(row.errors.is_empty(), "{row:?}");
        // The open bar [20 000 H, 20 001 H) is the newest of the 5 000.
        let reach = 15_001 * H;
        assert_eq!(hl_reach_ms(Interval::H1, now), reach);
        assert_eq!(
            row.notes,
            vec![format!(
                "HL serves only the newest 5000 1h bars: start clamped from 1970-01-01T00:00:00Z to {}",
                fmt_time(reach)
            )]
        );
        let reqs = seen.lock().unwrap().clone();
        assert_eq!(reqs.len(), 1, "the reachable range is one window");
        let req = body_of(&reqs[0])["req"].clone();
        assert_eq!(
            (req["startTime"].as_i64(), req["endTime"].as_i64()),
            (Some(reach), Some(20_000 * H - 1)),
            "the open bar is not asked for"
        );
        // A range HL no longer serves: noted, nothing asked.
        let (base, seen) = serve(vec![]).await;
        let gone = HlPlan {
            to_ms: 10_000 * H,
            ..plan.clone()
        };
        let row = hl_bars(&hl(&base), &store, &gone, &FAST, now).await;
        assert!(row.errors.is_empty(), "{row:?}");
        assert!(
            row.notes[1].starts_with("nothing to fetch: no closed 1h bar"),
            "{:?}",
            row.notes
        );
        assert!(seen.lock().unwrap().is_empty());
        // A later start than the reach is kept, on the bar grid.
        let (base, seen) = serve(vec![canned(200, "[]")]).await;
        let later = HlPlan {
            from_ms: 19_000 * H + 5,
            ..plan
        };
        let row = hl_bars(&hl(&base), &store, &later, &FAST, now).await;
        assert!(row.notes.is_empty(), "{:?}", row.notes);
        assert_eq!(
            body_of(&seen.lock().unwrap()[0])["req"]["startTime"],
            19_001 * H
        );
    }

    #[tokio::test]
    async fn funding_pages_until_empty_and_resumes_after_the_last_row() {
        let page1 = json!([
            {"coin": "xyz:TSLA", "fundingRate": "0.0000125", "premium": "0.0001", "time": H + 3},
            {"coin": "xyz:TSLA", "fundingRate": "-0.00001", "premium": "-0.0001", "time": 2 * H + 3},
        ]);
        let page2 = json!([{"coin": "xyz:TSLA", "fundingRate": "0.00002", "time": 3 * H + 2}]);
        let (base, seen) = serve(vec![
            canned(200, page1.to_string()),
            canned(200, page2.to_string()),
            canned(200, "[]"),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteMarketData::open(dir.path()).unwrap();
        let plan = HlPlan {
            instrument: TSLA.into(),
            interval: Interval::H1,
            from_ms: H,
            to_ms: 10 * H,
        };
        let row = hl_funding_history(&hl(&base), &store, &plan, &FAST, 20 * H).await;
        assert!(row.errors.is_empty(), "{row:?}");
        assert_eq!((row.rows, row.reads), (3, 3));
        assert_eq!((row.first_ms, row.last_ms), (Some(H + 3), Some(3 * H + 2)));
        let starts: Vec<i64> = seen
            .lock()
            .unwrap()
            .iter()
            .map(|r| body_of(r)["startTime"].as_i64().unwrap())
            .collect();
        assert_eq!(starts, vec![H, 2 * H + 4, 3 * H + 3], "last.time + 1");
        assert_eq!(body_of(&seen.lock().unwrap()[0])["endTime"], 10 * H - 1);
        let f = store.funding(TSLA, 0, i64::MAX).await.unwrap();
        assert_eq!(f.points.len(), 3);
        assert_eq!(f.points[2].premium, None);

        // Resume: after the last stored time only (the first row is 3 ms
        // past `from`: no head).
        let (base, seen) = serve(vec![canned(200, "[]")]).await;
        let row = hl_funding_history(&hl(&base), &store, &plan, &FAST, 20 * H).await;
        assert_eq!((row.rows, row.reads), (0, 1));
        assert_eq!(body_of(&seen.lock().unwrap()[0])["startTime"], 3 * H + 3);
    }

    #[tokio::test]
    async fn hl_errors_name_the_instrument_and_stop_it() {
        let bad = json!([{"t": 0, "s": "xyz:TSLA", "i": "1h", "o": "0", "h": "1", "l": "1", "c": "1", "v": "1"}]);
        let (base, _) = serve(vec![
            canned(500, "null"),
            canned(200, "null"),
            canned(200, bad.to_string()),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteMarketData::open(dir.path()).unwrap();
        let plan = |id: &str| HlPlan {
            instrument: id.into(),
            interval: Interval::H1,
            from_ms: 0,
            to_ms: 10 * H,
        };
        let hl = hl(&base);
        let row = hl_bars(&hl, &store, &plan("hyperliquid:xyz:NOPE"), &FAST, 20 * H).await;
        assert!(
            row.errors[0].contains("unknown dex or coin"),
            "{:?}",
            row.errors
        );
        assert_eq!(row.reads, 1, "not_applicable is not retried");
        let row = hl_funding_history(&hl, &store, &plan(TSLA), &FAST, 20 * H).await;
        assert!(
            row.errors[0].contains("HL does not know coin xyz:TSLA"),
            "{:?}",
            row.errors
        );
        let row = hl_bars(&hl, &store, &plan(TSLA), &FAST, 20 * H).await;
        assert!(
            row.errors[0].contains("candleSnapshot xyz:TSLA 1h row 0 (1970-01-01T00:00:00Z)"),
            "{:?}",
            row.errors
        );
        assert_eq!(row.rows, 0, "a bad page is not written");
        let row = hl_bars(
            &hl,
            &store,
            &plan("solana:So11111111111111111111111111111111111111112"),
            &FAST,
            20 * H,
        )
        .await;
        assert!(
            row.errors[0].contains("not a Hyperliquid instrument"),
            "{:?}",
            row.errors
        );
    }

    #[test]
    fn sources_name_a_non_mainnet_host() {
        assert_eq!(hl_source("api.hyperliquid.xyz"), "hl");
        assert_eq!(
            hl_source("api.hyperliquid-testnet.xyz"),
            "hl:api.hyperliquid-testnet.xyz"
        );
    }
}
