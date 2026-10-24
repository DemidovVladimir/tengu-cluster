//! `[feeds.<n>]` runner (xmarket `rt-scheduler`): one supervised task per
//! feed under `tengu run` (wiring `bootstrap/runtime.rs`; config
//! `config/feeds.rs`; fire times `domain/schedule.rs`; operator doc
//! `docs/runtime.md` § Feeds).
//!
//! | Rule | How |
//! |---|---|
//! | when | `next_fire(schedule, max(now, park gate), last slot)`; a grid fire starts up to `jitter_pct` % of its interval late, an at-tick exactly; sleeps on the `Clock` in naps of ≤ 60 s, so a wall-clock jump shows within a minute; a last slot more than 1 s ahead of the clock (a backward step) counts as now — no stall |
//! | one run in flight | a tool or job run is awaited before the next fire is computed; a tick is not sent while this feed's previous event is still queued or running (counted `dropped`) |
//! | missed slots | never replayed: the next fire is computed when a run ends; a slot reached later than `late_grace_ms` after its time (system sleep) is skipped (`dropped`) |
//! | tool run | one call per fan-out entry, `concurrency` at a time, started in order; `ToolCall.id` = `feed:<name>:<slot ms>:<i>` (→ `ToolCtx.call_id`) |
//! | failed call | an observation with `status = error` (class of its most retry-worthy error, the longest `retry_after_ms`); text whose line 1 is `HTTP <non-2xx>`; an executor error (`fatal`: unknown tool, scope denial). Messages: no URLs, ≤ 200 chars cut at whole words (ids stay whole) |
//! | backoff | `next_delay(class, runs in a row with a retryable failure, retry_after, FEED)` over the retryable failures: `Retry(ms)` ⇒ the failed calls again after `ms`, same ids — a slot due first runs every call instead; `Park(ms)` (quota, long rate limit) ⇒ also no slot before then; `auth_required` / `fatal` / `decode` / `not_applicable` wait for the next slot — except on an at-tick slot (review #14: its next slot is a day or a week away), whose failed calls of any class are retried as `transient` until [`AT_RETRY_WINDOW_MS`] after the slot (an executor error — a busy or failing store — is `fatal`) |
//! | health (`FeedWriter`) | a failed run reports its error first (`backoff` while retrying, else `down`), then one `item` per ok call (`live`) |
//! | tick | `LoopDispatch::submit_tracked(target, event + ts_ms = slot, "<feed>:<slot ms>")`; an item per event sent |
//! | job | `RuntimeJob::run(slot ms, "feed:<name>:<slot ms>")` (`ports/runtime.rs`; a retry: the same slot and id); `Done` = one item, `Failed { class }` = a failed call of that class — the same backoff, at-tick window and health |
//! | stop | checked before every nap and every call; a call in progress finishes (bounded by `[runtime] shutdown_grace_secs`) |
//! | trace (`FeedEnv::trace`) | node `feed:<name>`, session `<name>:<slot ms>`; a tool run: `feed.fired` (`Running`: `run` = slot / retry, the call indices) → its calls caused by it (`tool.*` from the agent's `TracedExecutor`; correlation `feed:<name>:<slot ms>`) → `feed.completed` (`Ok`) · `feed.retrying` (`Pending`: class, `retry_in_ms`) · `feed.failed` (`Failed`: class, not retried) (each from the branch that writes the health row) · `feed.skipped` (`shutting_down`: a stop came before every call); a job run: `feed.fired` (`kind = "job"`, `run_id`) → what the job writes while it runs (caused by it; correlation `feed:<name>:<slot ms>`) → `feed.completed` · `feed.retrying` · `feed.failed`, as a tool run; a tick: `feed.fired` → `LoopDispatch` `loop.queued` / `loop.refused` caused by it → `feed.tick_sent` (`Ok`) · `feed.dropped` (`queue_full`) · `feed.failed` (`unknown_loop`) · `feed.skipped` (`shutting_down`); a skipped slot: `feed.dropped` (`late` · `previous_tick_running`) |

use std::sync::Arc;

use futures::StreamExt;
use serde_json::{json, Map, Value};
use tokio::sync::oneshot;
use tracing::{debug, warn};

use super::health::FeedWriter;
use super::loops::{LoopDispatch, Refused};
use super::{stopped, StopRx};
use crate::application::decision_loop::reduce::http_status;
use crate::application::trace_exec::{self, Cause};
use crate::domain::backoff::{next_delay, BackoffPolicy, Delay};
use crate::domain::message::ToolCall;
use crate::domain::observation::{ErrorClass, ObsStatus};
use crate::domain::runtime::scrub_urls;
use crate::domain::schedule::{jittered_ms, late_grace_ms, next_fire, Schedule};
use crate::domain::trace::{Component, EventDraft, Status};
use crate::domain::workflow::node_id;
use crate::ports::clock::Clock;
use crate::ports::engine::ToolExecutor;
use crate::ports::runtime::{JobOutcome, RuntimeJob};
use crate::ports::tool::ToolOutput;
use crate::ports::trace::TraceSink;

/// Longest single sleep: the wall clock is re-read at least this often.
const MAX_NAP_MS: i64 = 60_000;
/// Chars of a failure message kept for logs and health rows (whole words).
const MAX_MESSAGE_CHARS: usize = 200;
/// How long after an at-tick slot its failed calls are retried whatever
/// their class (module table: backoff).
pub(crate) const AT_RETRY_WINDOW_MS: i64 = 15 * 60_000;

/// Uniform in [0, 1] (jitter, backoff): `rate_limit::jitter01` under
/// `tengu run`, a constant in tests.
pub(crate) type Rand01 = Arc<dyn Fn() -> f64 + Send + Sync>;

/// What a feed does when it fires.
pub(crate) enum FeedJob {
    /// Call `tool` once per entry of `calls` (fan-out order).
    Tool {
        executor: Arc<dyn ToolExecutor>,
        tool: String,
        calls: Vec<Value>,
        concurrency: usize,
    },
    /// Send `event` (+ `ts_ms`) to decision loop `target`.
    Tick {
        loops: Arc<LoopDispatch>,
        target: String,
        event: Map<String, Value>,
    },
    /// Run a named application job (`config/feeds.rs` `JOBS`).
    Job { job: Arc<dyn RuntimeJob> },
}

/// One `[feeds.<n>]`, built.
pub(crate) struct FeedSpec {
    pub name: String,
    pub schedule: Schedule,
    pub jitter_pct: u32,
    pub run_on_start: bool,
    pub job: FeedJob,
}

/// A feed's clock, randomness, health writer and trace sink.
pub(crate) struct FeedEnv {
    pub clock: Arc<dyn Clock>,
    pub rand01: Rand01,
    pub health: FeedWriter,
    /// Where `feed.*` events go (module table); `None` = nowhere.
    pub trace: Option<Arc<dyn TraceSink>>,
}

/// Which calls a run makes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Run {
    /// A scheduled slot: every call.
    Slot,
    /// The failed calls of the slot `Next::slot_ms`, by fan-out index.
    Retry(Vec<usize>),
}

/// The next thing a feed does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Next {
    /// When: the slot plus jitter, or the retry time.
    pub at_ms: i64,
    /// The slot's own time: call ids, `ts_ms`, session ids.
    pub slot_ms: i64,
    /// A slot acted on later than `at_ms + late_ms` is skipped.
    pub late_ms: i64,
    pub run: Run,
    /// An at-tick slot (or the start run of a feed with at-ticks only):
    /// failures of any class are retried for [`AT_RETRY_WINDOW_MS`].
    pub at_tick: bool,
}

impl Next {
    /// `run_on_start`: a slot at `now_ms`; `at_tick` for a feed whose
    /// schedule has no grid (its next slot may be a day away).
    fn start(now_ms: i64, at_tick: bool) -> Self {
        Self {
            at_ms: now_ms,
            slot_ms: now_ms,
            late_ms: i64::MAX,
            run: Run::Slot,
            at_tick,
        }
    }
}

/// Why a call failed.
#[derive(Debug, Clone, PartialEq)]
struct Failure {
    class: ErrorClass,
    retry_after_ms: Option<u64>,
    message: String,
}

#[derive(Debug, Clone, PartialEq)]
enum Outcome {
    Ok,
    Failed(Failure),
    /// Not started: the stop signal fired first.
    Skipped,
}

struct PendingRetry {
    slot_ms: i64,
    calls: Vec<usize>,
    at_ms: i64,
    at_tick: bool,
}

/// One feed's state between runs (see the module table).
pub(crate) struct FeedRunner {
    spec: FeedSpec,
    env: FeedEnv,
    last_slot_ms: Option<i64>,
    /// Runs in a row with a retryable failure (`next_delay`'s attempt).
    failures: u32,
    /// No slot before this (`Delay::Park`).
    gate_ms: i64,
    retry: Option<PendingRetry>,
    /// The previous tick's event; closed once it is done.
    tick_done: Option<oneshot::Receiver<()>>,
}

impl FeedRunner {
    pub(crate) fn new(spec: FeedSpec, env: FeedEnv) -> Self {
        Self {
            spec,
            env,
            last_slot_ms: None,
            failures: 0,
            gate_ms: i64::MIN,
            retry: None,
            tick_done: None,
        }
    }

    /// From `now_ms`: the pending retry when it comes before the next slot,
    /// else that slot (jittered). `None` = the schedule never fires.
    pub(crate) fn next(&self, now_ms: i64) -> Option<Next> {
        let slot = next_fire(
            &self.spec.schedule,
            now_ms.max(self.gate_ms),
            self.last_slot_ms,
        );
        if let Some(r) = &self.retry {
            if slot.is_none_or(|s| r.at_ms < s.at_ms) {
                return Some(Next {
                    at_ms: r.at_ms,
                    slot_ms: r.slot_ms,
                    late_ms: i64::MAX,
                    run: Run::Retry(r.calls.clone()),
                    at_tick: r.at_tick,
                });
            }
        }
        let fire = slot?;
        Some(Next {
            at_ms: jittered_ms(fire, self.spec.jitter_pct, (self.env.rand01)()),
            slot_ms: fire.at_ms,
            late_ms: late_grace_ms(fire),
            run: Run::Slot,
            at_tick: fire.every_ms.is_none(),
        })
    }

    /// Act on `next` at `now_ms`.
    pub(crate) async fn run(&mut self, next: Next, now_ms: i64, stop: &StopRx) {
        self.retry = None;
        if next.run == Run::Slot {
            self.last_slot_ms = Some(next.slot_ms);
            let late_ms = now_ms.saturating_sub(next.at_ms);
            if late_ms > next.late_ms {
                warn!(
                    feed = %self.spec.name,
                    slot_ms = next.slot_ms,
                    late_ms,
                    "feed slot skipped: reached too late (system sleep or a stalled clock)"
                );
                self.env.health.dropped(1, now_ms).await;
                self.emit(
                    "feed.dropped",
                    Status::Dropped,
                    next.slot_ms,
                    None,
                    json!({"reason": "late", "late_ms": late_ms}),
                );
                return;
            }
        }
        match self.spec.job {
            FeedJob::Tick { .. } => self.run_tick(next.slot_ms, now_ms).await,
            FeedJob::Tool { .. } => self.run_tool(&next, stop).await,
            FeedJob::Job { .. } => self.run_job(&next, stop).await,
        }
    }

    /// One job run for the slot (module table: job); the outcome settles
    /// like one call's, its trace like a tool run's.
    async fn run_job(&mut self, next: &Next, stop: &StopRx) {
        let FeedJob::Job { job } = &self.spec.job else {
            return;
        };
        let stopping = stop.borrow().is_some();
        if stopping {
            return;
        }
        let (name, slot_ms) = (&self.spec.name, next.slot_ms);
        let run_id = format!("feed:{name}:{slot_ms}");
        let fired = self.emit(
            "feed.fired",
            Status::Running,
            slot_ms,
            None,
            json!({
                "kind": "job",
                "run": if next.run == Run::Slot { "slot" } else { "retry" },
                "run_id": run_id,
            }),
        );
        let cause = Cause::new(fired.clone(), format!("{name}:{slot_ms}")).correlation(&run_id);
        let outcome = match trace_exec::caused_by(cause, job.run(slot_ms, &run_id)).await {
            JobOutcome::Done { note } => {
                debug!(feed = %self.spec.name, slot_ms, %note, "feed job done");
                Outcome::Ok
            }
            JobOutcome::Failed { class, message } => failure(class, None, &message),
        };
        let now = self.env.clock.now_ms();
        self.settle(slot_ms, next.at_tick, &[(0, outcome)], now, fired)
            .await;
    }

    async fn run_tool(&mut self, next: &Next, stop: &StopRx) {
        let FeedJob::Tool {
            executor,
            tool,
            calls,
            concurrency,
        } = &self.spec.job
        else {
            return;
        };
        let indices: Vec<usize> = match &next.run {
            Run::Slot => (0..calls.len()).collect(),
            Run::Retry(ix) => ix.clone(),
        };
        let (name, slot_ms) = (&self.spec.name, next.slot_ms);
        let fired = self.emit(
            "feed.fired",
            Status::Running,
            slot_ms,
            None,
            json!({
                "kind": "tool",
                "tool": tool,
                "run": if next.run == Run::Slot { "slot" } else { "retry" },
                "calls": indices,
            }),
        );
        let cause = Cause::new(fired.clone(), format!("{name}:{slot_ms}"))
            .correlation(format!("feed:{name}:{slot_ms}"));
        let results: Vec<(usize, Outcome)> = futures::stream::iter(indices)
            .map(|i| {
                let cause = cause.clone();
                async move {
                    let stopping = stop.borrow().is_some();
                    if stopping {
                        return (i, Outcome::Skipped);
                    }
                    let call = ToolCall {
                        id: call_id(name, slot_ms, i),
                        name: tool.clone(),
                        arguments: calls.get(i).cloned().unwrap_or_else(|| json!({})),
                    };
                    let result =
                        trace_exec::caused_by(cause, executor.execute_typed(&call, &[])).await;
                    (i, outcome(result))
                }
            })
            .buffered((*concurrency).max(1))
            .collect()
            .await;
        let now = self.env.clock.now_ms();
        self.settle(slot_ms, next.at_tick, &results, now, fired)
            .await;
    }

    /// `feed.<what>` of slot `slot_ms` (module table); its event id.
    fn emit(
        &self,
        kind: &str,
        status: Status,
        slot_ms: i64,
        parent: Option<String>,
        payload: Value,
    ) -> Option<String> {
        let sink = self.env.trace.as_ref()?;
        let name = &self.spec.name;
        let mut d = EventDraft::new(Component::Feed, kind, status)
            .session(format!("{name}:{slot_ms}"))
            .node(node_id::feed(name))
            .payload(payload);
        if let Value::Object(p) = &mut d.payload {
            p.insert("slot_ms".into(), json!(slot_ms));
        }
        // A tick's session is its loop event's: one correlation for both.
        if matches!(self.spec.job, FeedJob::Tool { .. } | FeedJob::Job { .. }) {
            d = d.correlation(format!("feed:{name}:{slot_ms}"));
        }
        if let Some(p) = parent {
            d = d.parent(p);
        }
        sink.emit(d)
    }

    /// Backoff + health after a tool or job run (module table); its trace event,
    /// child of the run's `feed.fired`.
    async fn settle(
        &mut self,
        slot_ms: i64,
        at_tick: bool,
        results: &[(usize, Outcome)],
        now_ms: i64,
        fired: Option<String>,
    ) {
        let ok = results
            .iter()
            .filter(|(_, o)| matches!(o, Outcome::Ok))
            .count();
        let failed: Vec<(usize, &Failure)> = results
            .iter()
            .filter_map(|(i, o)| match o {
                Outcome::Failed(f) => Some((*i, f)),
                _ => None,
            })
            .collect();
        // An at-tick slot inside its window retries every failed call.
        let window = at_tick && now_ms < slot_ms.saturating_add(AT_RETRY_WINDOW_MS);
        let retryable: Vec<(usize, &Failure)> = failed
            .iter()
            .copied()
            .filter(|(_, f)| window || retry_rank(f.class).is_some())
            .collect();
        debug!(feed = %self.spec.name, slot_ms, ok, failed = failed.len(), "feed run");
        if let Some(&(_, lead)) = retryable.iter().max_by_key(|(_, f)| retry_rank(f.class)) {
            self.failures = self.failures.saturating_add(1);
            let retry_after = retryable.iter().filter_map(|(_, f)| f.retry_after_ms).max();
            // A class the backoff stops on (the at-tick window) retries as
            // transient: 1 s, 2 s, 4 s … 60 s.
            let class = match retry_rank(lead.class) {
                Some(_) => lead.class,
                None => ErrorClass::Transient,
            };
            let delay = next_delay(
                class,
                self.failures,
                retry_after,
                &BackoffPolicy::FEED,
                (self.env.rand01)(),
            );
            let retrying = match delay.ms() {
                Some(ms) => {
                    let at_ms = now_ms.saturating_add(ms.min(i64::MAX as u64) as i64);
                    self.gate_ms = match delay {
                        Delay::Park(_) => at_ms,
                        _ => i64::MIN,
                    };
                    self.retry = Some(PendingRetry {
                        slot_ms,
                        calls: retryable.iter().map(|(i, _)| *i).collect(),
                        at_ms,
                        at_tick,
                    });
                    true
                }
                None => false,
            };
            warn!(
                feed = %self.spec.name,
                slot_ms,
                failed = failed.len(),
                calls = results.len(),
                class = lead.class.as_str(),
                at_tick_window = window,
                retry_in_ms = ?delay.ms(),
                error = %lead.message,
                "feed calls failed; backing off"
            );
            self.env
                .health
                .error(lead.class, &lead.message, retrying, now_ms)
                .await;
            let (kind, status) = if retrying {
                ("feed.retrying", Status::Pending)
            } else {
                ("feed.failed", Status::Failed)
            };
            let payload = json!({
                "class": lead.class.as_str(),
                "error": lead.message,
                "retrying": retrying,
                "retry_in_ms": delay.ms(),
                "retry_calls": retryable.iter().map(|(i, _)| *i).collect::<Vec<_>>(),
                "failed": failed.len(),
                "ok": ok,
            });
            self.emit(kind, status, slot_ms, fired, payload);
        } else {
            self.failures = 0;
            self.gate_ms = i64::MIN;
            if let Some(&(_, lead)) = failed.first() {
                warn!(
                    feed = %self.spec.name,
                    slot_ms,
                    failed = failed.len(),
                    calls = results.len(),
                    class = lead.class.as_str(),
                    error = %lead.message,
                    "feed calls failed (not retried; next slot tries again)"
                );
                self.env
                    .health
                    .error(lead.class, &lead.message, false, now_ms)
                    .await;
                let payload = json!({
                    "class": lead.class.as_str(),
                    "error": lead.message,
                    "retrying": false,
                    "failed": failed.len(),
                    "ok": ok,
                });
                self.emit("feed.failed", Status::Failed, slot_ms, fired, payload);
            } else if ok > 0 {
                let payload = json!({"ok": ok});
                self.emit("feed.completed", Status::Ok, slot_ms, fired, payload);
            } else {
                // No call ran (stop requested before each): close the
                // `feed.fired` so it does not read as running forever.
                let payload = json!({"reason": "shutting_down", "skipped": results.len()});
                self.emit("feed.skipped", Status::Skipped, slot_ms, fired, payload);
            }
        }
        for _ in 0..ok {
            self.env.health.item(now_ms).await;
        }
    }

    async fn run_tick(&mut self, slot_ms: i64, now_ms: i64) {
        let FeedJob::Tick {
            loops,
            target,
            event,
        } = &self.spec.job
        else {
            return;
        };
        if let Some(done) = self.tick_done.as_mut() {
            if done.try_recv() == Err(oneshot::error::TryRecvError::Empty) {
                debug!(feed = %self.spec.name, decision_loop = %target, slot_ms, "tick skipped: the previous one is still queued or running");
                self.env.health.dropped(1, now_ms).await;
                let payload = json!({"reason": "previous_tick_running", "target": target});
                self.emit("feed.dropped", Status::Dropped, slot_ms, None, payload);
                return;
            }
        }
        let mut event = event.clone();
        event.insert("ts_ms".into(), json!(slot_ms));
        let session_id = format!("{}:{slot_ms}", self.spec.name);
        let fired = self.emit(
            "feed.fired",
            Status::Running,
            slot_ms,
            None,
            json!({"kind": "tick", "target": target, "event": event}),
        );
        // `loop.queued` / `loop.refused` are this tick's children.
        let sent = trace_exec::caused_by_sync(Cause::new(fired.clone(), &session_id), || {
            loops.submit_tracked(target, Value::Object(event), session_id.clone())
        });
        let (kind, status, reason) = match sent {
            Ok(done) => {
                self.tick_done = Some(done);
                self.env.health.item(now_ms).await;
                ("feed.tick_sent", Status::Ok, None)
            }
            Err(e @ Refused::ShuttingDown) => {
                debug!(feed = %self.spec.name, slot_ms, "tick not sent: shutting down");
                ("feed.skipped", Status::Skipped, Some(e))
            }
            // Other senders (webhooks) filled the loop's queue; the dispatch
            // warned. This tick is dropped, the next one tries again.
            Err(e @ Refused::QueueFull { .. }) => {
                debug!(feed = %self.spec.name, decision_loop = %target, slot_ms, "tick not sent: {e}");
                self.env.health.dropped(1, now_ms).await;
                ("feed.dropped", Status::Dropped, Some(e))
            }
            Err(e @ Refused::UnknownLoop) => {
                let message = format!("decision loop `{target}`: {e}");
                warn!(feed = %self.spec.name, %message, "tick not sent");
                self.env
                    .health
                    .error(ErrorClass::Fatal, &message, false, now_ms)
                    .await;
                ("feed.failed", Status::Failed, Some(e))
            }
        };
        let payload = json!({
            "target": target,
            "reason": reason.map(|e| e.as_str()),
            "error": reason.map(|e| e.to_string()),
        });
        self.emit(kind, status, slot_ms, fired, payload);
    }
}

/// `feed:<name>:<slot ms>:<i>` — the same for a retry of that call.
fn call_id(feed: &str, slot_ms: i64, i: usize) -> String {
    format!("feed:{feed}:{slot_ms}:{i}")
}

/// Retryable classes, most conservative first when several failed.
fn retry_rank(class: ErrorClass) -> Option<u8> {
    match class {
        ErrorClass::QuotaExhausted => Some(3),
        ErrorClass::RateLimited => Some(2),
        ErrorClass::Timeout => Some(1),
        ErrorClass::Transient => Some(0),
        ErrorClass::AuthRequired
        | ErrorClass::Fatal
        | ErrorClass::Decode
        | ErrorClass::NotApplicable => None,
    }
}

fn failure(class: ErrorClass, retry_after_ms: Option<u64>, message: &str) -> Outcome {
    Outcome::Failed(Failure {
        class,
        retry_after_ms,
        message: clip(message),
    })
}

/// `message` on one line, without URLs, cut after whole words once it
/// passes [`MAX_MESSAGE_CHARS`] (` …`): an id is never cut, a single longer
/// word is kept whole.
fn clip(message: &str) -> String {
    let text = scrub_urls(message);
    let (mut out, mut chars) = (String::new(), 0);
    for word in text.split_whitespace() {
        let n = word.chars().count();
        if chars > 0 && chars + 1 + n > MAX_MESSAGE_CHARS {
            out.push_str(" …");
            break;
        }
        if chars > 0 {
            out.push(' ');
            chars += 1;
        }
        out.push_str(word);
        chars += n;
    }
    out
}

/// A call's result → ok / failed (module table). An error row fails with
/// its most retry-worthy error ([`retry_rank`], else the first), so a
/// rate limit behind a transient error still honours its `Retry-After`.
fn outcome(result: anyhow::Result<ToolOutput>) -> Outcome {
    let out = match result {
        Ok(out) => out,
        Err(e) => return failure(ErrorClass::Fatal, None, &format!("{e:#}")),
    };
    match &out.observation {
        Some(o) if o.status == ObsStatus::Error => {
            let lead = (o.errors.iter().enumerate())
                .max_by_key(|(i, e)| (retry_rank(e.class), std::cmp::Reverse(*i)))
                .map(|(_, e)| e);
            failure(
                lead.map_or(ErrorClass::Transient, |e| e.class),
                o.errors.iter().filter_map(|e| e.retry_after_ms).max(),
                lead.map_or(o.headline.as_str(), |e| e.message.as_str()),
            )
        }
        Some(_) => Outcome::Ok,
        None => text_outcome(&out.text),
    }
}

/// Legacy text: failed only when line 1 is `HTTP <non-2xx>` (`http_request`).
fn text_outcome(text: &str) -> Outcome {
    let line1 = text.lines().next().unwrap_or("");
    let Some(status) = http_status(line1) else {
        return Outcome::Ok;
    };
    let class = match status {
        200..=299 => return Outcome::Ok,
        429 => ErrorClass::RateLimited,
        401 | 403 => ErrorClass::AuthRequired,
        408 => ErrorClass::Timeout,
        500..=599 => ErrorClass::Transient,
        _ => ErrorClass::Fatal,
    };
    failure(class, None, line1)
}

/// Sleep until `at_ms` in naps of ≤ [`MAX_NAP_MS`]; `false` = stopped.
async fn nap_until(clock: &dyn Clock, at_ms: i64, stop: &mut StopRx) -> bool {
    loop {
        let stopping = stop.borrow().is_some();
        if stopping {
            return false;
        }
        let now = clock.now_ms();
        if now >= at_ms {
            return true;
        }
        let until = at_ms.min(now.saturating_add(MAX_NAP_MS));
        tokio::select! {
            biased;
            _ = stopped(stop) => return false,
            _ = clock.sleep_until_ms(until) => {}
        }
    }
}

/// Run one feed until the stop signal. Never returns before it (a
/// supervised task that ends early fails the whole runtime).
pub(crate) async fn run_feed(spec: FeedSpec, env: FeedEnv, mut stop: StopRx) {
    let mut feed = FeedRunner::new(spec, env);
    if feed.spec.run_on_start && stop.borrow().is_none() {
        let now = feed.env.clock.now_ms();
        let at_only = feed.spec.schedule.longest_interval_ms().is_none();
        feed.run(Next::start(now, at_only), now, &stop).await;
    }
    loop {
        let stopping = stop.borrow().is_some();
        if stopping {
            return;
        }
        let now = feed.env.clock.now_ms();
        let Some(next) = feed.next(now) else {
            warn!(feed = %feed.spec.name, "feed schedule never fires; idle until shutdown");
            stopped(&mut stop).await;
            return;
        };
        let clock = Arc::clone(&feed.env.clock);
        if !nap_until(&*clock, next.at_ms, &mut stop).await {
            return;
        }
        let now = clock.now_ms();
        feed.run(next, now, &stop).await;
    }
}

#[cfg(test)]
#[path = "feeds_tests.rs"]
mod tests;
