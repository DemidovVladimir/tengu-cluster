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
//! | Share splits | the sandbox's `[backtest.splits]` applied to the window's bars before any number, exactly as a backtest applies them (`MarketData::adjust_for_splits`: bars before a split ÷ ratio, volume × ratio; funding untouched) — one row note per applied split, `splits_applied`; `market.db` keeps the raw bars |
//! | Row | ttl 0: a history read, never cached (recorded when `[recorder]` takes `mkt_history/1`) |
//! | Text | line 1, features and errors as `Observation::render_text` gives them (without `data`), what a fetch wrote, the split notes, a table of ≤ 48 of the sampled bars, one line per stored series — ≈ 5 KB at most, inside a 16k-window local model's 8 192-char result cap |

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use serde_json::Value;

use super::{defs, field, object_args, opt_str, opt_time, whole, XlabShared};
use crate::adapters::outbound::backfill::gecko::{
    self, gecko_bars, network_of, GeckoClient, GeckoPlan,
};
use crate::adapters::outbound::backfill::hl::{hl_bars, hl_funding_history, HlPlan};
use crate::adapters::outbound::backfill::{text_table, ReportRow, Retry};
use crate::adapters::outbound::http_class::read_error;
use crate::adapters::outbound::hyperliquid::info::{self, HlInfo};
use crate::adapters::outbound::tools::hyperliquid::store_live;
use crate::config::sections::SandboxSections;
use crate::domain::backtest::engine::MarketData;
use crate::domain::market::{InstrumentId, HYPERLIQUID};
use crate::domain::marketdata::{Interval, StockSplit};
use crate::domain::marketdata_stats::{sample_indices, FetchSummary, MarketHistory, StoredSeries};
use crate::domain::message::ToolDef;
use crate::domain::observation::{
    now_ms, ErrorClass, ObsSource, ObsStatus, Observation, Observed, ReadError,
};
use crate::domain::time::fmt_time;
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
        let splits = sandbox_splits(&self.shared.sandbox);
        let row = read_history(market, &req, client, &Retry::TOOL, &splits, now).await;
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

impl HistoryArgs {
    /// `args` at `now_ms` (defaults and refusals: module table).
    pub(crate) fn parse(args: &Value, now_ms: i64) -> Result<Self> {
        let tool = names::MARKET_HISTORY;
        let o = object_args(tool, args, ARGS)?;
        let id = opt_str(tool, o, "instrument")?.ok_or_else(|| {
            anyhow!("{tool}: 'instrument' is required (a full id such as hyperliquid:xyz:TSLA)")
        })?;
        let instrument =
            InstrumentId::parse(id).map_err(|e| anyhow!("{tool}: 'instrument': {e}"))?;
        let interval = match opt_str(tool, o, "interval")? {
            None => Interval::H1,
            Some(s) => Interval::parse(s).map_err(|e| anyhow!("{tool}: 'interval': {e}"))?,
        };
        let to_ms = opt_time(tool, o, "to")?.unwrap_or(now_ms);
        let from_ms = match opt_time(tool, o, "from")? {
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
        let pool = opt_str(tool, o, "pool")?.map(str::to_string);
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

/// The sandbox's `[backtest.splits]` by full id (none without `[backtest]`).
pub(crate) fn sandbox_splits(sandbox: &SandboxSections) -> BTreeMap<String, Vec<StockSplit>> {
    sandbox
        .backtest
        .as_ref()
        .map(|bt| bt.stock_splits())
        .unwrap_or_default()
}

/// The module table at `now_ms`: fetch first when `client` is given (its
/// build may have failed), then the read; `splits` = the sandbox's
/// `[backtest.splits]` ([`sandbox_splits`]).
pub(crate) async fn read_history(
    market: &dyn MarketDataStore,
    req: &HistoryArgs,
    client: Option<Result<FetchClient>>,
    retry: &Retry,
    splits: &BTreeMap<String, Vec<StockSplit>>,
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
        Ok((bars, funding)) => {
            // As a backtest reads them: the same adjustment, the same notes.
            let mut md = MarketData::default();
            md.bars.insert(id.clone(), bars);
            let notes = md.adjust_for_splits(splits);
            let bars = md.bars.remove(&id).map(|s| s.bars).unwrap_or_default();
            let mut row = MarketHistory::new(
                &id,
                iv,
                from,
                to,
                &bars,
                &funding.points,
                req.points,
                now_ms,
            );
            row.notes = notes;
            row
        }
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
    lines.extend(row.notes.iter().map(|n| {
        format!("{n} — [backtest.splits], as a backtest reads the bars (market.db keeps them raw)")
    }));
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
#[path = "history_tests.rs"]
mod tests;
