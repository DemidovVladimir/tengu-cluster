//! Tests of `src/application/runtime/feeds.rs`.

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
                        ..ReadError::new("book", class, "429 from https://api.hyperliquid.xyz/info")
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
        let want = retry_in_ms.map_or((t + 5 * MIN, Run::Slot), |ms| (t + ms, Run::Retry(vec![0])));
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
    let mut feed = FeedRunner::new(tool_spec("g", every(MIN), &grid, &["A"]), rig.env("g", 1.0));
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

fn tick_spec(name: &str, schedule: Schedule, loops: &Arc<LoopDispatch>, target: &str) -> FeedSpec {
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

/// Records each run; fails (`fatal`) while `failing`.
struct FakeJob {
    clock: Arc<ManualClock>,
    /// Virtual ms each run takes.
    run_ms: i64,
    failing: std::sync::atomic::AtomicBool,
    runs: Mutex<Vec<(i64, String)>>,
    stop_after: Option<(usize, Stopper)>,
}

impl FakeJob {
    fn new(clock: &Arc<ManualClock>) -> Self {
        Self {
            clock: Arc::clone(clock),
            run_ms: 0,
            failing: false.into(),
            runs: Mutex::new(vec![]),
            stop_after: None,
        }
    }

    fn runs(&self) -> Vec<(i64, String)> {
        self.runs.lock().unwrap().clone()
    }
}

#[async_trait]
impl RuntimeJob for FakeJob {
    async fn run(&self, slot_ms: i64, run_id: &str) -> JobOutcome {
        let n = {
            let mut runs = self.runs.lock().unwrap();
            runs.push((slot_ms, run_id.to_string()));
            runs.len()
        };
        self.clock.advance(self.run_ms);
        if let Some((after, stopper)) = &self.stop_after {
            if n >= *after {
                stopper.stop("test done", false);
            }
        }
        if self.failing.load(SeqCst) {
            JobOutcome::Failed {
                class: ErrorClass::Fatal,
                message: "cycle_unfinished: cycles/2026-W41 was claimed and never frozen".into(),
            }
        } else {
            JobOutcome::Done {
                note: "cycles/2026-W41 frozen".into(),
            }
        }
    }
}

fn job_spec(name: &str, schedule: Schedule, job: &Arc<FakeJob>) -> FeedSpec {
    FeedSpec {
        name: name.into(),
        schedule,
        jitter_pct: 0,
        run_on_start: false,
        job: FeedJob::Job {
            job: Arc::clone(job) as Arc<dyn RuntimeJob>,
        },
    }
}

/// A job run is awaited like a tool run: one per slot, never two at
/// once; slots missed while it ran are not replayed. Each run gets its
/// slot and the id `feed:<name>:<slot ms>`; a done run is one item.
#[tokio::test]
async fn job_feed_runs_once_per_slot() {
    let rig = Rig::at(t0());
    let mut job = FakeJob::new(&rig.clock);
    job.run_ms = 150_000; // each run outlasts two slots
    job.stop_after = Some((3, rig.stopper.clone()));
    let job = Arc::new(job);
    let spec = job_spec("soe_week", every(MIN), &job);
    tokio::time::timeout(
        Duration::from_secs(10),
        run_feed(spec, rig.env("soe_week", 0.5), rig.stopper.subscribe()),
    )
    .await
    .expect("returns on stop");
    let t = t0();
    assert_eq!(
        job.runs(),
        [t, t + 3 * MIN, t + 6 * MIN].map(|s| (s, format!("feed:soe_week:{s}")))
    );
    let h = rig.health("soe_week").await;
    assert_eq!((h.state, h.items, h.dropped), (FeedState::Live, 3, 0));
    // Stopped before a run: it does not start.
    let idle = Arc::new(FakeJob::new(&rig.clock));
    let mut feed = FeedRunner::new(job_spec("idle", every(MIN), &idle), rig.env("idle", 0.5));
    let stop = rig.stopper.subscribe();
    feed.run(slot(t), t, &stop).await;
    assert!(idle.runs().is_empty());
}

/// The weekly cycle fires once a week (`Mon 07:00` Paris): a failed run
/// (`fatal`, e.g. an unfinished cycle) retries as transient — same slot,
/// same id — until 15 min after the slot, then waits for the next week.
#[tokio::test]
async fn job_failure_retries_in_at_window() {
    let rig = Rig::at(utc("2026-10-04 00:00"));
    let job = Arc::new(FakeJob::new(&rig.clock));
    job.failing.store(true, SeqCst);
    let weekly = Schedule {
        zone: Zone::Paris,
        every_ms: None,
        windows: vec![],
        at: vec![parse_at("Mon 07:00").unwrap()],
    };
    let mut feed = FeedRunner::new(job_spec("soe_week", weekly, &job), rig.env("soe_week", 1.0));
    let stop = rig.stopper.subscribe();
    let tick = utc("2026-10-05 05:00"); // 07:00 CEST
    let next = feed.next(rig.clock.now_ms()).unwrap();
    assert_eq!((next.slot_ms, next.at_tick), (tick, true));
    rig.clock.set(tick);
    feed.run(next, tick, &stop).await;
    let h = rig.health("soe_week").await;
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
    assert_eq!(job.runs().len(), 4);
    assert!(job
        .runs()
        .iter()
        .all(|(s, id)| *s == tick && *id == format!("feed:soe_week:{tick}")));
    // The window closes: a failure then waits for next Monday.
    let r = feed.next(now).unwrap();
    let late = tick + AT_RETRY_WINDOW_MS;
    rig.clock.set(late);
    feed.run(r, late, &stop).await;
    let next = feed.next(late).unwrap();
    assert_eq!(
        (next.slot_ms, next.run.clone()),
        (utc("2026-10-12 05:00"), Run::Slot)
    );
    assert_eq!(rig.health("soe_week").await.state, FeedState::Down);
    // Done next week (a frozen week is a no-op `Done` too): live again.
    job.failing.store(false, SeqCst);
    rig.clock.set(next.at_ms);
    feed.run(next, utc("2026-10-12 05:00"), &stop).await;
    assert_eq!(rig.health("soe_week").await.state, FeedState::Live);
    assert!(feed.retry.is_none() && feed.failures == 0);
}

/// A job run traces like a tool run: `feed.fired` (`kind = "job"`, the
/// run id; correlation `feed:<n>:<slot>`) → `feed.failed` for a fatal
/// outcome (`retrying: false`, outside an at-tick window) or
/// `feed.completed` when done, each a child of its `feed.fired`.
#[tokio::test]
async fn job_run_traces_fired_then_settled() {
    use crate::application::trace_exec::tests::MemTrace;
    let sink = Arc::new(MemTrace::default());
    let rig = Rig::at(t0());
    let job = Arc::new(FakeJob::new(&rig.clock));
    job.failing.store(true, SeqCst);
    let env = FeedEnv {
        trace: Some(sink.clone()),
        ..rig.env("soe_week", 0.0)
    };
    let mut feed = FeedRunner::new(job_spec("soe_week", every(MIN), &job), env);
    let stop = rig.stopper.subscribe();
    let t = t0();
    feed.run(slot(t), t, &stop).await;
    assert_eq!(sink.kinds(), ["feed.fired", "feed.failed"]);
    let d = sink.all();
    assert_eq!(d[0].status, Status::Running);
    assert_eq!(d[0].payload["kind"], json!("job"));
    assert_eq!(d[0].payload["run"], json!("slot"));
    assert_eq!(d[0].payload["run_id"], json!(format!("feed:soe_week:{t}")));
    assert_eq!(d[0].correlation_id, Some(format!("feed:soe_week:{t}")));
    assert_eq!(d[0].session_id, Some(format!("soe_week:{t}")));
    assert_eq!(d[1].parent_event_id, Some(MemTrace::id(0)));
    assert_eq!(d[1].status, Status::Failed);
    assert_eq!(d[1].payload["class"], json!("fatal"));
    assert_eq!(d[1].payload["retrying"], json!(false));

    job.failing.store(false, SeqCst);
    feed.run(slot(t + MIN), t + MIN, &stop).await;
    let d = sink.all();
    assert_eq!(
        (d[3].kind.as_str(), d[3].status),
        ("feed.completed", Status::Ok)
    );
    assert_eq!(d[3].parent_event_id, Some(MemTrace::id(2)));
    assert_eq!(rig.health("soe_week").await.state, FeedState::Live);
}
