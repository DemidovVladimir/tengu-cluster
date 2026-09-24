//! `state.world` — the loop's read-only view of the observation store. Each
//! `[decision_loops.<n>] world` alias (→ observation key) renders as one of:
//!
//! | Entry | Rendered |
//! |---|---|
//! | usable, age ≤ `world_max_age_secs` | `Observation::decision_value` |
//! | usable, older | `{"status":"stale","age_s":N}` |
//! | no row | `{"status":"missing"}` |
//! | error row / store unavailable | `{"status":"error","errors":[..]}` |
//!
//! Stale, missing and failed entries carry no feature numbers. The world is
//! read once per step and never fetched: typed tools (or a stream) write the
//! rows, the loop only reads them.

use std::collections::BTreeMap;

use serde_json::{json, Map, Value};
use tracing::warn;

use crate::config::decision_loop::DecisionLoopConfig;
use crate::domain::observation::{ErrorClass, Observation, ReadError};
use crate::ports::observation::ObservationStore;

enum Entry {
    Row(Box<Observation>),
    Missing,
    Unavailable(String),
}

/// One step's snapshot of every `world` alias.
pub(crate) struct World {
    now_ms: i64,
    max_age_ms: u64,
    entries: BTreeMap<String, Entry>,
}

impl World {
    /// No aliases (loops without `world`).
    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Self {
            now_ms: 0,
            max_age_ms: 0,
            entries: BTreeMap::new(),
        }
    }

    /// Read every alias in one `get_many`. Fail-soft: a store error marks
    /// every entry as an error (no numbers), never fails the step.
    pub(crate) async fn read(
        store: Option<&dyn ObservationStore>,
        cfg: &DecisionLoopConfig,
        now_ms: i64,
    ) -> Self {
        let max_age_ms = cfg.world_max_age_secs.saturating_mul(1000);
        if cfg.world.is_empty() {
            return Self {
                now_ms,
                max_age_ms,
                entries: BTreeMap::new(),
            };
        }
        let aliases: Vec<&String> = cfg.world.keys().collect();
        let keys: Vec<String> = cfg.world.values().cloned().collect();
        let rows: Vec<Entry> = match store {
            None => unavailable(keys.len(), "observation store unavailable"),
            Some(s) => match s.get_many(&keys).await {
                Ok(rows) => rows
                    .into_iter()
                    .map(|r| match r {
                        Some(o) => Entry::Row(Box::new(o)),
                        None => Entry::Missing,
                    })
                    .collect(),
                Err(e) => {
                    warn!(error = %format!("{e:#}"), "decision loop: world read failed");
                    unavailable(keys.len(), "observation store read failed")
                }
            },
        };
        Self {
            now_ms,
            max_age_ms,
            entries: aliases.into_iter().cloned().zip(rows).collect(),
        }
    }

    /// The row for `alias` when it is usable and at most `max_age_ms` old.
    fn usable_within(&self, alias: &str, max_age_ms: u64) -> Option<&Observation> {
        match self.entries.get(alias) {
            Some(Entry::Row(o)) if o.status.usable() && o.age_ms(self.now_ms) <= max_age_ms => {
                Some(o)
            }
            _ => None,
        }
    }

    /// `Observation::decision_root` of a fresh entry (`FromObservation` slots).
    pub(crate) fn fresh_root(&self, alias: &str) -> Option<Value> {
        self.usable_within(alias, self.max_age_ms)
            .map(|o| o.decision_root(self.now_ms))
    }

    /// Every `requires` alias is usable and within its own max age.
    pub(crate) fn satisfies(&self, requires: &BTreeMap<String, u64>) -> bool {
        requires.iter().all(|(alias, secs)| {
            self.usable_within(alias, secs.saturating_mul(1000))
                .is_some()
        })
    }

    /// The `state.world` object; `None` when the loop has no `world`.
    pub(crate) fn to_state(&self) -> Option<Value> {
        if self.entries.is_empty() {
            return None;
        }
        let mut out = Map::new();
        for (alias, entry) in &self.entries {
            let v = match entry {
                Entry::Missing => json!({"status": "missing"}),
                Entry::Unavailable(msg) => json!({
                    "status": "error",
                    "errors": [ReadError::new("store", ErrorClass::Transient, msg.clone())],
                }),
                Entry::Row(o) if !o.status.usable() => json!({
                    "status": "error",
                    "errors": o.errors,
                }),
                Entry::Row(o) if o.age_ms(self.now_ms) > self.max_age_ms => json!({
                    "status": "stale",
                    "age_s": o.meta(self.now_ms).age_s,
                }),
                Entry::Row(o) => o.decision_value(self.now_ms),
            };
            out.insert(alias.clone(), v);
        }
        Some(Value::Object(out))
    }
}

fn unavailable(n: usize, msg: &str) -> Vec<Entry> {
    (0..n)
        .map(|_| Entry::Unavailable(msg.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::observe::tests::MemStore;
    use crate::domain::observation::{ObsSource, ObsStatus};

    const MINT: &str = "So11111111111111111111111111111111111111112";

    fn cfg(world: &[(&str, &str)]) -> DecisionLoopConfig {
        let mut c: DecisionLoopConfig =
            toml::from_str("goal = \"g\"\nagent = \"a\"\n[actions.hold]\ndescription = \"x\"")
                .unwrap();
        c.world = world
            .iter()
            .map(|(a, k)| (a.to_string(), k.to_string()))
            .collect();
        c
    }

    fn price(at_ms: i64, status: ObsStatus) -> Observation {
        Observation {
            key: format!("price_oracle/1:{MINT}"),
            schema: "price_oracle/1".into(),
            tool: "sol_price".into(),
            observed_at_ms: at_ms,
            slot: None,
            ttl_ms: 10_000,
            source: ObsSource::Live,
            status,
            errors: vec![],
            headline: format!("sol_price {MINT} usd=150.25"),
            features: [("usd".to_string(), json!(150.25))].into(),
            data: json!({"mint": MINT, "usd": 150.25}),
        }
    }

    async fn world_with(row: Option<Observation>, now_ms: i64) -> World {
        let store = MemStore::default();
        if let Some(o) = row {
            store.put(&o).await.unwrap();
        }
        let key = format!("price_oracle/1:{MINT}");
        World::read(
            Some(&store),
            &cfg(&[("price", key.as_str()), ("gone", "x/1:y")]),
            now_ms,
        )
        .await
    }

    #[tokio::test]
    async fn fresh_entry_renders_features() {
        let w = world_with(Some(price(1_000, ObsStatus::Ok)), 6_000).await;
        let s = w.to_state().unwrap();
        assert_eq!(s["price"]["features"]["usd"], json!(150.25));
        assert_eq!(s["price"]["status"], json!("ok"));
        assert_eq!(s["gone"], json!({"status": "missing"}));
        assert!(w.fresh_root("price").is_some());
        assert!(w.satisfies(&BTreeMap::from([("price".to_string(), 30)])));
        assert!(!w.satisfies(&BTreeMap::from([("price".to_string(), 1)])));
        assert!(!w.satisfies(&BTreeMap::from([("gone".to_string(), 30)])));
    }

    #[tokio::test]
    async fn stale_entry_carries_no_numbers() {
        // 31 s old against the default 30 s world max age.
        let w = world_with(Some(price(0, ObsStatus::Ok)), 31_000).await;
        let s = w.to_state().unwrap();
        assert_eq!(s["price"], json!({"status": "stale", "age_s": 31.0}));
        assert!(!s.to_string().contains("150.25"), "{s}");
        assert!(w.fresh_root("price").is_none());
    }

    #[tokio::test]
    async fn no_store_is_an_error_entry() {
        let w = World::read(None, &cfg(&[("price", "price_oracle/1:x")]), 0).await;
        let s = w.to_state().unwrap();
        assert_eq!(s["price"]["status"], json!("error"));
        assert!(!w.satisfies(&BTreeMap::from([("price".to_string(), 30)])));
    }

    #[test]
    fn empty_world_is_omitted() {
        assert!(World::empty().to_state().is_none());
    }
}
