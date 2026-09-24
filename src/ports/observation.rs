//! Observation store port — the TTL cache typed tools read through
//! (`application::observe`) and decision loops read `state.world` from.
//! Impl: `adapters::outbound::observations::SqliteObservationStore`
//! (`<workspace>/.tengu/observations.db`).

use async_trait::async_trait;

use crate::domain::observation::Observation;

#[async_trait]
pub(crate) trait ObservationStore: Send + Sync {
    async fn get(&self, key: &str) -> anyhow::Result<Option<Observation>>;
    /// One result per key, in order.
    async fn get_many(&self, keys: &[String]) -> anyhow::Result<Vec<Option<Observation>>>;
    /// Upsert. Returns `Ok(false)` (ignored) for `ObsStatus::Error` rows and
    /// when old and new both carry a slot and `new.slot < old.slot`.
    async fn put(&self, obs: &Observation) -> anyhow::Result<bool>;
}
