//! Agent primitives for fleet orchestration: worker loop and artifact extraction.

use crate::adapters::types::{AgentId, AgentTaskExecutor, OrchestratorEvent, TokenUsage};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;

// ---------------------------------------------------------------------------
// Worker loop
// ---------------------------------------------------------------------------

/// Classify whether an error message indicates a retryable failure.
fn is_retryable(error: &str) -> bool {
    // Hard failures from http_request / transactions are never retryable.
    if error.contains("no retries allowed") || error.contains("must not be retried") {
        return false;
    }
    error.contains("timed out")
        || error.contains("rate limit")
        || error.contains("HTTP 429")
        || error.contains("HTTP 502")
        || error.contains("HTTP 503")
        || error.contains("response body")
        || error.contains("connection closed")
        || error.contains("connection reset")
}

/// Long-lived worker loop for a single agent. Listens on its dedicated
/// inbox, executes assigned tasks, and sends results to the orchestrator.
pub(crate) async fn agent_worker(
    agent_id: AgentId,
    mut inbox: mpsc::Receiver<OrchestratorEvent>,
    outbox: mpsc::Sender<OrchestratorEvent>,
    executor: Arc<dyn AgentTaskExecutor>,
) {
    tracing::info!(agent = %agent_id, "agent_worker — started, waiting for events");
    while let Some(event) = inbox.recv().await {
        match event {
            OrchestratorEvent::TaskAssignment {
                task_id,
                description,
                correlation_id,
                ..
            } => {
                tracing::info!(
                    agent = %agent_id,
                    task = %task_id,
                    correlation = %correlation_id,
                    prompt_len = description.len(),
                    "agent_worker — received TaskAssignment, executing"
                );
                let exec_start = Instant::now();

                match executor.execute(&description).await {
                    Ok((output, tool_outcomes)) => {
                        let duration = exec_start.elapsed();
                        let output_preview = if output.len() > 300 {
                            format!("{}...[{} chars]", &output[..300], output.len())
                        } else {
                            output.clone()
                        };
                        tracing::info!(
                            agent = %agent_id,
                            task = %task_id,
                            duration_ms = duration.as_millis() as u64,
                            output_len = output.len(),
                            output_preview = %output_preview,
                            tool_outcomes_count = tool_outcomes.len(),
                            "agent_worker — task completed successfully"
                        );
                        let _ = outbox
                            .send(OrchestratorEvent::TaskCompletion {
                                task_id,
                                agent_id: agent_id.clone(),
                                output,
                                artifacts: HashMap::new(),
                                token_usage: TokenUsage::default(),
                                duration,
                                correlation_id,
                                timestamp: chrono::Utc::now(),
                            })
                            .await;
                    }
                    Err(e) => {
                        let retryable = is_retryable(&e);
                        tracing::info!(
                            agent = %agent_id,
                            task = %task_id,
                            error = %e,
                            retryable = retryable,
                            duration_ms = exec_start.elapsed().as_millis() as u64,
                            "agent_worker — task failed"
                        );
                        let _ = outbox
                            .send(OrchestratorEvent::TaskError {
                                task_id,
                                agent_id: agent_id.clone(),
                                error: e,
                                retryable,
                                correlation_id,
                                timestamp: chrono::Utc::now(),
                            })
                            .await;
                    }
                }
            }

            OrchestratorEvent::TaskCancellation { task_id, .. } => {
                tracing::info!(agent = %agent_id, task = %task_id, "agent_worker — task cancelled");
            }

            OrchestratorEvent::Shutdown { reason, .. } => {
                tracing::info!(agent = %agent_id, reason = %reason, "agent_worker — shutting down");
                break;
            }

            other => {
                tracing::trace!(
                    agent = %agent_id,
                    event = ?std::mem::discriminant(&other),
                    "agent_worker — ignored event"
                );
            }
        }
    }
    tracing::debug!(agent = %agent_id, "agent_worker — inbox closed, exiting");
}
