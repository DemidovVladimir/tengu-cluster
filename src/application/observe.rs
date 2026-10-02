//! `observe()` — cache-or-fetch for typed tools. Serves only fresh rows,
//! never caches `Error` rows, offers every live result to the history
//! recorder (`ObservationStore::record`, `Error` and ttl-0 rows too), and a
//! store failure falls back to a live read (doctrine #4: memory degradation
//! never breaks a call).

use std::future::Future;

use tracing::warn;

use crate::domain::observation::{CachePolicy, ObsSource, ObsStatus, Observation, Observed};
use crate::ports::observation::ObservationStore;

/// Return the fresh cached row for `policy.key` (`source = cache`), else run
/// `fetch`, wrap its value as a `Live` observation, `record` it (history) and
/// store it when it is usable and `ttl_ms > 0`. `fetch` returns the TTL
/// because some tools pick it after the read (`solana_tx`: longer once
/// finalized).
pub(crate) async fn observe<T, F, Fut>(
    store: Option<&dyn ObservationStore>,
    tool: &str,
    policy: &CachePolicy,
    now_ms: i64,
    fetch: F,
) -> anyhow::Result<Observation>
where
    T: Observed,
    F: FnOnce() -> Fut,
    Fut: Future<Output = anyhow::Result<(T, u64)>>,
{
    if let Some(s) = store {
        match s.get(&policy.key).await {
            Ok(Some(row)) if row.is_fresh(now_ms, policy.max_age_ms) => {
                return Ok(row.served_from_cache())
            }
            Ok(_) => {}
            Err(e) => {
                warn!(key = %policy.key, error = %e, "observation store read failed; reading live")
            }
        }
    }
    let (value, ttl_ms) = fetch().await?;
    let obs = Observation::of(tool, &value, now_ms, ttl_ms, ObsSource::Live);
    if obs.key != policy.key {
        warn!(policy_key = %policy.key, key = %obs.key, tool, "observation key differs from its cache policy key");
    }
    if let Some(s) = store {
        // History first: every live result, `Error` and ttl-0 rows included.
        if let Err(e) = s.record(&obs).await {
            warn!(key = %obs.key, error = %e, "observation history write failed");
        }
        if obs.status != ObsStatus::Error && ttl_ms > 0 {
            if let Err(e) = s.put(&obs).await {
                warn!(key = %obs.key, error = %e, "observation store write failed");
            }
        }
    }
    Ok(obs)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::domain::observation::Features;
    use async_trait::async_trait;
    use serde::{Deserialize, Serialize};
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// Cache map + every row offered to `record` (the history).
    #[derive(Default)]
    pub(crate) struct MemStore(
        Mutex<HashMap<String, Observation>>,
        pub(crate) Mutex<Vec<Observation>>,
    );

    #[async_trait]
    impl ObservationStore for MemStore {
        async fn get(&self, key: &str) -> anyhow::Result<Option<Observation>> {
            Ok(self.0.lock().unwrap().get(key).cloned())
        }
        async fn get_many(&self, keys: &[String]) -> anyhow::Result<Vec<Option<Observation>>> {
            let m = self.0.lock().unwrap();
            Ok(keys.iter().map(|k| m.get(k).cloned()).collect())
        }
        async fn put(&self, obs: &Observation) -> anyhow::Result<bool> {
            self.0.lock().unwrap().insert(obs.key.clone(), obs.clone());
            Ok(true)
        }
        async fn put_if_unchanged(
            &self,
            obs: &Observation,
            expected_observed_at_ms: Option<i64>,
        ) -> anyhow::Result<bool> {
            let mut m = self.0.lock().unwrap();
            if m.get(&obs.key).map(|o| o.observed_at_ms) != expected_observed_at_ms {
                return Ok(false);
            }
            m.insert(obs.key.clone(), obs.clone());
            Ok(true)
        }
        async fn record(&self, obs: &Observation) -> anyhow::Result<()> {
            self.1.lock().unwrap().push(obs.clone());
            Ok(())
        }
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct Price {
        usd: Option<f64>,
    }

    impl Observed for Price {
        const SCHEMA: &'static str = "price_test/1";
        fn subject(&self) -> String {
            "So11111111111111111111111111111111111111112".into()
        }
        fn headline(&self) -> String {
            "price".into()
        }
        fn features(&self) -> Features {
            Features::new()
        }
        fn status(&self) -> ObsStatus {
            if self.usd.is_some() {
                ObsStatus::Ok
            } else {
                ObsStatus::Error
            }
        }
    }

    fn policy(args: serde_json::Value) -> CachePolicy {
        CachePolicy::new(
            Price::SCHEMA,
            "So11111111111111111111111111111111111111112",
            10_000,
            &args,
        )
    }

    #[tokio::test]
    async fn second_call_is_served_from_cache_until_max_age() {
        let store = MemStore::default();
        let counter = AtomicUsize::new(0);
        let calls = &counter;
        let fetch = move || async move {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok((Price { usd: Some(150.0) }, 10_000))
        };
        let a = observe(Some(&store), "p", &policy(json!({})), 1_000, fetch)
            .await
            .unwrap();
        assert_eq!(a.source, ObsSource::Live);
        let b = observe(Some(&store), "p", &policy(json!({})), 5_000, fetch)
            .await
            .unwrap();
        assert_eq!(b.source, ObsSource::Cache);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // max_age_secs = 0 forces a live read.
        let c = observe(
            Some(&store),
            "p",
            &policy(json!({"max_age_secs": 0})),
            5_000,
            fetch,
        )
        .await
        .unwrap();
        assert_eq!(c.source, ObsSource::Live);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn error_rows_are_not_cached() {
        let store = MemStore::default();
        let o = observe(Some(&store), "p", &policy(json!({})), 0, || async {
            Ok((Price { usd: None }, 10_000))
        })
        .await
        .unwrap();
        assert_eq!(o.status, ObsStatus::Error);
        assert!(store.0.lock().unwrap().is_empty());
    }

    /// `ops-history-recorder`: every live result reaches `record` — `Error`
    /// and ttl-0 rows too — while the cache holds neither; a cache hit is not
    /// recorded again.
    #[tokio::test]
    async fn live_results_are_recorded_error_and_ttl0_included() {
        let store = MemStore::default();
        let run = |at: i64, usd: Option<f64>, ttl: u64| {
            let store = &store;
            async move {
                observe(Some(store), "p", &policy(json!({})), at, || async move {
                    Ok((Price { usd }, ttl))
                })
                .await
                .unwrap()
            }
        };
        assert_eq!(run(1_000, None, 10_000).await.status, ObsStatus::Error);
        assert_eq!(run(2_000, Some(1.0), 0).await.source, ObsSource::Live);
        assert!(
            store.0.lock().unwrap().is_empty(),
            "no Error / ttl-0 row cached"
        );
        assert_eq!(run(3_000, Some(2.0), 10_000).await.source, ObsSource::Live);
        assert_eq!(run(4_000, Some(3.0), 10_000).await.source, ObsSource::Cache);
        let recorded: Vec<(ObsStatus, i64, u64)> = store
            .1
            .lock()
            .unwrap()
            .iter()
            .map(|o| (o.status, o.observed_at_ms, o.ttl_ms))
            .collect();
        assert_eq!(
            recorded,
            [
                (ObsStatus::Error, 1_000, 10_000),
                (ObsStatus::Ok, 2_000, 0),
                (ObsStatus::Ok, 3_000, 10_000)
            ]
        );
        assert_eq!(store.0.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn no_store_reads_live() {
        let o = observe(None, "p", &policy(json!({})), 0, || async {
            Ok((Price { usd: Some(1.0) }, 10_000))
        })
        .await
        .unwrap();
        assert_eq!(o.source, ObsSource::Live);
    }
}
