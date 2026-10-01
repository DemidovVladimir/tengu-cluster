//! `market_history` — one instrument's stored history in a window as the
//! `mkt_history/1:<instrument>:<interval>` row (`domain/marketdata_stats.rs`:
//! stats, sample, gaps), read from `<state dir>/market.db`.
//!
//! | Step | Rule |
//! |---|---|
//! | Refuse | no `[xmarket]` ⇒ `state_dir_missing`; `market.db` not openable ⇒ `market_data_unavailable`; arguments parse strictly (an unknown key, a wrong type, a bad id, `from` ≥ `to`, a window over 100 000 bars, `points` outside 1–200, `pool` on a `hyperliquid:` id, `fetch` for a venue without a source or a pool-less DEX id — all errors, no read) |
//! | Window | `[from, to)` of bar opens and funding times: `to` default now, `from` default `to` − 7 days; epoch ms, RFC 3339 or a UTC date |
//! | Fetch (`fetch = true`) | before the read, the part of the window not stored yet through `outbound/backfill/` (resume, clamp and closed-bar rules there): `hyperliquid:<coin>` → HL `candleSnapshot` + `fundingHistory` (`HlInfo`; `$HL_API_URL` only through `env_reads`); `solana:` / `robinhood:` → GeckoTerminal pool OHLCV for `pool` (`$GECKO_API_URL` likewise). Every request through the egress gate and the scope's `net_hosts`, against `[rate_limits.hyperliquid]` / `[rate_limits.geckoterminal]`, retried per `Retry::TOOL`. A failure is an error field (`fetch_bars`, `fetch_funding`, `fetch`, with its class) and the read still runs |
//! | Read | the window's bars + funding and the instrument's coverage from `market.db` — no network; a failed read ⇒ an `error` row (`market_db`) |
//! | Row | ttl 0: a history read, never cached (recorded when `[recorder]` takes `mkt_history/1`) |
//! | Text | line 1, features and errors as `Observation::render_text` gives them (without `data`), what a fetch wrote, a table of ≤ 48 of the sampled bars, one line per stored series — ≈ 5 KB at most, inside a 16k-window local model's 8 192-char result cap |

use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use serde_json::{Map, Value};

use super::{defs, XlabShared};
use crate::adapters::outbound::backfill::gecko::{
    self, gecko_bars, network_of, GeckoClient, GeckoPlan,
};
use crate::adapters::outbound::backfill::hl::{hl_bars, hl_funding_history, HlPlan};
use crate::adapters::outbound::backfill::{text_table, ReportRow, Retry};
use crate::adapters::outbound::http_class::read_error;
use crate::adapters::outbound::hyperliquid::info::{self, HlInfo};
use crate::adapters::outbound::tools::hyperliquid::store_live;
use crate::config::sections::SandboxSections;
use crate::domain::market::{InstrumentId, HYPERLIQUID};
use crate::domain::marketdata::{fmt_time, parse_time, Interval};
use crate::domain::marketdata_stats::{sample_indices, FetchSummary, MarketHistory, StoredSeries};
use crate::domain::message::ToolDef;
use crate::domain::observation::{
    now_ms, ErrorClass, ObsSource, ObsStatus, Observation, Observed, ReadError,
};
use crate::domain::tools as names;
use crate::ports::market_data::{CoverageRow, MarketDataStore};
use crate::ports::tool::{Tool, ToolCtx, ToolOutput};

/// Bars the observation data holds by default.
pub(crate) const DEFAULT_POINTS: usize = 48;
/// Upper bound of `points`.
pub(crate) const MAX_POINTS: usize = 200;
/// Rows of the text table at most (the local-model result cap).
pub(crate) const TEXT_ROWS: usize = 48;
/// Default window: the 7 days before `to`.
pub(crate) const DEFAULT_WINDOW_MS: i64 = 7 * 86_400_000;
/// Largest window, in bar slots of the interval.
pub(crate) const MAX_WINDOW_BARS: i64 = 100_000;

const ARGS: &[&str] = &[
    "instrument",
    "interval",
    "from",
    "to",
    "fetch",
    "pool",
    "points",
];

pub(crate) fn tools(shared: &XlabShared) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(MarketHistoryTool {
        def: defs::def(names::MARKET_HISTORY),
        shared: shared.clone(),
    })]
}

pub(crate) struct MarketHistoryTool {
    def: ToolDef,
    shared: XlabShared,
}

#[async_trait]
impl Tool for MarketHistoryTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        ctx.scope.check_fs_write(ctx.workspace)?;
        let market = self.shared.market()?;
        let now = now_ms();
        let req = HistoryArgs::parse(args, now)?;
        let client = req
            .fetch
            .then(|| fetch_client(ctx, &self.shared.sandbox, &req));
        let row = read_history(market, &req, client, &Retry::TOOL, now).await;
        let obs = Observation::of(names::MARKET_HISTORY, &row, now, 0, ObsSource::Live);
        store_live(self.shared.store.as_deref(), &obs).await;
        Ok(ToolOutput {
            text: render(&obs, &row, now),
            observation: Some(obs),
        })
    }
}

// ── Arguments (strict) ─────────────────────────────────────────────

/// The parsed call (module table).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HistoryArgs {
    pub instrument: InstrumentId,
    pub interval: Interval,
    pub from_ms: i64,
    pub to_ms: i64,
    pub fetch: bool,
    /// GeckoTerminal pool, verbatim (`solana:` / `robinhood:` ids only).
    pub pool: Option<String>,
    pub points: usize,
}

fn field<'a>(o: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    o.get(key).filter(|v| !v.is_null())
}

fn opt_str<'a>(o: &'a Map<String, Value>, key: &str) -> Result<Option<&'a str>> {
    let tool = names::MARKET_HISTORY;
    match field(o, key) {
        None => Ok(None),
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(Some(s.trim())),
        Some(v) => bail!("{tool}: '{key}' must be a non-empty string, got {v}"),
    }
}

/// A whole number from a JSON integer or an integral float.
fn whole(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| {
        v.as_f64()
            .filter(|x| x.is_finite() && x.fract() == 0.0 && x.abs() < 9.0e15)
            .map(|x| x as i64)
    })
}

/// `from` / `to`: a time string (`parse_time`) or an epoch-ms number.
fn opt_time(o: &Map<String, Value>, key: &str) -> Result<Option<i64>> {
    let tool = names::MARKET_HISTORY;
    match field(o, key) {
        None => Ok(None),
        Some(Value::String(s)) => parse_time(s)
            .map(Some)
            .map_err(|e| anyhow!("{tool}: '{key}': {e}")),
        Some(v) => whole(v).map(Some).ok_or_else(|| {
            anyhow!("{tool}: '{key}' must be epoch ms, RFC 3339 or a date (2026-09-25), got {v}")
        }),
    }
}

impl HistoryArgs {
    /// `args` at `now_ms` (defaults and refusals: module table).
    pub(crate) fn parse(args: &Value, now_ms: i64) -> Result<Self> {
        let tool = names::MARKET_HISTORY;
        let o = args
            .as_object()
            .ok_or_else(|| anyhow!("{tool}: arguments must be a JSON object"))?;
        let unknown: Vec<&str> = o
            .keys()
            .map(String::as_str)
            .filter(|k| !ARGS.contains(k))
            .collect();
        if !unknown.is_empty() {
            bail!(
                "{tool}: unknown argument(s) {unknown:?} (allowed: {})",
                ARGS.join(", ")
            );
        }
        let id = opt_str(o, "instrument")?.ok_or_else(|| {
            anyhow!("{tool}: 'instrument' is required (a full id such as hyperliquid:xyz:TSLA)")
        })?;
        let instrument =
            InstrumentId::parse(id).map_err(|e| anyhow!("{tool}: 'instrument': {e}"))?;
        let interval = match opt_str(o, "interval")? {
            None => Interval::H1,
            Some(s) => Interval::parse(s).map_err(|e| anyhow!("{tool}: 'interval': {e}"))?,
        };
        let to_ms = opt_time(o, "to")?.unwrap_or(now_ms);
        let from_ms = match opt_time(o, "from")? {
            Some(t) => t,
            None => to_ms.saturating_sub(DEFAULT_WINDOW_MS),
        };
        if from_ms >= to_ms {
            bail!(
                "{tool}: 'from' {} is not before 'to' {}",
                fmt_time(from_ms),
                fmt_time(to_ms)
            );
        }
        let slots = to_ms.saturating_sub(from_ms) / interval.ms();
        if slots > MAX_WINDOW_BARS {
            bail!(
                "{tool}: the window holds {slots} {interval} bars (more than {MAX_WINDOW_BARS}): \
                 narrow from / to or pick a longer interval"
            );
        }
        let fetch = match field(o, "fetch") {
            None => false,
            Some(Value::Bool(b)) => *b,
            Some(v) => bail!("{tool}: 'fetch' must be a boolean, got {v}"),
        };
        let pool = opt_str(o, "pool")?.map(str::to_string);
        let points = match field(o, "points") {
            None => DEFAULT_POINTS,
            Some(v) => match whole(v).filter(|p| (1..=MAX_POINTS as i64).contains(p)) {
                Some(p) => p as usize,
                None => {
                    bail!("{tool}: 'points' must be an integer from 1 to {MAX_POINTS}, got {v}")
                }
            },
        };
        let venue = instrument.venue();
        let gecko = network_of(venue).is_ok();
        if pool.is_some() && !gecko {
            bail!(
                "{tool}: 'pool' is a GeckoTerminal pool, for solana: / robinhood: ids only; \
                 {instrument} has none"
            );
        }
        if fetch && venue != HYPERLIQUID && !gecko {
            bail!(
                "{tool}: fetch: no history source for venue `{venue}` (hyperliquid, solana, \
                 robinhood) — read what is stored without fetch"
            );
        }
        if fetch && gecko && pool.is_none() {
            bail!("{tool}: fetch for {instrument} needs 'pool' (its GeckoTerminal pool address)");
        }
        Ok(Self {
            instrument,
            interval,
            from_ms,
            to_ms,
            fetch,
            pool,
            points,
        })
    }
}

// ── Fetch + read ───────────────────────────────────────────────────

/// Where a fetch goes.
pub(crate) enum FetchClient {
    Hl(HlInfo),
    Gecko(GeckoClient),
}

/// The fetch client of `req`'s venue on this call's HTTP client and scope
/// (the base-URL override only through `env_reads`) and the sandbox's
/// request budget.
fn fetch_client(
    ctx: &ToolCtx<'_>,
    sandbox: &SandboxSections,
    req: &HistoryArgs,
) -> Result<FetchClient> {
    if req.instrument.venue() == HYPERLIQUID {
        let budget = sandbox.rate_limits.get(info::RATE_LIMIT).cloned();
        let base = info::api_url(ctx.scope);
        let hl = HlInfo::new(ctx.http.clone(), &base, ctx.scope.clone(), budget)?;
        Ok(FetchClient::Hl(hl))
    } else {
        let budget = sandbox.rate_limits.get(gecko::RATE_LIMIT).cloned();
        let base = gecko::api_url(ctx.scope);
        let client = GeckoClient::new(ctx.http.clone(), &base, ctx.scope.clone(), budget)?;
        Ok(FetchClient::Gecko(client))
    }
}

/// One error field per failure of a backfill row, with its class.
fn row_errors(row: &ReportRow, field: &str, out: &mut Vec<ReadError>) {
    for (i, e) in row.errors.iter().enumerate() {
        let class = row.classes.get(i).copied().unwrap_or(ErrorClass::Fatal);
        out.push(ReadError::new(field, class, e.clone()));
    }
}

/// `<kind>: <note>` for each row's notes.
fn row_notes(rows: &[&ReportRow]) -> Vec<String> {
    rows.iter()
        .flat_map(|r| r.notes.iter().map(move |n| format!("{}: {n}", r.kind)))
        .collect()
}

/// Backfill the window's missing part (module table).
async fn fetch(
    market: &dyn MarketDataStore,
    req: &HistoryArgs,
    client: &FetchClient,
    retry: &Retry,
    now_ms: i64,
    errors: &mut Vec<ReadError>,
) -> FetchSummary {
    let instrument = req.instrument.to_string();
    match client {
        FetchClient::Hl(hl) => {
            let plan = HlPlan {
                instrument,
                interval: req.interval,
                from_ms: req.from_ms,
                to_ms: req.to_ms,
            };
            let bars = hl_bars(hl, market, &plan, retry, now_ms).await;
            let funding = hl_funding_history(hl, market, &plan, retry, now_ms).await;
            row_errors(&bars, "fetch_bars", errors);
            row_errors(&funding, "fetch_funding", errors);
            FetchSummary {
                bars: bars.rows,
                funding: Some(funding.rows),
                source: bars.source.clone(),
                notes: row_notes(&[&bars, &funding]),
            }
        }
        FetchClient::Gecko(g) => {
            let plan = GeckoPlan {
                instrument,
                pool: req.pool.clone().unwrap_or_default(),
                interval: req.interval,
                from_ms: req.from_ms,
                to_ms: req.to_ms,
            };
            let bars = gecko_bars(g, market, &plan, retry, now_ms).await;
            row_errors(&bars, "fetch_bars", errors);
            FetchSummary {
                bars: bars.rows,
                funding: None,
                source: bars.source.clone(),
                notes: row_notes(&[&bars]),
            }
        }
    }
}

fn stored_series(r: &CoverageRow) -> StoredSeries {
    StoredSeries {
        kind: r.kind.clone(),
        interval: r.interval,
        first_ms: r.first_ms,
        last_ms: r.last_ms,
        rows: r.rows,
        sources: r.sources.clone(),
    }
}

/// The module table at `now_ms`: fetch first when `client` is given (its
/// build may have failed), then the read.
pub(crate) async fn read_history(
    market: &dyn MarketDataStore,
    req: &HistoryArgs,
    client: Option<Result<FetchClient>>,
    retry: &Retry,
    now_ms: i64,
) -> MarketHistory {
    let id = req.instrument.to_string();
    let (iv, from, to) = (req.interval, req.from_ms, req.to_ms);
    let mut errors = Vec::new();
    let summary = match client {
        None => None,
        Some(Err(e)) => {
            errors.push(read_error("fetch", &e));
            None
        }
        Some(Ok(c)) => Some(fetch(market, req, &c, retry, now_ms, &mut errors).await),
    };
    let read = async {
        let bars = market.bars(&id, iv, from, to).await?;
        let funding = market.funding(&id, from, to).await?;
        anyhow::Ok((bars, funding))
    };
    let mut row = match read.await {
        Ok((bars, funding)) => MarketHistory::new(
            &id,
            iv,
            from,
            to,
            &bars.bars,
            &funding.points,
            req.points,
            now_ms,
        ),
        Err(e) => {
            errors.push(ReadError::new(
                "market_db",
                ErrorClass::Transient,
                format!("{e:#}"),
            ));
            let mut row = MarketHistory::failed(&id, iv, from, to, errors);
            row.fetch = summary;
            return row;
        }
    };
    match market.coverage(Some(&id)).await {
        Ok(rows) => row.coverage = rows.iter().map(stored_series).collect(),
        Err(e) => errors.push(ReadError::new(
            "coverage",
            ErrorClass::Transient,
            format!("{e:#}"),
        )),
    }
    row.fetch = summary;
    row.errors = errors;
    row
}

// ── Text ───────────────────────────────────────────────────────────

/// A table cell: the stored number as written (`372.33`), or rounded to 8
/// significant digits when that is longer than 12 characters (a float
/// tail: `123.99265`, `0.000012345679`) — `data` keeps the exact value.
fn cell(x: f64) -> String {
    let s = x.to_string();
    if s.len() <= 12 || !x.is_finite() || x == 0.0 {
        return s;
    }
    let mag = x.abs().log10().floor() as i32;
    let decimals = (7 - mag).clamp(0, 20) as usize;
    let r = format!("{x:.decimals$}");
    if r.contains('.') {
        r.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        r
    }
}

/// The tool's text (module table).
pub(crate) fn render(obs: &Observation, row: &MarketHistory, now_ms: i64) -> String {
    let mut head = obs.clone();
    head.data = Value::Null;
    let mut lines = vec![head.render_text(now_ms)];
    if let Some(f) = &row.fetch {
        let funding = f
            .funding
            .map(|n| format!(", {n} funding row(s)"))
            .unwrap_or_default();
        lines.push(format!(
            "fetched: {} bar(s){funding} written from {}",
            f.bars, f.source
        ));
        lines.extend(f.notes.iter().map(|n| format!("fetch {n}")));
    }
    let n_bars = row.stats.as_ref().map_or(0, |s| s.n_bars);
    if row.bars.is_empty() {
        if row.status() == ObsStatus::Absent {
            lines.push(
                "no bar stored in the window — fetch = true backfills it (Hyperliquid, or \
                 GeckoTerminal with pool)"
                    .to_string(),
            );
        }
    } else {
        let shown = sample_indices(row.bars.len(), TEXT_ROWS);
        let mut title = format!(
            "bars ({} of {n_bars}, evenly sampled, the first and last kept; t = bar open, UTC",
            shown.len()
        );
        if shown.len() < row.bars.len() {
            title.push_str(&format!("; data holds {}", row.bars.len()));
        }
        lines.push(format!("{title}):"));
        let cells: Vec<Vec<String>> = shown
            .iter()
            .map(|&i| {
                let b = &row.bars[i];
                vec![
                    fmt_time(b.t_open_ms),
                    cell(b.o),
                    cell(b.h),
                    cell(b.l),
                    cell(b.c),
                    cell(b.v),
                    b.n.map_or_else(|| "-".to_string(), |n| n.to_string()),
                ]
            })
            .collect();
        let table = text_table(&["t", "o", "h", "l", "c", "v", "n"], &cells);
        lines.push(table.trim_end().to_string());
    }
    for s in &row.coverage {
        let iv = s.interval.map(|i| format!(" {i}")).unwrap_or_default();
        lines.push(format!(
            "stored {}{iv}: {} … {} rows={} sources={}",
            s.kind,
            fmt_time(s.first_ms),
            fmt_time(s.last_ms),
            s.rows,
            s.sources.join(", ")
        ));
    }
    if row.coverage.is_empty() && row.status() != ObsStatus::Error {
        lines.push(format!("stored: nothing for {}", row.instrument));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::outbound::backfill::test_support::FAST;
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
    const MINT: &str = "So11111111111111111111111111111111111111112";
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
        let row = read_history(&store, &req, None, &FAST, 100 * H).await;
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
            text.contains(
                "bars (4 of 9, evenly sampled, the first and last kept; t = bar open, UTC):"
            ),
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
        let row = read_history(&store, &req, None, &FAST, 100 * H).await;
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
        let row = read_history(&store, &req, None, &FAST, 100 * H).await;
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
        let row = read_history(&store, &req, None, &FAST, 5_001 * m).await;
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
        let row = read_history(&store, &req, client, &FAST, now).await;
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
        let again = read_history(&store, &req, client, &FAST, now).await;
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
                .any(|n| n.starts_with("bars: stored ")
                    && n.ends_with("up to date, nothing fetched")),
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
}
