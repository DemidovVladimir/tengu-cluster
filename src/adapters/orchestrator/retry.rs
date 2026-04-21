//! Per-step retry policy.

use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::warn;

use crate::adapters::orchestrator::events::{EventBus, OrchestratorEvent};
use crate::adapters::orchestrator::executor::WorkerHandle;
use crate::adapters::orchestrator::plan::Step;

#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    /// Delay for attempts 2, 3, 4… (attempt 1 runs immediately).
    pub backoff: Vec<Duration>,
}

impl RetryPolicy {
    pub fn new(max_attempts: u32) -> Self {
        Self {
            max_attempts,
            backoff: vec![
                Duration::from_secs(1),
                Duration::from_secs(3),
                Duration::from_secs(9),
            ],
        }
    }
}

pub enum StepOutcome {
    Ok(String),
    Exhausted(String),
}

pub async fn run_step_with_retry(
    step: &Step,
    step_inputs: &str,
    worker: Arc<dyn WorkerHandle>,
    policy: &RetryPolicy,
    events: &EventBus,
) -> StepOutcome {
    let mut last_err = String::new();
    for attempt in 1..=policy.max_attempts {
        match worker.run_step(step, step_inputs).await {
            Ok(output) => return StepOutcome::Ok(output),
            Err(err) => {
                last_err = err.to_string();
                let _ = events.send(OrchestratorEvent::StepFailed {
                    step_id: step.id.clone(),
                    attempt,
                    error: last_err.clone(),
                });
                warn!(step = ?step.id, attempt, error = %last_err, "step failed");
                if attempt < policy.max_attempts {
                    let delay = policy
                        .backoff
                        .get((attempt - 1) as usize)
                        .copied()
                        .unwrap_or(Duration::from_secs(9));
                    sleep(delay).await;
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
    use crate::adapters::orchestrator::events::new_bus;
    use crate::adapters::orchestrator::plan::StepId;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct FlakyWorker {
        fails_until: u32,
        calls: Arc<AtomicU32>,
    }
    #[async_trait]
    impl WorkerHandle for FlakyWorker {
        async fn run_step(&self, _: &Step, _: &str) -> anyhow::Result<String> {
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
        }
    }

    #[tokio::test]
    async fn succeeds_on_first_try() {
        let worker = Arc::new(FlakyWorker {
            fails_until: 0,
            calls: Arc::new(AtomicU32::new(0)),
        });
        let mut policy = RetryPolicy::new(3);
        policy.backoff = vec![
            Duration::from_millis(1),
            Duration::from_millis(1),
            Duration::from_millis(1),
        ];
        let bus = new_bus();
        match run_step_with_retry(&test_step(), "", worker, &policy, &bus).await {
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
        let mut policy = RetryPolicy::new(3);
        policy.backoff = vec![Duration::from_millis(1), Duration::from_millis(1)];
        let bus = new_bus();
        match run_step_with_retry(&test_step(), "", worker, &policy, &bus).await {
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
        let mut policy = RetryPolicy::new(3);
        policy.backoff = vec![Duration::from_millis(1), Duration::from_millis(1)];
        let bus = new_bus();
        match run_step_with_retry(&test_step(), "", worker, &policy, &bus).await {
            StepOutcome::Ok(s) => assert!(s.contains("output from call 2")),
            _ => panic!("expected ok"),
        }
    }
}
