//! One orchestrator turn for a trigger that is not a chat: a planner-bound
//! webhook endpoint (`inbound/webhooks.rs`) and a decision loop's escalation
//! ([`orchestrator_escalator`], every `tengu run` / Studio session with
//! `[orchestrator]`, any build).
//!
//! | Step | What |
//! |---|---|
//! | planner | [`PlannerSource`] — the planner's engine, built at turn time (`orchestrator::planner_llm`; a failed build drops that turn, never the process) |
//! | turn | `build_orchestrator` on the trigger's session id, `Orchestrator::handle`, then the metrics barrier (`flush_metrics`) |
//! | answer | a log line (400-char preview) + Open Brain (`agentic_memory`, step id `webhook-handler`, with `postgres_memory`) — never the trace (it carries no answer) |
//! | owner | the caller spawns the turn on the process's `Turns` (drained on shutdown) |

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tracing::{info, warn};

use crate::application::orchestrator::planner::PlannerLlm;
use crate::application::orchestrator::trace::OrchestratorTrace;
use crate::application::runtime::loops::Turns;
use crate::application::trace_exec::{self, GatedTrace};
use crate::config::Config;
use crate::ports::decision::Escalator;
use crate::ports::trace::TraceSink;

/// The escalator loops use when `[orchestrator]` is configured: a
/// low-confidence step becomes a one-shot orchestrator turn, recorded into
/// `trace` under the loop event's cause and owned by `turns` (the dispatch's
/// — `bootstrap::runtime`).
pub(crate) fn orchestrator_escalator(
    config: &Config,
    trace: Arc<dyn TraceSink>,
    turns: Arc<Turns>,
) -> Option<Arc<dyn Escalator>> {
    config.orchestrator.as_ref().map(|_| {
        Arc::new(OrchestratorEscalator {
            config: config.clone(),
            planner: config_planner(),
            trace,
            turns,
        }) as Arc<dyn Escalator>
    })
}

/// Builds the planner's LLM call for a one-shot turn: from the config
/// (`planner_llm`, at turn time — an engine that cannot build fails that
/// turn, never the listener); tests hand a stub engine.
pub(crate) type PlannerSource = Arc<dyn Fn(&Config) -> Result<Option<PlannerLlm>> + Send + Sync>;

pub(crate) fn config_planner() -> PlannerSource {
    Arc::new(crate::bootstrap::orchestrator::planner_llm)
}

/// Run one orchestrator turn for the trigger's message (module table).
/// Final agent text is written to the tracing log; durable side-effects
/// (`agentic_memory` step summaries, metrics records) flow through the standard
/// pipeline. Errors are logged and discarded — the listener stays up.
/// `trace` = where its plans and steps are recorded (`None`: nowhere);
/// every metrics record of the turn is in it when this returns (the
/// barrier, `Orchestrator::flush_metrics`).
pub(crate) async fn run_one_shot(
    config: &Config,
    planner: &PlannerSource,
    session_id: &str,
    user_message: String,
    trace: Option<OrchestratorTrace>,
) {
    // Per-request orchestrator: the planner calls its own engine, each step
    // is a `run-agent` child that reloads the sandbox config itself.
    let llm = match planner(config) {
        Ok(Some(llm)) => llm,
        Ok(None) => {
            warn!(session_id = %session_id, "webhook turn aborted: no [orchestrator] in the config");
            return;
        }
        Err(e) => {
            warn!(
                session_id = %session_id,
                error = %format!("{e:#}"),
                "webhook turn aborted: the planner's engine did not build"
            );
            return;
        }
    };
    let Some(orchestrator) = crate::bootstrap::orchestrator::build_orchestrator(
        config,
        llm,
        session_id.to_string(),
        trace,
    ) else {
        warn!(
            session_id = %session_id,
            "build_orchestrator returned None — webhook turn aborted (engine != \"rag\"). This is a sandbox-config issue, not a runtime crisis."
        );
        return;
    };

    let final_text = orchestrator.handle(user_message).await;
    // The turn's last `metrics.recorded` is written before it ends (and its
    // trace gate closes): nothing of it follows the run's closing event.
    orchestrator.flush_metrics().await;
    drop(orchestrator);
    if final_text.trim().is_empty() {
        warn!(
            session_id = %session_id,
            "webhook orchestrator turn returned empty output"
        );
        return;
    }

    let preview: String = final_text.chars().take(400).collect();
    let truncated = final_text.chars().count() > 400;
    info!(
        session_id = %session_id,
        output_chars = final_text.chars().count(),
        preview = %preview,
        truncated = truncated,
        "webhook orchestrator turn completed"
    );

    // Persist the orchestrator's final output to Open Brain (`agentic_memory`,
    // Postgres) with a synthetic step_id `"webhook-handler"`. Without this,
    // webhook turns where the planner emitted `Direct { response }` (no
    // subagent dispatched, so no `compress_and_store` and no backstop) leave
    // nothing recallable. Coexists with subagent step summaries on the same
    // session_id; different step_ids keep them distinct.
    persist_webhook_output(&config.sandbox_owner(), session_id, &final_text).await;
}

/// Fire-and-forget persist of the webhook turn's final text into Open Brain
/// (`agentic_memory`, Postgres). Logs success at info, errors at warn — never
/// propagates. Embedding is best-effort: no `OPENROUTER_API_KEY` (or an embed
/// error) stores a text-only memory.
#[cfg(feature = "postgres_memory")]
async fn persist_webhook_output(sandbox: &str, session_id: &str, final_text: &str) {
    let embedding = match std::env::var("OPENROUTER_API_KEY") {
        Ok(api_key) => {
            let embedder = crate::adapters::outbound::memory::embedder::Embedder::new(
                api_key,
                // Open Brain is `vector(1536)`: always the pinned model.
                crate::domain::memory::DEFAULT_EMBEDDING_MODEL.to_string(),
            );
            match embedder.embed(final_text).await {
                Ok(v) => Some(v),
                Err(e) => {
                    warn!(
                        session_id = %session_id,
                        error = %e,
                        "webhook persist: embedding failed; writing text-only memory"
                    );
                    None
                }
            }
        }
        Err(_) => None,
    };
    match crate::adapters::outbound::tools::agentic_memory::write_step_summary_with_embedding(
        sandbox,
        session_id,
        "webhook-handler",
        final_text,
        embedding.as_deref(),
    )
    .await
    {
        Ok(id) => info!(
            entry_id = %id,
            session_id = %session_id,
            step_id = "webhook-handler",
            "webhook output persisted to agentic_memory"
        ),
        Err(e) => warn!(
            session_id = %session_id,
            error = %e,
            "webhook output persist FAILED — agentic_memory unavailable (is TENGU_MEMORY_DATABASE_URL set?)"
        ),
    }
}

/// Without `postgres_memory` there is no durable memory backend — the webhook
/// turn still completes and its output is in the tracing log, just not
/// recallable.
#[cfg(not(feature = "postgres_memory"))]
async fn persist_webhook_output(_sandbox: &str, session_id: &str, _final_text: &str) {
    warn!(
        session_id = %session_id,
        "webhook output not persisted — built without the `postgres_memory` feature"
    );
}

/// `Escalator` for decision loops: a low-confidence step becomes a one-shot
/// orchestrator turn on the loop's session id (planner → subagents), exactly
/// like a planner-bound webhook. The loop event does not wait for it; the
/// process's `turns` own it (module table).
struct OrchestratorEscalator {
    config: Config,
    planner: PlannerSource,
    trace: Arc<dyn TraceSink>,
    turns: Arc<Turns>,
}

#[async_trait]
impl Escalator for OrchestratorEscalator {
    async fn escalate(&self, session_id: String, message: String) {
        let config = self.config.clone();
        let planner = Arc::clone(&self.planner);
        // Recorded under what the loop event runs caused by (its
        // `loop.started`, a decide run's root), read before the spawn.
        let parent = trace_exec::cause().and_then(|c| c.parent);
        let gate = GatedTrace::new(Arc::clone(&self.trace));
        let trace = OrchestratorTrace::of(&(Arc::clone(&gate) as Arc<dyn TraceSink>), parent);
        let sid = session_id.clone();
        let turn = trace_exec::close_after(gate, async move {
            run_one_shot(&config, &planner, &sid, message, trace).await;
        });
        if self.turns.spawn(turn).is_err() {
            warn!(session_id = %session_id, "escalation dropped: shutting down");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::trace_exec::tests::MemTrace;

    /// Loops escalate only where a planner exists — in every build, not
    /// only one with the webhook listener.
    #[test]
    fn escalates_only_with_an_orchestrator() {
        let sink: Arc<dyn TraceSink> = Arc::new(MemTrace::default());
        let mut config = Config::default();
        assert!(orchestrator_escalator(&config, Arc::clone(&sink), Turns::new()).is_none());
        config.orchestrator = Some(toml::from_str("agent = \"planner\"").unwrap());
        assert!(orchestrator_escalator(&config, sink, Turns::new()).is_some());
    }
}
