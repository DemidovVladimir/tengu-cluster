//! Observation store port — the TTL cache typed tools read through
//! (`application::observe`) and decision loops read `state.world` from.
//! Impl: `adapters::outbound::observations::SqliteObservationStore`
//! (`<workspace>/.tengu/observations.db`).

use async_trait::async_trait;

use crate::domain::observation::Observation;

#[async_trait]
pub(crate) trait ObservationStore: Send + Sync {
    /// `Err` also when a row exists but does not parse: a cache caller reads
    /// live (and overwrites it), a state caller must not treat it as absent.
    async fn get(&self, key: &str) -> anyhow::Result<Option<Observation>>;
    /// One result per key, in order; a row that does not parse is `None`.
    async fn get_many(&self, keys: &[String]) -> anyhow::Result<Vec<Option<Observation>>>;
    /// Upsert. Returns `Ok(false)` (ignored) for `ObsStatus::Error` rows and
    /// when old and new both carry a slot and `new.slot < old.slot`.
    async fn put(&self, obs: &Observation) -> anyhow::Result<bool>;
    /// Compare-and-swap for read-modify-write rows (`lp_state`): write only
    /// while the stored row's `observed_at_ms` still equals `expected`
    /// (`None` = no row may exist). `Ok(false)` = conflict (or an `Error`
    /// row). Atomic across processes. A store that cannot compare-and-swap
    /// keeps this default and never writes.
    async fn put_if_unchanged(
        &self,
        _obs: &Observation,
        _expected_observed_at_ms: Option<i64>,
    ) -> anyhow::Result<bool> {
        anyhow::bail!("observation store does not support conditional writes")
    }
    /// Delete `keys` (missing keys are fine); returns how many rows went.
    /// The write tools drop the rows a landed transaction made stale.
    async fn remove(&self, _keys: &[String]) -> anyhow::Result<usize> {
        anyhow::bail!("observation store does not support removal")
    }
}
