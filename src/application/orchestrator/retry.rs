//! Per-step retry policy: a failed attempt's class ([`StepFailure`], else
//! `transient`) picks what happens next through
//! `domain::backoff::next_delay`.
//!
//! | Class | Next |
//! |---|---|
//! | `fatal`, `decode`, `auth_required`, `not_applicable` | no retry: the step is exhausted at once (a deterministic failure is never paid for twice) |
//! | `transient`, `timeout`, `rate_limited` | the next attempt after 1 s, 2 s, 4 s … (≤ 9 s; no jitter: one step, no herd), up to `max_attempts` |
//! | `quota_exhausted` (a park) | no retry inside a plan |

use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::warn;

use crate::application::orchestrator::events::{EventBus, OrchestratorEvent};
use crate::domain::backoff::{next_delay, BackoffPolicy, Delay};
use crate::domain::observation::ErrorClass;
use crate::domain::plan::{Plan, Step, StepFailure};
use crate::ports::orchestration::WorkerHandle;

/// Waits between attempts of one step (`next_delay`; attempts are counted
/// by [`RetryPolicy::max_attempts`]).
const STEP_BACKOFF: BackoffPolicy = BackoffPolicy {
    base_ms: 1_000,
    cap_ms: 9_000,
    min_ms: 1_000,
    max_attempts: u32::MAX,
    quota_park_ms: 0,
};

#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Attempts per step (`[orchestrator] max_attempts_per_step`).
    pub max_attempts: u32,
    /// Waits between them (module table).
    pub backoff: BackoffPolicy,
}

impl RetryPolicy {
    pub fn new(max_attempts: u32) -> Self {
        Self {
            max_attempts,
            backoff: STEP_BACKOFF,
        }
    }

    /// `max_attempts` attempts, `ms` apart.
    #[cfg(test)]
    pub fn waiting(max_attempts: u32, ms: u64) -> Self {
        Self {
            max_attempts,
            backoff: BackoffPolicy {
                base_ms: ms,
                cap_ms: ms,
                min_ms: ms,
                ..STEP_BACKOFF
            },
        }
    }
}

/// How retryable a failed attempt is: its [`StepFailure`] class, else
/// `transient`.
fn class_of(err: &anyhow::Error) -> ErrorClass {
    err.downcast_ref::<StepFailure>()
        .map_or(ErrorClass::Transient, |f| f.class)
}

/// The wait before the next attempt after attempt `attempt` failed with
/// `class` (module table); `None` = no next attempt.
fn retry_in(class: ErrorClass, attempt: u32, policy: &RetryPolicy) -> Option<Duration> {
    if attempt >= policy.max_attempts {
        return None;
    }
    match next_delay(class, attempt, None, &policy.backoff, 1.0) {
        Delay::Retry(ms) => Some(Duration::from_millis(ms)),
        Delay::Park(_) | Delay::Stop => None,
    }
}

pub enum StepOutcome {
    Ok(String),
    Exhausted(String),
}

pub async fn run_step_with_retry(
    step: &Step,
    step_inputs: &str,
    plan: &Plan,
    worker: Arc<dyn WorkerHandle>,
    policy: &RetryPolicy,
    events: &EventBus,
) -> StepOutcome {
    let mut last_err = String::new();
    for attempt in 1..=policy.max_attempts {
        match worker.run_step(step, step_inputs, plan).await {
            Ok(output) => return StepOutcome::Ok(output),
            Err(err) => {
                last_err = err.to_string();
                let class = class_of(&err);
                let retry_in = retry_in(class, attempt, policy);
                let _ = events.send(OrchestratorEvent::StepFailed {
                    step_id: step.id.clone(),
                    attempt,
                    error: last_err.clone(),
                    retry_in_ms: retry_in.map(|d| d.as_millis() as u64),
                });
                warn!(step = ?step.id, attempt, class = class.as_str(), error = %last_err, "step failed");
                match retry_in {
                    Some(delay) => sleep(delay).await,
                    None => break,
                }
            }
        }
    }
    let _ = events.send(OrchestratorEvent::StepExhausted {
        step_id: step.id.clone(),
        final_error: last_err.clone(),
    });
    StepOutcome::Exhausted(last_err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::orchestrator::events::new_bus;
    use crate::domain::plan::StepId;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct FlakyWorker {
        fails_until: u32,
        calls: Arc<AtomicU32>,
    }
    #[async_trait]
    impl WorkerHandle for FlakyWorker {
        async fn run_step(&self, _: &Step, _: &str, _: &Plan) -> anyhow::Result<String> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if call <= self.fails_until {
                anyhow::bail!("flaky fail #{}", call)
            } else {
                Ok(format!("output from call {}", call))
            }
        }
    }

    fn test_step() -> Step {
        Step {
            id: StepId::new("s1"),
            agent: "x".into(),
            goal: "g".into(),
            depends_on: vec![],
            compose: None,
        }
    }

    fn test_plan() -> Plan {
        Plan {
            steps: vec![test_step()],
        }
    }

    /// Transient waits double from 1 s (≤ 9 s); a deterministic failure
    /// and the last attempt get none.
    #[test]
    fn waits_follow_the_class() {
        let policy = RetryPolicy::new(6);
        let secs = |class, attempt| retry_in(class, attempt, &policy).map(|d| d.as_secs());
        assert_eq!(secs(ErrorClass::Transient, 1), Some(1));
        assert_eq!(secs(ErrorClass::Timeout, 2), Some(2));
        assert_eq!(secs(ErrorClass::Transient, 3), Some(4));
        assert_eq!(secs(ErrorClass::Transient, 5), Some(9));
        assert_eq!(secs(ErrorClass::Transient, 6), None, "the last attempt");
        for class in [
            ErrorClass::Fatal,
            ErrorClass::Decode,
            ErrorClass::AuthRequired,
            ErrorClass::QuotaExhausted,
        ] {
            assert_eq!(secs(class, 1), None, "{class:?}");
        }
    }

    /// A fatal attempt is the last one: one call, exhausted, no retry
    /// announced.
    #[tokio::test]
    async fn a_fatal_failure_is_not_retried() {
        struct Fatal(Arc<AtomicU32>);
        #[async_trait]
        impl WorkerHandle for Fatal {
            async fn run_step(&self, _: &Step, _: &str, _: &Plan) -> anyhow::Result<String> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Err(StepFailure::new(ErrorClass::Fatal, "no agent 'x'").into())
            }
        }
        let calls = Arc::new(AtomicU32::new(0));
        let bus = new_bus();
        let mut rx = bus.subscribe();
        let policy = RetryPolicy::waiting(3, 1);
        let worker = Arc::new(Fatal(Arc::clone(&calls)));
        match run_step_with_retry(&test_step(), "", &test_plan(), worker, &policy, &bus).await {
            StepOutcome::Exhausted(e) => assert_eq!(e, "no agent 'x'"),
            _ => panic!("expected exhausted"),
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        match rx.try_recv() {
            Ok(OrchestratorEvent::StepFailed { retry_in_ms, .. }) => assert_eq!(retry_in_ms, None),
            other => panic!("expected StepFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn succeeds_on_first_try() {
        let worker = Arc::new(FlakyWorker {
            fails_until: 0,
            calls: Arc::new(AtomicU32::new(0)),
        });
        let policy = RetryPolicy::waiting(3, 1);
        let bus = new_bus();
        match run_step_with_retry(&test_step(), "", &test_plan(), worker, &policy, &bus).await {
            StepOutcome::Ok(s) => assert!(s.contains("output from call 1")),
            _ => panic!("expected ok"),
        }
    }

    #[tokio::test]
    async fn exhausts_after_max_attempts() {
        let worker = Arc::new(FlakyWorker {
            fails_until: 99,
            calls: Arc::new(AtomicU32::new(0)),
        });
        let policy = RetryPolicy::waiting(3, 1);
        let bus = new_bus();
        match run_step_with_retry(&test_step(), "", &test_plan(), worker, &policy, &bus).await {
            StepOutcome::Exhausted(e) => assert!(e.contains("flaky fail")),
            _ => panic!("expected exhausted"),
        }
    }

    #[tokio::test]
    async fn succeeds_on_second_attempt() {
        let worker = Arc::new(FlakyWorker {
            fails_until: 1,
            calls: Arc::new(AtomicU32::new(0)),
        });
        let policy = RetryPolicy::waiting(3, 1);
        let bus = new_bus();
        match run_step_with_retry(&test_step(), "", &test_plan(), worker, &policy, &bus).await {
            StepOutcome::Ok(s) => assert!(s.contains("output from call 2")),
            _ => panic!("expected ok"),
        }
    }
}
