//! `[feeds.<n>]` runner (xmarket `rt-scheduler`): one supervised task per
//! feed under `tengu run` (wiring `bootstrap/runtime.rs`; config
//! `config/feeds.rs`; fire times `domain/schedule.rs`; operator doc
//! `docs/runtime-2026-09-30.md` § Feeds).
//!
//! | Rule | How |
//! |---|---|
//! | when | `next_fire(schedule, max(now, park gate), last slot)`; a grid fire starts up to `jitter_pct` % of its interval late, an at-tick exactly; sleeps on the `Clock` in naps of ≤ 60 s, so a wall-clock jump shows within a minute; a last slot more than 1 s ahead of the clock (a backward step) counts as now — no stall |
//! | one run in flight | a tool run is awaited before the next fire is computed; a tick is not sent while this feed's previous event is still queued or running (counted `dropped`) |
//! | missed slots | never replayed: the next fire is computed when a run ends; a slot reached later than `late_grace_ms` after its time (system sleep) is skipped (`dropped`) |
//! | tool run | one call per fan-out entry, `concurrency` at a time, started in order; `ToolCall.id` = `feed:<name>:<slot ms>:<i>` (→ `ToolCtx.call_id`) |
//! | failed call | an observation with `status = error` (class of its most retry-worthy error, the longest `retry_after_ms`); text whose line 1 is `HTTP <non-2xx>`; an executor error (`fatal`: unknown tool, scope denial). Messages: no URLs, ≤ 200 chars cut at whole words (ids stay whole) |
//! | backoff | `next_delay(class, runs in a row with a retryable failure, retry_after, FEED)` over the retryable failures: `Retry(ms)` ⇒ the failed calls again after `ms`, same ids — a slot due first runs every call instead; `Park(ms)` (quota, long rate limit) ⇒ also no slot before then; `auth_required` / `fatal` / `decode` / `not_applicable` wait for the next slot — except on an at-tick slot (review #14: its next slot is a day or a week away), whose failed calls of any class are retried as `transient` until [`AT_RETRY_WINDOW_MS`] after the slot (an executor error — a busy or failing store — is `fatal`) |
//! | health (`FeedWriter`) | a failed run reports its error first (`backoff` while retrying, else `down`), then one `item` per ok call (`live`) |
//! | tick | `LoopDispatch::submit_tracked(target, event + ts_ms = slot, "<feed>:<slot ms>")`; an item per event sent |
//! | stop | checked before every nap and every call; a call in progress finishes (bounded by `[runtime] shutdown_grace_secs`) |
//! | trace (`FeedEnv::trace`) | node `feed:<name>`, session `<name>:<slot ms>`; a tool run: `feed.fired` (`Running`: `run` = slot / retry, the call indices) → its calls caused by it (`tool.*` from the agent's `TracedExecutor`; correlation `feed:<name>:<slot ms>`) → `feed.completed` (`Ok`) · `feed.retrying` (`Pending`: class, `retry_in_ms`) · `feed.failed` (`Failed`: class, not retried) (each from the branch that writes the health row) · `feed.skipped` (`shutting_down`: a stop came before every call); a tick: `feed.fired` → `LoopDispatch` `loop.queued` / `loop.refused` caused by it → `feed.tick_sent` (`Ok`) · `feed.dropped` (`queue_full`) · `feed.failed` (`unknown_loop`) · `feed.skipped` (`shutting_down`); a skipped slot: `feed.dropped` (`late` · `previous_tick_running`) |

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
        if matches!(self.spec.job, FeedJob::Tick { .. }) {
            self.run_tick(next.slot_ms, now_ms).await;
        } else {
            self.run_tool(&next, stop).await;
        }
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
        if matches!(self.spec.job, FeedJob::Tool { .. }) {
            d = d.correlation(format!("feed:{name}:{slot_ms}"));
        }
        if let Some(p) = parent {
            d = d.parent(p);
        }
        sink.emit(d)
    }

    /// Backoff + health after a tool run (module table); its trace event,
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
mod tests {
    use super::*;
    use crate::application::runtime::health::HealthBoard;
    use crate::application::runtime::loops::LoopHandler;
    use crate::application::runtime::{Stopper, Supervisor};
    use crate::domain::observation::{ObsSource, Observation, ReadError};
    use crate::domain::runtime::{FeedHealth, FeedState, RunState};
    use crate::domain::schedule::parse_at;
    use crate::domain::tz::Zone;
    use crate::ports::clock::ManualClock;
    use async_trait::async_trait;
    use chrono::NaiveDateTime;
    use std::collections::{BTreeMap, HashMap};
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    use std::sync::Mutex;
    use std::time::Duration;

    const MIN: i64 = 60_000;

    fn utc(s: &str) -> i64 {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M")
            .unwrap()
            .and_utc()
            .timestamp_millis()
    }

    /// Sunday of the weekend run, on the minute grid.
    fn t0() -> i64 {
        utc("2026-10-04 12:00")
    }

    fn every(ms: i64) -> Schedule {
        Schedule {
            zone: Zone::Utc,
            every_ms: Some(ms as u64),
            windows: vec![],
            at: vec![],
        }
    }

    #[derive(Clone)]
    enum Reply {
        ErrorRow(ErrorClass, Option<u64>),
        /// An error row with several errors, in this order.
        Errors(Vec<(ErrorClass, Option<u64>)>),
        Text(&'static str),
        Fail(&'static str),
    }

    /// Records every call; replies ok unless scripted by call index.
    struct FakeExec {
        clock: Arc<ManualClock>,
        /// Virtual ms each call takes.
        run_ms: i64,
        /// Yields inside a call, so concurrent calls overlap.
        yields: usize,
        script: Mutex<HashMap<usize, Reply>>,
        calls: Mutex<Vec<ToolCall>>,
        running: AtomicUsize,
        max_running: AtomicUsize,
        stop_after: Option<(usize, Stopper)>,
    }

    impl FakeExec {
        fn new(clock: &Arc<ManualClock>) -> Self {
            Self {
                clock: Arc::clone(clock),
                run_ms: 0,
                yields: 0,
                script: Mutex::new(HashMap::new()),
                calls: Mutex::new(vec![]),
                running: AtomicUsize::new(0),
                max_running: AtomicUsize::new(0),
                stop_after: None,
            }
        }

        fn script(&self, i: usize, reply: Reply) {
            self.script.lock().unwrap().insert(i, reply);
        }

        fn ids(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(|c| c.id.clone())
                .collect()
        }
    }

    #[async_trait]
    impl ToolExecutor for FakeExec {
        async fn execute(
            &self,
            call: &ToolCall,
            m: &[crate::domain::message::Message],
        ) -> anyhow::Result<String> {
            Ok(self.execute_typed(call, m).await?.text)
        }

        async fn execute_typed(
            &self,
            call: &ToolCall,
            _m: &[crate::domain::message::Message],
        ) -> anyhow::Result<ToolOutput> {
            let n = {
                let mut calls = self.calls.lock().unwrap();
                calls.push(call.clone());
                calls.len()
            };
            let now = self.running.fetch_add(1, SeqCst) + 1;
            self.max_running.fetch_max(now, SeqCst);
            for _ in 0..self.yields {
                tokio::task::yield_now().await;
            }
            self.clock.advance(self.run_ms);
            self.running.fetch_sub(1, SeqCst);
            if let Some((after, stopper)) = &self.stop_after {
                if n >= *after {
                    stopper.stop("test done", false);
                }
            }
            let i: usize = call.id.rsplit(':').next().unwrap().parse().unwrap();
            let reply = self.script.lock().unwrap().get(&i).cloned();
            let coin = call.arguments["coin"].as_str().unwrap_or_default();
            let obs = |status, errors| Observation {
                key: format!("hl_book/1:hyperliquid:{coin}"),
                schema: "hl_book/1".into(),
                tool: call.name.clone(),
                observed_at_ms: self.clock.now_ms(),
                slot: None,
                ttl_ms: 2_000,
                source: ObsSource::Live,
                status,
                errors,
                headline: "book".into(),
                features: Default::default(),
                data: Value::Null,
            };
            Ok(match reply {
                None => ToolOutput {
                    text: "book".into(),
                    observation: Some(obs(ObsStatus::Ok, vec![])),
                },
                Some(Reply::ErrorRow(class, retry_after_ms)) => ToolOutput {
                    text: "book error".into(),
                    observation: Some(obs(
                        ObsStatus::Error,
                        vec![ReadError {
                            retry_after_ms,
                            ..ReadError::new(
                                "book",
                                class,
                                "429 from https://api.hyperliquid.xyz/info",
                            )
                        }],
                    )),
                },
                Some(Reply::Errors(errors)) => ToolOutput {
                    text: "book error".into(),
                    observation: Some(obs(
                        ObsStatus::Error,
                        errors
                            .into_iter()
                            .map(|(class, retry_after_ms)| ReadError {
                                retry_after_ms,
                                ..ReadError::new("book", class, class.as_str())
                            })
                            .collect(),
                    )),
                },
                Some(Reply::Text(t)) => ToolOutput::from(t.to_string()),
                Some(Reply::Fail(e)) => anyhow::bail!("{e}"),
            })
        }
    }

    struct Rig {
        clock: Arc<ManualClock>,
        board: Arc<HealthBoard>,
        stopper: Stopper,
    }

    impl Rig {
        fn at(now_ms: i64) -> Self {
            Self {
                clock: Arc::new(ManualClock::at(now_ms)),
                board: Arc::new(HealthBoard::new(
                    "xmarket-weekend",
                    "test:1:u",
                    now_ms,
                    5,
                    BTreeMap::new(),
                )),
                stopper: Supervisor::new().stopper(),
            }
        }

        fn env(&self, name: &str, rand01: f64) -> FeedEnv {
            FeedEnv {
                clock: Arc::clone(&self.clock) as Arc<dyn Clock>,
                rand01: Arc::new(move || rand01),
                health: self.board.feed(name, true, 600, None, self.clock.now_ms()),
                trace: None,
            }
        }

        async fn health(&self, name: &str) -> FeedHealth {
            let hb = self
                .board
                .beat(
                    &BTreeMap::new(),
                    RunState::Running,
                    None,
                    self.clock.now_ms(),
                )
                .await;
            hb.feeds[name].clone()
        }
    }

    fn tool_spec(name: &str, schedule: Schedule, exec: &Arc<FakeExec>, coins: &[&str]) -> FeedSpec {
        FeedSpec {
            name: name.into(),
            schedule,
            jitter_pct: 0,
            run_on_start: false,
            job: FeedJob::Tool {
                executor: Arc::clone(exec) as Arc<dyn ToolExecutor>,
                tool: "hl_book".into(),
                calls: coins.iter().map(|c| json!({ "coin": c })).collect(),
                concurrency: 1,
            },
        }
    }

    fn slot(at_ms: i64) -> Next {
        Next {
            at_ms,
            slot_ms: at_ms,
            late_ms: MIN,
            run: Run::Slot,
            at_tick: false,
        }
    }

    #[tokio::test]
    async fn slow_runs_skip_missed_slots_and_never_overlap() {
        let rig = Rig::at(t0());
        let mut exec = FakeExec::new(&rig.clock);
        exec.run_ms = 150_000; // each run outlasts two slots
        exec.stop_after = Some((3, rig.stopper.clone()));
        let exec = Arc::new(exec);
        let spec = tool_spec("hl_ctx", every(MIN), &exec, &["xyz:TSLA"]);
        tokio::time::timeout(
            Duration::from_secs(10),
            run_feed(spec, rig.env("hl_ctx", 0.5), rig.stopper.subscribe()),
        )
        .await
        .expect("returns on stop");
        let t = t0();
        assert_eq!(
            exec.ids(),
            [t, t + 3 * MIN, t + 6 * MIN].map(|s| format!("feed:hl_ctx:{s}:0"))
        );
        assert_eq!(exec.max_running.load(SeqCst), 1);
        let h = rig.health("hl_ctx").await;
        assert_eq!((h.state, h.items, h.dropped), (FeedState::Live, 3, 0));
    }

    #[tokio::test]
    async fn fan_out_runs_in_order_with_deterministic_ids() {
        let rig = Rig::at(t0());
        let exec = Arc::new(FakeExec::new(&rig.clock));
        let coins = ["xyz:TSLA", "xyz:NVDA", "xyz:AAPL", "xyz:MSFT"];
        let spec = tool_spec("hl_book", every(5 * MIN), &exec, &coins);
        let mut feed = FeedRunner::new(spec, rig.env("hl_book", 0.5));
        let next = feed.next(t0()).unwrap();
        assert_eq!(next, slot(t0()).with_late(5 * MIN));
        feed.run(next, t0(), &rig.stopper.subscribe()).await;
        let calls = exec.calls.lock().unwrap().clone();
        let t = t0();
        assert_eq!(
            calls.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            (0..4)
                .map(|i| format!("feed:hl_book:{t}:{i}"))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            calls
                .iter()
                .map(|c| c.arguments["coin"].clone())
                .collect::<Vec<_>>(),
            coins.map(|c| json!(c))
        );
        assert!(calls.iter().all(|c| c.name == "hl_book"));
        assert_eq!(rig.health("hl_book").await.items, 4);

        // concurrency = 2: two calls overlap, started in order.
        let mut two = FakeExec::new(&rig.clock);
        two.yields = 20;
        let two = Arc::new(two);
        let mut spec = tool_spec("hl_book2", every(5 * MIN), &two, &coins);
        if let FeedJob::Tool { concurrency, .. } = &mut spec.job {
            *concurrency = 2;
        }
        let mut feed = FeedRunner::new(spec, rig.env("hl_book2", 0.5));
        feed.run(slot(t0()), t0(), &rig.stopper.subscribe()).await;
        assert_eq!(two.max_running.load(SeqCst), 2);
        assert_eq!(
            two.ids(),
            (0..4)
                .map(|i| format!("feed:hl_book2:{t}:{i}"))
                .collect::<Vec<_>>()
        );
    }

    impl Next {
        fn with_late(mut self, late_ms: i64) -> Self {
            self.late_ms = late_ms;
            self
        }
    }

    #[tokio::test]
    async fn error_rows_back_off_and_retry_only_the_failed_calls() {
        let rig = Rig::at(t0());
        let exec = Arc::new(FakeExec::new(&rig.clock));
        let spec = tool_spec("hl_book", every(MIN), &exec, &["A", "B", "C"]);
        let mut feed = FeedRunner::new(spec, rig.env("hl_book", 1.0));
        let stop = rig.stopper.subscribe();
        let t = t0();

        // B is rate limited (Retry-After 5 s): A and C count, B is retried.
        exec.script(1, Reply::ErrorRow(ErrorClass::RateLimited, Some(5_000)));
        feed.run(feed.next(t).unwrap(), t, &stop).await;
        let h = rig.health("hl_book").await;
        assert_eq!((h.state, h.items), (FeedState::Live, 2));
        assert_eq!(h.last_error_class, Some(ErrorClass::RateLimited));
        assert_eq!(h.last_error.as_deref(), Some("429 from <url>"));
        let retry = feed.next(t).unwrap();
        assert_eq!(
            retry,
            Next {
                at_ms: t + 5_000,
                slot_ms: t,
                late_ms: i64::MAX,
                run: Run::Retry(vec![1]),
                at_tick: false,
            }
        );
        exec.script.lock().unwrap().clear();
        rig.clock.set(t + 5_000);
        feed.run(retry, t + 5_000, &stop).await;
        assert_eq!(
            exec.ids()[3],
            format!("feed:hl_book:{t}:1"),
            "same id on retry"
        );
        assert_eq!(feed.next(t + 5_000).unwrap(), slot(t + MIN));

        // Every call transient: the retries back off 1, 2, 4, 8, 16 s
        // (rand = 1) …
        for i in 0..3 {
            exec.script(i, Reply::ErrorRow(ErrorClass::Transient, None));
        }
        rig.clock.set(t + MIN);
        feed.run(slot(t + MIN), t + MIN, &stop).await;
        assert_eq!(rig.health("hl_book").await.state, FeedState::Backoff);
        let mut now = t + MIN;
        for want in [1_000, 2_000, 4_000, 8_000, 16_000] {
            let r = feed.next(now).unwrap();
            assert_eq!(
                (r.at_ms - now, r.slot_ms, r.run.clone()),
                (want, t + MIN, Run::Retry(vec![0, 1, 2]))
            );
            now = r.at_ms;
            rig.clock.set(now);
            feed.run(r, now, &stop).await;
        }
        // … until the next retry (+32 s = t+123 s) would come after the slot
        // at t+120 s: the slot runs every call instead.
        assert_eq!(now, t + MIN + 31_000);
        assert_eq!(feed.next(now).unwrap(), slot(t + 2 * MIN));
        exec.script.lock().unwrap().clear();
        let n = exec.ids().len();
        rig.clock.set(t + 2 * MIN);
        feed.run(slot(t + 2 * MIN), t + 2 * MIN, &stop).await;
        assert_eq!(exec.ids().len(), n + 3);
        assert!(feed.retry.is_none() && feed.failures == 0);
        assert_eq!(rig.health("hl_book").await.state, FeedState::Live);

        // Quota exhausted (Retry-After 90 s): parked — the slot at t+4 min is
        // skipped, the failed call retried at t+4.5 min, then t+5 min.
        exec.script(0, Reply::ErrorRow(ErrorClass::QuotaExhausted, Some(90_000)));
        let now = t + 3 * MIN;
        rig.clock.set(now);
        feed.run(slot(now), now, &stop).await;
        let r = feed.next(now).unwrap();
        assert_eq!(
            (r.at_ms, r.run.clone()),
            (now + 90_000, Run::Retry(vec![0]))
        );
        // Auth errors are not retried: down; the next slot runs as usual.
        exec.script.lock().unwrap().clear();
        exec.script(0, Reply::ErrorRow(ErrorClass::AuthRequired, None));
        let now = r.at_ms;
        rig.clock.set(now);
        feed.run(r, now, &stop).await;
        let h = rig.health("hl_book").await;
        assert_eq!(
            (h.state, h.last_error_class),
            (FeedState::Down, Some(ErrorClass::AuthRequired))
        );
        assert_eq!(feed.next(now).unwrap(), slot(t + 5 * MIN));
    }

    #[tokio::test]
    async fn the_most_retry_worthy_error_of_a_row_leads() {
        use ErrorClass::*;
        let t = t0();
        for (errors, class, retry_in_ms) in [
            // A rate limit behind a transient error: its Retry-After wins.
            (
                vec![(Transient, None), (RateLimited, Some(30_000))],
                RateLimited,
                Some(30_000),
            ),
            // Retryable beats not retryable, wherever it sits.
            (
                vec![(AuthRequired, None), (Timeout, None)],
                Timeout,
                Some(1_000),
            ),
            // Nothing retryable: the first error, no retry.
            (vec![(Decode, None), (AuthRequired, None)], Decode, None),
        ] {
            let rig = Rig::at(t);
            let exec = Arc::new(FakeExec::new(&rig.clock));
            exec.script(0, Reply::Errors(errors.clone()));
            let spec = tool_spec("f", every(5 * MIN), &exec, &["A"]);
            let mut feed = FeedRunner::new(spec, rig.env("f", 0.0));
            feed.run(slot(t), t, &rig.stopper.subscribe()).await;
            let h = rig.health("f").await;
            assert_eq!(h.last_error_class, Some(class), "{errors:?}");
            assert_eq!(h.last_error.as_deref(), Some(class.as_str()), "{errors:?}");
            let next = feed.next(t).unwrap();
            let want =
                retry_in_ms.map_or((t + 5 * MIN, Run::Slot), |ms| (t + ms, Run::Retry(vec![0])));
            assert_eq!((next.at_ms, next.run), want, "{errors:?}");
        }
    }

    #[tokio::test]
    async fn feed_rows_land_in_the_registered_store() {
        use crate::application::observe::tests::MemStore;
        use crate::ports::observation::ObservationStore;
        let rig = Rig::at(t0());
        let store = Arc::new(MemStore::default());
        let exec = Arc::new(FakeExec::new(&rig.clock));
        exec.script(1, Reply::ErrorRow(ErrorClass::Timeout, None));
        let spec = tool_spec("hl_book", every(MIN), &exec, &["A", "B"]);
        let env = FeedEnv {
            health: rig.board.feed(
                "hl_book",
                true,
                180,
                Some(store.clone() as Arc<dyn ObservationStore>),
                t0(),
            ),
            ..rig.env("unused", 0.0)
        };
        assert!(
            store.get("feed/1:hl_book").await.unwrap().is_none(),
            "none before an item"
        );
        let mut feed = FeedRunner::new(spec, env);
        feed.run(slot(t0()), t0(), &rig.stopper.subscribe()).await;
        let row = store
            .get("feed/1:hl_book")
            .await
            .unwrap()
            .expect("feed/1 row");
        assert_eq!(row.observed_at_ms, t0(), "stamped with the last item");
        let h: FeedHealth = row.typed().unwrap();
        assert_eq!(
            (h.state, h.items, h.required, h.stale_after_s),
            (FeedState::Live, 1, true, 180)
        );
        assert_eq!(h.last_error_class, Some(ErrorClass::Timeout));
    }

    #[tokio::test]
    async fn one_failing_call_does_not_starve_the_others() {
        let rig = Rig::at(t0());
        let exec = Arc::new(FakeExec::new(&rig.clock));
        let spec = tool_spec("hl_book", every(MIN), &exec, &["A", "B"]);
        let mut feed = FeedRunner::new(spec, rig.env("hl_book", 1.0));
        let stop = rig.stopper.subscribe();
        exec.script(1, Reply::ErrorRow(ErrorClass::Transient, None));
        let mut now = t0();
        let mut slots = Vec::new();
        for _ in 0..40 {
            let next = feed.next(now).unwrap();
            now = next.at_ms;
            rig.clock.set(now);
            if next.run == Run::Slot {
                slots.push(next.slot_ms);
            }
            feed.run(next, now, &stop).await;
        }
        // B keeps failing and backing off; every minute slot still runs.
        let t = t0();
        assert_eq!(
            slots[..5],
            [t, t + MIN, t + 2 * MIN, t + 3 * MIN, t + 4 * MIN]
        );
        let a_calls = exec.ids().iter().filter(|id| id.ends_with(":0")).count();
        assert_eq!(a_calls, slots.len());
    }

    #[tokio::test]
    async fn executor_errors_and_http_text_are_classified() {
        for (reply, want) in [
            (
                Reply::Fail("Tool 'hl_book' is not available to this agent."),
                Some(ErrorClass::Fatal),
            ),
            (
                Reply::Text("HTTP 503 Service Unavailable\n{}"),
                Some(ErrorClass::Transient),
            ),
            (
                Reply::Text("HTTP 429 Too Many Requests"),
                Some(ErrorClass::RateLimited),
            ),
            (
                Reply::Text("HTTP 403 Forbidden"),
                Some(ErrorClass::AuthRequired),
            ),
            (Reply::Text("HTTP 404 Not Found"), Some(ErrorClass::Fatal)),
            (Reply::Text("HTTP 200 OK\n{\"a\":1}"), None),
            (Reply::Text("plain text"), None),
        ] {
            let rig = Rig::at(t0());
            let exec = Arc::new(FakeExec::new(&rig.clock));
            exec.script(0, reply);
            let spec = tool_spec("f", every(MIN), &exec, &["A"]);
            let mut feed = FeedRunner::new(spec, rig.env("f", 0.0));
            feed.run(slot(t0()), t0(), &rig.stopper.subscribe()).await;
            let h = rig.health("f").await;
            assert_eq!(h.last_error_class, want);
            assert_eq!(h.items, u64::from(want.is_none()));
        }
    }

    /// Review #14: an at-tick slot's next run is a day away, so a failure
    /// the backoff stops on (an executor error is `fatal`: a busy store, an
    /// IO error) is retried as transient — same id, 1 s, 2 s … — until
    /// 15 min after the slot; then it waits for the next tick. A grid feed
    /// still waits for its next slot.
    #[tokio::test]
    async fn a_failed_at_tick_is_retried_within_a_bounded_window() {
        let rig = Rig::at(utc("2026-10-04 00:00"));
        let exec = Arc::new(FakeExec::new(&rig.clock));
        exec.script(0, Reply::Fail("database is locked"));
        let daily = Schedule {
            zone: Zone::Utc,
            every_ms: None,
            windows: vec![],
            at: vec![parse_at("daily 00:01").unwrap()],
        };
        let spec = tool_spec("risk_day", daily, &exec, &["A"]);
        let mut feed = FeedRunner::new(spec, rig.env("risk_day", 1.0));
        let stop = rig.stopper.subscribe();
        let tick = utc("2026-10-04 00:01");
        let next = feed.next(rig.clock.now_ms()).unwrap();
        assert_eq!((next.slot_ms, next.at_tick), (tick, true));
        rig.clock.set(tick);
        feed.run(next, tick, &stop).await;
        let h = rig.health("risk_day").await;
        assert_eq!(
            (h.state, h.last_error_class),
            (FeedState::Backoff, Some(ErrorClass::Fatal))
        );
        let mut now = tick;
        for want in [1_000, 2_000, 4_000] {
            let r = feed.next(now).unwrap();
            assert_eq!(
                (r.at_ms - now, r.slot_ms, r.run.clone(), r.at_tick),
                (want, tick, Run::Retry(vec![0]), true)
            );
            now = r.at_ms;
            rig.clock.set(now);
            feed.run(r, now, &stop).await;
        }
        assert!(exec
            .ids()
            .iter()
            .all(|id| *id == format!("feed:risk_day:{tick}:0")));
        // The window closes: a failure then waits for the next tick.
        let r = feed.next(now).unwrap();
        let late = tick + AT_RETRY_WINDOW_MS;
        rig.clock.set(late);
        feed.run(r, late, &stop).await;
        let next = feed.next(late).unwrap();
        assert_eq!(
            (next.slot_ms, next.run.clone()),
            (utc("2026-10-05 00:01"), Run::Slot)
        );
        assert_eq!(rig.health("risk_day").await.state, FeedState::Down);
        // It succeeds: live, the count starts over.
        exec.script.lock().unwrap().clear();
        rig.clock.set(next.at_ms);
        feed.run(next, utc("2026-10-05 00:01"), &stop).await;
        assert_eq!(rig.health("risk_day").await.state, FeedState::Live);
        assert!(feed.retry.is_none() && feed.failures == 0);

        // A grid feed: an executor error waits for the next slot.
        let grid = Arc::new(FakeExec::new(&rig.clock));
        grid.script(0, Reply::Fail("database is locked"));
        let t = t0();
        let mut feed =
            FeedRunner::new(tool_spec("g", every(MIN), &grid, &["A"]), rig.env("g", 1.0));
        feed.run(slot(t), t, &stop).await;
        assert_eq!(feed.next(t).unwrap(), slot(t + MIN));
        // The start run of an at-only feed is retried like its ticks.
        assert!(Next::start(t, true).at_tick && !Next::start(t, false).at_tick);
    }

    #[test]
    fn failure_messages_are_clipped_at_whole_words_without_urls() {
        let id = "robinhood:0x322F0929c4625eD5bAd873c95208D54E1c003b2d";
        let long = format!("{} {id} tail", "word ".repeat(36));
        let clipped = clip(&long);
        assert!(
            clipped.chars().count() <= MAX_MESSAGE_CHARS + 2,
            "{clipped}"
        );
        assert!(clipped.ends_with(" …"), "{clipped}");
        // The id did not fit: dropped whole, never cut.
        assert!(!clipped.contains("robinhood"), "{clipped}");
        let fits = format!("{} {id}", "word ".repeat(20));
        assert!(clip(&fits).ends_with(id), "{}", clip(&fits));
        let one_word = "x".repeat(300);
        assert_eq!(
            clip(&one_word),
            one_word,
            "a single long word is kept whole"
        );
        assert_eq!(
            clip("HTTP 503 https://api.hyperliquid.xyz/info\nbody"),
            "HTTP 503 <url> body"
        );
    }

    #[tokio::test]
    async fn late_slots_are_skipped_not_replayed() {
        let rig = Rig::at(t0());
        let exec = Arc::new(FakeExec::new(&rig.clock));
        let spec = tool_spec("hl_ctx", every(MIN), &exec, &["A"]);
        let mut feed = FeedRunner::new(spec, rig.env("hl_ctx", 0.0));
        let next = feed.next(t0()).unwrap();
        // Woke 61 s after the slot (grace = the 60 s interval).
        let now = t0() + 61_000;
        rig.clock.set(now);
        feed.run(next, now, &rig.stopper.subscribe()).await;
        assert!(exec.ids().is_empty());
        assert_eq!(rig.health("hl_ctx").await.dropped, 1);
        assert_eq!(feed.next(now).unwrap().slot_ms, t0() + 2 * MIN);
        // Within the grace it still runs.
        let next = feed.next(now).unwrap();
        feed.run(next, t0() + 2 * MIN + 59_000, &rig.stopper.subscribe())
            .await;
        assert_eq!(exec.ids().len(), 1);
        // An at-tick two minutes late is skipped.
        let rig = Rig::at(t0());
        let exec = Arc::new(FakeExec::new(&rig.clock));
        let tick = Schedule {
            zone: Zone::NewYork,
            every_ms: None,
            windows: vec![],
            at: vec![parse_at("Sun 18:00").unwrap()],
        };
        let mut feed = FeedRunner::new(
            tool_spec("entry", tick, &exec, &["A"]),
            rig.env("entry", 0.5),
        );
        let next = feed.next(t0()).unwrap();
        assert_eq!((next.at_ms, next.late_ms), (utc("2026-10-04 22:00"), MIN));
        feed.run(next, utc("2026-10-04 22:02"), &rig.stopper.subscribe())
            .await;
        assert!(exec.ids().is_empty());
    }

    /// Records events; each waits for a permit when `gate` is set.
    #[derive(Default)]
    struct TickLoop {
        seen: Mutex<Vec<(Value, String)>>,
        gate: Option<tokio::sync::Semaphore>,
        stop_after: Option<(usize, Stopper)>,
    }

    #[async_trait]
    impl LoopHandler for TickLoop {
        async fn handle(&self, event: &Value, session_id: &str) -> anyhow::Result<()> {
            let n = {
                let mut seen = self.seen.lock().unwrap();
                seen.push((event.clone(), session_id.to_string()));
                seen.len()
            };
            if let Some(gate) = &self.gate {
                gate.acquire().await?.forget();
            }
            if let Some((after, stopper)) = &self.stop_after {
                if n >= *after {
                    stopper.stop("test done", false);
                }
            }
            Ok(())
        }
    }

    fn tick_spec(
        name: &str,
        schedule: Schedule,
        loops: &Arc<LoopDispatch>,
        target: &str,
    ) -> FeedSpec {
        FeedSpec {
            name: name.into(),
            schedule,
            jitter_pct: 0,
            run_on_start: false,
            job: FeedJob::Tick {
                loops: Arc::clone(loops),
                target: target.into(),
                event: Map::from_iter([("phase".to_string(), json!("entry"))]),
            },
        }
    }

    async fn until(what: &str, f: impl Fn() -> bool) {
        for _ in 0..500 {
            if f() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        panic!("timed out waiting for {what}");
    }

    #[tokio::test]
    async fn tick_feed_sends_one_event_at_a_time() {
        let rig = Rig::at(t0());
        let handler = Arc::new(TickLoop {
            gate: Some(tokio::sync::Semaphore::new(0)),
            ..Default::default()
        });
        let loops = Arc::new(LoopDispatch::new(
            BTreeMap::from([(
                "xm_main".to_string(),
                Arc::clone(&handler) as Arc<dyn LoopHandler>,
            )]),
            4,
            crate::config::runtime::DEFAULT_MAX_QUEUED_PER_LOOP,
        ));
        let spec = tick_spec("exit_tick", every(MIN), &loops, "xm_main");
        let mut feed = FeedRunner::new(spec, rig.env("exit_tick", 0.5));
        let stop = rig.stopper.subscribe();
        let t = t0();
        feed.run(slot(t), t, &stop).await;
        until("first event", || handler.seen.lock().unwrap().len() == 1).await;
        // Still running: the next tick is dropped, not queued.
        feed.run(slot(t + MIN), t + MIN, &stop).await;
        let h = rig.health("exit_tick").await;
        assert_eq!((h.items, h.dropped), (1, 1));
        handler.gate.as_ref().unwrap().add_permits(1);
        until("first event done", || {
            loops.stats()["xm_main"].completed == 1
        })
        .await;
        handler.gate.as_ref().unwrap().add_permits(1);
        feed.run(slot(t + 2 * MIN), t + 2 * MIN, &stop).await;
        until("second event", || handler.seen.lock().unwrap().len() == 2).await;
        let seen = handler.seen.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec![
                (
                    json!({"phase": "entry", "ts_ms": t}),
                    format!("exit_tick:{t}")
                ),
                (
                    json!({"phase": "entry", "ts_ms": t + 2 * MIN}),
                    format!("exit_tick:{}", t + 2 * MIN)
                ),
            ]
        );
        // A target that is not built reports the feed down.
        let spec = tick_spec("orphan", every(MIN), &loops, "nope");
        let mut feed = FeedRunner::new(spec, rig.env("orphan", 0.5));
        feed.run(slot(t), t, &stop).await;
        let h = rig.health("orphan").await;
        assert_eq!(
            (h.state, h.last_error_class),
            (FeedState::Down, Some(ErrorClass::Fatal))
        );
    }

    /// A tick: `feed.fired` → the loop's `loop.queued` caused by it (same
    /// session = one correlation) → `feed.tick_sent`; a tick while the last
    /// one runs is `feed.dropped` (`previous_tick_running`), as health counts it.
    #[tokio::test]
    async fn tick_trace_links_loop_event() {
        use crate::application::trace_exec::tests::MemTrace;
        let sink = Arc::new(MemTrace::default());
        let rig = Rig::at(t0());
        let handler = Arc::new(TickLoop {
            gate: Some(tokio::sync::Semaphore::new(0)),
            ..Default::default()
        });
        let loops = Arc::new(
            LoopDispatch::new(
                BTreeMap::from([(
                    "demo".to_string(),
                    Arc::clone(&handler) as Arc<dyn LoopHandler>,
                )]),
                1,
                4,
            )
            .with_trace(sink.clone()),
        );
        let spec = tick_spec("tick", every(MIN), &loops, "demo");
        let env = FeedEnv {
            trace: Some(sink.clone()),
            ..rig.env("tick", 0.5)
        };
        let mut feed = FeedRunner::new(spec, env);
        let stop = rig.stopper.subscribe();
        let t = t0();
        feed.run(slot(t), t, &stop).await;
        until("started", || loops.stats()["demo"].in_flight == 1).await;
        feed.run(slot(t + MIN), t + MIN, &stop).await;
        handler.gate.as_ref().unwrap().add_permits(1);
        until("done", || loops.stats()["demo"].completed == 1).await;

        let d = sink.all();
        let session = format!("tick:{t}");
        let first: Vec<(usize, &EventDraft)> = d
            .iter()
            .enumerate()
            .filter(|(_, e)| e.session_id.as_deref() == Some(session.as_str()))
            .collect();
        let kinds: Vec<&str> = first.iter().map(|(_, e)| e.kind.as_str()).collect();
        assert_eq!(
            kinds,
            [
                "feed.fired",
                "loop.queued",
                "feed.tick_sent",
                "loop.started",
                "loop.completed"
            ]
        );
        let fired = MemTrace::id(first[0].0);
        assert_eq!(first[0].1.node_id.as_deref(), Some("feed:tick"));
        assert_eq!(first[0].1.payload["event"]["ts_ms"], json!(t));
        assert_eq!(first[1].1.parent_event_id.as_deref(), Some(fired.as_str()));
        assert_eq!(first[2].1.parent_event_id.as_deref(), Some(fired.as_str()));
        assert_eq!(first[2].1.status, Status::Ok);
        assert!(
            first.iter().all(|(_, e)| e.correlation_id.is_none()),
            "session = correlation"
        );
        let dropped = d
            .iter()
            .find(|e| e.session_id.as_deref() == Some(format!("tick:{}", t + MIN).as_str()))
            .unwrap();
        assert_eq!(dropped.kind, "feed.dropped");
        assert_eq!(dropped.payload["reason"], json!("previous_tick_running"));
        assert_eq!(rig.health("tick").await.dropped, 1);
    }

    /// A tool run: `feed.fired` → each call's `tool.*` caused by it
    /// (correlation `feed:<n>:<slot>`) → `feed.failed` for a fatal executor
    /// error (`retrying: false`, health `down`), `feed.retrying` with the
    /// delay for a retryable one, `feed.completed` when every call is ok.
    #[tokio::test]
    async fn fatal_tool_failure_traces_failed_not_retrying() {
        use crate::application::trace_exec::tests::MemTrace;
        use crate::application::trace_exec::TracedExecutor;
        let sink = Arc::new(MemTrace::default());
        let rig = Rig::at(t0());
        let exec = Arc::new(FakeExec::new(&rig.clock));
        exec.script(
            0,
            Reply::Fail("read in/absent.txt: No such file or directory"),
        );
        let traced: Arc<dyn ToolExecutor> =
            Arc::new(TracedExecutor::new(exec.clone(), sink.clone(), "lab", None));
        let mut spec = tool_spec("probe", every(MIN), &exec, &["A"]);
        if let FeedJob::Tool { executor, .. } = &mut spec.job {
            *executor = traced;
        }
        let env = FeedEnv {
            trace: Some(sink.clone()),
            ..rig.env("probe", 0.0)
        };
        let mut feed = FeedRunner::new(spec, env);
        let stop = rig.stopper.subscribe();
        let t = t0();
        feed.run(slot(t), t, &stop).await;
        let d = sink.all();
        assert_eq!(
            sink.kinds(),
            ["feed.fired", "tool.started", "tool.failed", "feed.failed"]
        );
        let fired = MemTrace::id(0);
        assert_eq!(d[0].payload["run"], json!("slot"));
        assert_eq!(d[0].payload["calls"], json!([0]));
        assert_eq!(d[1].parent_event_id.as_deref(), Some(fired.as_str()));
        assert_eq!(d[1].call_id, Some(format!("feed:probe:{t}:0")));
        assert_eq!(d[1].correlation_id, Some(format!("feed:probe:{t}")));
        assert_eq!(d[1].session_id, Some(format!("probe:{t}")));
        assert_eq!(d[3].parent_event_id.as_deref(), Some(fired.as_str()));
        assert_eq!(d[3].status, Status::Failed);
        assert_eq!(d[3].payload["class"], json!("fatal"));
        assert_eq!(d[3].payload["retrying"], json!(false));
        assert_eq!(rig.health("probe").await.state, FeedState::Down);

        // Retryable: `feed.retrying` (Pending) with the delay; then ok.
        exec.script(0, Reply::ErrorRow(ErrorClass::Timeout, None));
        feed.run(slot(t + MIN), t + MIN, &stop).await;
        exec.script.lock().unwrap().remove(&0);
        let d = sink.all();
        let last = d.last().unwrap();
        assert_eq!(
            (last.kind.as_str(), last.status),
            ("feed.retrying", Status::Pending)
        );
        assert_eq!(last.payload["class"], json!("timeout"));
        assert!(last.payload["retry_in_ms"].as_u64().unwrap() > 0);
        let next = feed.next(t + MIN).unwrap();
        assert_eq!(next.run, Run::Retry(vec![0]));
        feed.run(next.clone(), next.at_ms, &stop).await;
        let d = sink.all();
        let fired = d.iter().rev().find(|e| e.kind == "feed.fired").unwrap();
        assert_eq!(fired.payload["run"], json!("retry"));
        assert_eq!(d.last().unwrap().kind, "feed.completed");

        // Stop requested before the calls: nothing runs, the run's
        // `feed.fired` is closed by `feed.skipped` (no health change).
        let calls = exec.calls.lock().unwrap().len();
        rig.stopper.stop("test stop", false);
        let next = feed.next(t + 2 * MIN).unwrap();
        feed.run(next.clone(), next.at_ms, &stop).await;
        assert_eq!(exec.calls.lock().unwrap().len(), calls, "no call ran");
        let d = sink.all();
        let (fired, last) = (&d[d.len() - 2], d.last().unwrap());
        assert_eq!(fired.kind, "feed.fired");
        assert_eq!(
            (last.kind.as_str(), last.status),
            ("feed.skipped", Status::Skipped)
        );
        assert_eq!(last.parent_event_id, Some(MemTrace::id(d.len() - 2)));
        assert_eq!(last.payload["reason"], json!("shutting_down"));
    }

    #[tokio::test]
    async fn at_tick_feed_runs_end_to_end_on_new_york_wall_time() {
        // Friday 20:00 ET before the weekend run; the tick is Sun 18:00 EDT.
        let rig = Rig::at(utc("2026-10-03 00:00"));
        let handler = Arc::new(TickLoop {
            stop_after: Some((1, rig.stopper.clone())),
            ..Default::default()
        });
        let loops = Arc::new(LoopDispatch::new(
            BTreeMap::from([(
                "xm_main".to_string(),
                Arc::clone(&handler) as Arc<dyn LoopHandler>,
            )]),
            4,
            crate::config::runtime::DEFAULT_MAX_QUEUED_PER_LOOP,
        ));
        let schedule = Schedule {
            zone: Zone::NewYork,
            every_ms: None,
            windows: vec![],
            at: vec![parse_at("Sun 18:00").unwrap()],
        };
        let spec = tick_spec("weekend_entry", schedule, &loops, "xm_main");
        tokio::time::timeout(
            Duration::from_secs(10),
            run_feed(spec, rig.env("weekend_entry", 0.5), rig.stopper.subscribe()),
        )
        .await
        .expect("returns on stop");
        let seen = handler.seen.lock().unwrap().clone();
        let at = utc("2026-10-04 22:00");
        assert_eq!(seen[0].0, json!({"phase": "entry", "ts_ms": at}));
        assert_eq!(seen[0].1, format!("weekend_entry:{at}"));
    }

    #[tokio::test]
    async fn run_on_start_fires_at_once_then_on_the_grid() {
        let start = t0() + 10_000;
        let rig = Rig::at(start);
        let mut exec = FakeExec::new(&rig.clock);
        exec.stop_after = Some((2, rig.stopper.clone()));
        let exec = Arc::new(exec);
        let mut spec = tool_spec("hl_ctx", every(MIN), &exec, &["A"]);
        spec.run_on_start = true;
        spec.jitter_pct = 50;
        tokio::time::timeout(
            Duration::from_secs(10),
            run_feed(spec, rig.env("hl_ctx", 1.0), rig.stopper.subscribe()),
        )
        .await
        .expect("returns on stop");
        assert_eq!(
            exec.ids(),
            [start, t0() + MIN].map(|s| format!("feed:hl_ctx:{s}:0"))
        );
        // Jitter delays the call (50 % of 60 s at rand 1), not the slot id.
        assert_eq!(rig.clock.now_ms(), t0() + MIN + 30_000);
    }

    /// A clock whose sleeps never end: only the stop signal ends a nap.
    struct Frozen(i64);

    #[async_trait]
    impl Clock for Frozen {
        fn now_ms(&self) -> i64 {
            self.0
        }
        async fn sleep_until_ms(&self, _t_ms: i64) {
            std::future::pending::<()>().await;
        }
    }

    #[tokio::test]
    async fn stop_interrupts_a_nap_and_skips_remaining_calls() {
        // The next slot is an hour away and the clock never gets there.
        let rig = Rig::at(0);
        let exec = Arc::new(FakeExec::new(&rig.clock));
        let spec = tool_spec("slow", every(3_600_000), &exec, &["A"]);
        let env = FeedEnv {
            clock: Arc::new(Frozen(1)),
            ..rig.env("slow", 0.0)
        };
        let stopper = rig.stopper.clone();
        let task = tokio::spawn(run_feed(spec, env, rig.stopper.subscribe()));
        tokio::time::sleep(Duration::from_millis(30)).await;
        stopper.stop("SIGTERM", false);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("returns promptly")
            .unwrap();
        // Stop already fired: a run makes no call.
        let spec = tool_spec("late", every(MIN), &exec, &["A", "B"]);
        let mut feed = FeedRunner::new(spec, rig.env("late", 0.0));
        feed.run(slot(0), 0, &rig.stopper.subscribe()).await;
        assert!(exec.ids().is_empty());
    }
}
