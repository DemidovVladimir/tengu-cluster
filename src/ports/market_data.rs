//! `MarketDataStore` — the market-data warehouse behind history-first
//! research (xlab, `docs/xlab-2026-10-01.md` § 4): bars, funding and
//! asset contexts per full instrument id. Impl: `SqliteMarketData`
//! (`adapters/outbound/market_data.rs`, `<state dir>/market.db`); fed by the
//! backfill fetchers (`adapters/outbound/backfill/`), read by the backtest
//! use case (`application/backtest/`) and the `market_history` tool.

// Consumers land with the xlab wave (docs/xlab-2026-10-01.md); drop this then.
#![cfg_attr(not(test), allow(dead_code))]

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::domain::marketdata::{
    Bar, BarSeries, CtxPoint, CtxSeries, FundingPoint, FundingSeries, Interval,
};

/// What the store holds for one instrument and kind.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct CoverageRow {
    pub instrument: String,
    /// `bars` | `funding` | `ctx`.
    pub kind: String,
    /// Bars only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval: Option<Interval>,
    /// Earliest / latest row time (bars: `t_open_ms`).
    pub first_ms: i64,
    pub last_ms: i64,
    pub rows: u64,
    /// Distinct `source` values, sorted.
    pub sources: Vec<String>,
}

#[async_trait]
pub(crate) trait MarketDataStore: Send + Sync {
    /// Upsert bars (same `(instrument, interval, t_open_ms)` = replaced);
    /// returns the rows written. `source`: `hl`, `gecko:<network>:<pool>`,
    /// `json:<file name>`, …
    async fn put_bars(
        &self,
        instrument: &str,
        interval: Interval,
        source: &str,
        bars: &[Bar],
    ) -> anyhow::Result<usize>;
    /// Bars with `from_ms <= t_open_ms < to_ms`, ascending.
    async fn bars(
        &self,
        instrument: &str,
        interval: Interval,
        from_ms: i64,
        to_ms: i64,
    ) -> anyhow::Result<BarSeries>;
    /// Upsert funding points (same `(instrument, t_ms)` = replaced).
    async fn put_funding(
        &self,
        instrument: &str,
        source: &str,
        points: &[FundingPoint],
    ) -> anyhow::Result<usize>;
    /// Funding with `from_ms <= t_ms < to_ms`, ascending.
    async fn funding(
        &self,
        instrument: &str,
        from_ms: i64,
        to_ms: i64,
    ) -> anyhow::Result<FundingSeries>;
    /// Upsert context samples (same `(instrument, t_ms)` = replaced).
    async fn put_ctx(
        &self,
        instrument: &str,
        source: &str,
        points: &[CtxPoint],
    ) -> anyhow::Result<usize>;
    /// Context samples with `from_ms <= t_ms < to_ms`, ascending.
    async fn ctx(&self, instrument: &str, from_ms: i64, to_ms: i64) -> anyhow::Result<CtxSeries>;
    /// Coverage rows, one per (instrument, kind, interval); `instrument`
    /// filters to one id. Sorted by instrument, kind, interval.
    async fn coverage(&self, instrument: Option<&str>) -> anyhow::Result<Vec<CoverageRow>>;
}
