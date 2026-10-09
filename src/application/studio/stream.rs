//! Studio live delivery (`TENGU_STUDIO_PLAN.md` § 5: "live transport is
//! bounded and may lag; durable evidence remains replayable"): one recorded
//! run, or the run of the runtime that holds the lease now, as a bounded
//! per-client stream of [`StreamItem`]s. The trace file is the source
//! (`TraceReader::follow`); a client that falls behind is told so and reads
//! the rest from the file by `seq`. Transport-free: `adapters/inbound/studio`
//! maps the items to SSE.
//!
//! | Rule | Behaviour |
//! |---|---|
//! | backlog | an event already in the file when the client attached (`seq` ≤ `TraceReader::last_seq` then) waits for room in the client's buffer: a long replay never lags |
//! | live | a later event must fit the client's buffer now ([`StreamLimits::client_buffer`], [`CLIENT_BUFFER`] = 256); when it does not, the client gets [`StreamItem::Lagged`] (`last_seq` = the last event it was handed) and the stream ends — it resumes after that `seq`: no gap, no repeat |
//! | live run ([`follow_live`]) | the newest `run` recording whose `runtime_id` is the heartbeat's lease holder ([`live_run`]), checked every [`StreamLimits::holder_poll`] ([`HOLDER_POLL`] = 1 s); a new holder (a restart) = [`StreamItem::Run`] `restarted`, then that run from `seq` 1; before any = one `Run` with `run_id: null`, `waiting` + why (no heartbeat, no recording yet) |
//! | resume ([`follow_live`]) | a `Last-Event-ID` `<run_id>:<seq>` of the live run = after that `seq`; of another run = the live run from `seq` 1 |
//! | end | the receiver is dropped (the client left): the pump and its file tail stop within one poll |

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde::Serialize;
use tokio::sync::mpsc::{self, error::TrySendError, Receiver, Sender};
use tokio::time::MissedTickBehavior;

use crate::domain::trace::{event_id, ExecutionEvent, RunKind, RunSummary};
use crate::ports::trace::TraceReader;

/// Items a client may hold unread (live events past it lag the client).
pub(crate) const CLIENT_BUFFER: usize = 256;
/// How often [`follow_live`] re-reads the heartbeat holder.
pub(crate) const HOLDER_POLL: Duration = Duration::from_secs(1);

/// One item a Studio client receives.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum StreamItem {
    /// One trace event, in `seq` order (boxed: the other items are small).
    Trace(Box<ExecutionEvent>),
    /// [`follow_live`]: the run the next events belong to (or why none).
    Run(RunAttach),
    /// The client fell behind; nothing follows.
    Lagged(Lag),
}

/// Which run [`follow_live`] follows now.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct RunAttach {
    /// `None` while no runtime recording can be followed.
    pub run_id: Option<String>,
    /// The lease holder the run belongs to (`None`: no heartbeat).
    pub runtime_id: Option<String>,
    /// `attached` (first run of this stream) · `restarted` (a new holder)
    /// · `waiting` (nothing to follow yet).
    pub reason: &'static str,
    pub detail: String,
}

/// Where a lagged client resumes.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Lag {
    pub run_id: String,
    /// The last `seq` this client was handed: resume after it.
    pub last_seq: u64,
    /// `<run_id>:<last_seq>` (the `Last-Event-ID` to resume with); `None`
    /// before the first event.
    pub last_event_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StreamLimits {
    pub client_buffer: usize,
    pub holder_poll: Duration,
}

impl Default for StreamLimits {
    fn default() -> Self {
        Self {
            client_buffer: CLIENT_BUFFER,
            holder_poll: HOLDER_POLL,
        }
    }
}

/// The lease holder of the sandbox's runtime now (`run-<sandbox>.json`
/// `holder`); `None` without a readable heartbeat.
pub(crate) type HolderFn = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// `run_id`'s events after `after` (module table: backlog, live, lag).
/// `Err` before anything is sent: a bad id or a run that does not exist.
/// Needs a tokio runtime.
pub(crate) fn follow_run(
    reader: &dyn TraceReader,
    run_id: &str,
    after: u64,
    limits: StreamLimits,
) -> Result<Receiver<StreamItem>> {
    let mut tail = Tail::open(reader, run_id, after)?;
    let (tx, rx) = mpsc::channel(limits.client_buffer.max(1));
    tokio::spawn(async move {
        loop {
            let next = tokio::select! {
                ev = tail.rx.recv() => ev,
                _ = tx.closed() => return,
            };
            let Some(ev) = next else { return };
            if tail.hand(&tx, ev).await != Handed::Yes {
                return;
            }
        }
    });
    Ok(rx)
}

/// The newest `run` recording of the runtime `holder` (`runtime_id` =
/// the lease holder); `None` when it has none.
pub(crate) fn live_run(reader: &dyn TraceReader, holder: &str) -> Result<Option<RunSummary>> {
    Ok(pick_live(&reader.runs()?, holder).cloned())
}

/// [`live_run`] over a listing already read.
pub(crate) fn pick_live<'a>(runs: &'a [RunSummary], holder: &str) -> Option<&'a RunSummary> {
    runs.iter()
        .filter(|r| r.runtime_id.as_deref() == Some(holder))
        .filter(|r| r.kind.as_deref() == Some(RunKind::Run.as_str()))
        .max_by(|a, b| (a.started_ms, &a.run_id).cmp(&(b.started_ms, &b.run_id)))
}

/// The run of the runtime that holds the lease, across restarts (module
/// table: live run, resume). `resume` = a client's `Last-Event-ID`. Needs
/// a tokio runtime.
pub(crate) fn follow_live(
    reader: Arc<dyn TraceReader>,
    holder: HolderFn,
    resume: Option<(String, u64)>,
    limits: StreamLimits,
) -> Receiver<StreamItem> {
    let (tx, rx) = mpsc::channel(limits.client_buffer.max(1));
    tokio::spawn(pump_live(reader, holder, resume, limits, tx));
    rx
}

enum Next {
    Event(Option<Box<ExecutionEvent>>),
    Poll,
}

async fn pump_live(
    reader: Arc<dyn TraceReader>,
    holder: HolderFn,
    mut resume: Option<(String, u64)>,
    limits: StreamLimits,
    tx: Sender<StreamItem>,
) {
    // The holder followed now + its run's tail.
    let mut current: Option<(String, Tail)> = None;
    // The last `waiting` detail sent (never repeated).
    let mut told: Option<String> = None;
    let mut tick = tokio::time::interval(limits.holder_poll);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        let next = tokio::select! {
            ev = next_event(&mut current) => Next::Event(ev.map(Box::new)),
            _ = tick.tick() => Next::Poll,
            _ = tx.closed() => return,
        };
        match next {
            Next::Event(Some(ev)) => {
                let Some((_, tail)) = current.as_mut() else {
                    continue;
                };
                if tail.hand(&tx, *ev).await != Handed::Yes {
                    return;
                }
            }
            // The tail ended (its task died): re-attach at the next poll,
            // after the last event this client holds — never from seq 1.
            Next::Event(None) => {
                if let Some((_, tail)) = current.take() {
                    resume = Some((tail.run_id, tail.last));
                }
            }
            Next::Poll => {
                let now = holder();
                if current.is_some() && current.as_ref().map(|(h, _)| h) == now.as_ref() {
                    continue;
                }
                let target = {
                    let (r, h) = (Arc::clone(&reader), now.clone());
                    tokio::task::spawn_blocking(move || target(&*r, h.as_deref()))
                        .await
                        .unwrap_or_else(|e| Err(waiting(None, format!("holder lookup: {e}"))))
                };
                let attach = target.and_then(|run| {
                    let after = match resume.take() {
                        Some((r, seq)) if r == run.run_id => seq,
                        _ => 0,
                    };
                    let h = now.clone().unwrap_or_default();
                    match Tail::open(&*reader, &run.run_id, after) {
                        Ok(tail) => Ok((
                            RunAttach {
                                run_id: Some(run.run_id),
                                runtime_id: Some(h.clone()),
                                reason: if current.is_some() {
                                    "restarted"
                                } else {
                                    "attached"
                                },
                                detail: format!("runtime {h}"),
                            },
                            tail,
                        )),
                        Err(e) => Err(waiting(Some(h), format!("{e:#}"))),
                    }
                });
                match attach {
                    Ok((a, tail)) => {
                        if tx.send(StreamItem::Run(a)).await.is_err() {
                            return;
                        }
                        current = Some((now.unwrap_or_default(), tail));
                        told = None;
                    }
                    // Still on the old run (a new holder not recorded
                    // yet): keep it; tell a client that follows nothing.
                    Err(w) if current.is_none() && told.as_deref() != Some(&w.detail) => {
                        told = Some(w.detail.clone());
                        if tx.send(StreamItem::Run(w)).await.is_err() {
                            return;
                        }
                    }
                    Err(_) => {}
                }
            }
        }
    }
}

/// The current run's next event; pending while there is none.
async fn next_event(current: &mut Option<(String, Tail)>) -> Option<ExecutionEvent> {
    match current {
        Some((_, tail)) => tail.rx.recv().await,
        None => std::future::pending().await,
    }
}

/// The run to follow for `holder`, or why there is none.
fn target(reader: &dyn TraceReader, holder: Option<&str>) -> Result<RunSummary, RunAttach> {
    let Some(h) = holder else {
        return Err(waiting(
            None,
            "no heartbeat: no `tengu run` of this sandbox under this TENGU_HOME yet".into(),
        ));
    };
    match live_run(reader, h) {
        Ok(Some(run)) => Ok(run),
        Ok(None) => Err(waiting(
            Some(h.to_string()),
            format!("runtime {h} has no trace recording yet"),
        )),
        Err(e) => Err(waiting(Some(h.to_string()), format!("{e:#}"))),
    }
}

fn waiting(runtime_id: Option<String>, detail: String) -> RunAttach {
    RunAttach {
        run_id: None,
        runtime_id,
        reason: "waiting",
        detail,
    }
}

/// One followed run: its file tail and where this client is in it.
struct Tail {
    run_id: String,
    /// The newest `seq` when the client attached: up to it = backlog.
    head: u64,
    /// The last `seq` handed to the client.
    last: u64,
    rx: Receiver<ExecutionEvent>,
}

#[derive(Debug, PartialEq, Eq)]
enum Handed {
    Yes,
    Lagged,
    Gone,
}

impl Tail {
    fn open(reader: &dyn TraceReader, run_id: &str, after: u64) -> Result<Self> {
        let head = reader.last_seq(run_id)?;
        let rx = reader.follow(run_id, after)?;
        Ok(Self {
            run_id: run_id.to_string(),
            head,
            last: after,
            rx,
        })
    }

    /// Hand `ev` to the client: a backlog event waits for room, a live one
    /// must fit now (else the client gets `Lagged`).
    async fn hand(&mut self, tx: &Sender<StreamItem>, ev: ExecutionEvent) -> Handed {
        let seq = ev.seq;
        if seq <= self.head {
            if tx.send(StreamItem::Trace(Box::new(ev))).await.is_err() {
                return Handed::Gone;
            }
        } else {
            match tx.try_send(StreamItem::Trace(Box::new(ev))) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    let lag = Lag {
                        run_id: self.run_id.clone(),
                        last_seq: self.last,
                        last_event_id: (self.last > 0).then(|| event_id(&self.run_id, self.last)),
                    };
                    // Waits for one slot: the client reads what it holds first.
                    return match tx.send(StreamItem::Lagged(lag)).await {
                        Ok(()) => Handed::Lagged,
                        Err(_) => Handed::Gone,
                    };
                }
                Err(TrySendError::Closed(_)) => return Handed::Gone,
            }
        }
        self.last = seq;
        Handed::Yes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::Mutex;

    use crate::adapters::outbound::trace_store::{JsonlTraceReader, JsonlTraceSink};
    use crate::domain::secrets::SecretRegistry;
    use crate::domain::trace::{Component, EventDraft, Status};
    use crate::ports::trace::TraceSink;

    const SANDBOX: &str = "control-loop-lab";

    fn sink(root: &Path, runtime_id: Option<&str>) -> JsonlTraceSink {
        JsonlTraceSink::open(
            root,
            SANDBOX,
            Some(&"c".repeat(64)),
            runtime_id,
            if runtime_id.is_some() {
                RunKind::Run
            } else {
                RunKind::Decide
            },
            Arc::new(SecretRegistry::new()),
        )
        .unwrap()
    }

    fn emit(s: &JsonlTraceSink, n: u64) {
        for i in 0..n {
            s.emit(
                EventDraft::new(Component::Loop, "loop.completed", Status::Ok)
                    .node("loop:demo")
                    .payload(serde_json::json!({"i": i})),
            )
            .unwrap();
        }
    }

    fn reader(root: &Path) -> Arc<JsonlTraceReader> {
        Arc::new(
            JsonlTraceReader::new(root, SANDBOX)
                .unwrap()
                .tuned(Duration::from_millis(5), 256),
        )
    }

    fn limits(client_buffer: usize) -> StreamLimits {
        StreamLimits {
            client_buffer,
            holder_poll: Duration::from_millis(20),
        }
    }

    async fn recv(rx: &mut Receiver<StreamItem>) -> Option<StreamItem> {
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("stream timed out")
    }

    fn seq(item: Option<StreamItem>) -> u64 {
        match item {
            Some(StreamItem::Trace(e)) => e.seq,
            other => panic!("not an event: {other:?}"),
        }
    }

    fn run(item: Option<StreamItem>) -> RunAttach {
        match item {
            Some(StreamItem::Run(a)) => a,
            other => panic!("not a run: {other:?}"),
        }
    }

    /// 300 events already written, a buffer of 4, a slow reader: every
    /// event arrives in order, never `Lagged` (the backlog waits).
    #[tokio::test]
    async fn backlog_waits_for_a_slow_client() {
        let tmp = tempfile::tempdir().unwrap();
        let s = sink(tmp.path(), None);
        emit(&s, 299);
        let r = reader(tmp.path());
        let mut rx = follow_run(&*r, s.run_id().unwrap(), 0, limits(4)).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let got: Vec<u64> = {
            let mut v = Vec::new();
            for _ in 0..300 {
                v.push(seq(recv(&mut rx).await));
            }
            v
        };
        assert_eq!(got, (1..=300).collect::<Vec<u64>>());
    }

    /// Live events past a full buffer: the client keeps what it holds,
    /// then `Lagged` naming the last one, then the end; resuming after it
    /// yields the rest — no gap, no repeat.
    #[tokio::test]
    async fn live_overflow_lags_then_resumes_without_gap() {
        let tmp = tempfile::tempdir().unwrap();
        let s = sink(tmp.path(), None);
        let run_id = s.run_id().unwrap().to_string();
        let r = reader(tmp.path());
        let mut rx = follow_run(&*r, &run_id, 0, limits(4)).unwrap();
        assert_eq!(seq(recv(&mut rx).await), 1, "backlog: run.opened");
        emit(&s, 30);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut got = Vec::new();
        let lag = loop {
            match recv(&mut rx).await {
                Some(StreamItem::Trace(e)) => got.push(e.seq),
                Some(StreamItem::Lagged(l)) => break l,
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(got, [2, 3, 4, 5], "the buffer's 4");
        assert_eq!(lag.last_seq, 5);
        assert_eq!(lag.last_event_id, Some(format!("{run_id}:5")));
        assert_eq!(recv(&mut rx).await, None, "the stream ends after Lagged");
        let mut again = follow_run(&*r, &run_id, lag.last_seq, limits(4)).unwrap();
        let rest: Vec<u64> = {
            let mut v = Vec::new();
            for _ in 6..=31 {
                v.push(seq(recv(&mut again).await));
            }
            v
        };
        assert_eq!(rest, (6..=31).collect::<Vec<u64>>());
    }

    /// The live stream follows the heartbeat holder's `run` recording:
    /// `waiting` first, then `attached` + its events, then on a new holder
    /// `restarted` + the new run from `seq` 1 — runs never mix.
    #[tokio::test]
    async fn live_follows_the_holder_across_a_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let (h1, h2) = (
            "host:1:0f0e0d0c-0b0a-4908-8706-050403020100",
            "host:2:1f0e0d0c-0b0a-4908-8706-050403020100",
        );
        let decide = sink(tmp.path(), None);
        emit(&decide, 2);
        let a = sink(tmp.path(), Some(h1));
        emit(&a, 2);
        let holder: Arc<Mutex<Option<String>>> = Arc::default();
        let hf: HolderFn = {
            let h = Arc::clone(&holder);
            Arc::new(move || h.lock().unwrap().clone())
        };
        let r = reader(tmp.path());
        let mut rx = follow_live(r.clone(), hf, None, limits(64));
        let w = run(recv(&mut rx).await);
        assert_eq!((w.reason, w.run_id, w.runtime_id), ("waiting", None, None));
        *holder.lock().unwrap() = Some(h1.into());
        let att = run(recv(&mut rx).await);
        assert_eq!(att.reason, "attached");
        assert_eq!(att.run_id.as_deref(), a.run_id());
        assert_eq!(att.runtime_id.as_deref(), Some(h1));
        for want in 1..=3 {
            let Some(StreamItem::Trace(e)) = recv(&mut rx).await else {
                panic!()
            };
            assert_eq!((e.seq, e.run_id.as_str()), (want, a.run_id().unwrap()));
        }
        // A holder with no recording yet: the client keeps the old run.
        *holder.lock().unwrap() = Some(h2.into());
        tokio::time::sleep(Duration::from_millis(60)).await;
        emit(&a, 1);
        assert_eq!(seq(recv(&mut rx).await), 4);
        let b = sink(tmp.path(), Some(h2));
        let re = run(recv(&mut rx).await);
        assert_eq!(re.reason, "restarted");
        assert_eq!(re.run_id.as_deref(), b.run_id());
        let Some(StreamItem::Trace(e)) = recv(&mut rx).await else {
            panic!()
        };
        assert_eq!((e.seq, e.run_id.as_str()), (1, b.run_id().unwrap()));
        assert_eq!(e.runtime_id.as_deref(), Some(h2));
    }

    /// A resume id of the live run continues after it; one of another run
    /// starts the live run from `seq` 1.
    #[tokio::test]
    async fn live_resume_by_last_event_id() {
        let tmp = tempfile::tempdir().unwrap();
        let h = "host:3:2f0e0d0c-0b0a-4908-8706-050403020100";
        let a = sink(tmp.path(), Some(h));
        emit(&a, 4);
        let run_id = a.run_id().unwrap().to_string();
        let r = reader(tmp.path());
        let hf: HolderFn = Arc::new(move || Some(h.to_string()));
        let mut rx = follow_live(r.clone(), hf.clone(), Some((run_id.clone(), 3)), limits(64));
        assert_eq!(run(recv(&mut rx).await).reason, "attached");
        assert_eq!(seq(recv(&mut rx).await), 4);
        assert_eq!(seq(recv(&mut rx).await), 5);
        let other = Some(("5b0c7d0e-8a4e-4f0a-9d8e-2f1c3b4a5d6e".to_string(), 3));
        let mut rx = follow_live(r, hf, other, limits(64));
        assert_eq!(run(recv(&mut rx).await).run_id, Some(run_id));
        assert_eq!(seq(recv(&mut rx).await), 1);
    }

    /// A run that does not exist is refused before anything streams.
    #[tokio::test]
    async fn unknown_run_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let r = reader(tmp.path());
        assert!(follow_run(&*r, "5b0c7d0e-8a4e-4f0a-9d8e-2f1c3b4a5d6e", 0, limits(4)).is_err());
        assert!(follow_run(&*r, "../x", 0, limits(4)).is_err());
    }
}
