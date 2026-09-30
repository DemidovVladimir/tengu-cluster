//! Loop events of one process (`tengu run`, `tengu webhooks` loop
//! endpoints; the `[feeds]` scheduler submits here next wave). Every loop is
//! built once, so its history and state live in this one place.
//!
//! | Rule | How |
//! |---|---|
//! | one event at a time per loop, in arrival order | per-loop `turn` mutex (tokio's is FIFO) |
//! | ≤ `[runtime] max_decisions_in_flight` running across loops | one FIFO `Semaphore` |
//! | shutdown ([`LoopDispatch::drain`]) | new events refused ([`Refused::ShuttingDown`]), queued ones dropped, running ones get until the deadline, then are aborted |
//! | stats ([`LoopStats`]) | queued · in flight · accepted · completed · failed · dropped · last event / finish times · last error |

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::{watch, Semaphore};
use tokio::task::JoinSet;
use tokio::time::Instant;
use tracing::warn;

use crate::application::decision_loop::DecisionLoop;
use crate::domain::observation::now_ms;

/// One loop's event handler: `DecisionLoop`, fakes in tests.
#[async_trait]
pub(crate) trait LoopHandler: Send + Sync {
    async fn handle(&self, event: &Value, session_id: &str) -> anyhow::Result<()>;
}

#[async_trait]
impl LoopHandler for DecisionLoop {
    /// Outcomes are in the decision audit; the runtime keeps counts only.
    async fn handle(&self, event: &Value, session_id: &str) -> anyhow::Result<()> {
        self.handle_event(event, session_id).await.map(|_| ())
    }
}

/// Why an event was not accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refused {
    UnknownLoop,
    ShuttingDown,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Refused::UnknownLoop => "no such decision loop in this process",
            Refused::ShuttingDown => "shutting down — not accepting loop events",
        })
    }
}

/// Counters of one loop since the process started.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LoopStats {
    /// Accepted, waiting for the loop or an in-flight slot.
    pub queued: u32,
    /// Running now (0 or 1).
    pub in_flight: u32,
    pub accepted: u64,
    /// Finished `Ok`.
    pub completed: u64,
    /// Finished with an error.
    pub failed: u64,
    /// Refused or dropped at shutdown, or aborted at the deadline.
    pub dropped: u64,
    pub last_event_at_ms: Option<i64>,
    /// When the last event finished (completed or failed).
    pub last_done_at_ms: Option<i64>,
    pub last_error: Option<String>,
}

/// What [`LoopDispatch::drain`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct DrainReport {
    /// Running events that finished inside the grace period.
    pub finished: u64,
    /// Queued events dropped without running.
    pub dropped: u64,
    /// Running events aborted at the deadline.
    pub aborted: u64,
}

struct Slot {
    name: String,
    handler: Arc<dyn LoopHandler>,
    turn: Arc<tokio::sync::Mutex<()>>,
    stats: Mutex<LoopStats>,
}

impl Slot {
    fn stats(&self) -> MutexGuard<'_, LoopStats> {
        self.stats.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Every loop of the process + the shared in-flight cap.
pub(crate) struct LoopDispatch {
    slots: BTreeMap<String, Arc<Slot>>,
    permits: Arc<Semaphore>,
    closing: watch::Sender<bool>,
    tasks: Mutex<JoinSet<()>>,
}

impl LoopDispatch {
    /// `max_in_flight` < 1 is treated as 1 (config validation refuses 0).
    pub(crate) fn new(
        handlers: BTreeMap<String, Arc<dyn LoopHandler>>,
        max_in_flight: usize,
    ) -> Self {
        let slots = handlers
            .into_iter()
            .map(|(name, handler)| {
                let slot = Slot {
                    name: name.clone(),
                    handler,
                    turn: Arc::new(tokio::sync::Mutex::new(())),
                    stats: Mutex::new(LoopStats::default()),
                };
                (name, Arc::new(slot))
            })
            .collect();
        Self {
            slots,
            permits: Arc::new(Semaphore::new(max_in_flight.max(1))),
            closing: watch::channel(false).0,
            tasks: Mutex::new(JoinSet::new()),
        }
    }

    pub(crate) fn names(&self) -> Vec<String> {
        self.slots.keys().cloned().collect()
    }

    #[cfg_attr(not(feature = "webhooks"), allow(dead_code))] // webhooks; feeds next wave
    pub(crate) fn has(&self, name: &str) -> bool {
        self.slots.contains_key(name)
    }

    /// Per-loop counters, by loop name.
    pub(crate) fn stats(&self) -> BTreeMap<String, LoopStats> {
        self.slots
            .iter()
            .map(|(n, s)| (n.clone(), s.stats().clone()))
            .collect()
    }

    /// Queue `event` for `loop_name` and return at once; it runs when the
    /// loop and an in-flight slot are free. Refused after [`Self::drain`]
    /// began. Callers: webhook loop endpoints; the `[feeds]` scheduler next
    /// wave.
    #[cfg_attr(not(feature = "webhooks"), allow(dead_code))]
    pub(crate) fn submit(
        &self,
        loop_name: &str,
        event: Value,
        session_id: String,
    ) -> Result<(), Refused> {
        let slot = Arc::clone(self.slots.get(loop_name).ok_or(Refused::UnknownLoop)?);
        let mut tasks = self.tasks.lock().unwrap_or_else(PoisonError::into_inner);
        if *self.closing.borrow() {
            slot.stats().dropped += 1;
            return Err(Refused::ShuttingDown);
        }
        while tasks.try_join_next().is_some() {}
        let ticket = Ticket::queue(Arc::clone(&slot));
        let permits = Arc::clone(&self.permits);
        let mut closing = self.closing.subscribe();
        tasks.spawn(async move {
            let mut ticket = ticket;
            let acquired = tokio::select! {
                biased;
                _ = closing.wait_for(|c| *c) => None,
                got = async {
                    let turn = Arc::clone(&slot.turn).lock_owned().await;
                    let permit = Arc::clone(&permits).acquire_owned().await;
                    (turn, permit)
                } => Some(got),
            };
            let Some((_turn, Ok(_permit))) = acquired else {
                warn!(decision_loop = %slot.name, session_id = %session_id, "loop event dropped at shutdown (never ran)");
                return;
            };
            ticket.start();
            let result = slot.handler.handle(&event, &session_id).await;
            if let Err(e) = &result {
                let error = format!("{e:#}");
                warn!(decision_loop = %slot.name, session_id = %session_id, %error, "decision loop event failed");
            }
            ticket.finish(result.map_err(|e| format!("{e:#}")));
        });
        Ok(())
    }

    /// Refuse new events, drop queued ones, let running ones finish until
    /// `deadline`, then abort them.
    pub(crate) async fn drain(&self, deadline: Instant) -> DrainReport {
        // Counters before the queue can react to `closing`; `submit` waits on
        // the same lock, so nothing new is accepted in between.
        let (mut set, before) = {
            let mut tasks = self.tasks.lock().unwrap_or_else(PoisonError::into_inner);
            let before = self.totals();
            self.closing.send_replace(true);
            (std::mem::take(&mut *tasks), before)
        };
        let mut aborted = 0;
        loop {
            match tokio::time::timeout_at(deadline, set.join_next()).await {
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => {
                    set.abort_all();
                    while let Some(r) = set.join_next().await {
                        if matches!(&r, Err(e) if e.is_cancelled()) {
                            aborted += 1;
                        }
                    }
                    break;
                }
            }
        }
        let after = self.totals();
        DrainReport {
            finished: after.finished - before.finished,
            dropped: (after.dropped - before.dropped).saturating_sub(aborted),
            aborted,
        }
    }

    fn totals(&self) -> DrainReport {
        self.slots.values().fold(DrainReport::default(), |acc, s| {
            let st = s.stats();
            DrainReport {
                finished: acc.finished + st.completed + st.failed,
                dropped: acc.dropped + st.dropped,
                aborted: 0,
            }
        })
    }
}

/// One accepted event's place in its loop's counters. Dropping it before
/// `finish` (shutdown, abort) counts the event as dropped.
struct Ticket {
    slot: Arc<Slot>,
    running: bool,
    done: bool,
}

impl Ticket {
    fn queue(slot: Arc<Slot>) -> Self {
        {
            let mut st = slot.stats();
            st.accepted += 1;
            st.queued += 1;
            st.last_event_at_ms = Some(now_ms());
        }
        Self {
            slot,
            running: false,
            done: false,
        }
    }

    fn start(&mut self) {
        let mut st = self.slot.stats();
        st.queued = st.queued.saturating_sub(1);
        st.in_flight += 1;
        self.running = true;
    }

    fn finish(&mut self, result: Result<(), String>) {
        let mut st = self.slot.stats();
        st.in_flight = st.in_flight.saturating_sub(1);
        st.last_done_at_ms = Some(now_ms());
        match result {
            Ok(()) => st.completed += 1,
            Err(e) => {
                st.failed += 1;
                st.last_error = Some(e);
            }
        }
        self.done = true;
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let mut st = self.slot.stats();
        st.dropped += 1;
        if self.running {
            st.in_flight = st.in_flight.saturating_sub(1);
            st.last_error = Some("aborted at the shutdown deadline".into());
        } else {
            st.queued = st.queued.saturating_sub(1);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::time::Duration;

    /// Sleeps `ms` per event and records `(session_id, start order)`.
    pub(crate) struct SlowLoop {
        pub ms: u64,
        pub fail: bool,
        pub seen: Mutex<Vec<String>>,
        pub running: std::sync::atomic::AtomicUsize,
        pub max_running: std::sync::atomic::AtomicUsize,
    }

    impl SlowLoop {
        pub(crate) fn new(ms: u64) -> Arc<Self> {
            Arc::new(Self {
                ms,
                fail: false,
                seen: Mutex::new(vec![]),
                running: Default::default(),
                max_running: Default::default(),
            })
        }
    }

    #[async_trait]
    impl LoopHandler for SlowLoop {
        async fn handle(&self, _event: &Value, session_id: &str) -> anyhow::Result<()> {
            use std::sync::atomic::Ordering::SeqCst;
            self.seen.lock().unwrap().push(session_id.to_string());
            let now = self.running.fetch_add(1, SeqCst) + 1;
            self.max_running.fetch_max(now, SeqCst);
            tokio::time::sleep(Duration::from_millis(self.ms)).await;
            self.running.fetch_sub(1, SeqCst);
            if self.fail {
                anyhow::bail!("jev 503");
            }
            Ok(())
        }
    }

    fn dispatch(loops: &[(&str, Arc<SlowLoop>)], max: usize) -> LoopDispatch {
        LoopDispatch::new(
            loops
                .iter()
                .map(|(n, l)| (n.to_string(), Arc::clone(l) as Arc<dyn LoopHandler>))
                .collect(),
            max,
        )
    }

    async fn settle(d: &LoopDispatch, loop_name: &str, done: u64) {
        for _ in 0..500 {
            let st = &d.stats()[loop_name];
            if st.completed + st.failed >= done {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("loop {loop_name} did not finish {done} events");
    }

    #[tokio::test]
    async fn one_event_at_a_time_per_loop_in_arrival_order() {
        let a = SlowLoop::new(20);
        let d = dispatch(&[("a", a.clone())], 4);
        for i in 0..3 {
            d.submit("a", Value::Null, format!("s{i}")).unwrap();
        }
        assert_eq!(d.stats()["a"].accepted, 3);
        settle(&d, "a", 3).await;
        assert_eq!(*a.seen.lock().unwrap(), ["s0", "s1", "s2"]);
        assert_eq!(a.max_running.load(std::sync::atomic::Ordering::SeqCst), 1);
        let st = &d.stats()["a"];
        assert_eq!((st.queued, st.in_flight, st.completed), (0, 0, 3));
        assert!(st.last_done_at_ms.is_some() && st.last_event_at_ms.is_some());
    }

    #[tokio::test]
    async fn global_cap_bounds_events_across_loops() {
        let (a, b) = (SlowLoop::new(40), SlowLoop::new(40));
        let d = dispatch(&[("a", a.clone()), ("b", b.clone())], 1);
        d.submit("a", Value::Null, "sa".into()).unwrap();
        d.submit("b", Value::Null, "sb".into()).unwrap();
        tokio::time::sleep(Duration::from_millis(15)).await;
        let running: u32 = d.stats().values().map(|s| s.in_flight).sum();
        let queued: u32 = d.stats().values().map(|s| s.queued).sum();
        assert_eq!((running, queued), (1, 1));
        settle(&d, "a", 1).await;
        settle(&d, "b", 1).await;
    }

    #[tokio::test]
    async fn unknown_loop_and_failures_are_reported() {
        let mut f = SlowLoop::new(1);
        Arc::get_mut(&mut f).unwrap().fail = true;
        let d = dispatch(&[("f", f)], 2);
        assert_eq!(
            d.submit("nope", Value::Null, "s".into()),
            Err(Refused::UnknownLoop)
        );
        d.submit("f", Value::Null, "s".into()).unwrap();
        settle(&d, "f", 1).await;
        let st = &d.stats()["f"];
        assert_eq!((st.failed, st.completed), (1, 0));
        assert_eq!(st.last_error.as_deref(), Some("jev 503"));
    }

    #[tokio::test]
    async fn drain_lets_the_running_event_finish_and_drops_the_queue() {
        let a = SlowLoop::new(150);
        let d = dispatch(&[("a", a.clone())], 4);
        d.submit("a", Value::Null, "running".into()).unwrap();
        d.submit("a", Value::Null, "queued".into()).unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        let report = d.drain(Instant::now() + Duration::from_secs(5)).await;
        assert_eq!(
            report,
            DrainReport {
                finished: 1,
                dropped: 1,
                aborted: 0
            }
        );
        assert_eq!(*a.seen.lock().unwrap(), ["running"]);
        assert_eq!(
            d.submit("a", Value::Null, "late".into()),
            Err(Refused::ShuttingDown)
        );
        let st = &d.stats()["a"];
        assert_eq!(
            (st.completed, st.dropped, st.queued, st.in_flight),
            (1, 2, 0, 0)
        );
    }

    #[tokio::test]
    async fn drain_aborts_what_outlives_the_deadline() {
        let a = SlowLoop::new(60_000);
        let d = dispatch(&[("a", a)], 4);
        d.submit("a", Value::Null, "stuck".into()).unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        let t0 = std::time::Instant::now();
        let report = d.drain(Instant::now() + Duration::from_millis(100)).await;
        assert!(t0.elapsed() < Duration::from_secs(5));
        assert_eq!(
            report,
            DrainReport {
                finished: 0,
                dropped: 0,
                aborted: 1
            }
        );
        let st = &d.stats()["a"];
        assert_eq!((st.in_flight, st.dropped), (0, 1));
        assert_eq!(
            st.last_error.as_deref(),
            Some("aborted at the shutdown deadline")
        );
    }
}
