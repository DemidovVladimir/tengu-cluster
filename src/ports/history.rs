//! History store port (`ops-history-recorder`, tracker convention 5) — the
//! append-only time series of observations behind research, replay and the
//! weekend clock. Fed by `RecordingObservationStore`
//! (`adapters/outbound/observations.rs`): rows `put` writes plus every live
//! result `observe()` passes to `ObservationStore::record`. Impl:
//! `adapters::outbound::history_sqlite::SqliteHistoryStore`
//! (`<TENGU_HOME>/state/<xmarket.state>/history/<YYYYMMDD>.db`).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::observation::{Features, ObsSource, ObsStatus, Observation, ReadError};

/// One recorded observation: the envelope without `tool`, `ttl_ms` and
/// `headline`; `data` only for `[recorder] keep_data` schemas.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct HistoryRow {
    /// Full observation key, e.g. `mkt_ctx/1:hyperliquid:xyz:TSLA`.
    pub key: String,
    pub schema: String,
    pub observed_at_ms: i64,
    /// `features.venue_ts_ms`, when the venue stamps its data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub venue_ts_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<u64>,
    pub source: ObsSource,
    pub status: ObsStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<ReadError>,
    pub features: Features,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl HistoryRow {
    pub fn of(obs: &Observation, keep_data: bool) -> Self {
        let venue_ts_ms = obs
            .features
            .get("venue_ts_ms")
            .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)));
        Self {
            key: obs.key.clone(),
            schema: obs.schema.clone(),
            observed_at_ms: obs.observed_at_ms,
            venue_ts_ms,
            slot: obs.slot,
            source: obs.source,
            status: obs.status,
            errors: obs.errors.clone(),
            features: obs.features.clone(),
            data: (keep_data && !obs.data.is_null()).then(|| obs.data.clone()),
        }
    }
}

#[async_trait]
pub(crate) trait HistoryStore: Send + Sync {
    /// Append rows, each into its UTC day; a row whose `(key,
    /// observed_at_ms)` is already there is skipped. Returns the rows added.
    async fn append(&self, rows: &[HistoryRow]) -> anyhow::Result<usize>;
    /// Rows of `key` with `from_ms <= observed_at_ms < to_ms`, oldest first.
    async fn range(&self, key: &str, from_ms: i64, to_ms: i64) -> anyhow::Result<Vec<HistoryRow>>;
    /// Per key, in order: its latest row with `observed_at_ms <= t_ms`;
    /// `None` when there is none or it is older than `max_age_ms`.
    async fn asof(
        &self,
        keys: &[String],
        t_ms: i64,
        max_age_ms: u64,
    ) -> anyhow::Result<Vec<Option<HistoryRow>>>;
}
