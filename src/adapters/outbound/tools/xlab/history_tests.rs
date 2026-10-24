//! Tests of `src/adapters/outbound/tools/xlab/history.rs`.

use super::*;
use crate::adapters::outbound::http_class::test_support::FAST;
use crate::adapters::outbound::http_class::test_support::{
    canned, local_scope, serve, test_client,
};
use crate::adapters::outbound::market_data::SqliteMarketData;
use crate::adapters::outbound::tools::hyperliquid::test_support::{hl, serve_info, Route};
use crate::adapters::outbound::tools::workspace::test_support::TestHarness;
use crate::domain::marketdata::{Bar, FundingPoint};
use crate::domain::observation::assert_features_ok;
use crate::domain::scope::ToolScope;
use crate::domain::token::tool_result_char_budget;
use serde_json::json;

const H: i64 = 3_600_000;
const TSLA: &str = "hyperliquid:xyz:TSLA";
const KIOXIA: &str = "hyperliquid:xyz:KIOXIA";
const MINT: &str = "So11111111111111111111111111111111111111112";
/// A sandbox without `[backtest.splits]`.
const NO_SPLITS: BTreeMap<String, Vec<StockSplit>> = BTreeMap::new();
const POOL: &str = "8sLbNZoA1cfnvMJLPfp98ZLAnFSYCFApfJKMbiXNLwxj";
/// The candle fixture: 67 hourly bars, 2026-09-25T20:00Z … 2026-09-28T14:00Z.
const FIRST: i64 = 1_790_366_400_000;
const LAST: i64 = 1_790_604_000_000;

fn fixture(name: &str) -> String {
    let path = format!(
        "{}/tests/fixtures/hyperliquid/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn bar(t: i64, c: f64) -> Bar {
    Bar {
        t_open_ms: t,
        o: c,
        h: c + 0.5,
        l: c - 0.5,
        c,
        v: 12.5,
        n: Some(4),
    }
}

#[test]
fn cells_keep_short_numbers_and_round_float_tails() {
    for (x, want) in [
        (372.33, "372.33"),
        (360.0, "360"),
        (1120.694, "1120.694"),
        (123.987_654_321_000_01, "123.98765"),
        (0.000_012_345_678_912_34, "0.000012345679"),
        (1_234_567.891_234, "1234567.9"),
        (0.0, "0"),
    ] {
        assert_eq!(cell(x), want, "{x}");
    }
}

fn args(v: Value) -> HistoryArgs {
    HistoryArgs::parse(&v, 100 * H).unwrap()
}

fn shared(market: SqliteMarketData) -> XlabShared {
    XlabShared {
        market: Ok(Arc::new(market)),
        store: None,
        sandbox: Arc::new(SandboxSections::default()),
    }
}

#[test]
fn arguments_parse_strictly_with_defaults() {
    let now = 100 * H;
    let a = args(json!({"instrument": TSLA}));
    assert_eq!(a.instrument.to_string(), TSLA);
    assert_eq!(
        (a.interval, a.from_ms, a.to_ms, a.fetch, a.points),
        (
            Interval::H1,
            now - DEFAULT_WINDOW_MS,
            now,
            false,
            DEFAULT_POINTS
        )
    );
    let a = args(
        json!({"instrument": TSLA, "interval": "4h", "from": "1970-01-02",
                            "to": 50 * H, "points": 200.0, "fetch": true}),
    );
    assert_eq!(
        (a.interval, a.from_ms, a.to_ms, a.points, a.fetch),
        (Interval::H4, 24 * H, 50 * H, 200, true)
    );
    // Only `to`: the 7 days before it.
    let a = args(json!({"instrument": TSLA, "to": "1970-01-04T00:00:00Z"}));
    assert_eq!(a.from_ms, 72 * H - DEFAULT_WINDOW_MS);
    let gecko = args(json!({"instrument": format!("solana:{MINT}"), "pool": POOL,
                                "fetch": true}));
    assert_eq!(gecko.pool.as_deref(), Some(POOL));

    for (v, needle) in [
        (json!([]), "must be a JSON object"),
        (json!({}), "'instrument' is required"),
        (
            json!({"instrument": "TSLA"}),
            "is not `<venue>:<native id>`",
        ),
        (
            json!({"instrument": TSLA, "coin": "x"}),
            "unknown argument(s) [\"coin\"]",
        ),
        (
            json!({"instrument": TSLA, "interval": "2h"}),
            "not one of 1m, 5m",
        ),
        (
            json!({"instrument": TSLA, "from": 5 * H, "to": 5 * H}),
            "is not before 'to'",
        ),
        (
            json!({"instrument": TSLA, "from": "friday"}),
            "'from': `friday` is not epoch ms",
        ),
        (
            json!({"instrument": TSLA, "to": true}),
            "'to' must be epoch ms",
        ),
        (json!({"instrument": TSLA, "points": 0}), "from 1 to 200"),
        (json!({"instrument": TSLA, "points": 201}), "from 1 to 200"),
        (json!({"instrument": TSLA, "points": 2.5}), "from 1 to 200"),
        (
            json!({"instrument": TSLA, "fetch": "yes"}),
            "'fetch' must be a boolean",
        ),
        (
            json!({"instrument": TSLA, "pool": POOL}),
            "for solana: / robinhood: ids only",
        ),
        (
            json!({"instrument": format!("solana:{MINT}"), "fetch": true}),
            "needs 'pool'",
        ),
        (
            json!({"instrument": "binance-spot:BTCUSDT", "fetch": true}),
            "no history source for venue `binance-spot`",
        ),
        (
            json!({"instrument": TSLA, "interval": "1m", "from": 0, "to": 100_001_i64 * 60_000}),
            "more than 100000",
        ),
    ] {
        let e = HistoryArgs::parse(&v, now).unwrap_err().to_string();
        assert!(e.starts_with("market_history: "), "{e}");
        assert!(e.contains(needle), "{v}: {e}");
    }
    // A DEX id without fetch reads what is stored: no pool needed.
    assert!(HistoryArgs::parse(&json!({"instrument": format!("solana:{MINT}")}), now).is_ok());
}

/// 10 hourly bars from 0 with one hole (5h) + 3 funding rows.
async fn seeded() -> (tempfile::TempDir, SqliteMarketData) {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteMarketData::open(dir.path()).unwrap();
    let bars: Vec<Bar> = (0..10)
        .filter(|h| *h != 5)
        .map(|h| bar(h * H, 100.0 + h as f64))
        .collect();
    store
        .put_bars(TSLA, Interval::H1, "hl", &bars)
        .await
        .unwrap();
    let p = |t: i64, r: f64| FundingPoint {
        t_ms: t + 54,
        rate_1h: r,
        premium: None,
    };
    store
        .put_funding(
            TSLA,
            "hl",
            &[p(H, 0.0000125), p(2 * H, 0.0000125), p(3 * H, 0.0000125)],
        )
        .await
        .unwrap();
    (dir, store)
}

#[tokio::test]
async fn a_read_reports_stats_a_sample_and_the_coverage() {
    let (_dir, store) = seeded().await;
    let req = args(json!({"instrument": TSLA, "from": 0, "to": 10 * H, "points": 4}));
    let row = read_history(&store, &req, None, &FAST, &NO_SPLITS, 100 * H).await;
    let obs = Observation::of(names::MARKET_HISTORY, &row, 100 * H, 0, ObsSource::Live);
    assert_eq!(obs.key, format!("mkt_history/1:{TSLA}:1h"));
    assert_eq!((obs.status, obs.ttl_ms), (ObsStatus::Ok, 0));
    assert_features_ok(&obs.features);
    let f = &obs.features;
    for (k, v) in [
        ("n_bars", 9),
        ("points", 4),
        ("gaps", 1),
        ("funding_points", 3),
    ] {
        assert_eq!(f[k], v, "{k}");
    }
    assert_eq!(f["last_close"], 109.0);
    assert!((f["funding_mean_apr_pct"].as_f64().unwrap() - 10.95).abs() < 1e-9);
    let ret = (109.0f64 / 100.0).ln() * 10_000.0;
    assert!((f["ret_bps"].as_f64().unwrap() - ret).abs() < 1e-9);
    assert!(!f.contains_key("fetched_bars"), "no fetch");
    // data: the sample (first and last kept) and every stored series.
    let back: MarketHistory = obs.typed().unwrap();
    let opens: Vec<i64> = back.bars.iter().map(|b| b.t_open_ms / H).collect();
    assert_eq!(opens, vec![0, 3, 6, 9]);
    let kinds: Vec<&str> = back.coverage.iter().map(|s| s.kind.as_str()).collect();
    assert_eq!(kinds, vec!["bars", "funding"]);

    let text = render(&obs, &row, 100 * H);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines[0],
        format!(
            "mkt_history {TSLA} 1h bars=9 1970-01-01T00:00:00Z … 1970-01-01T09:00:00Z \
                 last_close=109 ret_bps={ret:.1} | ok 0s live"
        )
    );
    assert!(lines[1].starts_with("avg_volume=12.5 "), "{}", lines[1]);
    assert!(
        text.contains("bars (4 of 9, evenly sampled, the first and last kept; t = bar open, UTC):"),
        "{text}"
    );
    assert!(
        text.contains("1970-01-01T09:00:00Z  109  109.5  108.5  109  12.5  4"),
        "{text}"
    );
    assert!(
        text.contains(
            "stored bars 1h: 1970-01-01T00:00:00Z … 1970-01-01T09:00:00Z rows=9 sources=hl"
        ),
        "{text}"
    );
    assert!(text.contains("stored funding: "), "{text}");
    assert!(
        !text.contains("t_open_ms"),
        "no data JSON in the text: {text}"
    );
}

#[tokio::test]
async fn an_empty_window_is_absent_and_says_how_to_fill_it() {
    let (_dir, store) = seeded().await;
    let req = args(json!({"instrument": TSLA, "from": 50 * H, "to": 60 * H}));
    let row = read_history(&store, &req, None, &FAST, &NO_SPLITS, 100 * H).await;
    let obs = Observation::of(names::MARKET_HISTORY, &row, 100 * H, 0, ObsSource::Live);
    assert_eq!(obs.status, ObsStatus::Absent);
    assert_eq!(obs.features["gaps"], 10);
    let text = render(&obs, &row, 100 * H);
    assert!(
        text.contains("bars=0 in 1970-01-03T02:00:00Z … 1970-01-03T12:00:00Z: none stored"),
        "{text}"
    );
    assert!(text.contains("fetch = true backfills it"), "{text}");
    // Another instrument: nothing stored at all.
    let req = args(json!({"instrument": "hyperliquid:SOL", "from": 0, "to": 10 * H}));
    let row = read_history(&store, &req, None, &FAST, &NO_SPLITS, 100 * H).await;
    let obs = Observation::of(names::MARKET_HISTORY, &row, 100 * H, 0, ObsSource::Live);
    assert!(render(&obs, &row, 100 * H).ends_with("stored: nothing for hyperliquid:SOL"));
}

/// The worst case for a local model: 200 points over 5 000 one-minute
/// bars of a long Solana mint — the text stays inside the 16k window's
/// cap (8 192 chars) with the table cut to 48 rows; data holds 200.
#[tokio::test]
async fn the_text_fits_a_local_models_result_cap() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteMarketData::open(dir.path()).unwrap();
    let id = format!("solana:{MINT}");
    let m = 60_000;
    let bars: Vec<Bar> = (0..5_000)
        .map(|i| Bar {
            t_open_ms: i * m,
            o: 123.456_789_012,
            h: 124.987_654_321,
            l: 122.123_456_789,
            c: 123.987_654_321 + i as f64 * 0.000_001,
            v: 1_234_567.891_234,
            n: None,
        })
        .collect();
    let source = format!("gecko:solana:{POOL}");
    store
        .put_bars(&id, Interval::M1, &source, &bars)
        .await
        .unwrap();
    let req = args(json!({"instrument": id, "interval": "1m", "from": 0,
                              "to": 5_000 * m, "points": 200}));
    let row = read_history(&store, &req, None, &FAST, &NO_SPLITS, 5_001 * m).await;
    let obs = Observation::of(names::MARKET_HISTORY, &row, 5_001 * m, 0, ObsSource::Live);
    let text = render(&obs, &row, 5_001 * m);
    let cap = tool_result_char_budget(16_384);
    assert!(
        text.len() < cap * 3 / 4,
        "{} chars of a {cap} cap:\n{text}",
        text.len()
    );
    assert!(text.contains("bars (48 of 5000, evenly sampled"), "{text}");
    assert!(text.contains("; data holds 200):"), "{text}");
    assert_eq!(row.bars.len(), 200);
    assert!(
        obs.data.to_string().len() > text.len(),
        "the bars stay in data"
    );
    assert!(text.contains(&source), "full pool address: {text}");
}

#[tokio::test]
async fn an_hl_fetch_backfills_bars_and_funding_then_reads_them() {
    let (base, seen) = serve_info(vec![
        Route {
            when: json!({"type": "candleSnapshot"}),
            exact: false,
            status: 200,
            body: fixture("candleSnapshot_xyz_TSLA_1h.json"),
        },
        Route {
            when: json!({"type": "fundingHistory"}),
            exact: false,
            status: 200,
            body: fixture("fundingHistory_xyz_TSLA.json"),
        },
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteMarketData::open(dir.path()).unwrap();
    let now = LAST + 3 * H;
    let req = HistoryArgs::parse(
        &json!({"instrument": TSLA, "from": FIRST, "to": LAST + H, "fetch": true}),
        now,
    )
    .unwrap();
    let client = Some(Ok(FetchClient::Hl(hl(&base))));
    let row = read_history(&store, &req, client, &FAST, &NO_SPLITS, now).await;
    assert!(row.errors.is_empty(), "{:?}", row.errors);
    let fetch = row.fetch.clone().unwrap();
    // 67 bars; 68 funding rows served, the one at `to` outside the window.
    assert_eq!((fetch.bars, fetch.funding), (67, Some(67)));
    assert_eq!(fetch.source, "hl:127.0.0.1");
    let obs = Observation::of(names::MARKET_HISTORY, &row, now, 0, ObsSource::Live);
    assert_eq!(obs.status, ObsStatus::Ok);
    let f = &obs.features;
    for (k, v) in [
        ("n_bars", 67),
        ("points", 48),
        ("gaps", 0),
        ("funding_points", 67),
        ("fetched_bars", 67),
        ("fetched_funding", 67),
        ("first_ms", FIRST),
        ("last_ms", LAST),
    ] {
        assert_eq!(f[k], v, "{k}");
    }
    assert_eq!(f["last_close"], 360.2);
    let ret = (360.2f64 / 372.33).ln() * 10_000.0;
    assert!((f["ret_bps"].as_f64().unwrap() - ret).abs() < 1e-9);
    let text = render(&obs, &row, now);
    assert!(
        text.contains("last_close=360.2 ret_bps=-331.2 | ok 0s live"),
        "{text}"
    );
    assert!(
        text.contains("fetched: 67 bar(s), 67 funding row(s) written from hl:127.0.0.1"),
        "{text}"
    );
    let types: Vec<String> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|b| b["type"].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(
        types,
        vec!["candleSnapshot", "fundingHistory"],
        "one request each"
    );

    // Resumed: the bars are up to date (no candle request); funding asks
    // only for the tail after its last stored row (HL settles a few ms
    // past the hour), which holds nothing new.
    let client = Some(Ok(FetchClient::Hl(hl(&base))));
    let again = read_history(&store, &req, client, &FAST, &NO_SPLITS, now).await;
    assert!(again.errors.is_empty(), "{:?}", again.errors);
    let count = |t: &str| {
        seen.lock()
            .unwrap()
            .iter()
            .filter(|b| b["type"] == t)
            .count()
    };
    assert_eq!((count("candleSnapshot"), count("fundingHistory")), (1, 2));
    let fetch = again.fetch.unwrap();
    assert_eq!((fetch.bars, fetch.funding), (0, Some(0)));
    assert!(
        fetch
            .notes
            .iter()
            .any(|n| n.starts_with("bars: stored ") && n.ends_with("up to date, nothing fetched")),
        "{:?}",
        fetch.notes
    );
    assert_eq!(again.stats.unwrap().n_bars, 67);
}

#[tokio::test]
async fn a_gecko_fetch_backfills_pool_bars() {
    let ohlcv: Vec<Value> = [9, 8, 7]
        .iter()
        .map(|h| json!([h * 3600, 150.0, 151.0, 149.0, 150.5, 1000.0]))
        .collect();
    let page = json!({"data": {"id": "x", "type": "ohlcv_request_response",
            "attributes": {"ohlcv_list": ohlcv}}, "meta": {}});
    let (base, seen) = serve(vec![canned(200, page.to_string())]).await;
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteMarketData::open(dir.path()).unwrap();
    let id = format!("solana:{MINT}");
    let req = HistoryArgs::parse(
        &json!({"instrument": id, "from": 7 * H, "to": 10 * H, "fetch": true, "pool": POOL}),
        20 * H,
    )
    .unwrap();
    let gecko = GeckoClient::new(test_client(), &base, local_scope(), None).unwrap();
    let row = read_history(
        &store,
        &req,
        Some(Ok(FetchClient::Gecko(gecko))),
        &FAST,
        &NO_SPLITS,
        20 * H,
    )
    .await;
    assert!(row.errors.is_empty(), "{:?}", row.errors);
    let fetch = row.fetch.clone().unwrap();
    assert_eq!((fetch.bars, fetch.funding), (3, None));
    assert_eq!(fetch.source, format!("gecko:solana:{POOL}"));
    let obs = Observation::of(names::MARKET_HISTORY, &row, 20 * H, 0, ObsSource::Live);
    assert_eq!(obs.features["n_bars"], 3);
    assert!(
        !obs.features.contains_key("fetched_funding"),
        "Gecko has no funding"
    );
    assert!(!obs.features.contains_key("funding_mean_apr_pct"));
    let req_line = seen.lock().unwrap()[0].lines().next().unwrap().to_string();
    assert!(
        req_line.contains(&format!("/networks/solana/pools/{POOL}/ohlcv/hour?")),
        "{req_line}"
    );
    assert!(req_line.contains(&format!("token={MINT}")), "{req_line}");
}

#[tokio::test]
async fn fetch_failures_are_error_fields_and_the_read_still_runs() {
    let (_dir, store) = seeded().await;
    // Every request answered 404 (fatal) / 429 (rate limited).
    let (base, _) = serve_info(vec![
        Route {
            when: json!({"type": "candleSnapshot"}),
            exact: false,
            status: 404,
            body: "{}".into(),
        },
        Route {
            when: json!({"type": "fundingHistory"}),
            exact: false,
            status: 429,
            body: "{}".into(),
        },
    ])
    .await;
    // The window runs past the stored bars (0 … 9 h): a tail to fetch.
    let req = HistoryArgs::parse(
        &json!({"instrument": TSLA, "from": 0, "to": 20 * H, "fetch": true}),
        100 * H,
    )
    .unwrap();
    let row = read_history(
        &store,
        &req,
        Some(Ok(FetchClient::Hl(hl(&base)))),
        &FAST,
        &NO_SPLITS,
        100 * H,
    )
    .await;
    let obs = Observation::of(names::MARKET_HISTORY, &row, 100 * H, 0, ObsSource::Live);
    assert_eq!(obs.status, ObsStatus::Partial, "stored bars still read");
    assert_eq!(
        (obs.features["n_bars"].clone(), obs.features["gaps"].clone()),
        (9.into(), 11.into())
    );
    let fields: Vec<(&str, ErrorClass)> = obs
        .errors
        .iter()
        .map(|e| (e.field.as_str(), e.class))
        .collect();
    assert_eq!(
        fields,
        vec![
            ("fetch_bars", ErrorClass::Fatal),
            ("fetch_funding", ErrorClass::RateLimited)
        ]
    );
    assert_eq!(obs.features["fetched_bars"], 0);
    let text = render(&obs, &row, 100 * H);
    assert!(text.contains("error fetch_bars: fatal HTTP 404"), "{text}");

    // A client that could not be built, and no stored bar: an error row.
    let req = HistoryArgs::parse(
        &json!({"instrument": "hyperliquid:SOL", "from": 0, "to": 10 * H, "fetch": true}),
        100 * H,
    )
    .unwrap();
    let row = read_history(
        &store,
        &req,
        Some(Err(anyhow!("HL_API_URL is not a URL"))),
        &FAST,
        &NO_SPLITS,
        100 * H,
    )
    .await;
    let obs = Observation::of(names::MARKET_HISTORY, &row, 100 * H, 0, ObsSource::Live);
    assert_eq!(obs.status, ObsStatus::Error);
    assert_eq!(
        (obs.errors[0].field.as_str(), obs.errors[0].class),
        ("fetch", ErrorClass::Fatal)
    );
    assert!(row.fetch.is_none());
}

/// `[backtest.splits]` reach `market_history` exactly as they reach a
/// backtest (review finding: KIOXIA's configured 3-for-1 read raw —
/// ret −11 178 bps, vol 713, drawdown 7 205 instead of −192 / 78 /
/// 1 614): the bars before the split ÷ ratio, volume × ratio, every
/// number and the sample of the adjusted series, the note in the row and
/// the text; `market.db` keeps the raw bars.
#[tokio::test]
async fn market_history_applies_the_sandboxs_share_splits() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteMarketData::open(dir.path()).unwrap();
    // 3 hourly bars near 340, the 3-for-1 split at 03:00, 3 near 115.
    let closes = [342.0, 345.0, 340.79, 114.93, 116.0, 115.5];
    let raw: Vec<Bar> = closes
        .iter()
        .enumerate()
        .map(|(i, c)| bar(i as i64 * H, *c))
        .collect();
    store
        .put_bars(KIOXIA, Interval::H1, "hl", &raw)
        .await
        .unwrap();
    let splits = r#"
            [splits]
            "hyperliquid:xyz:KIOXIA" = [{ at = "1970-01-01T03:00:00Z", ratio = 3.0 }]
        "#;
    let tool = MarketHistoryTool {
        def: defs::def(names::MARKET_HISTORY),
        shared: XlabShared {
            market: Ok(Arc::new(store)),
            store: None,
            sandbox: Arc::new(SandboxSections {
                backtest: Some(toml::from_str(splits).unwrap()),
                ..Default::default()
            }),
        },
    };
    let h = TestHarness::new(dir.path());
    let out = tool
        .execute(
            &json!({"instrument": KIOXIA, "from": 0, "to": 6 * H}),
            &h.ctx(),
        )
        .await
        .unwrap();
    let obs = out.observation.unwrap();
    // The series a backtest reads: the pre-split bars ÷ 3, volume × 3.
    let adjusted: Vec<Bar> = raw
        .iter()
        .map(|b| match b.t_open_ms < 3 * H {
            true => Bar {
                o: b.o / 3.0,
                h: b.h / 3.0,
                l: b.l / 3.0,
                c: b.c / 3.0,
                v: b.v * 3.0,
                ..*b
            },
            false => *b,
        })
        .collect();
    let want = crate::domain::marketdata_stats::bar_stats(&adjusted).unwrap();
    let row: MarketHistory = obs.typed().unwrap();
    assert_eq!(row.stats.as_ref(), Some(&want));
    let ret = (115.5f64 / (342.0 / 3.0)).ln() * 10_000.0;
    assert!((obs.features["ret_bps"].as_f64().unwrap() - ret).abs() < 1e-9);
    assert!(
        obs.features["vol_bps"].as_f64().unwrap() < 200.0,
        "no fake crash"
    );
    assert_eq!(obs.features["splits_applied"], 1);
    assert_eq!(row.bars[0].c, 342.0 / 3.0, "the sample is adjusted");
    let note = "split-adjusted hyperliquid:xyz:KIOXIA: ratio 3 (new shares per old) at \
                    1970-01-01T03:00:00Z — 3 bars before it: prices ÷ 3, volume × 3";
    assert_eq!(row.notes, vec![note.to_string()]);
    assert!(
        out.text.contains(&format!(
            "\n{note} — [backtest.splits], as a backtest reads the bars (market.db keeps \
                 them raw)\n"
        )),
        "{}",
        out.text
    );
    // market.db keeps the raw bars; a window after the split needs no
    // adjustment and says nothing.
    let market = tool.shared.market().unwrap();
    let stored = market.bars(KIOXIA, Interval::H1, 0, 6 * H).await.unwrap();
    assert_eq!(stored.bars[0].c, 342.0);
    let late = tool
        .execute(
            &json!({"instrument": KIOXIA, "from": 3 * H, "to": 6 * H}),
            &h.ctx(),
        )
        .await
        .unwrap();
    let late = late.observation.unwrap();
    assert!(!late.features.contains_key("splits_applied"));
    // Without the section the raw jump shows (what the backtest would
    // book unconfigured).
    let raw_row = read_history(
        market,
        &args(json!({"instrument": KIOXIA, "from": 0, "to": 6 * H})),
        None,
        &FAST,
        &NO_SPLITS,
        100 * H,
    )
    .await;
    let raw_ret = (115.5f64 / 342.0).ln() * 10_000.0;
    assert!((raw_row.stats.unwrap().ret_bps.unwrap() - raw_ret).abs() < 1e-9);
    assert!(raw_row.notes.is_empty());
}

#[tokio::test]
async fn execute_gates_scope_refuses_without_a_warehouse_and_reads() {
    let dir = tempfile::tempdir().unwrap();
    let (_d, store) = seeded().await;
    let tool = MarketHistoryTool {
        def: defs::def(names::MARKET_HISTORY),
        shared: shared(store),
    };
    assert_eq!(tool.definition().name, names::MARKET_HISTORY);
    let call = json!({"instrument": TSLA, "from": 0, "to": 10 * H});
    // The workspace is the scope's gate (the observation store).
    let denied = TestHarness::with_scope(dir.path(), ToolScope::default());
    assert!(tool.execute(&call, &denied.ctx()).await.is_err());
    let h = TestHarness::new(dir.path());
    let out = tool.execute(&call, &h.ctx()).await.unwrap();
    let obs = out.observation.unwrap();
    assert_eq!((obs.status, obs.ttl_ms), (ObsStatus::Ok, 0));
    assert!(
        out.text
            .starts_with(&format!("mkt_history {TSLA} 1h bars=9 ")),
        "{}",
        out.text
    );
    // Bad arguments: an error, no row.
    let e = tool
        .execute(&json!({"instrument": TSLA, "points": 0}), &h.ctx())
        .await
        .unwrap_err();
    assert!(e.to_string().contains("'points'"), "{e}");
    // No [xmarket]: refused before anything else.
    let refused = MarketHistoryTool {
        def: defs::def(names::MARKET_HISTORY),
        shared: XlabShared {
            market: super::super::open_market(&SandboxSections::default()),
            store: None,
            sandbox: Arc::new(SandboxSections::default()),
        },
    };
    let e = refused.execute(&call, &h.ctx()).await.unwrap_err();
    assert!(e.to_string().starts_with("state_dir_missing: "), "{e}");
}
