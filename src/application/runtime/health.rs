//! Runtime health of `tengu run`, written every `[runtime] heartbeat_secs` by
//! [`heartbeat_task`] and read back by `tengu doctor --live`
//! (`domain::runtime::live_verdict`):
//!
//! | Output | Where | When |
//! |---|---|---|
//! | `run-<sandbox>.json` (`Heartbeat`) | runtime state dir (`RuntimeStore`) | every beat; `stopping`, then `stopped` on shutdown |
//! | `loop/1:<name>` (`LoopHealth`) | the loop agent's observation store | every beat |
//! | `feed/1:<name>` (`FeedHealth`) | the store the feed registered with | once it has an item: every beat and on each report (items at most 1/s); `observed_at_ms` = last item |
//!
//! Feeds report through [`FeedWriter`] (the `[feeds]` scheduler, next wave).
//! Row writes are fail-soft (warn, doctrine #4).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::time::MissedTickBehavior;
use tracing::warn;

use super::loops::{LoopDispatch, LoopStats};
use super::{stopped, StopRx};
use crate::domain::observation::{now_ms, ErrorClass, ObsSource, Observation};
use crate::domain::runtime::{scrub_urls, FeedHealth, Heartbeat, LoopHealth, RunState};
use crate::ports::observation::ObservationStore;
use crate::ports::runtime::RuntimeStore;

/// Producer name on health rows (`Observation::tool`).
const PRODUCER: &str = "runtime";
/// Feed rows written by `item` reports at most this often.
const ITEM_ROW_EVERY_MS: i64 = 1_000;

struct FeedSlot {
    health: FeedHealth,
    store: Option<Arc<dyn ObservationStore>>,
    last_row_ms: i64,
}

/// Health of one `tengu run`: identity, the loop stores for `loop/1` rows
/// and the feed table.
pub(crate) struct HealthBoard {
    sandbox: String,
    holder: String,
    pid: u32,
    started_at_ms: i64,
    heartbeat_secs: u64,
    loop_stores: BTreeMap<String, Arc<dyn ObservationStore>>,
    feeds: Mutex<BTreeMap<String, FeedSlot>>,
}

impl HealthBoard {
    pub(crate) fn new(
        sandbox: &str,
        holder: &str,
        started_at_ms: i64,
        heartbeat_secs: u64,
        loop_stores: BTreeMap<String, Arc<dyn ObservationStore>>,
    ) -> Self {
        Self {
            sandbox: sandbox.to_string(),
            holder: holder.to_string(),
            pid: std::process::id(),
            started_at_ms,
            heartbeat_secs,
            loop_stores,
            feeds: Mutex::new(BTreeMap::new()),
        }
    }

    fn feeds(&self) -> MutexGuard<'_, BTreeMap<String, FeedSlot>> {
        self.feeds.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Register feed `name` (`connecting`); its `feed/1` rows go to `store`
    /// (the feed agent's observation store). Re-registering resets it.
    #[allow(dead_code)] // rt-scheduler (next wave) registers each [feeds.<n>] here
    pub(crate) fn feed(
        self: &Arc<Self>,
        name: &str,
        required: bool,
        stale_after_s: u64,
        store: Option<Arc<dyn ObservationStore>>,
        now_ms: i64,
    ) -> FeedWriter {
        self.feeds().insert(
            name.to_string(),
            FeedSlot {
                health: FeedHealth::new(name, required, stale_after_s, now_ms),
                store,
                last_row_ms: i64::MIN,
            },
        );
        FeedWriter {
            board: Arc::clone(self),
            name: name.to_string(),
        }
    }

    /// Apply `f` to feed `name`; write its row when the state changed or
    /// `force` (else at most every `ITEM_ROW_EVERY_MS`).
    async fn report(&self, name: &str, now_ms: i64, f: impl FnOnce(&mut FeedHealth)) {
        let write = {
            let mut feeds = self.feeds();
            let Some(slot) = feeds.get_mut(name) else {
                warn!(feed = %name, "health report for an unregistered feed");
                return;
            };
            let before = slot.health.state;
            f(&mut slot.health);
            let due = slot.health.state != before
                || now_ms.saturating_sub(slot.last_row_ms) >= ITEM_ROW_EVERY_MS;
            match (&slot.store, due) {
                (Some(store), true) => {
                    slot.last_row_ms = now_ms;
                    Some((Arc::clone(store), slot.health.clone()))
                }
                _ => None,
            }
        };
        if let Some((store, health)) = write {
            put_feed_row(&*store, &health, self.heartbeat_secs).await;
        }
    }

    /// One beat at `now_ms`: refresh feeds (`live` → `stalled`), write the
    /// `loop/1` and `feed/1` rows, return the heartbeat to persist.
    pub(crate) async fn beat(
        &self,
        loops: &BTreeMap<String, LoopStats>,
        state: RunState,
        stop_reason: Option<&str>,
        now_ms: i64,
    ) -> Heartbeat {
        let feeds: Vec<(FeedHealth, Option<Arc<dyn ObservationStore>>)> = {
            let mut feeds = self.feeds();
            feeds
                .values_mut()
                .map(|slot| {
                    slot.health.refresh(now_ms);
                    slot.last_row_ms = now_ms;
                    (slot.health.clone(), slot.store.clone())
                })
                .collect()
        };
        let loops: BTreeMap<String, LoopHealth> = loops
            .iter()
            .map(|(name, s)| (name.clone(), loop_health(name, s, now_ms)))
            .collect();
        let ttl_ms = self.heartbeat_secs.saturating_mul(3_000);
        for (name, health) in &loops {
            if let Some(store) = self.loop_stores.get(name) {
                let row = Observation::of(PRODUCER, health, now_ms, ttl_ms, ObsSource::Live);
                put_row(&**store, &row).await;
            }
        }
        for (health, store) in &feeds {
            if let Some(store) = store {
                put_feed_row(&**store, health, self.heartbeat_secs).await;
            }
        }
        Heartbeat {
            sandbox: self.sandbox.clone(),
            pid: self.pid,
            holder: self.holder.clone(),
            state,
            stop_reason: stop_reason.map(str::to_string),
            started_at_ms: self.started_at_ms,
            ts_ms: now_ms,
            heartbeat_secs: self.heartbeat_secs,
            loops,
            feeds: feeds
                .into_iter()
                .map(|(h, _)| (h.name.clone(), h))
                .collect(),
        }
    }
}

/// One feed's reporting handle (cheap to clone).
#[derive(Clone)]
pub(crate) struct FeedWriter {
    board: Arc<HealthBoard>,
    name: String,
}

#[allow(dead_code)] // rt-scheduler (next wave) reports through these
impl FeedWriter {
    /// An item arrived (poll answered, message received): `live`.
    pub(crate) async fn item(&self, now_ms: i64) {
        self.board
            .report(&self.name, now_ms, |h| h.on_item(now_ms))
            .await;
    }

    /// A read failed: `backoff` while retrying, else `down`. `message` is
    /// stored without URLs.
    pub(crate) async fn error(
        &self,
        class: ErrorClass,
        message: &str,
        retrying: bool,
        now_ms: i64,
    ) {
        self.board
            .report(&self.name, now_ms, |h| {
                h.on_error(class, message, retrying, now_ms)
            })
            .await;
    }

    /// (Re)connecting: `connecting`, one more reconnect.
    pub(crate) async fn reconnecting(&self, now_ms: i64) {
        self.board
            .report(&self.name, now_ms, |h| h.on_reconnect(now_ms))
            .await;
    }

    /// Items dropped (queue full, coalesced away).
    pub(crate) async fn dropped(&self, n: u64, now_ms: i64) {
        self.board
            .report(&self.name, now_ms, |h| h.on_dropped(n, now_ms))
            .await;
    }
}

/// Counters → `loop/1` payload at `now_ms`.
fn loop_health(name: &str, s: &LoopStats, now_ms: i64) -> LoopHealth {
    LoopHealth {
        name: name.to_string(),
        queue_depth: s.queued,
        in_flight: s.in_flight,
        accepted: s.accepted,
        completed: s.completed,
        failed: s.failed,
        dropped: s.dropped,
        last_event_at_ms: s.last_event_at_ms,
        last_decision_at_ms: s.last_done_at_ms,
        last_decision_age_s: s
            .last_done_at_ms
            .map(|t| (now_ms.saturating_sub(t).max(0) as f64 / 100.0).round() / 10.0),
        last_error: s.last_error.as_deref().map(scrub_urls),
        updated_at_ms: now_ms,
    }
}

/// `feed/1` row stamped with the last item; none before the first item.
async fn put_feed_row(store: &dyn ObservationStore, health: &FeedHealth, heartbeat_secs: u64) {
    let Some(at_ms) = health.row_at_ms() else {
        return;
    };
    let ttl_ms = health
        .stale_after_s
        .max(heartbeat_secs)
        .saturating_mul(1000);
    let row = Observation::of(PRODUCER, health, at_ms, ttl_ms, ObsSource::Live);
    put_row(store, &row).await;
}

async fn put_row(store: &dyn ObservationStore, row: &Observation) {
    if let Err(e) = store.put(row).await {
        let error = format!("{e:#}");
        warn!(key = %row.key, %error, "health row write failed");
    }
}

/// Beat every `heartbeat_secs` (first beat at once) until the stop signal:
/// rows + `run-<sandbox>.json`. The final `stopping` / `stopped` beats are
/// written by the runtime's shutdown.
pub(crate) async fn heartbeat_task(
    board: Arc<HealthBoard>,
    loops: Arc<LoopDispatch>,
    store: Arc<dyn RuntimeStore>,
    mut stop: StopRx,
) {
    let mut tick = tokio::time::interval(Duration::from_secs(board.heartbeat_secs.max(1)));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = stopped(&mut stop) => return,
            _ = tick.tick() => {}
        }
        let hb = board
            .beat(&loops.stats(), RunState::Running, None, now_ms())
            .await;
        write_beat(&*store, &hb).await;
    }
}

/// Persist `hb`; fail-soft (a missing heartbeat makes `doctor --live` fail,
/// which is the signal).
pub(crate) async fn write_beat(store: &dyn RuntimeStore, hb: &Heartbeat) {
    if let Err(e) = store.write_heartbeat(hb).await {
        let error = format!("{e:#}");
        warn!(sandbox = %hb.sandbox, %error, "heartbeat write failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::observe::tests::MemStore;
    use crate::domain::runtime::{FeedState, RunnerLease};

    const T0: i64 = 1_790_000_000_000;

    fn board(loop_store: Option<Arc<MemStore>>) -> Arc<HealthBoard> {
        let stores = loop_store
            .map(|s| BTreeMap::from([("xm_main".to_string(), s as Arc<dyn ObservationStore>)]))
            .unwrap_or_default();
        Arc::new(HealthBoard::new(
            "xmarket",
            "vps-1:4242:0f8c6a52-5d0e-4c8e-9a71-3b2f1c9d7e44",
            T0 - 60_000,
            5,
            stores,
        ))
    }

    async fn row(store: &MemStore, key: &str) -> Option<Observation> {
        store.get(key).await.unwrap()
    }

    #[tokio::test]
    async fn feed_rows_follow_the_writer_and_are_stamped_with_the_last_item() {
        let store = Arc::new(MemStore::default());
        let b = board(None);
        let w = b.feed("hl_ctx", true, 60, Some(store.clone()), T0);
        w.reconnecting(T0 + 10).await;
        assert!(
            row(&store, "feed/1:hl_ctx").await.is_none(),
            "no row before the first item"
        );
        w.item(T0 + 1_000).await;
        let r = row(&store, "feed/1:hl_ctx").await.unwrap();
        assert_eq!(r.observed_at_ms, T0 + 1_000);
        assert_eq!(r.features["state"], serde_json::json!("live"));
        // Items inside 1 s do not rewrite the row; a state change does.
        w.item(T0 + 1_500).await;
        assert_eq!(
            row(&store, "feed/1:hl_ctx").await.unwrap().observed_at_ms,
            T0 + 1_000
        );
        w.error(
            ErrorClass::RateLimited,
            "429 from https://api.hyperliquid.xyz/info",
            true,
            T0 + 1_600,
        )
        .await;
        let r = row(&store, "feed/1:hl_ctx").await.unwrap();
        assert_eq!(r.observed_at_ms, T0 + 1_500, "stamped with the last item");
        let h: FeedHealth = r.typed().unwrap();
        assert_eq!((h.state, h.items), (FeedState::Backoff, 2));
        assert_eq!(h.last_error.as_deref(), Some("429 from <url>"));
        w.dropped(4, T0 + 1_700).await;
        // Quiet past stale_after_s: the beat marks it stalled.
        let hb = b
            .beat(&BTreeMap::new(), RunState::Running, None, T0 + 90_000)
            .await;
        assert_eq!(
            hb.feeds["hl_ctx"].state,
            FeedState::Backoff,
            "backoff is not live"
        );
        w.item(T0 + 91_000).await;
        let hb = b
            .beat(&BTreeMap::new(), RunState::Running, None, T0 + 200_000)
            .await;
        assert_eq!(hb.feeds["hl_ctx"].state, FeedState::Stalled);
        assert_eq!(hb.feeds["hl_ctx"].dropped, 4);
        let h: FeedHealth = row(&store, "feed/1:hl_ctx").await.unwrap().typed().unwrap();
        assert_eq!(h.state, FeedState::Stalled);
        assert_eq!(h.updated_at_ms, T0 + 200_000);
    }

    #[tokio::test]
    async fn beat_writes_loop_rows_and_the_heartbeat_payload() {
        let store = Arc::new(MemStore::default());
        let b = board(Some(store.clone()));
        let stats = BTreeMap::from([(
            "xm_main".to_string(),
            LoopStats {
                queued: 2,
                in_flight: 1,
                accepted: 5,
                completed: 1,
                failed: 1,
                dropped: 0,
                last_event_at_ms: Some(T0 - 1_000),
                last_done_at_ms: Some(T0 - 14_000),
                last_error: Some("error sending request for url (http://127.0.0.1:9/api)".into()),
            },
        )]);
        let hb = b
            .beat(&stats, RunState::Stopping, Some("SIGTERM"), T0)
            .await;
        assert_eq!(hb.state, RunState::Stopping);
        assert_eq!(hb.stop_reason.as_deref(), Some("SIGTERM"));
        assert_eq!(
            (hb.ts_ms, hb.started_at_ms, hb.heartbeat_secs),
            (T0, T0 - 60_000, 5)
        );
        assert_eq!(hb.pid, std::process::id());
        let l = &hb.loops["xm_main"];
        assert_eq!(
            (l.queue_depth, l.in_flight, l.last_decision_age_s),
            (2, 1, Some(14.0))
        );
        assert_eq!(
            l.last_error.as_deref(),
            Some("error sending request for url (<url>)")
        );
        let r = row(&store, "loop/1:xm_main").await.unwrap();
        assert_eq!((r.observed_at_ms, r.ttl_ms), (T0, 15_000));
        assert_eq!(r.features["queue_depth"], serde_json::json!(2));
        assert_eq!(r.typed::<LoopHealth>().unwrap(), *l);
    }

    /// Records every heartbeat written.
    #[derive(Default)]
    struct Beats(std::sync::Mutex<Vec<Heartbeat>>);

    #[async_trait::async_trait]
    impl RuntimeStore for Beats {
        async fn acquire_lease(
            &self,
            _r: &str,
            _h: &str,
            _t: i64,
            _n: i64,
        ) -> anyhow::Result<RunnerLease> {
            anyhow::bail!("unused")
        }
        async fn release_lease(&self, _r: &str, _h: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn write_heartbeat(&self, hb: &Heartbeat) -> anyhow::Result<()> {
            self.0.lock().unwrap().push(hb.clone());
            Ok(())
        }
    }

    #[tokio::test]
    async fn heartbeat_task_beats_at_once_and_returns_on_stop() {
        let stopper = super::super::Supervisor::new().stopper();
        let beats = Arc::new(Beats::default());
        let loops = Arc::new(LoopDispatch::new(BTreeMap::new(), 1));
        let task = tokio::spawn(heartbeat_task(
            board(None),
            loops,
            beats.clone(),
            stopper.subscribe(),
        ));
        for _ in 0..200 {
            if !beats.0.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        stopper.stop("SIGINT", false);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("heartbeat task returns on stop")
            .unwrap();
        let beats = beats.0.lock().unwrap();
        assert_eq!(
            beats.len(),
            1,
            "first beat at once, the next only after 5 s"
        );
        assert_eq!(beats[0].state, RunState::Running);
        assert_eq!(beats[0].sandbox, "xmarket");
    }
}
