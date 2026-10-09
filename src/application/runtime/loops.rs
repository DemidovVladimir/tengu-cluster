//! Loop events of one process (`tengu run`, `tengu webhooks` loop
//! endpoints; `[feeds]` ticks via [`LoopDispatch::submit_tracked`]). Every
//! loop is built once, so its history and state live in this one place.
//!
//! | Rule | How |
//! |---|---|
//! | one event at a time per loop, in arrival order | per-loop `turn` mutex (tokio's is FIFO) |
//! | ≤ `[runtime] max_decisions_in_flight` running across loops | one FIFO `Semaphore` |
//! | ≤ `[runtime] max_queued_per_loop` waiting per loop | one more is refused ([`Refused::QueueFull`]) with a warn, counted dropped, `last_error` set |
//! | shutdown ([`LoopDispatch::drain`]) | new events refused ([`Refused::ShuttingDown`]), queued ones dropped, running ones get until the deadline, then are aborted |
//! | stats ([`LoopStats`]) | queued · in flight · accepted · completed · failed · dropped · last event / finish times · last error |
//! | trace ([`LoopDispatch::with_trace`]) | `loop.queued` (`Pending`, parent = the sender's `trace_exec::cause`: a tick's `feed.fired`) → `loop.started` (`Running`, `waited_ms`) → `loop.completed` (`Ok`) / `loop.failed` (`Failed`, `error`) with `duration_ms`; `loop.dropped` (`Dropped`: queued at shutdown, or aborted at the deadline); `loop.refused` (`Refused`: `unknown_loop` · `shutting_down` · `queue_full`); each the one before's child, node `loop:<name>`, session = the event's; payload `stats` = the counters right after (the same [`LoopStats`] the heartbeat writes). The handler runs caused by `loop.started` |

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::{oneshot, watch, Semaphore};
use tokio::task::JoinSet;
use tokio::time::Instant;
use tracing::warn;

use crate::application::decision_loop::DecisionLoop;
use crate::application::trace_exec::{self, Cause};
use crate::domain::observation::now_ms;
use crate::domain::trace::{Component, EventDraft, Status};
use crate::domain::workflow::node_id;
use crate::ports::trace::TraceSink;

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
    /// `max` events (`[runtime] max_queued_per_loop`) already wait for the loop.
    QueueFull {
        max: usize,
    },
}

impl Refused {
    /// `loop.refused` payload `reason`.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Refused::UnknownLoop => "unknown_loop",
            Refused::ShuttingDown => "shutting_down",
            Refused::QueueFull { .. } => "queue_full",
        }
    }
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::UnknownLoop => f.write_str("no such decision loop in this process"),
            Refused::ShuttingDown => f.write_str("shutting down — not accepting loop events"),
            Refused::QueueFull { max } => write!(
                f,
                "decision loop queue full: {max} events already waiting ([runtime] \
                 max_queued_per_loop) — event refused, retry later"
            ),
        }
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

impl LoopStats {
    /// The counters as a trace payload carries them.
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "queued": self.queued,
            "in_flight": self.in_flight,
            "accepted": self.accepted,
            "completed": self.completed,
            "failed": self.failed,
            "dropped": self.dropped,
        })
    }
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
    /// Waiting events one loop may hold (`[runtime] max_queued_per_loop`).
    max_queued: usize,
    closing: watch::Sender<bool>,
    tasks: Mutex<JoinSet<()>>,
    /// Where `loop.*` events go ([`Self::with_trace`]); `None` = nowhere.
    trace: Option<Arc<dyn TraceSink>>,
}

impl LoopDispatch {
    /// `max_in_flight` / `max_queued` < 1 are treated as 1 (config
    /// validation refuses 0).
    pub(crate) fn new(
        handlers: BTreeMap<String, Arc<dyn LoopHandler>>,
        max_in_flight: usize,
        max_queued: usize,
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
            max_queued: max_queued.max(1),
            closing: watch::channel(false).0,
            tasks: Mutex::new(JoinSet::new()),
            trace: None,
        }
    }

    /// Write `loop.*` events to `sink` (module table).
    pub(crate) fn with_trace(mut self, sink: Arc<dyn TraceSink>) -> Self {
        self.trace = Some(sink);
        self
    }

    pub(crate) fn names(&self) -> Vec<String> {
        self.slots.keys().cloned().collect()
    }

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
    /// began, and while `max_queued` events already wait for the loop.
    /// Callers: webhook loop endpoints, Studio's send-event.
    #[cfg_attr(not(any(feature = "webhooks", feature = "studio")), allow(dead_code))]
    pub(crate) fn submit(
        &self,
        loop_name: &str,
        event: Value,
        session_id: String,
    ) -> Result<(), Refused> {
        self.enqueue(loop_name, event, session_id, None)
    }

    /// [`Self::submit`], plus a receiver that closes once the event is done
    /// — finished, failed, dropped at shutdown or aborted. The `[feeds]`
    /// scheduler sends a feed's next tick only then.
    pub(crate) fn submit_tracked(
        &self,
        loop_name: &str,
        event: Value,
        session_id: String,
    ) -> Result<oneshot::Receiver<()>, Refused> {
        let (done, rx) = oneshot::channel();
        self.enqueue(loop_name, event, session_id, Some(done))?;
        Ok(rx)
    }

    fn enqueue(
        &self,
        loop_name: &str,
        event: Value,
        session_id: String,
        done: Option<oneshot::Sender<()>>,
    ) -> Result<(), Refused> {
        let parent = trace_exec::cause().and_then(|c| c.parent);
        let Some(slot) = self.slots.get(loop_name).map(Arc::clone) else {
            self.refused(loop_name, &session_id, parent, Refused::UnknownLoop, None);
            return Err(Refused::UnknownLoop);
        };
        let mut tasks = self.tasks.lock().unwrap_or_else(PoisonError::into_inner);
        if *self.closing.borrow() {
            let stats = {
                let mut st = slot.stats();
                st.dropped += 1;
                st.clone()
            };
            let refused = Refused::ShuttingDown;
            self.refused(loop_name, &session_id, parent, refused, Some(&stats));
            return Err(refused);
        }
        // Enqueues serialise on `tasks`, so the count cannot grow between
        // this check and `Ticket::queue`.
        let queued = slot.stats().queued;
        if queued as usize >= self.max_queued {
            let refused = Refused::QueueFull {
                max: self.max_queued,
            };
            let stats = {
                let mut st = slot.stats();
                st.dropped += 1;
                st.last_error = Some(refused.to_string());
                st.clone()
            };
            warn!(decision_loop = %slot.name, session_id = %session_id, queued, max_queued = self.max_queued, "loop event refused: queue full");
            self.refused(loop_name, &session_id, parent, refused, Some(&stats));
            return Err(refused);
        }
        while tasks.try_join_next().is_some() {}
        let trace = self.trace.clone().map(|sink| TicketTrace {
            sink,
            session_id: session_id.clone(),
            last: parent,
            since: std::time::Instant::now(),
        });
        let ticket = Ticket::queue(Arc::clone(&slot), trace);
        let permits = Arc::clone(&self.permits);
        let mut closing = self.closing.subscribe();
        tasks.spawn(async move {
            // Dropped with the task, whatever ends it: closes the receiver.
            let _done = done;
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
            let cause = Cause::new(ticket.last_event(), session_id.clone());
            let result =
                trace_exec::caused_by(cause, slot.handler.handle(&event, &session_id)).await;
            if let Err(e) = &result {
                let error = format!("{e:#}");
                warn!(decision_loop = %slot.name, session_id = %session_id, %error, "decision loop event failed");
            }
            ticket.finish(result.map_err(|e| format!("{e:#}")));
        });
        Ok(())
    }

    /// `loop.refused` (module table): `stats` = the loop's counters after
    /// the refusal (`None`: no such loop).
    fn refused(
        &self,
        loop_name: &str,
        session_id: &str,
        parent: Option<String>,
        why: Refused,
        stats: Option<&LoopStats>,
    ) {
        let Some(sink) = &self.trace else { return };
        let mut d = EventDraft::new(Component::Loop, "loop.refused", Status::Refused)
            .session(session_id)
            .node(node_id::loop_(loop_name))
            .payload(json!({
                "loop": loop_name,
                "reason": why.as_str(),
                "error": why.to_string(),
                "stats": stats.map(LoopStats::to_json),
            }));
        if let Some(p) = parent {
            d = d.parent(p);
        }
        sink.emit(d);
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
    trace: Option<TicketTrace>,
}

/// A ticket's `loop.*` events (module table): each one's parent is the
/// one before.
struct TicketTrace {
    sink: Arc<dyn TraceSink>,
    session_id: String,
    /// The last event written (the first's parent: the sender's cause).
    last: Option<String>,
    /// Queued, then started: `waited_ms` / `duration_ms`.
    since: std::time::Instant,
}

impl Ticket {
    fn queue(slot: Arc<Slot>, trace: Option<TicketTrace>) -> Self {
        let stats = {
            let mut st = slot.stats();
            st.accepted += 1;
            st.queued += 1;
            st.last_event_at_ms = Some(now_ms());
            st.clone()
        };
        let mut t = Self {
            slot,
            running: false,
            done: false,
            trace,
        };
        t.emit("loop.queued", Status::Pending, &stats, json!({}), false);
        t
    }

    fn start(&mut self) {
        let stats = {
            let mut st = self.slot.stats();
            st.queued = st.queued.saturating_sub(1);
            st.in_flight += 1;
            st.clone()
        };
        self.running = true;
        let waited_ms = self
            .trace
            .as_ref()
            .map(|t| t.since.elapsed().as_millis() as u64);
        self.emit(
            "loop.started",
            Status::Running,
            &stats,
            json!({"waited_ms": waited_ms}),
            false,
        );
        if let Some(t) = &mut self.trace {
            t.since = std::time::Instant::now();
        }
    }

    fn finish(&mut self, result: Result<(), String>) {
        let (stats, error) = {
            let mut st = self.slot.stats();
            st.in_flight = st.in_flight.saturating_sub(1);
            st.last_done_at_ms = Some(now_ms());
            let error = match result {
                Ok(()) => {
                    st.completed += 1;
                    None
                }
                Err(e) => {
                    st.failed += 1;
                    st.last_error = Some(e.clone());
                    Some(e)
                }
            };
            (st.clone(), error)
        };
        self.done = true;
        match error {
            None => self.emit("loop.completed", Status::Ok, &stats, json!({}), true),
            Some(e) => self.emit(
                "loop.failed",
                Status::Failed,
                &stats,
                json!({"error": e}),
                true,
            ),
        }
    }

    /// The last `loop.*` event written: the parent of what the handler does.
    fn last_event(&self) -> Option<String> {
        self.trace.as_ref().and_then(|t| t.last.clone())
    }

    fn emit(&mut self, kind: &str, status: Status, stats: &LoopStats, extra: Value, timed: bool) {
        let Some(t) = &mut self.trace else { return };
        let mut payload = json!({"loop": self.slot.name, "stats": stats.to_json()});
        if let (Value::Object(p), Value::Object(x)) = (&mut payload, extra) {
            p.extend(x);
        }
        let mut d = EventDraft::new(Component::Loop, kind, status)
            .session(t.session_id.clone())
            .node(node_id::loop_(&self.slot.name))
            .payload(payload);
        if let Some(p) = t.last.take() {
            d = d.parent(p);
        }
        if timed {
            d = d.duration(t.since.elapsed().as_millis() as u64);
        }
        t.last = t.sink.emit(d);
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let stats = {
            let mut st = self.slot.stats();
            st.dropped += 1;
            if self.running {
                st.in_flight = st.in_flight.saturating_sub(1);
                st.last_error = Some("aborted at the shutdown deadline".into());
            } else {
                st.queued = st.queued.saturating_sub(1);
            }
            st.clone()
        };
        let reason = if self.running {
            "aborted_at_deadline"
        } else {
            "dropped_at_shutdown"
        };
        let running = self.running;
        self.emit(
            "loop.dropped",
            Status::Dropped,
            &stats,
            json!({"reason": reason}),
            running,
        );
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
        dispatch_queued(
            loops,
            max,
            crate::config::runtime::DEFAULT_MAX_QUEUED_PER_LOOP,
        )
    }

    fn dispatch_queued(
        loops: &[(&str, Arc<SlowLoop>)],
        max: usize,
        max_queued: usize,
    ) -> LoopDispatch {
        LoopDispatch::new(
            loops
                .iter()
                .map(|(n, l)| (n.to_string(), Arc::clone(l) as Arc<dyn LoopHandler>))
                .collect(),
            max,
            max_queued,
        )
    }

    /// W1-gate review: an event burst cannot park unbounded tasks — past
    /// `max_queued` waiting events a loop refuses (`QueueFull`, counted
    /// dropped, `last_error` set, tracked receivers too) and accepts again
    /// once its queue drains. Other loops are unaffected.
    #[tokio::test]
    async fn a_full_loop_queue_refuses_until_it_drains() {
        let (a, b) = (SlowLoop::new(80), SlowLoop::new(1));
        let d = dispatch_queued(&[("a", a.clone()), ("b", b)], 4, 2);
        d.submit("a", Value::Null, "running".into()).unwrap();
        for _ in 0..200 {
            if d.stats()["a"].in_flight == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        d.submit("a", Value::Null, "q1".into()).unwrap();
        d.submit_tracked("a", Value::Null, "q2".into()).unwrap();
        let full = Refused::QueueFull { max: 2 };
        assert_eq!(d.submit("a", Value::Null, "burst".into()), Err(full));
        assert_eq!(
            d.submit_tracked("a", Value::Null, "burst2".into()).err(),
            Some(full)
        );
        let st = &d.stats()["a"];
        assert_eq!(
            (st.accepted, st.queued, st.in_flight, st.dropped),
            (3, 2, 1, 2)
        );
        let error = st.last_error.clone().unwrap();
        assert!(
            error.contains("queue full: 2 events already waiting")
                && error.contains("max_queued_per_loop"),
            "{error}"
        );
        // Another loop still accepts.
        d.submit("b", Value::Null, "other".into()).unwrap();
        settle(&d, "b", 1).await;

        settle(&d, "a", 3).await;
        assert_eq!(*a.seen.lock().unwrap(), ["running", "q1", "q2"]);
        d.submit("a", Value::Null, "after".into()).unwrap();
        settle(&d, "a", 4).await;
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
    async fn tracked_receivers_close_when_the_event_is_done_or_dropped() {
        use tokio::sync::oneshot::error::TryRecvError;
        let a = SlowLoop::new(60);
        let d = dispatch(&[("a", a.clone())], 4);
        let mut running = d
            .submit_tracked("a", Value::Null, "running".into())
            .unwrap();
        let mut queued = d.submit_tracked("a", Value::Null, "queued".into()).unwrap();
        tokio::time::sleep(Duration::from_millis(15)).await;
        assert_eq!(running.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(queued.try_recv(), Err(TryRecvError::Empty));
        let wait = |rx| tokio::time::timeout(Duration::from_secs(5), rx);
        assert!(wait(&mut running).await.expect("closed").is_err());
        assert_eq!(*a.seen.lock().unwrap(), ["running", "queued"]);
        assert!(wait(&mut queued).await.expect("closed").is_err());
        // Dropped at shutdown (never ran): closed too.
        let slow = SlowLoop::new(150);
        let d = dispatch(&[("s", slow)], 4);
        d.submit("s", Value::Null, "first".into()).unwrap();
        let mut dropped = d.submit_tracked("s", Value::Null, "second".into()).unwrap();
        tokio::time::sleep(Duration::from_millis(15)).await;
        d.drain(Instant::now() + Duration::from_secs(5)).await;
        assert_eq!(dropped.try_recv(), Err(TryRecvError::Closed));
        assert_eq!(
            d.submit_tracked("nope", Value::Null, "x".into()).err(),
            Some(Refused::UnknownLoop)
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

    /// Records the trace cause each event's handler ran under.
    struct CauseLoop {
        ms: u64,
        fail: bool,
        causes: Mutex<Vec<Option<Cause>>>,
    }

    #[async_trait]
    impl LoopHandler for CauseLoop {
        async fn handle(&self, _event: &Value, _session_id: &str) -> anyhow::Result<()> {
            self.causes.lock().unwrap().push(trace_exec::cause());
            tokio::time::sleep(Duration::from_millis(self.ms)).await;
            if self.fail {
                anyhow::bail!("jev 503");
            }
            Ok(())
        }
    }

    /// queued → started → completed / failed, each the one before's child;
    /// the first's parent = the sender's cause; the handler runs caused by
    /// `loop.started`; counters ride along; shutdown drops are `loop.dropped`,
    /// refusals `loop.refused` with their reason.
    #[tokio::test]
    async fn trace_events_follow_ticket_lifecycle() {
        use crate::application::trace_exec::tests::MemTrace;
        let sink = Arc::new(MemTrace::default());
        let ok = Arc::new(CauseLoop {
            ms: 1,
            fail: false,
            causes: Mutex::new(vec![]),
        });
        let bad = Arc::new(CauseLoop {
            ms: 1,
            fail: true,
            causes: Mutex::new(vec![]),
        });
        let slow = Arc::new(CauseLoop {
            ms: 200,
            fail: false,
            causes: Mutex::new(vec![]),
        });
        let handlers: BTreeMap<String, Arc<dyn LoopHandler>> = [
            ("demo".to_string(), ok.clone() as Arc<dyn LoopHandler>),
            ("bad".to_string(), bad as Arc<dyn LoopHandler>),
            ("slow".to_string(), slow as Arc<dyn LoopHandler>),
        ]
        .into();
        let d = LoopDispatch::new(handlers, 4, 1).with_trace(sink.clone());
        let tick = Cause::new(Some("run:5".into()), "tick:1759912340000");
        trace_exec::caused_by_sync(tick, || {
            d.submit_tracked(
                "demo",
                json!({"scenario": "normal"}),
                "tick:1759912340000".into(),
            )
        })
        .unwrap();
        settle(&d, "demo", 1).await;
        d.submit("bad", Value::Null, "webhook-a".into()).unwrap();
        settle(&d, "bad", 1).await;
        assert_eq!(
            d.submit("nope", Value::Null, "webhook-b".into()),
            Err(Refused::UnknownLoop)
        );

        let ev = sink.all();
        let of = |s: &str| -> Vec<(usize, &EventDraft)> {
            ev.iter()
                .enumerate()
                .filter(|(_, e)| e.session_id.as_deref() == Some(s))
                .collect()
        };
        let demo = of("tick:1759912340000");
        let kinds: Vec<&str> = demo.iter().map(|(_, e)| e.kind.as_str()).collect();
        assert_eq!(kinds, ["loop.queued", "loop.started", "loop.completed"]);
        assert_eq!(demo[0].1.parent_event_id.as_deref(), Some("run:5"));
        assert_eq!(demo[1].1.parent_event_id, Some(MemTrace::id(demo[0].0)));
        assert_eq!(demo[2].1.parent_event_id, Some(MemTrace::id(demo[1].0)));
        assert_eq!(demo[0].1.status, Status::Pending);
        assert_eq!(demo[2].1.status, Status::Ok);
        assert_eq!(demo[0].1.node_id.as_deref(), Some("loop:demo"));
        assert_eq!(demo[0].1.payload["stats"]["queued"], json!(1));
        assert_eq!(demo[1].1.payload["stats"]["in_flight"], json!(1));
        assert_eq!(demo[2].1.payload["stats"]["completed"], json!(1));
        assert!(demo[2].1.duration_ms.is_some());
        let seen = ok.causes.lock().unwrap()[0].clone().unwrap();
        assert_eq!(seen.parent, Some(MemTrace::id(demo[1].0)));
        assert_eq!(seen.session.as_deref(), Some("tick:1759912340000"));

        let bad = of("webhook-a");
        assert_eq!(bad[0].1.parent_event_id, None, "no cause: no parent");
        assert_eq!(bad[2].1.kind, "loop.failed");
        assert_eq!(bad[2].1.payload["error"], json!("jev 503"));
        let none = of("webhook-b");
        assert_eq!(none[0].1.kind, "loop.refused");
        assert_eq!(none[0].1.payload["reason"], json!("unknown_loop"));

        // Queue full, then shutdown: one running, one queued (dropped), one refused.
        d.submit("slow", Value::Null, "s1".into()).unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        d.submit("slow", Value::Null, "s2".into()).unwrap();
        assert!(matches!(
            d.submit("slow", Value::Null, "s3".into()),
            Err(Refused::QueueFull { max: 1 })
        ));
        d.drain(Instant::now() + Duration::from_millis(50)).await;
        assert_eq!(
            d.submit("slow", Value::Null, "s4".into()),
            Err(Refused::ShuttingDown)
        );
        let ev = sink.all();
        let last = |s: &str| {
            ev.iter()
                .rfind(|e| e.session_id.as_deref() == Some(s))
                .cloned()
                .unwrap()
        };
        let s1 = last("s1");
        assert_eq!(
            (s1.kind.as_str(), s1.status),
            ("loop.dropped", Status::Dropped)
        );
        assert_eq!(s1.payload["reason"], json!("aborted_at_deadline"));
        assert_eq!(last("s2").payload["reason"], json!("dropped_at_shutdown"));
        assert_eq!(last("s3").payload["reason"], json!("queue_full"));
        assert_eq!(last("s3").status, Status::Refused);
        assert_eq!(last("s4").payload["reason"], json!("shutting_down"));
    }
}
